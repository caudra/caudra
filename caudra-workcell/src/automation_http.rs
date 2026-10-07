//! The [`HttpClient`] behind automations' `http()`. Workcell's bounded client sends each request
//! once, follows only same-origin redirects, and checks every hop against the public-internet
//! policy, or a private-network one when the session allows it. Requests run on a runtime of their
//! own that serves every session for the life of the process, so remote and sandboxed sessions
//! send theirs from this machine too.

use std::future::{Future, ready};
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};
use std::time::Duration;

use caudra_agent::automation::http::{
    HttpAnswer, HttpCall, HttpClient, HttpError, HttpErrorKind, HttpFuture, INVALID_HEADER_NAME,
    INVALID_HEADER_VALUE, TIMED_OUT,
};
use caudra_automation::host::HttpMethod;
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::warn;
use workcell::net::bytes::Bytes;
use workcell::net::http::{HeaderMap, HeaderName, HeaderValue, Method};
use workcell::net::{
    BoundedResponse, FetchOptions, HttpClient as NetClient, NetError, OperatorConfiguredPolicy,
    ProxyConfiguration, RedirectScope, RequestSpec, ReqwestTransport, RetryPolicy,
    TokioDnsResolver, UrlPolicy, UrlPolicyError,
};

use crate::ambient_proxy;

const RUNTIME_THREAD_NAME: &str = "caudra-automation-http";
/// Requests spend their time waiting on the network, so two workers serve every session's.
const RUNTIME_WORKERS: usize = 2;
/// What `[automations] allow_private_network` adds to the public internet: loopback and private
/// addresses, and special-use names such as `localhost`. Credentials in a URL stay refused.
const PRIVATE_NETWORK: UrlPolicy = UrlPolicy::OperatorConfigured(OperatorConfiguredPolicy {
    allow_non_public_ips: true,
    allow_special_use_names: true,
    allow_url_credentials: false,
});
const REFUSED_BY_POLICY: &str = "the network policy refused the target";
const PROXY_REFUSED: &str = "the outbound proxy refused the request or could not complete it";
const REDIRECT_FAILED: &str = "a redirect had no usable target or went past the redirect limit";
const TRANSPORT_FAILED: &str = "the request failed";
const TASK_FAILED: &str = "the request ended without an answer";

/// The process's client and the runtime its requests run on, which as a static is never dropped.
static PROCESS_CLIENT: LazyLock<Option<(Runtime, Arc<dyn HttpClient>)>> =
    LazyLock::new(process_client);

/// Sends automations' `http()` requests through Workcell on a Tokio runtime.
pub struct AutomationHttpClient {
    runtime: Handle,
    public: NetClient,
    private: NetClient,
}

/// A request running on the client's runtime. Dropping it cancels the request, wherever it is.
struct InFlight {
    task: JoinHandle<Result<BoundedResponse, NetError>>,
    cancellation: CancellationToken,
    timeout: Duration,
}

/// The client every session's automations share, built on first use behind the proxy the web
/// tools use. `None` when its runtime cannot start, which leaves `http()` unavailable.
pub fn automation_http_client() -> Option<Arc<dyn HttpClient>> {
    PROCESS_CLIENT
        .as_ref()
        .map(|(_, client)| Arc::clone(client))
}

impl AutomationHttpClient {
    /// Runs every request on `runtime`, through `proxy` where it routes one.
    pub fn new(runtime: Handle, proxy: ProxyConfiguration) -> Self {
        let client = |policy| {
            NetClient::new(
                policy,
                Arc::new(TokioDnsResolver),
                Arc::new(ReqwestTransport),
            )
            .with_proxy(proxy.clone())
        };
        Self {
            runtime,
            public: client(UrlPolicy::PublicInternet),
            private: client(PRIVATE_NETWORK),
        }
    }
}

impl HttpClient for AutomationHttpClient {
    fn send(&self, call: HttpCall) -> HttpFuture {
        let client = if call.allow_private_network {
            self.private.clone()
        } else {
            self.public.clone()
        };
        let timeout = call.timeout;
        let request = match request(call) {
            Ok(request) => request,
            Err(error) => return Box::pin(ready(Err(error))),
        };
        let cancellation = request.options.cancellation.clone();
        Box::pin(InFlight {
            task: self
                .runtime
                .spawn(async move { client.request(request).await }),
            cancellation,
            timeout,
        })
    }
}

impl Future for InFlight {
    type Output = Result<HttpAnswer, HttpError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let timeout = self.timeout;
        Pin::new(&mut self.task).poll(cx).map(|ended| match ended {
            Ok(Ok(response)) => Ok(answer(response)),
            Ok(Err(error)) => Err(http_error(&error, timeout)),
            Err(error) => Err(task_failed(&error)),
        })
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.task.abort();
    }
}

fn process_client() -> Option<(Runtime, Arc<dyn HttpClient>)> {
    let runtime = Builder::new_multi_thread()
        .worker_threads(RUNTIME_WORKERS)
        .thread_name(RUNTIME_THREAD_NAME)
        .enable_all()
        .build()
        .inspect_err(|error| {
            warn!(%error, "automation http() is unavailable: its runtime could not start");
        })
        .ok()?;
    let proxy = ambient_proxy().unwrap_or_else(|error| {
        warn!(%error, "automation http() dials directly: the proxy environment is unusable");
        ProxyConfiguration::direct()
    });
    let client = Arc::new(AutomationHttpClient::new(runtime.handle().clone(), proxy));
    Some((runtime, client))
}

/// One attempt that follows only same-origin redirects, bounded by the call's timeout and
/// response limit.
fn request(call: HttpCall) -> Result<RequestSpec, HttpError> {
    Ok(RequestSpec {
        method: method(call.method),
        url: call.url,
        body: call.body.map(Bytes::from),
        redirects: RedirectScope::SameOrigin,
        options: FetchOptions {
            timeout: call.timeout,
            max_body_bytes: call.max_response_bytes,
            headers: headers(call.headers)?,
            retry: RetryPolicy::disabled(),
            ..FetchOptions::default()
        },
    })
}

fn method(method: HttpMethod) -> Method {
    match method {
        HttpMethod::Get => Method::GET,
        HttpMethod::Post => Method::POST,
        HttpMethod::Put => Method::PUT,
        HttpMethod::Patch => Method::PATCH,
        HttpMethod::Delete => Method::DELETE,
    }
}

/// The headers in sending order, every value marked sensitive because any may hold a secret. The
/// host checked them already, so one rejected here anyway is named but never shown.
fn headers(headers: Vec<(String, String)>) -> Result<HeaderMap, HttpError> {
    let mut map = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let Ok(header) = HeaderName::try_from(name.as_str()) else {
            return Err(HttpError::new(
                HttpErrorKind::InvalidArgument,
                format!("{name:?} {INVALID_HEADER_NAME}"),
            ));
        };
        let Ok(mut value) = HeaderValue::try_from(value) else {
            return Err(HttpError::new(
                HttpErrorKind::InvalidArgument,
                format!("header {name} {INVALID_HEADER_VALUE}"),
            ));
        };
        value.set_sensitive(true);
        map.append(header, value);
    }
    Ok(map)
}

fn answer(response: BoundedResponse) -> HttpAnswer {
    HttpAnswer {
        status: response.status.as_u16(),
        body: Vec::from(response.body),
        truncated: response.truncated,
    }
}

/// A message names a header, a host or an address at most. A payload that can hold a URL or a
/// `Location`, whose path or query may carry a secret, gives way to a fixed message.
fn http_error(error: &NetError, timeout: Duration) -> HttpError {
    match error {
        NetError::Policy(refused) => HttpError::new(HttpErrorKind::Refused, refusal(refused)),
        NetError::Proxy(_) => HttpError::new(HttpErrorKind::Refused, PROXY_REFUSED),
        NetError::ReservedHeader { .. } | NetError::RequestBodyTooLarge { .. } => {
            HttpError::new(HttpErrorKind::InvalidArgument, error.to_string())
        }
        NetError::Timeout => {
            HttpError::new(HttpErrorKind::Timeout, format!("{TIMED_OUT} {timeout:?}"))
        }
        NetError::Dns(_)
        | NetError::EmptyDnsAnswer(_)
        | NetError::Transport(_)
        | NetError::Cancelled => HttpError::new(HttpErrorKind::Transport, error.to_string()),
        NetError::Redirect(_) => HttpError::new(HttpErrorKind::Transport, REDIRECT_FAILED),
        _ => HttpError::new(HttpErrorKind::Transport, TRANSPORT_FAILED),
    }
}

/// `InvalidUrl` holds the rejected value, the URL or a `Location`, and a newer variant may too.
fn refusal(error: &UrlPolicyError) -> String {
    match error {
        UrlPolicyError::UnsupportedScheme
        | UrlPolicyError::CredentialsNotAllowed
        | UrlPolicyError::MissingHost
        | UrlPolicyError::SpecialUseHostname(_)
        | UrlPolicyError::NonPublicIp { .. } => error.to_string(),
        _ => REFUSED_BY_POLICY.to_owned(),
    }
}

/// The task panicked, or its runtime shut down under it.
fn task_failed(error: &JoinError) -> HttpError {
    warn!(
        panicked = error.is_panic(),
        "automation http request ended without an answer"
    );
    HttpError::new(HttpErrorKind::Transport, TASK_FAILED)
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle as ServerThread};

    use test_case::test_case;
    use url::Url;

    use super::{
        AutomationHttpClient, Builder, Duration, HttpAnswer, HttpCall, HttpClient, HttpError,
        HttpErrorKind, HttpMethod, INVALID_HEADER_NAME, INVALID_HEADER_VALUE, Method, NetError,
        ProxyConfiguration, Runtime, TIMED_OUT,
    };

    const TEST_WORKERS: usize = 1;
    const ANY_PORT: u16 = 0;
    /// Bounds every wait of the test server, so a misbehaving client fails a test rather than
    /// hanging it. Shorter than [`CALL_TIMEOUT`], so a request left running keeps its connection
    /// open past it.
    const SERVER_WAIT: Duration = Duration::from_secs(10);
    const CALL_TIMEOUT: Duration = Duration::from_secs(30);
    const SHORT_TIMEOUT: Duration = Duration::from_millis(300);
    const RESPONSE_LIMIT: usize = 1024;
    const SHORT_LIMIT: usize = 8;
    const OK: u16 = 200;
    const FOUND: u16 = 302;
    const TEMPORARY_REDIRECT: u16 = 307;
    const UNPROCESSABLE: u16 = 422;
    const UNAVAILABLE: u16 = 503;
    const HTTP_VERSION: &str = "HTTP/1.1";
    const REASON: &str = "Canned";
    const CONTENT_LENGTH: &str = "Content-Length";
    const LOCATION: &str = "Location";
    const AUTHORIZATION: &str = "Authorization";
    const BEARER: &str = "Bearer unit-token-0123";
    const CUSTOM_HEADER: &str = "X-Custom";
    const CUSTOM_VALUE: &str = "custom-value";
    const SPACED_NAME: &str = "X Spaced";
    const BROKEN_VALUE: &str = "split\r\nvalue";
    const PATH: &str = "/v1/items";
    const QUERY: &str = "page=2&sort=name";
    const MOVED_PATH: &str = "/moved";
    const ELSEWHERE_PATH: &str = "/elsewhere";
    const SECRET_SEGMENT: &str = "path-secret-4711";
    const SECRET_VALUE: &str = "query-secret-4711";
    const REQUEST_BODY: &str = r#"{"item":"one"}"#;
    const RESPONSE_BODY: &str = r#"{"ok":true}"#;
    const LONG_BODY: &str = "0123456789abcdef0123456789abcdef";
    const NO_CONNECTION: &str = "the client must not connect";
    const KEPT_OPEN: &str = "the client must close the connection";
    const NOT_HELD: &str = "the server must hold the request";
    const NOT_FAILED: &str = "the request must fail";
    const LEAKED: &str = "an error message must name neither the path nor the query";

    /// A request as the test server read it.
    struct Received {
        /// `METHOD target HTTP/1.1`.
        line: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    /// What the test server does with a connection once it has read the request.
    enum Reply {
        Answer(String),
        /// Closes the connection without answering.
        Close,
        /// Reports the request, then waits for the client to close the connection.
        Hold(mpsc::Sender<()>),
    }

    impl Received {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(header, _)| header.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    fn bind() -> TcpListener {
        TcpListener::bind((Ipv4Addr::LOCALHOST, ANY_PORT)).unwrap()
    }

    fn url(listener: &TcpListener, target: &str) -> Url {
        Url::parse(&format!(
            "http://{}{target}",
            listener.local_addr().unwrap()
        ))
        .unwrap()
    }

    /// Serves one connection per reply, in order, then hands back the listener and the requests.
    fn serve(
        listener: TcpListener,
        replies: Vec<Reply>,
    ) -> ServerThread<(TcpListener, Vec<Received>)> {
        thread::spawn(move || {
            let requests = replies
                .into_iter()
                .map(|reply| serve_one(&listener, reply))
                .collect();
            (listener, requests)
        })
    }

    fn serve_one(listener: &TcpListener, reply: Reply) -> Received {
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(SERVER_WAIT)).unwrap();
        let mut reader = BufReader::new(&stream);
        let request = read_request(&mut reader);
        match reply {
            Reply::Answer(response) => (&stream).write_all(response.as_bytes()).unwrap(),
            Reply::Close => {}
            Reply::Hold(held) => {
                held.send(()).unwrap();
                assert_eq!(reader.read(&mut [0]).expect(KEPT_OPEN), 0, "{KEPT_OPEN}");
            }
        }
        request
    }

    fn read_request(reader: &mut impl BufRead) -> Received {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut headers = Vec::new();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            let Some((name, value)) = header.trim_end().split_once(':') else {
                break;
            };
            headers.push((name.to_owned(), value.trim().to_owned()));
        }
        let mut request = Received {
            line: line.trim_end().to_owned(),
            headers,
            body: Vec::new(),
        };
        let length = request
            .header(CONTENT_LENGTH)
            .map_or(0, |length| length.parse().unwrap());
        request.body.resize(length, 0);
        reader.read_exact(&mut request.body).unwrap();
        request
    }

    fn response(status: u16, headers: &[(&str, &str)], body: &str) -> String {
        let headers: String = headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect();
        format!(
            "{HTTP_VERSION} {status} {REASON}\r\n{headers}{CONTENT_LENGTH}: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn assert_untouched(listener: &TcpListener) {
        listener.set_nonblocking(true).unwrap();
        let accepted = listener.accept();
        assert!(
            matches!(&accepted, Err(error) if error.kind() == ErrorKind::WouldBlock),
            "{NO_CONNECTION}"
        );
    }

    fn runtime() -> Runtime {
        Builder::new_multi_thread()
            .worker_threads(TEST_WORKERS)
            .enable_all()
            .build()
            .unwrap()
    }

    /// Dials directly, whatever proxy the test environment sets.
    fn client(runtime: &Runtime) -> AutomationHttpClient {
        AutomationHttpClient::new(runtime.handle().clone(), ProxyConfiguration::direct())
    }

    /// Sends `call` on a runtime of its own, dropped outside any runtime once the answer is in.
    fn send(call: HttpCall) -> Result<HttpAnswer, HttpError> {
        let runtime = runtime();
        smol::block_on(client(&runtime).send(call))
    }

    fn failure(sent: Result<HttpAnswer, HttpError>) -> HttpError {
        sent.err().expect(NOT_FAILED)
    }

    fn call(method: HttpMethod, url: Url) -> HttpCall {
        HttpCall {
            method,
            url,
            headers: Vec::new(),
            body: None,
            timeout: CALL_TIMEOUT,
            allow_private_network: true,
            max_response_bytes: RESPONSE_LIMIT,
        }
    }

    fn header(name: &str, value: &str) -> (String, String) {
        (name.to_owned(), value.to_owned())
    }

    #[test_case(HttpMethod::Post, Method::POST, OK; "post_answered_ok")]
    #[test_case(HttpMethod::Patch, Method::PATCH, UNPROCESSABLE; "patch_answered_unprocessable")]
    fn a_request_reaches_the_server_as_sent_and_its_answer_comes_back(
        method: HttpMethod,
        wire: Method,
        status: u16,
    ) {
        let origin = bind();
        let url = url(&origin, &format!("{PATH}?{QUERY}"));
        let server = serve(
            origin,
            vec![Reply::Answer(response(status, &[], RESPONSE_BODY))],
        );

        let answer = send(HttpCall {
            headers: vec![
                header(AUTHORIZATION, BEARER),
                header(CUSTOM_HEADER, CUSTOM_VALUE),
            ],
            body: Some(REQUEST_BODY.as_bytes().to_vec()),
            ..call(method, url)
        })
        .unwrap();

        let (_, requests) = server.join().unwrap();
        let request = &requests[0];
        assert_eq!(
            request.line,
            format!("{wire} {PATH}?{QUERY} {HTTP_VERSION}")
        );
        assert_eq!(request.header(AUTHORIZATION), Some(BEARER));
        assert_eq!(request.header(CUSTOM_HEADER), Some(CUSTOM_VALUE));
        assert_eq!(request.body, REQUEST_BODY.as_bytes());
        assert_eq!(answer.status, status);
        assert_eq!(answer.body, RESPONSE_BODY.as_bytes());
        assert!(!answer.truncated);
    }

    #[test]
    fn a_private_address_is_refused_without_the_private_network_and_never_dialled() {
        let origin = bind();

        let refused = failure(send(HttpCall {
            allow_private_network: false,
            ..call(HttpMethod::Get, url(&origin, PATH))
        }));

        assert_eq!(refused.kind, HttpErrorKind::Refused);
        assert_untouched(&origin);
    }

    #[test]
    fn a_redirect_to_another_origin_comes_back_unfollowed() {
        let (origin, elsewhere) = (bind(), bind());
        let location = url(&elsewhere, ELSEWHERE_PATH);
        let url = url(&origin, PATH);
        let server = serve(
            origin,
            vec![Reply::Answer(response(
                FOUND,
                &[(LOCATION, location.as_str())],
                RESPONSE_BODY,
            ))],
        );

        let answer = send(call(HttpMethod::Get, url)).unwrap();

        server.join().unwrap();
        assert_eq!(answer.status, FOUND);
        assert_untouched(&elsewhere);
    }

    #[test]
    fn a_same_origin_307_is_followed_with_the_method_and_the_body() {
        let origin = bind();
        let url = url(&origin, PATH);
        let server = serve(
            origin,
            vec![
                Reply::Answer(response(TEMPORARY_REDIRECT, &[(LOCATION, MOVED_PATH)], "")),
                Reply::Answer(response(OK, &[], RESPONSE_BODY)),
            ],
        );

        let answer = send(HttpCall {
            body: Some(REQUEST_BODY.as_bytes().to_vec()),
            ..call(HttpMethod::Post, url)
        })
        .unwrap();

        let (_, requests) = server.join().unwrap();
        let followed = &requests[1];
        assert_eq!(
            followed.line,
            format!("{} {MOVED_PATH} {HTTP_VERSION}", Method::POST)
        );
        assert_eq!(followed.body, REQUEST_BODY.as_bytes());
        assert_eq!(answer.status, OK);
        assert_eq!(answer.body, RESPONSE_BODY.as_bytes());
    }

    #[test]
    fn a_response_past_the_limit_is_cut_and_marked_truncated() {
        let origin = bind();
        let url = url(&origin, PATH);
        let server = serve(origin, vec![Reply::Answer(response(OK, &[], LONG_BODY))]);

        let answer = send(HttpCall {
            max_response_bytes: SHORT_LIMIT,
            ..call(HttpMethod::Get, url)
        })
        .unwrap();

        server.join().unwrap();
        assert_eq!(answer.body, LONG_BODY.as_bytes()[..SHORT_LIMIT]);
        assert!(answer.truncated);
    }

    #[test]
    fn a_server_that_never_answers_ends_the_request_as_a_timeout() {
        let origin = bind();
        let url = url(&origin, PATH);
        let (held, _request_held) = mpsc::channel();
        let server = serve(origin, vec![Reply::Hold(held)]);

        let timed_out = failure(send(HttpCall {
            timeout: SHORT_TIMEOUT,
            ..call(HttpMethod::Get, url)
        }));

        server.join().unwrap();
        assert_eq!(
            timed_out,
            HttpError::new(
                HttpErrorKind::Timeout,
                format!("{TIMED_OUT} {SHORT_TIMEOUT:?}")
            )
        );
    }

    #[test]
    fn dropping_the_future_cancels_the_request_and_closes_its_connection() {
        let origin = bind();
        let url = url(&origin, PATH);
        let (held, request_held) = mpsc::channel();
        let server = serve(origin, vec![Reply::Hold(held)]);
        let runtime = runtime();

        let sending = client(&runtime).send(call(HttpMethod::Get, url));
        request_held.recv_timeout(SERVER_WAIT).expect(NOT_HELD);
        drop(sending);

        assert!(server.join().is_ok(), "{KEPT_OPEN}");
    }

    #[test_case("Host"; "host")]
    #[test_case("Transfer-Encoding"; "transfer_encoding")]
    #[test_case("Proxy-Authorization"; "proxy_authorization")]
    fn a_reserved_header_is_an_invalid_argument_and_nothing_is_sent(name: &str) {
        let origin = bind();

        let refused = failure(send(HttpCall {
            headers: vec![header(name, CUSTOM_VALUE)],
            ..call(HttpMethod::Get, url(&origin, PATH))
        }));

        let reserved = NetError::ReservedHeader {
            name: name.to_ascii_lowercase(),
        };
        assert_eq!(
            refused,
            HttpError::new(HttpErrorKind::InvalidArgument, reserved.to_string())
        );
        assert_untouched(&origin);
    }

    #[test_case(SPACED_NAME, CUSTOM_VALUE => format!("{SPACED_NAME:?} {INVALID_HEADER_NAME}"); "a_name_with_a_space")]
    #[test_case(CUSTOM_HEADER, BROKEN_VALUE => format!("header {CUSTOM_HEADER} {INVALID_HEADER_VALUE}"); "a_value_with_a_line_break")]
    fn an_invalid_header_is_an_invalid_argument_that_never_shows_its_value(
        name: &str,
        value: &str,
    ) -> String {
        let origin = bind();

        let refused = failure(send(HttpCall {
            headers: vec![header(name, value)],
            ..call(HttpMethod::Get, url(&origin, PATH))
        }));

        assert_eq!(refused.kind, HttpErrorKind::InvalidArgument);
        assert_untouched(&origin);
        refused.message
    }

    #[test_case(HttpMethod::Post, Reply::Close => Err(HttpErrorKind::Transport); "post_closed_unanswered")]
    #[test_case(HttpMethod::Get, Reply::Close => Err(HttpErrorKind::Transport); "get_closed_unanswered")]
    #[test_case(HttpMethod::Get, Reply::Answer(response(UNAVAILABLE, &[], "")) => Ok(UNAVAILABLE); "get_answered_unavailable")]
    fn a_request_reaches_the_server_once(
        method: HttpMethod,
        reply: Reply,
    ) -> Result<u16, HttpErrorKind> {
        let origin = bind();
        let url = url(&origin, PATH);
        let server = serve(origin, vec![reply]);

        let sent = send(call(method, url));

        let (origin, _) = server.join().unwrap();
        assert_untouched(&origin);
        sent.map(|answer| answer.status).map_err(|error| error.kind)
    }

    #[test_case(true => HttpErrorKind::Transport; "a_closed_port")]
    #[test_case(false => HttpErrorKind::Refused; "a_refused_private_address")]
    fn an_error_message_names_neither_the_path_nor_the_query(
        allow_private_network: bool,
    ) -> HttpErrorKind {
        let closed = url(&bind(), &format!("/{SECRET_SEGMENT}?token={SECRET_VALUE}"));

        let failed = failure(send(HttpCall {
            allow_private_network,
            ..call(HttpMethod::Post, closed)
        }));

        assert!(
            !failed.message.contains(SECRET_SEGMENT) && !failed.message.contains(SECRET_VALUE),
            "{LEAKED}: {}",
            failed.message
        );
        failed.kind
    }
}
