use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream;
use http::{HeaderMap, HeaderValue, StatusCode};
use workcell_net::{
    BodyStream, DnsError, DnsResolver, HttpClient, HttpTransport, RetryPolicy, TransportError,
    TransportRequest, TransportResponse, UrlPolicy,
};

use crate::{
    CacheCounts, ResolveSourceIconOptions, SourceIconError, SourceIconResolver, SourceIconSource,
};

const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x08, 0x99, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0xab, 0xce, 0x36, 0x89, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

struct FakeDns;

#[async_trait]
impl DnsResolver for FakeDns {
    async fn resolve(&self, _hostname: &str) -> Result<Vec<IpAddr>, DnsError> {
        Ok(vec!["93.184.216.34".parse().unwrap()])
    }
}

#[derive(Clone)]
struct FakeResponse {
    status: StatusCode,
    content_type: Option<&'static str>,
    location: Option<&'static str>,
    body: &'static [u8],
}

#[derive(Default)]
struct FakeTransport {
    routes: Mutex<HashMap<String, VecDeque<FakeResponse>>>,
    requests: Mutex<Vec<String>>,
}

#[async_trait]
impl HttpTransport for FakeTransport {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        let url = request.url.to_string();
        self.requests.lock().unwrap().push(url.clone());
        let response = self
            .routes
            .lock()
            .unwrap()
            .get_mut(&url)
            .and_then(VecDeque::pop_front)
            .unwrap_or(FakeResponse {
                status: StatusCode::NOT_FOUND,
                content_type: Some("text/plain"),
                location: None,
                body: b"not found",
            });
        let mut headers = HeaderMap::new();
        if let Some(content_type) = response.content_type {
            headers.insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static(content_type),
            );
        }
        if let Some(location) = response.location {
            headers.insert(http::header::LOCATION, HeaderValue::from_static(location));
        }
        let body: BodyStream = Box::pin(stream::once(async move {
            Ok(Bytes::from_static(response.body))
        }));
        Ok(TransportResponse {
            status: response.status,
            headers,
            body,
        })
    }
}

fn test_resolver(
    routes: impl IntoIterator<Item = (&'static str, Vec<FakeResponse>)>,
) -> (SourceIconResolver, Arc<FakeTransport>) {
    let transport = Arc::new(FakeTransport {
        routes: Mutex::new(
            routes
                .into_iter()
                .map(|(url, responses)| (url.to_owned(), responses.into()))
                .collect(),
        ),
        requests: Mutex::default(),
    });
    let client = HttpClient::new(
        UrlPolicy::PublicInternet,
        Arc::new(FakeDns),
        transport.clone(),
    );
    (SourceIconResolver::new(client), transport)
}

fn ok_png() -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        content_type: Some("image/png"),
        location: None,
        body: PNG,
    }
}

fn redirect(location: &'static str) -> FakeResponse {
    FakeResponse {
        status: StatusCode::FOUND,
        content_type: None,
        location: Some(location),
        body: b"",
    }
}

#[tokio::test]
async fn prefers_html_icon_and_uses_supplied_html() {
    let (resolver, transport) = test_resolver([("https://example.com/custom.png", vec![ok_png()])]);
    let mut options = ResolveSourceIconOptions::new("https://example.com/page");
    options.html = Some("<link rel='icon' sizes='32x32' href='/custom.png'>".to_owned());
    let icon = resolver.resolve(options).await.unwrap().unwrap();
    assert_eq!(icon.icon_url, "https://example.com/custom.png");
    assert_eq!(icon.icon_source, SourceIconSource::HtmlLink);
    assert!(icon.icon_data_url.starts_with("data:image/png;base64,"));
    assert!(!icon.cache.html_fetched);
    assert_eq!(
        icon.cache.encoded,
        CacheCounts {
            hits: 0,
            misses: 1,
            writes: 1
        }
    );
    assert_eq!(
        transport.requests.lock().unwrap().as_slice(),
        ["https://example.com/custom.png"]
    );
}

#[tokio::test]
async fn fetches_bounded_page_html_when_not_supplied() {
    let (resolver, transport) = test_resolver([
        (
            "https://example.com/page",
            vec![FakeResponse {
                status: StatusCode::OK,
                content_type: Some("text/html; charset=utf-8"),
                location: None,
                body: b"<link rel='icon' href='/custom.png'>",
            }],
        ),
        ("https://example.com/custom.png", vec![ok_png()]),
    ]);
    let icon = resolver
        .resolve(ResolveSourceIconOptions::new("https://example.com/page"))
        .await
        .unwrap()
        .unwrap();
    assert!(icon.cache.html_fetched);
    assert_eq!(
        transport.requests.lock().unwrap().as_slice(),
        ["https://example.com/page", "https://example.com/custom.png"]
    );
}

#[tokio::test]
async fn traverses_directories_and_probes_in_batches() {
    let (resolver, transport) = test_resolver([
        (
            "https://example.com/docs/page",
            vec![FakeResponse {
                status: StatusCode::NOT_FOUND,
                content_type: Some("text/plain"),
                location: None,
                body: b"no page",
            }],
        ),
        (
            "https://example.com/docs/favicon.png",
            vec![ok_png(), ok_png()],
        ),
    ]);
    let mut options = ResolveSourceIconOptions::new("https://example.com/docs/page");
    options.max_candidates = 8;
    let icon = resolver.resolve(options).await.unwrap().unwrap();
    assert_eq!(icon.icon_source, SourceIconSource::PathFallback);
    assert_eq!(icon.icon_url, "https://example.com/docs/favicon.png");
    let requests = transport.requests.lock().unwrap();
    assert!(requests.contains(&"https://example.com/docs/favicon.png".to_owned()));
}

#[tokio::test]
async fn positive_probe_and_encoded_caches_avoid_second_icon_fetch() {
    let (resolver, transport) = test_resolver([(
        "https://example.com/page/favicon.png",
        vec![ok_png(), ok_png()],
    )]);
    let mut options = ResolveSourceIconOptions::new("https://example.com/page");
    options.html = Some(String::new());
    options.max_candidates = 1;
    let first = resolver.resolve(options.clone()).await.unwrap().unwrap();
    let first_count = transport.requests.lock().unwrap().len();
    let second = resolver.resolve(options).await.unwrap().unwrap();
    assert_eq!(transport.requests.lock().unwrap().len(), first_count);
    assert_eq!(first.cache.encoded.misses, 1);
    assert_eq!(second.cache.probe.hits, 1);
    assert_eq!(second.cache.encoded.hits, 1);
}

#[tokio::test]
async fn rejects_truncated_icon_and_caches_the_negative_encoding() {
    let large = Box::leak([PNG, &[0; 32]].concat().into_boxed_slice());
    let (resolver, transport) = test_resolver([(
        "https://example.com/custom.png",
        vec![FakeResponse {
            status: StatusCode::OK,
            content_type: Some("image/png"),
            location: None,
            body: large,
        }],
    )]);
    let mut options = ResolveSourceIconOptions::new("https://example.com/page");
    options.html = Some("<link rel='icon' href='/custom.png'>".to_owned());
    options.max_icon_bytes = PNG.len() - 1;
    options.max_candidates = 1;
    assert!(resolver.resolve(options).await.unwrap().is_none());
    assert!(
        transport
            .requests
            .lock()
            .unwrap()
            .contains(&"https://example.com/custom.png".to_owned())
    );
}

#[tokio::test]
async fn svg_is_normalized_to_png() {
    let (resolver, _) = test_resolver([(
        "https://example.com/icon.svg",
        vec![FakeResponse {
            status: StatusCode::OK,
            content_type: Some("image/svg+xml"),
            location: None,
            body: b"<svg xmlns='http://www.w3.org/2000/svg'></svg>",
        }],
    )]);
    let mut options = ResolveSourceIconOptions::new("https://example.com/page");
    options.html = Some("<link rel='icon' href='/icon.svg'>".to_owned());
    options.max_candidates = 1;
    let icon = resolver.resolve(options).await.unwrap().unwrap();
    assert_eq!(icon.icon_url, "https://example.com/icon.svg");
    assert!(icon.icon_data_url.starts_with("data:image/png;base64,"));
}

#[tokio::test]
async fn cancellation_is_reported_without_network_io() {
    let (resolver, transport) = test_resolver([]);
    let options = ResolveSourceIconOptions::new("https://example.com/page");
    options.cancellation.cancel();
    assert!(matches!(
        resolver.resolve(options).await,
        Err(SourceIconError::Cancelled)
    ));
    assert!(transport.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn rejects_local_page_before_network_io() {
    let (resolver, transport) = test_resolver([]);
    let result = resolver
        .resolve(ResolveSourceIconOptions::new("http://127.0.0.1/private"))
        .await
        .unwrap();
    assert!(result.is_none());
    assert!(transport.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn negative_probe_cache_is_reused_and_clearable() {
    let (resolver, transport) = test_resolver([(
        "https://example.com/page/favicon.png",
        vec![FakeResponse {
            status: StatusCode::NOT_FOUND,
            content_type: Some("text/plain"),
            location: None,
            body: b"missing",
        }],
    )]);
    let mut options = ResolveSourceIconOptions::new("https://example.com/page");
    options.html = Some(String::new());
    options.max_candidates = 1;
    assert!(resolver.resolve(options.clone()).await.unwrap().is_none());
    let request_count = transport.requests.lock().unwrap().len();
    assert!(resolver.resolve(options.clone()).await.unwrap().is_none());
    assert_eq!(transport.requests.lock().unwrap().len(), request_count);

    resolver.clear_caches();
    assert!(resolver.resolve(options).await.unwrap().is_none());
    assert_eq!(transport.requests.lock().unwrap().len(), request_count + 1);
}

#[tokio::test]
async fn declared_icons_and_total_requests_are_bounded() {
    let (resolver, transport) = test_resolver([]);
    let html = (0..100)
        .map(|index| format!("<link rel='icon' href='/icon-{index}.png'>"))
        .collect::<String>();
    let mut options = ResolveSourceIconOptions::new("https://example.com/page");
    options.html = Some(html);
    options.max_candidates = 3;
    options.max_requests = 10;

    assert!(resolver.resolve(options).await.unwrap().is_none());
    let requests = transport.requests.lock().unwrap();
    assert_eq!(
        requests.iter().filter(|url| url.contains("/icon-")).count(),
        3
    );
    assert!(requests.len() <= 10);
}

#[tokio::test]
async fn rejects_overlong_and_oversegmented_page_urls_before_io() {
    let (resolver, transport) = test_resolver([]);
    let segmented = format!(
        "https://example.com/{}/page",
        (0..129).map(|_| "a").collect::<Vec<_>>().join("/")
    );
    assert!(
        resolver
            .resolve(ResolveSourceIconOptions::new(segmented))
            .await
            .unwrap()
            .is_none()
    );
    let overlong = format!("https://example.com/{}", "a".repeat(8 * 1024));
    assert!(
        resolver
            .resolve(ResolveSourceIconOptions::new(overlong))
            .await
            .unwrap()
            .is_none()
    );
    assert!(transport.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn transient_http_failures_do_not_poison_the_encoded_cache() {
    let (resolver, transport) = test_resolver([(
        "https://example.com/custom.png",
        vec![
            FakeResponse {
                status: StatusCode::SERVICE_UNAVAILABLE,
                content_type: Some("text/plain"),
                location: None,
                body: b"busy",
            },
            ok_png(),
        ],
    )]);
    let mut options = ResolveSourceIconOptions::new("https://example.com/page");
    options.html = Some("<link rel='icon' href='/custom.png'>".to_owned());
    options.max_candidates = 1;
    options.max_requests = 1;
    options.retry = RetryPolicy::disabled();

    assert!(resolver.resolve(options.clone()).await.unwrap().is_none());
    assert!(resolver.resolve(options).await.unwrap().is_some());
    assert_eq!(
        transport
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|url| url.as_str() == "https://example.com/custom.png")
            .count(),
        2
    );
}

#[tokio::test]
async fn icon_fetches_follow_two_redirects_but_not_a_third() {
    let (resolver, transport) = test_resolver([
        (
            "https://example.com/custom.png",
            vec![redirect("/redirect-1.png")],
        ),
        (
            "https://example.com/redirect-1.png",
            vec![redirect("/redirect-2.png")],
        ),
        (
            "https://example.com/redirect-2.png",
            vec![redirect("/redirect-3.png")],
        ),
        ("https://example.com/redirect-3.png", vec![ok_png()]),
    ]);
    let mut options = ResolveSourceIconOptions::new("https://example.com/page");
    options.html = Some("<link rel='icon' href='/custom.png'>".to_owned());
    options.max_candidates = 1;
    options.max_requests = 1;
    options.retry = RetryPolicy::disabled();

    assert!(resolver.resolve(options).await.unwrap().is_none());
    assert_eq!(
        transport.requests.lock().unwrap().as_slice(),
        [
            "https://example.com/custom.png",
            "https://example.com/redirect-1.png",
            "https://example.com/redirect-2.png",
        ]
    );
}

struct PendingBodyTransport {
    requests: Mutex<usize>,
}

#[async_trait]
impl HttpTransport for PendingBodyTransport {
    async fn execute(
        &self,
        _request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        *self.requests.lock().unwrap() += 1;
        Ok(TransportResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Box::pin(stream::pending()),
        })
    }
}

#[tokio::test]
async fn total_resolution_deadline_covers_icon_body_reads() {
    let transport = Arc::new(PendingBodyTransport {
        requests: Mutex::new(0),
    });
    let client = HttpClient::new(
        UrlPolicy::PublicInternet,
        Arc::new(FakeDns),
        transport.clone(),
    );
    let resolver = SourceIconResolver::new(client);
    let mut options = ResolveSourceIconOptions::new("https://example.com/page");
    options.html = Some("<link rel='icon' href='/custom.png'>".to_owned());
    options.total_timeout = Duration::from_millis(5);
    options.timeout = Duration::from_secs(1);
    options.max_candidates = 1;

    let result = tokio::time::timeout(Duration::from_millis(100), resolver.resolve(options))
        .await
        .expect("resolver obeys total deadline")
        .unwrap();
    assert!(result.is_none());
    assert_eq!(*transport.requests.lock().unwrap(), 1);
}
