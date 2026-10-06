mod content;
mod icons;
mod input;
mod output;
mod pdf_response;

use std::time::Duration;

use http::StatusCode;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use workcell_source_icons::SourceIconError;

use crate::WebToolDependencies;
use crate::dependencies::{WebHttpError, WebHttpRequest};
use crate::types::WebfetchOutput;

pub(crate) use input::{NormalizedWebfetchInput, normalize_input};
pub(crate) use output::utf8_prefix;

const MIB: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 5 * MIB;
pub(super) const MAX_PDF_RESPONSE_BYTES: usize = 6 * MIB;
pub(super) const MAX_REDIRECTS: usize = 5;

#[derive(Debug, thiserror::Error)]
pub enum WebfetchError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("Tool invocation was aborted.")]
    Aborted,
    #[error("{0}")]
    TimedOut(String),
    /// Network safety rules or the outbound proxy refused the target.
    #[error("{0}")]
    Denied(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Operation(String),
}

impl WebfetchError {
    /// The symbolic token a caller branches on, so it never has to parse the
    /// message to tell a refused target from a slow or broken one.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidInput(_) => "invalid_input",
            Self::Aborted => "cancelled",
            Self::TimedOut(_) => "timed_out",
            Self::Denied(_) => "web_request_denied",
            Self::NotFound(_) => "not_found",
            Self::Operation(_) => "operation_failed",
        }
    }

    fn timed_out(timeout_seconds: u64) -> Self {
        Self::TimedOut(format!("Request timed out after {timeout_seconds} seconds"))
    }
}

pub(crate) struct WebfetchExecution {
    pub output: WebfetchOutput,
    pub model_text: String,
}

pub(crate) async fn execute(
    input: input::NormalizedWebfetchInput,
    dependencies: &WebToolDependencies,
    cancellation: CancellationToken,
) -> Result<WebfetchExecution, WebfetchError> {
    let timeout = Duration::from_secs(input.timeout_seconds);
    // Webfetch timeout is one total deadline for network transfer and primary
    // HTML/PDF parsing. Optional icon decoration is skipped at that deadline.
    let deadline = Instant::now() + timeout;
    input
        .policy
        .validate_url(&input.url)
        .map_err(input::policy_error)?;
    let response = dependencies
        .http
        .execute(WebHttpRequest {
            kind: input.request_kind,
            method: input.method.clone(),
            url: input.url.clone(),
            headers: input.headers.clone(),
            body: None,
            timeout,
            max_redirects: input.max_redirects,
            // Octet-stream responses need the PDF allowance before their
            // signature can be inspected.
            max_body_bytes: input.max_body_bytes,
            cancellation: cancellation.clone(),
        })
        .await
        .map_err(|error| map_http_error(error, input.timeout_seconds))?;
    input
        .policy
        .validate_url(&response.final_url)
        .map_err(input::policy_error)?;
    if !response.status.is_success() {
        let message = format!(
            "webfetch returned {} {}",
            response.status.as_u16(),
            response.status.canonical_reason().unwrap_or_default()
        );
        return Err(match response.status {
            StatusCode::NOT_FOUND | StatusCode::GONE => WebfetchError::NotFound(message),
            _ => WebfetchError::Operation(message),
        });
    }

    let content_type = content::normalized_content_type(&response.headers);
    // Re-apply the wire bound here so an injected transport cannot bypass the
    // same memory/output invariant enforced by the production net client.
    let bounded_length = response.body.len().min(MAX_PDF_RESPONSE_BYTES);
    let body_was_over_limit = response.body.len() > MAX_PDF_RESPONSE_BYTES;
    let body = response.body.slice(..bounded_length);
    if content::is_pdf(content_type.as_deref())
        || content::should_probe_pdf(content_type.as_deref())
    {
        return pdf_response::execute(
            pdf_response::PdfResponse {
                request: input,
                status: response.status,
                final_url: response.final_url,
                bytes: body.to_vec(),
                body_truncated: response.truncated || body_was_over_limit,
                content_type,
            },
            dependencies,
            cancellation,
            deadline,
        )
        .await;
    }
    if content_type
        .as_deref()
        .is_some_and(|value| !content::is_text_like(value))
    {
        return Err(WebfetchError::Operation(format!(
            "webfetch cannot return non-text content type: {}",
            content_type.as_deref().unwrap_or("unknown")
        )));
    }

    let text_body_truncated = body.len() > MAX_RESPONSE_BYTES;
    let body = content::decode_text(
        &body[..body.len().min(MAX_RESPONSE_BYTES)],
        &response.headers,
        content::is_html(content_type.as_deref()),
    );
    let formatted = content::format(
        body.clone(),
        input.format,
        content_type.as_deref(),
        response.final_url.as_str(),
        cancellation.clone(),
        deadline,
        input.timeout_seconds,
    )
    .await?;
    let source_cut = (response.truncated || text_body_truncated)
        .then(|| format!("the first {} MiB of the response", MAX_RESPONSE_BYTES / MIB));
    let bounded = output::truncate_model_output(&formatted.output, source_cut.as_deref());
    let summary_input = formatted
        .summary_input
        .as_deref()
        .map(output::truncate_summary_input);
    let icon = icons::resolve(
        dependencies,
        response.final_url.as_str(),
        content::is_html(content_type.as_deref()).then_some(body),
        cancellation,
        deadline,
    )
    .await
    .or_else(|error| match error {
        SourceIconError::Cancelled => Err(WebfetchError::Aborted),
        _ => Ok(None),
    })?;
    let output = WebfetchOutput {
        kind: "webfetch",
        url: input.url.to_string(),
        final_url: Some(response.final_url.to_string()),
        content_type,
        format: input.format,
        pdf_mode: None,
        status: response.status.as_u16(),
        title: formatted.title,
        output: bounded.text.clone(),
        summary_input,
        truncated: response.truncated
            || text_body_truncated
            || formatted.truncated
            || bounded.truncated,
        pdf_attachment: None,
        page_count: None,
        pdf_fallback_reason: None,
        extraction_method: formatted.extraction_method,
        extraction_low_signal: formatted.extraction_low_signal,
        icon_url: icon.as_ref().map(|value| value.icon_url.clone()),
        icon_data_url: icon.as_ref().map(|value| value.icon_data_url.clone()),
    };
    Ok(WebfetchExecution {
        model_text: bounded.text,
        output,
    })
}

fn map_http_error(error: WebHttpError, timeout: u64) -> WebfetchError {
    match error {
        WebHttpError::Cancelled => WebfetchError::Aborted,
        WebHttpError::Timeout => WebfetchError::timed_out(timeout),
        WebHttpError::Rejected(message) => {
            WebfetchError::Denied(format!("URL is blocked by network safety rules: {message}"))
        }
        WebHttpError::ProxyRejected => WebfetchError::Denied(
            "The outbound proxy refused this request; the target is not permitted.".to_owned(),
        ),
        WebHttpError::RedirectRejected | WebHttpError::RequestFailed => {
            WebfetchError::Operation("webfetch request failed.".to_owned())
        }
    }
}
