use http::{HeaderMap, HeaderValue};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::WebfetchError;
use crate::blocking::{self, BlockingError};
use crate::html::{add_title_context, extract_html_for_prompt};
use crate::types::WebfetchFormat;

const USER_AGENT: &str = "Workcell-ToolRuntime/0.1";

pub(super) struct FormattedContent {
    pub output: String,
    pub summary_input: Option<String>,
    pub title: Option<String>,
    pub truncated: bool,
    pub extraction_method: Option<&'static str>,
    pub extraction_low_signal: Option<bool>,
}

pub(super) async fn format(
    body: String,
    format: WebfetchFormat,
    content_type: Option<&str>,
    base_url: &str,
    cancellation: CancellationToken,
    deadline: Instant,
    timeout_seconds: u64,
) -> Result<FormattedContent, WebfetchError> {
    if cancellation.is_cancelled() {
        return Err(WebfetchError::Aborted);
    }
    if Instant::now() >= deadline {
        return Err(timeout_error(timeout_seconds));
    }
    if !is_html(content_type) {
        return Ok(FormattedContent {
            output: body,
            summary_input: None,
            title: None,
            truncated: false,
            extraction_method: None,
            extraction_low_signal: None,
        });
    }
    let markdown_base_url = base_url.to_owned();
    let text_base_url = base_url.to_owned();
    let worker_body = body.clone();
    let parsed = blocking::run_until(deadline, &cancellation, move || {
        extract_html_for_prompt(&worker_body, true, &markdown_base_url)
    })
    .await
    .map_err(|error| blocking_error(error, timeout_seconds))?;
    let markdown_extracted = parsed;
    let title = markdown_extracted.title.clone();
    if format == WebfetchFormat::Html {
        let summary_input = add_title_context(&markdown_extracted.output, title.as_deref(), true);
        return Ok(FormattedContent {
            output: body,
            summary_input: Some(summary_input),
            title,
            truncated: false,
            extraction_method: Some(markdown_extracted.method),
            extraction_low_signal: Some(markdown_extracted.low_signal),
        });
    }
    let extracted = if format == WebfetchFormat::Markdown {
        markdown_extracted
    } else {
        blocking::run_until(deadline, &cancellation, move || {
            extract_html_for_prompt(&body, false, &text_base_url)
        })
        .await
        .map_err(|error| blocking_error(error, timeout_seconds))?
    };
    let title = extracted.title.clone().or(title);
    let output = add_title_context(
        &extracted.output,
        title.as_deref(),
        format == WebfetchFormat::Markdown,
    );
    Ok(FormattedContent {
        summary_input: None,
        output,
        title,
        truncated: false,
        extraction_method: Some(extracted.method),
        extraction_low_signal: Some(extracted.low_signal),
    })
}

pub(super) fn headers(format: WebfetchFormat) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::USER_AGENT,
        HeaderValue::from_static(USER_AGENT),
    );
    headers.insert(
        http::header::ACCEPT,
        HeaderValue::from_static(if format == WebfetchFormat::Html {
            "text/html,application/xhtml+xml,text/plain,application/pdf;q=0.9,*/*;q=0.1"
        } else {
            "text/markdown,text/html,application/xhtml+xml,text/plain,application/pdf;q=0.9,*/*;q=0.1"
        }),
    );
    headers
}

pub(super) fn normalized_content_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
}

pub(super) fn is_text_like(value: &str) -> bool {
    value.starts_with("text/")
        || value.contains("json")
        || value.contains("xml")
        || value.contains("javascript")
        || value.contains("xhtml")
}

pub(super) fn is_html(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.contains("html") || value.contains("xhtml"))
}

pub(super) fn is_pdf(value: Option<&str>) -> bool {
    matches!(value, Some("application/pdf" | "application/x-pdf"))
}

pub(super) fn should_probe_pdf(value: Option<&str>) -> bool {
    matches!(
        value,
        Some("application/octet-stream" | "binary/octet-stream" | "application/download")
    )
}

fn parse_error() -> WebfetchError {
    WebfetchError::Operation("Failed to parse HTML content.".to_owned())
}

fn timeout_error(timeout_seconds: u64) -> WebfetchError {
    WebfetchError::Operation(format!("Request timed out after {timeout_seconds} seconds"))
}

fn blocking_error(error: BlockingError, timeout_seconds: u64) -> WebfetchError {
    match error {
        BlockingError::Cancelled => WebfetchError::Aborted,
        BlockingError::TimedOut => timeout_error(timeout_seconds),
        BlockingError::Panicked => parse_error(),
    }
}
