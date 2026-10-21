use std::fmt::Display;

use http::{HeaderMap, Method};
use url::Url;
use workcell_net::{PrivateAllowlist, UrlPolicy, UrlPolicyError};

use super::{MAX_PDF_RESPONSE_BYTES, MAX_REDIRECTS, WebfetchError, content};
use crate::WebHttpRequestKind;
use crate::types::{WebfetchFormat, WebfetchInput, WebfetchPdfMode};

const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
const MAX_TIMEOUT_SECONDS: u64 = 60;
const BLOCKED_PREFIX: &str = "URL is blocked by network safety rules: ";

pub(crate) struct NormalizedWebfetchInput {
    pub url: Url,
    /// `url` as the caller wrote it, before any upgrade to HTTPS, so a refusal
    /// names the port the caller asked for.
    pub requested: Url,
    pub format: WebfetchFormat,
    pub pdf_mode: WebfetchPdfMode,
    pub timeout_seconds: u64,
    pub policy: UrlPolicy,
    pub request_kind: WebHttpRequestKind,
    pub method: Method,
    pub headers: HeaderMap,
    pub max_redirects: usize,
    pub max_body_bytes: usize,
    /// Host-set page limit for PDF attachments. `None` attaches any PDF that
    /// fits the response bound; `Some(0)` always extracts text instead.
    pub pdf_attachment_page_limit: Option<usize>,
}

/// Plain HTTP is upgraded to HTTPS, except for a target `private` admits by its
/// name or address: a LAN or tailnet service often serves only HTTP.
pub(crate) fn normalize_input(
    input: WebfetchInput,
    policy: UrlPolicy,
    private: &PrivateAllowlist,
) -> Result<NormalizedWebfetchInput, WebfetchError> {
    if input.url.trim().is_empty() {
        return Err(WebfetchError::InvalidInput(
            "Invalid arguments: url must not be empty".to_owned(),
        ));
    }
    let original = input.url.trim();
    let requested = Url::parse(original)
        .map_err(|_| WebfetchError::InvalidInput(format!("Invalid URL: {original}")))?;
    let mut url = requested.clone();
    if !matches!(url.scheme(), "http" | "https") {
        return Err(WebfetchError::InvalidInput(format!(
            "URL must use http or https: {original}"
        )));
    }
    if url.scheme() == "http" && !private.admits_private(policy, &url) {
        url.set_scheme("https")
            .map_err(|()| WebfetchError::InvalidInput(format!("Invalid URL: {original}")))?;
    }
    private
        .validate_url(policy, &url)
        .map_err(|error| policy_error(error, &requested, false))?;
    let timeout_seconds = input
        .timeout
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
        .min(MAX_TIMEOUT_SECONDS);
    if timeout_seconds == 0 {
        return Err(WebfetchError::InvalidInput(
            "Invalid arguments: timeout must be a positive integer".to_owned(),
        ));
    }
    Ok(NormalizedWebfetchInput {
        url,
        requested,
        format: input.format,
        pdf_mode: input.pdf_mode,
        timeout_seconds,
        policy,
        request_kind: WebHttpRequestKind::PublicGet,
        method: Method::GET,
        headers: content::headers(input.format),
        max_redirects: MAX_REDIRECTS,
        max_body_bytes: MAX_PDF_RESPONSE_BYTES,
        pdf_attachment_page_limit: None,
    })
}

/// `target` is the refused URL, and `redirected` says a redirect led to it.
pub(super) fn policy_error(error: UrlPolicyError, target: &Url, redirected: bool) -> WebfetchError {
    match error {
        UrlPolicyError::UnsupportedScheme => {
            WebfetchError::InvalidInput("URL must use http or https".to_owned())
        }
        UrlPolicyError::InvalidUrl(_) => WebfetchError::InvalidInput("Invalid URL".to_owned()),
        other if other.is_non_public_target() => WebfetchError::PrivateTarget {
            message: blocked(other),
            url: target.clone(),
            redirected,
        },
        other => WebfetchError::Denied(blocked(other)),
    }
}

pub(super) fn blocked(reason: impl Display) -> String {
    format!("{BLOCKED_PREFIX}{reason}")
}
