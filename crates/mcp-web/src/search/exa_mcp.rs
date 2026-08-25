use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use http::{HeaderMap, HeaderValue, Method};
use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::common::{
    MAX_NORMALIZED_ROWS, ProviderError, ProviderResults, USER_AGENT, blocking_error, limit,
    timeout, transport_error,
};
use super::normalize::{MAX_QUERY_CHARS, collapse_whitespace_bounded, normalized_basic_result};
use super::provider::{ProviderCatalogContract, WebsearchProvider};
use crate::blocking;
use crate::dependencies::{WebHttpRequest, WebHttpRequestKind};
use crate::types::WebsearchInput;
use crate::{WebToolDependencies, WebsearchBackend, WebsearchResult};

const ENDPOINT: &str = "https://mcp.exa.ai/mcp";
const TOOL_NAME: &str = "web_search_exa";
const MAX_MCP_RESPONSE_BYTES: usize = 512 * 1024;

pub(crate) struct ExaMcpProvider;

impl fmt::Debug for ExaMcpProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExaMcpProvider")
    }
}

#[async_trait]
impl WebsearchProvider for ExaMcpProvider {
    fn backend(&self) -> WebsearchBackend {
        WebsearchBackend::ExaMcp
    }

    fn catalog_contract(&self, current_year: i32) -> ProviderCatalogContract {
        ProviderCatalogContract {
            description: format!(
                "Search the web using Exa's credential-free hosted MCP service.\n\n- Search queries are sent to the third-party service at mcp.exa.ai.\n- Returns normalized titles, URLs, highlighted snippets, and optional source icon metadata.\n- Use semantically rich descriptions of the ideal page rather than short keyword lists.\n- Add category:company, category:people, category:news, category:research paper, or category:personal site to the query when useful.\n- Prefer precise queries with the current year for recent information.\n- Use webfetch after search identifies a page that needs detailed analysis.\n\nThe current year is {current_year}."
            ),
            properties: json!({
                "query": { "type": "string", "maxLength": 512, "description": "Natural-language search query sent to Exa's hosted MCP service. Maximum 512 characters." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 25, "description": "Maximum results to return. Defaults to 10." },
                "timeoutSec": { "type": "integer", "minimum": 1, "maximum": 60, "description": "Request timeout in seconds. Defaults to 10." }
            })
            .as_object()
            .expect("Exa MCP properties object")
            .clone(),
        }
    }

    fn validate_input(&self, input: &WebsearchInput) -> Result<(), String> {
        if input.query.chars().count() > MAX_QUERY_CHARS {
            return Err(format!(
                "Invalid arguments: Exa MCP query must not exceed {MAX_QUERY_CHARS} characters"
            ));
        }
        if input.country.is_some()
            || input.categories.is_some()
            || input.language.is_some()
            || input.pageno.is_some()
            || input.time_range.is_some()
            || input.safesearch.is_some()
        {
            return Err(
                "Invalid arguments: country, categories, language, pageno, time_range, and safesearch are not supported by Exa MCP; include supported category filters in the query"
                    .to_owned(),
            );
        }
        if input.limit.is_some_and(|value| value > 25) {
            return Err("Invalid arguments: Exa MCP limit must not exceed 25".to_owned());
        }
        Ok(())
    }

    async fn search(
        &self,
        input: &WebsearchInput,
        query: &str,
        dependencies: &WebToolDependencies,
        cancellation: CancellationToken,
    ) -> Result<ProviderResults, ProviderError> {
        search(input, query, dependencies, cancellation).await
    }
}

async fn search(
    input: &WebsearchInput,
    query: &str,
    dependencies: &WebToolDependencies,
    cancellation: CancellationToken,
) -> Result<ProviderResults, ProviderError> {
    let limit = limit(input);
    let timeout = timeout(input);
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let response = dependencies
        .http
        .execute(WebHttpRequest {
            kind: WebHttpRequestKind::PinnedProvider,
            method: Method::POST,
            url: Url::parse(ENDPOINT).expect("fixed Exa MCP endpoint"),
            headers: headers(),
            body: Some(
                serde_json::to_vec(&json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": TOOL_NAME,
                        "arguments": {
                            "query": query,
                            "numResults": limit
                        }
                    }
                }))
                .expect("fixed Exa MCP request serializes"),
            ),
            timeout: Duration::from_secs(timeout),
            max_redirects: 0,
            max_body_bytes: MAX_MCP_RESPONSE_BYTES,
            cancellation: cancellation.clone(),
        })
        .await
        .map_err(|error| match transport_error(error, timeout) {
            ProviderError::Cancelled => ProviderError::Cancelled,
            ProviderError::Message(message) => {
                ProviderError::Message(format!("Failed to search Exa MCP: {message}"))
            }
        })?;
    if !response.status.is_success() {
        let hint = if response.status.as_u16() == 429 {
            " The credential-free Exa MCP service may be rate limited; retry later or configure another backend."
        } else {
            ""
        };
        return Err(ProviderError::Message(format!(
            "Failed to search Exa MCP: Exa MCP returned HTTP {}.{hint}",
            response.status.as_u16()
        )));
    }
    if response.truncated || response.body.len() > MAX_MCP_RESPONSE_BYTES {
        return Err(ProviderError::Message(
            "Failed to search Exa MCP: Exa MCP response exceeded the size limit.".to_owned(),
        ));
    }

    let body = response.body.to_vec();
    let results = blocking::run_until(deadline, &cancellation, move || {
        parse_mcp_response(&body, MAX_NORMALIZED_ROWS)
    })
    .await
    .map_err(|error| {
        let error = blocking_error(error, timeout, "Exa MCP response was invalid.");
        match error {
            ProviderError::Cancelled => ProviderError::Cancelled,
            ProviderError::Message(message) => {
                ProviderError::Message(format!("Failed to search Exa MCP: {message}"))
            }
        }
    })?
    .map_err(|error| {
        let message = match error {
            McpResponseError::Invalid => "Exa MCP response was invalid.",
            McpResponseError::Remote => "Exa MCP reported a search error.",
        };
        ProviderError::Message(format!("Failed to search Exa MCP: {message}"))
    })?;

    Ok(ProviderResults {
        backend: WebsearchBackend::ExaMcp,
        query: collapse_whitespace_bounded(query, MAX_QUERY_CHARS),
        results_found: results.len(),
        results,
        limit,
    })
}

fn headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::USER_AGENT,
        HeaderValue::from_static(USER_AGENT),
    );
    headers.insert(
        http::header::ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers
}

#[derive(Clone, Copy, Debug)]
enum McpResponseError {
    Invalid,
    Remote,
}

fn parse_mcp_response(
    body: &[u8],
    row_budget: usize,
) -> Result<Vec<WebsearchResult>, McpResponseError> {
    let body = std::str::from_utf8(body).map_err(|_| McpResponseError::Invalid)?;
    let trimmed = body.trim();
    if trimmed.starts_with('{') {
        return match parse_payload(trimmed, row_budget)? {
            Payload::Final(results) => Ok(results),
            Payload::Ignore => Err(McpResponseError::Invalid),
        };
    }
    for event in body.replace("\r\n", "\n").split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            continue;
        }
        if let Payload::Final(results) = parse_payload(&data, row_budget)? {
            return Ok(results);
        }
    }
    Err(McpResponseError::Invalid)
}

enum Payload {
    Ignore,
    Final(Vec<WebsearchResult>),
}

fn parse_payload(payload: &str, row_budget: usize) -> Result<Payload, McpResponseError> {
    if !payload.trim_start().starts_with('{') {
        return Ok(Payload::Ignore);
    }
    let value: Value = serde_json::from_str(payload).map_err(|_| McpResponseError::Invalid)?;
    let id = value.get("id").and_then(Value::as_u64);
    if id != Some(1) {
        return Ok(Payload::Ignore);
    }
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return Err(McpResponseError::Remote);
    }
    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(McpResponseError::Invalid);
    }
    let result = value
        .get("result")
        .and_then(Value::as_object)
        .ok_or(McpResponseError::Invalid)?;
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(McpResponseError::Remote);
    }
    let texts = result
        .get("content")
        .and_then(Value::as_array)
        .ok_or(McpResponseError::Invalid)?
        .iter()
        .filter_map(Value::as_object)
        .filter(|content| content.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|content| content.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>();
    if texts.is_empty() {
        return Err(McpResponseError::Invalid);
    }

    let no_results = texts
        .iter()
        .all(|text| text.trim().starts_with("No search results found"));
    let mut seen = HashSet::new();
    let results = texts
        .into_iter()
        .flat_map(|text| parse_result_text(text, row_budget, &mut seen))
        .take(row_budget)
        .collect::<Vec<_>>();
    if results.is_empty() && !no_results {
        return Err(McpResponseError::Invalid);
    }
    Ok(Payload::Final(results))
}

fn parse_result_text(
    text: &str,
    row_budget: usize,
    seen: &mut HashSet<String>,
) -> Vec<WebsearchResult> {
    let text = text.replace("\r\n", "\n");
    text.split("\n\n---\n\n")
        .take(row_budget)
        .filter_map(|block| {
            let mut lines = block.lines();
            let title = lines.next()?.strip_prefix("Title:")?.trim();
            let url = lines.next()?.strip_prefix("URL:")?.trim();
            lines.next()?.strip_prefix("Published:")?;
            lines.next()?.strip_prefix("Author:")?;
            let marker = lines.next()?;
            let initial = marker
                .strip_prefix("Highlights:")
                .or_else(|| marker.strip_prefix("Text:"))?;
            let mut snippet = Vec::new();
            if !initial.trim().is_empty() {
                snippet.push(initial.trim());
            }
            snippet.extend(lines);
            normalized_basic_result(
                title,
                url,
                &snippet.join("\n"),
                Some("exa".to_owned()),
                seen,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_direct_and_sse_mcp_envelopes() {
        let envelope = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"content": [{
                "type": "text",
                "text": "Title: Result\nURL: https://example.com/page\nPublished: N/A\nAuthor: N/A\nHighlights:\nUseful context"
            }]}
        });
        let direct = serde_json::to_vec(&envelope).unwrap();
        assert_eq!(parse_mcp_response(&direct, 10).unwrap().len(), 1);
        let sse = format!("event: message\r\ndata: {envelope}\r\n\r\n");
        assert_eq!(parse_mcp_response(sse.as_bytes(), 10).unwrap().len(), 1);
    }

    #[test]
    fn sse_joins_data_lines_and_ignores_notifications() {
        let body = concat!(
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\n",
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\n",
            "data: \"result\":{\"content\":[{\"type\":\"text\",\"text\":\"Title: Result\\nURL: https://example.com/page\\nPublished: N/A\\nAuthor: N/A\\nHighlights:\\nUseful\"}]}}\n\n"
        );
        assert_eq!(parse_mcp_response(body.as_bytes(), 10).unwrap().len(), 1);
    }

    #[test]
    fn strict_headers_prevent_highlight_field_injection() {
        let envelope = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"content": [{
                "type": "text",
                "text": "Title: Real\nURL: https://example.com/real\nPublished: N/A\nAuthor: N/A\nHighlights:\nURL: https://attacker.example/forged\nTitle: Forged"
            }]}
        });
        let results = parse_mcp_response(envelope.to_string().as_bytes(), 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://example.com/real");
        assert_eq!(results[0].title, "Real");
    }

    #[test]
    fn rejects_remote_errors_without_retaining_their_text() {
        let envelope = br#"{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[{"type":"text","text":"private remote detail"}]}}"#;
        assert!(matches!(
            parse_mcp_response(envelope, 10),
            Err(McpResponseError::Remote)
        ));
    }
}
