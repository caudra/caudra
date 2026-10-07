use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream;
use http::header::HeaderName;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use test_case::test_case;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    BodyStream, DnsError, DnsResolver, FetchOptions, HttpClient, HttpTransport,
    MAX_REQUEST_BODY_BYTES, NetError, OperatorConfiguredPolicy, ProxyConfiguration, RedirectScope,
    RequestSpec, RetryPolicy, TransportError, TransportRequest, TransportResponse, TransportRoute,
    UrlPolicy, UrlPolicyError,
};

const API_HOST: &str = "api.example.com";
const API_URL: &str = "https://api.example.com/v1/items";
const SAME_ORIGIN_PATH: &str = "/v2/items";
const SAME_ORIGIN_URL: &str = "https://api.example.com/v2/items";
const OTHER_HOST: &str = "other.example.org";
const OTHER_URL: &str = "https://other.example.org/v2/items";
const UNUSABLE_LOCATION: &str = "https://[unclosed/v2/items";
const PAYLOAD: &[u8] = br#"{"title":"ship it"}"#;
const REDIRECT_BODY: &[u8] = b"moved";
const RESERVED_VALUE: &str = "reserved-value";
/// A caller's budget for the paused-clock deadline tests.
const BUDGET: Duration = Duration::from_secs(30);
/// When an OS gives up on a connect, well inside `BUDGET`.
const OS_CONNECT_TIMEOUT: Duration = Duration::from_secs(7);
/// Just past `BUDGET`.
const PAST_DEADLINE: Duration = Duration::from_secs(31);
/// What a transport reports when its own timer, or the OS, gives up on a hop.
const TIMED_OUT: &str = "operation timed out";

#[derive(Default)]
struct FakeDns {
    answers: HashMap<String, Vec<IpAddr>>,
    queries: Mutex<Vec<String>>,
}

struct SlowDns;

#[async_trait]
impl DnsResolver for SlowDns {
    async fn resolve(&self, _hostname: &str) -> Result<Vec<IpAddr>, DnsError> {
        tokio::time::sleep(Duration::from_secs(1)).await;
        Ok(vec!["93.184.216.34".parse().unwrap()])
    }
}

#[async_trait]
impl DnsResolver for FakeDns {
    async fn resolve(&self, hostname: &str) -> Result<Vec<IpAddr>, DnsError> {
        self.queries.lock().unwrap().push(hostname.to_owned());
        self.answers
            .get(hostname)
            .cloned()
            .ok_or_else(|| DnsError::new("missing fake DNS answer"))
    }
}

#[derive(Default)]
struct FakeTransport {
    responses: Mutex<VecDeque<Result<TransportResponse, TransportError>>>,
    requests: Mutex<Vec<TransportRequest>>,
}

#[async_trait]
impl HttpTransport for FakeTransport {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        self.requests.lock().unwrap().push(request);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(TransportError::new("missing fake response")))
    }
}

fn response(status: StatusCode, headers: HeaderMap, chunks: &[&'static [u8]]) -> TransportResponse {
    let chunks = chunks
        .iter()
        .map(|chunk| Ok(Bytes::from_static(chunk)))
        .collect::<Vec<_>>();
    let body: BodyStream = Box::pin(stream::iter(chunks));
    TransportResponse {
        status,
        headers,
        body,
    }
}

fn pinned_addresses(request: &TransportRequest) -> Vec<IpAddr> {
    match &request.route {
        TransportRoute::Direct { resolved_addresses } => resolved_addresses.clone(),
        TransportRoute::Proxy { .. } => panic!("expected a direct route"),
    }
}

fn fixture(dns: FakeDns, responses: Vec<TransportResponse>) -> (HttpClient, Arc<FakeTransport>) {
    let transport = Arc::new(FakeTransport {
        responses: Mutex::new(responses.into_iter().map(Ok).collect()),
        requests: Mutex::default(),
    });
    let client = HttpClient::new(UrlPolicy::PublicInternet, Arc::new(dns), transport.clone());
    (client, transport)
}

fn public_dns(names: &[&str]) -> FakeDns {
    FakeDns {
        answers: names
            .iter()
            .map(|name| ((*name).to_owned(), vec!["93.184.216.34".parse().unwrap()]))
            .collect(),
        queries: Mutex::default(),
    }
}

#[tokio::test]
async fn follows_redirects_manually_and_revalidates_each_dns_answer() {
    let mut redirect_headers = HeaderMap::new();
    redirect_headers.insert(
        http::header::LOCATION,
        HeaderValue::from_static("https://cdn.example.org/icon.png"),
    );
    let (client, transport) = fixture(
        public_dns(&["example.com", "cdn.example.org"]),
        vec![
            response(StatusCode::FOUND, redirect_headers, &[]),
            response(StatusCode::OK, HeaderMap::new(), &[b"icon"]),
        ],
    );
    let result = client
        .get("https://example.com/start", FetchOptions::default())
        .await
        .unwrap();
    assert_eq!(result.url.as_str(), "https://cdn.example.org/icon.png");
    assert_eq!(result.body, Bytes::from_static(b"icon"));
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        pinned_addresses(&requests[0]),
        pinned_addresses(&requests[1])
    );
}

#[tokio::test]
async fn cross_origin_redirect_rebuilds_headers_from_a_safe_allowlist() {
    let mut redirect_headers = HeaderMap::new();
    redirect_headers.insert(
        http::header::LOCATION,
        HeaderValue::from_static("https://cdn.example.org/icon.png"),
    );
    let (client, transport) = fixture(
        public_dns(&["example.com", "cdn.example.org"]),
        vec![
            response(StatusCode::FOUND, redirect_headers, &[]),
            response(StatusCode::OK, HeaderMap::new(), &[b"icon"]),
        ],
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_static("Bearer secret"),
    );
    headers.insert("x-api-key", HeaderValue::from_static("api-secret"));
    headers.insert("x-auth-token", HeaderValue::from_static("token-secret"));
    headers.insert(http::header::ACCEPT, HeaderValue::from_static("image/png"));
    headers.insert(
        http::header::USER_AGENT,
        HeaderValue::from_static("test-agent"),
    );

    client
        .get(
            "https://example.com/start",
            FetchOptions {
                headers,
                retry: RetryPolicy::disabled(),
                ..FetchOptions::default()
            },
        )
        .await
        .unwrap();

    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].headers["x-api-key"], "api-secret");
    assert!(
        requests[1]
            .headers
            .get(http::header::AUTHORIZATION)
            .is_none()
    );
    assert!(requests[1].headers.get("x-api-key").is_none());
    assert!(requests[1].headers.get("x-auth-token").is_none());
    assert_eq!(requests[1].headers[http::header::ACCEPT], "image/png");
    assert_eq!(requests[1].headers[http::header::USER_AGENT], "test-agent");
}

#[tokio::test]
async fn rejects_redirect_to_private_literal_before_second_request() {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::LOCATION,
        HeaderValue::from_static("http://169.254.169.254/latest/meta-data"),
    );
    let (client, transport) = fixture(
        public_dns(&["example.com"]),
        vec![response(StatusCode::FOUND, headers, &[])],
    );
    assert!(matches!(
        client
            .get("https://example.com/start", FetchOptions::default())
            .await,
        Err(NetError::Policy(UrlPolicyError::NonPublicIp { .. }))
    ));
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn rejects_mixed_public_and_private_dns_answers() {
    let dns = FakeDns {
        answers: HashMap::from([(
            "example.com".to_owned(),
            vec![
                "93.184.216.34".parse().unwrap(),
                "127.0.0.1".parse().unwrap(),
            ],
        )]),
        queries: Mutex::default(),
    };
    let (client, transport) = fixture(dns, vec![]);
    assert!(matches!(
        client
            .get("https://example.com", FetchOptions::default())
            .await,
        Err(NetError::Policy(UrlPolicyError::NonPublicIp { .. }))
    ));
    assert!(transport.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn dns_failure_and_empty_answers_fail_before_transport() {
    let (missing_client, missing_transport) = fixture(FakeDns::default(), vec![]);
    assert!(matches!(
        missing_client
            .get(
                "https://missing.example.net",
                FetchOptions {
                    retry: RetryPolicy::disabled(),
                    ..FetchOptions::default()
                },
            )
            .await,
        Err(NetError::Dns(_))
    ));
    assert!(missing_transport.requests.lock().unwrap().is_empty());

    let empty_dns = FakeDns {
        answers: HashMap::from([("empty.example.net".to_owned(), Vec::new())]),
        queries: Mutex::default(),
    };
    let (empty_client, empty_transport) = fixture(empty_dns, vec![]);
    assert!(matches!(
        empty_client
            .get(
                "https://empty.example.net",
                FetchOptions {
                    retry: RetryPolicy::disabled(),
                    ..FetchOptions::default()
                },
            )
            .await,
        Err(NetError::EmptyDnsAnswer(hostname)) if hostname == "empty.example.net"
    ));
    assert!(empty_transport.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn rejects_redirect_to_hostname_resolving_private_before_second_request() {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::LOCATION,
        HeaderValue::from_static("http://internal.example.com/admin"),
    );
    let dns = FakeDns {
        answers: HashMap::from([
            (
                "public.example.com".to_owned(),
                vec!["93.184.216.34".parse().unwrap()],
            ),
            (
                "internal.example.com".to_owned(),
                vec!["10.0.0.8".parse().unwrap()],
            ),
        ]),
        queries: Mutex::default(),
    };
    let (client, transport) = fixture(dns, vec![response(StatusCode::FOUND, headers, &[])]);

    assert!(matches!(
        client
            .get("https://public.example.com/start", FetchOptions::default())
            .await,
        Err(NetError::Policy(UrlPolicyError::NonPublicIp { .. }))
    ));
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn body_stream_is_cut_at_the_configured_bound() {
    let (client, _) = fixture(
        public_dns(&["example.com"]),
        vec![response(
            StatusCode::OK,
            HeaderMap::new(),
            &[b"abc", b"defgh"],
        )],
    );
    let result = client
        .get(
            "https://example.com",
            FetchOptions {
                max_body_bytes: 5,
                retry: RetryPolicy::disabled(),
                ..FetchOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.body, Bytes::from_static(b"abcde"));
    assert!(result.truncated);
}

#[tokio::test]
async fn cancellation_wins_before_network_io() {
    let (client, transport) = fixture(public_dns(&["example.com"]), vec![]);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let result = client
        .get(
            "https://example.com",
            FetchOptions {
                cancellation,
                retry: RetryPolicy::disabled(),
                ..FetchOptions::default()
            },
        )
        .await;
    assert!(matches!(result, Err(NetError::Cancelled)));
    assert!(transport.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn total_deadline_covers_dns_resolution() {
    let transport = Arc::new(FakeTransport::default());
    let client = HttpClient::new(
        UrlPolicy::PublicInternet,
        Arc::new(SlowDns),
        transport.clone(),
    );
    let result = client
        .get(
            "https://example.com",
            FetchOptions {
                timeout: Duration::from_millis(1),
                retry: RetryPolicy::disabled(),
                ..FetchOptions::default()
            },
        )
        .await;
    assert!(matches!(result, Err(NetError::Timeout)));
    assert!(transport.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancellation_interrupts_a_pending_body_stream() {
    let body: BodyStream = Box::pin(stream::pending());
    let (client, _) = fixture(
        public_dns(&["example.com"]),
        vec![TransportResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body,
        }],
    );
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        client
            .get(
                "https://example.com",
                FetchOptions {
                    cancellation: task_cancellation,
                    retry: RetryPolicy::disabled(),
                    ..FetchOptions::default()
                },
            )
            .await
    });
    tokio::task::yield_now().await;
    cancellation.cancel();
    assert!(matches!(task.await.unwrap(), Err(NetError::Cancelled)));
}

#[tokio::test]
async fn operator_policy_can_reach_injected_local_service() {
    let transport = Arc::new(FakeTransport {
        responses: Mutex::new(VecDeque::from([Ok(response(
            StatusCode::OK,
            HeaderMap::new(),
            &[b"ok"],
        ))])),
        requests: Mutex::default(),
    });
    let client = HttpClient::new(
        UrlPolicy::OperatorConfigured(OperatorConfiguredPolicy {
            allow_non_public_ips: true,
            allow_special_use_names: true,
            allow_url_credentials: false,
        }),
        Arc::new(FakeDns {
            answers: HashMap::from([(
                "service.local".to_owned(),
                vec!["127.0.0.1".parse().unwrap()],
            )]),
            queries: Mutex::default(),
        }),
        transport,
    );
    let response = client
        .get("http://service.local/health", FetchOptions::default())
        .await
        .unwrap();
    assert_eq!(response.body, Bytes::from_static(b"ok"));
}

fn proxy_fixture(
    bypass: Option<&str>,
    dns: FakeDns,
    responses: Vec<TransportResponse>,
) -> (HttpClient, Arc<FakeTransport>) {
    let (client, transport) = fixture(dns, responses);
    let proxy =
        ProxyConfiguration::from_values(None, None, Some("http://proxy.internal:8080"), bypass)
            .unwrap();
    (client.with_proxy(proxy), transport)
}

#[tokio::test]
async fn a_proxied_hop_never_resolves_the_target_itself() {
    // The whole point of proxy support: under enforcement the guest cannot
    // resolve, so a lookup here would fail closed before the proxy is reached.
    let dns = FakeDns::default();
    let (client, transport) = proxy_fixture(
        None,
        dns,
        vec![response(StatusCode::OK, HeaderMap::new(), &[b"body"])],
    );
    let result = client
        .get("https://example.com/page", FetchOptions::default())
        .await
        .unwrap();
    assert_eq!(result.body, Bytes::from_static(b"body"));

    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(matches!(requests[0].route, TransportRoute::Proxy { .. }));
}

#[tokio::test]
async fn a_bypassed_host_still_resolves_and_pins_every_answer() {
    let dns = public_dns(&["internal.example.com"]);
    let (client, transport) = proxy_fixture(
        Some("internal.example.com"),
        dns,
        vec![response(StatusCode::OK, HeaderMap::new(), &[b"body"])],
    );
    client
        .get("https://internal.example.com/page", FetchOptions::default())
        .await
        .unwrap();

    let requests = transport.requests.lock().unwrap();
    assert_eq!(
        pinned_addresses(&requests[0]),
        vec!["93.184.216.34".parse::<IpAddr>().unwrap()]
    );
}

#[tokio::test]
async fn each_redirect_hop_selects_its_own_route() {
    let mut redirect_headers = HeaderMap::new();
    redirect_headers.insert(
        http::header::LOCATION,
        HeaderValue::from_static("https://cdn.example.org/icon.png"),
    );
    let (client, transport) = proxy_fixture(
        Some("example.com"),
        public_dns(&["example.com"]),
        vec![
            response(StatusCode::FOUND, redirect_headers, &[]),
            response(StatusCode::OK, HeaderMap::new(), &[b"icon"]),
        ],
    );
    client
        .get("https://example.com/start", FetchOptions::default())
        .await
        .unwrap();

    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(matches!(requests[0].route, TransportRoute::Direct { .. }));
    assert!(matches!(requests[1].route, TransportRoute::Proxy { .. }));
}

#[tokio::test]
async fn dns_free_policy_still_rejects_local_targets_under_a_proxy() {
    for target in [
        "http://localhost/admin",
        "http://127.0.0.1/admin",
        "http://[::1]/admin",
        "http://169.254.169.254/latest/meta-data",
    ] {
        let (client, transport) = proxy_fixture(None, FakeDns::default(), vec![]);
        let result = client.get(target, FetchOptions::default()).await;
        assert!(
            matches!(result, Err(NetError::Policy(_))),
            "expected {target} to be rejected locally"
        );
        assert!(transport.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_proxy_refusal_is_reported_as_policy_and_never_retried() {
    let transport = Arc::new(FakeTransport {
        responses: Mutex::new(VecDeque::from([
            Err(TransportError::proxy(
                "outbound proxy refused the connection",
            )),
            Ok(response(StatusCode::OK, HeaderMap::new(), &[b"body"])),
        ])),
        requests: Mutex::default(),
    });
    let client = HttpClient::new(
        UrlPolicy::PublicInternet,
        Arc::new(FakeDns::default()),
        transport.clone(),
    )
    .with_proxy(
        ProxyConfiguration::from_values(None, None, Some("http://proxy.internal:8080"), None)
            .unwrap(),
    );
    let result = client
        .get("https://example.com/page", FetchOptions::default())
        .await;
    assert!(matches!(result, Err(NetError::Proxy(_))));
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

/// Answers each lookup with the next scripted address, so a later hop to the
/// same name can be rebound.
struct RebindingDns(Mutex<VecDeque<IpAddr>>);

#[async_trait]
impl DnsResolver for RebindingDns {
    async fn resolve(&self, _hostname: &str) -> Result<Vec<IpAddr>, DnsError> {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .map(|address| vec![address])
            .ok_or_else(|| DnsError::new("missing fake DNS answer"))
    }
}

/// Never answers. Given a token, it cancels the caller once the hop is in flight.
struct StalledTransport(Option<CancellationToken>);

#[async_trait]
impl HttpTransport for StalledTransport {
    async fn execute(
        &self,
        _request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        if let Some(cancellation) = &self.0 {
            cancellation.cancel();
        }
        std::future::pending().await
    }
}

/// Lets `.0` pass on the paused clock before the scripted transport answers,
/// which puts that answer on a chosen side of the deadline.
struct Delayed(Duration, FakeTransport);

#[async_trait]
impl HttpTransport for Delayed {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        tokio::time::advance(self.0).await;
        self.1.execute(request).await
    }
}

/// Fails each lookup only once `.0` has passed on the paused clock.
struct DelayedDnsFailure(Duration);

#[async_trait]
impl DnsResolver for DelayedDnsFailure {
    async fn resolve(&self, _hostname: &str) -> Result<Vec<IpAddr>, DnsError> {
        tokio::time::advance(self.0).await;
        Err(DnsError::new(TIMED_OUT))
    }
}

fn delayed(delay: Duration, results: Vec<Result<TransportResponse, TransportError>>) -> HttpClient {
    let transport = Delayed(
        delay,
        FakeTransport {
            responses: Mutex::new(results.into()),
            requests: Mutex::default(),
        },
    );
    HttpClient::new(
        UrlPolicy::PublicInternet,
        Arc::new(public_dns(&[API_HOST])),
        Arc::new(transport),
    )
}

fn lookup_failing_after(delay: Duration) -> HttpClient {
    HttpClient::new(
        UrlPolicy::PublicInternet,
        Arc::new(DelayedDnsFailure(delay)),
        Arc::new(FakeTransport::default()),
    )
}

/// A response whose body fails only once `delay` has passed on the paused clock.
fn body_failing_after(delay: Duration) -> TransportResponse {
    let body: BodyStream = Box::pin(stream::once(async move {
        tokio::time::advance(delay).await;
        Err::<Bytes, _>(TransportError::new(TIMED_OUT))
    }));
    TransportResponse {
        status: StatusCode::OK,
        headers: HeaderMap::new(),
        body,
    }
}

/// One attempt within `BUDGET`, as a caller that disables retries makes it.
/// Retrying would reach the deadline through the backoff anyway and hide how
/// the failure itself was judged.
fn single_attempt() -> FetchOptions {
    FetchOptions {
        timeout: BUDGET,
        retry: RetryPolicy::disabled(),
        ..FetchOptions::default()
    }
}

fn scripted(
    results: Vec<Result<TransportResponse, TransportError>>,
) -> (HttpClient, Arc<FakeTransport>) {
    let transport = Arc::new(FakeTransport {
        responses: Mutex::new(results.into()),
        requests: Mutex::default(),
    });
    let client = HttpClient::new(
        UrlPolicy::PublicInternet,
        Arc::new(public_dns(&[API_HOST])),
        transport.clone(),
    );
    (client, transport)
}

/// A client whose resolver and transport both stay observable.
fn observed(responses: Vec<TransportResponse>) -> (HttpClient, Arc<FakeDns>, Arc<FakeTransport>) {
    let dns = Arc::new(public_dns(&[API_HOST, OTHER_HOST]));
    let transport = Arc::new(FakeTransport {
        responses: Mutex::new(responses.into_iter().map(Ok).collect()),
        requests: Mutex::default(),
    });
    let client = HttpClient::new(UrlPolicy::PublicInternet, dns.clone(), transport.clone());
    (client, dns, transport)
}

/// A request with a body under the widest scope, so every such test also
/// proves the scope cannot widen what a replayed request follows.
fn spec(method: Method, options: FetchOptions) -> RequestSpec {
    RequestSpec {
        method,
        url: Url::parse(API_URL).unwrap(),
        body: Some(Bytes::from_static(PAYLOAD)),
        redirects: RedirectScope::AnyOrigin,
        options,
    }
}

fn get(redirects: RedirectScope, options: FetchOptions) -> RequestSpec {
    RequestSpec {
        body: None,
        redirects,
        ..spec(Method::GET, options)
    }
}

fn with_headers(headers: &[(HeaderName, &'static str)]) -> FetchOptions {
    FetchOptions {
        headers: headers
            .iter()
            .map(|(name, value)| (name.clone(), HeaderValue::from_static(value)))
            .collect(),
        ..FetchOptions::default()
    }
}

fn empty(status: StatusCode) -> TransportResponse {
    response(status, HeaderMap::new(), &[])
}

fn redirect(status: StatusCode, location: &'static str) -> TransportResponse {
    let mut headers = HeaderMap::new();
    headers.insert(http::header::LOCATION, HeaderValue::from_static(location));
    response(status, headers, &[REDIRECT_BODY])
}

fn retrying() -> FetchOptions {
    FetchOptions {
        retry: RetryPolicy {
            max_retries: 3,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            ..RetryPolicy::default()
        },
        ..FetchOptions::default()
    }
}

#[tokio::test]
async fn a_direct_request_hands_its_method_and_body_to_the_transport() {
    let (client, transport) = fixture(public_dns(&[API_HOST]), vec![empty(StatusCode::OK)]);
    client
        .request(spec(Method::PATCH, FetchOptions::default()))
        .await
        .unwrap();

    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests[0].method, Method::PATCH);
    assert_eq!(requests[0].body.as_deref(), Some(PAYLOAD));
    assert_eq!(
        pinned_addresses(&requests[0]),
        vec!["93.184.216.34".parse::<IpAddr>().unwrap()]
    );
}

#[tokio::test]
async fn a_proxied_request_hands_its_method_and_body_to_the_transport() {
    let (client, transport) = proxy_fixture(None, FakeDns::default(), vec![empty(StatusCode::OK)]);
    client
        .request(spec(Method::DELETE, FetchOptions::default()))
        .await
        .unwrap();

    let requests = transport.requests.lock().unwrap();
    assert!(matches!(requests[0].route, TransportRoute::Proxy { .. }));
    assert_eq!(requests[0].method, Method::DELETE);
    assert_eq!(requests[0].body.as_deref(), Some(PAYLOAD));
}

#[tokio::test]
async fn a_body_over_the_cap_is_rejected_before_dns_or_transport() {
    let (client, dns, transport) = observed(vec![]);
    let oversized = MAX_REQUEST_BODY_BYTES + 1;
    let result = client
        .request(RequestSpec {
            body: Some(Bytes::from(vec![0; oversized])),
            ..spec(Method::POST, FetchOptions::default())
        })
        .await;

    assert!(matches!(
        result,
        Err(NetError::RequestBodyTooLarge { length }) if length == oversized
    ));
    assert!(dns.queries.lock().unwrap().is_empty());
    assert!(transport.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_body_at_the_cap_is_sent() {
    let (client, transport) = fixture(public_dns(&[API_HOST]), vec![empty(StatusCode::OK)]);
    client
        .request(RequestSpec {
            body: Some(Bytes::from(vec![0; MAX_REQUEST_BODY_BYTES])),
            ..spec(Method::POST, FetchOptions::default())
        })
        .await
        .unwrap();

    let requests = transport.requests.lock().unwrap();
    assert_eq!(
        requests[0].body.as_ref().map(Bytes::len),
        Some(MAX_REQUEST_BODY_BYTES)
    );
}

#[tokio::test]
async fn post_and_patch_are_attempted_once_whatever_the_retry_policy() {
    for method in [Method::POST, Method::PATCH] {
        let (client, transport) = scripted(vec![
            Err(TransportError::new("connection reset")),
            Ok(empty(StatusCode::OK)),
        ]);
        let result = client.request(spec(method.clone(), retrying())).await;
        assert!(
            matches!(result, Err(NetError::Transport(_))),
            "{method} was retried after a transport failure"
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 1);

        let (client, transport) = scripted(vec![
            Ok(empty(StatusCode::SERVICE_UNAVAILABLE)),
            Ok(empty(StatusCode::OK)),
        ]);
        let response = client
            .request(spec(method.clone(), retrying()))
            .await
            .unwrap();
        assert_eq!(
            response.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{method} was retried after a retry status"
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn put_is_retried_after_a_transport_failure_and_a_retry_status() {
    let (client, transport) = scripted(vec![
        Err(TransportError::new("connection reset")),
        Ok(empty(StatusCode::SERVICE_UNAVAILABLE)),
        Ok(empty(StatusCode::OK)),
    ]);
    let response = client.request(spec(Method::PUT, retrying())).await.unwrap();

    assert_eq!(response.status, StatusCode::OK);
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests.iter().all(
            |request| request.method == Method::PUT && request.body.as_deref() == Some(PAYLOAD)
        )
    );
}

#[tokio::test]
async fn a_non_get_request_returns_a_301_302_or_303_as_its_final_response() {
    for status in [
        StatusCode::MOVED_PERMANENTLY,
        StatusCode::FOUND,
        StatusCode::SEE_OTHER,
    ] {
        let (client, transport) = fixture(
            public_dns(&[API_HOST]),
            vec![redirect(status, "/v2/items"), empty(StatusCode::OK)],
        );
        let response = client
            .request(spec(Method::POST, FetchOptions::default()))
            .await
            .unwrap();

        assert_eq!(response.status, status);
        assert_eq!(response.url.as_str(), API_URL);
        assert_eq!(response.body, Bytes::from_static(REDIRECT_BODY));
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn a_same_origin_307_or_308_is_followed_with_the_method_body_and_headers() {
    for status in [
        StatusCode::TEMPORARY_REDIRECT,
        StatusCode::PERMANENT_REDIRECT,
    ] {
        let (client, transport) = fixture(
            public_dns(&[API_HOST]),
            vec![redirect(status, "/v2/items"), empty(StatusCode::CREATED)],
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        let response = client
            .request(spec(
                Method::POST,
                FetchOptions {
                    headers,
                    ..FetchOptions::default()
                },
            ))
            .await
            .unwrap();

        assert_eq!(response.status, StatusCode::CREATED);
        assert_eq!(response.url.as_str(), "https://api.example.com/v2/items");
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].method, Method::POST);
        assert_eq!(requests[1].body.as_deref(), Some(PAYLOAD));
        assert_eq!(
            requests[1].headers[http::header::AUTHORIZATION],
            "Bearer secret"
        );
    }
}

#[tokio::test]
async fn a_307_or_308_to_another_origin_is_returned_rather_than_followed() {
    for status in [
        StatusCode::TEMPORARY_REDIRECT,
        StatusCode::PERMANENT_REDIRECT,
    ] {
        for location in [
            "https://other.example.org/v2/items",
            "http://api.example.com/v2/items",
            "https://api.example.com:8443/v2/items",
        ] {
            let (client, transport) = fixture(
                public_dns(&[API_HOST, "other.example.org"]),
                vec![redirect(status, location), empty(StatusCode::OK)],
            );
            let response = client
                .request(spec(Method::POST, FetchOptions::default()))
                .await
                .unwrap();

            assert_eq!(response.status, status, "{location}");
            assert_eq!(transport.requests.lock().unwrap().len(), 1, "{location}");
        }
    }
}

#[tokio::test]
async fn a_followed_307_hop_is_resolved_and_checked_again() {
    let transport = Arc::new(FakeTransport {
        responses: Mutex::new(VecDeque::from([Ok(redirect(
            StatusCode::TEMPORARY_REDIRECT,
            "/v2/items",
        ))])),
        requests: Mutex::default(),
    });
    let dns = RebindingDns(Mutex::new(VecDeque::from([
        "93.184.216.34".parse().unwrap(),
        "10.0.0.8".parse().unwrap(),
    ])));
    let client = HttpClient::new(UrlPolicy::PublicInternet, Arc::new(dns), transport.clone());
    let result = client
        .request(spec(Method::POST, FetchOptions::default()))
        .await;

    assert!(matches!(
        result,
        Err(NetError::Policy(UrlPolicyError::NonPublicIp { .. }))
    ));
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_get_with_a_body_returns_a_302_rather_than_carrying_the_body_elsewhere() {
    let (client, transport) = fixture(
        public_dns(&[API_HOST, "other.example.org"]),
        vec![
            redirect(StatusCode::FOUND, "https://other.example.org/v2/items"),
            empty(StatusCode::OK),
        ],
    );
    let response = client
        .request(spec(Method::GET, FetchOptions::default()))
        .await
        .unwrap();

    assert_eq!(response.status, StatusCode::FOUND);
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_request_response_is_cut_at_the_configured_bound() {
    let (client, _) = fixture(
        public_dns(&[API_HOST]),
        vec![response(
            StatusCode::OK,
            HeaderMap::new(),
            &[b"abc", b"defgh"],
        )],
    );
    let result = client
        .request(spec(
            Method::POST,
            FetchOptions {
                max_body_bytes: 5,
                ..FetchOptions::default()
            },
        ))
        .await
        .unwrap();

    assert_eq!(result.body, Bytes::from_static(b"abcde"));
    assert!(result.truncated);
}

#[tokio::test]
async fn the_total_deadline_stops_a_request_the_transport_never_answers() {
    let client = HttpClient::new(
        UrlPolicy::PublicInternet,
        Arc::new(public_dns(&[API_HOST])),
        Arc::new(StalledTransport(None)),
    );
    let result = client
        .request(spec(
            Method::POST,
            FetchOptions {
                timeout: Duration::from_millis(1),
                ..FetchOptions::default()
            },
        ))
        .await;

    assert!(matches!(result, Err(NetError::Timeout)));
}

#[tokio::test]
async fn cancellation_stops_a_request_the_transport_is_still_serving() {
    let cancellation = CancellationToken::new();
    let client = HttpClient::new(
        UrlPolicy::PublicInternet,
        Arc::new(public_dns(&[API_HOST])),
        Arc::new(StalledTransport(Some(cancellation.clone()))),
    );
    let result = client
        .request(spec(
            Method::POST,
            FetchOptions {
                cancellation,
                ..FetchOptions::default()
            },
        ))
        .await;

    assert!(matches!(result, Err(NetError::Cancelled)));
}

#[test_case(lookup_failing_after(PAST_DEADLINE) ; "while resolving the host")]
#[test_case(delayed(PAST_DEADLINE, vec![Err(TransportError::new(TIMED_OUT))]) ; "while awaiting the response")]
#[test_case(delayed(Duration::ZERO, vec![Ok(body_failing_after(PAST_DEADLINE))]) ; "while reading the body")]
#[tokio::test(start_paused = true)]
async fn a_failure_that_surfaces_after_the_deadline_is_a_timeout(client: HttpClient) {
    let result = client
        .request(get(RedirectScope::AnyOrigin, single_attempt()))
        .await;

    assert!(matches!(result, Err(NetError::Timeout)), "{result:?}");
}

#[tokio::test(start_paused = true)]
async fn a_timeout_inside_the_budget_is_a_transport_failure_that_a_get_retries() {
    let once = delayed(
        OS_CONNECT_TIMEOUT,
        vec![Err(TransportError::new(TIMED_OUT))],
    )
    .request(get(RedirectScope::AnyOrigin, single_attempt()))
    .await;
    assert!(matches!(once, Err(NetError::Transport(_))), "{once:?}");

    let retried = delayed(
        OS_CONNECT_TIMEOUT,
        vec![
            Err(TransportError::new(TIMED_OUT)),
            Ok(empty(StatusCode::OK)),
        ],
    )
    .request(get(
        RedirectScope::AnyOrigin,
        FetchOptions {
            timeout: BUDGET,
            ..retrying()
        },
    ))
    .await
    .unwrap();
    assert_eq!(retried.status, StatusCode::OK);
}

#[tokio::test(start_paused = true)]
async fn a_response_that_arrives_after_the_deadline_is_still_returned() {
    let late = delayed(
        PAST_DEADLINE,
        vec![Ok(response(StatusCode::OK, HeaderMap::new(), &[PAYLOAD]))],
    )
    .request(get(RedirectScope::AnyOrigin, single_attempt()))
    .await
    .unwrap();

    assert_eq!(late.body, Bytes::from_static(PAYLOAD));
}

#[test_case(StatusCode::MOVED_PERMANENTLY ; "a 301")]
#[test_case(StatusCode::FOUND ; "a 302")]
#[test_case(StatusCode::SEE_OTHER ; "a 303")]
#[test_case(StatusCode::TEMPORARY_REDIRECT ; "a 307")]
#[test_case(StatusCode::PERMANENT_REDIRECT ; "a 308")]
#[tokio::test]
async fn a_same_origin_get_follows_a_same_origin_redirect(status: StatusCode) {
    let (client, _, transport) = observed(vec![
        redirect(status, SAME_ORIGIN_PATH),
        empty(StatusCode::OK),
    ]);
    let response = client
        .request(get(RedirectScope::SameOrigin, FetchOptions::default()))
        .await
        .unwrap();

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.url.as_str(), SAME_ORIGIN_URL);
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
}

#[test_case(OTHER_URL ; "another host")]
#[test_case("http://api.example.com/v2/items" ; "another scheme")]
#[test_case("https://api.example.com:8443/v2/items" ; "another port")]
#[tokio::test]
async fn a_same_origin_get_returns_a_cross_origin_redirect_without_resolving_or_contacting_it(
    location: &'static str,
) {
    let (client, dns, transport) = observed(vec![
        redirect(StatusCode::FOUND, location),
        empty(StatusCode::OK),
    ]);
    let response = client
        .request(get(RedirectScope::SameOrigin, FetchOptions::default()))
        .await
        .unwrap();

    assert_eq!(response.status, StatusCode::FOUND);
    assert_eq!(response.headers[http::header::LOCATION], location);
    assert_eq!(response.url.as_str(), API_URL);
    assert_eq!(response.body, Bytes::from_static(REDIRECT_BODY));
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    assert_eq!(*dns.queries.lock().unwrap(), [API_HOST]);
}

#[tokio::test]
async fn an_any_origin_get_follows_a_cross_origin_redirect() {
    let (client, dns, transport) = observed(vec![
        redirect(StatusCode::FOUND, OTHER_URL),
        empty(StatusCode::OK),
    ]);
    let response = client
        .request(get(RedirectScope::AnyOrigin, FetchOptions::default()))
        .await
        .unwrap();

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.url.as_str(), OTHER_URL);
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
    assert_eq!(*dns.queries.lock().unwrap(), [API_HOST, OTHER_HOST]);
}

#[tokio::test]
async fn a_same_origin_get_counts_only_a_followable_redirect_against_the_limit() {
    let no_redirects = FetchOptions {
        max_redirects: 0,
        ..FetchOptions::default()
    };
    let (client, _, _) = observed(vec![redirect(StatusCode::FOUND, SAME_ORIGIN_PATH)]);
    let followable = client
        .request(get(RedirectScope::SameOrigin, no_redirects.clone()))
        .await;
    assert!(matches!(followable, Err(NetError::Redirect(_))));

    let (client, _, _) = observed(vec![redirect(StatusCode::FOUND, OTHER_URL)]);
    let returned = client
        .request(get(RedirectScope::SameOrigin, no_redirects))
        .await
        .unwrap();
    assert_eq!(returned.status, StatusCode::FOUND);
}

#[test_case(RedirectScope::SameOrigin, empty(StatusCode::FOUND) ; "same origin without a location")]
#[test_case(RedirectScope::AnyOrigin, empty(StatusCode::FOUND) ; "any origin without a location")]
#[test_case(RedirectScope::SameOrigin, redirect(StatusCode::FOUND, UNUSABLE_LOCATION) ; "same origin with an unusable location")]
#[test_case(RedirectScope::AnyOrigin, redirect(StatusCode::FOUND, UNUSABLE_LOCATION) ; "any origin with an unusable location")]
#[tokio::test]
async fn a_get_redirect_without_a_usable_location_is_an_error(
    redirects: RedirectScope,
    response: TransportResponse,
) {
    let (client, _, transport) = observed(vec![response]);
    let result = client
        .request(get(redirects, FetchOptions::default()))
        .await;

    assert!(matches!(
        result,
        Err(NetError::Redirect(_) | NetError::Policy(UrlPolicyError::InvalidUrl(_)))
    ));
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_replayable_redirect_without_a_location_is_returned() {
    let (client, _, transport) = observed(vec![empty(StatusCode::TEMPORARY_REDIRECT)]);
    let response = client
        .request(spec(Method::POST, FetchOptions::default()))
        .await
        .unwrap();

    assert_eq!(response.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[test_case(http::header::HOST ; "host")]
#[test_case(http::header::CONTENT_LENGTH ; "content length")]
#[test_case(http::header::TRANSFER_ENCODING ; "transfer encoding")]
#[test_case(http::header::CONNECTION ; "connection")]
#[test_case(HeaderName::from_static("keep-alive") ; "keep alive")]
#[test_case(http::header::TE ; "te")]
#[test_case(http::header::TRAILER ; "trailer")]
#[test_case(http::header::UPGRADE ; "upgrade")]
#[test_case(http::header::EXPECT ; "expect")]
#[test_case(http::header::PROXY_AUTHORIZATION ; "proxy authorization")]
#[test_case(HeaderName::from_static("proxy-connection") ; "any proxy prefixed name")]
#[tokio::test]
async fn a_reserved_header_is_rejected_before_dns_or_transport(name: HeaderName) {
    let (client, dns, transport) = observed(vec![empty(StatusCode::OK), empty(StatusCode::OK)]);
    let options = with_headers(&[(name.clone(), RESERVED_VALUE)]);
    let get = client
        .get_url(Url::parse(API_URL).unwrap(), options.clone())
        .await;
    let post = client.request(spec(Method::POST, options)).await;

    for result in [get, post] {
        let error = result.expect_err("a reserved header was sent");
        assert!(
            matches!(&error, NetError::ReservedHeader { name: rejected } if rejected == name.as_str()),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(message.contains(name.as_str()), "{message}");
        assert!(!message.contains(RESERVED_VALUE), "{message}");
    }
    assert!(dns.queries.lock().unwrap().is_empty());
    assert!(transport.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn accept_and_a_custom_api_key_header_are_sent() {
    let (client, _, transport) = observed(vec![empty(StatusCode::OK), empty(StatusCode::OK)]);
    let options = with_headers(&[
        (http::header::ACCEPT, "application/json"),
        (HeaderName::from_static("x-api-key"), "api-secret"),
    ]);
    client
        .get_url(Url::parse(API_URL).unwrap(), options.clone())
        .await
        .unwrap();
    client
        .request(spec(Method::POST, options.clone()))
        .await
        .unwrap();

    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.headers == options.headers)
    );
}

#[test]
fn a_request_spec_debug_shows_no_path_query_header_value_or_body() {
    let request = RequestSpec {
        url: Url::parse("https://api.example.com/path-secret?token=query-secret").unwrap(),
        body: Some(Bytes::from_static(b"body-secret")),
        ..spec(
            Method::POST,
            with_headers(&[(http::header::AUTHORIZATION, "Bearer header-secret")]),
        )
    };

    let rendered = format!("{request:?}");
    for shown in ["https://api.example.com", "AnyOrigin", "authorization"] {
        assert!(rendered.contains(shown), "{rendered}");
    }
    assert!(!rendered.contains("secret"), "{rendered}");
}

#[test]
fn a_transport_request_debug_shows_no_path_query_header_value_body_or_address() {
    let address: IpAddr = "93.184.216.34".parse().unwrap();
    let body = Bytes::from_static(b"body-secret");
    let request = TransportRequest {
        method: Method::POST,
        url: Url::parse("https://api.example.com/path-secret?token=query-secret").unwrap(),
        headers: with_headers(&[(http::header::AUTHORIZATION, "Bearer header-secret")]).headers,
        body: Some(body.clone()),
        route: TransportRoute::Direct {
            resolved_addresses: vec![address],
        },
        timeout: Duration::from_secs(7),
    };

    let rendered = format!("{request:?}");
    for shown in [
        "POST",
        "https://api.example.com",
        "authorization",
        "Direct",
        "7s",
    ] {
        assert!(rendered.contains(shown), "{rendered}");
    }
    assert!(
        rendered.contains(&format!("body_bytes: Some({})", body.len())),
        "{rendered}"
    );
    assert!(!rendered.contains("secret"), "{rendered}");
    assert!(!rendered.contains(&address.to_string()), "{rendered}");
}
