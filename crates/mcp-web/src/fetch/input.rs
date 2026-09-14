use http::{HeaderMap, Method};
use url::Url;
use workcell_net::{UrlPolicy, UrlPolicyError};

use super::{MAX_PDF_RESPONSE_BYTES, MAX_REDIRECTS, WebfetchError, content};
use crate::WebHttpRequestKind;
use crate::types::{WebfetchFormat, WebfetchInput, WebfetchPdfMode};

const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
const MAX_TIMEOUT_SECONDS: u64 = 60;

pub(crate) struct NormalizedWebfetchInput {
    pub url: Url,
    pub format: WebfetchFormat,
    pub pdf_mode: WebfetchPdfMode,
    pub timeout_seconds: u64,
    pub policy: UrlPolicy,
    pub request_kind: WebHttpRequestKind,
    pub method: Method,
    pub headers: HeaderMap,
    pub max_redirects: usize,
    pub max_body_bytes: usize,
}

pub(crate) fn normalize_input(
    input: WebfetchInput,
    policy: UrlPolicy,
) -> Result<NormalizedWebfetchInput, WebfetchError> {
    if input.url.trim().is_empty() {
        return Err(WebfetchError::InvalidInput(
            "Invalid arguments: url must not be empty".to_owned(),
        ));
    }
    let original = input.url.trim();
    let mut url = Url::parse(original)
        .map_err(|_| WebfetchError::InvalidInput(format!("Invalid URL: {original}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(WebfetchError::InvalidInput(format!(
            "URL must use http or https: {original}"
        )));
    }
    if url.scheme() == "http" {
        url.set_scheme("https")
            .map_err(|()| WebfetchError::InvalidInput(format!("Invalid URL: {original}")))?;
    }
    policy
        .validate_url(&url)
        .map_err(|error| WebfetchError::InvalidInput(policy_message(error)))?;
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
        format: input.format,
        pdf_mode: input.pdf_mode,
        timeout_seconds,
        policy,
        request_kind: WebHttpRequestKind::PublicGet,
        method: Method::GET,
        headers: content::headers(input.format),
        max_redirects: MAX_REDIRECTS,
        max_body_bytes: MAX_PDF_RESPONSE_BYTES,
    })
}

pub(super) fn policy_message(error: UrlPolicyError) -> String {
    match error {
        UrlPolicyError::UnsupportedScheme => "URL must use http or https".to_owned(),
        UrlPolicyError::InvalidUrl(_) => "Invalid URL".to_owned(),
        other => format!("URL is blocked by network safety rules: {other}"),
    }
}
