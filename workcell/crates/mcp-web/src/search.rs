mod brave;
mod common;
mod exa;
mod exa_mcp;
mod icons;
mod kagi;
mod normalize;
mod output;
pub(crate) mod provider;
mod searxng;
mod serpapi;

use chrono::{DateTime, Datelike, Utc};
use tokio_util::sync::CancellationToken;

use crate::config::ConfigurationState;
use crate::search::common::ProviderError;
use crate::search::provider::WebsearchProvider;
use crate::types::{WebsearchFallback, WebsearchInput, WebsearchOutput};
use crate::{WebToolDependencies, WebsearchExecutionConfiguration};

pub(crate) use exa::ExaProvider;
pub(crate) use exa_mcp::ExaMcpProvider;
pub(crate) use kagi::KagiProvider;
pub(crate) use searxng::SearxngProvider;
pub(crate) use serpapi::SerpApiProvider;

const ABORTED_ERROR: &str = "Tool invocation was aborted.";
const COUNTRY_PROPERTY: &str = "country";
const CATEGORIES_PROPERTY: &str = "categories";
const LANGUAGE_PROPERTY: &str = "language";
const PAGENO_PROPERTY: &str = "pageno";
const TIME_RANGE_PROPERTY: &str = "time_range";
const SAFESEARCH_PROPERTY: &str = "safesearch";
const LIMIT_PROPERTY: &str = "limit";
const TIMEOUT_PROPERTY: &str = "timeoutSec";

pub(crate) async fn execute(
    input: WebsearchInput,
    configuration: &WebsearchExecutionConfiguration,
    dependencies: &WebToolDependencies,
    cancellation: CancellationToken,
) -> Result<WebsearchOutput, String> {
    let query = input.query.trim().to_owned();
    let backend = configuration.backend();
    if query.is_empty() {
        return Ok(output::error(
            backend,
            query,
            "Search query must not be empty.",
        ));
    }

    match configuration.state() {
        ConfigurationState::Unavailable { backend, issue } => {
            return Ok(output::error(*backend, query, issue.message()));
        }
        ConfigurationState::Disabled(backend) => {
            return Ok(output::error(
                Some(*backend),
                query,
                "Websearch is disabled by the server configuration.",
            ));
        }
        ConfigurationState::Ready(_) | ConfigurationState::Combined { .. } => {}
    }
    let providers = configuration.providers();
    let candidates = match input.backend {
        Some(chosen) => providers
            .iter()
            .filter(|provider| provider.backend() == chosen)
            .collect::<Vec<_>>(),
        None => providers.iter().collect(),
    };
    let input = WebsearchInput {
        backend: None,
        ..input
    };
    let current_year = DateTime::<Utc>::from(dependencies.clock.now()).year();

    let mut failures = Vec::new();
    for (index, provider) in candidates.iter().enumerate() {
        let attempt = if index == 0 {
            input.clone()
        } else {
            let adapted = adapt_input(&input, provider.as_ref(), current_year);
            if let Err(error) = provider.validate_input(&adapted) {
                failures.push(WebsearchFallback {
                    backend: provider.backend(),
                    error,
                });
                continue;
            }
            adapted
        };
        match provider
            .search(&attempt, &query, dependencies, cancellation.clone())
            .await
        {
            Ok(found) => {
                let results = found.results.into_iter().take(found.limit).collect();
                let results = icons::enrich(results, dependencies, cancellation)
                    .await
                    .map_err(|_| ABORTED_ERROR.to_owned())?;
                let output =
                    output::success(found.backend, found.query, found.results_found, results);
                return Ok(output::with_fallbacks(output, failures));
            }
            Err(ProviderError::Message(error)) => failures.push(WebsearchFallback {
                backend: provider.backend(),
                error,
            }),
            Err(ProviderError::Cancelled) => return Err(ABORTED_ERROR.to_owned()),
        }
    }
    Ok(output::all_failed(backend, query, failures))
}

/// Drop the optional parameters a fallback backend does not declare, so a request
/// shaped for the primary backend still reaches it.
fn adapt_input(
    input: &WebsearchInput,
    provider: &dyn WebsearchProvider,
    current_year: i32,
) -> WebsearchInput {
    let properties = provider.catalog_contract(current_year).properties;
    let accepts = |name: &str| properties.contains_key(name);
    let input = input.clone();
    WebsearchInput {
        country: input.country.filter(|_| accepts(COUNTRY_PROPERTY)),
        categories: input.categories.filter(|_| accepts(CATEGORIES_PROPERTY)),
        language: input.language.filter(|_| accepts(LANGUAGE_PROPERTY)),
        pageno: input.pageno.filter(|_| accepts(PAGENO_PROPERTY)),
        time_range: input.time_range.filter(|_| accepts(TIME_RANGE_PROPERTY)),
        safesearch: input.safesearch.filter(|_| accepts(SAFESEARCH_PROPERTY)),
        limit: input.limit.filter(|_| accepts(LIMIT_PROPERTY)),
        timeout_sec: input.timeout_sec.filter(|_| accepts(TIMEOUT_PROPERTY)),
        ..input
    }
}

pub(crate) use brave::BraveProvider;
