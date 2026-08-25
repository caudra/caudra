use std::sync::{Arc, RwLock};

use rmcp::model::{CallToolResult, ContentBlock, Tool};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::catalog;
use crate::fetch::{self, WebfetchError};
use crate::search;
use crate::types::{WebfetchInput, WebsearchInput};
use crate::{WebToolDependencies, WebsearchExecutionConfiguration};

/// Cloneable composition unit for the MCP server. Clones share immutable
/// configuration and dependency handles but no mutable invocation state.
#[derive(Clone)]
pub struct WebToolGroup {
    configuration: Arc<RwLock<WebsearchConfigurationState>>,
    dependencies: WebToolDependencies,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebsearchConfigurationSource {
    Environment,
    Control,
}

#[derive(Clone)]
struct WebsearchConfigurationState {
    // Every session in this process shares one controlled configuration. Run
    // separate Workcell processes when execution environments need isolation.
    fallback: WebsearchExecutionConfiguration,
    current: WebsearchExecutionConfiguration,
    source: WebsearchConfigurationSource,
    revision: u64,
}

#[derive(Clone, Debug)]
pub struct WebsearchConfigurationSnapshot {
    pub configuration: WebsearchExecutionConfiguration,
    pub source: WebsearchConfigurationSource,
    pub revision: u64,
}

impl WebToolGroup {
    /// Construct with production HTTP, icon, clock, and native PDF dependencies.
    #[must_use]
    pub fn new(configuration: WebsearchExecutionConfiguration) -> Self {
        Self::production(configuration)
    }

    #[must_use]
    pub fn production(configuration: WebsearchExecutionConfiguration) -> Self {
        Self::production_with_source_icons(configuration, false)
    }

    #[must_use]
    pub fn production_with_source_icons(
        configuration: WebsearchExecutionConfiguration,
        source_icons_enabled: bool,
    ) -> Self {
        Self {
            configuration: Arc::new(RwLock::new(WebsearchConfigurationState {
                fallback: configuration.clone(),
                current: configuration,
                source: WebsearchConfigurationSource::Environment,
                revision: 0,
            })),
            dependencies: WebToolDependencies::production_with_source_icons(source_icons_enabled),
        }
    }

    /// Construct with fully injected dependencies, primarily for offline tests
    /// and alternate hosts.
    #[must_use]
    pub fn with_dependencies(
        configuration: WebsearchExecutionConfiguration,
        dependencies: WebToolDependencies,
    ) -> Self {
        Self {
            configuration: Arc::new(RwLock::new(WebsearchConfigurationState {
                fallback: configuration.clone(),
                current: configuration,
                source: WebsearchConfigurationSource::Environment,
                revision: 0,
            })),
            dependencies,
        }
    }

    #[must_use]
    pub fn catalog(&self, current_year: i32) -> Vec<Tool> {
        catalog(current_year, &self.snapshot().configuration)
    }

    #[must_use]
    pub fn snapshot(&self) -> WebsearchConfigurationSnapshot {
        let state = self
            .configuration
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        WebsearchConfigurationSnapshot {
            configuration: state.current.clone(),
            source: state.source,
            revision: state.revision,
        }
    }

    pub fn replace_configuration(&self, configuration: WebsearchExecutionConfiguration) -> u64 {
        let mut state = self
            .configuration
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.current = configuration;
        state.source = WebsearchConfigurationSource::Control;
        state.revision = state.revision.saturating_add(1);
        state.revision
    }

    pub fn clear_configuration(&self) -> u64 {
        let mut state = self
            .configuration
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.current = state.fallback.clone();
        state.source = WebsearchConfigurationSource::Environment;
        state.revision = state.revision.saturating_add(1);
        state.revision
    }

    /// Returns `None` only for names outside this group. Invalid arguments and
    /// operational webfetch failures are MCP tool errors; websearch provider
    /// failures intentionally remain successful error-shaped results.
    pub async fn dispatch(
        &self,
        name: &str,
        arguments: Value,
        cancellation: CancellationToken,
    ) -> Option<Result<CallToolResult, rmcp::ErrorData>> {
        let configuration = self.snapshot().configuration;
        let result = match name {
            "websearch" => match parse_arguments::<WebsearchInput>(name, arguments)
                .and_then(|input| validate_websearch(input, &configuration))
            {
                Ok(input) => {
                    match search::execute(input, &configuration, &self.dependencies, cancellation)
                        .await
                    {
                        Ok(output) => {
                            let model_text = output.formatted_results.clone();
                            success(&model_text, output)
                        }
                        Err(error) => tool_error(error),
                    }
                }
                Err(error) => tool_error(error),
            },
            "webfetch" => match parse_arguments::<WebfetchInput>(name, arguments)
                .and_then(validate_webfetch)
                .and_then(|input| {
                    fetch::normalize_input(input, self.dependencies.webfetch_policy)
                        .map_err(|error| error.to_string())
                }) {
                Ok(input) => match fetch::execute(input, &self.dependencies, cancellation).await {
                    Ok(execution) => success(&execution.model_text, execution.output),
                    Err(error) => tool_error(error),
                },
                Err(error) => tool_error(error),
            },
            _ => return None,
        };
        Some(result)
    }
}

fn parse_arguments<T: DeserializeOwned>(name: &str, value: Value) -> Result<T, String> {
    serde_json::from_value(value)
        .map_err(|error| format!("Invalid arguments for tool {name}: {error}"))
}

fn validate_websearch(
    input: WebsearchInput,
    configuration: &WebsearchExecutionConfiguration,
) -> Result<WebsearchInput, String> {
    if input.query.trim().is_empty() {
        return Err("Invalid arguments: query must not be empty".to_owned());
    }
    if input
        .country
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err("Invalid arguments: country must not be empty".to_owned());
    }
    if input
        .categories
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err("Invalid arguments: categories must not be empty".to_owned());
    }
    if input
        .language
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err("Invalid arguments: language must not be empty".to_owned());
    }
    if input.pageno == Some(0) {
        return Err("Invalid arguments: pageno must be a positive integer".to_owned());
    }
    if input.limit == Some(0) {
        return Err("Invalid arguments: limit must be a positive integer".to_owned());
    }
    if input.timeout_sec == Some(0) {
        return Err("Invalid arguments: timeoutSec must be a positive integer".to_owned());
    }
    if input.timeout_sec.is_some_and(|value| value > 60) {
        return Err("Invalid arguments: timeoutSec must not exceed 60".to_owned());
    }
    if input.safesearch.is_some_and(|value| value > 2) {
        return Err("Invalid arguments: safesearch must be 0, 1, or 2".to_owned());
    }
    if let Some(provider) = configuration.provider() {
        provider.validate_input(&input)?;
    } else if input.country.is_some()
        || input.categories.is_some()
        || input.language.is_some()
        || input.pageno.is_some()
        || input.time_range.is_some()
        || input.safesearch.is_some()
        || input.limit.is_some()
        || input.timeout_sec.is_some()
    {
        return Err(
            "Invalid arguments: only query is accepted while websearch is unavailable".to_owned(),
        );
    }
    Ok(input)
}

fn validate_webfetch(input: WebfetchInput) -> Result<WebfetchInput, String> {
    if input.url.is_empty() {
        return Err("Invalid arguments: url must not be empty".to_owned());
    }
    if input.timeout == Some(0) {
        return Err("Invalid arguments: timeout must be a positive integer".to_owned());
    }
    Ok(input)
}

fn success(model_text: &str, output: impl Serialize) -> Result<CallToolResult, rmcp::ErrorData> {
    let structured = serde_json::to_value(output).map_err(|error| {
        rmcp::ErrorData::internal_error(
            "Failed to serialize web tool result",
            Some(Value::String(error.to_string())),
        )
    })?;
    let mut result = CallToolResult::default();
    result.content = vec![ContentBlock::text(model_text.to_owned())];
    result.structured_content = Some(structured);
    Ok(result)
}

fn tool_error(error: impl IntoToolError) -> Result<CallToolResult, rmcp::ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(
        error.tool_error(),
    )]))
}

trait IntoToolError {
    fn tool_error(self) -> String;
}

impl IntoToolError for String {
    fn tool_error(self) -> String {
        self
    }
}

impl IntoToolError for WebfetchError {
    fn tool_error(self) -> String {
        self.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WebsearchBackend;

    #[test]
    fn shared_configuration_replaces_and_clears_to_environment_fallback() {
        let group = WebToolGroup::new(WebsearchExecutionConfiguration::brave("operator-key"));
        let clone = group.clone();

        assert_eq!(
            clone.snapshot().configuration.backend(),
            Some(WebsearchBackend::Brave)
        );
        assert_eq!(
            group.replace_configuration(WebsearchExecutionConfiguration::exa("control-key")),
            1
        );
        assert_eq!(
            clone.snapshot().configuration.backend(),
            Some(WebsearchBackend::Exa)
        );
        assert_eq!(
            clone.snapshot().source,
            WebsearchConfigurationSource::Control
        );

        assert_eq!(clone.clear_configuration(), 2);
        let restored = group.snapshot();
        assert_eq!(
            restored.configuration.backend(),
            Some(WebsearchBackend::Brave)
        );
        assert_eq!(restored.source, WebsearchConfigurationSource::Environment);

        let rendered = format!("{:?}", restored.configuration);
        assert!(!rendered.contains("operator-key"));
        assert!(!rendered.contains("control-key"));
    }
}
