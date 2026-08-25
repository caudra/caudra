use std::collections::HashSet;
use std::fmt;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use http::{HeaderMap, HeaderValue, Method};
use serde_json::{Map, Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::common::{
    MAX_NORMALIZED_ROWS, MAX_RESPONSE_BYTES, MAX_SNIPPET_CHARS, ProviderError, ProviderResults,
    USER_AGENT, blocking_error, limit, timeout, transport_error,
};
use super::normalize::{
    MAX_QUERY_CHARS, bounded_highlights, collapse_whitespace_bounded, normalized_basic_result,
};
use super::provider::{ProviderCatalogContract, WebsearchProvider};
use crate::blocking;
use crate::config::Secret;
use crate::dependencies::{WebHttpRequest, WebHttpRequestKind};
use crate::types::{TimeRange, WebsearchInput};
use crate::{WebToolDependencies, WebsearchBackend};

const ENDPOINT: &str = "https://api.exa.ai/search";

pub(crate) struct ExaProvider {
    api_key: Secret,
}

impl ExaProvider {
    pub(crate) fn new(api_key: Secret) -> Self {
        Self { api_key }
    }
}

impl fmt::Debug for ExaProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExaProvider")
            .field("api_key", &self.api_key)
            .finish()
    }
}

#[async_trait]
impl WebsearchProvider for ExaProvider {
    fn backend(&self) -> WebsearchBackend {
        WebsearchBackend::Exa
    }

    fn catalog_contract(&self, current_year: i32) -> ProviderCatalogContract {
        ProviderCatalogContract {
            description: format!(
                "Search the web using the configured Exa backend.\n\n- Returns titles, URLs, highlighted snippets, and optional source icon metadata.\n- A time range limits results by publication date.\n- Any nonzero safe-search value enables Exa moderation.\n- Prefer precise queries with the current year for recent information.\n- Use webfetch after search identifies a page that needs detailed analysis.\n\nThe current year is {current_year}."
            ),
            properties: json!({
                "query": { "type": "string", "description": "Search query." },
                "time_range": { "type": "string", "enum": ["day", "month", "year"], "description": "Limit results to content published during the selected period." },
                "safesearch": { "type": "integer", "minimum": 0, "maximum": 2, "description": "Set to 1 or 2 to enable Exa moderation; 0 disables it." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 25, "description": "Maximum results to return. Defaults to 10." },
                "timeoutSec": { "type": "integer", "minimum": 1, "maximum": 60, "description": "Request timeout in seconds. Defaults to 10." }
            })
            .as_object()
            .expect("Exa properties object")
            .clone(),
        }
    }

    fn validate_input(&self, input: &WebsearchInput) -> Result<(), String> {
        if input.country.is_some()
            || input.categories.is_some()
            || input.language.is_some()
            || input.pageno.is_some()
        {
            return Err(
                "Invalid arguments: country, categories, language, and pageno are not supported by Exa"
                    .to_owned(),
            );
        }
        if input.limit.is_some_and(|value| value > 25) {
            return Err("Invalid arguments: Exa limit must not exceed 25".to_owned());
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
        search(
            input,
            query,
            self.api_key.expose(),
            dependencies,
            cancellation,
        )
        .await
    }
}

async fn search(
    input: &WebsearchInput,
    query: &str,
    api_key: &str,
    dependencies: &WebToolDependencies,
    cancellation: CancellationToken,
) -> Result<ProviderResults, ProviderError> {
    let limit = limit(input);
    let request_body = lower_body(input, query, limit, dependencies);
    let headers = headers(api_key).ok_or_else(|| {
        ProviderError::Message("Failed to search Exa: Search backend request failed.".to_owned())
    })?;
    let timeout = timeout(input);
    // Provider transfer and bounded JSON parsing share one deadline.
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let response = dependencies
        .http
        .execute(WebHttpRequest {
            kind: WebHttpRequestKind::PinnedProvider,
            method: Method::POST,
            url: Url::parse(ENDPOINT).expect("fixed Exa endpoint"),
            headers,
            body: serde_json::to_vec(&request_body).ok(),
            timeout: Duration::from_secs(timeout),
            max_redirects: 0,
            max_body_bytes: MAX_RESPONSE_BYTES,
            cancellation: cancellation.clone(),
        })
        .await
        .map_err(|error| match transport_error(error, timeout) {
            ProviderError::Cancelled => ProviderError::Cancelled,
            ProviderError::Message(message) => {
                ProviderError::Message(format!("Failed to search Exa: {message}"))
            }
        })?;
    if !response.status.is_success() {
        let hint = if matches!(response.status.as_u16(), 401 | 403) {
            " Check the configured Exa API key."
        } else {
            ""
        };
        return Err(ProviderError::Message(format!(
            "Failed to search Exa: Exa returned HTTP {}.{hint}",
            response.status.as_u16()
        )));
    }
    if response.truncated || response.body.len() > MAX_RESPONSE_BYTES {
        return Err(ProviderError::Message(
            "Failed to search Exa: Exa response exceeded the size limit.".to_owned(),
        ));
    }
    let body = response.body.to_vec();
    let results = blocking::run_until(deadline, &cancellation, move || {
        let Value::Object(data) = serde_json::from_slice::<Value>(&body).ok()? else {
            return None;
        };
        Some(exa_results(&data, MAX_NORMALIZED_ROWS))
    })
    .await
    .map_err(|error| {
        let error = blocking_error(error, timeout, "Exa response was not valid JSON.");
        match error {
            ProviderError::Cancelled => ProviderError::Cancelled,
            ProviderError::Message(message) => {
                ProviderError::Message(format!("Failed to search Exa: {message}"))
            }
        }
    })?
    .ok_or_else(|| {
        ProviderError::Message("Failed to search Exa: Exa response was not valid JSON.".to_owned())
    })?;
    Ok(ProviderResults {
        backend: WebsearchBackend::Exa,
        query: collapse_whitespace_bounded(query, MAX_QUERY_CHARS),
        results_found: results.len(),
        results,
        limit,
    })
}

fn lower_body(
    input: &WebsearchInput,
    query: &str,
    limit: usize,
    dependencies: &WebToolDependencies,
) -> Map<String, Value> {
    let mut request = Map::from_iter([
        ("query".to_owned(), Value::String(query.to_owned())),
        ("type".to_owned(), Value::String("auto".to_owned())),
        ("numResults".to_owned(), json!(limit)),
        (
            "contents".to_owned(),
            json!({"highlights": {"maxCharacters": MAX_SNIPPET_CHARS}}),
        ),
    ]);
    if let Some(range) = input.time_range {
        let days = match range {
            TimeRange::Day => 1,
            TimeRange::Month => 30,
            TimeRange::Year => 365,
        };
        let time = dependencies
            .clock
            .now()
            .checked_sub(Duration::from_secs(days * 24 * 60 * 60))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let timestamp: DateTime<Utc> = time.into();
        request.insert(
            "startPublishedDate".to_owned(),
            Value::String(timestamp.to_rfc3339_opts(SecondsFormat::Millis, true)),
        );
    }
    if let Some(safesearch) = input.safesearch {
        request.insert("moderation".to_owned(), Value::Bool(safesearch > 0));
    }
    request
}

fn headers(api_key: &str) -> Option<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::USER_AGENT,
        HeaderValue::from_static(USER_AGENT),
    );
    headers.insert(
        http::header::ACCEPT,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert("x-api-key", HeaderValue::from_str(api_key).ok()?);
    Some(headers)
}

fn exa_results(data: &Map<String, Value>, row_budget: usize) -> Vec<crate::WebsearchResult> {
    let mut seen = HashSet::new();
    data.get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(row_budget)
        .filter_map(|row| {
            let row = row.as_object()?;
            let highlights = bounded_highlights(row.get("highlights"));
            let snippet = if highlights.is_empty() {
                row.get("text").and_then(Value::as_str).unwrap_or_default()
            } else {
                &highlights
            };
            normalized_basic_result(
                row.get("title")?.as_str()?,
                row.get("url")?.as_str()?,
                snippet,
                Some("exa".to_owned()),
                &mut seen,
            )
        })
        .collect()
}
