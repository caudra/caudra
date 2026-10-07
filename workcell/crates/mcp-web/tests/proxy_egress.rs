//! Pinned provider calls are the second outbound path in this crate and have
//! their own client, so proxy support has to be proven there separately. The
//! fake proxy is a local listener, so nothing here needs DNS or real egress.

use std::net::SocketAddr;
use std::time::Duration;

use http::{HeaderMap, HeaderValue, Method};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use url::Url;
use workcell_mcp_web::{
    ProductionWebHttpTransport, ProxyConfiguration, WebHttpError, WebHttpRequest,
    WebHttpRequestKind, WebHttpTransport,
};

const PROVIDER_KEY: &str = "provider-secret-canary";

/// Accept one connection and return everything the proxy was told.
async fn spawn_proxy() -> (SocketAddr, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut seen = Vec::new();
        let mut byte = [0_u8; 1];
        while stream.read_exact(&mut byte).await.is_ok() {
            seen.push(byte[0]);
            if seen.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        stream.flush().await.unwrap();
        drop(stream);
        String::from_utf8_lossy(&seen).into_owned()
    });
    (address, handle)
}

fn provider_request(url: &str) -> WebHttpRequest {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_static(PROVIDER_KEY),
    );
    WebHttpRequest {
        kind: WebHttpRequestKind::PinnedProvider,
        method: Method::POST,
        url: Url::parse(url).unwrap(),
        headers,
        body: Some(b"{}".to_vec()),
        timeout: Duration::from_secs(5),
        max_redirects: 0,
        max_body_bytes: 1024,
        cancellation: CancellationToken::new(),
    }
}

#[tokio::test]
async fn a_pinned_provider_call_tunnels_through_the_proxy_without_exposing_the_key() {
    let (address, proxy) = spawn_proxy().await;
    let transport = ProductionWebHttpTransport::with_proxy(
        ProxyConfiguration::from_values(None, None, Some(&format!("http://{address}")), None)
            .unwrap(),
    );

    let result = transport
        .execute(provider_request("https://mcp.exa.ai/mcp"))
        .await;
    assert!(
        matches!(result, Err(WebHttpError::ProxyRejected)),
        "expected a proxy refusal, got {result:?}"
    );

    let seen = proxy.await.unwrap();
    assert!(
        seen.starts_with("CONNECT mcp.exa.ai:443 "),
        "provider hop did not reach the proxy as a tunnel: {seen}"
    );
    // The tunnel is established before TLS, so the credential stays inside it.
    assert!(!seen.contains(PROVIDER_KEY));
}

#[tokio::test]
async fn a_bypassed_provider_host_is_dialled_directly() {
    let (address, proxy) = spawn_proxy().await;
    let transport = ProductionWebHttpTransport::with_proxy(
        ProxyConfiguration::from_values(
            None,
            None,
            Some(&format!("http://{address}")),
            Some("127.0.0.1"),
        )
        .unwrap(),
    );

    // A closed local port stands in for the bypassed origin, so the direct dial
    // fails immediately and the test issues no outbound traffic at all.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_address = closed.local_addr().unwrap();
    drop(closed);

    let result = transport
        .execute(provider_request(&format!("https://{closed_address}/mcp")))
        .await;
    assert!(
        matches!(result, Err(WebHttpError::RequestFailed)),
        "expected a direct dial failure, got {result:?}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), proxy)
            .await
            .is_err(),
        "the bypassed host must never reach the proxy"
    );
}
