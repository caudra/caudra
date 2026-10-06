use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::header::HeaderName;
use http::{HeaderMap, Method, StatusCode};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::deadline::sleep_until_or_cancel;
use crate::{
    DnsResolver, HttpTransport, NetError, ProxyConfiguration, ReqwestTransport, RetryPolicy,
    TokioDnsResolver, UrlPolicy,
};

/// Largest request body [`HttpClient::request`] will send.
pub const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;

const DEFAULT_MAX_REDIRECTS: usize = 5;
const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_RETRIES: usize = 3;
/// Headers the transport derives from the request and the connection. A
/// caller value could reframe the body or smuggle a second request, retarget
/// the hop, or change how the connection is used.
const RESERVED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "upgrade",
    "expect",
];
/// Proxy headers address the proxy, which the operator configures, and on a
/// direct hop would hand a proxy credential to the origin.
const RESERVED_HEADER_PREFIX: &str = "proxy-";

/// Options for a bounded request.
#[derive(Clone, Debug)]
pub struct FetchOptions {
    /// Total wall-clock budget across DNS, redirects, body reads, and retries.
    pub timeout: Duration,
    /// Maximum number of redirect hops, defensively capped at 20 by the client.
    pub max_redirects: usize,
    /// Maximum response body prefix retained in memory.
    pub max_body_bytes: usize,
    /// Request headers.
    pub headers: HeaderMap,
    /// Retry behavior for transport failures and configured statuses. Only
    /// idempotent methods are ever retried.
    pub retry: RetryPolicy,
    /// Cooperative caller cancellation.
    pub cancellation: CancellationToken,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            max_redirects: DEFAULT_MAX_REDIRECTS,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            headers: HeaderMap::new(),
            retry: RetryPolicy::default(),
            cancellation: CancellationToken::new(),
        }
    }
}

/// Which redirects [`HttpClient::request`] may follow.
///
/// Whatever the scope, each followed hop gets the same policy check,
/// resolution, and pinning as the first, and a redirect the scope does not
/// allow is returned as the final response with its bounded body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RedirectScope {
    /// Follow a redirect only to the origin of the hop that answered. A GET
    /// without a body follows any redirect status there. Any other request
    /// follows only a 307 or 308, re-sending its method, body, and headers.
    SameOrigin,
    /// A GET without a body also follows a redirect to another origin, after
    /// rebuilding its headers from a safe allowlist. This is what
    /// [`HttpClient::get_url`] does. Any other request is treated as
    /// [`Self::SameOrigin`], because a replayed request must not move origin.
    AnyOrigin,
}

/// One request for [`HttpClient::request`].
#[derive(Clone)]
pub struct RequestSpec {
    /// Request method.
    pub method: Method,
    /// Target URL, validated by the client policy before any I/O.
    pub url: Url,
    /// Request body of at most [`MAX_REQUEST_BODY_BYTES`].
    pub body: Option<Bytes>,
    /// Which redirects may be followed.
    pub redirects: RedirectScope,
    /// Deadline, redirect, response, header, retry, and cancellation bounds.
    /// A header the client derives itself fails with
    /// [`NetError::ReservedHeader`].
    pub options: FetchOptions,
}

impl fmt::Debug for RequestSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Paths, queries, header values, and bodies routinely carry credentials,
        // so a derived representation would turn any debug log into a disclosure.
        formatter
            .debug_struct("RequestSpec")
            .field("method", &self.method)
            .field("origin", &self.url.origin().ascii_serialization())
            .field("redirects", &self.redirects)
            .field(
                "header_names",
                &self.options.headers.keys().collect::<Vec<_>>(),
            )
            .field("body_bytes", &self.body.as_ref().map(Bytes::len))
            .finish_non_exhaustive()
    }
}

/// A response body bounded to a caller-selected byte prefix.
#[derive(Clone, Debug)]
pub struct BoundedResponse {
    /// Final status. It is a redirect status only when [`HttpClient::request`]
    /// ended the chain on a redirect it does not follow.
    pub status: StatusCode,
    /// Final response headers.
    pub headers: HeaderMap,
    /// Final validated URL after manual redirects.
    pub url: Url,
    /// At most `FetchOptions::max_body_bytes` bytes.
    pub body: Bytes,
    /// Whether more bytes existed and the stream was dropped at the bound.
    pub truncated: bool,
}

/// Policy-enforcing, bounded HTTP client with injectable DNS and transport.
#[derive(Clone)]
pub struct HttpClient {
    pub(crate) policy: UrlPolicy,
    pub(crate) resolver: Arc<dyn DnsResolver>,
    pub(crate) transport: Arc<dyn HttpTransport>,
    pub(crate) proxy: ProxyConfiguration,
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::public_internet()
    }
}

impl HttpClient {
    /// Construct the production public-internet client.
    #[must_use]
    pub fn public_internet() -> Self {
        Self::new(
            UrlPolicy::PublicInternet,
            Arc::new(TokioDnsResolver),
            Arc::new(ReqwestTransport),
        )
    }

    /// Construct a client with explicit policy, resolver, and one-hop transport.
    #[must_use]
    pub fn new(
        policy: UrlPolicy,
        resolver: Arc<dyn DnsResolver>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            policy,
            resolver,
            transport,
            proxy: ProxyConfiguration::direct(),
        }
    }

    /// Route matching hops through an operator-configured outbound proxy.
    ///
    /// Hostname, scheme, credential, and IP-literal policy still apply. What a
    /// proxied hop gives up is local resolution and connector pinning, because
    /// the proxy performs the lookup and therefore owns the address decision.
    #[must_use]
    pub fn with_proxy(mut self, proxy: ProxyConfiguration) -> Self {
        self.proxy = proxy;
        self
    }

    /// Return the URL policy used for initial and redirect targets.
    #[must_use]
    pub const fn policy(&self) -> UrlPolicy {
        self.policy
    }

    /// Parse and fetch a URL with bounded retries.
    pub async fn get(
        &self,
        value: &str,
        options: FetchOptions,
    ) -> Result<BoundedResponse, NetError> {
        let url = self.policy.parse_url(value, None)?;
        self.get_url(url, options).await
    }

    /// Fetch an already parsed URL. It is revalidated before any I/O.
    pub async fn get_url(
        &self,
        url: Url,
        options: FetchOptions,
    ) -> Result<BoundedResponse, NetError> {
        self.request(RequestSpec {
            method: Method::GET,
            url,
            body: None,
            redirects: RedirectScope::AnyOrigin,
            options,
        })
        .await
    }

    /// Send one bounded request. Before any DNS or I/O the URL is revalidated,
    /// the body length checked against [`MAX_REQUEST_BODY_BYTES`], and the
    /// headers checked for one the client derives itself: `Host`,
    /// `Content-Length`, `Transfer-Encoding`, `Connection`, `Keep-Alive`, `TE`,
    /// `Trailer`, `Upgrade`, `Expect`, or any `Proxy-*`, which fails with
    /// [`NetError::ReservedHeader`].
    ///
    /// A GET without a body follows any redirect status, to another origin
    /// only under [`RedirectScope::AnyOrigin`], where it behaves exactly as
    /// [`Self::get_url`]. Any other request follows only a same-origin 307 or
    /// 308, re-sending its method and body, whatever the scope. A redirect that
    /// is not followed is returned as the final response, and one that would be
    /// followed past `max_redirects` is an error. Only idempotent methods are
    /// retried.
    pub async fn request(&self, request: RequestSpec) -> Result<BoundedResponse, NetError> {
        self.policy.validate_url(&request.url)?;
        if let Some(length) = request
            .body
            .as_ref()
            .map(Bytes::len)
            .filter(|length| *length > MAX_REQUEST_BODY_BYTES)
        {
            return Err(NetError::RequestBodyTooLarge { length });
        }
        if let Some(name) = request
            .options
            .headers
            .keys()
            .find(|name| is_reserved(name))
        {
            return Err(NetError::ReservedHeader {
                name: name.as_str().to_owned(),
            });
        }
        let options = &request.options;
        let deadline = Instant::now() + options.timeout;
        // Neither a transport failure nor a retry status proves the server did
        // nothing, so a request that may not be repeated is attempted once
        // whatever the policy allows.
        let retries = if request.method.is_idempotent() {
            options.retry.max_retries.min(MAX_RETRIES)
        } else {
            0
        };
        let mut attempt = 0;
        loop {
            let result = self.fetch_redirect_chain(&request, deadline).await;
            match result {
                Ok(response)
                    if options.retry.statuses.contains(&response.status) && attempt < retries =>
                {
                    let delay = options.retry.delay_for(attempt, Some(&response.headers));
                    sleep_until_or_cancel(delay, deadline, &options.cancellation).await?;
                    attempt += 1;
                }
                Err(error) if error.is_retryable() && attempt < retries => {
                    let delay = options.retry.delay_for(attempt, None);
                    sleep_until_or_cancel(delay, deadline, &options.cancellation).await?;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }
}

fn is_reserved(name: &HeaderName) -> bool {
    // A `HeaderName` is lowercase by construction, so no spelling of a
    // reserved name can slip past these exact comparisons.
    let name = name.as_str();
    RESERVED_HEADERS.contains(&name) || name.starts_with(RESERVED_HEADER_PREFIX)
}
