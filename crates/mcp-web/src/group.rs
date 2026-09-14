use std::{
    mem::size_of,
    sync::{Arc, RwLock},
};

use http::{HeaderMap, Method};
#[cfg(feature = "mcp")]
use rmcp::model::{CallToolResult, ContentBlock, Tool};
#[cfg(feature = "mcp")]
use serde::{Serialize, de::DeserializeOwned};
#[cfg(feature = "mcp")]
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;
use workcell_net::UrlPolicy;

#[cfg(feature = "mcp")]
use crate::catalog;
use crate::fetch::{self, WebfetchError};
use crate::search;
use crate::types::{
    WebExecution, WebfetchFormat, WebfetchInput, WebfetchOutput, WebfetchPdfMode, WebsearchInput,
    WebsearchOutput,
};
use crate::{
    ProxyConfiguration, WebToolDependencies, WebsearchBackend, WebsearchExecutionConfiguration,
};

pub struct PreparedWebsearch {
    pub permission_query: String,
    pub backend: Option<WebsearchBackend>,
    input: WebsearchInput,
    configuration: WebsearchExecutionConfiguration,
    configuration_source: WebsearchConfigurationSource,
    configuration_revision: u64,
}

pub struct PreparedWebfetch {
    pub permission_url: String,
    pub format: WebfetchFormat,
    pub pdf_mode: WebfetchPdfMode,
    pub timeout_seconds: u64,
    input: fetch::NormalizedWebfetchInput,
}

impl PreparedWebsearch {
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.permission_query.capacity())
            .saturating_add(self.input.query.capacity())
            .saturating_add(self.input.country.as_ref().map_or(0, String::capacity))
            .saturating_add(self.input.categories.as_ref().map_or(0, String::capacity))
            .saturating_add(self.input.language.as_ref().map_or(0, String::capacity))
    }
}

impl PreparedWebfetch {
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        const HEADER_ENTRY_OVERHEAD: usize = 128;

        size_of::<Self>()
            .saturating_add(self.permission_url.capacity())
            .saturating_add(self.input.url.as_str().len().saturating_mul(2))
            .saturating_add(
                self.input
                    .headers
                    .iter()
                    .map(|(name, value)| {
                        name.as_str()
                            .len()
                            .saturating_add(value.as_bytes().len())
                            .saturating_add(HEADER_ENTRY_OVERHEAD)
                    })
                    .fold(0, usize::saturating_add),
            )
    }
}

pub struct PreparedWebsearchOperation {
    prepared: PreparedWebsearch,
    configuration: Arc<RwLock<WebsearchConfigurationState>>,
}

impl PreparedWebsearchOperation {
    /// Conservative retained bytes; shared process configuration is excluded.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>().saturating_add(self.prepared.retained_bytes())
    }

    #[must_use]
    pub fn query(&self) -> &str {
        &self.prepared.input.query
    }

    #[must_use]
    pub fn permission_query(&self) -> &str {
        &self.prepared.permission_query
    }

    #[must_use]
    pub const fn input(&self) -> &WebsearchInput {
        &self.prepared.input
    }

    #[must_use]
    pub const fn backend(&self) -> Option<WebsearchBackend> {
        self.prepared.backend
    }

    #[must_use]
    pub const fn configuration_source(&self) -> WebsearchConfigurationSource {
        self.prepared.configuration_source
    }

    #[must_use]
    pub const fn configuration_revision(&self) -> u64 {
        self.prepared.configuration_revision
    }
}

pub struct PreparedWebfetchOperation {
    prepared: PreparedWebfetch,
    configuration: Arc<RwLock<WebsearchConfigurationState>>,
}

impl PreparedWebfetchOperation {
    /// Conservative retained bytes; shared process configuration is excluded.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>().saturating_add(self.prepared.retained_bytes())
    }

    #[must_use]
    pub fn url(&self) -> &Url {
        &self.prepared.input.url
    }

    #[must_use]
    pub const fn format(&self) -> WebfetchFormat {
        self.prepared.input.format
    }

    #[must_use]
    pub const fn pdf_mode(&self) -> WebfetchPdfMode {
        self.prepared.input.pdf_mode
    }

    #[must_use]
    pub const fn timeout_seconds(&self) -> u64 {
        self.prepared.input.timeout_seconds
    }

    #[must_use]
    pub const fn url_policy(&self) -> UrlPolicy {
        self.prepared.input.policy
    }

    #[must_use]
    pub const fn request_kind(&self) -> crate::WebHttpRequestKind {
        self.prepared.input.request_kind
    }

    #[must_use]
    pub fn method(&self) -> &Method {
        &self.prepared.input.method
    }

    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        &self.prepared.input.headers
    }

    #[must_use]
    pub const fn max_redirects(&self) -> usize {
        self.prepared.input.max_redirects
    }

    #[must_use]
    pub const fn max_body_bytes(&self) -> usize {
        self.prepared.input.max_body_bytes
    }
}

/// An exact prepared operation that cannot be duplicated before consuming execution.
///
/// ```compile_fail
/// use workcell_mcp_web::PreparedWebOperation;
///
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<PreparedWebOperation>();
/// ```
pub enum PreparedWebOperation {
    Websearch(PreparedWebsearchOperation),
    Webfetch(PreparedWebfetchOperation),
}

impl PreparedWebOperation {
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        match self {
            Self::Websearch(prepared) => prepared.retained_bytes(),
            Self::Webfetch(prepared) => prepared.retained_bytes(),
        }
    }
}

#[derive(Debug)]
pub enum WebOperationExecution {
    Websearch(WebExecution<WebsearchOutput>),
    Webfetch(WebExecution<WebfetchOutput>),
}

#[derive(Debug, thiserror::Error)]
pub enum WebOperationError {
    #[error("Prepared web operation belongs to a different web tool group")]
    GroupMismatch,
    #[error("{0}")]
    Websearch(String),
    #[error(transparent)]
    Webfetch(#[from] WebfetchError),
}

pub const STALE_WEBSEARCH_CONFIGURATION_ERROR: &str =
    "Prepared websearch is stale because the search configuration revision changed";

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
        Self::production_with_proxy(
            configuration,
            source_icons_enabled,
            &ProxyConfiguration::direct(),
        )
    }

    /// Construct production dependencies that route egress through `proxy`.
    #[must_use]
    pub fn production_with_proxy(
        configuration: WebsearchExecutionConfiguration,
        source_icons_enabled: bool,
        proxy: &ProxyConfiguration,
    ) -> Self {
        Self {
            configuration: Arc::new(RwLock::new(WebsearchConfigurationState {
                fallback: configuration.clone(),
                current: configuration,
                source: WebsearchConfigurationSource::Environment,
                revision: 0,
            })),
            dependencies: WebToolDependencies::production_with_proxy(source_icons_enabled, proxy),
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
    #[cfg(feature = "mcp")]
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

    /// Validate and normalize a search request without performing network I/O.
    pub fn prepare_websearch(&self, input: WebsearchInput) -> Result<PreparedWebsearch, String> {
        let snapshot = self.snapshot();
        let mut input = validate_websearch(input, &snapshot.configuration)?;
        input.query = input.query.trim().to_owned();
        Ok(PreparedWebsearch {
            permission_query: input.query.clone(),
            backend: snapshot.configuration.backend(),
            input,
            configuration: snapshot.configuration,
            configuration_source: snapshot.source,
            configuration_revision: snapshot.revision,
        })
    }

    pub fn prepare_websearch_operation(
        &self,
        input: WebsearchInput,
    ) -> Result<PreparedWebOperation, String> {
        self.prepare_websearch(input).map(|prepared| {
            PreparedWebOperation::Websearch(PreparedWebsearchOperation {
                prepared,
                configuration: self.configuration.clone(),
            })
        })
    }

    pub async fn execute_websearch(
        &self,
        prepared: PreparedWebsearch,
        cancellation: CancellationToken,
    ) -> Result<WebExecution<WebsearchOutput>, String> {
        if self.snapshot().revision != prepared.configuration_revision {
            return Err(STALE_WEBSEARCH_CONFIGURATION_ERROR.to_owned());
        }
        let output = search::execute(
            prepared.input,
            &prepared.configuration,
            &self.dependencies,
            cancellation,
        )
        .await?;
        Ok(WebExecution {
            model_text: output.formatted_results.clone(),
            output,
        })
    }

    pub async fn websearch(
        &self,
        input: WebsearchInput,
        cancellation: CancellationToken,
    ) -> Result<WebExecution<WebsearchOutput>, String> {
        self.execute_websearch(self.prepare_websearch(input)?, cancellation)
            .await
    }

    /// Validate and normalize a fetch request without performing network I/O.
    pub fn prepare_webfetch(
        &self,
        input: WebfetchInput,
    ) -> Result<PreparedWebfetch, WebfetchError> {
        let input = validate_webfetch(input).map_err(WebfetchError::InvalidInput)?;
        let input = fetch::normalize_input(input, self.dependencies.webfetch_policy)?;
        Ok(PreparedWebfetch {
            permission_url: input.url.to_string(),
            format: input.format,
            pdf_mode: input.pdf_mode,
            timeout_seconds: input.timeout_seconds,
            input,
        })
    }

    pub fn prepare_webfetch_operation(
        &self,
        input: WebfetchInput,
    ) -> Result<PreparedWebOperation, WebfetchError> {
        self.prepare_webfetch(input).map(|prepared| {
            PreparedWebOperation::Webfetch(PreparedWebfetchOperation {
                prepared,
                configuration: self.configuration.clone(),
            })
        })
    }

    pub async fn execute_webfetch(
        &self,
        prepared: PreparedWebfetch,
        cancellation: CancellationToken,
    ) -> Result<WebExecution<WebfetchOutput>, WebfetchError> {
        let execution = fetch::execute(prepared.input, &self.dependencies, cancellation).await?;
        Ok(WebExecution {
            output: execution.output,
            model_text: execution.model_text,
        })
    }

    pub async fn webfetch(
        &self,
        input: WebfetchInput,
        cancellation: CancellationToken,
    ) -> Result<WebExecution<WebfetchOutput>, WebfetchError> {
        self.execute_webfetch(self.prepare_webfetch(input)?, cancellation)
            .await
    }

    /// Execute one exact typed operation. The prepared value is deliberately consumed.
    ///
    /// ```compile_fail
    /// use tokio_util::sync::CancellationToken;
    /// use workcell_mcp_web::{PreparedWebOperation, WebToolGroup};
    ///
    /// async fn execute_twice(group: &WebToolGroup, prepared: PreparedWebOperation) {
    ///     let _ = group.execute_prepared(prepared, CancellationToken::new()).await;
    ///     let _ = group.execute_prepared(prepared, CancellationToken::new()).await;
    /// }
    /// ```
    pub async fn execute_prepared(
        &self,
        prepared: PreparedWebOperation,
        cancellation: CancellationToken,
    ) -> Result<WebOperationExecution, WebOperationError> {
        match prepared {
            PreparedWebOperation::Websearch(operation) => {
                if !Arc::ptr_eq(&operation.configuration, &self.configuration) {
                    return Err(WebOperationError::GroupMismatch);
                }
                self.execute_websearch(operation.prepared, cancellation)
                    .await
                    .map(WebOperationExecution::Websearch)
                    .map_err(WebOperationError::Websearch)
            }
            PreparedWebOperation::Webfetch(operation) => {
                if !Arc::ptr_eq(&operation.configuration, &self.configuration) {
                    return Err(WebOperationError::GroupMismatch);
                }
                self.execute_webfetch(operation.prepared, cancellation)
                    .await
                    .map(WebOperationExecution::Webfetch)
                    .map_err(WebOperationError::Webfetch)
            }
        }
    }

    /// Returns `None` only for names outside this group. Invalid arguments and
    /// operational webfetch failures are MCP tool errors; websearch provider
    /// failures intentionally remain successful error-shaped results.
    #[cfg(feature = "mcp")]
    pub async fn dispatch(
        &self,
        name: &str,
        arguments: Value,
        cancellation: CancellationToken,
    ) -> Option<Result<CallToolResult, rmcp::ErrorData>> {
        let result = match name {
            "websearch" => match parse_arguments::<WebsearchInput>(name, arguments) {
                Ok(input) => match self.websearch(input, cancellation).await {
                    Ok(execution) => success(&execution.model_text, execution.output),
                    Err(error) => tool_error(error),
                },
                Err(error) => tool_error(error),
            },
            "webfetch" => match parse_arguments::<WebfetchInput>(name, arguments) {
                Ok(input) => match self.webfetch(input, cancellation).await {
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

#[cfg(feature = "mcp")]
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

#[cfg(feature = "mcp")]
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

#[cfg(feature = "mcp")]
fn tool_error(error: impl IntoToolError) -> Result<CallToolResult, rmcp::ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(
        error.tool_error(),
    )]))
}

#[cfg(feature = "mcp")]
trait IntoToolError {
    fn tool_error(self) -> String;
}

#[cfg(feature = "mcp")]
impl IntoToolError for String {
    fn tool_error(self) -> String {
        self
    }
}

#[cfg(feature = "mcp")]
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
