use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::deadline::sleep_until_or_cancel;
use crate::{
    DnsResolver, HttpTransport, NetError, ReqwestTransport, RetryPolicy, TokioDnsResolver,
    UrlPolicy,
};

const DEFAULT_MAX_REDIRECTS: usize = 5;
const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_RETRIES: usize = 3;

/// Options for a bounded GET operation.
#[derive(Clone, Debug)]
pub struct FetchOptions {
    /// Total wall-clock budget across DNS, redirects, body reads, and retries.
    pub timeout: Duration,
    /// Maximum number of redirect hops, defensively capped at 20 by the client.
    pub max_redirects: usize,
    /// Maximum body prefix retained in memory.
    pub max_body_bytes: usize,
    /// Request headers.
    pub headers: HeaderMap,
    /// Retry behavior for transport failures and configured statuses.
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

/// A response body bounded to a caller-selected byte prefix.
#[derive(Clone, Debug)]
pub struct BoundedResponse {
    /// Final non-redirect status.
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
        }
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
        self.policy.validate_url(&url)?;
        let deadline = Instant::now() + options.timeout;
        let retries = options.retry.max_retries.min(MAX_RETRIES);
        let mut attempt = 0;
        loop {
            let result = self
                .fetch_redirect_chain(url.clone(), &options, deadline)
                .await;
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
