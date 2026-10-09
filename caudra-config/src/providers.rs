use std::collections::HashMap;
use std::fs;
use std::io::Error;
use std::path::PathBuf;
use std::process;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};
use tracing::debug;

use caudra_storage::paths;
use caudra_storage::thinking::ReasoningOptions;

use crate::config_version::{CONFIG_VERSION_KEY, ConfigVersion};
use crate::{ConfigField, ConfigValue};

pub(crate) const PROVIDERS_FILE: &str = "providers.toml";
pub(crate) const PROVIDERS_VERSION: u32 = 1;
const BAD_CONFIG_EXIT_CODE: i32 = 2;
/// The only built-in that reads `enable_free_models`.
const OPENCODE_SLUG: &str = "opencode";
/// Keys a built-in slug ignores, because built-ins keep their compiled
/// protocol, model catalog and auth wiring. Opencode reads the last one.
pub const BUILTIN_IGNORED_FIELDS: [&str; 5] = [
    "protocol",
    "api_key_env",
    "discover_models",
    "models",
    "enable_free_models",
];
const DISCOVERED_OR_PROTOCOL: &str = "discovered, or the protocol default";
const OFF_UNLESS_DECLARED: &str = "false";
const UNPRICED: &str = "0";

/// The keys of a `[SLUG.purposes]` table.
pub(crate) const PURPOSE_FIELDS: &[ConfigField] = &[
    ConfigField {
        name: ModelPurpose::Fast.as_str(),
        ty: "string | string[]",
        default: ConfigValue::Unset,
        min: None,
        max: None,
        env: None,
        description: "Model id prefixes for small, fast models, best first. A prefix covers every id that starts with it, and the first one also names the model that fills the slot, so it has to be a real id",
    },
    ConfigField {
        name: ModelPurpose::Best.as_str(),
        ty: "string | string[]",
        default: ConfigValue::Unset,
        min: None,
        max: None,
        env: None,
        description: "Model id prefixes for flagship models, best first. A prefix cannot also be in `fast`. A model a job is bound to in the picker wins over both lists",
    },
];

/// A workload slot a model can be bound to. A purpose records what the user
/// wants a model used for, never a claim about what the model is capable of.
///
/// Lives here rather than in caudra-providers so the config layer can validate
/// bindings without depending on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelPurpose {
    Chat,
    Plan,
    Subagent,
    Compact,
    Title,
    Goal,
    Extract,
    Memory,
    Fast,
    Best,
}

impl ModelPurpose {
    pub const ALL: [Self; 10] = [
        Self::Chat,
        Self::Plan,
        Self::Subagent,
        Self::Compact,
        Self::Title,
        Self::Goal,
        Self::Extract,
        Self::Memory,
        Self::Fast,
        Self::Best,
    ];

    /// The purposes that select a size lane and may classify provider models.
    pub const CLASSES: [Self; 2] = [Self::Fast, Self::Best];

    /// Purposes that make useful binding targets in the model picker.
    pub const TARGETS: [Self; 4] = [Self::Chat, Self::Plan, Self::Fast, Self::Best];

    /// Title-cased name for pickers and docs. `Display` stays lowercase so it
    /// matches what the config and state rows hold.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Plan => "Plan",
            Self::Subagent => "Subagent",
            Self::Compact => "Compact",
            Self::Title => "Title",
            Self::Goal => "Goal",
            Self::Extract => "Extract",
            Self::Memory => "Memory",
            Self::Fast => "Fast",
            Self::Best => "Best",
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Plan => "plan",
            Self::Subagent => "subagent",
            Self::Compact => "compact",
            Self::Title => "title",
            Self::Goal => "goal",
            Self::Extract => "extract",
            Self::Memory => "memory",
            Self::Fast => "fast",
            Self::Best => "best",
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
    "unknown model purpose '{0}', expected one of: chat, plan, subagent, compact, title, goal, extract, memory, fast, best"
)]
pub struct UnknownPurpose(pub String);

fn deserialize_purposes<'de, D>(
    deserializer: D,
) -> Result<HashMap<ModelPurpose, PurposeModels>, D::Error>
where
    D: Deserializer<'de>,
{
    let purposes: HashMap<ModelPurpose, PurposeModels> = HashMap::deserialize(deserializer)?;
    if let Some(purpose) = ModelPurpose::ALL
        .into_iter()
        .find(|purpose| purposes.contains_key(purpose) && !ModelPurpose::CLASSES.contains(purpose))
    {
        return Err(serde::de::Error::custom(format!(
            "model purpose '{purpose}' cannot classify provider models; expected fast or best"
        )));
    }
    if let (Some(fast), Some(best)) = (
        purposes.get(&ModelPurpose::Fast),
        purposes.get(&ModelPurpose::Best),
    ) && let Some(prefix) = fast
        .iter()
        .find(|fast_prefix| best.iter().any(|best_prefix| best_prefix == *fast_prefix))
    {
        return Err(serde::de::Error::custom(format!(
            "model prefix '{prefix}' cannot be assigned to both fast and best"
        )));
    }
    Ok(purposes)
}

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "OneOrMany", into = "OneOrMany")]
pub struct PurposeModels(OneOrMany);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn as_slice(&self) -> &[String] {
        match self {
            Self::One(id) => std::slice::from_ref(id),
            Self::Many(ids) => ids,
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum InvalidPurposeModels {
    #[error("a model purpose must declare at least one model prefix")]
    Empty,
    #[error("model purpose prefix at index {0} must not be empty or whitespace-only")]
    EmptyPrefix(usize),
}

impl TryFrom<OneOrMany> for PurposeModels {
    type Error = InvalidPurposeModels;

    fn try_from(value: OneOrMany) -> Result<Self, Self::Error> {
        let prefixes = value.as_slice();
        if prefixes.is_empty() {
            return Err(InvalidPurposeModels::Empty);
        }
        if let Some(index) = prefixes.iter().position(|prefix| prefix.trim().is_empty()) {
            return Err(InvalidPurposeModels::EmptyPrefix(index));
        }
        Ok(Self(value))
    }
}

/// Round-trips back to the shape the operator wrote, so caudra rewriting this
/// file on upsert does not reformat a hand-written one-liner into a list.
impl From<PurposeModels> for OneOrMany {
    fn from(value: PurposeModels) -> Self {
        value.0
    }
}

impl PurposeModels {
    /// The id that wins the slot when several are declared.
    pub fn preferred(&self) -> Option<&str> {
        self.0.as_slice().first().map(String::as_str)
    }

    /// Whether `model_id` is one this purpose covers. Prefix, not equality, so a
    /// family of fine-tunes is one line rather than one line each.
    pub fn matches(&self, model_id: &str) -> bool {
        self.0
            .as_slice()
            .iter()
            .any(|prefix| model_id.starts_with(prefix))
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.as_slice().iter().map(String::as_str)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDef {
    pub id: String,
    #[serde(flatten)]
    pub fields: ModelFields,
}

impl ModelDef {
    /// The keys beside the flattened [`ModelFields::FIELDS`].
    pub(crate) const FIELDS: &[ConfigField] = &[ConfigField {
        name: "id",
        ty: "string",
        default: ConfigValue::Required("\"my-model\""),
        min: None,
        max: None,
        env: None,
        description: "The model id, which makes the spec `SLUG/ID`",
    }];
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_pdf: Option<bool>,
    /// The endpoint honours OpenAI's explicit `prompt_cache_breakpoint` on a
    /// Responses input block, so the system prompt closes with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_cache_breakpoints: Option<bool>,
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
    pub(crate) const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: "context_window",
            ty: "integer",
            default: ConfigValue::Varies(DISCOVERED_OR_PROTOCOL),
            min: None,
            max: None,
            env: None,
            description: "Tokens of context",
        },
        ConfigField {
            name: "max_output_tokens",
            ty: "integer",
            default: ConfigValue::Varies(DISCOVERED_OR_PROTOCOL),
            min: None,
            max: None,
            env: None,
            description: "The most tokens one response may hold",
        },
        ConfigField {
            name: "supports_tool_examples",
            ty: "bool",
            default: ConfigValue::Varies(OFF_UNLESS_DECLARED),
            min: None,
            max: None,
            env: None,
            description: "Send tool examples as a structured field. It is off unless declared, because the protocol says nothing about the model behind it",
        },
        ConfigField {
            name: "supports_thinking",
            ty: "bool",
            default: ConfigValue::Varies(DISCOVERED_OR_PROTOCOL),
            min: None,
            max: None,
            env: None,
            description: "The model accepts extended thinking",
        },
        ConfigField {
            name: "requires_thinking",
            ty: "bool",
            default: ConfigValue::Varies(OFF_UNLESS_DECLARED),
            min: None,
            max: None,
            env: None,
            description: "For an API that rejects requests with thinking off. It implies `supports_thinking` and raises thinking to minimal effort when it is off, compaction included",
        },
        ConfigField {
            name: "supports_vision",
            ty: "bool",
            default: ConfigValue::Varies(OFF_UNLESS_DECLARED),
            min: None,
            max: None,
            env: None,
            description: "The model accepts images. When false, image input and `view_image` are off",
        },
        ConfigField {
            name: "supports_pdf",
            ty: "bool",
            default: ConfigValue::Varies(OFF_UNLESS_DECLARED),
            min: None,
            max: None,
            env: None,
            description: "`anthropic` and `openai-responses` only. The model reads a PDF that `webfetch` attaches inside its tool result. When it is off, `webfetch` returns the text of the PDF instead",
        },
        ConfigField {
            name: "supports_cache_breakpoints",
            ty: "bool",
            default: ConfigValue::Varies(OFF_UNLESS_DECLARED),
            min: None,
            max: None,
            env: None,
            description: "`openai-responses` only. The endpoint honours an explicit `prompt_cache_breakpoint`, so the system prompt closes with one",
        },
        ConfigField {
            name: "reasoning_options",
            ty: "table[]",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "The reasoning controls the model takes, such as `[{ type = \"effort\", values = [\"low\", \"high\"] }]`. A `type` is `toggle`, `effort` with `values`, or `budget_tokens` with an optional `min` and `max`. `[]` declares that it takes none, so Caudra sends no reasoning level",
        },
        ConfigField {
            name: "pricing_input",
            ty: "float",
            default: ConfigValue::Varies(UNPRICED),
            min: None,
            max: None,
            env: None,
            description: "USD per million input tokens",
        },
        ConfigField {
            name: "pricing_output",
            ty: "float",
            default: ConfigValue::Varies(UNPRICED),
            min: None,
            max: None,
            env: None,
            description: "USD per million output tokens",
        },
        ConfigField {
            name: "pricing_cache_write",
            ty: "float",
            default: ConfigValue::Varies(UNPRICED),
            min: None,
            max: None,
            env: None,
            description: "USD per million tokens written to the prompt cache",
        },
        ConfigField {
            name: "pricing_cache_read",
            ty: "float",
            default: ConfigValue::Varies(UNPRICED),
            min: None,
            max: None,
            env: None,
            description: "USD per million tokens read from the prompt cache",
        },
        ConfigField {
            name: "pricing_fast_input",
            ty: "float",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "USD per million input tokens in fast mode",
        },
        ConfigField {
            name: "pricing_fast_output",
            ty: "float",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "USD per million output tokens in fast mode",
        },
    ];

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
            supports_pdf: self.supports_pdf.or(base.supports_pdf),
            supports_cache_breakpoints: self
                .supports_cache_breakpoints
                .or(base.supports_cache_breakpoints),
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

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProviderSlugError {
    #[error("provider name cannot be empty")]
    Empty,
    #[error(
        "`{key}` is reserved in {file}; choose another provider name",
        key = CONFIG_VERSION_KEY,
        file = PROVIDERS_FILE
    )]
    Reserved,
}

/// Slug for a new custom provider. Top-level keys of `providers.toml` are
/// slugs, so the file's own `version` key cannot be one.
pub fn custom_provider_slug(name: &str) -> Result<String, ProviderSlugError> {
    let slug = slugify(name);
    match slug.as_str() {
        "" => Err(ProviderSlugError::Empty),
        CONFIG_VERSION_KEY => Err(ProviderSlugError::Reserved),
        _ => Ok(slug),
    }
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

impl OverrideFields {
    pub(crate) const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: "context_window",
            ty: "integer",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "Tokens of context",
        },
        ConfigField {
            name: "max_output_tokens",
            ty: "integer",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "The most tokens one response may hold",
        },
        ConfigField {
            name: "supports_thinking",
            ty: "bool",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "The models accept extended thinking",
        },
        ConfigField {
            name: "supports_vision",
            ty: "bool",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "The models accept images",
        },
        ConfigField {
            name: "base",
            ty: "string",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "The native provider an opaque upstream works like, such as `llama-cpp`, `google`, or `anthropic`. Caudra warns about a value it does not know and ignores it",
        },
        ConfigField {
            name: "path_prefix",
            ty: "string",
            default: ConfigValue::Varies(
                "`/v1`, `/v1beta` for Gemini routes, none for Anthropic and Z.AI",
            ),
            min: None,
            max: None,
            env: None,
            description: "The path Caudra sends ahead of each request, which Aperture appends to the upstream base URL. Set it to `\"\"` when that URL already has its own path",
        },
    ];
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

impl ProviderOverride {
    /// The keys beside the flattened [`OverrideFields::FIELDS`].
    pub(crate) const FIELDS: &[ConfigField] = &[ConfigField {
        name: "models",
        ty: "table",
        default: ConfigValue::Toml("{}"),
        min: None,
        max: None,
        env: None,
        description: "Overrides for single models, keyed by model id, which win key by key. Quote an id that holds a dot, such as `models.\"qwen-3.6\"`",
    }];
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
    /// Which model ids are small or flagship choices for this provider.
    /// Checked-in defaults, outranked by an assignment made in the picker.
    #[serde(
        default,
        skip_serializing_if = "HashMap::is_empty",
        deserialize_with = "deserialize_purposes"
    )]
    pub purposes: HashMap<ModelPurpose, PurposeModels>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelDef>,
}

impl ProviderDef {
    pub(crate) const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: "display_name",
            ty: "string",
            default: ConfigValue::Varies("the built-in name, or the slug"),
            min: None,
            max: None,
            env: None,
            description: "The name pickers and auth status show",
        },
        ConfigField {
            name: "protocol",
            ty: "string",
            default: ConfigValue::Required("\"openai\""),
            min: None,
            max: None,
            env: None,
            description: "The wire format: `openai`, `openai-responses`, `anthropic`, or `google`",
        },
        ConfigField {
            name: "base_url",
            ty: "string",
            default: ConfigValue::Varies("the plan URL, or the built-in URL"),
            min: None,
            max: None,
            env: Some("<SLUG>_BASE_URL"),
            description: "The API origin. Caudra appends the protocol paths",
        },
        ConfigField {
            name: "plan",
            ty: "string",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "A built-in plan key, which sets the base URL and the default model",
        },
        ConfigField {
            name: "api_key_env",
            ty: "string",
            default: ConfigValue::Varies("`<SLUG>_API_KEY`"),
            min: None,
            max: None,
            env: None,
            description: "The environment variable that holds the API key",
        },
        ConfigField {
            name: "api_key",
            ty: "string",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "An API key, stored as plain text. Caudra tries the environment variable and saved credentials first",
        },
        ConfigField {
            name: "default_model",
            ty: "string",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "The model to use after login when none is saved yet, such as `my-provider/my-model`",
        },
        ConfigField {
            name: "discover_models",
            ty: "bool",
            default: ConfigValue::Bool(false),
            min: None,
            max: None,
            env: None,
            description: "Also list the models the provider's model endpoint reports",
        },
        ConfigField {
            name: "enable_free_models",
            ty: "bool",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "Opencode only. Show the free models of its catalog. Unset counts as `false`",
        },
        ConfigField {
            name: "overrides",
            ty: "table",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "Aperture only. Overrides for the upstream providers it routes, keyed by upstream id",
        },
        ConfigField {
            name: "model_defaults",
            ty: "table",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "Model keys for every model of the provider",
        },
        ConfigField {
            name: "purposes",
            ty: "table",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "Model id prefixes for the `fast` and `best` purposes",
        },
        ConfigField {
            name: "models",
            ty: "table[]",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "The models the provider serves",
        },
    ];

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
    /// Named ahead of the flattened map, so the key never reads as a slug.
    #[serde(default)]
    version: ConfigVersion<PROVIDERS_VERSION>,
    #[serde(flatten)]
    pub providers: HashMap<String, ProviderDef>,
}

impl ProvidersConfig {
    /// Read and parse `providers.toml`. Hard-exits on parse errors so a typo
    /// in a purpose or pricing surfaces immediately instead of silently dropping
    /// every provider and starting caudra with an empty registry.
    pub fn load() -> Self {
        let path = providers_file_path(paths::config_dir_path());
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
        let path = providers_file_path(paths::config_dir());
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

fn providers_file_path(directory: Result<PathBuf, Error>) -> PathBuf {
    directory
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
/// `my-proxy` -> `MY_PROXY_BASE_URL`). Ollama and llama.cpp never read it, they
/// take `OLLAMA_HOST` and `LLAMA_CPP_HOST`.
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
    let [
        protocol,
        api_key_env,
        discover_models,
        models,
        enable_free_models,
    ] = BUILTIN_IGNORED_FIELDS;
    [
        (protocol, def.protocol.is_some()),
        (api_key_env, def.api_key_env.is_some()),
        (discover_models, def.discover_models),
        (models, !def.models.is_empty()),
        (
            enable_free_models,
            def.enable_free_models.is_some() && slug != OPENCODE_SLUG,
        ),
    ]
    .into_iter()
    .filter_map(|(field, set)| set.then_some(field))
    .collect()
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
    use crate::config_version::ConfigVersionError;
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
supports_cache_breakpoints = true
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
        assert_eq!(settings.supports_cache_breakpoints, Some(true));
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

        assert_eq!(def.purposes.get(&ModelPurpose::Plan), None);
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

    const NON_CLASS_PURPOSE: &str = "cannot classify provider models; expected fast or best";
    const EMPTY_PURPOSE_MODELS: &str = "must declare at least one model prefix";
    const EMPTY_PURPOSE_PREFIX: &str = "must not be empty or whitespace-only";
    const DUPLICATE_PURPOSE_PREFIX: &str = "cannot be assigned to both fast and best";

    #[test_case("chat" ; "chat")]
    #[test_case("plan" ; "plan")]
    #[test_case("subagent" ; "subagent")]
    #[test_case("compact" ; "compact")]
    #[test_case("title" ; "title")]
    #[test_case("goal" ; "goal")]
    fn provider_purposes_reject_workload_keys(purpose: &str) {
        let input = format!("[local.purposes]\n{purpose} = \"model\"\n");
        let error = toml::from_str::<ProvidersConfig>(&input).unwrap_err();

        assert!(error.to_string().contains(NON_CLASS_PURPOSE), "{error}");
    }

    #[test_case("fast = []", EMPTY_PURPOSE_MODELS ; "empty_list")]
    #[test_case("fast = \"\"", EMPTY_PURPOSE_PREFIX ; "empty_string")]
    #[test_case("fast = \"   \"", EMPTY_PURPOSE_PREFIX ; "whitespace_string")]
    #[test_case("fast = [\"small\", \"\\t\"]", EMPTY_PURPOSE_PREFIX ; "whitespace_list_entry")]
    fn provider_purposes_reject_empty_model_prefixes(declaration: &str, expected: &str) {
        let input = format!("[local.purposes]\n{declaration}\n");
        let error = toml::from_str::<ProvidersConfig>(&input).unwrap_err();

        assert!(error.to_string().contains(expected), "{error}");
    }

    #[test]
    fn provider_purposes_reject_the_same_prefix_in_both_lanes() {
        let input = r#"
[local.purposes]
fast = ["shared", "small"]
best = ["large", "shared"]
"#;
        let error = toml::from_str::<ProvidersConfig>(input).unwrap_err();

        assert!(
            error.to_string().contains(DUPLICATE_PURPOSE_PREFIX),
            "{error}"
        );
    }

    #[test]
    fn provider_purposes_allow_distinct_overlapping_prefixes() {
        let input = r#"
[local.purposes]
fast = "gpt-4"
best = "gpt-4.1"
"#;

        assert!(toml::from_str::<ProvidersConfig>(input).is_ok());
    }

    #[test_case("\"model\"", false ; "one")]
    #[test_case("[\"model\"]", true ; "singleton_many")]
    #[test_case("[\"model\", \"other\"]", true ; "many")]
    fn purpose_models_preserve_their_one_or_many_shape(value: &str, expected_array: bool) {
        let input = format!("[local.purposes]\nfast = {value}\n");
        let parsed: ProvidersConfig = toml::from_str(&input).unwrap();
        let rewritten = toml::to_string_pretty(&parsed).unwrap();
        let rewritten_value: toml::Value = toml::from_str(&rewritten).unwrap();
        let fast = &rewritten_value["local"]["purposes"]["fast"];

        assert_eq!(fast.is_array(), expected_array, "{rewritten}");
        let reparsed: ProvidersConfig = toml::from_str(&rewritten).unwrap();
        assert_eq!(
            reparsed.get("local").unwrap().purposes[&ModelPurpose::Fast],
            parsed.get("local").unwrap().purposes[&ModelPurpose::Fast]
        );
    }

    #[test]
    fn purpose_models_validate_without_trimming_model_ids() {
        let input = "[local.purposes]\nfast = \" model \"\n";
        let parsed: ProvidersConfig = toml::from_str(input).unwrap();

        assert_eq!(
            parsed.get("local").unwrap().purposes[&ModelPurpose::Fast].preferred(),
            Some(" model ")
        );
    }

    #[test_case("chat", ModelPurpose::Chat ; "chat")]
    #[test_case("plan", ModelPurpose::Plan ; "plan")]
    #[test_case("subagent", ModelPurpose::Subagent ; "subagent")]
    #[test_case("compact", ModelPurpose::Compact ; "compact")]
    #[test_case("title", ModelPurpose::Title ; "title")]
    #[test_case("goal", ModelPurpose::Goal ; "goal")]
    #[test_case("extract", ModelPurpose::Extract ; "extract")]
    #[test_case("memory", ModelPurpose::Memory ; "memory")]
    #[test_case("fast", ModelPurpose::Fast ; "fast")]
    #[test_case("best", ModelPurpose::Best ; "best")]
    fn purpose_parses_and_renders_the_same_name(input: &str, expected: ModelPurpose) {
        assert_eq!(input.parse::<ModelPurpose>().unwrap(), expected);
        assert_eq!(expected.to_string(), input);
    }

    #[test]
    fn purpose_sets_keep_routing_order() {
        assert_eq!(
            ModelPurpose::ALL,
            [
                ModelPurpose::Chat,
                ModelPurpose::Plan,
                ModelPurpose::Subagent,
                ModelPurpose::Compact,
                ModelPurpose::Title,
                ModelPurpose::Goal,
                ModelPurpose::Extract,
                ModelPurpose::Memory,
                ModelPurpose::Fast,
                ModelPurpose::Best,
            ]
        );
        assert_eq!(
            ModelPurpose::CLASSES,
            [ModelPurpose::Fast, ModelPurpose::Best]
        );
        assert_eq!(
            ModelPurpose::TARGETS,
            [
                ModelPurpose::Chat,
                ModelPurpose::Plan,
                ModelPurpose::Fast,
                ModelPurpose::Best,
            ]
        );
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

    #[test_case("My Proxy", Ok("my-proxy".into()) ; "usable_name")]
    #[test_case("", Err(ProviderSlugError::Empty) ; "empty")]
    #[test_case(" -- ", Err(ProviderSlugError::Empty) ; "separators_only")]
    #[test_case(" Version ", Err(ProviderSlugError::Reserved) ; "reserved_after_slugify")]
    fn custom_provider_slug_refuses_unusable_names(
        name: &str,
        expected: Result<String, ProviderSlugError>,
    ) {
        assert_eq!(custom_provider_slug(name), expected);
    }

    /// Caudra rewrites the whole file on save, in the current shape.
    #[test]
    fn saving_stamps_the_version_without_turning_it_into_a_provider() {
        let unversioned: ProvidersConfig = toml::from_str(DEFAULTS_TOML).unwrap();
        let rewritten = toml::to_string_pretty(&unversioned).unwrap();
        let document: toml::Table = toml::from_str(&rewritten).unwrap();
        let reparsed: ProvidersConfig = toml::from_str(&rewritten).unwrap();

        assert_eq!(
            document[CONFIG_VERSION_KEY].as_integer(),
            Some(i64::from(PROVIDERS_VERSION)),
            "{rewritten}"
        );
        assert_eq!(reparsed.providers.keys().collect::<Vec<_>>(), ["local"]);
    }

    #[test_case(
        format!("{CONFIG_VERSION_KEY} = {}\n", PROVIDERS_VERSION + 1),
        ConfigVersionError::Newer {
            found: i64::from(PROVIDERS_VERSION + 1),
            latest: PROVIDERS_VERSION,
        }
        ; "newer_version"
    )]
    #[test_case(
        format!("[{CONFIG_VERSION_KEY}]\nprotocol = \"openai\"\n"),
        ConfigVersionError::Invalid
        ; "provider_named_version"
    )]
    fn config_rejects_a_version_it_cannot_read(document: String, expected: ConfigVersionError) {
        let error = toml::from_str::<ProvidersConfig>(&document).unwrap_err();

        assert!(error.to_string().contains(&expected.to_string()), "{error}");
    }
}
