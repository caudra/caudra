use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process;

use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tracing::debug;

use caudra_storage::paths;
use caudra_storage::thinking::ReasoningOptions;

const PROVIDERS_FILE: &str = "providers.toml";
const BAD_CONFIG_EXIT_CODE: i32 = 2;
/// The only built-in that reads `enable_free_models`.
const OPENCODE_SLUG: &str = "opencode";

/// A workload slot a model can be bound to. A purpose records what the user
/// wants a model used for, never a claim about what the model is capable of.
///
/// Lives here rather than in caudra-providers so the config layer can validate
/// bindings without depending on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelPurpose {
    Chat,
    Fast,
    Balanced,
    Best,
    Title,
    Compact,
    Goal,
}

impl ModelPurpose {
    pub const ALL: [Self; 7] = [
        Self::Chat,
        Self::Fast,
        Self::Balanced,
        Self::Best,
        Self::Title,
        Self::Compact,
        Self::Goal,
    ];

    /// The purposes that name how much capability a caller wants, as opposed to
    /// which workload is asking. Only these carry a curated model table, and
    /// only these are worth pointing another purpose at.
    pub const CLASSES: [Self; 3] = [Self::Fast, Self::Balanced, Self::Best];

    /// Title-cased name for pickers and docs. `Display` stays lowercase so it
    /// matches what the config and state rows hold.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Fast => "Fast",
            Self::Balanced => "Balanced",
            Self::Best => "Best",
            Self::Title => "Title",
            Self::Compact => "Compact",
            Self::Goal => "Goal",
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Best => "best",
            Self::Title => "title",
            Self::Compact => "compact",
            Self::Goal => "goal",
        }
    }
}

impl std::fmt::Display for ModelPurpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ModelPurpose {
    type Err = UnknownPurpose;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|purpose| purpose.as_str() == s)
            .ok_or_else(|| UnknownPurpose(s.to_string()))
    }
}

#[derive(Debug, thiserror::Error)]
#[error(
    "unknown model purpose '{0}', expected one of: chat, fast, balanced, best, title, compact, goal"
)]
pub struct UnknownPurpose(pub String);

/// Model id prefixes a provider offers for one purpose, best first.
///
/// Accepts a bare string or a list so the common single-model case stays a
/// one-liner. Entries match by prefix, and the first one also names the model
/// that wins the slot, so it has to be a real id. That is the same contract the
/// built-in tables keep: a curated entry lists prefixes and resolves its default
/// from the first of them.
///
/// Prefixes are what make a server full of fine-tune variants declarable. One
/// `qwen3.8-27b` covers every `qwen3.8-27b-*` the endpoint loads, without the
/// config needing an edit each time a new one appears.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "OneOrMany", into = "OneOrMany")]
pub struct PurposeModels(Vec<String>);

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl From<OneOrMany> for PurposeModels {
    fn from(value: OneOrMany) -> Self {
        match value {
            OneOrMany::One(id) => Self(vec![id]),
            OneOrMany::Many(ids) => Self(ids),
        }
    }
}

/// Round-trips back to the shape the operator wrote, so caudra rewriting this
/// file on upsert does not reformat a hand-written one-liner into a list.
impl From<PurposeModels> for OneOrMany {
    fn from(value: PurposeModels) -> Self {
        match <[String; 1]>::try_from(value.0) {
            Ok([id]) => Self::One(id),
            Err(ids) => Self::Many(ids),
        }
    }
}

impl PurposeModels {
    /// The id that wins the slot when several are declared.
    pub fn preferred(&self) -> Option<&str> {
        self.0.first().map(String::as_str)
    }

    /// Whether `model_id` is one this purpose covers. Prefix, not equality, so a
    /// family of fine-tunes is one line rather than one line each.
    pub fn matches(&self, model_id: &str) -> bool {
        self.0.iter().any(|prefix| model_id.starts_with(prefix))
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDef {
    pub id: String,
    #[serde(flatten)]
    pub fields: ModelFields,
}

/// Facts declarable per model, or for every model of a provider at once via
/// [`ProviderDef::model_defaults`]. Facts only: which workload a model serves is
/// a binding, declared in [`ProviderDef::purposes`].
///
/// Every field is optional so absence means "inherit" rather than "reset to the
/// default": a non-optional field would make an exact entry silently outrank a
/// provider default it never mentioned.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_tool_examples: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
    /// Reasoning controls this model accepts, in the models.dev shape. Declared
    /// here for a model no catalog describes, so a level it never advertised is
    /// snapped into this list instead of being sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_options: Option<ReasoningOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_output: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_cache_write: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_fast_input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_fast_output: Option<f64>,
}

impl ModelFields {
    /// Any pricing field set means the user provided pricing (other fields default to 0).
    pub fn has_pricing(&self) -> bool {
        self.pricing_input.is_some()
            || self.pricing_output.is_some()
            || self.pricing_cache_write.is_some()
            || self.pricing_cache_read.is_some()
    }

    pub fn has_fast_pricing(&self) -> bool {
        self.pricing_fast_input.is_some() || self.pricing_fast_output.is_some()
    }

    /// Field-by-field override: whatever `self` declares wins, and `base` fills
    /// every gap.
    fn over(self, base: &Self) -> Self {
        Self {
            context_window: self.context_window.or(base.context_window),
            max_output_tokens: self.max_output_tokens.or(base.max_output_tokens),
            supports_tool_examples: self.supports_tool_examples.or(base.supports_tool_examples),
            supports_thinking: self.supports_thinking.or(base.supports_thinking),
            requires_thinking: self.requires_thinking.or(base.requires_thinking),
            supports_vision: self.supports_vision.or(base.supports_vision),
            reasoning_options: self
                .reasoning_options
                .or_else(|| base.reasoning_options.clone()),
            pricing_input: self.pricing_input.or(base.pricing_input),
            pricing_output: self.pricing_output.or(base.pricing_output),
            pricing_cache_write: self.pricing_cache_write.or(base.pricing_cache_write),
            pricing_cache_read: self.pricing_cache_read.or(base.pricing_cache_read),
            pricing_fast_input: self.pricing_fast_input.or(base.pricing_fast_input),
            pricing_fast_output: self.pricing_fast_output.or(base.pricing_fast_output),
        }
    }
}

/// Normalize a provider name into a lowercase, hyphen-separated slug.
/// "My Cool Provider" -> "my-cool-provider"
pub fn slugify(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    Openai,
    OpenaiResponses,
    Anthropic,
    Google,
}

impl FromStr for Protocol {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "openai" => Ok(Self::Openai),
            "openai-responses" => Ok(Self::OpenaiResponses),
            "anthropic" => Ok(Self::Anthropic),
            "google" => Ok(Self::Google),
            _ => Err(format!("unknown protocol: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ProviderPlan {
    pub display_name: &'static str,
    pub base_url: &'static str,
    pub default_model: Option<&'static str>,
    pub login_url: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuiltInProvider {
    pub slug: &'static str,
    pub display_name: &'static str,
    pub protocol: Protocol,
    pub default_base_url: &'static str,
    pub default_api_key_env: &'static str,
    pub default_model: &'static str,
    pub plans: Option<&'static [(&'static str, ProviderPlan)]>,
    pub login_url: Option<&'static str>,
    /// Whether the login flow should prompt for a base URL (e.g. local inference servers).
    pub needs_url: bool,
}

inventory::collect!(BuiltInProvider);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OverrideFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Path prefix sent to the gateway, replacing the default (`/v1`, or
    /// `/v1beta` for Gemini routes). Set it to `""` when the upstream's base
    /// url already carries its own path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
}

/// Overrides for a single gateway provider (Aperture), keyed by its id (e.g.
/// `zai`, `ollama`, `ikora-openai`). Provider-level fields apply to every model
/// from that provider; `models` refine individual models.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderOverride {
    #[serde(flatten)]
    pub default: OverrideFields,
    #[serde(default)]
    pub models: HashMap<String, OverrideFields>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderDef {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub discover_models: bool,
    /// Opencode-only: when `Some(false)`, free catalog models are hidden
    /// entirely. Defaults to `false` when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_free_models: Option<bool>,
    /// Aperture-only: per-gateway-provider overrides for the routed native
    /// providers.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub overrides: HashMap<String, ProviderOverride>,
    /// Applied to every model of this provider, including ones only discovery
    /// knows about. A matching `models` entry overrides it field by field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_defaults: Option<ModelFields>,
    /// Which model id serves each workload while this provider is active.
    /// Checked-in bindings, outranked by an assignment made in the picker.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub purposes: HashMap<ModelPurpose, PurposeModels>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelDef>,
}

impl ProviderDef {
    /// Effective declared settings for `model_id`.
    ///
    /// An exact `models` entry wins field by field over `model_defaults`, so a
    /// provider whose model ids churn keeps its context window and reasoning
    /// options without an entry per id.
    pub fn model_settings(&self, model_id: &str) -> ModelFields {
        let defaults = self.model_defaults.clone().unwrap_or_default();
        match self.models.iter().find(|m| m.id == model_id) {
            Some(declared) => declared.fields.clone().over(&defaults),
            None => defaults,
        }
    }

    pub fn declares(&self, model_id: &str) -> bool {
        self.models.iter().any(|m| m.id == model_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProvidersConfig {
    #[serde(flatten)]
    pub providers: HashMap<String, ProviderDef>,
}

impl ProvidersConfig {
    /// Read and parse `providers.toml`. Hard-exits on parse errors so a typo
    /// in a purpose or pricing surfaces immediately instead of silently dropping
    /// every provider and starting caudra with an empty registry.
    pub fn load() -> Self {
        let path = providers_file_path();
        if !path.exists() {
            return Self::default();
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot read providers.toml");
                return Self::default();
            }
        };
        match toml::from_str::<ProvidersConfig>(&content) {
            Ok(config) => {
                debug!(path = %path.display(), "loaded providers config");
                config
            }
            Err(e) => {
                eprintln!("error: invalid {}: {e}", path.display());
                process::exit(BAD_CONFIG_EXIT_CODE);
            }
        }
    }

    pub fn save(&self) -> Result<(), std::io::Error> {
        let path = providers_file_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(&path, content)?;
        debug!(path = %path.display(), "saved providers config");
        Ok(())
    }

    pub fn get(&self, slug: &str) -> Option<&ProviderDef> {
        self.providers.get(slug)
    }

    pub fn upsert(&mut self, slug: String, def: ProviderDef) {
        self.providers.insert(slug, def);
    }

    pub fn remove(&mut self, slug: &str) -> bool {
        self.providers.remove(slug).is_some()
    }
}

fn providers_file_path() -> PathBuf {
    paths::config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(PROVIDERS_FILE)
}

pub fn builtin_provider(slug: &str) -> Option<&'static BuiltInProvider> {
    inventory::iter::<BuiltInProvider>()
        .into_iter()
        .find(|p| p.slug == slug)
}

pub fn all_builtins() -> Vec<&'static BuiltInProvider> {
    inventory::iter::<BuiltInProvider>().collect()
}

pub fn resolve_api_key_env(slug: &str, def: Option<&ProviderDef>) -> String {
    if let Some(d) = def
        && let Some(env) = &d.api_key_env
    {
        return env.clone();
    }
    if let Some(builtin) = builtin_provider(slug) {
        return builtin.default_api_key_env.to_string();
    }
    format!("{}_API_KEY", slug.to_uppercase().replace('-', "_"))
}

/// The `<SLUG>_BASE_URL` env var name (e.g. `anthropic` -> `ANTHROPIC_BASE_URL`,
/// `llama-cpp` -> `LLAMA_CPP_BASE_URL`).
pub fn base_url_env_var(slug: &str) -> String {
    format!("{}_BASE_URL", slug.to_uppercase().replace('-', "_"))
}

/// The `<SLUG>_BASE_URL` override, or `None` when unset or empty.
pub fn base_url_override(slug: &str) -> Option<String> {
    std::env::var(base_url_env_var(slug))
        .ok()
        .filter(|url| !url.is_empty())
}

/// Env override then `providers.toml`, without the built-in default. Callers
/// that already carry a default (the openai-compat layer, whose static default
/// can be more specific than the inventory one) use this.
pub fn configured_base_url(slug: &str, def: Option<&ProviderDef>) -> Option<String> {
    if let Some(url) = base_url_override(slug) {
        return Some(url);
    }
    let def = def?;
    if let Some(url) = &def.base_url {
        return Some(url.clone());
    }
    let plan_name = def.plan.as_ref()?;
    builtin_provider(slug)?
        .plans?
        .iter()
        .find(|(key, _)| key == plan_name)
        .map(|(_, plan)| plan.base_url.to_string())
}

pub fn resolve_base_url(slug: &str, def: Option<&ProviderDef>) -> Option<String> {
    configured_base_url(slug, def)
        .or_else(|| builtin_provider(slug).map(|b| b.default_base_url.to_string()))
}

/// Fields a `providers.toml` entry sets that a built-in slug ignores, because
/// built-ins keep their compiled protocol, model catalog and auth wiring.
/// Callers decide what counts as built-in (the inventory misses the `opencode`
/// slugs) and when to report it.
pub fn ignored_builtin_fields(slug: &str, def: &ProviderDef) -> Vec<&'static str> {
    let mut ignored = Vec::new();
    if def.protocol.is_some() {
        ignored.push("protocol");
    }
    if def.api_key_env.is_some() {
        ignored.push("api_key_env");
    }
    if def.discover_models {
        ignored.push("discover_models");
    }
    if !def.models.is_empty() {
        ignored.push("models");
    }
    if def.enable_free_models.is_some() && slug != OPENCODE_SLUG {
        ignored.push("enable_free_models");
    }
    ignored
}

pub fn resolve_protocol(slug: &str, def: Option<&ProviderDef>) -> Option<Protocol> {
    if let Some(d) = def
        && let Some(p) = &d.protocol
    {
        return Some(*p);
    }
    builtin_provider(slug).map(|b| b.protocol)
}

pub fn resolve_display_name(slug: &str, def: Option<&ProviderDef>) -> String {
    if let Some(d) = def
        && let Some(name) = &d.display_name
    {
        return name.clone();
    }
    builtin_provider(slug)
        .map(|b| b.display_name.to_string())
        .unwrap_or_else(|| slug.to_string())
}

pub fn resolve_default_model(slug: &str, def: Option<&ProviderDef>) -> Option<String> {
    if let Some(d) = def {
        if let Some(m) = &d.default_model {
            return Some(m.clone());
        }
        if let Some(plan_name) = &d.plan
            && let Some(builtin) = builtin_provider(slug)
            && let Some(plans) = builtin.plans
        {
            for (key, plan) in plans {
                if key == plan_name
                    && let Some(m) = &plan.default_model
                {
                    return Some(m.to_string());
                }
            }
        }
    }
    builtin_provider(slug).map(|b| b.default_model.to_string())
}

pub fn resolve_login_url(slug: &str, plan: Option<&str>) -> Option<String> {
    if let Some(plan_name) = plan
        && let Some(builtin) = builtin_provider(slug)
        && let Some(plans) = builtin.plans
    {
        for (key, plan) in plans {
            if *key == plan_name
                && let Some(url) = plan.login_url
            {
                return Some(url.to_string());
            }
        }
    }
    builtin_provider(slug).and_then(|b| b.login_url.map(|u| u.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const UNKNOWN_PURPOSE: &str = "unknown variant `compaction`";
    const DECLARED_LEVELS_TOML: &str = r#"
[local]
protocol = "openai"
base_url = "http://127.0.0.1:8080/v1"
discover_models = true

[[local.models]]
id = "qwen3.8-27b"
context_window = 229376
max_output_tokens = 32768

[[local.models.reasoning_options]]
type = "effort"
values = ["none", "low", "medium", "xhigh"]
"#;

    const DECLARED_WINS: &str =
        "a level declared in providers.toml is the only description this model has";

    const ABSENT_MEANS_INHERIT: &str =
        "an absent field must inherit the provider default, not reset it";

    const DEFAULTS_TOML: &str = r#"
[local]
protocol = "openai"

[local.purposes]
fast = ["small", "small-draft"]
best = "qwen3.8-27b-lora"

[local.model_defaults]
context_window = 229376
max_output_tokens = 32768
reasoning_options = []

[[local.models]]
id = "qwen3.8-27b-lora"

[[local.models]]
id = "small"
context_window = 8192
"#;

    #[test_case("qwen3.8-27b-lora", Some(229_376) ; "exact_entry_inherits_what_it_omits")]
    #[test_case("small", Some(8_192) ; "exact_entry_overrides_field_by_field")]
    #[test_case("never-declared", Some(229_376) ; "discovered_model_gets_defaults")]
    fn model_defaults_apply_unless_an_exact_entry_overrides(model_id: &str, window: Option<u32>) {
        let parsed: ProvidersConfig = toml::from_str(DEFAULTS_TOML).unwrap();
        let settings = parsed.get("local").unwrap().model_settings(model_id);

        assert_eq!(settings.context_window, window, "{ABSENT_MEANS_INHERIT}");
        assert_eq!(settings.max_output_tokens, Some(32_768));
    }

    #[test_case(ModelPurpose::Fast, "small" ; "list_prefers_its_first_entry")]
    #[test_case(ModelPurpose::Best, "qwen3.8-27b-lora" ; "bare_string_is_still_accepted")]
    fn declared_purposes_bind_a_model_id(purpose: ModelPurpose, expected: &str) {
        let parsed: ProvidersConfig = toml::from_str(DEFAULTS_TOML).unwrap();
        let def = parsed.get("local").unwrap();

        assert_eq!(
            def.purposes
                .get(&purpose)
                .and_then(PurposeModels::preferred),
            Some(expected)
        );
    }

    /// Every declared entry serves the purpose, not just the one that wins the
    /// slot, and each covers its whole family: a server that loads a dozen
    /// fine-tunes of one base must not need a dozen config lines.
    #[test_case("small", true ; "exact_entry")]
    #[test_case("small-draft", true ; "later_entry_still_counts")]
    #[test_case("small-v2-experimental", true ; "variant_matches_by_prefix")]
    #[test_case("qwen3.8-27b-lora", false ; "another_purposes_model_does_not")]
    #[test_case("tiny", false ; "unrelated_id_does_not")]
    fn declared_entries_cover_their_family(model_id: &str, expected: bool) {
        let parsed: ProvidersConfig = toml::from_str(DEFAULTS_TOML).unwrap();
        let fast = parsed
            .get("local")
            .unwrap()
            .purposes
            .get(&ModelPurpose::Fast)
            .unwrap();

        assert_eq!(fast.matches(model_id), expected);
    }

    #[test]
    fn undeclared_purposes_stay_absent_rather_than_defaulting() {
        let parsed: ProvidersConfig = toml::from_str(DEFAULTS_TOML).unwrap();
        let def = parsed.get("local").unwrap();

        assert_eq!(def.purposes.get(&ModelPurpose::Balanced), None);
        assert_eq!(def.purposes.get(&ModelPurpose::Title), None);
    }

    /// `models` and `model_defaults` both flatten, and caudra rewrites this
    /// file on upsert, so a shape that parses but cannot be re-serialized would
    /// corrupt the user's config on the next write.
    #[test]
    fn defaults_and_models_survive_a_serialize_roundtrip() {
        let parsed: ProvidersConfig = toml::from_str(DEFAULTS_TOML).unwrap();
        let rewritten = toml::to_string_pretty(&parsed).unwrap();
        let reparsed: ProvidersConfig = toml::from_str(&rewritten).unwrap();

        let local = reparsed.get("local").unwrap();
        let settings = local.model_settings("small");
        assert_eq!(settings.context_window, Some(8_192));
        assert_eq!(settings.max_output_tokens, Some(32_768));
        assert_eq!(
            local
                .purposes
                .get(&ModelPurpose::Fast)
                .map(|models| models.iter().collect::<Vec<_>>()),
            Some(vec!["small", "small-draft"]),
            "a declared list must survive the rewrite caudra does on upsert"
        );
    }

    /// An empty list is a real declaration ("this endpoint takes no reasoning
    /// controls"), not an absent one, so it must survive the merge.
    #[test]
    fn empty_default_reasoning_options_are_declared_not_missing() {
        let parsed: ProvidersConfig = toml::from_str(DEFAULTS_TOML).unwrap();
        let settings = parsed
            .get("local")
            .unwrap()
            .model_settings("never-declared");

        assert!(settings.reasoning_options.expect(DECLARED_WINS).is_empty());
    }

    #[test]
    fn model_def_reads_declared_reasoning_options() {
        let parsed: ProvidersConfig = toml::from_str(DECLARED_LEVELS_TOML).unwrap();
        let model = &parsed.get("local").unwrap().models[0];

        assert_eq!(model.id, "qwen3.8-27b");
        assert_eq!(model.fields.context_window, Some(229_376));
        assert_eq!(model.fields.max_output_tokens, Some(32_768));
        let options = model.fields.reasoning_options.clone().expect(DECLARED_WINS);
        assert_eq!(
            options.efforts(),
            ["none", "low", "medium", "xhigh"],
            "{DECLARED_WINS}"
        );
        assert!(
            options.can_disable(),
            "a declared none is how off is spelled"
        );
    }

    #[test]
    fn provider_def_roundtrip() {
        let mut config = ProvidersConfig::default();
        config.upsert(
            "my-provider".into(),
            ProviderDef {
                protocol: Some(Protocol::Openai),
                base_url: Some("https://api.example.com/v1".into()),
                api_key_env: Some("MY_API_KEY".into()),
                discover_models: true,
                enable_free_models: Some(false),
                ..Default::default()
            },
        );
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: ProvidersConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            parsed.get("my-provider").unwrap().protocol,
            Some(Protocol::Openai)
        );
        assert_eq!(
            parsed.get("my-provider").unwrap().base_url,
            Some("https://api.example.com/v1".into())
        );
        assert_eq!(
            parsed.get("my-provider").unwrap().enable_free_models,
            Some(false)
        );
    }

    const EMPTY_PROVIDER_DEF_TOML: &str = "";

    #[test]
    fn provider_def_enable_free_models_defaults_none() {
        let def: ProviderDef = toml::from_str(EMPTY_PROVIDER_DEF_TOML).unwrap();
        assert_eq!(def.enable_free_models, None);
    }

    const UNKNOWN_PURPOSE_TOML: &str = r#"[local.purposes]
compaction = "x"
"#;

    #[test]
    fn config_rejects_an_unknown_purpose() {
        let error = toml::from_str::<ProvidersConfig>(UNKNOWN_PURPOSE_TOML).unwrap_err();
        assert!(error.to_string().contains(UNKNOWN_PURPOSE), "{error}");
    }

    #[test_case("chat", ModelPurpose::Chat ; "chat")]
    #[test_case("fast", ModelPurpose::Fast ; "fast")]
    #[test_case("balanced", ModelPurpose::Balanced ; "balanced")]
    #[test_case("best", ModelPurpose::Best ; "best")]
    #[test_case("title", ModelPurpose::Title ; "title")]
    #[test_case("compact", ModelPurpose::Compact ; "compact")]
    #[test_case("goal", ModelPurpose::Goal ; "goal")]
    fn purpose_parses_and_renders_the_same_name(input: &str, expected: ModelPurpose) {
        assert_eq!(input.parse::<ModelPurpose>().unwrap(), expected);
        assert_eq!(expected.to_string(), input);
    }

    #[test]
    fn purpose_from_str_rejects_an_unknown_name() {
        assert!("weak".parse::<ModelPurpose>().is_err());
    }

    #[test_case("anthropic", None => "ANTHROPIC_API_KEY".to_string(); "builtin_default")]
    #[test_case("my-custom", None => "MY_CUSTOM_API_KEY".to_string(); "custom_default")]
    fn resolve_api_key_env_tests(slug: &str, def: Option<&ProviderDef>) -> String {
        resolve_api_key_env(slug, def)
    }

    #[test]
    fn resolve_base_url_prefers_def_over_none() {
        // Unique slug: `openai` would pick up a real OPENAI_BASE_URL from the shell.
        let slug = "caudra-test-def-over-none-slug";
        let def = ProviderDef {
            base_url: Some("http://proxy.local/v1".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_base_url(slug, Some(&def)).as_deref(),
            Some("http://proxy.local/v1")
        );
        assert_ne!(
            resolve_base_url(slug, Some(&def)),
            resolve_base_url(slug, None)
        );
    }

    #[test]
    fn resolve_base_url_empty_def_matches_none() {
        let slug = "caudra-test-empty-def-slug";
        let def = ProviderDef::default();
        assert_eq!(
            resolve_base_url(slug, Some(&def)),
            resolve_base_url(slug, None)
        );
    }

    #[test]
    fn resolve_base_url_custom_slug_uses_def() {
        let slug = "caudra-test-custom-base-url-slug";
        let def = ProviderDef {
            base_url: Some("http://xxxx:1234/v1".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_base_url(slug, Some(&def)).as_deref(),
            Some("http://xxxx:1234/v1")
        );
        assert_eq!(resolve_base_url(slug, None), None);
    }

    #[test]
    fn resolve_base_url_env_beats_def() {
        let slug = "caudra-test-env-base-url-slug";
        let env_var = base_url_env_var(slug);
        // SAFETY: unique test-only var; removed before the test returns.
        unsafe {
            std::env::set_var(&env_var, "http://env.local/v1");
        }
        let def = ProviderDef {
            base_url: Some("http://toml.local/v1".into()),
            ..Default::default()
        };
        let got = resolve_base_url(slug, Some(&def));
        unsafe {
            std::env::remove_var(&env_var);
        }
        assert_eq!(got.as_deref(), Some("http://env.local/v1"));
    }

    #[test]
    fn ignored_builtin_fields_lists_custom_only_fields() {
        let def = ProviderDef {
            base_url: Some("http://proxy.local/v1".into()),
            protocol: Some(Protocol::Openai),
            api_key_env: Some("MY_KEY".into()),
            discover_models: true,
            ..Default::default()
        };
        assert_eq!(
            ignored_builtin_fields("anthropic", &def),
            ["protocol", "api_key_env", "discover_models"]
        );
    }

    #[test]
    fn ignored_builtin_fields_keeps_opencode_free_models() {
        let def = ProviderDef {
            enable_free_models: Some(false),
            ..Default::default()
        };
        assert!(ignored_builtin_fields(OPENCODE_SLUG, &def).is_empty());
        assert_eq!(
            ignored_builtin_fields("openrouter", &def),
            ["enable_free_models"]
        );
    }

    #[test_case("MyProvider", "myprovider"; "mixed_case")]
    #[test_case("My Cool Provider", "my-cool-provider"; "spaces")]
    #[test_case("  my-provider  ", "my-provider"; "trimmed")]
    #[test_case("My--Provider", "my-provider"; "double_dash")]
    #[test_case("-my-provider-", "my-provider"; "leading_trailing_dash")]
    #[test_case("My_Provider", "my-provider"; "underscores")]
    #[test_case("My.Cool@Provider!", "my-cool-provider"; "special_chars")]
    fn slugify_tests(input: &str, expected: &str) {
        assert_eq!(slugify(input), expected);
    }
}
