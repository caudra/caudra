use crate::WebsearchBackend;
use crate::types::{WebsearchFallback, WebsearchOutput, WebsearchResult};

const MAX_MODEL_OUTPUT_BYTES: usize = 50 * 1024;

pub(super) fn success(
    backend: WebsearchBackend,
    query: String,
    results_found: usize,
    results: Vec<WebsearchResult>,
) -> WebsearchOutput {
    let formatted_results = format_results(&results);
    WebsearchOutput {
        kind: "websearch",
        backend: Some(backend),
        query,
        results_found,
        results,
        formatted_results,
        error: None,
        error_message: None,
        fallbacks: Vec::new(),
    }
}

pub(super) fn error(
    backend: Option<WebsearchBackend>,
    query: String,
    message: &str,
) -> WebsearchOutput {
    // Provider/configuration errors are successful tool calls so the model can
    // explain or recover, matching the TypeScript authority.
    WebsearchOutput {
        kind: "websearch",
        backend,
        query,
        results_found: 0,
        results: Vec::new(),
        formatted_results: format!("Error: {message}"),
        error: Some(true),
        error_message: Some(message.to_owned()),
        fallbacks: Vec::new(),
    }
}

/// Name the backends that failed before `output.backend` answered, in the
/// structured output and ahead of the model-facing results.
pub(super) fn with_fallbacks(
    mut output: WebsearchOutput,
    fallbacks: Vec<WebsearchFallback>,
) -> WebsearchOutput {
    if fallbacks.is_empty() {
        return output;
    }
    let answered = output.backend.map_or("", WebsearchBackend::as_str);
    let notes = fallbacks
        .iter()
        .map(|fallback| {
            format!(
                "Note: {} failed ({}); results from {answered}.\n",
                fallback.backend.as_str(),
                fallback.error
            )
        })
        .collect::<String>();
    output.formatted_results.insert_str(0, &notes);
    output.fallbacks = fallbacks;
    output
}

/// A lone failure keeps the single-backend error shape; several are reported together.
pub(super) fn all_failed(
    primary: Option<WebsearchBackend>,
    query: String,
    fallbacks: Vec<WebsearchFallback>,
) -> WebsearchOutput {
    if let [fallback] = fallbacks.as_slice() {
        return error(Some(fallback.backend), query, &fallback.error);
    }
    let failures = fallbacks
        .iter()
        .map(|fallback| format!("{}: {}", fallback.backend.as_str(), fallback.error))
        .collect::<Vec<_>>()
        .join("; ");
    let mut output = error(
        primary,
        query,
        &format!("Every websearch backend failed. {failures}"),
    );
    output.fallbacks = fallbacks;
    output
}

fn format_results(results: &[WebsearchResult]) -> String {
    if results.is_empty() {
        return "No results found for the given query.".to_owned();
    }
    let output = results
        .iter()
        .enumerate()
        .map(|(index, result)| {
            let engine = result
                .engine
                .as_deref()
                .map_or_else(String::new, |engine| format!(" [{engine}]"));
            format!(
                "{}. {}{}\n   URL: {}\n   {}",
                index + 1,
                result.title,
                engine,
                result.url,
                result.snippet
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    utf8_prefix(&output, MAX_MODEL_OUTPUT_BYTES).to_owned()
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &value[..boundary]
}
