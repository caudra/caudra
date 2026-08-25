use std::sync::Arc;

use base64::Engine;
use http::StatusCode;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;
use workcell_source_icons::SourceIconError;

use super::icons;
use super::input::NormalizedWebfetchInput;
use super::output::{filename_from_url, normalize_pdf_text, truncate_model_output};
use super::{WebfetchError, WebfetchExecution};
use crate::WebToolDependencies;
use crate::blocking::{self, BlockingError};
use crate::types::{WebfetchOutput, WebfetchPdfAttachment, WebfetchPdfMode};

pub(super) struct PdfResponse {
    pub request: NormalizedWebfetchInput,
    pub status: StatusCode,
    pub final_url: Url,
    pub bytes: Vec<u8>,
    pub body_truncated: bool,
    pub content_type: Option<String>,
}

pub(super) async fn execute(
    response: PdfResponse,
    dependencies: &WebToolDependencies,
    cancellation: CancellationToken,
    deadline: Instant,
) -> Result<WebfetchExecution, WebfetchError> {
    if !response.bytes.starts_with(b"%PDF-") {
        return Err(WebfetchError::Operation(format!(
            "webfetch cannot return non-text content type: {}",
            response.content_type.as_deref().unwrap_or("unknown")
        )));
    }
    if response.request.pdf_mode == WebfetchPdfMode::Attachment {
        if response.body_truncated {
            return Err(WebfetchError::Operation(
                "PDF response exceeded the attachment size limit.".to_owned(),
            ));
        }
        return attachment(response, dependencies, cancellation, deadline).await;
    }

    let timeout_seconds = response.request.timeout_seconds;
    let extractor = Arc::clone(&dependencies.pdf);
    let (extracted, response) = blocking::run_until(deadline, &cancellation, move || {
        (extractor.extract(&response.bytes), response)
    })
    .await
    .map_err(|error| blocking_error(error, timeout_seconds))?;
    let extracted = extracted.map_err(|_| parse_error())?;
    let formatted = normalize_pdf_text(&extracted.text);
    let bounded = truncate_model_output(&formatted);
    let icon = icons::resolve(
        dependencies,
        response.final_url.as_str(),
        None,
        cancellation.clone(),
        deadline,
    )
    .await
    .or_else(icon_error)?;
    if cancellation.is_cancelled() {
        return Err(WebfetchError::Aborted);
    }
    Ok(WebfetchExecution {
        output: WebfetchOutput {
            kind: "webfetch",
            url: response.request.url.to_string(),
            final_url: Some(response.final_url.to_string()),
            content_type: Some("application/pdf".to_owned()),
            format: response.request.format,
            pdf_mode: Some(WebfetchPdfMode::Extract),
            status: response.status.as_u16(),
            title: extracted.title,
            output: bounded.text.clone(),
            summary_input: None,
            truncated: response.body_truncated || extracted.truncated || bounded.truncated,
            pdf_attachment: None,
            extraction_method: None,
            extraction_low_signal: None,
            icon_url: icon.as_ref().map(|value| value.icon_url.clone()),
            icon_data_url: icon.as_ref().map(|value| value.icon_data_url.clone()),
        },
        model_text: bounded.text,
    })
}

async fn attachment(
    response: PdfResponse,
    dependencies: &WebToolDependencies,
    cancellation: CancellationToken,
    deadline: Instant,
) -> Result<WebfetchExecution, WebfetchError> {
    let filename =
        filename_from_url(&response.final_url).or_else(|| filename_from_url(&response.request.url));
    let attachment = WebfetchPdfAttachment {
        attachment_type: "file",
        mime: "application/pdf",
        url: format!(
            "data:application/pdf;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&response.bytes)
        ),
        filename: filename.clone(),
        size_bytes: response.bytes.len(),
    };
    let model_text =
        "PDF fetched successfully. The PDF is available as an application/pdf attachment."
            .to_owned();
    let icon = icons::resolve(
        dependencies,
        response.final_url.as_str(),
        None,
        cancellation.clone(),
        deadline,
    )
    .await
    .or_else(icon_error)?;
    if cancellation.is_cancelled() {
        return Err(WebfetchError::Aborted);
    }
    Ok(WebfetchExecution {
        output: WebfetchOutput {
            kind: "webfetch",
            url: response.request.url.to_string(),
            final_url: Some(response.final_url.to_string()),
            content_type: Some("application/pdf".to_owned()),
            format: response.request.format,
            pdf_mode: Some(WebfetchPdfMode::Attachment),
            status: response.status.as_u16(),
            title: Some(filename.unwrap_or_else(|| "Fetched PDF".to_owned())),
            output: model_text.clone(),
            summary_input: None,
            // Truncated bodies are rejected before attachment construction, so
            // every emitted data URL contains the complete bounded response.
            truncated: false,
            pdf_attachment: Some(attachment),
            extraction_method: None,
            extraction_low_signal: None,
            icon_url: icon.as_ref().map(|value| value.icon_url.clone()),
            icon_data_url: icon.as_ref().map(|value| value.icon_data_url.clone()),
        },
        model_text,
    })
}

fn parse_error() -> WebfetchError {
    WebfetchError::Operation("Failed to parse PDF content.".to_owned())
}

fn blocking_error(error: BlockingError, timeout_seconds: u64) -> WebfetchError {
    match error {
        BlockingError::Cancelled => WebfetchError::Aborted,
        BlockingError::TimedOut => {
            WebfetchError::Operation(format!("Request timed out after {timeout_seconds} seconds"))
        }
        BlockingError::Panicked => parse_error(),
    }
}

fn icon_error(
    error: SourceIconError,
) -> Result<Option<workcell_source_icons::ResolvedSourceIcon>, WebfetchError> {
    match error {
        SourceIconError::Cancelled => Err(WebfetchError::Aborted),
        _ => Ok(None),
    }
}
