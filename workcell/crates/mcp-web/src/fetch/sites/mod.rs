//! Site rules: per-site knowledge of where a page's content can actually be
//! read. A rule turns a URL into ordered attempts, each with its own target,
//! headers, and optionally a renderer for a feed or API response. Adding a site
//! is one module here and one entry in [`RULES`].

mod reddit;

use http::{HeaderMap, HeaderValue, StatusCode};
use url::Url;

use super::content;
use crate::types::WebfetchFormat;

const RULES: &[&dyn SiteRule] = &[&reddit::Reddit];

pub(crate) trait SiteRule: Sync {
    /// The attempts that read `url`, or `None` to fetch it unchanged.
    fn plan(&self, url: &Url, format: WebfetchFormat) -> Option<SitePlan>;
}

pub(crate) struct SitePlan {
    pub attempts: Vec<SiteAttempt>,
    /// An icon URL off the rate-limited host, declared so the icon resolver
    /// never fetches the page itself.
    pub icon: Option<&'static str>,
}

pub(crate) struct SiteAttempt {
    pub url: Url,
    pub headers: HeaderMap,
    /// Statuses that move on to the next attempt instead of failing the fetch.
    pub fallback_on: &'static [StatusCode],
    /// `None` hands the response to the ordinary HTML, text, and PDF pipeline.
    pub render: Option<Render>,
}

pub(crate) type Render = fn(&str, &RenderContext) -> Result<SitePage, SiteRenderError>;

pub(crate) struct RenderContext {
    pub final_url: Url,
    pub markdown: bool,
}

pub(crate) struct SitePage {
    pub title: Option<String>,
    pub body: String,
    pub method: &'static str,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SiteRenderError {
    #[error("the {0} response could not be parsed: {1}")]
    Malformed(&'static str, String),
    #[error("the {0} response held nothing to show")]
    Empty(&'static str),
}

impl SiteAttempt {
    pub(crate) fn direct(url: Url, headers: HeaderMap) -> Self {
        Self {
            url,
            headers,
            fallback_on: &[],
            render: None,
        }
    }
}

pub(crate) fn plan(url: &Url, format: WebfetchFormat) -> Option<SitePlan> {
    RULES
        .iter()
        .find_map(|rule| rule.plan(url, format))
        .filter(|plan| !plan.attempts.is_empty())
}

/// The default request headers for `format` with a site's own User-Agent and Accept.
fn site_headers(
    format: WebfetchFormat,
    user_agent: &'static str,
    accept: &'static str,
) -> HeaderMap {
    let mut headers = content::headers(format);
    headers.insert(
        http::header::USER_AGENT,
        HeaderValue::from_static(user_agent),
    );
    headers.insert(http::header::ACCEPT, HeaderValue::from_static(accept));
    headers
}
