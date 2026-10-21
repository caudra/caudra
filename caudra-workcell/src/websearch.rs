//! The backends the embedded `websearch` tool uses: the global `[websearch]` table first, then
//! Workcell's own variables, read through `caudra_config::global_env_value` so a project `.env`
//! never supplies one. Workcell's parser decides what each setting means.

use std::sync::Arc;

use caudra_config::websearch::{WebsearchBackendName, WebsearchConfig, WebsearchMode};
use sha2::{Digest, Sha256};
use tracing::{info, warn};
use workcell::web::{
    WEBSEARCH_BACKEND_VARIABLE, WEBSEARCH_MODE_VARIABLE, WebsearchBackend,
    WebsearchConfigurationIssue, WebsearchExecutionConfiguration, WebsearchMode as ResolvedMode,
    resolve_web_configuration_with,
};

use crate::HostInner;

const DISABLED: &str = "disabled";
const LIST_SEPARATOR: &str = ",";
const SHOWN_SEPARATOR: &str = ", ";
const SET: u8 = 1;
const UNSET: u8 = 2;
const UNREADABLE: u8 = 3;

/// What a variable reads as: `Err` for a value that is not valid UTF-8.
pub(crate) type EnvRead<'a> = &'a dyn Fn(&str) -> Result<Option<String>, ()>;

pub(crate) struct Resolution {
    pub(crate) configuration: WebsearchExecutionConfiguration,
    /// A digest of every setting read, so an unchanged one is not applied again. Values are
    /// never kept.
    fingerprint: [u8; 32],
}

/// The configuration `config` and `env` select.
pub(crate) fn resolve(config: &WebsearchConfig, env: EnvRead<'_>) -> Resolution {
    let mut hasher = Sha256::new();
    let configuration = resolve_web_configuration_with(|name| {
        let value = configured(config, name).map_or_else(|| env(name), |value| Ok(Some(value)));
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name);
        match &value {
            Ok(Some(value)) => {
                hasher.update([SET]);
                hasher.update((value.len() as u64).to_le_bytes());
                hasher.update(value);
            }
            Ok(None) => hasher.update([UNSET]),
            Err(()) => hasher.update([UNREADABLE]),
        }
        value
    });
    Resolution {
        configuration,
        fingerprint: hasher.finalize().into(),
    }
}

/// The value `[websearch]` gives Workcell's variable `name`, `None` when the table leaves it to
/// the environment.
fn configured(config: &WebsearchConfig, name: &str) -> Option<String> {
    match name {
        WEBSEARCH_BACKEND_VARIABLE => config.backends.as_ref().map(|backends| {
            if backends.is_empty() {
                DISABLED.to_owned()
            } else {
                backends
                    .iter()
                    .map(|backend| backend.as_str())
                    .collect::<Vec<_>>()
                    .join(LIST_SEPARATOR)
            }
        }),
        WEBSEARCH_MODE_VARIABLE => config.mode.map(|mode| mode.as_str().to_owned()),
        _ => WebsearchBackendName::ALL.into_iter().find_map(|backend| {
            backend
                .fields()
                .iter()
                .find(|field| field.env == Some(name))
                .and_then(|field| config.value(backend, field.name))
                .map(|value| value.expose().to_owned())
        }),
    }
}

pub(crate) fn production_env(name: &str) -> Result<Option<String>, ()> {
    caudra_config::global_env_value(name).map_err(|_| ())
}

/// Where a backend's settings come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingSource {
    Config,
    Environment,
    Unset,
}

/// Whether one backend would work if it were listed, and why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendStatus {
    pub backend: WebsearchBackendName,
    pub issue: Option<WebsearchConfigurationIssue>,
    pub source: SettingSource,
}

impl BackendStatus {
    pub fn ready(&self) -> bool {
        self.issue.is_none()
    }
}

/// What applying a `[websearch]` table left in force.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebsearchReport {
    /// Whether the settings differed from the ones in force, so the tool changed.
    pub changed: bool,
    pub mode: ResolvedMode,
    pub active: Vec<WebsearchBackend>,
    pub skipped: Vec<(WebsearchBackend, WebsearchConfigurationIssue)>,
    pub disabled: bool,
    /// Why no backend is ready, when none is.
    pub unavailable: Option<WebsearchConfigurationIssue>,
}

impl WebsearchReport {
    fn of(configuration: &WebsearchExecutionConfiguration, changed: bool) -> Self {
        Self {
            changed,
            mode: configuration.mode(),
            active: configuration.ready_backends(),
            skipped: configuration.skipped().to_vec(),
            disabled: configuration.status() == DISABLED,
            unavailable: configuration.issue(),
        }
    }

    /// One line per backend that was skipped or setting that keeps websearch from working.
    /// They name variables and keys, never a value.
    pub fn warnings(&self) -> Vec<String> {
        let unavailable = self
            .unavailable
            .map(|issue| format!("websearch is unavailable: {}", issue.message()));
        self.skipped
            .iter()
            .map(|(backend, issue)| {
                format!("websearch skips {}: {}", backend.as_str(), issue.message())
            })
            .chain(unavailable)
            .collect()
    }

    /// The backends in force, such as `searxng, exa-mcp (fallback)`.
    pub fn summary(&self) -> String {
        if self.disabled {
            return format!("websearch is {DISABLED}");
        }
        let active: Vec<&str> = self.active.iter().map(|backend| backend.as_str()).collect();
        match active.as_slice() {
            [] => "websearch has no ready backend".to_owned(),
            [only] => format!("websearch uses {only}"),
            _ => format!(
                "websearch uses {} ({})",
                active.join(SHOWN_SEPARATOR),
                self.mode.as_str()
            ),
        }
    }
}

/// Changes the backends of a running host's `websearch` tool.
#[derive(Clone)]
pub struct WebsearchControl {
    host: Arc<HostInner>,
}

impl WebsearchControl {
    pub(crate) fn new(host: Arc<HostInner>) -> Self {
        Self { host }
    }

    /// Puts `config` in force, unless every setting it reads is what is already in force: a
    /// change bumps the configuration revision, which fails a search prepared before it.
    pub fn apply(&self, config: &WebsearchConfig) -> WebsearchReport {
        self.apply_with(config, &production_env)
    }

    pub(crate) fn apply_with(&self, config: &WebsearchConfig, env: EnvRead<'_>) -> WebsearchReport {
        let resolution = resolve(config, env);
        let changed = {
            let mut fingerprint = self
                .host
                .websearch_fingerprint
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let changed = *fingerprint != Some(resolution.fingerprint);
            *fingerprint = Some(resolution.fingerprint);
            changed
        };
        if changed {
            self.host
                .web
                .replace_configuration(resolution.configuration.clone());
        }
        let report = WebsearchReport::of(&resolution.configuration, changed);
        for (backend, issue) in &report.skipped {
            warn!(
                backend = backend.as_str(),
                issue = issue.as_str(),
                "websearch backend skipped"
            );
        }
        if let Some(issue) = report.unavailable {
            warn!(issue = issue.as_str(), "websearch unavailable");
        }
        info!(
            changed,
            mode = report.mode.as_str(),
            backends = ?report.active.iter().map(|backend| backend.as_str()).collect::<Vec<_>>(),
            disabled = report.disabled,
            "websearch configured"
        );
        report
    }

    /// Every backend as `config` and the environment set it up, each resolved on its own.
    pub fn status(&self, config: &WebsearchConfig) -> Vec<BackendStatus> {
        statuses(config, &production_env)
    }
}

pub(crate) fn statuses(config: &WebsearchConfig, env: EnvRead<'_>) -> Vec<BackendStatus> {
    WebsearchBackendName::ALL
        .into_iter()
        .map(|backend| {
            let alone = WebsearchConfig {
                mode: Some(WebsearchMode::Single),
                backends: Some(vec![backend]),
                ..config.clone()
            };
            let fields = backend.fields();
            let source = if fields
                .iter()
                .any(|field| config.value(backend, field.name).is_some())
            {
                SettingSource::Config
            } else if fields.iter().filter_map(|field| field.env).any(|name| {
                env(name).is_ok_and(|value| value.is_some_and(|value| !value.trim().is_empty()))
            }) {
                SettingSource::Environment
            } else {
                SettingSource::Unset
            };
            BackendStatus {
                backend,
                issue: resolve(&alone, env).configuration.issue(),
                source,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use caudra_agent::tools::ToolRegistry;
    use caudra_config::websearch::{SearxngSettings, SecretValue};
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;
    use workcell::web::{
        BRAVE_API_KEY_VARIABLE, EXA_API_KEY_VARIABLE, KAGI_API_KEY_VARIABLE,
        SEARXNG_API_KEY_VARIABLE, SEARXNG_BEARER_TOKEN_VARIABLE, SEARXNG_PASSWORD_VARIABLE,
        SEARXNG_URL_VARIABLE, SEARXNG_USER_VARIABLE, SERPAPI_API_KEY_VARIABLE,
        SERPAPI_ENGINE_VARIABLE, WEB_CONFIGURATION_VARIABLES,
    };

    use super::*;
    use crate::{WEBSEARCH_TOOL, WorkcellHost};

    const PROPERTIES: &str = "properties";
    const CATEGORIES: &str = "categories";
    const SEARXNG: &str = "searxng";
    const URL: &str = "http://100.64.0.2:8787";
    const OTHER_URL: &str = "http://127.0.0.1:9999";
    const CANARY: &str = "secret-canary\u{7}";
    const INVALID_BACKEND: &str = "altavista";
    const READY: &str = "ready";
    const UNAVAILABLE: &str = "unavailable";

    fn env(values: &[(&str, &str)]) -> impl Fn(&str) -> Result<Option<String>, ()> {
        let values: HashMap<String, String> = values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        move |name| Ok(values.get(name).cloned())
    }

    fn resolved(config: &WebsearchConfig, values: &[(&str, &str)]) -> Resolution {
        resolve(config, &env(values))
    }

    #[test]
    fn nothing_set_keeps_exa_mcp() {
        let resolution = resolved(&WebsearchConfig::default(), &[]);
        assert_eq!(
            resolution.configuration.backend(),
            Some(WebsearchBackend::ExaMcp)
        );
        assert_eq!(resolution.configuration.status(), READY);
        assert!(resolution.configuration.skipped().is_empty());
    }

    #[test]
    fn the_environment_selects_searxng() {
        let resolution = resolved(
            &WebsearchConfig::default(),
            &[
                (WEBSEARCH_BACKEND_VARIABLE, SEARXNG),
                (SEARXNG_URL_VARIABLE, URL),
            ],
        );
        assert_eq!(
            resolution.configuration.backend(),
            Some(WebsearchBackend::Searxng)
        );
        assert_eq!(resolution.configuration.status(), READY);
    }

    #[test]
    fn an_invalid_backend_is_unavailable_and_warned_without_its_value() {
        let resolution = resolved(
            &WebsearchConfig::default(),
            &[(WEBSEARCH_BACKEND_VARIABLE, INVALID_BACKEND)],
        );
        assert_eq!(resolution.configuration.status(), UNAVAILABLE);
        let warnings = WebsearchReport::of(&resolution.configuration, true).warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(!warnings[0].contains(INVALID_BACKEND), "{warnings:?}");
    }

    #[test]
    fn an_invalid_credential_never_reaches_a_warning_or_debug() {
        let resolution = resolved(
            &WebsearchConfig::default(),
            &[
                (WEBSEARCH_BACKEND_VARIABLE, SEARXNG),
                (SEARXNG_URL_VARIABLE, URL),
                (SEARXNG_API_KEY_VARIABLE, CANARY),
            ],
        );
        assert_eq!(resolution.configuration.status(), UNAVAILABLE);
        let report = WebsearchReport::of(&resolution.configuration, true);
        let shown = format!("{:?} {:?}", report.warnings(), resolution.configuration);
        assert!(!shown.contains(CANARY), "{shown}");
    }

    #[test]
    fn a_config_key_wins_over_the_environment() {
        let config = WebsearchConfig {
            backends: Some(vec![WebsearchBackendName::Searxng]),
            searxng: SearxngSettings {
                url: Some(SecretValue::new(URL)),
                ..SearxngSettings::default()
            },
            ..WebsearchConfig::default()
        };
        let from_config = resolved(&config, &[(SEARXNG_URL_VARIABLE, OTHER_URL)]);
        let from_env = resolved(
            &WebsearchConfig {
                backends: config.backends.clone(),
                ..WebsearchConfig::default()
            },
            &[(SEARXNG_URL_VARIABLE, URL)],
        );
        assert_eq!(
            from_config.configuration.backend(),
            Some(WebsearchBackend::Searxng)
        );
        assert_eq!(from_config.fingerprint, from_env.fingerprint);
    }

    #[test]
    fn an_unconfigured_backend_in_the_list_is_skipped() {
        let config = WebsearchConfig {
            backends: Some(vec![
                WebsearchBackendName::Brave,
                WebsearchBackendName::ExaMcp,
            ]),
            ..WebsearchConfig::default()
        };
        let resolution = resolved(&config, &[]);
        let report = WebsearchReport::of(&resolution.configuration, true);
        assert_eq!(report.active, [WebsearchBackend::ExaMcp]);
        assert_eq!(
            report.skipped,
            [(
                WebsearchBackend::Brave,
                WebsearchConfigurationIssue::MissingBraveApiKey
            )]
        );
    }

    #[test]
    fn an_empty_list_turns_websearch_off() {
        let config = WebsearchConfig {
            backends: Some(Vec::new()),
            ..WebsearchConfig::default()
        };
        let report = WebsearchReport::of(&resolved(&config, &[]).configuration, true);
        assert!(report.disabled);
    }

    #[test_case(WebsearchBackendName::ExaMcp, true, SettingSource::Unset; "exa_mcp_needs_nothing")]
    #[test_case(WebsearchBackendName::Searxng, true, SettingSource::Environment; "searxng_from_env")]
    #[test_case(WebsearchBackendName::Kagi, false, SettingSource::Unset; "kagi_without_a_key")]
    fn each_backend_reports_whether_it_is_set_up(
        backend: WebsearchBackendName,
        ready: bool,
        source: SettingSource,
    ) {
        let statuses = statuses(
            &WebsearchConfig::default(),
            &env(&[(SEARXNG_URL_VARIABLE, URL)]),
        );
        let status = statuses
            .iter()
            .find(|status| status.backend == backend)
            .unwrap();
        assert_eq!((status.ready(), status.source), (ready, source));
    }

    fn searxng_from_env() -> impl Fn(&str) -> Result<Option<String>, ()> {
        env(&[
            (WEBSEARCH_BACKEND_VARIABLE, SEARXNG),
            (SEARXNG_URL_VARIABLE, URL),
        ])
    }

    fn registered(root: &TempDir) -> (WorkcellHost, ToolRegistry) {
        let host = WorkcellHost::new(root.path(), None).unwrap();
        let registry = ToolRegistry::new();
        host.register(&registry).unwrap();
        (host, registry)
    }

    fn properties(registry: &ToolRegistry) -> Vec<String> {
        registry.get(WEBSEARCH_TOOL).unwrap().tool.schema()[PROPERTIES]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn an_applied_backend_reaches_the_registered_tool() {
        let root = TempDir::new().unwrap();
        let (host, registry) = registered(&root);
        assert!(!properties(&registry).iter().any(|key| key == CATEGORIES));

        let report = host
            .websearch()
            .apply_with(&WebsearchConfig::default(), &searxng_from_env());

        assert!(report.changed);
        assert_eq!(report.active, [WebsearchBackend::Searxng]);
        assert!(properties(&registry).iter().any(|key| key == CATEGORIES));
        let accepted = json!({ "query": SEARXNG, CATEGORIES: "it" });
        assert!(
            registry
                .get(WEBSEARCH_TOOL)
                .unwrap()
                .try_parse(&accepted)
                .is_some()
        );
    }

    #[test]
    fn applying_the_same_settings_again_keeps_the_revision() {
        let root = TempDir::new().unwrap();
        let (host, _registry) = registered(&root);
        let control = host.websearch();
        control.apply_with(&WebsearchConfig::default(), &searxng_from_env());
        let revision = host.inner.web.snapshot().revision;

        let report = control.apply_with(&WebsearchConfig::default(), &searxng_from_env());

        assert!(!report.changed);
        assert_eq!(host.inner.web.snapshot().revision, revision);
    }

    #[test]
    fn every_backend_variable_has_a_config_key() {
        let keyed = [
            SEARXNG_URL_VARIABLE,
            SEARXNG_API_KEY_VARIABLE,
            SEARXNG_BEARER_TOKEN_VARIABLE,
            SEARXNG_USER_VARIABLE,
            SEARXNG_PASSWORD_VARIABLE,
            EXA_API_KEY_VARIABLE,
            BRAVE_API_KEY_VARIABLE,
            KAGI_API_KEY_VARIABLE,
            SERPAPI_API_KEY_VARIABLE,
            SERPAPI_ENGINE_VARIABLE,
        ];
        for name in WEB_CONFIGURATION_VARIABLES {
            let has_key = WebsearchBackendName::ALL.into_iter().any(|backend| {
                backend
                    .fields()
                    .iter()
                    .any(|field| field.env == Some(*name))
            });
            let selector = [WEBSEARCH_BACKEND_VARIABLE, WEBSEARCH_MODE_VARIABLE].contains(name);
            assert_eq!(has_key, keyed.contains(name), "{name}");
            assert!(has_key || selector, "{name} has no [websearch] key");
        }
    }
}
