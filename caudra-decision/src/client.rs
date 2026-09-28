use std::io::ErrorKind;
use std::time::{Duration, Instant};

use async_io::Timer;
use async_trait::async_trait;
use futures_lite::{future, io::AsyncReadExt};
use isahc::config::{Configurable, RedirectPolicy};
use isahc::http::{HeaderValue, Uri, header};
use isahc::{HttpClient, Request};
use url::Url;

use crate::engine::{DecisionEngine, DecisionError, check_deadline};
use crate::question_set::bounded_json;
use crate::wire::{DecisionRequest, DecisionResponse, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES};

const RETRY_DELAY: Duration = Duration::from_millis(25);
const MAX_ATTEMPTS: usize = 2;

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
        for attempt in 0..MAX_ATTEMPTS {
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
            let status = response.status().as_u16();
            if attempt == 0 && matches!(status, 429 | 503 | 529) {
                let delay = response
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(RETRY_DELAY);
                drop(response);
                let Some(retry_at) = Instant::now()
                    .checked_add(delay)
                    .filter(|at| *at < deadline)
                else {
                    return Err(DecisionError::Timeout);
                };
                Timer::at(retry_at).await;
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(DecisionError::Http { status });
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
        Err(DecisionError::Timeout)
    }
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
    use std::time::{Duration, Instant};

    use async_io::{Async, Timer};
    use futures_lite::future;
    use test_case::test_case;

    use super::HttpDecisionClient;
    use crate::engine::{DecisionEngine, DecisionError};
    use crate::wire::{
        MAX_RESPONSE_BYTES,
        tests::{request, response},
    };

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);
    const STREAM_DEADLINE: Duration = Duration::from_millis(200);
    const TEST_TOKEN: &str = "test-bearer-not-a-secret";
    const STATUS_OK: u16 = 200;
    const STATUS_REDIRECT: u16 = 307;
    const STATUS_UNAUTHORIZED: u16 = 401;
    const STATUS_UNAVAILABLE: u16 = 503;
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

    #[test_case(429; "rate_limited")]
    #[test_case(503; "unavailable")]
    #[test_case(529; "overloaded")]
    fn retries_transient_status_once(status: u16) {
        let server = server(vec![
            http_response(status, "ignored", "Retry-After: 0\r\n"),
            success(),
        ]);
        let result = future::block_on(
            client(&server.endpoint).decide(&request(), Instant::now() + TEST_TIMEOUT),
        );
        server.task.join().unwrap();
        assert!(result.is_ok());
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn never_retries_a_third_time() {
        let unavailable = http_response(STATUS_UNAVAILABLE, "ignored", "Retry-After: 0\r\n");
        let server = server(vec![unavailable.clone(), unavailable]);
        let result = future::block_on(
            client(&server.endpoint).decide(&request(), Instant::now() + TEST_TIMEOUT),
        );
        server.task.join().unwrap();
        assert_eq!(
            result,
            Err(DecisionError::Http {
                status: STATUS_UNAVAILABLE
            })
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn rejects_retry_that_cannot_fit_deadline() {
        let server = server(vec![http_response(
            STATUS_UNAVAILABLE,
            "",
            "Retry-After: 3600\r\n",
        )]);
        let result = future::block_on(
            client(&server.endpoint).decide(&request(), Instant::now() + TEST_TIMEOUT),
        );
        server.task.join().unwrap();
        assert_eq!(result, Err(DecisionError::Timeout));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
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
    #[test_case(r#"{"answers":{},"usage":{"input_tokens":0,"output_tokens":0}}"#; "missing_answers")]
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
