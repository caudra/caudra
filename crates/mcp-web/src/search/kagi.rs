use std::{collections::HashSet, fmt, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use http::{HeaderMap, HeaderValue, Method};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::common::{
    MAX_NORMALIZED_ROWS, MAX_RESPONSE_BYTES, ProviderError, ProviderResults, USER_AGENT, limit,
    timeout, transport_error,
};
use super::normalize::{MAX_QUERY_CHARS, collapse_whitespace_bounded, normalized_basic_result};
use super::provider::{ProviderCatalogContract, WebsearchProvider};
use crate::config::Secret;
use crate::dependencies::{WebHttpRequest, WebHttpRequestKind};
use crate::types::{TimeRange, WebsearchInput};
use crate::{WebToolDependencies, WebsearchBackend, WebsearchResult};

const ENDPOINT: &str = "https://kagi.com/api/v1/search";

pub(crate) struct KagiProvider {
    api_key: Secret,
}

impl KagiProvider {
    pub(crate) fn new(api_key: Secret) -> Self {
        Self { api_key }
    }
}

impl fmt::Debug for KagiProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KagiProvider")
            .field("api_key", &self.api_key)
            .finish()
    }
}

#[async_trait]
impl WebsearchProvider for KagiProvider {
    fn backend(&self) -> WebsearchBackend {
        WebsearchBackend::Kagi
    }

    fn catalog_contract(&self, current_year: i32) -> ProviderCatalogContract {
        ProviderCatalogContract {
            description: format!(
                "Search the web using the configured Kagi Search API backend. Supports country targeting, pagination, recency, safe search, and result limits. The current year is {current_year}."
            ),
            properties: json!({
                "query": {"type":"string","description":"Search query."},
                "country": {"type":"string","minLength":2,"maxLength":2},
                "pageno": {"type":"integer","minimum":1,"maximum":10},
                "time_range": {"type":"string","enum":["day","month","year"]},
                "safesearch": {"type":"integer","minimum":0,"maximum":2},
                "limit": {"type":"integer","minimum":1,"maximum":25},
                "timeoutSec": {"type":"integer","minimum":1,"maximum":60}
            })
            .as_object()
            .expect("Kagi properties")
            .clone(),
        }
    }

    fn validate_input(&self, input: &WebsearchInput) -> Result<(), String> {
        if input.categories.is_some() || input.language.is_some() {
            return Err(
                "Invalid arguments: categories and language are not supported by Kagi".to_owned(),
            );
        }
        if input.pageno.is_some_and(|v| v > 10) {
            return Err("Invalid arguments: Kagi pageno must not exceed 10".to_owned());
        }
        if input
            .country
            .as_deref()
            .is_some_and(|v| v.len() != 2 || !v.bytes().all(|byte| byte.is_ascii_alphabetic()))
        {
            return Err("Invalid arguments: country must be a two-letter code".to_owned());
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
        let result_limit = limit(input);
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
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.api_key.expose())).map_err(|_| {
                ProviderError::Message(
                    "Failed to search Kagi: Search backend request failed.".to_owned(),
                )
            })?,
        );
        let caller_timeout = timeout(input);
        let body = serde_json::to_vec(&lower_body(input, query, result_limit, dependencies))
            .map_err(|_| {
                ProviderError::Message(
                    "Failed to search Kagi: Search backend request failed.".to_owned(),
                )
            })?;
        let seconds = caller_timeout;
        let response = dependencies
            .http
            .execute(WebHttpRequest {
                kind: WebHttpRequestKind::PinnedProvider,
                method: Method::POST,
                url: Url::parse(ENDPOINT).expect("fixed Kagi endpoint"),
                headers,
                body: Some(body),
                timeout: Duration::from_secs(seconds),
                max_redirects: 0,
                max_body_bytes: MAX_RESPONSE_BYTES,
                cancellation,
            })
            .await
            .map_err(|e| match transport_error(e, seconds) {
                ProviderError::Cancelled => ProviderError::Cancelled,
                ProviderError::Message(m) => {
                    ProviderError::Message(format!("Failed to search Kagi: {m}"))
                }
            })?;
        if !response.status.is_success() {
            return Err(ProviderError::Message(format!(
                "Failed to search Kagi: Kagi returned HTTP {}.",
                response.status.as_u16()
            )));
        }
        if response.truncated {
            return Err(ProviderError::Message(
                "Failed to search Kagi: Kagi response exceeded the size limit.".to_owned(),
            ));
        }
        let data: Value = serde_json::from_slice(&response.body).map_err(|_| {
            ProviderError::Message(
                "Failed to search Kagi: Kagi response was not valid JSON.".to_owned(),
            )
        })?;
        if has_provider_error(data.get("error")) {
            return Err(ProviderError::Message(
                "Failed to search Kagi: Kagi reported a search error.".to_owned(),
            ));
        }
        if data
            .get("data")
            .and_then(|value| value.get("search"))
            .and_then(Value::as_array)
            .is_none()
        {
            return Err(ProviderError::Message(
                "Failed to search Kagi: Kagi response did not contain search results.".to_owned(),
            ));
        }
        let results = kagi_results(&data, MAX_NORMALIZED_ROWS);
        Ok(ProviderResults {
            backend: WebsearchBackend::Kagi,
            query: collapse_whitespace_bounded(query, MAX_QUERY_CHARS),
            results_found: results.len(),
            results,
            limit: result_limit,
        })
    }
}

fn lower_body(
    input: &WebsearchInput,
    query: &str,
    limit: usize,
    dependencies: &WebToolDependencies,
) -> Map<String, Value> {
    let mut body = Map::from_iter([
        ("query".into(), json!(query)),
        ("workflow".into(), json!("search")),
        ("format".into(), json!("json")),
        ("limit".into(), json!(limit)),
        (
            "timeout".into(),
            json!(timeout(input).saturating_sub(1).max(1)),
        ),
    ]);
    if let Some(v) = input.pageno {
        body.insert("page".into(), json!(v));
    }
    let mut filters = Map::new();
    if let Some(country) = &input.country {
        filters.insert("region".into(), json!(country.to_ascii_uppercase()));
    }
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
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let timestamp: DateTime<Utc> = time.into();
        filters.insert(
            "after".into(),
            Value::String(timestamp.format("%Y-%m-%d").to_string()),
        );
    }
    if !filters.is_empty() {
        body.insert("filters".into(), Value::Object(filters));
    }
    if let Some(v) = input.safesearch {
        body.insert("safe_search".into(), json!(v > 0));
    }
    body
}

fn has_provider_error(error: Option<&Value>) -> bool {
    match error {
        Some(Value::String(value)) => !value.trim().is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
        _ => false,
    }
}

fn kagi_results(data: &Value, budget: usize) -> Vec<WebsearchResult> {
    let mut seen = HashSet::new();
    data.get("data")
        .and_then(|v| v.get("search"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(budget)
        .filter_map(|v| {
            let row = v.as_object()?;
            normalized_basic_result(
                row.get("title")?.as_str()?,
                row.get("url")?.as_str()?,
                row.get("snippet")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                Some("kagi".to_owned()),
                &mut seen,
            )
        })
        .collect()
}
