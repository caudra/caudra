use std::net::IpAddr;
use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use http::{HeaderMap, Method, StatusCode};
use thiserror::Error;
use url::Url;

/// A streaming response body returned by an injectable transport.
pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, TransportError>> + Send + 'static>>;

/// A transport-level failure, before HTTP status handling.
#[derive(Debug, Error)]
#[error("HTTP transport failed: {message}")]
pub struct TransportError {
    message: String,
}

impl TransportError {
    /// Construct an error suitable for an injected transport.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// A single already-validated HTTP request.
#[derive(Clone, Debug)]
pub struct TransportRequest {
    /// Request method. The high-level client currently emits only GET.
    pub method: Method,
    /// Validated URL for this exact hop.
    pub url: Url,
    /// Caller headers after redirect-sensitive filtering.
    pub headers: HeaderMap,
    /// Every policy-approved DNS answer, used for connector pinning.
    pub resolved_addresses: Vec<IpAddr>,
    /// Remaining total operation time.
    pub timeout: Duration,
}

/// An HTTP response whose body has not yet been buffered.
pub struct TransportResponse {
    /// Response status.
    pub status: StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    /// Streaming body. Dropping it must cancel or close further reads.
    pub body: BodyStream,
}

/// Injectable HTTP seam used by offline tests and alternate connectors.
///
/// Implementations MUST perform exactly one request and MUST NOT follow
/// redirects. Redirect policy belongs to [`crate::HttpClient`], where each new
/// hostname can be resolved and checked before any connection is attempted.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// Execute one request hop.
    async fn execute(&self, request: TransportRequest)
    -> Result<TransportResponse, TransportError>;
}

/// Reqwest transport with redirects and environment proxies disabled.
///
/// A client is built per hop so the DNS answers vetted by policy can be pinned
/// into the connector. This avoids the validate-then-resolve race that otherwise
/// permits DNS rebinding between policy lookup and socket connection.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReqwestTransport;

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        let port = request
            .url
            .port_or_known_default()
            .ok_or_else(|| TransportError::new("URL has no known port"))?;
        let sockets = request
            .resolved_addresses
            .iter()
            .map(|address| std::net::SocketAddr::new(*address, port))
            .collect::<Vec<_>>();
        let hostname = request
            .url
            .host_str()
            .ok_or_else(|| TransportError::new("URL has no hostname"))?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(request.timeout)
            .resolve_to_addrs(hostname, &sockets)
            .build()
            .map_err(|error| TransportError::new(error.to_string()))?;
        let response = client
            .request(request.method, request.url)
            .headers(request.headers)
            .send()
            .await
            .map_err(|error| TransportError::new(error.to_string()))?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .bytes_stream()
            .map(|result| result.map_err(|error| TransportError::new(error.to_string())));
        Ok(TransportResponse {
            status,
            headers,
            body: Box::pin(body),
        })
    }
}
