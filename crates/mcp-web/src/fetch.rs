mod content;
mod icons;
mod input;
mod output;
mod pdf_response;

use std::time::Duration;

use http::Method;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use workcell_source_icons::SourceIconError;

use crate::WebToolDependencies;
use crate::dependencies::{WebHttpError, WebHttpRequest, WebHttpRequestKind};
use crate::types::WebfetchOutput;

pub(crate) use input::{NormalizedWebfetchInput, normalize_input};
pub(crate) use output::utf8_prefix;

const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
const MAX_PDF_RESPONSE_BYTES: usize = 6 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum WebfetchError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("Tool invocation was aborted.")]
    Aborted,
    #[error("{0}")]
    Operation(String),
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
    let response = dependencies
        .http
        .execute(WebHttpRequest {
            kind: WebHttpRequestKind::PublicGet,
            method: Method::GET,
            url: input.url.clone(),
            headers: content::headers(input.format),
            body: None,
            timeout,
            max_redirects: 5,
            // Octet-stream responses need the PDF allowance before their
            // signature can be inspected.
            max_body_bytes: MAX_PDF_RESPONSE_BYTES,
            cancellation: cancellation.clone(),
        })
        .await
        .map_err(|error| map_http_error(error, input.timeout_seconds))?;
    dependencies
        .webfetch_policy
        .validate_url(&response.final_url)
        .map_err(|error| WebfetchError::Operation(input::policy_message(error)))?;
    if !response.status.is_success() {
        return Err(WebfetchError::Operation(format!(
            "webfetch returned {} {}",
            response.status.as_u16(),
            response.status.canonical_reason().unwrap_or_default()
        )));
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
    let body = String::from_utf8_lossy(&body[..body.len().min(MAX_RESPONSE_BYTES)]).into_owned();
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
    let bounded = output::truncate_model_output(&formatted.output);
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
        WebHttpError::Timeout => {
            WebfetchError::Operation(format!("Request timed out after {timeout} seconds"))
        }
        WebHttpError::Rejected(message) => {
            WebfetchError::Operation(format!("URL is blocked by network safety rules: {message}"))
        }
        WebHttpError::RedirectRejected | WebHttpError::RequestFailed => {
            WebfetchError::Operation("webfetch request failed.".to_owned())
        }
    }
}
