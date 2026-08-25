use std::{collections::HashSet, fmt, time::Duration};

use async_trait::async_trait;
use http::{HeaderMap, HeaderValue, Method};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::common::{
    MAX_NORMALIZED_ROWS, MAX_RESPONSE_BYTES, ProviderError, ProviderResults, USER_AGENT, limit,
    timeout, transport_error,
};
use super::normalize::{MAX_QUERY_CHARS, collapse_whitespace_bounded, normalized_basic_result};
use super::provider::{ProviderCatalogContract, WebsearchProvider};
use crate::config::{Secret, SerpApiEngine};
use crate::dependencies::{WebHttpRequest, WebHttpRequestKind};
use crate::types::{TimeRange, WebsearchInput};
use crate::{WebToolDependencies, WebsearchBackend, WebsearchResult};

const ENDPOINT: &str = "https://serpapi.com/search.json";

pub(crate) struct SerpApiProvider {
    api_key: Secret,
    engine: SerpApiEngine,
}
impl SerpApiProvider {
    pub(crate) fn new(api_key: Secret, engine: SerpApiEngine) -> Self {
        Self { api_key, engine }
    }
}
impl fmt::Debug for SerpApiProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SerpApiProvider")
            .field("api_key", &self.api_key)
            .field("engine", &self.engine)
            .finish()
    }
}

#[async_trait]
impl WebsearchProvider for SerpApiProvider {
    fn backend(&self) -> WebsearchBackend {
        WebsearchBackend::Serpapi
    }
    fn catalog_contract(&self, current_year: i32) -> ProviderCatalogContract {
        let properties = match self.engine {
            SerpApiEngine::Google => {
                json!({"query":{"type":"string"},"country":{"type":"string","minLength":2,"maxLength":2},"language":{"type":"string","maxLength":32},"pageno":{"type":"integer","minimum":1,"maximum":10},"time_range":{"type":"string","enum":["day","month","year"]},"safesearch":{"type":"integer","minimum":0,"maximum":2},"limit":{"type":"integer","minimum":1,"maximum":20},"timeoutSec":{"type":"integer","minimum":1,"maximum":60}})
            }
            SerpApiEngine::Bing => {
                json!({"query":{"type":"string"},"country":{"type":"string","minLength":2,"maxLength":2},"pageno":{"type":"integer","minimum":1,"maximum":10},"safesearch":{"type":"integer","minimum":0,"maximum":2},"limit":{"type":"integer","minimum":1,"maximum":25},"timeoutSec":{"type":"integer","minimum":1,"maximum":60}})
            }
        };
        ProviderCatalogContract {
            description: format!(
                "Search the web using SerpApi's {} engine. The current year is {current_year}.",
                self.engine.as_str()
            ),
            properties: properties.as_object().expect("SerpApi properties").clone(),
        }
    }
    fn validate_input(&self, input: &WebsearchInput) -> Result<(), String> {
        if input.categories.is_some() {
            return Err("Invalid arguments: categories is not supported by SerpApi".to_owned());
        }
        if self.engine == SerpApiEngine::Bing
            && (input.language.is_some() || input.time_range.is_some())
        {
            return Err(
                "Invalid arguments: language and time_range are not supported by SerpApi Bing"
                    .to_owned(),
            );
        }
        if input.country.as_deref().is_some_and(|value| {
            value.len() != 2 || !value.bytes().all(|byte| byte.is_ascii_alphabetic())
        }) {
            return Err("Invalid arguments: country must be a two-letter code".to_owned());
        }
        if input
            .language
            .as_deref()
            .is_some_and(|value| value.is_empty() || value.len() > 32 || !value.is_ascii())
        {
            return Err(
                "Invalid arguments: language must be a short ASCII language code".to_owned(),
            );
        }
        if input.pageno.is_some_and(|value| value > 10) {
            return Err("Invalid arguments: SerpApi pageno must not exceed 10".to_owned());
        }
        let max = if self.engine == SerpApiEngine::Google {
            20
        } else {
            25
        };
        if input.limit.is_some_and(|v| v > max) {
            return Err(format!(
                "Invalid arguments: SerpApi {} limit must not exceed {max}",
                self.engine.as_str()
            ));
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
        let max = if self.engine == SerpApiEngine::Google {
            20
        } else {
            25
        };
        let result_limit = limit(input).min(max);
        let url = lower_url(
            input,
            query,
            result_limit,
            self.engine,
            self.api_key.expose(),
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::USER_AGENT,
            HeaderValue::from_static(USER_AGENT),
        );
        headers.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        let seconds = timeout(input);
        let response = dependencies
            .http
            .execute(WebHttpRequest {
                kind: WebHttpRequestKind::PinnedProvider,
                method: Method::GET,
                url,
                headers,
                body: None,
                timeout: Duration::from_secs(seconds),
                max_redirects: 0,
                max_body_bytes: MAX_RESPONSE_BYTES,
                cancellation,
            })
            .await
            .map_err(|e| match transport_error(e, seconds) {
                ProviderError::Cancelled => ProviderError::Cancelled,
                ProviderError::Message(m) => {
                    ProviderError::Message(format!("Failed to search SerpApi: {m}"))
                }
            })?;
        if !response.status.is_success() {
            return Err(ProviderError::Message(format!(
                "Failed to search SerpApi: SerpApi returned HTTP {}.",
                response.status.as_u16()
            )));
        }
        if response.truncated {
            return Err(ProviderError::Message(
                "Failed to search SerpApi: SerpApi response exceeded the size limit.".to_owned(),
            ));
        }
        let data: Value = serde_json::from_slice(&response.body).map_err(|_| {
            ProviderError::Message(
                "Failed to search SerpApi: SerpApi response was not valid JSON.".to_owned(),
            )
        })?;
        if data
            .get("error")
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .is_some()
        {
            return Err(ProviderError::Message(
                "Failed to search SerpApi: SerpApi reported a search error.".to_owned(),
            ));
        }
        let engine = format!("serpapi-{}", self.engine.as_str());
        let results = serpapi_results(&data, MAX_NORMALIZED_ROWS, &engine);
        Ok(ProviderResults {
            backend: WebsearchBackend::Serpapi,
            query: collapse_whitespace_bounded(query, MAX_QUERY_CHARS),
            results_found: results.len(),
            results,
            limit: result_limit,
        })
    }
}

fn lower_url(
    input: &WebsearchInput,
    query: &str,
    limit: usize,
    engine: SerpApiEngine,
    key: &str,
) -> Url {
    let mut url = Url::parse(ENDPOINT).expect("fixed SerpApi endpoint");
    let mut p = url.query_pairs_mut();
    p.append_pair("api_key", key);
    p.append_pair("engine", engine.as_str());
    p.append_pair("q", query);
    match engine {
        SerpApiEngine::Google => {
            p.append_pair("num", &limit.to_string());
            if let Some(v) = &input.country {
                p.append_pair("gl", v);
            }
            if let Some(v) = &input.language {
                p.append_pair("hl", v);
            }
            if let Some(v) = input.pageno {
                p.append_pair("start", &((v - 1) * limit as u64).to_string());
            }
            if let Some(v) = input.time_range {
                p.append_pair(
                    "tbs",
                    match v {
                        TimeRange::Day => "qdr:d",
                        TimeRange::Month => "qdr:m",
                        TimeRange::Year => "qdr:y",
                    },
                );
            }
            if let Some(v) = input.safesearch {
                p.append_pair("safe", if v == 0 { "off" } else { "active" });
            }
        }
        SerpApiEngine::Bing => {
            if let Some(v) = &input.country {
                p.append_pair("cc", v);
            }
            if let Some(v) = input.pageno {
                p.append_pair("first", &(((v - 1) * 10) + 1).to_string());
            }
            if let Some(v) = input.safesearch {
                p.append_pair(
                    "safeSearch",
                    match v {
                        0 => "Off",
                        1 => "Moderate",
                        _ => "Strict",
                    },
                );
            }
        }
    }
    drop(p);
    url
}

fn serpapi_results(data: &Value, budget: usize, engine: &str) -> Vec<WebsearchResult> {
    let mut seen = HashSet::new();
    data.get("organic_results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(budget)
        .filter_map(|v| {
            let r = v.as_object()?;
            normalized_basic_result(
                r.get("title")?.as_str()?,
                r.get("link")?.as_str()?,
                r.get("snippet").and_then(Value::as_str).unwrap_or_default(),
                Some(engine.to_owned()),
                &mut seen,
            )
        })
        .collect()
}
