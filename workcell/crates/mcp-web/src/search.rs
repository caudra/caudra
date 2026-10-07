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

use tokio_util::sync::CancellationToken;

use crate::config::ConfigurationState;
use crate::search::common::ProviderError;
use crate::types::{WebsearchInput, WebsearchOutput};
use crate::{WebToolDependencies, WebsearchExecutionConfiguration};

pub(crate) use exa::ExaProvider;
pub(crate) use exa_mcp::ExaMcpProvider;
pub(crate) use kagi::KagiProvider;
pub(crate) use searxng::SearxngProvider;
pub(crate) use serpapi::SerpApiProvider;

pub(crate) async fn execute(
    input: WebsearchInput,
    configuration: &WebsearchExecutionConfiguration,
    dependencies: &WebToolDependencies,
    cancellation: CancellationToken,
) -> Result<WebsearchOutput, String> {
    let query = input.query.trim().to_owned();
    let backend = configured_backend(configuration.state());
    if query.is_empty() {
        return Ok(output::error(
            backend,
            query,
            "Search query must not be empty.",
        ));
    }

    let provider = match configuration.state() {
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
        ConfigurationState::Ready(provider) => provider,
    };
    let provider_result = provider
        .search(&input, &query, dependencies, cancellation.clone())
        .await;

    match provider_result {
        Ok(provider) => {
            let results = provider.results.into_iter().take(provider.limit).collect();
            let results = icons::enrich(results, dependencies, cancellation)
                .await
                .map_err(|_| "Tool invocation was aborted.".to_owned())?;
            Ok(output::success(
                provider.backend,
                provider.query,
                provider.results_found,
                results,
            ))
        }
        Err(ProviderError::Message(message)) => Ok(output::error(backend, query, &message)),
        Err(ProviderError::Cancelled) => Err("Tool invocation was aborted.".to_owned()),
    }
}

fn configured_backend(state: &ConfigurationState) -> Option<crate::WebsearchBackend> {
    match state {
        ConfigurationState::Unavailable { backend, .. } => *backend,
        ConfigurationState::Disabled(backend) => Some(*backend),
        ConfigurationState::Ready(provider) => Some(provider.backend()),
    }
}
pub(crate) use brave::BraveProvider;
