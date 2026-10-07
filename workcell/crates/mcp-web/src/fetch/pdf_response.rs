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
use super::{MAX_PDF_RESPONSE_BYTES, MIB, WebfetchError, WebfetchExecution};
use crate::WebToolDependencies;
use crate::blocking::{self, BlockingError};
use crate::pdf::MAX_EXTRACTED_TEXT_BYTES;
use crate::types::{WebfetchOutput, WebfetchPdfAttachment, WebfetchPdfMode};

const ATTACHMENTS_DISABLED: &str = "PDF attachments are disabled";

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
    if response.request.pdf_mode == WebfetchPdfMode::Extract {
        return extract(response, None, dependencies, cancellation, deadline).await;
    }
    let page_limit = response.request.pdf_attachment_page_limit;
    if page_limit == Some(0) {
        let reason = ATTACHMENTS_DISABLED.to_owned();
        return extract(response, Some(reason), dependencies, cancellation, deadline).await;
    }
    if response.body_truncated {
        return Err(WebfetchError::Operation(
            "PDF response exceeded the attachment size limit.".to_owned(),
        ));
    }
    let Some(page_limit) = page_limit else {
        return attachment(response, None, dependencies, cancellation, deadline).await;
    };
    let timeout_seconds = response.request.timeout_seconds;
    let extractor = Arc::clone(&dependencies.pdf);
    let (page_count, response) = blocking::run_until(deadline, &cancellation, move || {
        (extractor.page_count(&response.bytes), response)
    })
    .await
    .map_err(|error| blocking_error(error, timeout_seconds))?;
    let page_count = page_count.map_err(|_| parse_error())?;
    if page_count > page_limit {
        let reason = format!(
            "the PDF has {}, above the {page_limit}-page attachment limit",
            pages(page_count)
        );
        return extract(response, Some(reason), dependencies, cancellation, deadline).await;
    }
    attachment(
        response,
        Some(page_count),
        dependencies,
        cancellation,
        deadline,
    )
    .await
}

/// Extracts the PDF text. `fallback` names why a requested attachment was
/// replaced by text, and leads the model text so the model knows it never
/// received the file.
async fn extract(
    response: PdfResponse,
    fallback: Option<String>,
    dependencies: &WebToolDependencies,
    cancellation: CancellationToken,
    deadline: Instant,
) -> Result<WebfetchExecution, WebfetchError> {
    let timeout_seconds = response.request.timeout_seconds;
    let extractor = Arc::clone(&dependencies.pdf);
    let (extracted, response) = blocking::run_until(deadline, &cancellation, move || {
        (extractor.extract(&response.bytes), response)
    })
    .await
    .map_err(|error| blocking_error(error, timeout_seconds))?;
    let extracted = extracted.map_err(|_| match &fallback {
        Some(reason) => WebfetchError::Operation(format!(
            "Not attached because {reason}, and the PDF text could not be extracted."
        )),
        None => parse_error(),
    })?;
    let text = normalize_pdf_text(&extracted.text);
    let text = match &fallback {
        Some(reason) => {
            format!("[Not attached because {reason}. The extracted text follows.]\n\n{text}")
        }
        None => text,
    };
    let source_cut = if response.body_truncated {
        Some(format!(
            "the first {} MiB of the PDF",
            MAX_PDF_RESPONSE_BYTES / MIB
        ))
    } else if extracted.truncated {
        Some(format!(
            "the first {} MiB of the PDF text",
            MAX_EXTRACTED_TEXT_BYTES / MIB
        ))
    } else {
        None
    };
    let bounded = truncate_model_output(&text, source_cut.as_deref());
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
            truncated: bounded.truncated,
            pdf_attachment: None,
            page_count: Some(extracted.page_count),
            pdf_fallback_reason: fallback,
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
    page_count: Option<usize>,
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
    let model_text = match page_count {
        Some(count) => format!(
            "PDF fetched successfully. The PDF has {} and is available as an application/pdf attachment.",
            pages(count)
        ),
        None => "PDF fetched successfully. The PDF is available as an application/pdf attachment."
            .to_owned(),
    };
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
            page_count,
            pdf_fallback_reason: None,
            extraction_method: None,
            extraction_low_signal: None,
            icon_url: icon.as_ref().map(|value| value.icon_url.clone()),
            icon_data_url: icon.as_ref().map(|value| value.icon_data_url.clone()),
        },
        model_text,
    })
}

fn pages(count: usize) -> String {
    if count == 1 {
        "1 page".to_owned()
    } else {
        format!("{count} pages")
    }
}

fn parse_error() -> WebfetchError {
    WebfetchError::Operation("Failed to parse PDF content.".to_owned())
}

fn blocking_error(error: BlockingError, timeout_seconds: u64) -> WebfetchError {
    match error {
        BlockingError::Cancelled => WebfetchError::Aborted,
        BlockingError::TimedOut => WebfetchError::timed_out(timeout_seconds),
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
