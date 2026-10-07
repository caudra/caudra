use std::sync::LazyLock;

use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE};
use http::{HeaderMap, HeaderValue};
use regex::bytes::Regex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::WebfetchError;
use crate::blocking::{self, BlockingError};
use crate::html::{add_title_context, extract_html_for_prompt};
use crate::types::WebfetchFormat;

const USER_AGENT: &str = "Workcell-ToolRuntime/0.1";
/// How far into an HTML document a `<meta>` charset declaration is honored,
/// matching the prescan length browsers use.
const META_PRESCAN_BYTES: usize = 1024;

static META_CHARSET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i-u)<meta[^>]*?charset\s*=\s*["']?\s*([a-z0-9_:.\-]+)"#)
        .expect("meta charset regex")
});

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
        return Err(WebfetchError::timed_out(timeout_seconds));
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

/// Decodes a text body. A byte-order mark wins, then the Content-Type charset,
/// then an HTML `<meta>` declaration, then UTF-8. Malformed sequences become
/// U+FFFD rather than failing the fetch.
pub(super) fn decode_text(bytes: &[u8], headers: &HeaderMap, html: bool) -> String {
    let declared = header_charset(headers).or_else(|| html.then(|| meta_charset(bytes)).flatten());
    let (text, _, _) = declared.unwrap_or(UTF_8).decode(bytes);
    text.into_owned()
}

fn header_charset(headers: &HeaderMap) -> Option<&'static Encoding> {
    let value = headers.get(http::header::CONTENT_TYPE)?.to_str().ok()?;
    value.split(';').skip(1).find_map(|parameter| {
        let (name, label) = parameter.split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("charset") {
            return None;
        }
        Encoding::for_label(label.trim().trim_matches('"').as_bytes())
    })
}

fn meta_charset(bytes: &[u8]) -> Option<&'static Encoding> {
    let prefix = &bytes[..bytes.len().min(META_PRESCAN_BYTES)];
    let label = META_CHARSET.captures(prefix)?.get(1)?.as_bytes();
    let encoding = Encoding::for_label(label)?;
    // A document that declares its encoding in ASCII bytes cannot be UTF-16,
    // so browsers read such a declaration as UTF-8.
    Some(if encoding == UTF_16BE || encoding == UTF_16LE {
        UTF_8
    } else {
        encoding
    })
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

fn blocking_error(error: BlockingError, timeout_seconds: u64) -> WebfetchError {
    match error {
        BlockingError::Cancelled => WebfetchError::Aborted,
        BlockingError::TimedOut => WebfetchError::timed_out(timeout_seconds),
        BlockingError::Panicked => parse_error(),
    }
}

#[cfg(test)]
mod tests {
    use encoding_rs::{SHIFT_JIS, WINDOWS_1251};
    use test_case::test_case;

    use super::*;

    const CYRILLIC: &str = "Привет, мир";
    const JAPANESE: &str = "日本語の本文";
    const UMLAUTS: &str = "Grüße";
    const UTF8_BYTE_ORDER_MARK: &[u8] = b"\xEF\xBB\xBF";

    fn content_type(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_str(value).expect("content type"),
        );
        headers
    }

    #[test]
    fn a_content_type_charset_decodes_the_body() {
        let (bytes, _, _) = WINDOWS_1251.encode(CYRILLIC);
        let headers = content_type("text/plain; charset=windows-1251");
        assert_eq!(decode_text(&bytes, &headers, false), CYRILLIC);
    }

    #[test_case(r#"<meta charset="Shift_JIS">"# ; "a meta charset attribute")]
    #[test_case(r#"<meta http-equiv="Content-Type" content="text/html; charset=Shift_JIS">"# ; "a meta http-equiv declaration")]
    fn a_meta_declaration_decodes_an_html_body(declaration: &str) {
        let page = format!("<html><head>{declaration}</head><body>{JAPANESE}</body></html>");
        let (bytes, _, _) = SHIFT_JIS.encode(&page);
        assert_eq!(decode_text(&bytes, &content_type("text/html"), true), page);
    }

    #[test]
    fn a_content_type_charset_outranks_a_meta_declaration() {
        let page = format!(r#"<meta charset="utf-8"><p>{CYRILLIC}</p>"#);
        let (bytes, _, _) = WINDOWS_1251.encode(&page);
        let headers = content_type("text/html; charset=windows-1251");
        assert_eq!(decode_text(&bytes, &headers, true), page);
    }

    #[test]
    fn a_byte_order_mark_outranks_the_content_type_charset() {
        let bytes = [UTF8_BYTE_ORDER_MARK, UMLAUTS.as_bytes()].concat();
        let headers = content_type("text/plain; charset=windows-1252");
        assert_eq!(decode_text(&bytes, &headers, false), UMLAUTS);
    }

    #[test]
    fn an_unknown_charset_label_falls_back_to_utf8() {
        let headers = content_type("text/plain; charset=no-such-charset");
        assert_eq!(decode_text(UMLAUTS.as_bytes(), &headers, false), UMLAUTS);
    }

    #[test]
    fn a_meta_declaration_in_plain_text_is_only_text() {
        let body = format!(r#"<meta charset="windows-1251"> {UMLAUTS}"#);
        assert_eq!(
            decode_text(body.as_bytes(), &content_type("text/plain"), false),
            body
        );
    }
}
