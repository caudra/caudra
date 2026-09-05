use std::net::IpAddr;
use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use http::{HeaderMap, Method, StatusCode};
use thiserror::Error;
use url::Url;

use crate::ProxyEndpoint;

/// A streaming response body returned by an injectable transport.
pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, TransportError>> + Send + 'static>>;

/// A transport-level failure, before HTTP status handling.
#[derive(Debug, Error)]
#[error("HTTP transport failed: {message}")]
pub struct TransportError {
    message: String,
    proxy: bool,
}

impl TransportError {
    /// Construct an error suitable for an injected transport.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            proxy: false,
        }
    }

    /// Construct an error for a hop that failed at the configured proxy.
    #[must_use]
    pub fn proxy(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            proxy: true,
        }
    }

    /// Whether the failure occurred reaching or negotiating with the proxy.
    #[must_use]
    pub const fn is_proxy(&self) -> bool {
        self.proxy
    }
}

/// How a hop must reach its target.
///
/// Splitting the route this way makes a direct hop without pinned addresses
/// unrepresentable. A single optional address list would let a caller silently
/// fall back to connector DNS, which is exactly the validate-then-resolve race
/// the client exists to prevent.
#[derive(Clone, Debug)]
pub enum TransportRoute {
    /// Connect straight to these policy-approved addresses.
    Direct {
        /// Every DNS answer accepted by policy, used for connector pinning.
        resolved_addresses: Vec<IpAddr>,
    },
    /// Connect through this proxy, which owns resolution and address policy.
    Proxy {
        /// Operator-configured proxy for this hop.
        endpoint: ProxyEndpoint,
    },
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
    /// Whether this hop is dialled directly or through a proxy.
    pub route: TransportRoute,
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

/// Reqwest transport with redirects and ambient environment proxies disabled.
///
/// A client is built per hop so the DNS answers vetted by policy can be pinned
/// into the connector. This avoids the validate-then-resolve race that otherwise
/// permits DNS rebinding between policy lookup and socket connection. A proxied
/// hop has no answers to pin: the proxy resolves the name, so it also owns the
/// address policy for that hop.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReqwestTransport;

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn execute(
        &self,
        request: TransportRequest,
    ) -> Result<TransportResponse, TransportError> {
        let proxied = matches!(request.route, TransportRoute::Proxy { .. });
        // `no_proxy` also disables reqwest's own environment detection, so the
        // only proxy reachable from here is the one selected by policy above.
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(request.timeout);
        match &request.route {
            TransportRoute::Direct { resolved_addresses } => {
                if resolved_addresses.is_empty() {
                    // Passing an empty override to reqwest would silently hand
                    // the hostname back to system DNS, unpinned and unchecked.
                    return Err(TransportError::new("direct route has no resolved address"));
                }
                let port = request
                    .url
                    .port_or_known_default()
                    .ok_or_else(|| TransportError::new("URL has no known port"))?;
                let sockets = resolved_addresses
                    .iter()
                    .map(|address| std::net::SocketAddr::new(*address, port))
                    .collect::<Vec<_>>();
                let hostname = request
                    .url
                    .host_str()
                    .ok_or_else(|| TransportError::new("URL has no hostname"))?;
                builder = builder.resolve_to_addrs(hostname, &sockets);
            }
            TransportRoute::Proxy { endpoint } => {
                builder = endpoint.apply(builder);
            }
        }
        let client = builder
            .build()
            .map_err(|error| TransportError::new(error.to_string()))?;
        let response = client
            .request(request.method, request.url)
            .headers(request.headers)
            .send()
            .await
            .map_err(|error| {
                // A refusal from an enforcing proxy arrives as a failed connect.
                // The reqwest message is not retained for it: an operator proxy
                // URL is topology and may carry credentials.
                if proxied && error.is_connect() {
                    TransportError::proxy("outbound proxy refused the connection")
                } else {
                    TransportError::new(error.to_string())
                }
            })?;
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
