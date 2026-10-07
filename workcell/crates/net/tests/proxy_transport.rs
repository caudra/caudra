//! The reqwest transport must actually dial the configured proxy rather than
//! the origin, send what the request carries, and leave a hop the origin
//! never finishes to end as a timeout. Everything here runs against a local
//! listener, so no test needs DNS or outbound access.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use test_case::test_case;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use url::Url;
use workcell_net::{
    DnsError, DnsResolver, FetchOptions, HttpClient, HttpTransport, NetError,
    OperatorConfiguredPolicy, ProxyConfiguration, ProxyEndpoint, RedirectScope, RequestSpec,
    ReqwestTransport, RetryPolicy, TokioDnsResolver, TransportError, TransportRequest,
    TransportResponse, TransportRoute, UrlPolicy,
};

const PAYLOAD: &[u8] = br#"{"title":"ship it"}"#;
const QUERY_SECRET: &str = "query-secret";
const STALL_TIMEOUT: Duration = Duration::from_millis(500);
const STALLED_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc";

/// A resolver that fails the test if a proxied hop tries to resolve a name.
struct ForbiddenDns;

#[async_trait]
impl DnsResolver for ForbiddenDns {
    async fn resolve(&self, _hostname: &str) -> Result<Vec<std::net::IpAddr>, DnsError> {
        panic!("a proxied hop must not resolve the target itself");
    }
}

/// Accept one connection, capture the request line, and optionally answer.
async fn spawn_proxy(reply: Option<&'static [u8]>) -> (SocketAddr, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_head(&mut stream).await;
        if let Some(reply) = reply {
            stream.write_all(reply).await.unwrap();
            stream.flush().await.unwrap();
        }
        drop(stream);
        String::from_utf8_lossy(&request)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    });
    (address, handle)
}

/// Accept one connection, answer it, and return its request line and body.
async fn spawn_recorder(reply: &'static [u8]) -> (SocketAddr, JoinHandle<(String, Vec<u8>)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut request_line = String::new();
        stream.read_line(&mut request_line).await.unwrap();
        let mut content_length = 0;
        loop {
            let mut header = String::new();
            stream.read_line(&mut header).await.unwrap();
            if header == "\r\n" {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; content_length];
        stream.read_exact(&mut body).await.unwrap();
        stream.get_mut().write_all(reply).await.unwrap();
        (request_line.trim_end().to_owned(), body)
    });
    (address, handle)
}

/// Accept one connection, answer its request with `reply`, and then hold the
/// connection open without another byte, so only a timer can end the hop.
async fn spawn_staller(reply: &'static [u8], stalled: Arc<Notify>) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // Answering before the request is written would arrive on an idle
        // connection, which the client rejects rather than reads as a reply.
        read_head(&mut stream).await;
        stream.write_all(reply).await.unwrap();
        if reply.is_empty() {
            stalled.notify_one();
        }
        std::future::pending::<()>().await;
        drop(stream);
    });
    (address, server)
}

/// Read up to the blank line that ends a request head, or to end of stream.
async fn read_head(stream: &mut TcpStream) -> Vec<u8> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") && stream.read_exact(&mut byte).await.is_ok() {
        head.push(byte[0]);
    }
    head
}

/// Drives the real transport and counts the response heads it returns, so a
/// stalled-body case can show its hop failed while reading the body.
struct HeadCounter {
    heads: AtomicUsize,
    stalled: Arc<Notify>,
}

#[async_trait]
impl HttpTransport for HeadCounter {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        let response = ReqwestTransport.execute(request).await?;
        self.heads.fetch_add(1, Ordering::Relaxed);
        self.stalled.notify_one();
        Ok(response)
    }
}

#[tokio::test]
async fn a_plain_http_target_is_sent_to_the_proxy_in_absolute_form() {
    let (address, proxy) =
        spawn_proxy(Some(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")).await;
    let endpoint = ProxyEndpoint::parse(&format!("http://{address}")).unwrap();
    let response = ReqwestTransport
        .execute(TransportRequest {
            method: Method::GET,
            url: Url::parse("http://example.com/page").unwrap(),
            headers: HeaderMap::new(),
            body: None,
            route: TransportRoute::Proxy { endpoint },
            timeout: Duration::from_secs(5),
        })
        .await
        .unwrap();

    assert_eq!(response.status, http::StatusCode::OK);
    assert_eq!(
        proxy.await.unwrap(),
        "GET http://example.com/page HTTP/1.1",
        "the origin form would mean the client dialled the target directly"
    );
}

#[tokio::test]
async fn an_https_target_reaches_the_proxy_as_a_tunnel_request() {
    // Closing after CONNECT proves the hop terminated at the proxy, and that a
    // refusal there surfaces as its own error rather than a retryable failure.
    let (address, proxy) = spawn_proxy(None).await;
    let client = HttpClient::new(
        UrlPolicy::PublicInternet,
        Arc::new(ForbiddenDns),
        Arc::new(ReqwestTransport),
    )
    .with_proxy(
        ProxyConfiguration::from_values(None, None, Some(&format!("http://{address}")), None)
            .unwrap(),
    );
    let result = client
        .get(
            "https://example.com/search",
            FetchOptions {
                timeout: Duration::from_secs(5),
                retry: RetryPolicy::disabled(),
                ..FetchOptions::default()
            },
        )
        .await;

    assert!(
        matches!(result, Err(NetError::Proxy(_))),
        "expected a proxy failure, got {result:?}"
    );
    assert_eq!(proxy.await.unwrap(), "CONNECT example.com:443 HTTP/1.1");
}

#[tokio::test]
async fn a_proxied_request_reaches_the_proxy_with_its_method_and_body() {
    let (address, proxy) =
        spawn_recorder(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n").await;
    let client = HttpClient::new(
        UrlPolicy::PublicInternet,
        Arc::new(ForbiddenDns),
        Arc::new(ReqwestTransport),
    )
    .with_proxy(
        ProxyConfiguration::from_values(Some(&format!("http://{address}")), None, None, None)
            .unwrap(),
    );
    let response = client
        .request(RequestSpec {
            method: Method::POST,
            url: Url::parse("http://example.com/items").unwrap(),
            body: Some(Bytes::from_static(PAYLOAD)),
            redirects: RedirectScope::SameOrigin,
            options: FetchOptions {
                timeout: Duration::from_secs(5),
                ..FetchOptions::default()
            },
        })
        .await
        .unwrap();

    assert_eq!(response.status, StatusCode::CREATED);
    assert_eq!(
        proxy.await.unwrap(),
        (
            "POST http://example.com/items HTTP/1.1".to_owned(),
            PAYLOAD.to_vec()
        )
    );
}

#[tokio::test]
async fn a_direct_request_reaches_the_pinned_address_with_its_method_and_body() {
    let (address, origin) = spawn_recorder(b"HTTP/1.1 204 No Content\r\n\r\n").await;
    let response = ReqwestTransport
        .execute(TransportRequest {
            method: Method::PUT,
            url: Url::parse(&format!("http://pinned.example:{}/items", address.port())).unwrap(),
            headers: HeaderMap::new(),
            body: Some(Bytes::from_static(PAYLOAD)),
            route: TransportRoute::Direct {
                resolved_addresses: vec![address.ip()],
            },
            timeout: Duration::from_secs(5),
        })
        .await
        .unwrap();

    assert_eq!(response.status, StatusCode::NO_CONTENT);
    assert_eq!(
        origin.await.unwrap(),
        ("PUT /items HTTP/1.1".to_owned(), PAYLOAD.to_vec())
    );
}

#[tokio::test]
async fn a_transport_failure_does_not_echo_the_request_url() {
    // Closing after the request head fails the hop once reqwest has attached
    // the URL to its error.
    let (address, origin) = spawn_proxy(None).await;
    let result = ReqwestTransport
        .execute(TransportRequest {
            method: Method::GET,
            url: Url::parse(&format!("http://{address}/items?token={QUERY_SECRET}")).unwrap(),
            headers: HeaderMap::new(),
            body: None,
            route: TransportRoute::Direct {
                resolved_addresses: vec![address.ip()],
            },
            timeout: Duration::from_secs(5),
        })
        .await;
    origin.await.unwrap();

    let Err(error) = result else {
        panic!("a connection closed without a response must fail the hop");
    };
    assert!(!error.to_string().contains(QUERY_SECRET), "{error}");
}

#[test_case(b"", 0 ; "an origin that never answers")]
#[test_case(STALLED_HEAD, 1 ; "a body that stalls after the head")]
#[tokio::test(start_paused = true)]
async fn a_hop_the_origin_never_finishes_is_a_timeout(reply: &'static [u8], heads: usize) {
    let stalled = Arc::new(Notify::new());
    let (address, server) = spawn_staller(reply, stalled.clone()).await;
    let transport = Arc::new(HeadCounter {
        heads: AtomicUsize::new(0),
        stalled: stalled.clone(),
    });
    let client = HttpClient::new(
        UrlPolicy::OperatorConfigured(OperatorConfiguredPolicy {
            allow_non_public_ips: true,
            allow_special_use_names: false,
            allow_url_credentials: false,
        }),
        Arc::new(TokioDnsResolver),
        transport.clone(),
    );
    let request = client.request(RequestSpec {
        method: Method::GET,
        url: Url::parse(&format!("http://{address}/items?token={QUERY_SECRET}")).unwrap(),
        body: None,
        redirects: RedirectScope::SameOrigin,
        options: FetchOptions {
            timeout: STALL_TIMEOUT,
            retry: RetryPolicy::disabled(),
            ..FetchOptions::default()
        },
    });
    tokio::pin!(request);
    loop {
        tokio::select! {
            result = &mut request => panic!("the hop ended before stalling: {result:?}"),
            () = stalled.notified() => break,
            () = tokio::task::yield_now() => {}
        }
    }
    assert_eq!(transport.heads.load(Ordering::Relaxed), heads);
    tokio::time::advance(STALL_TIMEOUT).await;
    let result = request.await;
    server.abort();

    let Err(error) = result else {
        panic!("a hop the origin never finishes must fail");
    };
    assert!(matches!(error, NetError::Timeout), "{error:?}");
    assert!(!error.to_string().contains(QUERY_SECRET), "{error}");
    assert_eq!(transport.heads.load(Ordering::Relaxed), heads);
}
