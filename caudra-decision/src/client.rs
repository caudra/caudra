use std::io::ErrorKind;
use std::time::{Duration, Instant, SystemTime};

use async_io::Timer;
use async_trait::async_trait;
use futures_lite::{future, io::AsyncReadExt};
use isahc::config::{Configurable, RedirectPolicy};
use isahc::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use isahc::{HttpClient, Request};
use jiff::fmt::rfc2822::DateTimeParser;
use url::Url;

use crate::engine::{DecisionEngine, DecisionError, check_deadline};
use crate::question_set::bounded_json;
use crate::wire::{DecisionRequest, DecisionResponse, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES};

const MAX_RETRIES: u32 = 2;
const BACKOFF_INITIAL: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
const BACKOFF_MULTIPLIER: u32 = 2;
const BACKOFF_JITTER: f64 = 0.25;
const RETRY_AFTER_MS: &str = "retry-after-ms";
const MILLIS_PER_SECOND: f64 = 1_000.0;
static HTTP_DATE: DateTimeParser = DateTimeParser::new();

pub struct HttpDecisionClient {
    client: HttpClient,
    endpoint: Uri,
    authorization: Option<HeaderValue>,
}

impl HttpDecisionClient {
    pub fn new(endpoint: &str, api_key: Option<&str>) -> Result<Self, DecisionError> {
        let url = Url::parse(endpoint).map_err(|_| {
            DecisionError::Rejected("endpoint must be an absolute HTTP or HTTPS URL")
        })?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(DecisionError::Rejected(
                "endpoint must use HTTP or HTTPS without userinfo or a fragment",
            ));
        }
        let endpoint = url
            .as_str()
            .parse()
            .map_err(|_| DecisionError::Rejected("endpoint is not a valid HTTP URI"))?;
        let authorization = api_key
            .map(|key| {
                if key.trim().is_empty() {
                    return Err(DecisionError::Rejected(
                        "bearer credential must not be empty",
                    ));
                }
                let mut value = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| {
                    DecisionError::Rejected("bearer credential is not a valid HTTP header")
                })?;
                value.set_sensitive(true);
                Ok(value)
            })
            .transpose()?;
        let client = HttpClient::builder()
            .proxy(None)
            .redirect_policy(RedirectPolicy::None)
            .build()
            .map_err(|_| DecisionError::Unreachable)?;
        Ok(Self {
            client,
            endpoint,
            authorization,
        })
    }

    async fn send(
        &self,
        request: &DecisionRequest,
        deadline: Instant,
    ) -> Result<DecisionResponse, DecisionError> {
        let body = bounded_json(request, MAX_REQUEST_BYTES)?;
        let mut retries = 0;
        loop {
            check_deadline(deadline)?;
            let mut builder = Request::post(self.endpoint.clone())
                .timeout(deadline.saturating_duration_since(Instant::now()))
                .redirect_policy(RedirectPolicy::None)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json");
            if let Some(authorization) = &self.authorization {
                builder = builder.header(header::AUTHORIZATION, authorization);
            }
            let http_request = builder
                .body(body.clone())
                .map_err(|_| DecisionError::Rejected("request cannot be constructed"))?;
            let response = self
                .client
                .send_async(http_request)
                .await
                .map_err(|error| {
                    if error.is_timeout() {
                        DecisionError::Timeout
                    } else {
                        DecisionError::Unreachable
                    }
                })?;
            let status = response.status();
            if !status.is_success() {
                let failure = DecisionError::Http {
                    status: status.as_u16(),
                };
                if retries == MAX_RETRIES || !is_retryable(status) {
                    return Err(failure);
                }
                let retry_at = retry_delay(
                    response.headers(),
                    retries,
                    SystemTime::now(),
                    fastrand::f64(),
                )
                .and_then(|delay| Instant::now().checked_add(delay))
                .filter(|at| *at < deadline)
                .ok_or(failure)?;
                drop(response);
                Timer::at(retry_at).await;
                retries += 1;
                continue;
            }
            if response
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
            {
                return Err(DecisionError::Invalid("response exceeds the byte limit"));
            }
            let mut bytes = Vec::new();
            response
                .into_body()
                .take(MAX_RESPONSE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|error| {
                    if error.kind() == ErrorKind::TimedOut || Instant::now() >= deadline {
                        DecisionError::Timeout
                    } else {
                        DecisionError::Unreachable
                    }
                })?;
            if bytes.len() > MAX_RESPONSE_BYTES {
                return Err(DecisionError::Invalid("response exceeds the byte limit"));
            }
            check_deadline(deadline)?;
            let response: DecisionResponse = serde_json::from_slice(&bytes).map_err(|_| {
                DecisionError::Invalid("response does not match the decision JSON schema")
            })?;
            response.validate_for(request)?;
            check_deadline(deadline)?;
            return Ok(response);
        }
    }
}

fn is_retryable(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
    ) || status.is_server_error()
}

/// Server retry headers win over backoff. One that cannot be read ends the
/// retries rather than risk retrying sooner than the server asked.
fn retry_delay(
    headers: &HeaderMap,
    retries: u32,
    now: SystemTime,
    jitter: f64,
) -> Option<Duration> {
    if let Some(value) = headers.get(RETRY_AFTER_MS) {
        let millis: f64 = value.to_str().ok()?.trim().parse().ok()?;
        return Duration::try_from_secs_f64(millis / MILLIS_PER_SECOND).ok();
    }
    let Some(value) = headers.get(header::RETRY_AFTER) else {
        return Some(backoff(retries, jitter));
    };
    let value = value.to_str().ok()?.trim();
    match value.parse() {
        Ok(seconds) => Duration::try_from_secs_f64(seconds).ok(),
        Err(_) => {
            let retry_at = SystemTime::from(HTTP_DATE.parse_timestamp(value).ok()?);
            Some(retry_at.duration_since(now).unwrap_or_default())
        }
    }
}

fn backoff(retries: u32, jitter: f64) -> Duration {
    BACKOFF_INITIAL
        .saturating_mul(BACKOFF_MULTIPLIER.saturating_pow(retries))
        .min(BACKOFF_MAX)
        .mul_f64(1.0 - BACKOFF_JITTER * jitter)
}

#[async_trait]
impl DecisionEngine for HttpDecisionClient {
    async fn decide(
        &self,
        request: &DecisionRequest,
        deadline: Instant,
    ) -> Result<DecisionResponse, DecisionError> {
        check_deadline(deadline)?;
        request.validate()?;
        future::race(self.send(request, deadline), async {
            Timer::at(deadline).await;
            Err(DecisionError::Timeout)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::process::Command;
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant, UNIX_EPOCH};

    use async_io::{Async, Timer};
    use futures_lite::future;
    use isahc::http::{HeaderMap, HeaderName, HeaderValue};
    use test_case::test_case;

    use super::{BACKOFF_INITIAL, BACKOFF_MAX, HttpDecisionClient, backoff, retry_delay};
    use crate::engine::{DecisionEngine, DecisionError};
    use crate::wire::{
        DecisionResponse, MAX_RESPONSE_BYTES,
        tests::{request, response},
    };

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);
    const STREAM_DEADLINE: Duration = Duration::from_millis(200);
    const TEST_TOKEN: &str = "test-bearer-not-a-secret";
    const STATUS_OK: u16 = 200;
    const STATUS_REDIRECT: u16 = 307;
    const STATUS_UNAUTHORIZED: u16 = 401;
    const STATUS_UNAVAILABLE: u16 = 503;
    const RETRY_NOW: &str = "Retry-After: 0\r\n";
    const NOW_UNIX_SECS: u64 = 1_445_412_477;
    const LATER_HTTP_DATE: &str = "Wed, 21 Oct 2015 07:28:00 GMT";
    const EARLIER_HTTP_DATE: &str = "Wed, 21 Oct 2015 07:27:00 GMT";
    const DIRECT_REQUEST_TEST: &str = "client::tests::sends_full_endpoint_bearer_and_wire_body";

    struct Server {
        endpoint: String,
        requests: Arc<Mutex<Vec<String>>>,
        task: JoinHandle<()>,
    }

    fn read_request(stream: &mut TcpStream) -> String {
        stream.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(TEST_TIMEOUT)).unwrap();
        let mut reader = BufReader::new(stream);
        let mut headers = String::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap();
            }
            headers.push_str(&line);
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        headers.push_str(&String::from_utf8(body).unwrap());
        headers
    }

    fn server(responses: Vec<String>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let task = thread::spawn(move || {
            for response in responses {
                let mut stream = accept(&listener);
                recorded.lock().unwrap().push(read_request(&mut stream));
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Server {
            endpoint,
            requests,
            task,
        }
    }

    fn accept(listener: &TcpListener) -> TcpStream {
        let listener = Async::new(listener.try_clone().unwrap()).unwrap();
        let (stream, _) = future::block_on(future::race(listener.accept(), async {
            Timer::after(TEST_TIMEOUT).await;
            Err(io::Error::other("test server did not receive a connection"))
        }))
        .unwrap();
        let stream = stream.into_inner().unwrap();
        stream.set_nonblocking(false).unwrap();
        stream
    }

    fn http_response(status: u16, body: &str, headers: &str) -> String {
        format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
            body.len()
        )
    }

    fn success() -> String {
        http_response(STATUS_OK, &serde_json::to_string(&response()).unwrap(), "")
    }

    fn client(endpoint: &str) -> HttpDecisionClient {
        HttpDecisionClient::new(endpoint, Some(TEST_TOKEN)).unwrap()
    }

    #[test]
    fn ambient_proxies_cannot_redirect_decision_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let mut command = Command::new(env::current_exe().unwrap());
        command.args(["--exact", DIRECT_REQUEST_TEST, "--nocapture"]);
        for name in [
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
        ] {
            command.env(name, &proxy);
        }
        command.env("no_proxy", "").env("NO_PROXY", "");
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        listener.set_nonblocking(true).unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn sends_full_endpoint_bearer_and_wire_body() {
        let server = server(vec![success()]);
        let result = future::block_on(
            client(&server.endpoint).decide(&request(), Instant::now() + TEST_TIMEOUT),
        );
        server.task.join().unwrap();
        assert_eq!(result.unwrap(), response());
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let sent = &requests[0];
        assert!(sent.starts_with("POST /v1/systemone HTTP/1.1"));
        assert!(
            sent.to_ascii_lowercase()
                .contains(&format!("authorization: bearer {TEST_TOKEN}"))
        );
        assert!(sent.contains(&serde_json::to_string(&request()).unwrap()));
    }

    fn decide_with(replies: Vec<String>) -> (Result<DecisionResponse, DecisionError>, usize) {
        let server = server(replies);
        let result = future::block_on(
            client(&server.endpoint).decide(&request(), Instant::now() + TEST_TIMEOUT),
        );
        server.task.join().unwrap();
        let attempts = server.requests.lock().unwrap().len();
        (result, attempts)
    }

    #[test_case(408; "request_timeout")]
    #[test_case(429; "rate_limited")]
    #[test_case(500; "internal_error")]
    #[test_case(503; "unavailable")]
    #[test_case(529; "overloaded")]
    fn retries_sdk_statuses(status: u16) {
        let (result, attempts) =
            decide_with(vec![http_response(status, "ignored", RETRY_NOW), success()]);
        assert_eq!(result, Ok(response()));
        assert_eq!(attempts, 2);
    }

    #[test_case(400; "bad_request")]
    #[test_case(401; "unauthorized")]
    #[test_case(404; "not_found")]
    #[test_case(422; "unprocessable")]
    fn client_errors_are_not_retried(status: u16) {
        let (result, attempts) = decide_with(vec![http_response(status, "", RETRY_NOW)]);
        assert_eq!(result, Err(DecisionError::Http { status }));
        assert_eq!(attempts, 1);
    }

    #[test]
    fn stops_after_two_retries() {
        let unavailable = http_response(STATUS_UNAVAILABLE, "ignored", RETRY_NOW);
        let (result, attempts) = decide_with(vec![unavailable; 3]);
        assert_eq!(
            result,
            Err(DecisionError::Http {
                status: STATUS_UNAVAILABLE
            })
        );
        assert_eq!(attempts, 3);
    }

    #[test_case("Retry-After: 3600\r\n"; "beyond_deadline")]
    #[test_case("Retry-After: soon\r\n"; "unreadable_delay")]
    fn retry_the_server_does_not_allow_returns_its_status(headers: &str) {
        let (result, attempts) = decide_with(vec![http_response(STATUS_UNAVAILABLE, "", headers)]);
        assert_eq!(
            result,
            Err(DecisionError::Http {
                status: STATUS_UNAVAILABLE
            })
        );
        assert_eq!(attempts, 1);
    }

    #[test]
    fn retry_after_ms_takes_precedence() {
        let throttled = http_response(
            STATUS_UNAVAILABLE,
            "",
            "retry-after-ms: 0\r\nRetry-After: 3600\r\n",
        );
        let (result, attempts) = decide_with(vec![throttled, success()]);
        assert_eq!(result, Ok(response()));
        assert_eq!(attempts, 2);
    }

    #[test_case(&[], Some(BACKOFF_INITIAL); "backoff_without_headers")]
    #[test_case(&[("retry-after-ms", "250")], Some(Duration::from_millis(250)); "milliseconds")]
    #[test_case(&[("retry-after-ms", "1.5")], Some(Duration::from_micros(1_500)); "fractional_milliseconds")]
    #[test_case(&[("retry-after-ms", "-1")], None; "negative_milliseconds")]
    #[test_case(&[("retry-after-ms", "0"), ("retry-after", "3600")], Some(Duration::ZERO); "milliseconds_first")]
    #[test_case(&[("retry-after", "2")], Some(Duration::from_secs(2)); "seconds")]
    #[test_case(&[("retry-after", "0.25")], Some(Duration::from_millis(250)); "decimal_seconds")]
    #[test_case(&[("retry-after", LATER_HTTP_DATE)], Some(Duration::from_secs(3)); "future_http_date")]
    #[test_case(&[("retry-after", EARLIER_HTTP_DATE)], Some(Duration::ZERO); "past_http_date")]
    #[test_case(&[("retry-after", "soon")], None; "unreadable")]
    fn retry_delay_honors_server_headers(pairs: &[(&str, &str)], expected: Option<Duration>) {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        let now = UNIX_EPOCH + Duration::from_secs(NOW_UNIX_SECS);
        assert_eq!(retry_delay(&headers, 0, now, 0.0), expected);
    }

    #[test_case(0, 0.0, Duration::from_millis(500); "initial")]
    #[test_case(1, 0.0, Duration::from_secs(1); "doubled")]
    #[test_case(2, 0.0, Duration::from_secs(2); "doubled_twice")]
    #[test_case(4, 0.0, BACKOFF_MAX; "capped")]
    #[test_case(0, 0.5, Duration::from_micros(437_500); "half_jitter")]
    #[test_case(1, 1.0, Duration::from_millis(750); "full_jitter")]
    fn backoff_doubles_to_its_cap_less_jitter(retries: u32, jitter: f64, expected: Duration) {
        assert_eq!(backoff(retries, jitter), expected);
    }

    #[test]
    fn does_not_follow_redirects_with_credentials() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let location = format!(
            "Location: http://{}/stolen\r\n",
            target.local_addr().unwrap()
        );
        let server = server(vec![http_response(STATUS_REDIRECT, "", &location)]);
        let result = future::block_on(
            client(&server.endpoint).decide(&request(), Instant::now() + TEST_TIMEOUT),
        );
        server.task.join().unwrap();
        assert_eq!(
            result,
            Err(DecisionError::Http {
                status: STATUS_REDIRECT
            })
        );
        assert!(target.accept().is_err());
    }

    #[test]
    fn http_errors_do_not_expose_response_body() {
        let server = server(vec![http_response(STATUS_UNAUTHORIZED, TEST_TOKEN, "")]);
        let error = future::block_on(
            client(&server.endpoint).decide(&request(), Instant::now() + TEST_TIMEOUT),
        )
        .unwrap_err();
        server.task.join().unwrap();
        assert_eq!(
            error,
            DecisionError::Http {
                status: STATUS_UNAUTHORIZED
            }
        );
        assert!(!format!("{error:?} {error}").contains(TEST_TOKEN));
    }

    #[test_case(false; "declared_length")]
    #[test_case(true; "streamed_without_length")]
    fn bounds_response_bytes(streamed: bool) {
        let body = " ".repeat(MAX_RESPONSE_BYTES + 1);
        let reply = if streamed {
            format!("HTTP/1.1 {STATUS_OK} Test\r\nConnection: close\r\n\r\n{body}")
        } else {
            http_response(STATUS_OK, &body, "")
        };
        let server = server(vec![reply]);
        let result = future::block_on(
            client(&server.endpoint).decide(&request(), Instant::now() + TEST_TIMEOUT),
        );
        server.task.join().unwrap();
        assert!(matches!(result, Err(DecisionError::Invalid(_))));
    }

    #[test_case("not json"; "invalid_json")]
    #[test_case(r#"{"model":"m","answers":{},"usage":{"input_tokens":0,"output_tokens":0}}"#; "missing_answers")]
    fn rejects_malformed_success(body: &str) {
        let server = server(vec![http_response(STATUS_OK, body, "")]);
        let result = future::block_on(
            client(&server.endpoint).decide(&request(), Instant::now() + TEST_TIMEOUT),
        );
        server.task.join().unwrap();
        assert!(matches!(result, Err(DecisionError::Invalid(_))));
    }

    #[test_case(false; "waiting_for_headers")]
    #[test_case(true; "waiting_for_body")]
    fn deadline_includes_response_streaming(headers: bool) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        let (release, hold) = mpsc::channel();
        let task = thread::spawn(move || {
            let mut stream = accept(&listener);
            read_request(&mut stream);
            if headers {
                let _ = stream.write_all(b"HTTP/1.1 200 Test\r\nContent-Length: 100\r\n\r\n");
            }
            hold.recv_timeout(TEST_TIMEOUT).unwrap();
        });
        let client = client(&endpoint);
        let deadline = Instant::now() + STREAM_DEADLINE;
        let result = future::block_on(client.decide(&request(), deadline));
        release.send(()).unwrap();
        task.join().unwrap();
        assert_eq!(result, Err(DecisionError::Timeout));
    }

    #[test]
    fn expired_deadline_does_not_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        let result = future::block_on(client(&endpoint).decide(&request(), Instant::now()));
        assert_eq!(result, Err(DecisionError::Timeout));
        assert!(listener.accept().is_err());
    }

    #[test_case("file:///tmp/not-http"; "scheme")]
    #[test_case("https://username:password@example.test/v1/systemone"; "userinfo")]
    #[test_case("https://example.test/v1/systemone#fragment"; "fragment")]
    fn rejects_unsafe_endpoint(endpoint: &str) {
        assert!(matches!(
            HttpDecisionClient::new(endpoint, None),
            Err(DecisionError::Rejected(_))
        ));
    }
}
