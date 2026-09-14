//! Provider error types with retry semantics.
//! Retryable: 429, 5xx, IO, HTTP transport. Non-retryable: other 4xx, JSON parse, config,
//! channel closed, user cancel. `user_message()` returns human-readable text for each variant,
//! including whatever the provider said in the response body.

use std::time::Duration;

use futures_lite::io::AsyncReadExt;
use isahc::http::HeaderMap;
use serde_json::Value;

/// Enough of an error body to diagnose one, and no more: a non-200 can be an
/// endless stream, and nothing downstream reads past the first screenful.
const ERROR_BODY_CAP: u64 = 64 * 1024;
const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(10);
const UNREADABLE_ERROR_BODY: &str = "unable to read error body";

const HEADER_RETRY_AFTER: &str = "retry-after";
const HEADER_RETRY_AFTER_MS: &str = "retry-after-ms";
/// Keeps `Duration::from_secs_f64` away from its overflow panic; the retry loop
/// clamps far harder than this before it ever waits.
const MAX_PARSED_RETRY_AFTER_SECS: f64 = 86_400.0;
const MINUTE_SECS: u64 = 60;
const HOUR_SECS: u64 = 3_600;

/// The transcript can afford a paragraph of provider text; the status bar is one
/// clipped line, so it gets a much shorter slice of the same detail.
const DETAIL_CAP: usize = 400;
const RETRY_DETAIL_CAP: usize = 120;
const HTML_SNIFF_CHARS: usize = 16;
const HTML_PREFIXES: [&str; 2] = ["<!doctype", "<html"];
const ELLIPSIS: char = '…';

const AUTH_LABEL: &str = "authentication failed";
const AUTH_HINT: &str = ", run `caudra auth login` or check your API key";
const RATE_LIMIT_LABEL: &str = "rate limited";
const RATE_LIMIT_FALLBACK: &str = "rate limited, try again in a moment";
const OVERLOADED_LABEL: &str = "provider is overloaded";
const OVERLOADED_FALLBACK: &str = "provider is overloaded, try again later";
const RETRY_RATE_LIMIT_LABEL: &str = "Rate limited";
const RETRY_OVERLOADED_LABEL: &str = "Provider is overloaded";
const GATEWAY_401: &str = "authentication failed: request was blocked by a gateway or proxy, your token may be missing or expired, run `caudra auth login` or check your API key";
const GATEWAY_403: &str = "forbidden: request was blocked by a gateway or proxy, check your account and provider settings";

/// Auth and billing never become healthy by waiting, whatever the body says.
const NEVER_TRANSIENT_STATUSES: [u16; 3] = [401, 402, 403];
/// A provider that names its own outage is worth retrying even when it labelled
/// the failure with a status that says otherwise. Phrases are narrow on purpose:
/// "try again later" rather than "try again", which genuine refusals also say.
const TRANSIENT_PHRASES: [&str; 5] = [
    "overloaded",
    "try again later",
    "temporarily unavailable",
    "service unavailable",
    "please retry",
];

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("API error ({status}): {message}")]
    Api {
        status: u16,
        message: String,
        /// Only ever set by `from_response`; every other construction site is a
        /// locally detected failure with no HTTP response behind it.
        retry_after: Option<Duration>,
    },
    #[error("{message}")]
    Config { message: String },
    #[error("tool error in {tool}: {message}")]
    Tool { tool: String, message: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("http: {0}")]
    Http(#[from] isahc::Error),
    #[error("http request: {0}")]
    HttpRequest(#[from] isahc::http::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("channel send failed")]
    Channel,
    #[error("cancelled")]
    Cancelled,
    #[error("stream timed out after {secs}s of inactivity")]
    Timeout { secs: u64 },
    #[error("compaction returned no summary")]
    EmptySummary,
    #[error("automatic recovery exhausted for {rule}")]
    SteeringExhausted { rule: String },
}

impl AgentError {
    pub fn api(status: u16, message: impl Into<String>) -> Self {
        Self::Api {
            status,
            message: message.into(),
            retry_after: None,
        }
    }

    /// The provider's own `Retry-After`, when it sent one.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Api { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// A stable discriminant for logs and dashboards. The `Display` text moves
    /// with provider wording, so grouping on it would split one failure mode
    /// across many buckets.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Api { .. } => "api",
            Self::Config { .. } => "config",
            Self::Tool { .. } => "tool",
            Self::Io(_) => "io",
            Self::Http(_) => "http",
            Self::HttpRequest(_) => "http_request",
            Self::Json(_) => "json",
            Self::Channel => "channel",
            Self::Cancelled => "cancelled",
            Self::Timeout { .. } => "timeout",
            Self::EmptySummary => "empty_summary",
            Self::SteeringExhausted { .. } => "steering_exhausted",
        }
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub fn is_retryable(&self) -> bool {
        if self.is_context_overflow() {
            return false;
        }
        match self {
            Self::Api {
                status, message, ..
            } => {
                *status == 408
                    || *status == 429
                    || *status >= 500
                    || (!NEVER_TRANSIENT_STATUSES.contains(status) && is_transient_message(message))
            }
            Self::Io(_) | Self::Http(_) | Self::Timeout { .. } => true,
            Self::Config { .. }
            | Self::Tool { .. }
            | Self::Channel
            | Self::Json(_)
            | Self::Cancelled
            | Self::EmptySummary
            | Self::SteeringExhausted { .. }
            | Self::HttpRequest(_) => false,
        }
    }

    /// Returns true if the error indicates a context window overflow.
    ///
    /// Provider error formats:
    /// - Anthropic:  413 "prompt is too long"  <https://docs.anthropic.com/en/docs/errors>
    /// - Anthropic:  400 long-context entitlement refusals, which name the feature instead of
    ///   the size; compacting below the wide window is the only way forward.
    /// - OpenAI:     400 "maximum context length is X tokens"  <https://platform.openai.com/docs/guides/error-codes>
    /// - Gemini:     400 "input token count exceeds" / "too many tokens"  <https://ai.google.dev/gemini-api/docs/troubleshooting>
    /// - Ollama:     400 "context length exceeded"  <https://docs.ollama.com/api/errors>
    /// - llama.cpp:  400 "exceeds the available context size"  <https://github.com/ggml-org/llama.cpp/blob/master/tools/server/server-context.cpp>
    /// - Bedrock:    400 ValidationException "Input is too long for requested model"  <https://repost.aws/knowledge-center/bedrock-validation-exception-errors>
    /// - DeepSeek:   400 "maximum context length is X tokens"  <https://api-docs.deepseek.com/quick_start/pricing>
    /// - Mistral:    400 "too large for model with X maximum context length"  <https://docs.mistral.ai/resources/known-limitations>
    /// - OpenRouter: 400 "endpoint's maximum context length is X tokens"  <https://openrouter.ai/docs/api/reference/errors-and-debugging.mdx>
    /// - Synthetic:  400 pass-through from upstream models (OpenAI-compatible)  <https://synthetic.new>
    pub fn is_context_overflow(&self) -> bool {
        match self {
            Self::Api { status: 413, .. } => true,
            Self::Api {
                status: 400,
                message,
                ..
            } => {
                let m = message.to_lowercase();
                let is_scope = m.contains("context")
                    || m.contains("token")
                    || m.contains("prompt")
                    || m.contains("input");
                let is_overflow = m.contains("exceeds")
                    || m.contains("exceeded")
                    || m.contains("too long")
                    || m.contains("too many")
                    || m.contains("maximum");
                let is_long_context_entitlement =
                    m.contains("long context") && !m.contains("out of extra usage");
                (is_scope && is_overflow) || is_long_context_entitlement
            }
            _ => false,
        }
    }

    pub fn is_auth_error(&self) -> bool {
        matches!(self, Self::Api { status: 401, .. })
    }

    pub fn is_model_unavailable(&self) -> bool {
        let Self::Api { message, .. } = self else {
            return false;
        };
        if let Ok(value) = serde_json::from_str::<Value>(message) {
            let error = value.get("error");
            let unavailable_code = [
                error.and_then(|error| error.get("code")),
                error.and_then(|error| error.get("type")),
                value.get("code"),
                value.get("type"),
            ]
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|code| {
                matches!(
                    code.to_ascii_lowercase().as_str(),
                    "model_not_found"
                        | "model_not_available"
                        | "model_unavailable"
                        | "unknown_model"
                )
            });
            if unavailable_code {
                return true;
            }
            let detail = [
                error.and_then(|error| error.get("message")),
                error.filter(|error| error.is_string()),
                value.get("message"),
                value.get("detail"),
            ]
            .into_iter()
            .flatten()
            .find_map(|value| Value::as_str(value).filter(|detail| !detail.trim().is_empty()));
            return detail.is_some_and(model_unavailable_message);
        }
        model_unavailable_message(message)
    }

    pub fn should_rotate_key(&self) -> bool {
        matches!(self, Self::Api { status, .. } if *status == 429 || *status == 401 || *status == 403)
    }

    pub fn user_message(&self) -> String {
        match self {
            Self::Config { message } => message.clone(),
            Self::Api {
                status,
                message,
                retry_after,
            } => api_user_message(*status, message, *retry_after),
            Self::Tool { tool, message } => format!("{tool}: {message}"),
            Self::Io(e) => format!("I/O error: {e}"),
            Self::Http(_) => "connection error, check your network".into(),
            Self::Timeout { .. } => "stream timed out, retrying".into(),
            Self::HttpRequest(e) => format!("request error: {e}"),
            Self::Json(_) => "received an invalid response from the API".into(),
            Self::Channel => "internal error, try again".into(),
            Self::Cancelled => "cancelled".into(),
            Self::EmptySummary => "compaction returned no summary, history kept as is".into(),
            Self::SteeringExhausted { rule } => format!(
                "automatic recovery limit reached for {rule}; review the partial output and resume or adjust steering limits"
            ),
        }
    }

    pub async fn from_response(response: isahc::Response<isahc::AsyncBody>) -> Self {
        let status = response.status().as_u16();
        let retry_after = parse_retry_after(response.headers());
        let message = read_error_body(response.into_body()).await;
        Self::Api {
            status,
            message,
            retry_after,
        }
    }

    pub fn retry_message(&self) -> String {
        match self {
            Self::Api {
                status, message, ..
            } => api_retry_message(*status, message),
            Self::Io(_) | Self::Http(_) => "Connection error".into(),
            Self::Timeout { .. } => "Stream timed out".into(),
            _ => self.to_string(),
        }
    }
}

fn is_transient_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    TRANSIENT_PHRASES
        .iter()
        .any(|phrase| message.contains(phrase))
}

fn model_unavailable_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    if message.contains("unknown model") {
        return true;
    }
    let Some(model_index) = message.find("model") else {
        return false;
    };
    let model_detail = message[model_index..]
        .split(['.', ',', ';', '\n'])
        .next()
        .unwrap_or_default();
    if model_detail.contains("not available for this feature") {
        return false;
    }
    model_detail.contains("not found")
        || model_detail.contains("not available")
        || model_detail.contains("unavailable")
        || model_detail.contains("does not exist")
        || model_detail.contains("doesn't exist")
}

fn api_user_message(status: u16, body: &str, retry_after: Option<Duration>) -> String {
    let message = match status {
        401 | 403 if is_html(body) => gateway_message(status).to_owned(),
        401 => format!(
            "{}{AUTH_HINT}",
            labeled(AUTH_LABEL, AUTH_LABEL, body, DETAIL_CAP)
        ),
        429 => labeled(RATE_LIMIT_LABEL, RATE_LIMIT_FALLBACK, body, DETAIL_CAP),
        529 => labeled(OVERLOADED_LABEL, OVERLOADED_FALLBACK, body, DETAIL_CAP),
        _ => {
            let label = if status >= 500 {
                format!("server error ({status})")
            } else {
                format!("API error ({status})")
            };
            labeled(&label, &label, body, DETAIL_CAP)
        }
    };
    // Retrying stopped somewhere the user cannot see; when the provider named a
    // window, that window is the difference between "try again" and "wait".
    match retry_after {
        Some(after) => format!("{message} (retry after {})", friendly_wait(after)),
        None => message,
    }
}

fn friendly_wait(after: Duration) -> String {
    let secs = after.as_secs().max(1);
    match secs {
        0..MINUTE_SECS => format!("{secs}s"),
        MINUTE_SECS..HOUR_SECS => format!("{}m", secs / MINUTE_SECS),
        _ => format!("{}h", secs / HOUR_SECS),
    }
}

fn api_retry_message(status: u16, body: &str) -> String {
    let label = match status {
        429 => RETRY_RATE_LIMIT_LABEL.to_owned(),
        529 => RETRY_OVERLOADED_LABEL.to_owned(),
        _ if status >= 500 => format!("Server error ({status})"),
        _ => format!("API error ({status})"),
    };
    labeled(&label, &label, body, RETRY_DETAIL_CAP)
}

fn gateway_message(status: u16) -> &'static str {
    if status == 403 {
        GATEWAY_403
    } else {
        GATEWAY_401
    }
}

fn labeled(label: &str, fallback: &str, body: &str, cap: usize) -> String {
    match provider_detail(body, cap) {
        Some(detail) => format!("{label}: {detail}"),
        None => fallback.to_owned(),
    }
}

/// The provider's own explanation, pulled out of whatever shape it arrived in.
/// A gateway error page is markup rather than an explanation, so it is dropped
/// and the caller falls back to status-specific guidance.
fn provider_detail(body: &str, cap: usize) -> Option<String> {
    let body = body.trim();
    if body.is_empty() || is_html(body) {
        return None;
    }
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| json_detail(&value))
        .unwrap_or_else(|| body.to_owned());
    Some(truncate(&detail, cap))
}

/// A bare 429 says nothing a status code did not already say; the error code
/// beside the message is what separates a per-minute limit from a dead quota.
fn json_detail(value: &Value) -> Option<String> {
    let error = value.get("error");
    let message = [
        error.and_then(|e| e.get("message")),
        error,
        value.get("message"),
        value.get("detail"),
    ]
    .into_iter()
    .flatten()
    .find_map(non_empty_str)?;
    let code = error
        .and_then(|e| e.get("type").or_else(|| e.get("code")))
        .and_then(non_empty_str);
    Some(match code {
        Some(code) if !message.contains(&code) => format!("{code}: {message}"),
        _ => message,
    })
}

fn non_empty_str(value: &Value) -> Option<String> {
    let text = value.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn is_html(body: &str) -> bool {
    let head: String = body
        .trim_start()
        .chars()
        .take(HTML_SNIFF_CHARS)
        .collect::<String>()
        .to_ascii_lowercase();
    HTML_PREFIXES.iter().any(|prefix| head.starts_with(prefix))
}

fn truncate(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_owned();
    }
    let kept: String = text.chars().take(cap).collect();
    format!("{}{ELLIPSIS}", kept.trim_end())
}

/// `retry-after-ms` first because providers that send both use it for the
/// sub-second precision the seconds form cannot express.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    if let Some(millis) = header(HEADER_RETRY_AFTER_MS)
        .and_then(|value| value.trim().parse::<f64>().ok())
        .and_then(|millis| positive_secs(millis / 1000.0))
    {
        return Some(millis);
    }
    let value = header(HEADER_RETRY_AFTER)?.trim();
    value
        .parse::<f64>()
        .ok()
        .and_then(positive_secs)
        .or_else(|| parse_http_date(value))
}

fn parse_http_date(value: &str) -> Option<Duration> {
    let target = jiff::fmt::rfc2822::parse(value).ok()?.timestamp();
    let delta = target.as_millisecond() - jiff::Timestamp::now().as_millisecond();
    positive_secs(delta as f64 / 1000.0)
}

fn positive_secs(secs: f64) -> Option<Duration> {
    (secs.is_finite() && secs > 0.0)
        .then(|| Duration::from_secs_f64(secs.min(MAX_PARSED_RETRY_AFTER_SECS)))
}

/// A non-200 body is untrusted input on a connection that has already
/// misbehaved, so it gets both a size cap and its own deadline. Without them a
/// server that accepts the request and then dribbles forever parks the agent
/// task with no stream timeout covering it.
async fn read_error_body(body: isahc::AsyncBody) -> String {
    let read = async {
        let mut buf = Vec::new();
        match body.take(ERROR_BODY_CAP).read_to_end(&mut buf).await {
            Ok(_) => String::from_utf8_lossy(&buf).into_owned(),
            Err(_) => UNREADABLE_ERROR_BODY.to_owned(),
        }
    };
    futures_lite::future::or(read, async {
        smol::Timer::after(ERROR_BODY_TIMEOUT).await;
        UNREADABLE_ERROR_BODY.to_owned()
    })
    .await
}

impl<T> From<flume::SendError<T>> for AgentError {
    fn from(_: flume::SendError<T>) -> Self {
        Self::Channel
    }
}

impl From<caudra_storage::StorageError> for AgentError {
    fn from(e: caudra_storage::StorageError) -> Self {
        match e {
            caudra_storage::StorageError::Io(io) => Self::Io(io),
            caudra_storage::StorageError::Json(j) => Self::Json(j),
            other => Self::api(0, other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isahc::http::{HeaderName, HeaderValue};
    use test_case::test_case;

    const BODY: &str = "bad input";
    const STEERING_RULE: &str = "empty_output";
    const STEERING_KIND: &str = "steering_exhausted";
    const STEERING_MESSAGE: &str = "automatic recovery limit reached for empty_output; review the partial output and resume or adjust steering limits";
    const STEERING_DISPLAY: &str = "automatic recovery exhausted for empty_output";

    const ANTHROPIC_RATE_LIMIT: &str = r#"{"type":"error","error":{"type":"rate_limit_error","message":"This request would exceed your organization's rate limit"}}"#;
    const ANTHROPIC_RATE_LIMIT_DETAIL: &str =
        "rate limited: rate_limit_error: This request would exceed your organization's rate limit";
    const OPENAI_QUOTA: &str =
        r#"{"error":{"message":"You exceeded your current quota","code":"insufficient_quota"}}"#;
    const OPENAI_QUOTA_DETAIL: &str =
        "rate limited: insufficient_quota: You exceeded your current quota";
    const GATEWAY_HTML: &str = "<!DOCTYPE html>\n<html><body>502 Bad Gateway</body></html>";
    const REDUNDANT_CODE: &str = r#"{"error":{"code":"overloaded","message":"overloaded, retry"}}"#;
    /// The two texts OpenAI streams as `service_unavailable_error`, which reaches
    /// `AgentError` labelled 400 because the SSE type map has no server status for it.
    const OVERLOADED_TEXT: &str = "Our servers are currently overloaded. Please try again later.";
    const VERIFY_ACCESS_TEXT: &str = "Unable to verify model access right now. Please retry.";
    const PLAIN_BAD_REQUEST: &str = "invalid request";
    const OVERFLOW_TEXT: &str =
        "This model's maximum context length is 200000 tokens. Please try again later.";
    const CREDIT_TEXT: &str = "Your credit balance is too low. Please try again later.";
    const HTTP_DATE_OFFSET_SECS: i64 = 300;
    const HTTP_DATE_TOLERANCE_SECS: u64 = 5;

    #[test_case(STEERING_RULE)]
    fn steering_exhaustion_is_not_a_transport_retry(rule: &str) {
        let error = AgentError::SteeringExhausted { rule: rule.into() };
        assert_eq!(error.kind(), STEERING_KIND);
        assert_eq!(error.user_message(), STEERING_MESSAGE);
        assert_eq!(error.to_string(), STEERING_DISPLAY);
        assert_eq!(error.retry_message(), STEERING_DISPLAY);
        assert!(!error.is_retryable());
        assert!(!error.should_rotate_key());
        assert!(!error.is_context_overflow());
        assert_eq!(error.status(), None);
        assert_eq!(error.retry_after(), None);
    }

    fn api(status: u16) -> AgentError {
        AgentError::api(status, String::new())
    }

    fn api_msg(status: u16, message: &str) -> AgentError {
        AgentError::api(status, message)
    }

    fn rate_limited_after(after: Duration) -> AgentError {
        AgentError::Api {
            status: 429,
            message: String::new(),
            retry_after: Some(after),
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test_case(408, true  ; "request_timeout")]
    #[test_case(429, true  ; "rate_limit")]
    #[test_case(500, true  ; "server_error")]
    #[test_case(529, true  ; "overloaded")]
    #[test_case(400, false ; "bad_request")]
    #[test_case(401, false ; "unauthorized")]
    fn api_retryable(status: u16, expected: bool) {
        assert_eq!(api(status).is_retryable(), expected);
    }

    #[test_case(400, OVERLOADED_TEXT, true   ; "overloaded_behind_a_400")]
    #[test_case(400, VERIFY_ACCESS_TEXT, true ; "please_retry_behind_a_400")]
    #[test_case(400, PLAIN_BAD_REQUEST, false ; "plain_bad_request")]
    #[test_case(400, OVERFLOW_TEXT, false     ; "overflow_outranks_transient_wording")]
    #[test_case(402, CREDIT_TEXT, false       ; "billing_never_transient")]
    #[test_case(401, OVERLOADED_TEXT, false   ; "auth_never_transient")]
    fn transient_wording_overrides_a_misleading_status(status: u16, message: &str, expected: bool) {
        assert_eq!(api_msg(status, message).is_retryable(), expected);
    }

    #[test_case(401, true  ; "unauthorized")]
    #[test_case(403, false ; "forbidden")]
    fn api_auth_error(status: u16, expected: bool) {
        assert_eq!(api(status).is_auth_error(), expected);
    }

    #[test_case(404, r#"{"error":{"code":"model_not_found","message":"model 'gpt-5.6-luna' not found","param":"model","type":"invalid_request_error"}}"#, true ; "structured_code")]
    #[test_case(404, "model 'qwen' not found", true ; "named_model_not_found")]
    #[test_case(400, "unknown model qwen", true ; "unknown_model")]
    #[test_case(404, "The model `qwen` does not exist or you do not have access", true ; "model_does_not_exist")]
    #[test_case(404, "model qwen is not available", true ; "model_not_available")]
    #[test_case(404, "route not found", false ; "generic_not_found")]
    #[test_case(400, "Feature not available for this model", false ; "feature_unavailable")]
    #[test_case(400, "This model is not available for this feature", false ; "model_feature_unavailable")]
    #[test_case(404, r#"{"model":"qwen","message":"organization not found"}"#, false ; "unrelated_json_field")]
    #[test_case(404, r#"{"error":{"message":""},"message":"model qwen not found"}"#, true ; "empty_nested_message")]
    #[test_case(429, "rate limit exceeded", false ; "rate_limit")]
    fn model_unavailable_api_error(status: u16, message: &str, expected: bool) {
        assert_eq!(api_msg(status, message).is_model_unavailable(), expected);
    }

    #[test]
    fn non_api_error_is_not_model_unavailable() {
        assert!(
            !AgentError::Config {
                message: "model qwen not found".into(),
            }
            .is_model_unavailable()
        );
    }

    #[test_case(429, "Rate limited"        ; "rate_limited")]
    #[test_case(529, "Provider is overloaded" ; "overloaded")]
    #[test_case(500, "Server error (500)"  ; "server_error")]
    fn retry_message_api(status: u16, expected: &str) {
        assert_eq!(api(status).retry_message(), expected);
    }

    #[test_case(429, RATE_LIMIT_FALLBACK  ; "user_msg_429")]
    #[test_case(529, OVERLOADED_FALLBACK  ; "user_msg_529")]
    #[test_case(500, "server error (500)" ; "user_msg_500")]
    #[test_case(400, "API error (400)"    ; "user_msg_400")]
    fn user_message_falls_back_without_detail(status: u16, expected: &str) {
        assert_eq!(api(status).user_message(), expected);
    }

    #[test]
    fn user_message_401_always_carries_the_login_hint() {
        assert_eq!(api(401).user_message(), format!("{AUTH_LABEL}{AUTH_HINT}"));
        assert_eq!(
            api_msg(401, BODY).user_message(),
            format!("{AUTH_LABEL}: {BODY}{AUTH_HINT}")
        );
    }

    #[test_case(429, BODY, "rate limited: bad input"             ; "plain_body")]
    #[test_case(529, BODY, "provider is overloaded: bad input"   ; "overloaded")]
    #[test_case(500, BODY, "server error (500): bad input"       ; "server_error")]
    #[test_case(400, BODY, "API error (400): bad input"          ; "other_status")]
    #[test_case(429, ANTHROPIC_RATE_LIMIT, ANTHROPIC_RATE_LIMIT_DETAIL ; "anthropic_nested")]
    #[test_case(429, OPENAI_QUOTA, OPENAI_QUOTA_DETAIL           ; "openai_code")]
    #[test_case(429, r#"{"error":"flat string"}"#, "rate limited: flat string" ; "flat_error_string")]
    #[test_case(429, r#"{"message":"top level"}"#, "rate limited: top level"   ; "top_level_message")]
    #[test_case(529, REDUNDANT_CODE, "provider is overloaded: overloaded, retry" ; "code_not_duplicated")]
    #[test_case(429, r#"{"unrelated":1}"#, r#"rate limited: {"unrelated":1}"#   ; "unrecognised_json_kept_raw")]
    fn user_message_extracts_provider_detail(status: u16, body: &str, expected: &str) {
        assert_eq!(api_msg(status, body).user_message(), expected);
    }

    #[test_case(401, GATEWAY_401 ; "unauthorized")]
    #[test_case(403, GATEWAY_403 ; "forbidden")]
    fn html_body_yields_gateway_guidance(status: u16, expected: &str) {
        assert_eq!(api_msg(status, GATEWAY_HTML).user_message(), expected);
    }

    #[test]
    fn html_body_is_never_shown_as_detail() {
        let message = api_msg(500, GATEWAY_HTML).user_message();
        assert_eq!(message, "server error (500)");
        assert!(!message.contains('<'));
    }

    #[test]
    fn detail_is_truncated_to_the_cap() {
        let body = "x".repeat(DETAIL_CAP * 2);
        let message = api_msg(429, &body).user_message();
        assert!(message.ends_with(ELLIPSIS));
        assert_eq!(
            message.chars().count(),
            RATE_LIMIT_LABEL.chars().count() + ": ".len() + DETAIL_CAP + 1
        );
    }

    #[test]
    fn retry_message_uses_the_shorter_cap() {
        let body = "x".repeat(DETAIL_CAP * 2);
        let err = api_msg(429, &body);
        assert!(err.retry_message().chars().count() < err.user_message().chars().count());
        assert_eq!(
            err.retry_message().chars().count(),
            RETRY_RATE_LIMIT_LABEL.chars().count() + ": ".len() + RETRY_DETAIL_CAP + 1
        );
    }

    #[test]
    fn retry_message_carries_detail() {
        assert_eq!(
            api_msg(429, ANTHROPIC_RATE_LIMIT).retry_message(),
            "Rate limited: rate_limit_error: This request would exceed your organization's rate limit"
        );
    }

    #[test_case(&[("retry-after", "3")], 3_000                        ; "seconds")]
    #[test_case(&[("retry-after", "1.5")], 1_500                      ; "fractional_seconds")]
    #[test_case(&[("retry-after-ms", "250")], 250                     ; "millis")]
    #[test_case(&[("retry-after-ms", "250"), ("retry-after", "9")], 250 ; "millis_wins")]
    fn parse_retry_after_reads_the_hint(pairs: &[(&str, &str)], expected_millis: u64) {
        assert_eq!(
            parse_retry_after(&headers(pairs)),
            Some(Duration::from_millis(expected_millis))
        );
    }

    #[test_case(&[]                              ; "absent")]
    #[test_case(&[("retry-after", "later")]      ; "garbage")]
    #[test_case(&[("retry-after", "0")]          ; "zero")]
    #[test_case(&[("retry-after", "-5")]         ; "negative")]
    #[test_case(&[("retry-after", "Mon, 1 Jan 2001 00:00:00 +0000")] ; "past_date")]
    fn parse_retry_after_rejects_unusable_values(pairs: &[(&str, &str)]) {
        assert_eq!(parse_retry_after(&headers(pairs)), None);
    }

    #[test]
    fn parse_retry_after_reads_http_dates() {
        let target =
            jiff::Timestamp::now() + jiff::SignedDuration::from_secs(HTTP_DATE_OFFSET_SECS);
        let formatted =
            jiff::fmt::rfc2822::to_string(&target.to_zoned(jiff::tz::TimeZone::UTC)).unwrap();
        let parsed = parse_retry_after(&headers(&[("retry-after", &formatted)])).unwrap();
        let offset = Duration::from_secs(HTTP_DATE_OFFSET_SECS as u64);
        assert!(parsed <= offset);
        assert!(parsed > offset - Duration::from_secs(HTTP_DATE_TOLERANCE_SECS));
    }

    #[test]
    fn retry_after_is_only_carried_by_api_errors() {
        assert_eq!(api(429).retry_after(), None);
        assert_eq!(AgentError::Timeout { secs: 30 }.retry_after(), None);
        assert_eq!(
            rate_limited_after(Duration::from_secs(7)).retry_after(),
            Some(Duration::from_secs(7))
        );
    }

    #[test_case(1, "rate limited, try again in a moment (retry after 1s)"  ; "seconds")]
    #[test_case(90, "rate limited, try again in a moment (retry after 1m)" ; "minutes")]
    #[test_case(7_200, "rate limited, try again in a moment (retry after 2h)" ; "hours")]
    fn user_message_names_the_window_the_provider_asked_for(secs: u64, expected: &str) {
        assert_eq!(
            rate_limited_after(Duration::from_secs(secs)).user_message(),
            expected
        );
    }

    #[test]
    fn timeout_is_retryable() {
        assert!(AgentError::Timeout { secs: 30 }.is_retryable());
    }

    // llama.cpp: https://github.com/ggml-org/llama.cpp/blob/master/tools/server/server-context.cpp
    #[test_case(400, "request (268914 tokens) exceeds the available context size (262144 tokens)", true   ; "llama_cpp_overshoot")]
    // OpenAI: https://platform.openai.com/docs/guides/error-codes
    #[test_case(400, "Input exceeds context limit", true                                                 ; "openai_style")]
    // OpenAI: https://platform.openai.com/docs/guides/error-codes
    #[test_case(400, "This model's maximum context length is 8192 tokens. However, you requested 9850 tokens", true ; "openai_max_context")]
    // Gemini: https://ai.google.dev/gemini-api/docs/troubleshooting
    #[test_case(400, "The input token count exceeds the maximum number of tokens allowed", true           ; "gemini_exceeds")]
    // Gemini: https://ai.google.dev/gemini-api/docs/troubleshooting
    #[test_case(400, "Request contains too many tokens. Please reduce the input size.", true              ; "gemini_too_many")]
    // Gemini: https://ai.google.dev/gemini-api/docs/troubleshooting
    #[test_case(400, "Your input context is too long.", true                                              ; "gemini_500_input")]
    // Ollama: https://docs.ollama.com/api/errors
    #[test_case(400, "context length exceeded", true                                                      ; "ollama")]
    // Anthropic: https://docs.anthropic.com/en/docs/errors
    #[test_case(413, "prompt is too long", true                                                           ; "anthropic_413")]
    // HTTP 413: https://www.rfc-editor.org/rfc/rfc9110.html#name-413-content-too-large
    #[test_case(413, "Payload too large", true                                                            ; "generic_413")]
    // DeepSeek: https://api-docs.deepseek.com/quick_start/pricing
    #[test_case(400, "This model's maximum context length is 131072 tokens. However, you requested 168754 tokens", true ; "deepseek")]
    // Mistral: https://docs.mistral.ai/resources/known-limitations
    #[test_case(400, "Prompt contains 321774 tokens and 0 draft tokens, too large for model with 262144 maximum context length", true ; "mistral")]
    // OpenRouter: https://openrouter.ai/docs/api/reference/errors-and-debugging.mdx
    #[test_case(400, "This endpoint's maximum context length is 200000 tokens. However, you requested about 5028244 tokens", true ; "openrouter")]
    // Bedrock: https://repost.aws/knowledge-center/bedrock-validation-exception-errors
    #[test_case(400, "Input is too long for requested model.", true                                                          ; "bedrock")]
    #[test_case(400, "Input is too long for the model", true                                              ; "too_long_input")]
    // Anthropic: long-context entitlement refusals name the feature, not the size
    #[test_case(400, "Extra usage is required for long context requests", true                            ; "anthropic_long_context_entitlement")]
    #[test_case(400, "The long context beta is not yet available for this account", true                  ; "anthropic_long_context_beta")]
    #[test_case(400, "You're out of extra usage for long context requests", false                         ; "anthropic_extra_usage_exhausted")]
    #[test_case(400, "Rate limit exceeded", false                                                         ; "not_context")]
    #[test_case(400, "Invalid API key", false                                                             ; "auth_error")]
    #[test_case(500, "Internal server error", false                                                       ; "server_error")]
    #[test_case(400, "The output is too long", false                                                      ; "output_not_context")]
    fn is_context_overflow(status: u16, message: &str, expected: bool) {
        assert_eq!(api_msg(status, message).is_context_overflow(), expected);
    }

    #[test]
    fn context_overflow_is_not_retryable() {
        let err = api_msg(400, "request exceeds the available context size");
        assert!(err.is_context_overflow());
        assert!(!err.is_retryable());
    }
}
