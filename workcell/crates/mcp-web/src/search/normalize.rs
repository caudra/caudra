use std::collections::HashSet;

use base64::Engine;
use serde_json::Value;
use url::Url;

use super::common::MAX_SNIPPET_CHARS;
use crate::types::WebsearchResult;

pub(super) const MAX_QUERY_CHARS: usize = 512;
const MAX_TITLE_CHARS: usize = 300;
const MAX_RESULT_URL_BYTES: usize = 4 * 1024;
const MAX_ENGINE_CHARS: usize = 64;

pub(super) fn normalized_basic_result(
    raw_title: &str,
    raw_url: &str,
    raw_snippet: &str,
    engine: Option<String>,
    seen: &mut HashSet<String>,
) -> Option<WebsearchResult> {
    let url = normalize_http_url(raw_url)?;
    let title = collapse_whitespace_bounded(raw_title, MAX_TITLE_CHARS);
    if title.is_empty() || !seen.insert(url.clone()) {
        return None;
    }
    Some(WebsearchResult {
        title,
        url,
        snippet: collapse_whitespace_bounded(raw_snippet, MAX_SNIPPET_CHARS),
        engine,
        icon_url: None,
        icon_data_url: None,
    })
}

fn normalize_http_url(value: &str) -> Option<String> {
    if value.len() > MAX_RESULT_URL_BYTES {
        return None;
    }
    let url = Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let normalized = url.to_string();
    (normalized.len() <= MAX_RESULT_URL_BYTES).then_some(normalized)
}

pub(super) fn collapse_whitespace_bounded(value: &str, limit: usize) -> String {
    let mut output = String::new();
    let mut used = 0;
    push_words_bounded(&mut output, &mut used, value, limit);
    output
}

pub(super) fn first_engine(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(value)) => bounded_nonempty(value, MAX_ENGINE_CHARS),
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .find_map(|value| bounded_nonempty(value, MAX_ENGINE_CHARS)),
        _ => None,
    }
}

fn bounded_nonempty(value: &str, limit: usize) -> Option<String> {
    let value = collapse_whitespace_bounded(value, limit);
    (!value.is_empty()).then_some(value)
}

pub(super) fn bounded_highlights(value: Option<&Value>) -> String {
    let mut output = String::new();
    let mut used = 0;
    for highlight in value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        push_words_bounded(&mut output, &mut used, highlight, MAX_SNIPPET_CHARS);
        if used >= MAX_SNIPPET_CHARS {
            break;
        }
    }
    output
}

fn push_words_bounded(output: &mut String, used: &mut usize, value: &str, limit: usize) {
    for word in value.split_whitespace() {
        if !output.is_empty() {
            if used.saturating_add(1) >= limit {
                break;
            }
            output.push(' ');
            *used += 1;
        }
        for character in word.chars() {
            if *used >= limit {
                return;
            }
            output.push(character);
            *used += 1;
        }
    }
}

pub(super) fn safe_png_data_url(value: &str) -> bool {
    const PREFIX: &str = "data:image/png;base64,";
    if value.len() > 16_384 {
        return false;
    }
    let Some(encoded) = value.get(PREFIX.len()..).filter(|_| {
        value
            .get(..PREFIX.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
    }) else {
        return false;
    };
    let Ok(bytes) = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(encoded))
    else {
        return false;
    };
    if bytes.len() < 24 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return false;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("four width bytes"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("four height bytes"));
    width > 0 && height > 0 && width <= 256 && height <= 256
}
