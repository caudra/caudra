use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use http::{HeaderMap, HeaderValue, Method};
use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::common::{
    MAX_NORMALIZED_ROWS, MAX_RESPONSE_BYTES, ProviderError, ProviderResults, USER_AGENT,
    blocking_error, limit, timeout, transport_error,
};
use super::normalize::{
    MAX_QUERY_CHARS, collapse_whitespace_bounded, first_engine, normalized_basic_result,
    safe_png_data_url,
};
use super::provider::{ProviderCatalogContract, WebsearchProvider};
use crate::blocking;
use crate::config::Credential;
use crate::dependencies::{WebHttpRequest, WebHttpRequestKind};
use crate::types::{TimeRange, WebsearchInput};
use crate::{WebToolDependencies, WebsearchBackend};

pub(crate) struct SearxngProvider {
    endpoint: String,
    credential: Option<Credential>,
}

impl SearxngProvider {
    pub(crate) fn new(endpoint: String, credential: Option<Credential>) -> Self {
        Self {
            endpoint,
            credential,
        }
    }
}

impl fmt::Debug for SearxngProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearxngProvider")
            .field("endpoint", &"[CONFIGURED]")
            .field("credential", &self.credential)
            .finish()
    }
}

#[async_trait]
impl WebsearchProvider for SearxngProvider {
    fn backend(&self) -> WebsearchBackend {
        WebsearchBackend::Searxng
    }

    fn catalog_contract(&self, current_year: i32) -> ProviderCatalogContract {
        ProviderCatalogContract {
            description: format!(
                "Search the web using the configured SearXNG backend.\n\n- Returns titles, URLs, snippets, engine metadata, and optional source icon metadata.\n- Supports SearXNG categories, language, pagination, time range, and safe-search levels.\n- Prefer precise queries with the current year for recent information.\n- Use webfetch after search identifies a page that needs detailed analysis.\n\nThe current year is {current_year}."
            ),
            properties: json!({
                "query": { "type": "string", "description": "Search query." },
                "categories": { "type": "string", "description": "Comma-separated SearXNG categories." },
                "language": { "type": "string", "description": "Language code, for example en, de, or fr." },
                "pageno": { "type": "integer", "minimum": 1, "description": "SearXNG page number. Defaults to 1." },
                "time_range": { "type": "string", "enum": ["day", "month", "year"] },
                "safesearch": { "type": "integer", "minimum": 0, "maximum": 2, "description": "SearXNG safe-search level: 0, 1, or 2." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 25, "description": "Maximum results to return. Defaults to 10." },
                "timeoutSec": { "type": "integer", "minimum": 1, "maximum": 60, "description": "Request timeout in seconds. Defaults to 10." }
            })
            .as_object()
            .expect("SearXNG properties object")
            .clone(),
        }
    }

    fn validate_input(&self, input: &WebsearchInput) -> Result<(), String> {
        if input.country.is_some() {
            return Err("Invalid arguments: country is not supported by SearXNG".to_owned());
        }
        if input.limit.is_some_and(|value| value > 25) {
            return Err("Invalid arguments: SearXNG limit must not exceed 25".to_owned());
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
            &self.endpoint,
            self.credential.as_ref(),
            dependencies,
            cancellation,
        )
        .await
    }
}

async fn search(
    input: &WebsearchInput,
    query: &str,
    endpoint: &str,
    credential: Option<&Credential>,
    dependencies: &WebToolDependencies,
    cancellation: CancellationToken,
) -> Result<ProviderResults, ProviderError> {
    let mut url = parse_endpoint(endpoint, credential.is_some()).map_err(ProviderError::Message)?;
    lower_query(&mut url, input, query);
    let timeout = timeout(input);
    // Provider transfer and bounded JSON parsing share one deadline.
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let response = dependencies
        .http
        .execute(WebHttpRequest {
            kind: WebHttpRequestKind::OperatorGet,
            method: Method::GET,
            url,
            headers: headers(credential),
            body: None,
            timeout: Duration::from_secs(timeout),
            max_redirects: 5,
            max_body_bytes: MAX_RESPONSE_BYTES,
            cancellation: cancellation.clone(),
        })
        .await
        .map_err(|error| match transport_error(error, timeout) {
            ProviderError::Cancelled => ProviderError::Cancelled,
            ProviderError::Message(message) => {
                ProviderError::Message(format!("Failed to search SearXNG: {message}"))
            }
        })?;
    if !response.status.is_success() {
        let hint = if response.status.as_u16() == 401 {
            " Check the configured websearch credentials."
        } else {
            ""
        };
        return Err(ProviderError::Message(format!(
            "Failed to search SearXNG: SearXNG returned HTTP {}.{hint}",
            response.status.as_u16()
        )));
    }
    if response.truncated || response.body.len() > MAX_RESPONSE_BYTES {
        return Err(ProviderError::Message(
            "Failed to search SearXNG: SearXNG response exceeded the size limit.".to_owned(),
        ));
    }
    let body = response.body.to_vec();
    let fallback_query = collapse_whitespace_bounded(query, MAX_QUERY_CHARS);
    let parsed = blocking::run_until(deadline, &cancellation, move || {
        let Value::Object(data) = serde_json::from_slice::<Value>(&body).ok()? else {
            return None;
        };
        let results = searxng_results(&data, MAX_NORMALIZED_ROWS);
        let output_query = data
            .get("query")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map_or(fallback_query, |value| {
                collapse_whitespace_bounded(value, MAX_QUERY_CHARS)
            });
        let results_found = data
            .get("number_of_results")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
            .map_or(0, |value| value.round().max(0.0) as usize)
            .max(results.len());
        Some((results, output_query, results_found))
    })
    .await
    .map_err(|error| {
        let error = blocking_error(error, timeout, "SearXNG response was not valid JSON.");
        match error {
            ProviderError::Cancelled => ProviderError::Cancelled,
            ProviderError::Message(message) => {
                ProviderError::Message(format!("Failed to search SearXNG: {message}"))
            }
        }
    })?;
    let (results, output_query, results_found) = parsed.ok_or_else(|| {
        ProviderError::Message(
            "Failed to search SearXNG: SearXNG response was not valid JSON.".to_owned(),
        )
    })?;
    Ok(ProviderResults {
        backend: WebsearchBackend::Searxng,
        query: output_query,
        results_found,
        results,
        limit: limit(input),
    })
}

fn parse_endpoint(endpoint: &str, has_credentials: bool) -> Result<Url, String> {
    let url = Url::parse(endpoint.trim())
        .map_err(|_| "The configured SearXNG endpoint must be a valid HTTP(S) URL.".to_owned())?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err("The configured SearXNG endpoint must be a valid HTTP(S) URL.".to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() || authority_has_at(endpoint) {
        return Err("The configured SearXNG endpoint must not contain URL credentials.".to_owned());
    }
    if has_credentials && url.scheme() != "https" {
        return Err(
            "The configured SearXNG endpoint must use HTTPS when credentials are configured."
                .to_owned(),
        );
    }
    Ok(url)
}

fn lower_query(url: &mut Url, input: &WebsearchInput, query: &str) {
    set_query_parameter(url, "q", query);
    set_query_parameter(url, "format", "json");
    set_query_parameter(url, "results_on_new_tab", "0");
    set_query_parameter(url, "pageno", &input.pageno.unwrap_or(1).to_string());
    if let Some(value) = input.categories.as_deref() {
        set_query_parameter(url, "categories", value);
    }
    if let Some(value) = input.language.as_deref() {
        set_query_parameter(url, "language", value);
    }
    if let Some(value) = input.time_range {
        set_query_parameter(url, "time_range", time_range_name(value));
    }
    if let Some(value) = input.safesearch {
        set_query_parameter(url, "safesearch", &value.to_string());
    }
}

fn headers(credential: Option<&Credential>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::USER_AGENT,
        HeaderValue::from_static(USER_AGENT),
    );
    headers.insert(
        http::header::ACCEPT,
        HeaderValue::from_static("application/json"),
    );
    match credential {
        Some(Credential::ApiKey(key)) => insert(&mut headers, "x-api-key", key.expose()),
        Some(Credential::Bearer(token)) => insert(
            &mut headers,
            http::header::AUTHORIZATION,
            &format!("Bearer {}", token.expose()),
        ),
        Some(Credential::Basic { username, password }) => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(format!(
                "{}:{}",
                username.expose(),
                password.expose()
            ));
            insert(
                &mut headers,
                http::header::AUTHORIZATION,
                &format!("Basic {encoded}"),
            );
        }
        None => {}
    }
    headers
}

fn insert(headers: &mut HeaderMap, name: impl http::header::IntoHeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

fn set_query_parameter(url: &mut Url, key: &str, value: &str) {
    let existing = url
        .query_pairs()
        .filter(|(name, _)| name != key)
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    url.set_query(None);
    let mut query = url.query_pairs_mut();
    query.extend_pairs(existing);
    query.append_pair(key, value);
}

fn time_range_name(value: TimeRange) -> &'static str {
    match value {
        TimeRange::Day => "day",
        TimeRange::Month => "month",
        TimeRange::Year => "year",
    }
}

fn authority_has_at(value: &str) -> bool {
    value
        .trim()
        .split_once("://")
        .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'))
}

fn searxng_results(
    data: &serde_json::Map<String, Value>,
    row_budget: usize,
) -> Vec<crate::WebsearchResult> {
    let mut seen = HashSet::new();
    data.get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(row_budget)
        .filter_map(|row| {
            let row = row.as_object()?;
            let backend_icon = row
                .get("iconDataUrl")
                .or_else(|| row.get("favicon"))
                .and_then(Value::as_str)
                .filter(|value| safe_png_data_url(value))
                .map(str::to_owned);
            let mut result = normalized_basic_result(
                row.get("title")?.as_str()?,
                row.get("url")?.as_str()?,
                row.get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                first_engine(row.get("engine")),
                &mut seen,
            )?;
            result.icon_data_url = backend_icon;
            Some(result)
        })
        .collect()
}
