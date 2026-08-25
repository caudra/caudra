use crate::WebsearchBackend;
use crate::blocking::BlockingError;
use crate::dependencies::WebHttpError;
use crate::types::{WebsearchInput, WebsearchResult};

pub(super) const USER_AGENT: &str = "Workcell-ToolRuntime/0.1";
pub(super) const MAX_SNIPPET_CHARS: usize = 320;
pub(super) const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
pub(super) const MAX_NORMALIZED_ROWS: usize = 100;

const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 25;
const DEFAULT_TIMEOUT_SECONDS: u64 = 10;
const MAX_TIMEOUT_SECONDS: u64 = 60;

pub(crate) struct ProviderResults {
    pub backend: WebsearchBackend,
    pub query: String,
    pub results_found: usize,
    pub results: Vec<WebsearchResult>,
    pub limit: usize,
}

pub(crate) enum ProviderError {
    Cancelled,
    Message(String),
}

pub(super) fn limit(input: &WebsearchInput) -> usize {
    input
        .limit
        .unwrap_or(DEFAULT_LIMIT as u64)
        .min(MAX_LIMIT as u64) as usize
}

pub(super) fn timeout(input: &WebsearchInput) -> u64 {
    input
        .timeout_sec
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
        .min(MAX_TIMEOUT_SECONDS)
}

pub(super) fn safe_transport_error(error: WebHttpError, timeout: u64) -> String {
    // Never serialize provider/connector error details; implementations may
    // include request headers. Only explicitly allowlisted text reaches output.
    match error {
        WebHttpError::Timeout => format!("Request timed out after {timeout} seconds"),
        WebHttpError::Cancelled
        | WebHttpError::Rejected(_)
        | WebHttpError::RedirectRejected
        | WebHttpError::RequestFailed => "Search backend request failed.".to_owned(),
    }
}

pub(super) fn transport_error(error: WebHttpError, timeout: u64) -> ProviderError {
    if matches!(error, WebHttpError::Cancelled) {
        ProviderError::Cancelled
    } else {
        ProviderError::Message(safe_transport_error(error, timeout))
    }
}

pub(super) fn blocking_error(
    error: BlockingError,
    timeout: u64,
    invalid_response: &'static str,
) -> ProviderError {
    match error {
        BlockingError::Cancelled => ProviderError::Cancelled,
        BlockingError::TimedOut => {
            ProviderError::Message(format!("Request timed out after {timeout} seconds"))
        }
        BlockingError::Panicked => ProviderError::Message(invalid_response.to_owned()),
    }
}
