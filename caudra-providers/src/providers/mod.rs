use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_lite::StreamExt;
use futures_lite::io::AsyncBufRead;
use isahc::config::{Configurable, VersionNegotiation};
use isahc::http::request::Builder;
use serde::Deserialize;
use tracing::debug;

use crate::AgentError;

pub(crate) mod anthropic;
pub(crate) mod aperture;
pub(crate) mod catalog;
pub(crate) mod copilot;
pub mod custom;
pub(crate) mod deepseek;
pub mod dynamic;
pub(crate) mod google;
pub(crate) mod llama_cpp;
pub(crate) mod local;
pub(crate) mod mistral;
pub(crate) mod oauth;
pub(crate) mod ollama;
pub(crate) mod openai;
pub(crate) mod openai_compat;
pub mod opencode;
pub(crate) mod openrouter;
pub(crate) mod synthetic;
pub(crate) mod tensorx;
pub(crate) mod xai;
pub(crate) mod zai;

const LOW_SPEED_BYTES_PER_SEC: u32 = 1;

pub(crate) fn user_agent() -> &'static str {
    concat!(
        "caudra/v",
        env!("CARGO_PKG_VERSION"),
        "-g",
        env!("GIT_SHORT_HASH")
    )
}

#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub connect: Duration,
    pub stream: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            stream: Duration::from_secs(300),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedAuth {
    pub base_url: Option<String>,
    pub headers: Vec<(String, String)>,
}

impl ResolvedAuth {
    pub fn bearer(api_key: &str) -> Self {
        Self {
            base_url: None,
            headers: vec![("authorization".into(), format!("Bearer {api_key}"))],
        }
    }

    /// Apply all auth headers to an HTTP request builder.
    pub fn configure_request(&self, builder: Builder) -> Builder {
        self.headers.iter().fold(builder, |b, (key, value)| {
            b.header(key.as_str(), value.as_str())
        })
    }
}

pub(crate) fn with_prefix<'a>(
    prefix: &Option<String>,
    system: &'a str,
    buf: &'a mut String,
) -> &'a str {
    match prefix {
        Some(p) => {
            *buf = format!("{p}\n\n{system}");
            buf
        }
        None => system,
    }
}

pub(crate) fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

#[derive(Deserialize)]
pub(crate) struct SseErrorPayload {
    pub error: SseErrorDetail,
}

#[derive(Deserialize)]
pub(crate) struct SseErrorDetail {
    #[serde(default)]
    pub r#type: String,
    pub message: String,
}

impl SseErrorPayload {
    pub fn into_agent_error(self) -> AgentError {
        let status = match self.error.r#type.as_str() {
            "overloaded_error" => 529,
            "service_unavailable" | "service_unavailable_error" => 503,
            "api_error" | "server_error" => 500,
            "rate_limit_error" | "rate_limit_exceeded" | "tokens" => 429,
            "request_too_large" => 413,
            "timeout_error" | "request_timeout" => 408,
            "not_found_error" => 404,
            "permission_error" => 403,
            "billing_error" | "insufficient_quota" => 402,
            "authentication_error" | "invalid_api_key" => 401,
            // Deliberately 400 rather than a server status: `invalid_request_error`
            // arrives here, and `AgentError::is_context_overflow` only inspects 400
            // and 413, so widening this would cost compaction its trigger.
            _ => 400,
        };
        AgentError::api(status, self.error.message)
    }
}

/// One SSE line, or `None` at end of stream.
///
/// The yield is what makes a stream a stream. A buffered reader answers every
/// line already in its buffer without parking, and sending an event on an
/// unbounded channel never parks either, so a parser that only awaits these two
/// keeps the executor thread for a whole network read. Everything downstream —
/// the event forwarder, the OAuth relay sharing this task — then advances once
/// per read instead of once per event, and the reader sees a burst.
pub(crate) async fn next_sse_line<R: AsyncBufRead + Unpin>(
    lines: &mut futures_lite::io::Lines<R>,
    deadline: &mut Instant,
    stream_timeout: Duration,
) -> Result<Option<String>, AgentError> {
    futures_lite::future::yield_now().await;
    let remaining = deadline.saturating_duration_since(Instant::now());
    let result = futures_lite::future::or(
        async { lines.next().await.transpose().map_err(AgentError::from) },
        async {
            smol::Timer::after(remaining).await;
            Err(AgentError::Timeout {
                secs: stream_timeout.as_secs(),
            })
        },
    )
    .await;
    // Only payload renews the budget. A blank separator and a `:` comment
    // keep-alive both arrive on a stream that is producing nothing, and
    // renewing on those lets a wedged server hold the request open forever.
    if let Ok(Some(line)) = &result
        && !is_sse_filler(line)
    {
        *deadline = Instant::now() + stream_timeout;
    }
    result
}

fn is_sse_filler(line: &str) -> bool {
    let line = line.trim_end_matches('\r');
    line.is_empty() || line.starts_with(':')
}

pub(crate) fn http_client(timeouts: Timeouts) -> isahc::HttpClient {
    isahc::HttpClient::builder()
        .connect_timeout(timeouts.connect)
        // A server that accepts a request and then computes in silence is
        // normal: a cold prefill of a long conversation sends nothing for a
        // minute or more. `stream` is the one budget for that silence, and the
        // SSE reader enforces the same figure per payload line, so curl must
        // not abort earlier on a shorter one of its own.
        .low_speed_timeout(LOW_SPEED_BYTES_PER_SEC, timeouts.stream)
        // The workspace enables curl's http2 feature for OTLP over gRPC, which
        // would otherwise flip provider streaming to h2 over TLS. Streaming is
        // tuned for HTTP/1.1, so pin it.
        .version_negotiation(VersionNegotiation::http11())
        .build()
        .expect("failed to build HTTP client")
}

#[derive(Clone, Debug)]
pub struct KeyPool {
    keys: Arc<Vec<String>>,
    index: Arc<AtomicUsize>,
}

impl KeyPool {
    pub fn from_env(env_var: &str) -> Result<Self, AgentError> {
        let raw = std::env::var(env_var).map_err(|_| AgentError::Config {
            message: format!("{env_var} not set"),
        })?;
        let keys: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if keys.is_empty() {
            return Err(AgentError::Config {
                message: format!("{env_var} is empty"),
            });
        }
        Ok(Self {
            keys: Arc::new(keys),
            index: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn resolve(slug: &str, env_var: &str) -> Result<Self, AgentError> {
        if let Ok(pool) = Self::from_env(env_var) {
            debug!(slug, keys = pool.len(), "resolved API key from env");
            return Ok(pool);
        }
        if let Some(key) = Self::key_from_file(slug) {
            debug!(slug, "resolved API key from saved credentials");
            return Ok(Self::from_keys(vec![key]));
        }
        if let Some(key) = Self::key_from_config(slug) {
            debug!(slug, "resolved API key from providers.toml");
            return Ok(Self::from_keys(vec![key]));
        }
        Err(AgentError::Config {
            message: format!(
                "{env_var} not set and no saved credentials for '{slug}' — run `caudra auth login {slug}`"
            ),
        })
    }

    fn key_from_file(slug: &str) -> Option<String> {
        let dir = caudra_storage::StateDir::resolve().ok()?;
        caudra_storage::auth::load_provider_credentials(&dir, slug).map(|c| c.api_key)
    }

    fn key_from_config(slug: &str) -> Option<String> {
        caudra_config::providers::ProvidersConfig::load()
            .get(slug)
            .and_then(|d| d.api_key.clone())
    }

    pub(crate) fn from_keys(keys: Vec<String>) -> Self {
        Self {
            keys: Arc::new(keys),
            index: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn current(&self) -> &str {
        &self.keys[self.index.load(Ordering::Relaxed) % self.keys.len()]
    }

    pub fn rotate(&self) -> bool {
        if self.keys.len() <= 1 {
            return false;
        }
        self.index.fetch_add(1, Ordering::Relaxed);
        true
    }

    pub fn rotate_auth(
        &self,
        auth: &Mutex<ResolvedAuth>,
        build: impl FnOnce(&str) -> ResolvedAuth,
    ) -> bool {
        if !self.rotate() {
            return false;
        }
        *auth.lock().unwrap() = build(self.current());
        true
    }

    pub fn rotate_headers(
        &self,
        auth: &Mutex<ResolvedAuth>,
        build: impl FnOnce(&str) -> Vec<(String, String)>,
    ) -> bool {
        if !self.rotate() {
            return false;
        }
        auth.lock().unwrap().headers = build(self.current());
        true
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) const CREDENTIAL_IN_URL: &str =
        "credentials travel in headers, never in a URL a dry run shows";
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;
    use futures_lite::io::AsyncBufReadExt;
    use test_case::test_case;

    const STREAM_TIMEOUT: Duration = Duration::from_secs(300);
    /// More lines than the reader may consume before the competing task runs.
    const BUFFERED_SSE_LINES: usize = 8;
    const EXPECT_INTERLEAVED: &str =
        "a reader with buffered lines must let another task on its thread run";
    const EXPECT_NO_RENEWAL: &str = "a blank or comment line must not renew the stream deadline";
    const EXPECT_RENEWAL: &str = "a data line must renew the stream deadline";

    /// Deliberately free of the wording `AgentError::is_retryable` treats as
    /// transient, so these cases measure the type-to-status map and nothing else.
    const SSE_MESSAGE: &str = "provider rejected the request";
    const EXPECT_RETRYABLE: &str = "a transient SSE error type must map to a retryable status";
    const EXPECT_TERMINAL: &str = "a client-fault SSE error type must map to a terminal status";

    #[test_case("a b", "a%20b" ; "space")]
    #[test_case("a:b", "a%3Ab" ; "colon")]
    #[test_case("abc", "abc"   ; "passthrough")]
    fn urlenc_encodes(input: &str, expected: &str) {
        assert_eq!(urlenc(input), expected);
    }

    fn sse_error(r#type: &str) -> AgentError {
        SseErrorPayload {
            error: SseErrorDetail {
                r#type: r#type.to_owned(),
                message: SSE_MESSAGE.to_owned(),
            },
        }
        .into_agent_error()
    }

    #[test_case("overloaded_error", 529, true            ; "overloaded")]
    #[test_case("service_unavailable_error", 503, true   ; "service_unavailable_error")]
    #[test_case("service_unavailable", 503, true         ; "service_unavailable")]
    #[test_case("server_error", 500, true                ; "server_error")]
    #[test_case("rate_limit_error", 429, true            ; "rate_limited")]
    #[test_case("timeout_error", 408, true               ; "timeout")]
    #[test_case("request_timeout", 408, true             ; "request_timeout")]
    #[test_case("request_too_large", 413, false          ; "too_large")]
    #[test_case("authentication_error", 401, false       ; "unauthenticated")]
    #[test_case("invalid_request_error", 400, false      ; "invalid_request")]
    #[test_case("", 400, false                           ; "missing_type")]
    fn sse_error_type_maps_to_status(r#type: &str, expected: u16, retryable: bool) {
        let error = sse_error(r#type);
        assert_eq!(error.status(), Some(expected));
        assert_eq!(
            error.is_retryable(),
            retryable,
            "{}",
            if retryable {
                EXPECT_RETRYABLE
            } else {
                EXPECT_TERMINAL
            }
        );
    }

    struct NeverReader;

    impl futures_lite::io::AsyncRead for NeverReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Pending
        }
    }

    impl futures_lite::io::AsyncBufRead for NeverReader {
        fn poll_fill_buf(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<&[u8]>> {
            std::task::Poll::Pending
        }

        fn consume(self: std::pin::Pin<&mut Self>, _amt: usize) {}
    }

    #[test]
    fn next_sse_line_expired_deadline_returns_timeout() {
        smol::block_on(async {
            let mut lines = NeverReader.lines();
            let mut past = Instant::now() - Duration::from_secs(1);
            let err = next_sse_line(&mut lines, &mut past, STREAM_TIMEOUT)
                .await
                .unwrap_err();
            assert!(matches!(err, AgentError::Timeout { .. }));
        })
    }

    /// Keep-alives must not buy a wedged server another whole timeout.
    #[test_case("" ; "blank_separator")]
    #[test_case(": ping" ; "comment_keepalive")]
    #[test_case("\r" ; "blank_with_carriage_return")]
    fn sse_filler_does_not_extend_the_deadline(line: &str) {
        smol::block_on(async {
            let body = format!("{line}\ndata: {{}}\n");
            let mut lines = futures_lite::io::Cursor::new(body).lines();
            let started = Instant::now();
            let mut deadline = started + STREAM_TIMEOUT;

            next_sse_line(&mut lines, &mut deadline, STREAM_TIMEOUT)
                .await
                .unwrap();
            assert_eq!(deadline, started + STREAM_TIMEOUT, "{EXPECT_NO_RENEWAL}");

            next_sse_line(&mut lines, &mut deadline, STREAM_TIMEOUT)
                .await
                .unwrap();
            assert!(deadline > started + STREAM_TIMEOUT, "{EXPECT_RENEWAL}");
        })
    }

    /// A whole response already in the buffer, on a one-thread executor: the
    /// competing task stands in for the event forwarder, which is what turns
    /// deltas into a live count instead of one burst at the end of the read.
    #[test]
    fn next_sse_line_lets_another_task_run_between_buffered_lines() {
        let ex = smol::LocalExecutor::new();
        let other_ran = Rc::new(Cell::new(false));
        let read_when_other_ran = Rc::new(Cell::new(usize::MAX));

        let reader = ex.spawn({
            let other_ran = Rc::clone(&other_ran);
            let read_when_other_ran = Rc::clone(&read_when_other_ran);
            async move {
                let body = "data: {}\n".repeat(BUFFERED_SSE_LINES);
                let mut lines = futures_lite::io::Cursor::new(body).lines();
                let mut deadline = Instant::now() + STREAM_TIMEOUT;
                let mut read = 0;
                while next_sse_line(&mut lines, &mut deadline, STREAM_TIMEOUT)
                    .await
                    .unwrap()
                    .is_some()
                {
                    read += 1;
                    if other_ran.get() && read_when_other_ran.get() == usize::MAX {
                        read_when_other_ran.set(read);
                    }
                }
                read
            }
        });
        let other = ex.spawn(async move { other_ran.set(true) });

        let read = smol::block_on(ex.run(async {
            let read = reader.await;
            other.await;
            read
        }));
        assert_eq!(read, BUFFERED_SSE_LINES);
        assert!(
            read_when_other_ran.get() < BUFFERED_SSE_LINES,
            "{EXPECT_INTERLEAVED}"
        );
    }

    #[test]
    fn key_pool_single_key_current() {
        let pool = KeyPool::from_keys(vec!["sk-1".into()]);
        assert_eq!(pool.current(), "sk-1");
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn key_pool_single_key_rotate_returns_false() {
        let pool = KeyPool::from_keys(vec!["sk-1".into()]);
        assert!(!pool.rotate());
        assert_eq!(pool.current(), "sk-1");
    }

    #[test]
    fn key_pool_multi_key_rotates() {
        let pool = KeyPool::from_keys(vec!["sk-1".into(), "sk-2".into(), "sk-3".into()]);
        assert_eq!(pool.current(), "sk-1");
        assert!(pool.rotate());
        assert_eq!(pool.current(), "sk-2");
        assert!(pool.rotate());
        assert_eq!(pool.current(), "sk-3");
    }

    #[test]
    fn key_pool_wraps_around() {
        let pool = KeyPool::from_keys(vec!["a".into(), "b".into()]);
        pool.rotate();
        pool.rotate();
        assert_eq!(pool.current(), "a");
    }

    #[test]
    fn resolve_from_env() {
        let env_var = format!("CAUDRA_TEST_KEY_{}", fastrand::u32(..));
        unsafe { std::env::set_var(&env_var, "from-env") };
        let pool = KeyPool::resolve("test_slug", &env_var).unwrap();
        unsafe { std::env::remove_var(&env_var) };
        assert_eq!(pool.current(), "from-env");
    }

    #[test]
    fn resolve_env_supports_comma_separated() {
        let env_var = format!("CAUDRA_TEST_MULTI_{}", fastrand::u32(..));
        unsafe { std::env::set_var(&env_var, "sk-1, sk-2, sk-3") };
        let pool = KeyPool::resolve("test_slug", &env_var).unwrap();
        unsafe { std::env::remove_var(&env_var) };
        assert_eq!(pool.current(), "sk-1");
        assert!(pool.rotate());
        assert_eq!(pool.current(), "sk-2");
    }

    #[test]
    fn resolve_returns_error_when_nothing_found() {
        let slug = format!("test_resolve_none_{}", fastrand::u32(..));
        let env_var = format!("CAUDRA_TEST_KEY_NONE_{}", fastrand::u32(..));
        let result = KeyPool::resolve(&slug, &env_var);
        assert!(result.is_err());
        let msg = format!("{result:?}");
        assert!(msg.contains(&env_var) || msg.contains(&slug));
    }
}
