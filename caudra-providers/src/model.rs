//! Model registry with prefix-based lookup and token accounting.
//! Lookup is prefix-based: `claude-sonnet-4-20250514` matches the `claude-sonnet-4` entry,
//! so dated snapshots resolve without registry churn. `context_tokens()` sums input + output
//! + cache reads/writes because the context window limit applies to all of them combined.

use std::any::Any;
use std::ops::AddAssign;
use std::sync::Arc;

use caudra_config::ModelPolicy;
pub use caudra_config::providers::ModelPurpose;
use caudra_config::providers::UnknownPurpose;
use caudra_storage::sessions::{StoredTokenUsage, cache_hit_rate};
use caudra_storage::thinking::{ReasoningOption, ReasoningOptions};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::manifest::{
    ManifestRegistry, ProviderManifest, catalog_pricing, catalog_reasoning_options,
};
use crate::model_registry::{self, Binding};
use crate::providers::{anthropic, custom, dynamic};
use crate::types::ThinkingFields;

const PER_MILLION: f64 = 1_000_000.0;
const TOKEN_THOUSAND: u64 = 1_000;
const TOKEN_MILLION: u64 = 1_000_000;
const TOKEN_TENTHS: u128 = 10;
pub(crate) const ANTHROPIC_SLUG: &str = "anthropic";
const GPT_PREFIX: &str = "gpt-";
const GPT_4_PREFIX: &str = "gpt-4";
const OPEN_WEIGHTS_PREFIX: &str = "gpt-oss";
/// OpenAI ids trained on the Codex envelope that never spell `gpt`.
const CODEX_TRAINED_PREFIXES: [&str; 3] = ["o3", "o4", "codex"];

/// Vendor slugs aggregators use that spell a builtin provider differently.
/// Aggregators follow OpenRouter's naming, which hyphenates some vendors caudra
/// does not, so an exact slug match alone would miss them.
const VENDOR_ALIASES: [(&str, &str); 5] = [
    ("z-ai", "zai"),
    ("x-ai", "xai"),
    ("mistralai", "mistral"),
    ("google-vertex", "google"),
    ("github-copilot", "copilot"),
];

fn builtin_for_vendor(vendor: &str) -> &str {
    VENDOR_ALIASES
        .iter()
        .find(|(alias, _)| *alias == vendor)
        .map_or(vendor, |(_, slug)| slug)
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("model must be in 'provider/model' format (e.g. anthropic/claude-sonnet-4-20250514)")]
    InvalidFormat,
    #[error("unsupported provider '{0}'")]
    UnsupportedProvider(String),
    #[error("unknown model '{0}'")]
    UnknownModel(String),
    #[error(transparent)]
    InvalidPurpose(#[from] UnknownPurpose),
    #[error("model '{0}' is not allowed by provider model policy")]
    NotAllowed(String),
    #[error("model purpose {0} is bound in a cycle")]
    PurposeCycle(ModelPurpose),
}

/// Rates that replace the base ones once a prompt crosses `above` tokens.
/// Anthropic's 1M window and Gemini 2.5 both bill this way, and a single flat
/// rate cannot express either.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PricingTier {
    /// Prompt tokens above which these rates apply.
    pub above: u32,
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_write: f64,
    #[serde(default)]
    pub cache_read: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    /// Anthropic fast mode charges a premium that differs per model. `None`
    /// means the model has no fast tier, so asking for fast mode quietly falls
    /// back to standard rates instead of overcharging.
    #[serde(default)]
    pub fast: Option<FastPricing>,
    /// Context-size tiers, ascending by `above`. Empty for the flat majority.
    #[serde(default)]
    pub tiers: Vec<PricingTier>,
}

/// Metadata discovered at runtime from a provider's `/models` endpoint.
/// All fields optional -- most providers only return an ID.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub pricing: Option<ModelPricing>,
    pub supports_thinking: Option<bool>,
    pub supports_vision: Option<bool>,
    /// Levels and bounds the provider just told us about, which outrank both
    /// the static table and the catalog.
    pub reasoning_options: Option<ReasoningOptions>,
    /// Store of additional metadata from the provider.
    pub provider_info: Option<Arc<dyn Any + Send + Sync>>,
}

impl ModelInfo {
    pub fn id_only(id: String) -> Self {
        Self {
            id,
            context_window: None,
            max_output_tokens: None,
            pricing: None,
            supports_thinking: None,
            supports_vision: None,
            reasoning_options: None,
            provider_info: None,
        }
    }
}

/// Cache rates are stated rather than derived from `input`, because the ratio
/// is the model's own: Opus 5.5 reads cache at a twentieth of its input rate
/// where every model before it read at a tenth, so deriving would double the
/// bill on the tokens agentic work is mostly made of.
#[derive(Debug, Clone, Deserialize)]
pub struct FastPricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

impl FastPricing {
    /// Rates for a source that publishes only the two headline numbers, such as
    /// a `providers.toml` endpoint: cache follows input with the multipliers
    /// Anthropic applies to its standard rates.
    pub fn derived(input: f64, output: f64) -> Self {
        Self {
            input,
            output,
            cache_write: input * ModelPricing::CACHE_WRITE_MULTIPLIER,
            cache_read: input * ModelPricing::CACHE_READ_MULTIPLIER,
        }
    }
}

impl ModelPricing {
    pub const ZERO: Self = Self {
        input: 0.0,
        output: 0.0,
        cache_write: 0.0,
        cache_read: 0.0,
        fast: None,
        tiers: Vec::new(),
    };

    pub fn is_zero(&self) -> bool {
        self.input == 0.0 && self.output == 0.0 && self.cache_write == 0.0 && self.cache_read == 0.0
    }

    /// Rates for a prompt of `prompt_tokens`: the highest tier it crosses, else
    /// the base rates. Tiers are ascending, so the last match wins.
    fn rates_at(&self, prompt_tokens: u32) -> (f64, f64, f64, f64) {
        self.tiers
            .iter()
            .take_while(|tier| prompt_tokens > tier.above)
            .last()
            .map_or(
                (self.input, self.output, self.cache_write, self.cache_read),
                |tier| (tier.input, tier.output, tier.cache_write, tier.cache_read),
            )
    }

    /// Cache multipliers Anthropic applies on top of the base input rate.
    const CACHE_WRITE_MULTIPLIER: f64 = 1.25;
    const CACHE_READ_MULTIPLIER: f64 = 0.10;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFamily {
    Claude,
    Generic,
    Gemini,
    Glm,
    Gpt,
    Synthetic,
}

/// Provider-supplied size and default facts, kept separate so a known model is
/// not mistaken for a preferred one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFacts {
    /// Whether the provider deliberately presents this as a small model.
    pub small: bool,
    /// Whether this is the preferred model in its size lane.
    pub default: bool,
}

/// Marker suitable for presenting model supply metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelMarker {
    Small,
    Fast,
    Best,
}

impl ModelFacts {
    pub const fn class(&self) -> ModelPurpose {
        if self.small {
            ModelPurpose::Fast
        } else {
            ModelPurpose::Best
        }
    }

    pub const fn marker(&self) -> Option<ModelMarker> {
        match (self.small, self.default) {
            (true, false) => Some(ModelMarker::Small),
            (true, true) => Some(ModelMarker::Fast),
            (false, true) => Some(ModelMarker::Best),
            (false, false) => None,
        }
    }
}

/// Const-constructible mirror of [`ReasoningOption`], so the static tables can
/// carry a correction for a model the catalog gets wrong or has not reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticReasoningOption {
    Toggle,
    Effort(&'static [&'static str]),
    BudgetTokens { min: Option<u32>, max: Option<u32> },
}

pub(crate) fn reasoning_options_from_static(options: &[StaticReasoningOption]) -> ReasoningOptions {
    ReasoningOptions::new(
        options
            .iter()
            .map(|option| match option {
                StaticReasoningOption::Toggle => ReasoningOption::Toggle,
                StaticReasoningOption::Effort(values) => ReasoningOption::Effort {
                    values: values.iter().map(|v| (*v).to_string()).collect(),
                },
                StaticReasoningOption::BudgetTokens { min, max } => ReasoningOption::BudgetTokens {
                    min: *min,
                    max: *max,
                },
            })
            .collect(),
    )
}

#[derive(Debug)]
pub struct ModelEntry {
    pub prefixes: &'static [&'static str],
    /// Whether this is a deliberately small model. False means known non-small,
    /// not that it is the provider's flagship.
    pub small: bool,
    pub family: ModelFamily,
    /// Gates vision-only tools (`view_image`) and image blocks at request time.
    pub vision: bool,
    /// The preferred model within its small or non-small lane.
    pub default: bool,
    pub pricing: ModelPricing,
    pub max_output_tokens: Option<u32>,
    pub context_window: u32,
    /// Corrects the catalog where it describes the model rather than the
    /// request caudra sends. `None` defers to discovery and models.dev.
    pub reasoning_options: Option<&'static [StaticReasoningOption]>,
}

/// A release line, so a routing lane can answer inside the line the user is
/// already on: moving to `gpt-6-sol` should move Fast to `gpt-6-luna` rather
/// than leave it on the previous generation's small model.
///
/// `members` are id prefixes. The longest match wins, so a line can be spelled
/// as broadly as `gpt-6-` without capturing a narrower line declared beside it.
#[derive(Debug, Clone, Copy)]
pub struct ModelGeneration {
    pub label: &'static str,
    pub members: &'static [&'static str],
}

impl ModelGeneration {
    /// Whether a curated entry sits in this line. Read from the entry's own
    /// prefixes rather than a separate list, so the two can never disagree.
    pub fn contains(&self, entry: &ModelEntry) -> bool {
        entry
            .prefixes
            .iter()
            .any(|prefix| self.members.iter().any(|member| prefix.starts_with(member)))
    }
}

impl ModelEntry {
    pub const fn facts(&self) -> ModelFacts {
        ModelFacts {
            small: self.small,
            default: self.default,
        }
    }

    /// Compatibility size class for routing callers.
    pub const fn class(&self) -> ModelPurpose {
        self.facts().class()
    }
}

pub(crate) fn lookup_entry<'a>(
    entries: &'a [ModelEntry],
    model_id: &str,
) -> Result<&'a ModelEntry, ModelError> {
    entries
        .iter()
        .flat_map(|e| e.prefixes.iter().map(move |p| (p, e)))
        .filter(|(p, _)| model_id.starts_with(*p))
        .max_by_key(|(p, _)| p.len())
        .map(|(_, e)| e)
        .ok_or_else(|| ModelError::UnknownModel(model_id.to_string()))
}

impl ModelFamily {
    pub fn supports_tool_examples(self) -> bool {
        match self {
            ModelFamily::Claude | ModelFamily::Gpt | ModelFamily::Synthetic => true,
            ModelFamily::Generic | ModelFamily::Gemini | ModelFamily::Glm => false,
        }
    }

    /// Fallback for models missing from the static tables; per-model truth
    /// lives in `ModelEntry::vision`.
    pub fn supports_vision(self) -> bool {
        matches!(self, Self::Claude | Self::Gpt | Self::Gemini)
    }
}

/// `Required` marks APIs that reject requests with thinking disabled;
/// [`crate::RequestOptions::clamped`] raises `Off` to minimal effort for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingSupport {
    No,
    Yes,
    Required,
}

impl ThinkingSupport {
    /// `requires` wins: an API that rejects thinking-off requests
    /// necessarily supports thinking.
    pub fn from_flags(supports: Option<bool>, requires: bool) -> Option<Self> {
        match (requires, supports) {
            (true, _) => Some(Self::Required),
            (false, Some(true)) => Some(Self::Yes),
            (false, Some(false)) => Some(Self::No),
            (false, None) => None,
        }
    }
}

/// Who pays for a turn. A subscription price is the API list price for the same
/// tokens: the arithmetic is real, the invoice is not, so the two are counted
/// apart everywhere rather than summed into one misleading total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Billing {
    #[default]
    Api,
    Subscription,
}

impl Billing {
    pub fn from_oauth(oauth: bool) -> Self {
        if oauth { Self::Subscription } else { Self::Api }
    }

    pub fn is_subscription(self) -> bool {
        matches!(self, Self::Subscription)
    }
}

#[derive(Debug, Clone)]
pub struct Model {
    pub id: String,
    pub provider: Arc<str>,
    pub family: ModelFamily,
    pub supports_tool_examples_override: Option<bool>,
    /// Resolved thinking support, used by gateway providers (e.g. Aperture)
    /// that stream through a native provider chosen at runtime. `None` falls
    /// back to discovery, then the provider manifest.
    pub thinking_override: Option<ThinkingSupport>,
    pub supports_vision_override: Option<bool>,
    /// Declared by a custom endpoint that honours OpenAI's explicit
    /// `prompt_cache_breakpoint`; the builtin OpenAI provider decides by family.
    pub supports_cache_breakpoints_override: Option<bool>,
    pub pricing: ModelPricing,
    /// Discovery reported an explicit all-zero price. Distinct from a zero
    /// `pricing`, which also covers "no price is known".
    pub discovered_free: bool,
    /// `None` when unknown, see [`ProviderKind::fallback_max_output`].
    pub max_output_tokens: Option<u32>,
    pub context_window: u32,
    /// `context_window` is an input budget and `max_output_tokens` is granted on
    /// top of it, rather than the API total the two share. True only for the
    /// working windows caudra caps itself, which is why they need less reserved
    /// for compaction than a total does.
    pub window_excludes_output: bool,
    pub thinking_fields: Option<Box<ThinkingFields>>,
    /// Levels and bounds this model accepts, resolved once at construction so a
    /// request never needs a live catalog lookup. Empty when nothing declared
    /// them, which callers read as "send nothing and take the API default".
    pub reasoning_options: ReasoningOptions,
    /// Whether a turn on this model is invoiced. Providers that serve both an
    /// API key and a subscription set it from their auth in `adjust_model`, so
    /// a model built from a spec alone is `Api` until a provider says otherwise.
    pub billing: Billing,
}

#[derive(Clone, Copy)]
enum CatalogAccess {
    Warm,
    IfAvailable,
}

/// `ManifestRegistry::for_slug` resolves a custom slug to its base provider's
/// manifest so stubs still get thinking, display-name, and window defaults. Those
/// are transport concerns. Lineage is not: a custom provider's base is its wire
/// protocol, and an OpenAI-shaped endpoint serves whatever the operator loaded,
/// so a borrowed manifest never lends its family.
///
/// A dynamic provider names a real upstream, which is a lineage claim, so it
/// keeps inheriting. `for_slug` tries builtin, then dynamic, then custom, so a
/// borrowed manifest that is not dynamic was borrowed by a custom slug.
fn inherited_lineage(
    manifest: &ProviderManifest,
    slug: &str,
    entry: Option<&ModelEntry>,
) -> ModelFamily {
    let borrowed_by_custom = slug != manifest.slug && dynamic::base_for_slug(slug).is_none();
    if borrowed_by_custom {
        return ModelFamily::Generic;
    }
    entry.map_or(manifest.family, |entry| entry.family)
}

impl Model {
    /// When no static entry matches (a freshly released model the table has not
    /// caught up to yet), fall back to the provider defaults so it still resolves.
    fn from_base(manifest: &ProviderManifest, slug: &str, model_id: &str) -> Self {
        let static_entry = lookup_entry(manifest.models, model_id).ok();
        // A wrapper's own listing wins. Before it has listed, metadata already
        // discovered through the builtin remains a useful fallback.
        let discovered = model_registry::discovered(slug, model_id).or_else(|| {
            (slug != manifest.slug)
                .then(|| model_registry::discovered(manifest.slug, model_id))
                .flatten()
        });
        let discovered = discovered.as_ref();
        let family = inherited_lineage(manifest, slug, static_entry);
        // One lookup feeds pricing, limits and the reasoning ladder below.
        let catalog = manifest.catalog_meta(model_id);
        let catalog = catalog.as_ref();
        let discovered_pricing = discovered.and_then(|info| info.pricing.as_ref());
        // A static entry's rates win, but it cannot carry tiers, so the catalog
        // still supplies those when the two describe the same model.
        let pricing = discovered_pricing
            .cloned()
            .or_else(|| {
                static_entry.map(|entry| ModelPricing {
                    tiers: catalog_pricing(catalog)
                        .map(|catalog| catalog.tiers)
                        .unwrap_or_default(),
                    ..entry.pricing.clone()
                })
            })
            .or_else(|| catalog_pricing(catalog))
            .unwrap_or_default();
        let max_output_tokens = discovered
            .and_then(|info| info.max_output_tokens)
            .or_else(|| static_entry.and_then(|entry| entry.max_output_tokens))
            .or_else(|| catalog.map(|meta| meta.output))
            .or(manifest.fallback_max_output);
        // Whichever source supplies the window also says whether it is an input
        // budget, because only the source knows. Asking the resolved number
        // instead would flag any window that happened to equal one caudra caps
        // itself at, and miss one that a provider reported for itself.
        let (context_window, window_excludes_output) = discovered
            .and_then(|info| info.context_window)
            .map(|window| (window, false))
            .or_else(|| anthropic::shared::long_context_window(model_id).map(|w| (w, false)))
            .or_else(|| {
                static_entry.map(|entry| {
                    (
                        entry.context_window,
                        anthropic::shared::declares_input_budget(manifest.slug, entry),
                    )
                })
            })
            .or_else(|| catalog.map(|meta| (meta.context, meta.context_excludes_output)))
            .unwrap_or((manifest.fallback_context_window, false));
        // The static entry wins over the catalog on purpose: it is where caudra
        // records what a *request* accepts, which is not always what the model
        // is capable of. Discovery still wins over both, since the provider
        // just told us.
        let reasoning_options = discovered
            .and_then(|info| info.reasoning_options.clone())
            .or_else(|| {
                static_entry
                    .and_then(|entry| entry.reasoning_options)
                    .map(reasoning_options_from_static)
            })
            .or_else(|| catalog_reasoning_options(catalog))
            .unwrap_or_default();
        Self {
            id: model_id.to_string(),
            provider: Arc::from(slug),
            family,
            supports_tool_examples_override: None,
            thinking_override: None,
            supports_vision_override: None,
            supports_cache_breakpoints_override: None,
            pricing,
            discovered_free: discovered_pricing.is_some_and(ModelPricing::is_zero),
            max_output_tokens,
            context_window,
            window_excludes_output,
            thinking_fields: None,
            reasoning_options,
            billing: Billing::default(),
        }
    }

    /// Build a `Model` from a models.dev catalogue sub-provider (nvidia,
    /// fireworks, groq, ...). The slug is the catalogue sub-provider key, not a
    /// builtin; metadata is read once from the models.dev catalog and cached on
    /// the `Model` so `supports_thinking`/`supports_vision` do not need a live
    /// catalog lookup.
    fn from_catalog(
        slug: &str,
        model_id: &str,
        meta: crate::providers::catalog::CatalogMetaView,
    ) -> Self {
        Self {
            id: model_id.to_string(),
            provider: Arc::from(slug),
            family: ModelFamily::Generic,
            supports_tool_examples_override: None,
            thinking_override: ThinkingSupport::from_flags(Some(meta.supports_thinking), false),
            supports_vision_override: Some(meta.supports_vision),
            supports_cache_breakpoints_override: None,
            pricing: ModelPricing {
                input: meta.input_price,
                output: meta.output_price,
                cache_write: meta.cache_write,
                cache_read: meta.cache_read,
                fast: None,
                tiers: meta.pricing_tiers,
            },
            discovered_free: false,
            max_output_tokens: Some(meta.output),
            context_window: meta.context,
            window_excludes_output: meta.context_excludes_output,
            thinking_fields: None,
            reasoning_options: meta.reasoning_options,
            billing: Billing::default(),
        }
    }

    /// What the model says it accepts. A local model spells its levels as
    /// `thinking_fields` keys, so read them there rather than making the user
    /// declare the same thing twice.
    pub fn reasoning_options(&self) -> ReasoningOptions {
        match &self.thinking_fields {
            Some(fields) if self.reasoning_options.is_empty() => fields.reasoning_options(),
            _ => self.reasoning_options.clone(),
        }
    }

    pub fn supports_thinking(&self) -> bool {
        if let Some(thinking) = self.thinking_override {
            return thinking != ThinkingSupport::No;
        }
        // Discovery keys `known_models` by the builtin slug; resolve dynamic
        // and custom slugs through their base manifest before looking up.
        let Some(manifest) = ManifestRegistry::for_slug(&self.provider) else {
            return false;
        };
        model_registry::discovered(manifest.slug, &self.id)
            .and_then(|d| d.supports_thinking)
            .unwrap_or(manifest.supports_thinking)
    }

    /// A model that cannot be asked to stop reasoning. Read from what it
    /// declared, so a new model needs no flag; the override stays for gateways
    /// that know better than the catalog.
    pub fn requires_thinking(&self) -> bool {
        self.thinking_override == Some(ThinkingSupport::Required)
            || !self.reasoning_options().can_disable()
    }

    pub fn supports_vision(&self) -> bool {
        if let Some(vision) = self.supports_vision_override {
            return vision;
        }
        let manifest = ManifestRegistry::for_slug(&self.provider);
        manifest
            .and_then(|m| {
                model_registry::discovered(m.slug, &self.id).and_then(|d| d.supports_vision)
            })
            .or_else(|| {
                manifest
                    .and_then(|m| lookup_entry(m.models, &self.id).ok())
                    .map(|e| e.vision)
            })
            .unwrap_or_else(|| self.family.supports_vision())
    }

    pub fn supports_tool_examples(&self) -> bool {
        self.supports_tool_examples_override
            .unwrap_or_else(|| self.family.supports_tool_examples())
    }

    pub fn supports_cache_breakpoints(&self) -> bool {
        self.supports_cache_breakpoints_override.unwrap_or(false)
    }

    /// Which of the two editing contracts this model was trained on, so it is
    /// offered one rather than asked to choose. GPT-5 and its successors are
    /// trained on the Codex `apply_patch` envelope; everything else does better
    /// with string replacement.
    ///
    /// Matched on the id alone. [`ModelFamily`] cannot answer this: it is a
    /// per-provider label that Copilot and OpenRouter set to `Generic` for real
    /// `gpt-5*` weights, and that every OpenAI-shaped custom endpoint would
    /// otherwise set to `Gpt` for weights that are not GPT at all.
    ///
    /// `gpt-4*` predates the format and the open-weight `gpt-oss*` line was not
    /// trained on it, so both stay on string replacement. The reasoning and
    /// Codex ids never say `gpt`, so they are named.
    pub fn prefers_apply_patch(&self) -> bool {
        let id = self.id.to_ascii_lowercase();
        if id.contains(GPT_4_PREFIX) || id.starts_with(OPEN_WEIGHTS_PREFIX) {
            return false;
        }
        id.contains(GPT_PREFIX) || CODEX_TRAINED_PREFIXES.iter().any(|p| id.starts_with(p))
    }

    /// A model supports fast mode exactly when it carries fast-tier pricing, so
    /// capability and billing can never disagree. The provider gate keeps fast
    /// mode to Anthropic-based providers, resolved through the base manifest so
    /// oauth scripts keep it; Bedrock separately ignores `opts.fast` at request
    /// time.
    pub fn supports_fast(&self) -> bool {
        self.pricing.fast.is_some()
            && ManifestRegistry::for_slug(&self.provider).is_some_and(|m| m.slug == ANTHROPIC_SLUG)
    }

    pub fn spec(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    /// What the provider charges right now, so it is only ever correct for a
    /// turn that just finished: under a
    /// [`PricingSchedule`](crate::pricing::PricingSchedule) the answer moves
    /// with the clock. Anything historical wants [`Self::list_cost`].
    ///
    /// `None` on an unpriced model (oauth, local), so callers can hide the cost
    /// instead of showing a misleading "$0.000".
    pub fn billed_cost(&self, usage: &TokenUsage, fast: bool) -> Option<f64> {
        let cost = self.list_cost(usage, fast)?;
        let schedule = ManifestRegistry::for_slug(&self.provider).and_then(|m| m.pricing_schedule);
        Some(schedule.map_or(cost, |s| cost * s.multiplier_at(Timestamp::now())))
    }

    /// The quoted rates, with no wall-clock surcharge. Deterministic, which is
    /// what makes it right for re-pricing a session whose turns never recorded
    /// what they paid: the rate back then is unknown, and the table price is
    /// the honest guess.
    pub fn list_cost(&self, usage: &TokenUsage, fast: bool) -> Option<f64> {
        (!self.pricing.is_zero()).then(|| usage.cost(&self.pricing, fast))
    }

    pub fn provider_display_name(&self) -> &'static str {
        ManifestRegistry::for_slug(&self.provider).map_or("Unknown", |m| m.display_name)
    }

    /// Which model serves `purpose` while `anchor` is the caller's current model.
    ///
    /// A binding the user made wins. Otherwise each purpose falls back to its
    /// own rule, and every rule ends at `anchor`, so resolution never invents an
    /// id the active provider does not serve.
    pub fn resolve(
        purpose: ModelPurpose,
        anchor: &Self,
        policy: &ModelPolicy,
    ) -> Result<Self, ModelError> {
        Self::resolve_seen(
            purpose,
            anchor,
            policy,
            &mut Vec::new(),
            CatalogAccess::Warm,
        )
    }

    /// Non-warming variant of [`Self::resolve`] for latency-sensitive callers.
    /// An exact binding whose catalog provider is not already available returns
    /// [`ModelError::UnsupportedProvider`] without loading the catalog.
    pub fn resolve_if_available(
        purpose: ModelPurpose,
        anchor: &Self,
        policy: &ModelPolicy,
    ) -> Result<Self, ModelError> {
        Self::resolve_seen(
            purpose,
            anchor,
            policy,
            &mut Vec::new(),
            CatalogAccess::IfAvailable,
        )
    }

    /// [`Self::resolve`] against a binding the caller already read, so a caller
    /// that reports which binding it used cannot resolve a different one.
    pub fn resolve_binding(
        purpose: ModelPurpose,
        binding: Option<&Binding>,
        anchor: &Self,
        policy: &ModelPolicy,
    ) -> Result<Self, ModelError> {
        Self::resolve_captured_binding(purpose, binding, anchor, policy, CatalogAccess::Warm)
    }

    /// Non-warming variant of [`Self::resolve_binding`].
    pub fn resolve_binding_if_available(
        purpose: ModelPurpose,
        binding: Option<&Binding>,
        anchor: &Self,
        policy: &ModelPolicy,
    ) -> Result<Self, ModelError> {
        Self::resolve_captured_binding(purpose, binding, anchor, policy, CatalogAccess::IfAvailable)
    }

    fn resolve_captured_binding(
        purpose: ModelPurpose,
        binding: Option<&Binding>,
        anchor: &Self,
        policy: &ModelPolicy,
        catalog_access: CatalogAccess,
    ) -> Result<Self, ModelError> {
        let seen = &mut vec![purpose];
        match binding {
            Some(binding) => Self::follow(binding, anchor, policy, seen, catalog_access),
            None => Self::auto(purpose, anchor, policy, seen, catalog_access),
        }
    }

    fn resolve_seen(
        purpose: ModelPurpose,
        anchor: &Self,
        policy: &ModelPolicy,
        seen: &mut Vec<ModelPurpose>,
        catalog_access: CatalogAccess,
    ) -> Result<Self, ModelError> {
        if seen.contains(&purpose) {
            return Err(ModelError::PurposeCycle(purpose));
        }
        seen.push(purpose);
        match model_registry::binding(purpose) {
            Some(binding) => Self::follow(&binding, anchor, policy, seen, catalog_access),
            None => Self::auto(purpose, anchor, policy, seen, catalog_access),
        }
    }

    fn follow(
        binding: &Binding,
        anchor: &Self,
        policy: &ModelPolicy,
        seen: &mut Vec<ModelPurpose>,
        catalog_access: CatalogAccess,
    ) -> Result<Self, ModelError> {
        match binding {
            Binding::Exact(spec) => {
                match (catalog_access, Self::from_spec_with_policy(spec, policy)) {
                    (CatalogAccess::Warm, Err(ModelError::UnsupportedProvider(_))) => {
                        crate::warm_catalog();
                        Self::from_spec_with_policy(spec, policy)
                    }
                    (_, result) => result,
                }
            }
            Binding::Same(other) => {
                Self::resolve_seen(*other, anchor, policy, seen, catalog_access)
            }
        }
    }

    fn auto(
        purpose: ModelPurpose,
        anchor: &Self,
        policy: &ModelPolicy,
        seen: &mut Vec<ModelPurpose>,
        catalog_access: CatalogAccess,
    ) -> Result<Self, ModelError> {
        match purpose {
            ModelPurpose::Chat
            | ModelPurpose::Plan
            | ModelPurpose::Subagent
            | ModelPurpose::Compact => Ok(anchor.clone()),
            ModelPurpose::Title | ModelPurpose::Goal | ModelPurpose::Extract => {
                Self::resolve_seen(ModelPurpose::Fast, anchor, policy, seen, catalog_access)
            }
            ModelPurpose::Fast | ModelPurpose::Best => {
                Ok(Self::provider_default(purpose, anchor, policy)
                    .unwrap_or_else(|| anchor.clone()))
            }
        }
    }

    /// The provider's own answer for a slot: a `providers.toml` binding, then the
    /// curated table, then the cheapest price the provider reported.
    ///
    /// Only Fast consults price. It is a sound proxy for cheap and an unsound one
    /// for capable, so Best stops at the curated table and lets the caller fall
    /// back to the model the user already chose.
    fn provider_default(
        purpose: ModelPurpose,
        anchor: &Self,
        policy: &ModelPolicy,
    ) -> Option<Self> {
        let slug = anchor.provider.as_ref();
        let cheapest = (purpose == ModelPurpose::Fast)
            .then(|| {
                model_registry::cheapest_known(slug)
                    .or_else(|| model_registry::smallest_known(slug))
            })
            .flatten();
        custom::declared_purpose(slug, purpose)
            .into_iter()
            .chain(
                ManifestRegistry::prefixes_for_purpose(slug, purpose, &anchor.id)
                    .into_iter()
                    .map(str::to_string),
            )
            .chain(cheapest)
            .map(|model_id| format!("{slug}/{model_id}"))
            .filter(|spec| policy.allows(spec))
            .find_map(|spec| Self::from_spec(&spec).ok())
    }

    /// Supply facts declared by the operator or the curated provider table.
    /// Aggregators borrow the upstream provider's facts after removing their
    /// vendor prefix.
    ///
    /// `None` is the honest answer for a model nobody described. Nothing infers
    /// facts from price or discovery order.
    pub fn facts_of(provider: &str, model_id: &str) -> Option<ModelFacts> {
        custom::facts_for_model(provider, model_id)
            .or_else(|| ManifestRegistry::facts_for_model(provider, model_id))
            .or_else(|| {
                let (vendor, upstream) = model_id.split_once('/')?;
                ManifestRegistry::facts_for_model(builtin_for_vendor(vendor), upstream)
            })
    }

    pub fn marker_of(provider: &str, model_id: &str) -> Option<ModelMarker> {
        Self::facts_of(provider, model_id).and_then(|facts| facts.marker())
    }

    /// Compatibility view of [`Self::facts_of`] for routing consumers. It
    /// classifies every known model by size, including non-default models that
    /// intentionally have no [`ModelMarker`].
    pub fn class_of(provider: &str, model_id: &str) -> Option<ModelPurpose> {
        Self::facts_of(provider, model_id).map(|facts| facts.class())
    }

    /// Curated default for a builtin provider, with no conversation to fall back
    /// on. First-run setup only, before any model has been chosen.
    pub fn curated_default(slug: &str, purpose: ModelPurpose) -> Option<Self> {
        let entry = ManifestRegistry::find_default_for_purpose(slug, purpose)?;
        Self::from_spec(&format!("{slug}/{}", entry.prefixes[0])).ok()
    }

    pub fn from_spec_with_policy(spec: &str, policy: &ModelPolicy) -> Result<Self, ModelError> {
        if !policy.allows(spec) {
            return Err(ModelError::NotAllowed(spec.to_string()));
        }
        Self::from_spec(spec)
    }

    pub fn from_spec(spec: &str) -> Result<Self, ModelError> {
        let (slug, model_id) = spec.split_once('/').ok_or(ModelError::InvalidFormat)?;

        // Precedence: builtin, then dynamic script, then providers.toml custom,
        // then models.dev catalogue sub-provider.
        // Discovery drops any script slug a builtin or custom entry already owns,
        // so a script and a custom provider can never share a slug here.
        if let Some(manifest) = ManifestRegistry::get(slug) {
            return Ok(Self::from_base(manifest, slug, model_id));
        }

        if let Some(model) = dynamic::lookup_model(slug, model_id) {
            return Ok(model);
        }

        if let Some(base) = dynamic::base_for_slug(slug)
            && let Some(manifest) = ManifestRegistry::get(&base.to_string())
        {
            return Ok(Self::from_base(manifest, slug, model_id));
        }

        if let Some(model) = custom::lookup_model(slug, model_id) {
            return Ok(model);
        }

        if let Some(meta) = crate::providers::catalog::model_meta_if_available(slug, model_id) {
            return Ok(Self::from_catalog(slug, model_id, meta));
        }

        Err(ModelError::UnsupportedProvider(slug.to_string()))
    }

    /// Free public models surfaced through the OpenCode provider (Zen/Go),
    /// using the catalog's definition of free (zero input and output price),
    /// the same one that gates `enable_free_models`, plus models a provider's
    /// `/models` call reported at an explicit zero price.
    ///
    /// Queries the live catalog rather than `self.pricing`, which may not yet
    /// reflect catalog prices when discovery hasn't seeded the registry, and
    /// which reads zero for "price unknown" too.
    pub fn is_free(&self) -> bool {
        self.discovered_free
            || crate::providers::catalog::free_model_if_available(&self.provider, &self.id)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Non-cached input tokens. Total input = `input + cache_read + cache_creation`.
    #[serde(rename = "input_tokens")]
    pub input: u32,
    #[serde(rename = "output_tokens")]
    pub output: u32,
    #[serde(rename = "cache_creation_input_tokens")]
    pub cache_creation: u32,
    #[serde(rename = "cache_read_input_tokens")]
    pub cache_read: u32,
}

impl From<StoredTokenUsage> for TokenUsage {
    fn from(s: StoredTokenUsage) -> Self {
        Self {
            input: s.input,
            output: s.output,
            cache_creation: s.cache_creation,
            cache_read: s.cache_read,
        }
    }
}

impl TokenUsage {
    /// Ready to store, with what the turn was billed and who owes it. No
    /// `From<TokenUsage>` on purpose: a caller that forgets the cost quietly
    /// loses money from the session total, so saying it out loud is mandatory.
    pub fn billed(&self, cost: Option<f64>, billing: Billing) -> StoredTokenUsage {
        match billing {
            Billing::Api => self.spent(cost, None),
            Billing::Subscription => self.spent(None, cost),
        }
    }

    /// For a caller holding both columns already, such as a goal that ran turns
    /// under an API key and a subscription both.
    pub fn spent(&self, cost: Option<f64>, subscription_cost: Option<f64>) -> StoredTokenUsage {
        StoredTokenUsage {
            input: self.input,
            output: self.output,
            cache_creation: self.cache_creation,
            cache_read: self.cache_read,
            cost,
            subscription_cost,
        }
    }

    pub fn total_input(&self) -> u32 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_creation)
    }

    pub fn context_tokens(&self) -> u32 {
        self.total_input().saturating_add(self.output)
    }

    /// See [`caudra_storage::sessions::cache_hit_rate`].
    pub fn cache_hit_rate(&self) -> Option<f64> {
        cache_hit_rate(u64::from(self.cache_read), u64::from(self.total_input()))
    }

    pub fn format(&self, cost: Option<f64>) -> String {
        self.format_cost(cost, "")
    }

    /// Like [`format`](Self::format), but marks the cost as a running total.
    pub fn format_sum_cost(&self, cost: Option<f64>) -> String {
        self.format_cost(cost, "Σ")
    }

    fn format_cost(&self, cost: Option<f64>, prefix: &str) -> String {
        let tokens = format!(
            "{}↑ {}↓",
            format_tokens(self.total_input()),
            format_tokens(self.output)
        );
        match cost {
            Some(cost) => format!("{tokens} {prefix}${cost:.3}"),
            None => tokens,
        }
    }

    /// Crate-private on purpose: pricing outside [`Model`] skips the provider's
    /// schedule.
    pub(crate) fn cost(&self, pricing: &ModelPricing, fast: bool) -> f64 {
        let (input, output, cache_write, cache_read) = match &pricing.fast {
            // Fast mode quotes one flat premium, so it never reads tiers.
            Some(f) if fast => (f.input, f.output, f.cache_write, f.cache_read),
            // The tier boundary is on prompt size, which is everything the
            // model read: fresh input plus whatever came from cache.
            _ => pricing.rates_at(self.input + self.cache_read + self.cache_creation),
        };
        self.input as f64 * input / PER_MILLION
            + self.output as f64 * output / PER_MILLION
            + self.cache_creation as f64 * cache_write / PER_MILLION
            + self.cache_read as f64 * cache_read / PER_MILLION
    }
}

pub fn format_tokens(tokens: u32) -> String {
    format_tokens_wide(u64::from(tokens))
}

pub(crate) fn format_tokens_wide(tokens: u64) -> String {
    if tokens < TOKEN_THOUSAND {
        return tokens.to_string();
    }
    let thousand_tenths = rounded_tenths(tokens, TOKEN_THOUSAND);
    if tokens < TOKEN_MILLION && thousand_tenths < u128::from(TOKEN_THOUSAND) * TOKEN_TENTHS {
        return compact_tenths(thousand_tenths, "k");
    }
    compact_tenths(rounded_tenths(tokens, TOKEN_MILLION), "m")
}

fn rounded_tenths(value: u64, unit: u64) -> u128 {
    (u128::from(value) * TOKEN_TENTHS + u128::from(unit) / 2) / u128::from(unit)
}

fn compact_tenths(tenths: u128, suffix: &str) -> String {
    if tenths.is_multiple_of(TOKEN_TENTHS) {
        format!("{}{suffix}", tenths / TOKEN_TENTHS)
    } else {
        format!(
            "{}.{:01}{suffix}",
            tenths / TOKEN_TENTHS,
            tenths % TOKEN_TENTHS
        )
    }
}

impl AddAssign for TokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.input = self.input.saturating_add(rhs.input);
        self.output = self.output.saturating_add(rhs.output);
        self.cache_creation = self.cache_creation.saturating_add(rhs.cache_creation);
        self.cache_read = self.cache_read.saturating_add(rhs.cache_read);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_storage::StateDir;
    use test_case::test_case;

    fn policy(allowed: &[&str], excluded: &[&str]) -> ModelPolicy {
        ModelPolicy::new(
            &allowed
                .iter()
                .map(|pattern| (*pattern).into())
                .collect::<Vec<_>>(),
            &excluded
                .iter()
                .map(|pattern| (*pattern).into())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    const SLOTS: [ModelPurpose; 2] = ModelPurpose::CLASSES;
    /// These providers curate no deliberately small model.
    const NO_FAST_DEFAULTS: [&str; 2] = ["deepseek", "xai"];

    const AGGREGATOR: &str = "tensorx";
    const ALIASED_VENDOR_MODEL: &str = "z-ai/glm-4.5-air";
    const UNCLASSIFIABLE: &str = "ollama";
    const UNKNOWN_CATALOG_PROVIDER: &str = "caudra-cold-catalog-test-provider";
    const UNKNOWN_CATALOG_SPEC: &str = "caudra-cold-catalog-test-provider/model";

    const OPENAI_CHAT_SPEC: &str = "openai/gpt-5.6-sol";

    #[test_case("anthropic", "claude-haiku-4-5", Some(ModelPurpose::Fast) ; "curated_table")]
    #[test_case("anthropic", "claude-opus-4-6", Some(ModelPurpose::Best) ; "curated_best")]
    #[test_case("anthropic", "claude-sonnet-4-6", Some(ModelPurpose::Best) ; "known_non_small")]
    #[test_case("openai", "gpt-4.1-nano", Some(ModelPurpose::Fast) ; "longest_prefix_wins")]
    #[test_case(UNCLASSIFIABLE, "llama3", None ; "no_table_means_no_class")]
    fn class_of_reads_the_providers_own_answer(
        provider: &str,
        model_id: &str,
        expected: Option<ModelPurpose>,
    ) {
        assert_eq!(Model::class_of(provider, model_id), expected);
    }

    #[test_case("zai", "glm-4.5-air", Some(ModelMarker::Small) ; "small_non_default")]
    #[test_case("anthropic", "claude-haiku-4-5", Some(ModelMarker::Fast) ; "small_default")]
    #[test_case("anthropic", "claude-opus-5-5", Some(ModelMarker::Best) ; "non_small_default")]
    #[test_case("anthropic", "claude-sonnet-4-6", None ; "non_small_non_default")]
    #[test_case(UNCLASSIFIABLE, "llama3", None ; "unknown_facts")]
    fn marker_of_preserves_size_and_default_status(
        provider: &str,
        model_id: &str,
        expected: Option<ModelMarker>,
    ) {
        assert_eq!(Model::marker_of(provider, model_id), expected);
    }

    /// Aggregators carry no catalogue, and they spell some vendors differently
    /// than caudra does, so the class has to survive both hops.
    #[test]
    fn an_aggregator_borrows_the_facts_of_an_aliased_vendor() {
        assert_eq!(
            Model::facts_of(AGGREGATOR, ALIASED_VENDOR_MODEL),
            Some(ModelFacts {
                small: true,
                default: false,
            })
        );
        assert_eq!(
            Model::marker_of(AGGREGATOR, ALIASED_VENDOR_MODEL),
            Some(ModelMarker::Small)
        );
        assert_eq!(
            Model::class_of(AGGREGATOR, ALIASED_VENDOR_MODEL),
            Some(ModelPurpose::Fast)
        );
    }

    #[test_case("z-ai", "zai" ; "hyphenated_zai")]
    #[test_case("x-ai", "xai" ; "hyphenated_xai")]
    #[test_case("mistralai", "mistral" ; "run_together_mistral")]
    #[test_case("anthropic", "anthropic" ; "already_matching_slug_passes_through")]
    fn vendor_slugs_map_onto_builtins(vendor: &str, expected: &str) {
        assert_eq!(builtin_for_vendor(vendor), expected);
    }

    const EPSILON: f64 = 1e-10;
    /// The only builtin whose rates move with the wall clock.
    const SCHEDULED_PROVIDERS: [&str; 1] = ["deepseek"];
    const DEEPSEEK_SPEC: &str = "deepseek/deepseek-v4-pro";
    const UNPRICED_DEEPSEEK_SPEC: &str = "deepseek/my-custom-model";
    const MILLION: u32 = 1_000_000;
    const INPUT_ONLY: TokenUsage = TokenUsage {
        input: MILLION,
        output: 0,
        cache_creation: 0,
        cache_read: 0,
    };
    /// Four counters that cannot be confused with each other.
    const COUNTERS: TokenUsage = TokenUsage {
        input: 11,
        output: 22,
        cache_creation: 33,
        cache_read: 44,
    };
    const RECORDED_COST: f64 = 0.25;
    const FREE_MEANS_A_KNOWN_ZERO: &str = "only a price discovery reported as zero means free";
    const PAID_PRICING: ModelPricing = ModelPricing {
        input: 3.0,
        output: 15.0,
        cache_write: 0.0,
        cache_read: 0.0,
        fast: None,
        tiers: Vec::new(),
    };

    #[test_case(999, "999"         ; "under_thousand")]
    #[test_case(1_000, "1k"        ; "thousand")]
    #[test_case(1_049, "1k"        ; "rounds_down")]
    #[test_case(1_050, "1.1k"      ; "rounds_up")]
    #[test_case(12_300, "12.3k"    ; "keeps_a_useful_tenth")]
    #[test_case(372_000, "372k"    ; "drops_an_empty_tenth")]
    #[test_case(999_949, "999.9k"  ; "below_unit_promotion")]
    #[test_case(999_950, "1m"      ; "promotes_after_rounding")]
    #[test_case(999_999, "1m"      ; "just_under_million")]
    #[test_case(1_000_000, "1m"    ; "million")]
    #[test_case(1_050_000, "1.1m"  ; "million_keeps_a_tenth")]
    #[test_case(u32::MAX, "4295m"  ; "largest_session_count")]
    fn format_tokens_display(tokens: u32, expected: &str) {
        assert_eq!(format_tokens(tokens), expected);
    }

    #[test_case(TokenUsage { input: 12_000, output: 456, cache_creation: 200, cache_read: 100 }, None, "12.3k↑ 456↓" ; "without_cost")]
    #[test_case(TokenUsage { input: 1_000_000, output: 100_000, cache_creation: 200_000, cache_read: 500_000 }, Some(5.4), "1.7m↑ 100k↓ $5.400" ; "with_cost")]
    #[test_case(TokenUsage { input: u32::MAX, output: 1, cache_creation: 1, cache_read: 1 }, None, "4295m↑ 1↓" ; "input_saturates")]
    fn usage_formatting(usage: TokenUsage, cost: Option<f64>, expected: &str) {
        assert_eq!(usage.format(cost), expected);
    }

    #[test]
    fn sum_marker_applies_only_to_the_cost() {
        let usage = TokenUsage {
            input: 12_000,
            output: 456,
            cache_creation: 200,
            cache_read: 100,
        };
        assert_eq!(usage.format_sum_cost(Some(1.5)), "12.3k↑ 456↓ Σ$1.500");
        assert_eq!(usage.format_sum_cost(None), usage.format(None));
    }

    #[test_case("no-slash-here", ModelError::InvalidFormat ; "invalid_format")]
    #[test_case("foobar/gpt-4", ModelError::UnsupportedProvider("foobar".into()) ; "unsupported_provider")]
    fn from_spec_errors(spec: &str, expected: ModelError) {
        let err = Model::from_spec(spec).unwrap_err();
        assert_eq!(
            std::mem::discriminant(&err),
            std::mem::discriminant(&expected)
        );
    }

    /// Resolved the way production resolves them, not by assigning `family` by
    /// hand: the previous version of this test built a model no code path ever
    /// produces, which is why it passed while local models were being handed the
    /// wrong editor.
    #[test_case("openai/gpt-5.6-sol", true ; "gpt 5 on openai")]
    #[test_case("copilot/gpt-5.6-terra", true ; "gpt 5 under a generic family")]
    #[test_case("openai/o3-pro", true ; "reasoning ids never spell gpt")]
    #[test_case("openai/codex-mini", true ; "codex ids never spell gpt")]
    #[test_case("openai/gpt-4o", false ; "gpt 4 predates the format")]
    #[test_case("openrouter/gpt-oss-120b", false ; "open weights were not trained on it")]
    #[test_case("anthropic/claude-opus-4-8", false ; "claude uses string replacement")]
    #[test_case("zai/glm-4.6", false ; "glm uses string replacement")]
    #[test_case("llama-cpp/qwen3.8-27b-cyberstrike", false ; "an openai shaped local server is not gpt")]
    fn prefers_apply_patch_follows_the_model_id(spec: &str, expected: bool) {
        let model = Model::from_spec(spec).unwrap();

        assert_eq!(model.prefers_apply_patch(), expected, "{spec}");
    }

    /// A borrowed manifest lends windows and thinking defaults, never lineage.
    #[test]
    fn a_custom_slug_never_inherits_its_base_family() {
        let openai = ManifestRegistry::get("openai").unwrap();

        assert_eq!(
            inherited_lineage(openai, "ninfer-4090", None),
            ModelFamily::Generic,
            "an openai-protocol custom provider claimed GPT lineage"
        );
        assert_eq!(
            inherited_lineage(openai, "openai", None),
            ModelFamily::Gpt,
            "the provider that owns the manifest lost its own family"
        );
    }

    #[test]
    fn from_spec_with_policy_rejects_disallowed_exact_spec() {
        let policy = policy(&["anthropic/*"], &[]);
        let spec = "openai/gpt-5.6-sol";

        let error = Model::from_spec_with_policy(spec, &policy).unwrap_err();

        assert!(matches!(error, ModelError::NotAllowed(disallowed) if disallowed == spec));
    }

    #[test]
    fn from_spec_with_policy_resolves_allowed_exact_spec() {
        let policy = policy(&["openai/gpt-5.6-sol"], &[]);

        let model = Model::from_spec_with_policy("openai/gpt-5.6-sol", &policy).unwrap();

        assert_eq!(model.spec(), "openai/gpt-5.6-sol");
    }

    #[test]
    fn resolve_takes_an_allowed_curated_alternative() {
        let policy = policy(&["openai/gpt-5.4-nano"], &[]);
        let chat = Model::from_spec(OPENAI_CHAT_SPEC).unwrap();

        let model = Model::resolve(ModelPurpose::Fast, &chat, &policy).unwrap();

        assert_eq!(model.spec(), "openai/gpt-5.4-nano");
    }

    /// Every rule ends at the conversation model, which the user already chose,
    /// so a slot the policy empties can never resolve to nothing.
    #[test]
    fn resolve_falls_back_to_chat_when_the_policy_allows_no_candidate() {
        let policy = policy(&["anthropic/*"], &[]);
        let chat = Model::from_spec(OPENAI_CHAT_SPEC).unwrap();

        let model = Model::resolve(ModelPurpose::Fast, &chat, &policy).unwrap();

        assert_eq!(model.spec(), chat.spec());
    }

    #[test_case(ModelPurpose::Chat, OPENAI_CHAT_SPEC ; "chat_keeps_anchor")]
    #[test_case(ModelPurpose::Plan, OPENAI_CHAT_SPEC ; "plan_keeps_anchor")]
    #[test_case(ModelPurpose::Subagent, OPENAI_CHAT_SPEC ; "subagent_keeps_anchor")]
    #[test_case(ModelPurpose::Compact, OPENAI_CHAT_SPEC ; "compact_keeps_anchor")]
    #[test_case(ModelPurpose::Title, "openai/gpt-5.6-luna" ; "title_uses_fast")]
    #[test_case(ModelPurpose::Goal, "openai/gpt-5.6-luna" ; "goal_uses_fast")]
    #[test_case(ModelPurpose::Extract, "openai/gpt-5.6-luna" ; "extract_uses_fast")]
    #[test_case(ModelPurpose::Fast, "openai/gpt-5.6-luna" ; "fast_uses_small_default")]
    #[test_case(ModelPurpose::Best, "openai/gpt-5.6-sol" ; "best_uses_non_small_default")]
    fn automatic_purpose_resolution_uses_the_expected_lane(purpose: ModelPurpose, expected: &str) {
        let anchor = Model::from_spec(OPENAI_CHAT_SPEC).unwrap();

        let model = Model::resolve(purpose, &anchor, &policy(&[], &[])).unwrap();
        let non_warming = Model::resolve_if_available(purpose, &anchor, &policy(&[], &[])).unwrap();

        assert_eq!(model.spec(), expected);
        assert_eq!(non_warming.spec(), model.spec());
    }

    /// A lane answers inside the anchor's own release line. Anthropic declares
    /// none, so it keeps answering provider-wide, which is what makes Haiku the
    /// Fast model for a Claude 5 anchor that has no small sibling.
    #[test_case("openai/gpt-6-sol", ModelPurpose::Fast, "openai/gpt-6-luna" ; "gpt_6_keeps_fast_in_line")]
    #[test_case("openai/gpt-6-sol", ModelPurpose::Best, "openai/gpt-6-astra" ; "gpt_6_keeps_best_in_line")]
    #[test_case("openai/gpt-5.6-terra", ModelPurpose::Fast, "openai/gpt-5.6-luna" ; "gpt_5_6_keeps_fast_in_line")]
    #[test_case("openai/gpt-5.6-terra", ModelPurpose::Best, "openai/gpt-5.6-sol" ; "gpt_5_6_keeps_best_in_line")]
    #[test_case("openai/gpt-4.1", ModelPurpose::Best, "openai/gpt-6-astra" ; "an_unlined_anchor_takes_the_provider_default")]
    #[test_case("anthropic/claude-opus-5-5", ModelPurpose::Fast, "anthropic/claude-haiku-4-5" ; "an_unlined_provider_answers_across_lines")]
    fn a_lane_answers_inside_the_anchors_line(
        anchor_spec: &str,
        purpose: ModelPurpose,
        expected: &str,
    ) {
        let anchor = Model::from_spec(anchor_spec).unwrap();
        let resolved = Model::resolve(purpose, &anchor, &policy(&[], &[])).unwrap();
        assert_eq!(resolved.spec(), expected);
    }

    #[test]
    fn non_warming_resolution_leaves_an_unknown_exact_provider_cold() {
        assert!(crate::catalog_providers_if_available().is_none());
        let anchor = Model::from_spec(OPENAI_CHAT_SPEC).unwrap();
        let binding = Binding::Exact(UNKNOWN_CATALOG_SPEC.to_string());
        let model_policy = policy(&[], &[]);

        let captured_error = Model::resolve_binding_if_available(
            ModelPurpose::Plan,
            Some(&binding),
            &anchor,
            &model_policy,
        )
        .unwrap_err();
        assert!(matches!(
            captured_error,
            ModelError::UnsupportedProvider(provider)
                if provider == UNKNOWN_CATALOG_PROVIDER
        ));

        let temp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        model_registry::set_binding_and_persist(ModelPurpose::Plan, binding, &state_dir).unwrap();
        let purpose_error =
            Model::resolve_if_available(ModelPurpose::Plan, &anchor, &model_policy).unwrap_err();
        assert!(matches!(
            purpose_error,
            ModelError::UnsupportedProvider(provider)
                if provider == UNKNOWN_CATALOG_PROVIDER
        ));
        assert!(crate::catalog_providers_if_available().is_none());
        model_registry::clear_binding_and_persist(ModelPurpose::Plan, &state_dir).unwrap();
    }

    #[test]
    fn captured_non_warming_resolution_matches_resolution_for_an_available_model() {
        let anchor = Model::from_spec(OPENAI_CHAT_SPEC).unwrap();
        let binding = Binding::Exact("anthropic/claude-haiku-4-5".to_string());
        let model_policy = policy(&[], &[]);

        let resolved =
            Model::resolve_binding(ModelPurpose::Fast, Some(&binding), &anchor, &model_policy)
                .unwrap();
        let non_warming = Model::resolve_binding_if_available(
            ModelPurpose::Fast,
            Some(&binding),
            &anchor,
            &model_policy,
        )
        .unwrap();

        assert_eq!(non_warming.spec(), resolved.spec());
    }

    /// No anchor to read a line from, so these are the provider-wide answers:
    /// the first default in table order, which is why the tables are ordered
    /// newest line first.
    #[test_case("anthropic", ModelPurpose::Fast, "claude-haiku-4-5" ; "anthropic_small")]
    #[test_case("anthropic", ModelPurpose::Best, "claude-opus-5-5" ; "anthropic_flagship")]
    #[test_case("openai", ModelPurpose::Fast, "gpt-6-luna" ; "openai_small")]
    #[test_case("openai", ModelPurpose::Best, "gpt-6-astra" ; "openai_flagship")]
    fn curated_defaults_select_by_size_lane(provider: &str, purpose: ModelPurpose, expected: &str) {
        assert_eq!(
            Model::curated_default(provider, purpose).unwrap().id,
            expected
        );
    }

    #[test]
    fn from_spec_unknown_catalogue_subprovider_is_unsupported() {
        // The on-disk models.dev cache may populate the catalog in a
        // developer's environment, so pick a slug that is likely not in any
        // catalog and confirm it falls through to the generic
        // unsupported-provider branch.
        let err = Model::from_spec("definitely-not-a-catalog-slug/any-model").unwrap_err();
        assert!(matches!(err, ModelError::UnsupportedProvider(_)));
    }

    #[test]
    fn total_input_includes_cached_tokens() {
        let usage = TokenUsage {
            input: 5_000,
            output: 1_000,
            cache_creation: 10_000,
            cache_read: 150_000,
        };
        assert_eq!(usage.total_input(), 165_000);
    }

    #[test]
    fn cost_computes_all_token_types() {
        let pricing = ModelPricing {
            input: 3.00,
            output: 15.00,
            cache_write: 3.75,
            cache_read: 0.30,
            fast: None,
            tiers: Vec::new(),
        };
        let usage = TokenUsage {
            input: 1_000_000,
            output: 100_000,
            cache_creation: 200_000,
            cache_read: 500_000,
        };
        let cost = usage.cost(&pricing, false);
        let expected = 3.0 + 1.5 + 0.75 + 0.15;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn fast_mode_applies_premium_rates() {
        let pricing = ModelPricing {
            input: 5.00,
            output: 25.00,
            cache_write: 6.25,
            cache_read: 0.50,
            fast: Some(FastPricing {
                input: 30.00,
                output: 150.00,
                cache_write: 37.50,
                cache_read: 3.00,
            }),
            tiers: Vec::new(),
        };
        let usage = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cache_creation: 1_000_000,
            cache_read: 1_000_000,
        };
        let fast = usage.cost(&pricing, true);
        let expected = 30.0 + 150.0 + 37.5 + 3.0;
        assert!((fast - expected).abs() < 1e-10);
        assert!(fast > usage.cost(&pricing, false));
    }

    #[test]
    fn fast_flag_ignored_without_fast_tier() {
        let pricing = ModelPricing {
            input: 3.00,
            output: 15.00,
            cache_write: 3.75,
            cache_read: 0.30,
            fast: None,
            tiers: Vec::new(),
        };
        let usage = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cache_creation: 0,
            cache_read: 0,
        };
        assert_eq!(usage.cost(&pricing, true), usage.cost(&pricing, false));
    }

    #[test]
    fn fast_pricing_is_always_a_premium() {
        for manifest in ManifestRegistry::builtins() {
            for entry in manifest.models {
                let Some(fast) = &entry.pricing.fast else {
                    continue;
                };
                assert!(
                    fast.input >= entry.pricing.input
                        && fast.output >= entry.pricing.output
                        && fast.cache_write >= entry.pricing.cache_write
                        && fast.cache_read >= entry.pricing.cache_read,
                    "{}/{}: fast pricing must not be cheaper than standard",
                    manifest.slug,
                    entry.prefixes[0],
                );
            }
        }
    }

    #[test]
    fn spec_roundtrip() {
        for manifest in ManifestRegistry::builtins() {
            if manifest.accepts_arbitrary_models {
                continue;
            }
            let model = Model::curated_default(manifest.slug, ModelPurpose::Best).unwrap();
            let round = Model::from_spec(&model.spec()).unwrap();
            assert_eq!(round.id, model.id);
            assert_eq!(round.provider, model.provider);
        }
    }

    #[test]
    fn opencode_from_spec_parses_four_levels() {
        let spec = "opencode/nvidia/openai/gpt-oss-120b";
        let model = Model::from_spec(spec).unwrap();
        assert_eq!(model.provider, Arc::<str>::from("opencode"));
        assert_eq!(model.id, "nvidia/openai/gpt-oss-120b");
        assert_eq!(model.spec(), spec);
    }

    #[test]
    fn opencode_from_spec_parses_three_levels() {
        let spec = "opencode/opencode/big-pickle";
        let model = Model::from_spec(spec).unwrap();
        assert_eq!(model.provider, Arc::<str>::from("opencode"));
        assert_eq!(model.id, "opencode/big-pickle");
        assert_eq!(model.spec(), spec);
    }

    #[test]
    fn every_curated_slot_resolves_to_a_usable_model() {
        for manifest in ManifestRegistry::builtins() {
            if manifest.models.is_empty() {
                continue;
            }
            let slug: Arc<str> = Arc::from(manifest.slug);
            for &purpose in &SLOTS {
                if NO_FAST_DEFAULTS.contains(&manifest.slug) && purpose == ModelPurpose::Fast {
                    continue;
                }
                let model = Model::curated_default(manifest.slug, purpose).unwrap();
                assert_eq!(model.provider, slug);
                if let Some(max_output) = model.max_output_tokens {
                    assert!(max_output > 0);
                    assert!(model.context_window >= max_output);
                }
            }
        }
    }

    /// Provider-wide, a lane still needs exactly one answer: the first default
    /// in table order, which is what `curated_default` takes on a cold start.
    #[test]
    fn exactly_one_default_per_provider_slot() {
        for manifest in ManifestRegistry::builtins() {
            if manifest.models.is_empty() {
                continue;
            }
            for &purpose in &SLOTS {
                if NO_FAST_DEFAULTS.contains(&manifest.slug) && purpose == ModelPurpose::Fast {
                    continue;
                }
                let count = manifest
                    .models
                    .iter()
                    .filter(|entry| entry.class() == purpose && entry.default)
                    .count();
                let lines = manifest.generations.len().max(1);
                assert_eq!(
                    count, lines,
                    "{}/{purpose}: expected 1 default per line ({lines}), found {count}",
                    manifest.slug
                );
            }
        }
    }

    /// A lane is answered inside a line, so a line that sells a model for that
    /// lane must name exactly one of them as its default. A line with no model
    /// in the lane names none and falls through to the provider's own answer.
    #[test]
    fn exactly_one_default_per_line_and_slot() {
        for manifest in ManifestRegistry::builtins() {
            for line in manifest.generations {
                for &purpose in &SLOTS {
                    let members: Vec<_> = manifest
                        .models
                        .iter()
                        .filter(|entry| line.contains(entry) && entry.class() == purpose)
                        .collect();
                    if members.is_empty() {
                        continue;
                    }
                    let count = members.iter().filter(|entry| entry.default).count();
                    assert_eq!(
                        count, 1,
                        "{}/{}/{purpose}: expected exactly 1 default, found {count}",
                        manifest.slug, line.label
                    );
                }
            }
        }
    }

    /// A model may sit in at most one line, or `generation_of`'s longest-member
    /// tie-break would be deciding something the tables meant to state.
    #[test]
    fn declared_lines_never_overlap() {
        for manifest in ManifestRegistry::builtins() {
            for entry in manifest.models {
                let lines: Vec<_> = manifest
                    .generations
                    .iter()
                    .filter(|line| line.contains(entry))
                    .map(|line| line.label)
                    .collect();
                assert!(
                    lines.len() <= 1,
                    "{}/{}: claimed by {lines:?}",
                    manifest.slug,
                    entry.prefixes[0]
                );
            }
        }
    }

    #[test_case("anthropic/claude-99-turbo", "anthropic", "claude-99-turbo" ; "unknown_anthropic_model_accepted")]
    #[test_case("zai/glm-99", "zai", "glm-99" ; "unknown_zai_model_accepted")]
    #[test_case("openai/gpt-99", "openai", "gpt-99" ; "unknown_openai_model_accepted")]
    #[test_case("xai/grok-99", "xai", "grok-99" ; "unknown_xai_model_accepted")]
    #[test_case("synthetic/hf:nonexistent", "synthetic", "hf:nonexistent" ; "unknown_synthetic_model_accepted")]
    #[test_case("ollama/my-custom-model", "ollama", "my-custom-model" ; "unknown_ollama_model_accepted")]
    #[test_case("deepseek/my-custom-model", "deepseek", "my-custom-model" ; "unknown_deepseek_model_accepted")]
    fn unknown_model_accepted(spec: &str, expected_slug: &str, expected_id: &str) {
        let model = Model::from_spec(spec).unwrap();
        assert_eq!(model.provider, Arc::<str>::from(expected_slug));
        assert_eq!(model.id, expected_id);
        let manifest = ManifestRegistry::get(expected_slug).unwrap();
        assert_eq!(model.family, manifest.family);
    }

    #[test]
    fn from_base_unknown_model_uses_provider_fallbacks() {
        // Deliberately fake id so this stays valid when the model table changes.
        let model = Model::from_base(
            ManifestRegistry::get("anthropic").unwrap(),
            "anthropic",
            "claude-nonexistent-99",
        );
        assert_eq!(model.provider, Arc::<str>::from("anthropic"));
        assert_eq!(model.id, "claude-nonexistent-99");
        assert_eq!(model.spec(), "anthropic/claude-nonexistent-99");
        assert_eq!(model.family, ModelFamily::Claude);
        assert_eq!(model.max_output_tokens, Some(128_000));
        assert_eq!(model.context_window, 200_000);
        let p = &model.pricing;
        assert_eq!(
            (p.input, p.output, p.cache_write, p.cache_read),
            (0.0, 0.0, 0.0, 0.0)
        );
    }

    #[test_case("anthropic/claude-opus-4-8",       true  ; "claude")]
    #[test_case("openai/gpt-5.4",                   true  ; "gpt")]
    #[test_case("xai/grok-4.6",                     true  ; "grok")]
    #[test_case("google/gemini-2.5-pro",            true  ; "gemini")]
    #[test_case("copilot/claude-opus-4.7",          true  ; "copilot_entry_beats_generic_family")]
    #[test_case("zai/glm-5-code",                   false ; "glm_code_text_only")]
    #[test_case("deepseek/deepseek-v4-pro",         false ; "deepseek_text_only")]
    #[test_case("mistral/mistral-medium-latest",    true  ; "mistral_medium")]
    #[test_case("mistral/ministral-14b-latest",     false ; "ministral_text_only")]
    #[test_case("anthropic/claude-nonexistent-99",  true  ; "unknown_model_uses_family_fallback")]
    #[test_case("deepseek/my-custom-model",         false ; "unknown_generic_defaults_off")]
    fn vision_resolved_from_entry_or_family(spec: &str, expected: bool) {
        assert_eq!(Model::from_spec(spec).unwrap().supports_vision(), expected);
    }

    #[test_case("claude-opus-5",    true  ; "entry_with_fast_pricing")]
    #[test_case("claude-opus-5-5",  true  ; "opus_5_5_is_fast_capable")]
    #[test_case("claude-opus-5-1m", true  ; "long_context_suffix_still_matches_prefix")]
    #[test_case("claude-opus-4-7",  false ; "fast_withdrawn_from_the_table")]
    #[test_case("claude-sonnet-5",  false ; "entry_without_fast_pricing")]
    #[test_case("claude-opus-99",   false ; "no_entry_at_all")]
    fn supports_fast_follows_anthropic_table(model_id: &str, expected: bool) {
        let model = Model::from_base(
            ManifestRegistry::get("anthropic").unwrap(),
            "anthropic",
            model_id,
        );
        assert_eq!(model.supports_fast(), expected);
    }

    /// A longer id must not inherit the shorter prefix's rates. Both of these
    /// are cheaper than the entry that would otherwise swallow them, so the
    /// failure is silent overbilling rather than an error.
    #[test_case("claude-opus-5-5",  (4.00, 20.00, 5.00, 0.20)   ; "opus_5_5_does_not_inherit_opus_5")]
    #[test_case("claude-opus-5",    (5.00, 25.00, 6.25, 0.50)   ; "opus_5_keeps_its_own")]
    #[test_case("claude-fable-5-1", (10.00, 50.00, 12.50, 0.25) ; "fable_5_1_does_not_inherit_fable_5")]
    #[test_case("claude-fable-5",   (10.00, 50.00, 12.50, 1.00) ; "fable_5_keeps_its_own")]
    fn anthropic_rates_come_from_the_most_specific_prefix(
        model_id: &str,
        expected: (f64, f64, f64, f64),
    ) {
        let pricing = Model::from_base(
            ManifestRegistry::get("anthropic").unwrap(),
            "anthropic",
            model_id,
        )
        .pricing;
        assert_eq!(
            (
                pricing.input,
                pricing.output,
                pricing.cache_write,
                pricing.cache_read
            ),
            expected
        );
    }

    /// Opus 5.5 reads cache at a twentieth of input, so deriving the fast rate
    /// from the 0.10 multiplier every earlier model used would double the bill
    /// on the tokens agentic work is mostly made of.
    #[test]
    fn opus_5_5_fast_cache_reads_are_not_derived_from_input() {
        let model = Model::from_base(
            ManifestRegistry::get("anthropic").unwrap(),
            "anthropic",
            "claude-opus-5-5",
        );
        let usage = TokenUsage {
            cache_read: 1_000_000,
            ..Default::default()
        };
        assert_eq!(model.list_cost(&usage, true), Some(0.40));
    }

    #[test]
    fn supports_fast_false_for_non_anthropic_even_with_fast_pricing() {
        let mut model = Model::from_base(
            ManifestRegistry::get("google").unwrap(),
            "google",
            "gemini-2.5-pro",
        );
        model.pricing.fast = Some(FastPricing::derived(30.0, 150.0));
        assert!(!model.supports_fast());
    }

    #[test]
    fn discovered_vision_flows_into_curated_provider_model() {
        use crate::model::ModelInfo;

        model_registry::set_known_models(
            "synthetic",
            vec![
                ModelInfo {
                    supports_vision: Some(true),
                    ..ModelInfo::id_only("syn:test-vision".into())
                },
                ModelInfo::id_only("syn:test-blind".into()),
            ],
        );

        let vision = |id| Model::from_spec(id).unwrap().supports_vision();
        assert!(vision("synthetic/syn:test-vision"));
        assert!(!vision("synthetic/syn:test-blind"));
    }

    #[test]
    fn discovered_context_window_flows_into_from_base_for_unknown_model() {
        use crate::model::ModelInfo;

        let model_id = "test-discovered-context-window-model";
        let expected_window: u32 = 131_072;

        model_registry::set_known_models(
            "ollama",
            vec![ModelInfo {
                context_window: Some(expected_window),
                ..ModelInfo::id_only(model_id.to_string())
            }],
        );

        let model = Model::from_base(ManifestRegistry::get("ollama").unwrap(), "ollama", model_id);
        assert_eq!(model.context_window, expected_window);

        // A dynamic/custom slug shares its base provider's discovery.
        let wrapped = Model::from_base(
            ManifestRegistry::get("ollama").unwrap(),
            "my-ollama-wrap",
            model_id,
        );
        assert_eq!(wrapped.spec(), format!("my-ollama-wrap/{model_id}"));
        assert_eq!(wrapped.context_window, expected_window);
    }

    #[test]
    fn wrapper_discovery_wins_over_metadata_from_its_base_provider() {
        let model_id = "test-wrapper-specific-discovery";
        let wrapper_slug = "test-openai-wrapper";
        let base_window = 64_000;
        let wrapper_window = 192_000;
        model_registry::set_known_models(
            "openai",
            vec![ModelInfo {
                context_window: Some(base_window),
                ..ModelInfo::id_only(model_id.into())
            }],
        );
        model_registry::set_known_models(
            wrapper_slug,
            vec![ModelInfo {
                context_window: Some(wrapper_window),
                ..ModelInfo::id_only(model_id.into())
            }],
        );

        let model = Model::from_base(
            ManifestRegistry::get("openai").unwrap(),
            wrapper_slug,
            model_id,
        );

        assert_eq!(model.spec(), format!("{wrapper_slug}/{model_id}"));
        assert_eq!(model.context_window, wrapper_window);
    }

    /// "We could not read a price" must never reach the picker as "free", so
    /// only an explicit zero from discovery sets the flag.
    #[test_case(Some(ModelPricing::ZERO), true  ; "explicit_zero_is_free")]
    #[test_case(Some(PAID_PRICING),       false ; "priced_is_not_free")]
    #[test_case(None,                     false ; "unknown_price_is_not_free")]
    fn discovered_pricing_decides_free(pricing: Option<ModelPricing>, expected: bool) {
        let model_id = "test-discovered-free-model";
        model_registry::set_known_models(
            "ollama",
            vec![ModelInfo {
                pricing,
                ..ModelInfo::id_only(model_id.to_string())
            }],
        );

        let model = Model::from_base(ManifestRegistry::get("ollama").unwrap(), "ollama", model_id);
        assert_eq!(model.is_free(), expected, "{FREE_MEANS_A_KNOWN_ZERO}");
    }

    /// A schedule hung on the wrong manifest silently doubles every turn of a
    /// provider that bills flat.
    #[test]
    fn only_deepseek_bills_by_the_clock() {
        let scheduled: Vec<&str> = ManifestRegistry::builtins()
            .iter()
            .filter(|m| m.pricing_schedule.is_some())
            .map(|m| m.slug)
            .collect();
        assert_eq!(scheduled, SCHEDULED_PROVIDERS);
    }

    /// Nothing else pins the wiring: a real DeepSeek model has to pick the
    /// schedule up out of its manifest, and `list_cost` has to stay out of it.
    /// `billed_cost` reads the real clock, so the expectation is sampled either
    /// side of the call in case the hour ticks over mid-test.
    #[test]
    fn deepseek_bills_its_peak_surcharge_on_top_of_the_table() {
        let model = Model::from_spec(DEEPSEEK_SPEC).unwrap();
        let schedule = ManifestRegistry::for_slug(&model.provider)
            .and_then(|m| m.pricing_schedule)
            .expect("deepseek bills by the clock");

        let list = model.list_cost(&INPUT_ONLY, false).unwrap();
        let table_price = f64::from(INPUT_ONLY.input) * model.pricing.input / PER_MILLION;
        assert!(
            (list - table_price).abs() < EPSILON,
            "list_cost {list} must be the table price {table_price}, surcharge free"
        );

        let before = schedule.multiplier_at(Timestamp::now());
        let billed = model.billed_cost(&INPUT_ONLY, false).unwrap();
        let after = schedule.multiplier_at(Timestamp::now());
        assert!(
            [before, after]
                .iter()
                .any(|multiplier| (billed - list * multiplier).abs() < EPSILON),
            "billed {billed} is not {list} scaled by the schedule ({before} or {after})"
        );
    }

    /// A schedule must not turn "no price" into "$0.000". Callers hide `None`,
    /// and any multiple of nothing is still nothing.
    #[test]
    fn unpriced_models_stay_unpriced_under_a_schedule() {
        let model = Model::from_spec(UNPRICED_DEEPSEEK_SPEC).unwrap();
        assert!(model.pricing.is_zero());
        assert_eq!(model.list_cost(&INPUT_ONLY, false), None);
        assert_eq!(model.billed_cost(&INPUT_ONLY, false), None);
    }

    /// Every later total is rebuilt from what was stored, so storing a turn must
    /// not shuffle the counters, invent one, or drop the cost.
    #[test]
    fn billed_stores_every_counter_and_the_cost() {
        assert_eq!(
            COUNTERS.billed(Some(RECORDED_COST), Billing::Api),
            StoredTokenUsage {
                input: COUNTERS.input,
                output: COUNTERS.output,
                cache_creation: COUNTERS.cache_creation,
                cache_read: COUNTERS.cache_read,
                cost: Some(RECORDED_COST),
                subscription_cost: None,
            }
        );
        assert_eq!(COUNTERS.billed(None, Billing::Api).cost, None);
    }

    const WRONG_COLUMN: &str = "a price must land under the payer that owes it";

    #[test_case(Billing::Api,          Some(RECORDED_COST), None ; "an_api_key_is_invoiced")]
    #[test_case(Billing::Subscription, None, Some(RECORDED_COST) ; "a_plan_owes_nothing")]
    fn billed_files_the_price_under_its_payer(
        billing: Billing,
        cost: Option<f64>,
        subscription_cost: Option<f64>,
    ) {
        let stored = COUNTERS.billed(Some(RECORDED_COST), billing);
        assert_eq!(stored.cost, cost, "{WRONG_COLUMN}");
        assert_eq!(
            stored.subscription_cost, subscription_cost,
            "{WRONG_COLUMN}"
        );
    }
}
