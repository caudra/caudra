use std::fmt;
use std::slice;
use std::str::FromStr;
use std::sync::Arc;

use http::HeaderValue;
use url::Url;

use crate::search::provider::WebsearchProvider;
use crate::search::{
    BraveProvider, ExaMcpProvider, ExaProvider, KagiProvider, SearxngProvider, SerpApiProvider,
};

/// Search provider represented in normalized output.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, schemars::JsonSchema, serde::Deserialize, serde::Serialize,
)]
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
    pub const ALL: [Self; 6] = [
        Self::ExaMcp,
        Self::Searxng,
        Self::Exa,
        Self::Brave,
        Self::Kagi,
        Self::Serpapi,
    ];

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

impl FromStr for WebsearchBackend {
    type Err = WebsearchConfigurationIssue;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|backend| backend.as_str() == value)
            .ok_or(WebsearchConfigurationIssue::InvalidBackend)
    }
}

/// How several configured backends share one `websearch` tool.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WebsearchMode {
    /// Only the first ready backend is used.
    Single,
    /// The ready backends are tried in order until one answers.
    #[default]
    Fallback,
    /// The model picks a ready backend per call, or omits it to fall back in order.
    All,
}

impl WebsearchMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Fallback => "fallback",
            Self::All => "all",
        }
    }
}

impl FromStr for WebsearchMode {
    type Err = WebsearchConfigurationIssue;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        [Self::Single, Self::Fallback, Self::All]
            .into_iter()
            .find(|mode| mode.as_str() == value)
            .ok_or(WebsearchConfigurationIssue::InvalidMode)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebsearchConfigurationIssue {
    MissingBackend,
    InvalidBackend,
    InvalidBackendList,
    DuplicateBackend,
    InvalidMode,
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
            Self::InvalidBackendList => "invalid-backend-list",
            Self::DuplicateBackend => "duplicate-backend",
            Self::InvalidMode => "invalid-mode",
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
                "WORKCELL_WEBSEARCH_BACKEND must be disabled, or one or a comma-separated list of exa-mcp, searxng, exa, brave, kagi, and serpapi."
            }
            Self::InvalidBackendList => {
                "WORKCELL_WEBSEARCH_BACKEND must not contain empty entries, and disabled cannot be combined with other backends."
            }
            Self::DuplicateBackend => {
                "WORKCELL_WEBSEARCH_BACKEND must not list the same backend more than once."
            }
            Self::InvalidMode => "WORKCELL_WEBSEARCH_MODE must be single, fallback, or all.",
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
    /// Two or more ready providers in configured order; `mode` is never `Single`.
    Combined {
        mode: WebsearchMode,
        providers: Vec<Arc<dyn WebsearchProvider>>,
    },
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
            Self::Combined { mode, providers } => formatter
                .debug_struct("Combined")
                .field("mode", mode)
                .field("providers", providers)
                .finish(),
        }
    }
}

struct Configuration {
    state: ConfigurationState,
    skipped: Vec<(WebsearchBackend, WebsearchConfigurationIssue)>,
}

/// Immutable, cloneable provider selection. Secret fields are private and
/// redacted from `Debug`; construction is the only way to set configuration.
#[derive(Clone)]
pub struct WebsearchExecutionConfiguration(Arc<Configuration>);

impl Default for WebsearchExecutionConfiguration {
    fn default() -> Self {
        Self::exa_mcp()
    }
}

impl fmt::Debug for WebsearchExecutionConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut tuple = formatter.debug_tuple("WebsearchExecutionConfiguration");
        tuple.field(&self.0.state);
        if !self.0.skipped.is_empty() {
            tuple.field(&self.0.skipped);
        }
        tuple.finish()
    }
}

impl WebsearchExecutionConfiguration {
    /// The primary backend: the first ready one when several are combined.
    #[must_use]
    pub fn backend(&self) -> Option<WebsearchBackend> {
        match self.state() {
            ConfigurationState::Unavailable { backend, .. } => *backend,
            ConfigurationState::Disabled(backend) => Some(*backend),
            ConfigurationState::Ready(_) | ConfigurationState::Combined { .. } => {
                self.provider().map(|provider| provider.backend())
            }
        }
    }

    #[must_use]
    pub fn status(&self) -> &'static str {
        match self.state() {
            ConfigurationState::Ready(_) | ConfigurationState::Combined { .. } => "ready",
            ConfigurationState::Disabled(_) => "disabled",
            ConfigurationState::Unavailable { .. } => "unavailable",
        }
    }

    #[must_use]
    pub fn issue(&self) -> Option<WebsearchConfigurationIssue> {
        match self.state() {
            ConfigurationState::Unavailable { issue, .. } => Some(*issue),
            ConfigurationState::Disabled(_)
            | ConfigurationState::Ready(_)
            | ConfigurationState::Combined { .. } => None,
        }
    }

    /// Listed backends left out because their configuration is incomplete or invalid.
    #[must_use]
    pub fn skipped(&self) -> &[(WebsearchBackend, WebsearchConfigurationIssue)] {
        &self.0.skipped
    }

    /// Ready backends in the order they are tried; empty when search is not ready.
    #[must_use]
    pub fn ready_backends(&self) -> Vec<WebsearchBackend> {
        self.providers()
            .iter()
            .map(|provider| provider.backend())
            .collect()
    }

    /// `Single` whenever at most one backend is ready.
    #[must_use]
    pub fn mode(&self) -> WebsearchMode {
        match self.state() {
            ConfigurationState::Combined { mode, .. } => *mode,
            ConfigurationState::Unavailable { .. }
            | ConfigurationState::Disabled(_)
            | ConfigurationState::Ready(_) => WebsearchMode::Single,
        }
    }

    /// Combine separately resolved backends in order. Members that are not ready
    /// are reported by [`Self::skipped`]; when none is ready the first member's
    /// state is kept so its diagnostic still reaches the model.
    #[must_use]
    pub fn combined(mode: WebsearchMode, members: Vec<Self>) -> Self {
        let mut providers = Vec::new();
        let mut not_ready = Vec::new();
        let mut first = None;
        for member in members {
            match member.state() {
                ConfigurationState::Ready(provider) => providers.push(provider.clone()),
                ConfigurationState::Combined {
                    providers: nested, ..
                } => providers.extend(nested.iter().cloned()),
                ConfigurationState::Unavailable {
                    backend: Some(backend),
                    issue,
                } => not_ready.push((*backend, *issue)),
                ConfigurationState::Unavailable { backend: None, .. }
                | ConfigurationState::Disabled(_) => {}
            }
            not_ready.extend_from_slice(member.skipped());
            first.get_or_insert(member);
        }
        let Some(first) = first else {
            return Self::unconfigured();
        };
        if providers.is_empty() {
            let reported_by_state = usize::from(matches!(
                first.state(),
                ConfigurationState::Unavailable {
                    backend: Some(_),
                    ..
                }
            ));
            not_ready.drain(..reported_by_state);
            return Self::with_skipped(first.state().clone(), not_ready);
        }
        if mode == WebsearchMode::Single {
            providers.truncate(1);
        }
        let state = if providers.len() == 1 {
            ConfigurationState::Ready(providers.remove(0))
        } else {
            ConfigurationState::Combined { mode, providers }
        };
        Self::with_skipped(state, not_ready)
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
        Self::with_skipped(
            ConfigurationState::Unavailable { backend, issue },
            Vec::new(),
        )
    }

    #[must_use]
    pub fn disabled(backend: WebsearchBackend) -> Self {
        Self::with_skipped(ConfigurationState::Disabled(backend), Vec::new())
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
        Self::ready(ExaProvider::new(Secret(api_key)))
    }

    /// Use Exa's credential-free hosted MCP search endpoint.
    #[must_use]
    pub fn exa_mcp() -> Self {
        Self::ready(ExaMcpProvider)
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
        Self::ready(BraveProvider::new(Secret(api_key)))
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
        Self::ready(KagiProvider::new(Secret(api_key)))
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
        Self::ready(SerpApiProvider::new(Secret(api_key), engine))
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

    /// The primary provider: the first one tried when several are combined.
    pub(crate) fn provider(&self) -> Option<&dyn WebsearchProvider> {
        self.providers().first().map(AsRef::as_ref)
    }

    pub(crate) fn providers(&self) -> &[Arc<dyn WebsearchProvider>] {
        match self.state() {
            ConfigurationState::Ready(provider) => slice::from_ref(provider),
            ConfigurationState::Combined { providers, .. } => providers,
            ConfigurationState::Disabled(_) | ConfigurationState::Unavailable { .. } => &[],
        }
    }

    pub(crate) fn state(&self) -> &ConfigurationState {
        &self.0.state
    }

    fn ready(provider: impl WebsearchProvider + 'static) -> Self {
        Self::with_skipped(ConfigurationState::Ready(Arc::new(provider)), Vec::new())
    }

    fn with_skipped(
        state: ConfigurationState,
        skipped: Vec<(WebsearchBackend, WebsearchConfigurationIssue)>,
    ) -> Self {
        Self(Arc::new(Configuration { state, skipped }))
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
        Self::ready(SearxngProvider::new(endpoint, credential))
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
