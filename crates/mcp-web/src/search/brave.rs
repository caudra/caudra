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
    MAX_NORMALIZED_ROWS, MAX_RESPONSE_BYTES, ProviderError, ProviderResults, USER_AGENT,
    blocking_error, limit, timeout, transport_error,
};
use super::normalize::{MAX_QUERY_CHARS, collapse_whitespace_bounded, normalized_basic_result};
use super::provider::{ProviderCatalogContract, WebsearchProvider};
use crate::blocking;
use crate::config::Secret;
use crate::dependencies::{WebHttpRequest, WebHttpRequestKind};
use crate::types::{TimeRange, WebsearchInput};
use crate::{WebToolDependencies, WebsearchBackend, WebsearchResult};

const ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const MAX_QUERY_CHARS_BRAVE: usize = 400;
const MAX_QUERY_WORDS_BRAVE: usize = 50;
const MAX_LANGUAGE_BYTES: usize = 32;

pub(crate) struct BraveProvider {
    api_key: Secret,
}

impl BraveProvider {
    pub(crate) fn new(api_key: Secret) -> Self {
        Self { api_key }
    }
}

impl fmt::Debug for BraveProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BraveProvider")
            .field("api_key", &self.api_key)
            .finish()
    }
}

#[async_trait]
impl WebsearchProvider for BraveProvider {
    fn backend(&self) -> WebsearchBackend {
        WebsearchBackend::Brave
    }

    fn catalog_contract(&self, current_year: i32) -> ProviderCatalogContract {
        ProviderCatalogContract {
            description: format!(
                "Search the web using the configured Brave Search API backend.\n\n- Returns titles, URLs, snippets, and optional source icon metadata.\n- Supports country and language targeting, pagination, freshness filtering, and safe-search levels.\n- Queries are limited to 400 characters and 50 words.\n- Prefer precise queries with the current year for recent information.\n- Use webfetch after search identifies a page that needs detailed analysis.\n\nThe current year is {current_year}."
            ),
            properties: json!({
                "query": { "type": "string", "maxLength": 400, "description": "Search query, limited to 400 characters and 50 words." },
                "country": { "type": "string", "minLength": 2, "maxLength": 2, "description": "Two-letter country code for result targeting, for example US or DE." },
                "language": { "type": "string", "description": "Brave search language code, for example en, de, or pt-br." },
                "pageno": { "type": "integer", "minimum": 1, "maximum": 10, "description": "Result page number from 1 through 10. Defaults to 1." },
                "time_range": { "type": "string", "enum": ["day", "month", "year"], "description": "Limit results to pages from the selected period." },
                "safesearch": { "type": "integer", "minimum": 0, "maximum": 2, "description": "Safe-search level: 0 off, 1 moderate, or 2 strict." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 20, "description": "Maximum web results to return. Defaults to 10." },
                "timeoutSec": { "type": "integer", "minimum": 1, "maximum": 60, "description": "Request timeout in seconds. Defaults to 10." }
            })
            .as_object()
            .expect("Brave properties object")
            .clone(),
        }
    }

    fn validate_input(&self, input: &WebsearchInput) -> Result<(), String> {
        if input.categories.is_some() {
            return Err("Invalid arguments: categories is not supported by Brave".to_owned());
        }
        if input.query.chars().count() > MAX_QUERY_CHARS_BRAVE
            || input.query.split_whitespace().count() > MAX_QUERY_WORDS_BRAVE
        {
            return Err(
                "Invalid arguments: Brave query must not exceed 400 characters or 50 words"
                    .to_owned(),
            );
        }
        if let Some(country) = input.country.as_deref()
            && (country.len() != 2 || !country.bytes().all(|value| value.is_ascii_alphabetic()))
        {
            return Err("Invalid arguments: country must be a two-letter code".to_owned());
        }
        if input
            .language
            .as_deref()
            .is_some_and(|value| value.len() > MAX_LANGUAGE_BYTES || !value.is_ascii())
        {
            return Err(
                "Invalid arguments: language must be a short ASCII language code".to_owned(),
            );
        }
        if input.pageno.is_some_and(|value| value > 10) {
            return Err("Invalid arguments: Brave pageno must not exceed 10".to_owned());
        }
        if input.limit.is_some_and(|value| value > 20) {
            return Err("Invalid arguments: Brave limit must not exceed 20".to_owned());
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
        let result_limit = limit(input).min(20);
        let url = lower_query(input, query, result_limit);
        let headers = headers(self.api_key.expose()).ok_or_else(|| {
            ProviderError::Message(
                "Failed to search Brave: Search backend request failed.".to_owned(),
            )
        })?;
        let timeout = timeout(input);
        let deadline = Instant::now() + Duration::from_secs(timeout);
        let response = dependencies
            .http
            .execute(WebHttpRequest {
                kind: WebHttpRequestKind::PinnedProvider,
                method: Method::GET,
                url,
                headers,
                body: None,
                timeout: Duration::from_secs(timeout),
                max_redirects: 0,
                max_body_bytes: MAX_RESPONSE_BYTES,
                cancellation: cancellation.clone(),
            })
            .await
            .map_err(|error| match transport_error(error, timeout) {
                ProviderError::Cancelled => ProviderError::Cancelled,
                ProviderError::Message(message) => {
                    ProviderError::Message(format!("Failed to search Brave: {message}"))
                }
            })?;
        if !response.status.is_success() {
            let hint = if matches!(response.status.as_u16(), 401 | 403) {
                " Check the configured Brave API key."
            } else if response.status.as_u16() == 429 {
                " The Brave Search API rate limit was exceeded."
            } else {
                ""
            };
            return Err(ProviderError::Message(format!(
                "Failed to search Brave: Brave returned HTTP {}.{hint}",
                response.status.as_u16()
            )));
        }
        if response.truncated || response.body.len() > MAX_RESPONSE_BYTES {
            return Err(ProviderError::Message(
                "Failed to search Brave: Brave response exceeded the size limit.".to_owned(),
            ));
        }
        let body = response.body.to_vec();
        let fallback_query = collapse_whitespace_bounded(query, MAX_QUERY_CHARS);
        let parsed = blocking::run_until(deadline, &cancellation, move || {
            let Value::Object(data) = serde_json::from_slice::<Value>(&body).ok()? else {
                return None;
            };
            let results = brave_results(&data, MAX_NORMALIZED_ROWS);
            let output_query = data
                .get("query")
                .and_then(Value::as_object)
                .and_then(|query| query.get("altered").or_else(|| query.get("original")))
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map_or(fallback_query, |value| {
                    collapse_whitespace_bounded(value, MAX_QUERY_CHARS)
                });
            Some((results, output_query))
        })
        .await
        .map_err(|error| {
            match blocking_error(error, timeout, "Brave response was not valid JSON.") {
                ProviderError::Cancelled => ProviderError::Cancelled,
                ProviderError::Message(message) => {
                    ProviderError::Message(format!("Failed to search Brave: {message}"))
                }
            }
        })?
        .ok_or_else(|| {
            ProviderError::Message(
                "Failed to search Brave: Brave response was not valid JSON.".to_owned(),
            )
        })?;
        let (results, output_query) = parsed;
        Ok(ProviderResults {
            backend: WebsearchBackend::Brave,
            query: output_query,
            results_found: results.len(),
            results,
            limit: result_limit,
        })
    }
}

fn lower_query(input: &WebsearchInput, query: &str, limit: usize) -> Url {
    let mut url = Url::parse(ENDPOINT).expect("fixed Brave endpoint");
    let mut parameters = url.query_pairs_mut();
    parameters.append_pair("q", query);
    parameters.append_pair("count", &limit.to_string());
    parameters.append_pair("result_filter", "web");
    parameters.append_pair("text_decorations", "false");
    if let Some(country) = input.country.as_deref() {
        parameters.append_pair("country", &country.to_ascii_uppercase());
    }
    if let Some(language) = input.language.as_deref() {
        parameters.append_pair("search_lang", language.trim());
    }
    if let Some(page) = input.pageno {
        parameters.append_pair("offset", &(page - 1).to_string());
    }
    if let Some(range) = input.time_range {
        parameters.append_pair(
            "freshness",
            match range {
                TimeRange::Day => "pd",
                TimeRange::Month => "pm",
                TimeRange::Year => "py",
            },
        );
    }
    if let Some(safesearch) = input.safesearch {
        parameters.append_pair(
            "safesearch",
            match safesearch {
                0 => "off",
                1 => "moderate",
                _ => "strict",
            },
        );
    }
    drop(parameters);
    url
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
    headers.insert("x-subscription-token", HeaderValue::from_str(api_key).ok()?);
    Some(headers)
}

fn brave_results(data: &serde_json::Map<String, Value>, row_budget: usize) -> Vec<WebsearchResult> {
    let mut seen = HashSet::new();
    data.get("web")
        .and_then(Value::as_object)
        .and_then(|web| web.get("results"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(row_budget)
        .filter_map(|row| {
            let row = row.as_object()?;
            normalized_basic_result(
                row.get("title")?.as_str()?,
                row.get("url")?.as_str()?,
                row.get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                Some("brave".to_owned()),
                &mut seen,
            )
        })
        .collect()
}
