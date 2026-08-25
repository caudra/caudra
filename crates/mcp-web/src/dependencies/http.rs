use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use http::{HeaderMap, Method, StatusCode};
use tokio_util::sync::CancellationToken;
use url::Url;
use workcell_net::{
    FetchOptions, HttpClient, NetError, OperatorConfiguredPolicy, ReqwestTransport, RetryPolicy,
    TokioDnsResolver, UrlPolicy,
};

/// Trust/policy mode for an injected HTTP request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebHttpRequestKind {
    /// Model/user supplied target, subject to public-internet SSRF policy.
    PublicGet,
    /// Operator configured SearXNG endpoint, which may intentionally be local.
    OperatorGet,
    /// Provider-owned HTTPS endpoint. Redirects and environment proxies are forbidden.
    PinnedProvider,
}

/// Fully lowered request exposed to offline fake transports.
#[derive(Clone)]
pub struct WebHttpRequest {
    pub kind: WebHttpRequestKind,
    pub method: Method,
    pub url: Url,
    pub headers: HeaderMap,
    pub body: Option<Vec<u8>>,
    pub timeout: Duration,
    pub max_redirects: usize,
    pub max_body_bytes: usize,
    pub cancellation: CancellationToken,
}

impl fmt::Debug for WebHttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Headers and bodies can contain API credentials; never make the
        // convenient request Debug representation a secret-exfiltration path.
        let mut safe_url = self.url.clone();
        safe_url.set_query(None);
        safe_url.set_fragment(None);
        formatter
            .debug_struct("WebHttpRequest")
            .field("kind", &self.kind)
            .field("method", &self.method)
            .field("url", &safe_url)
            .field("headers", &"[REDACTED]")
            .field("body_bytes", &self.body.as_ref().map(Vec::len))
            .field("timeout", &self.timeout)
            .field("max_redirects", &self.max_redirects)
            .field("max_body_bytes", &self.max_body_bytes)
            .finish_non_exhaustive()
    }
}

/// Bounded response returned by production or fake transports.
#[derive(Clone)]
pub struct WebHttpResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub final_url: Url,
    pub body: Bytes,
    pub truncated: bool,
}

impl fmt::Debug for WebHttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut final_url = self.final_url.clone();
        final_url.set_query(None);
        final_url.set_fragment(None);
        formatter
            .debug_struct("WebHttpResponse")
            .field("status", &self.status)
            .field("headers", &"[REDACTED]")
            .field("final_url", &final_url)
            .field("body", &"[REDACTED]")
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// Sanitized transport error. Provider/connector strings are intentionally not
/// retained because they can echo authorization headers or credential values.
#[derive(Clone, Debug, thiserror::Error)]
pub enum WebHttpError {
    #[error("network operation was cancelled")]
    Cancelled,
    #[error("network operation timed out")]
    Timeout,
    #[error("network target was rejected: {0}")]
    Rejected(String),
    #[error("provider redirect was rejected")]
    RedirectRejected,
    #[error("HTTP request failed")]
    RequestFailed,
}

#[async_trait]
pub trait WebHttpTransport: Send + Sync {
    async fn execute(&self, request: WebHttpRequest) -> Result<WebHttpResponse, WebHttpError>;
}

/// Production adapter. Public and operator GETs use `workcell-net`; only the
/// provider-owned fixed HTTPS origins use reqwest directly because they require
/// no-proxy/no-redirect handling or methods outside the current bounded GET API.
#[derive(Clone)]
pub struct ProductionWebHttpTransport {
    public: HttpClient,
    operator: HttpClient,
}

impl Default for ProductionWebHttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl ProductionWebHttpTransport {
    #[must_use]
    pub fn new() -> Self {
        let operator = HttpClient::new(
            UrlPolicy::OperatorConfigured(OperatorConfiguredPolicy {
                allow_non_public_ips: true,
                allow_special_use_names: true,
                allow_url_credentials: false,
            }),
            Arc::new(TokioDnsResolver),
            Arc::new(ReqwestTransport),
        );
        Self {
            public: HttpClient::public_internet(),
            operator,
        }
    }

    async fn get(
        client: &HttpClient,
        request: WebHttpRequest,
    ) -> Result<WebHttpResponse, WebHttpError> {
        if request.method != Method::GET || request.body.is_some() {
            return Err(WebHttpError::RequestFailed);
        }
        let response = client
            .get_url(
                request.url,
                FetchOptions {
                    timeout: request.timeout,
                    max_redirects: request.max_redirects,
                    max_body_bytes: request.max_body_bytes,
                    headers: request.headers,
                    retry: RetryPolicy::disabled(),
                    cancellation: request.cancellation,
                },
            )
            .await
            .map_err(map_net_error)?;
        Ok(WebHttpResponse {
            status: response.status,
            headers: response.headers,
            final_url: response.url,
            body: response.body,
            truncated: response.truncated,
        })
    }

    async fn pinned_provider(request: WebHttpRequest) -> Result<WebHttpResponse, WebHttpError> {
        if request.url.scheme() != "https"
            || request.url.host().is_none()
            || !request.url.username().is_empty()
            || request.url.password().is_some()
            || request.url.fragment().is_some()
            || request.max_redirects != 0
            || (request.method == Method::GET && request.body.is_some())
        {
            return Err(WebHttpError::RequestFailed);
        }
        // These are fixed provider-controlled origins, not model-selected URLs.
        // Automatic redirects and environment proxies remain disabled so the
        // API key cannot be forwarded to an attacker-selected destination.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| WebHttpError::RequestFailed)?;
        let operation = async {
            let mut builder = client
                .request(request.method, request.url.clone())
                .headers(request.headers);
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            let response = builder
                .send()
                .await
                .map_err(|_| WebHttpError::RequestFailed)?;
            if response.status().is_redirection() {
                return Err(WebHttpError::RedirectRejected);
            }
            let status = response.status();
            let headers = response.headers().clone();
            let final_url = response.url().clone();
            let mut stream = response.bytes_stream();
            let mut body = BytesMut::with_capacity(request.max_body_bytes.min(16 * 1024));
            let mut truncated = false;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| WebHttpError::RequestFailed)?;
                let remaining = request.max_body_bytes.saturating_sub(body.len());
                if chunk.len() > remaining {
                    body.extend_from_slice(&chunk[..remaining]);
                    truncated = true;
                    break;
                }
                body.extend_from_slice(&chunk);
            }
            Ok(WebHttpResponse {
                status,
                headers,
                final_url,
                body: body.freeze(),
                truncated,
            })
        };
        tokio::select! {
            biased;
            () = request.cancellation.cancelled() => Err(WebHttpError::Cancelled),
            result = tokio::time::timeout(request.timeout, operation) => {
                result.map_err(|_| WebHttpError::Timeout)?
            }
        }
    }
}

#[async_trait]
impl WebHttpTransport for ProductionWebHttpTransport {
    async fn execute(&self, request: WebHttpRequest) -> Result<WebHttpResponse, WebHttpError> {
        match request.kind {
            WebHttpRequestKind::PublicGet => Self::get(&self.public, request).await,
            WebHttpRequestKind::OperatorGet => Self::get(&self.operator, request).await,
            WebHttpRequestKind::PinnedProvider => Self::pinned_provider(request).await,
        }
    }
}

fn map_net_error(error: NetError) -> WebHttpError {
    match error {
        NetError::Cancelled => WebHttpError::Cancelled,
        NetError::Timeout => WebHttpError::Timeout,
        NetError::Policy(error) => WebHttpError::Rejected(error.to_string()),
        _ => WebHttpError::RequestFailed,
    }
}
