use std::fmt;
use std::sync::Arc;

use http::HeaderValue;
use url::Url;

use crate::search::provider::WebsearchProvider;
use crate::search::{
    BraveProvider, ExaMcpProvider, ExaProvider, KagiProvider, SearxngProvider, SerpApiProvider,
};

/// Search provider represented in normalized output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WebsearchBackend {
    Searxng,
    Exa,
    #[serde(rename = "exa-mcp")]
    ExaMcp,
    Brave,
    Kagi,
    Serpapi,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SerpApiEngine {
    Google,
    Bing,
}

impl SerpApiEngine {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Bing => "bing",
        }
    }
}

impl WebsearchBackend {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Searxng => "searxng",
            Self::Exa => "exa",
            Self::ExaMcp => "exa-mcp",
            Self::Brave => "brave",
            Self::Kagi => "kagi",
            Self::Serpapi => "serpapi",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebsearchConfigurationIssue {
    MissingBackend,
    InvalidBackend,
    InvalidUnicode,
    MissingSearxngEndpoint,
    InvalidSearxngEndpoint,
    IncompleteBasicAuthentication,
    AmbiguousSearxngCredentials,
    MissingExaApiKey,
    MissingBraveApiKey,
    MissingKagiApiKey,
    MissingSerpApiKey,
    MissingSerpApiEngine,
    InvalidSerpApiEngine,
    InvalidCredential,
}

impl WebsearchConfigurationIssue {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingBackend => "missing-backend",
            Self::InvalidBackend => "invalid-backend",
            Self::InvalidUnicode => "invalid-unicode",
            Self::MissingSearxngEndpoint => "missing-searxng-endpoint",
            Self::InvalidSearxngEndpoint => "invalid-searxng-endpoint",
            Self::IncompleteBasicAuthentication => "incomplete-basic-authentication",
            Self::AmbiguousSearxngCredentials => "ambiguous-searxng-credentials",
            Self::MissingExaApiKey => "missing-exa-api-key",
            Self::MissingBraveApiKey => "missing-brave-api-key",
            Self::MissingKagiApiKey => "missing-kagi-api-key",
            Self::MissingSerpApiKey => "missing-serpapi-api-key",
            Self::MissingSerpApiEngine => "missing-serpapi-engine",
            Self::InvalidSerpApiEngine => "invalid-serpapi-engine",
            Self::InvalidCredential => "invalid-credential",
        }
    }

    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::MissingBackend => {
                "WORKCELL_WEBSEARCH_BACKEND is not configured. Use exa-mcp for credential-free hosted search, or configure searxng, exa, brave, kagi, or serpapi."
            }
            Self::InvalidBackend => {
                "WORKCELL_WEBSEARCH_BACKEND must be disabled, exa-mcp, searxng, exa, brave, kagi, or serpapi."
            }
            Self::InvalidUnicode => {
                "The selected websearch environment variables must contain valid UTF-8."
            }
            Self::MissingSearxngEndpoint => {
                "SEARXNG_URL is not configured. Set it on the Workcell server environment to enable websearch."
            }
            Self::InvalidSearxngEndpoint => {
                "SEARXNG_URL must be a valid HTTP(S) URL without embedded credentials, and must use HTTPS when credentials are configured."
            }
            Self::IncompleteBasicAuthentication => {
                "SEARXNG_USER and SEARXNG_PASSWORD must be configured together."
            }
            Self::AmbiguousSearxngCredentials => {
                "Configure only one SearXNG credential mode: API key, bearer token, or basic authentication."
            }
            Self::MissingExaApiKey => "EXA_API_KEY is not configured for the selected Exa backend.",
            Self::MissingBraveApiKey => {
                "BRAVE_API_KEY is not configured for the selected Brave backend."
            }
            Self::MissingKagiApiKey => {
                "KAGI_API_KEY is not configured for the selected Kagi backend."
            }
            Self::MissingSerpApiKey => {
                "SERPAPI_API_KEY is not configured for the selected SerpApi backend."
            }
            Self::MissingSerpApiEngine => {
                "SERPAPI_ENGINE is not configured for the selected SerpApi backend. Set it to google or bing."
            }
            Self::InvalidSerpApiEngine => "SERPAPI_ENGINE must be google or bing.",
            Self::InvalidCredential => {
                "The selected websearch credential contains characters that are not valid in an HTTP authentication header."
            }
        }
    }
}

#[derive(Clone)]
pub(crate) enum Credential {
    ApiKey(Secret),
    Bearer(Secret),
    Basic { username: Secret, password: Secret },
}

#[derive(Clone)]
pub(crate) struct Secret(String);

impl Secret {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(_) => formatter.write_str("ApiKey([REDACTED])"),
            Self::Bearer(_) => formatter.write_str("Bearer([REDACTED])"),
            Self::Basic { .. } => formatter.write_str("Basic([REDACTED])"),
        }
    }
}

#[derive(Clone)]
pub(crate) enum ConfigurationState {
    Unavailable {
        backend: Option<WebsearchBackend>,
        issue: WebsearchConfigurationIssue,
    },
    Disabled(WebsearchBackend),
    Ready(Arc<dyn WebsearchProvider>),
}

impl fmt::Debug for ConfigurationState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { backend, issue } => formatter
                .debug_struct("Unavailable")
                .field("backend", backend)
                .field("issue", issue)
                .finish(),
            Self::Disabled(backend) => formatter.debug_tuple("Disabled").field(backend).finish(),
            Self::Ready(provider) => formatter.debug_tuple("Ready").field(provider).finish(),
        }
    }
}

/// Immutable, cloneable provider selection. Secret fields are private and
/// redacted from `Debug`; construction is the only way to set configuration.
#[derive(Clone)]
pub struct WebsearchExecutionConfiguration(Arc<ConfigurationState>);

impl Default for WebsearchExecutionConfiguration {
    fn default() -> Self {
        Self::exa_mcp()
    }
}

impl fmt::Debug for WebsearchExecutionConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("WebsearchExecutionConfiguration")
            .field(&self.0)
            .finish()
    }
}

impl WebsearchExecutionConfiguration {
    #[must_use]
    pub fn backend(&self) -> Option<WebsearchBackend> {
        match self.state() {
            ConfigurationState::Unavailable { backend, .. } => *backend,
            ConfigurationState::Disabled(backend) => Some(*backend),
            ConfigurationState::Ready(provider) => Some(provider.backend()),
        }
    }

    #[must_use]
    pub fn status(&self) -> &'static str {
        match self.state() {
            ConfigurationState::Ready(_) => "ready",
            ConfigurationState::Disabled(_) => "disabled",
            ConfigurationState::Unavailable { .. } => "unavailable",
        }
    }

    #[must_use]
    pub fn issue(&self) -> Option<WebsearchConfigurationIssue> {
        match self.state() {
            ConfigurationState::Unavailable { issue, .. } => Some(*issue),
            ConfigurationState::Disabled(_) | ConfigurationState::Ready(_) => None,
        }
    }

    #[must_use]
    pub fn unconfigured() -> Self {
        Self::unavailable(None, WebsearchConfigurationIssue::MissingBackend)
    }

    #[must_use]
    pub fn unavailable(
        backend: Option<WebsearchBackend>,
        issue: WebsearchConfigurationIssue,
    ) -> Self {
        Self(Arc::new(ConfigurationState::Unavailable { backend, issue }))
    }

    #[must_use]
    pub fn disabled(backend: WebsearchBackend) -> Self {
        Self(Arc::new(ConfigurationState::Disabled(backend)))
    }

    #[must_use]
    pub fn searxng(endpoint: impl Into<String>) -> Self {
        Self::searxng_with_credential(endpoint, None)
    }

    #[must_use]
    pub fn searxng_api_key(endpoint: impl Into<String>, key: impl Into<String>) -> Self {
        Self::searxng_with_credential(endpoint, Some(Credential::ApiKey(Secret(key.into()))))
    }

    #[must_use]
    pub fn searxng_bearer(endpoint: impl Into<String>, token: impl Into<String>) -> Self {
        Self::searxng_with_credential(endpoint, Some(Credential::Bearer(Secret(token.into()))))
    }

    #[must_use]
    pub fn searxng_basic(
        endpoint: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self::searxng_with_credential(
            endpoint,
            Some(Credential::Basic {
                username: Secret(username.into()),
                password: Secret(password.into()),
            }),
        )
    }

    #[must_use]
    pub fn exa(api_key: impl Into<String>) -> Self {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Self::exa_without_api_key();
        }
        if HeaderValue::from_str(&api_key).is_err() {
            return Self::unavailable(
                Some(WebsearchBackend::Exa),
                WebsearchConfigurationIssue::InvalidCredential,
            );
        }
        Self(Arc::new(ConfigurationState::Ready(Arc::new(
            ExaProvider::new(Secret(api_key)),
        ))))
    }

    /// Use Exa's credential-free hosted MCP search endpoint.
    #[must_use]
    pub fn exa_mcp() -> Self {
        Self(Arc::new(ConfigurationState::Ready(Arc::new(
            ExaMcpProvider,
        ))))
    }

    /// Represents an Exa slot selected without a stored key.
    #[must_use]
    pub fn exa_without_api_key() -> Self {
        Self::unavailable(
            Some(WebsearchBackend::Exa),
            WebsearchConfigurationIssue::MissingExaApiKey,
        )
    }

    #[must_use]
    pub fn brave(api_key: impl Into<String>) -> Self {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Self::brave_without_api_key();
        }
        if HeaderValue::from_str(&api_key).is_err() {
            return Self::unavailable(
                Some(WebsearchBackend::Brave),
                WebsearchConfigurationIssue::InvalidCredential,
            );
        }
        Self(Arc::new(ConfigurationState::Ready(Arc::new(
            BraveProvider::new(Secret(api_key)),
        ))))
    }

    #[must_use]
    pub fn brave_without_api_key() -> Self {
        Self::unavailable(
            Some(WebsearchBackend::Brave),
            WebsearchConfigurationIssue::MissingBraveApiKey,
        )
    }

    #[must_use]
    pub fn kagi(api_key: impl Into<String>) -> Self {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Self::kagi_without_api_key();
        }
        if HeaderValue::from_str(&format!("Bearer {api_key}")).is_err() {
            return Self::unavailable(
                Some(WebsearchBackend::Kagi),
                WebsearchConfigurationIssue::InvalidCredential,
            );
        }
        Self(Arc::new(ConfigurationState::Ready(Arc::new(
            KagiProvider::new(Secret(api_key)),
        ))))
    }

    #[must_use]
    pub fn kagi_without_api_key() -> Self {
        Self::unavailable(
            Some(WebsearchBackend::Kagi),
            WebsearchConfigurationIssue::MissingKagiApiKey,
        )
    }

    #[must_use]
    pub fn serpapi(api_key: impl Into<String>, engine: SerpApiEngine) -> Self {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Self::serpapi_without_api_key();
        }
        Self(Arc::new(ConfigurationState::Ready(Arc::new(
            SerpApiProvider::new(Secret(api_key), engine),
        ))))
    }

    #[must_use]
    pub fn serpapi_without_api_key() -> Self {
        Self::unavailable(
            Some(WebsearchBackend::Serpapi),
            WebsearchConfigurationIssue::MissingSerpApiKey,
        )
    }

    #[must_use]
    pub fn serpapi_without_engine() -> Self {
        Self::unavailable(
            Some(WebsearchBackend::Serpapi),
            WebsearchConfigurationIssue::MissingSerpApiEngine,
        )
    }

    pub(crate) fn provider(&self) -> Option<&dyn WebsearchProvider> {
        match self.state() {
            ConfigurationState::Ready(provider) => Some(provider.as_ref()),
            ConfigurationState::Disabled(_) | ConfigurationState::Unavailable { .. } => None,
        }
    }

    pub(crate) fn state(&self) -> &ConfigurationState {
        &self.0
    }

    fn searxng_with_credential(
        endpoint: impl Into<String>,
        credential: Option<Credential>,
    ) -> Self {
        let endpoint = endpoint.into();
        if !valid_credential(&credential) {
            return Self::unavailable(
                Some(WebsearchBackend::Searxng),
                WebsearchConfigurationIssue::InvalidCredential,
            );
        }
        if !valid_searxng_endpoint(&endpoint, credential.is_some()) {
            return Self::unavailable(
                Some(WebsearchBackend::Searxng),
                WebsearchConfigurationIssue::InvalidSearxngEndpoint,
            );
        }
        Self(Arc::new(ConfigurationState::Ready(Arc::new(
            SearxngProvider::new(endpoint, credential),
        ))))
    }
}

fn valid_credential(credential: &Option<Credential>) -> bool {
    match credential {
        Some(Credential::ApiKey(key)) => HeaderValue::from_str(key.expose()).is_ok(),
        Some(Credential::Bearer(token)) => {
            HeaderValue::from_str(&format!("Bearer {}", token.expose())).is_ok()
        }
        Some(Credential::Basic { .. }) | None => true,
    }
}

fn valid_searxng_endpoint(endpoint: &str, has_credentials: bool) -> bool {
    let Ok(url) = Url::parse(endpoint.trim()) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https")
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && !endpoint.split_once("://").is_some_and(|(_, authority)| {
            authority
                .split('/')
                .next()
                .is_some_and(|part| part.contains('@'))
        })
        && (!has_credentials || url.scheme() == "https")
}
