use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream;
use http::{HeaderMap, HeaderValue, StatusCode};
use tokio_util::sync::CancellationToken;

use crate::{
    BodyStream, DnsError, DnsResolver, FetchOptions, HttpClient, HttpTransport, NetError,
    OperatorConfiguredPolicy, RetryPolicy, TransportError, TransportRequest, TransportResponse,
    UrlPolicy, UrlPolicyError,
};

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
        requests[0].resolved_addresses,
        requests[1].resolved_addresses
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
