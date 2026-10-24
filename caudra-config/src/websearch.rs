//! `[websearch]`: which backends the `websearch` tool uses, in what order and mode, and the
//! settings each backend reads. Only the global config sets it, because it holds credentials
//! and decides where search queries go. A key left unset reads Workcell's environment variable.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use toml_edit::{Array, DocumentMut, Value};

use crate::global_file::GlobalFileError;
use crate::settings_file::GlobalSettingsFile;
use crate::{ConfigError, ConfigField, ConfigValue};

pub const WEBSEARCH_KEY: &str = "websearch";
pub const MODE_KEY: &str = "mode";
pub const BACKENDS_KEY: &str = "backends";
pub const URL_KEY: &str = "url";
pub const API_KEY_KEY: &str = "api_key";
pub const BEARER_TOKEN_KEY: &str = "bearer_token";
pub const USER_KEY: &str = "user";
pub const PASSWORD_KEY: &str = "password";
pub const ENGINE_KEY: &str = "engine";
const REDACTED: &str = "[REDACTED]";
const STRING: &str = "string";
const UNSET: ConfigValue = ConfigValue::Unset;

/// How the ready backends answer a search.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WebsearchMode {
    /// Only the first ready backend.
    Single,
    /// The ready backends in order, each one taking over when the one before it fails.
    #[default]
    Fallback,
    /// The model picks a backend; without a pick, the fallback order applies.
    All,
}

impl WebsearchMode {
    pub const ALL: [Self; 3] = [Self::Single, Self::Fallback, Self::All];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Fallback => "fallback",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
pub enum WebsearchBackendName {
    #[serde(rename = "exa-mcp")]
    ExaMcp,
    #[serde(rename = "searxng")]
    Searxng,
    #[serde(rename = "exa")]
    Exa,
    #[serde(rename = "brave")]
    Brave,
    #[serde(rename = "kagi")]
    Kagi,
    #[serde(rename = "serpapi")]
    Serpapi,
}

impl WebsearchBackendName {
    pub const ALL: [Self; 6] = [
        Self::ExaMcp,
        Self::Searxng,
        Self::Exa,
        Self::Brave,
        Self::Kagi,
        Self::Serpapi,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExaMcp => "exa-mcp",
            Self::Searxng => "searxng",
            Self::Exa => "exa",
            Self::Brave => "brave",
            Self::Kagi => "kagi",
            Self::Serpapi => "serpapi",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::ExaMcp => "Exa MCP",
            Self::Searxng => "SearXNG",
            Self::Exa => "Exa",
            Self::Brave => "Brave",
            Self::Kagi => "Kagi",
            Self::Serpapi => "SerpApi",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|backend| backend.as_str() == name)
    }

    /// The keys of `[websearch.<backend>]`, each naming the variable it falls back to.
    pub const fn fields(self) -> &'static [ConfigField] {
        match self {
            Self::ExaMcp => &[],
            Self::Searxng => SEARXNG_FIELDS,
            Self::Exa => EXA_FIELDS,
            Self::Brave => BRAVE_FIELDS,
            Self::Kagi => KAGI_FIELDS,
            Self::Serpapi => SERPAPI_FIELDS,
        }
    }
}

impl fmt::Display for WebsearchBackendName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Whether a backend key holds a credential, which stays masked and earns a plain-text warning.
pub fn is_secret_key(key: &str) -> bool {
    !matches!(key, URL_KEY | ENGINE_KEY)
}

/// A setting that may hold a credential, so `Debug` never shows it.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

#[derive(Deserialize, Default, Debug, Clone, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct SearxngSettings {
    pub url: Option<SecretValue>,
    pub api_key: Option<SecretValue>,
    pub bearer_token: Option<SecretValue>,
    pub user: Option<SecretValue>,
    pub password: Option<SecretValue>,
}

#[derive(Deserialize, Default, Debug, Clone, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ApiKeySettings {
    pub api_key: Option<SecretValue>,
}

#[derive(Deserialize, Default, Debug, Clone, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct SerpApiSettings {
    pub api_key: Option<SecretValue>,
    pub engine: Option<SecretValue>,
}

/// `[websearch]` as configured. Every key is optional: an unset one reads the environment,
/// and the defaults apply after that.
#[derive(Deserialize, Default, Debug, Clone, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WebsearchConfig {
    pub mode: Option<WebsearchMode>,
    pub backends: Option<Vec<WebsearchBackendName>>,
    pub searxng: SearxngSettings,
    pub exa: ApiKeySettings,
    pub brave: ApiKeySettings,
    pub kagi: ApiKeySettings,
    pub serpapi: SerpApiSettings,
}

impl WebsearchConfig {
    pub const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: MODE_KEY,
            ty: STRING,
            default: ConfigValue::Str("fallback"),
            min: None,
            max: None,
            env: Some("WORKCELL_WEBSEARCH_MODE"),
            choices: &[],
            description: "How the ready backends answer: `single` uses the first ready one, `fallback` tries them in order when one fails, and `all` lets the model pick one, trying them in order when it does not",
        },
        ConfigField {
            name: BACKENDS_KEY,
            ty: "string[]",
            default: ConfigValue::Varies(
                "the comma-separated list in WORKCELL_WEBSEARCH_BACKEND, else [\"exa-mcp\"]",
            ),
            min: None,
            max: None,
            env: Some("WORKCELL_WEBSEARCH_BACKEND"),
            choices: &[],
            description: "Backends in priority order: exa-mcp, searxng, exa, brave, kagi, or serpapi. A backend that is not set up correctly is skipped with a warning. An empty list turns websearch off",
        },
    ];

    /// The value `backend`'s `key` holds, `None` while it is unset.
    pub fn value(&self, backend: WebsearchBackendName, key: &str) -> Option<&SecretValue> {
        match (backend, key) {
            (WebsearchBackendName::Searxng, URL_KEY) => self.searxng.url.as_ref(),
            (WebsearchBackendName::Searxng, API_KEY_KEY) => self.searxng.api_key.as_ref(),
            (WebsearchBackendName::Searxng, BEARER_TOKEN_KEY) => self.searxng.bearer_token.as_ref(),
            (WebsearchBackendName::Searxng, USER_KEY) => self.searxng.user.as_ref(),
            (WebsearchBackendName::Searxng, PASSWORD_KEY) => self.searxng.password.as_ref(),
            (WebsearchBackendName::Exa, API_KEY_KEY) => self.exa.api_key.as_ref(),
            (WebsearchBackendName::Brave, API_KEY_KEY) => self.brave.api_key.as_ref(),
            (WebsearchBackendName::Kagi, API_KEY_KEY) => self.kagi.api_key.as_ref(),
            (WebsearchBackendName::Serpapi, API_KEY_KEY) => self.serpapi.api_key.as_ref(),
            (WebsearchBackendName::Serpapi, ENGINE_KEY) => self.serpapi.engine.as_ref(),
            _ => None,
        }
    }

    /// Every key `overlay` sets wins.
    pub(crate) fn merge(&mut self, overlay: Self) {
        let Self {
            mode,
            backends,
            searxng,
            exa,
            brave,
            kagi,
            serpapi,
        } = overlay;
        replace(&mut self.mode, mode);
        replace(&mut self.backends, backends);
        replace(&mut self.searxng.url, searxng.url);
        replace(&mut self.searxng.api_key, searxng.api_key);
        replace(&mut self.searxng.bearer_token, searxng.bearer_token);
        replace(&mut self.searxng.user, searxng.user);
        replace(&mut self.searxng.password, searxng.password);
        replace(&mut self.exa.api_key, exa.api_key);
        replace(&mut self.brave.api_key, brave.api_key);
        replace(&mut self.kagi.api_key, kagi.api_key);
        replace(&mut self.serpapi.api_key, serpapi.api_key);
        replace(&mut self.serpapi.engine, serpapi.engine);
    }

    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        let backends = self.backends.as_deref().unwrap_or_default();
        match backends
            .iter()
            .enumerate()
            .find(|(index, backend)| backends[..*index].contains(backend))
        {
            Some((_, backend)) => Err(ConfigError::WebsearchDuplicateBackend(*backend)),
            None => Ok(()),
        }
    }
}

fn replace<T>(current: &mut Option<T>, overlay: Option<T>) {
    if overlay.is_some() {
        *current = overlay;
    }
}

const fn field(name: &'static str, env: &'static str, description: &'static str) -> ConfigField {
    ConfigField {
        name,
        ty: STRING,
        default: UNSET,
        min: None,
        max: None,
        env: Some(env),
        choices: &[],
        description,
    }
}

const SEARXNG_FIELDS: &[ConfigField] = &[
    field(
        URL_KEY,
        "SEARXNG_URL",
        "SearXNG base URL, HTTPS when a credential is set and never with credentials in it",
    ),
    field(
        API_KEY_KEY,
        "SEARXNG_API_KEY",
        "API key sent to SearXNG; set at most one of api_key, bearer_token, or user and password",
    ),
    field(
        BEARER_TOKEN_KEY,
        "SEARXNG_BEARER_TOKEN",
        "Bearer token sent to SearXNG",
    ),
    field(
        USER_KEY,
        "SEARXNG_USER",
        "Basic authentication user, set with password",
    ),
    field(
        PASSWORD_KEY,
        "SEARXNG_PASSWORD",
        "Basic authentication password, set with user",
    ),
];
const EXA_FIELDS: &[ConfigField] = &[field(API_KEY_KEY, "EXA_API_KEY", "Exa API key")];
const BRAVE_FIELDS: &[ConfigField] = &[field(API_KEY_KEY, "BRAVE_API_KEY", "Brave Search API key")];
const KAGI_FIELDS: &[ConfigField] = &[field(API_KEY_KEY, "KAGI_API_KEY", "Kagi API key")];
const SERPAPI_FIELDS: &[ConfigField] = &[
    field(API_KEY_KEY, "SERPAPI_API_KEY", "SerpApi API key"),
    field(
        ENGINE_KEY,
        "SERPAPI_ENGINE",
        "SerpApi engine: google or bing",
    ),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebsearchEdit {
    /// `None` removes the key.
    Mode(Option<WebsearchMode>),
    /// `None` removes the key, so the environment decides.
    Backends(Option<Vec<WebsearchBackendName>>),
    /// `None` removes the key, so the environment decides.
    Field {
        backend: WebsearchBackendName,
        key: &'static str,
        value: Option<String>,
    },
}

/// `[websearch]` of the global `caudra.toml` as written, `{env:NAME}` and `{file:PATH}`
/// references unexpanded, with the revision an edit commits against.
#[derive(Debug, Clone)]
pub struct GlobalWebsearchFile {
    file: GlobalSettingsFile,
    written: WebsearchConfig,
}

impl GlobalWebsearchFile {
    /// A missing file is an empty one.
    pub fn load(path: &Path) -> Result<Self, GlobalFileError> {
        GlobalSettingsFile::load(path).map(Self::read)
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    pub fn written(&self) -> &WebsearchConfig {
        &self.written
    }

    /// Commits `edit` only while the file still holds what [`Self::load`] read, and only when
    /// the result loads as a valid global config.
    pub fn apply(&self, edit: WebsearchEdit) -> Result<Self, GlobalFileError> {
        let edited = match edit {
            WebsearchEdit::Mode(mode) => self.file.edit(
                &[WEBSEARCH_KEY],
                MODE_KEY,
                mode.map(|mode| Value::from(mode.as_str())),
            ),
            WebsearchEdit::Backends(backends) => self.file.edit(
                &[WEBSEARCH_KEY],
                BACKENDS_KEY,
                backends.map(|backends| {
                    Value::Array(
                        backends
                            .iter()
                            .map(|backend| backend.as_str())
                            .collect::<Array>(),
                    )
                }),
            ),
            WebsearchEdit::Field {
                backend,
                key,
                value,
            } => self.file.edit(
                &[WEBSEARCH_KEY, backend.as_str()],
                key,
                value.map(Value::from),
            ),
        }?;
        Ok(Self::read(edited))
    }

    fn read(file: GlobalSettingsFile) -> Self {
        let written = written_section(file.document()).unwrap_or_default();
        Self { file, written }
    }
}

/// The section as written; the commit check already refused a shape that does not parse.
fn written_section(document: &DocumentMut) -> Option<WebsearchConfig> {
    #[derive(Deserialize)]
    struct Written {
        websearch: WebsearchConfig,
    }
    let section = document.get(WEBSEARCH_KEY)?;
    let mut alone = DocumentMut::new();
    alone.insert(WEBSEARCH_KEY, section.clone());
    toml::from_str::<Written>(&alone.to_string())
        .ok()
        .map(|written| written.websearch)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::RawConfig;
    use crate::config_file::{CONFIG_FILE, load_global_config};

    const ORIGINAL: &str =
        "# my settings\nversion = 5\n\n[ui]\n# keep this\ntheme = \"dark\" # trailing\n";
    const URL: &str = "http://100.64.0.2:8787";
    const SECRET: &str = "secret-canary";
    const KEY_REFERENCE: &str = "{env:HOME}";
    const EXTERNAL_EDIT: &str = "# edited elsewhere\n";

    fn written(contents: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    fn layer(source: &str) -> RawConfig {
        toml::from_str(source).unwrap()
    }

    fn loaded(path: &Path) -> WebsearchConfig {
        load_global_config(path)
            .unwrap()
            .settings
            .into_config(false)
            .unwrap()
            .websearch
    }

    #[test]
    fn a_section_reads_every_backend_table() {
        let config = layer(&format!(
            "[websearch]\nmode = \"all\"\nbackends = [\"searxng\", \"exa-mcp\"]\n\n[websearch.searxng]\nurl = \"{URL}\"\n\n[websearch.serpapi]\napi_key = \"{SECRET}\"\nengine = \"bing\"\n"
        ))
        .into_config(false)
        .unwrap()
        .websearch;
        assert_eq!(config.mode, Some(WebsearchMode::All));
        assert_eq!(
            config.backends,
            Some(vec![
                WebsearchBackendName::Searxng,
                WebsearchBackendName::ExaMcp
            ])
        );
        assert_eq!(
            config
                .value(WebsearchBackendName::Searxng, URL_KEY)
                .map(SecretValue::expose),
            Some(URL)
        );
        assert_eq!(
            config
                .value(WebsearchBackendName::Serpapi, ENGINE_KEY)
                .map(SecretValue::expose),
            Some("bing")
        );
    }

    #[test_case("[websearch]\nprovider = \"exa\"\n"; "unknown_key")]
    #[test_case("[websearch]\nbackends = [\"google\"]\n"; "unknown_backend")]
    #[test_case("[websearch]\nmode = \"random\"\n"; "unknown_mode")]
    #[test_case("[websearch.brave]\ntoken = \"x\"\n"; "unknown_backend_key")]
    fn an_unknown_setting_is_refused(source: &str) {
        assert!(toml::from_str::<RawConfig>(source).is_err());
    }

    #[test]
    fn a_backend_listed_twice_is_refused() {
        let error = layer("[websearch]\nbackends = [\"brave\", \"brave\"]\n")
            .into_config(false)
            .err()
            .unwrap();
        assert!(matches!(
            error,
            ConfigError::WebsearchDuplicateBackend(WebsearchBackendName::Brave)
        ));
    }

    #[test]
    fn a_project_layer_cannot_set_websearch() {
        let mut global = RawConfig::default();
        global.merge(layer(&format!("[websearch.searxng]\nurl = \"{URL}\"\n")));
        assert!(matches!(
            global.into_config(false),
            Err(ConfigError::ProjectWebsearch)
        ));
    }

    #[test]
    fn a_global_layer_overlays_key_by_key() {
        let mut global = layer(&format!(
            "[websearch]\nmode = \"single\"\n[websearch.searxng]\nurl = \"{URL}\"\n"
        ));
        global.merge_global(layer(&format!(
            "[websearch.searxng]\napi_key = \"{SECRET}\"\n"
        )));
        let config = global.into_config(false).unwrap().websearch;
        assert_eq!(config.mode, Some(WebsearchMode::Single));
        assert!(config.searxng.url.is_some());
        assert!(config.searxng.api_key.is_some());
    }

    #[test]
    fn debug_never_shows_a_value() {
        let config = layer(&format!(
            "[websearch.searxng]\nurl = \"{URL}\"\npassword = \"{SECRET}\"\n"
        ))
        .into_config(false)
        .unwrap()
        .websearch;
        let shown = format!("{config:?}");
        assert!(!shown.contains(SECRET), "{shown}");
        assert!(!shown.contains(URL), "{shown}");
    }

    #[test]
    fn edits_keep_everything_else_byte_identical() {
        let (_dir, path) = written(ORIGINAL);
        let edited = GlobalWebsearchFile::load(&path)
            .unwrap()
            .apply(WebsearchEdit::Backends(Some(vec![
                WebsearchBackendName::Searxng,
                WebsearchBackendName::ExaMcp,
            ])))
            .unwrap()
            .apply(WebsearchEdit::Mode(Some(WebsearchMode::All)))
            .unwrap()
            .apply(WebsearchEdit::Field {
                backend: WebsearchBackendName::Searxng,
                key: API_KEY_KEY,
                value: Some(KEY_REFERENCE.into()),
            })
            .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(ORIGINAL), "{text}");
        assert_eq!(edited.written().mode, Some(WebsearchMode::All));
        assert_eq!(
            edited
                .written()
                .value(WebsearchBackendName::Searxng, API_KEY_KEY)
                .map(SecretValue::expose),
            Some(KEY_REFERENCE)
        );
        let reloaded = GlobalWebsearchFile::load(&path).unwrap();
        assert_eq!(reloaded.written(), edited.written());
        assert_eq!(
            loaded(&path).backends,
            Some(vec![
                WebsearchBackendName::Searxng,
                WebsearchBackendName::ExaMcp
            ])
        );

        let removed = reloaded
            .apply(WebsearchEdit::Field {
                backend: WebsearchBackendName::Searxng,
                key: API_KEY_KEY,
                value: None,
            })
            .unwrap()
            .apply(WebsearchEdit::Mode(None))
            .unwrap();
        assert_eq!(removed.written().searxng, SearxngSettings::default());
        assert_eq!(removed.written().mode, None);
    }

    #[test]
    fn an_edit_over_a_changed_file_is_refused() {
        let (_dir, path) = written(ORIGINAL);
        let loaded = GlobalWebsearchFile::load(&path).unwrap();
        fs::write(&path, format!("{ORIGINAL}{EXTERNAL_EDIT}")).unwrap();
        assert!(
            loaded
                .apply(WebsearchEdit::Mode(Some(WebsearchMode::Single)))
                .is_err()
        );
        assert!(fs::read_to_string(&path).unwrap().ends_with(EXTERNAL_EDIT));
    }

    #[test]
    fn an_edit_that_breaks_the_file_is_refused() {
        let (_dir, path) = written(ORIGINAL);
        let loaded = GlobalWebsearchFile::load(&path).unwrap();
        let error = loaded
            .apply(WebsearchEdit::Backends(Some(vec![
                WebsearchBackendName::Kagi,
                WebsearchBackendName::Kagi,
            ])))
            .unwrap_err();
        assert!(matches!(error, GlobalFileError::Invalid { .. }), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), ORIGINAL);
    }

    #[test]
    fn written_values_keep_their_references() {
        let (_dir, path) = written(&format!(
            "{ORIGINAL}\n[websearch.brave]\napi_key = \"{KEY_REFERENCE}\"\n"
        ));
        let loaded = GlobalWebsearchFile::load(&path).unwrap();
        assert_eq!(
            loaded
                .written()
                .value(WebsearchBackendName::Brave, API_KEY_KEY)
                .map(SecretValue::expose),
            Some(KEY_REFERENCE)
        );
    }

    #[test_case(URL_KEY, false; "url")]
    #[test_case(ENGINE_KEY, false; "engine")]
    #[test_case(API_KEY_KEY, true; "api_key")]
    #[test_case(PASSWORD_KEY, true; "password")]
    fn credentials_are_secret(key: &str, secret: bool) {
        assert_eq!(is_secret_key(key), secret);
    }
}
