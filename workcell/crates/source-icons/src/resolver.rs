use std::sync::{Arc, LazyLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use workcell_net::{HttpClient, RetryPolicy};

use crate::budget::ResolutionBudget;
use crate::cache::IconCaches;
use crate::html::discover_html_icons;
use crate::resolver_options::{normalized_options, url_is_within_limits};

/// How an icon URL was discovered.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceIconSource {
    /// A `<link rel="icon">`-style declaration in page HTML.
    HtmlLink,
    /// A conventional favicon path guessed from the page hierarchy.
    PathFallback,
}

/// Hit/miss/write counts for one cache during a resolution.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CacheCounts {
    /// Reused positive or negative entries.
    pub hits: usize,
    /// Absent or expired entries.
    pub misses: usize,
    /// Positive or negative entries inserted.
    pub writes: usize,
}

/// Per-call cache diagnostics matching the TypeScript package behavior.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceIconCacheInfo {
    /// Whether page HTML had to be fetched by the resolver.
    pub html_fetched: bool,
    /// Fallback path probe cache activity.
    pub probe: CacheCounts,
    /// Normalized PNG cache activity.
    pub encoded: CacheCounts,
}

/// A discovered icon normalized to a PNG data URL.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedSourceIcon {
    /// Original HTML URL, or final redirected URL for a fallback probe.
    pub icon_url: String,
    /// Trusted `data:image/png;base64,...` payload.
    pub icon_data_url: String,
    /// Discovery mechanism.
    pub icon_source: SourceIconSource,
    /// Cache diagnostics for this resolution.
    pub cache: SourceIconCacheInfo,
}

/// Source icon resolution options.
#[derive(Clone, Debug)]
pub struct ResolveSourceIconOptions {
    /// Page whose icon should be discovered.
    pub page_url: String,
    /// Caller-supplied HTML, avoiding an extra page request.
    pub html: Option<String>,
    /// Total timeout for each HTML or full-icon fetch.
    pub timeout: Duration,
    /// Total timeout for each cheap fallback probe.
    pub probe_timeout: Duration,
    /// Maximum complete icon download size.
    pub max_icon_bytes: usize,
    /// Maximum fallback probe prefix size.
    pub max_probe_bytes: usize,
    /// Maximum page HTML prefix size.
    pub max_html_bytes: usize,
    /// Maximum fallback candidates considered.
    pub max_candidates: usize,
    /// Hard wall-clock bound for returning the async resolution result across
    /// discovery, network requests, and decoding.
    pub total_timeout: Duration,
    /// Maximum logical fetch operations across the complete resolution.
    /// Redirects and retries remain inside one operation; `total_timeout` is
    /// the caller-visible hard bound across every operation and redirect hop.
    pub max_requests: usize,
    /// Number of fallback probes issued concurrently.
    pub probe_batch_size: usize,
    /// PNG output dimensions, normalized largest-first.
    pub output_sizes: Vec<u32>,
    /// PNG compression-quality ladder, normalized largest-first.
    pub output_png_qualities: Vec<u8>,
    /// Return the first ladder output at or below this data URL length.
    pub data_url_soft_limit: usize,
    /// Retry policy inherited by every bounded fetch.
    pub retry: RetryPolicy,
    /// Cooperative cancellation shared across the whole resolution.
    pub cancellation: CancellationToken,
}

impl ResolveSourceIconOptions {
    /// Construct options with conservative decoration-oriented defaults.
    #[must_use]
    pub fn new(page_url: impl Into<String>) -> Self {
        Self {
            page_url: page_url.into(),
            html: None,
            timeout: Duration::from_millis(1_500),
            probe_timeout: Duration::from_millis(400),
            max_icon_bytes: 1_000_000,
            max_probe_bytes: 2_048,
            max_html_bytes: 512 * 1_024,
            max_candidates: 20,
            total_timeout: Duration::from_secs(5),
            max_requests: 24,
            probe_batch_size: 3,
            output_sizes: vec![24, 20, 16],
            output_png_qualities: vec![90, 80, 70],
            data_url_soft_limit: 2_048,
            retry: RetryPolicy::default(),
            cancellation: CancellationToken::new(),
        }
    }
}

/// A non-recoverable source icon operation failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SourceIconError {
    /// The caller cancelled resolution.
    #[error("source icon resolution was cancelled")]
    Cancelled,
}

/// Reusable resolver with bounded positive and negative LRU caches.
#[derive(Clone)]
pub struct SourceIconResolver {
    pub(crate) client: HttpClient,
    pub(crate) caches: Arc<IconCaches>,
}

impl Default for SourceIconResolver {
    fn default() -> Self {
        Self::new(HttpClient::public_internet())
    }
}

impl SourceIconResolver {
    /// Create a resolver around an injectable policy-enforcing HTTP client.
    #[must_use]
    pub fn new(client: HttpClient) -> Self {
        Self {
            client,
            caches: Arc::new(IconCaches::default()),
        }
    }

    /// Resolve and normalize one page icon.
    pub async fn resolve(
        &self,
        options: ResolveSourceIconOptions,
    ) -> Result<Option<ResolvedSourceIcon>, SourceIconError> {
        let options = normalized_options(options);
        let Ok(page_url) = self.client.policy().parse_url(&options.page_url, None) else {
            return Ok(None);
        };
        if !url_is_within_limits(&page_url) {
            return Ok(None);
        }
        if options.cancellation.is_cancelled() {
            return Err(SourceIconError::Cancelled);
        }
        let budget = ResolutionBudget::new(options.total_timeout, options.max_requests);
        let mut cache = SourceIconCacheInfo::default();
        let html = match options.html.as_deref() {
            Some(html) => Some(bounded_utf8(html, options.max_html_bytes)),
            None => {
                cache.html_fetched = true;
                self.fetch_page_html(&page_url, &options, &budget).await?
            }
        };
        if let Some(html) = html {
            for candidate in discover_html_icons(&html, &page_url, self.client.policy())
                .into_iter()
                .filter(|candidate| url_is_within_limits(&candidate.url))
                .take(options.max_candidates)
            {
                if let Some(icon) = self
                    .encode_icon(
                        candidate.url,
                        SourceIconSource::HtmlLink,
                        &options,
                        &budget,
                        &mut cache,
                    )
                    .await?
                {
                    return Ok(Some(icon));
                }
            }
        }

        let Some(url) = self
            .resolve_fallback(&page_url, &options, &budget, &mut cache)
            .await?
        else {
            return Ok(None);
        };
        self.encode_icon(
            url,
            SourceIconSource::PathFallback,
            &options,
            &budget,
            &mut cache,
        )
        .await
    }

    /// Clear both positive and negative caches owned by this resolver.
    pub fn clear_caches(&self) {
        self.caches.clear();
    }
}

fn bounded_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value[..boundary].to_owned()
}

static GLOBAL_RESOLVER: LazyLock<SourceIconResolver> = LazyLock::new(SourceIconResolver::default);

/// Resolve with the process-global production resolver and caches.
pub async fn resolve_source_icon(
    options: ResolveSourceIconOptions,
) -> Result<Option<ResolvedSourceIcon>, SourceIconError> {
    GLOBAL_RESOLVER.resolve(options).await
}

/// Clear the process-global positive and negative source icon caches.
pub fn clear_source_icon_caches() {
    GLOBAL_RESOLVER.clear_caches();
}
