//! The reqwest transport must actually dial the configured proxy rather than
//! the origin. Everything here runs against a local listener, so no test needs
//! DNS or outbound access.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http::{HeaderMap, Method};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use url::Url;
use workcell_net::{
    DnsError, DnsResolver, FetchOptions, HttpClient, HttpTransport, NetError, ProxyConfiguration,
    ProxyEndpoint, ReqwestTransport, RetryPolicy, TransportRequest, TransportRoute, UrlPolicy,
};

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
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while stream.read_exact(&mut byte).await.is_ok() {
            request.push(byte[0]);
            if request.ends_with(b"\r\n\r\n") {
                break;
            }
        }
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
