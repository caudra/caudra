use std::borrow::Cow;

use caudra_storage::thinking::ReasoningOptions;

use crate::model::{
    ModelEntry, ModelFacts, ModelFamily, ModelGeneration, ModelPricing, ModelPurpose,
};
use crate::pricing::PricingSchedule;
use crate::providers::catalog::CatalogMetaView;
use crate::providers::{
    anthropic, aperture, copilot, custom, deepseek, dynamic, google, llama_cpp, mistral, ollama,
    openai, openrouter, synthetic, tensorx, xai, zai,
};

#[derive(Debug, Clone, Copy)]
pub struct ProviderManifest {
    pub slug: &'static str,
    pub display_name: &'static str,
    pub family: ModelFamily,
    pub supports_thinking: bool,
    pub accepts_arbitrary_models: bool,
    pub fallback_max_output: Option<u32>,
    pub fallback_context_window: u32,
    pub models: &'static [ModelEntry],
    /// Release lines, newest first. A routing lane answers inside the anchor's
    /// line before falling back to the provider-wide default, so moving to
    /// `gpt-6-sol` moves Fast to `gpt-6-luna`. Empty for every provider that
    /// has never shipped two lines worth telling apart.
    pub generations: &'static [ModelGeneration],
    /// Whether this provider knows how to ask for fast mode. A fast rate on its
    /// own is not enough: a premium nobody can request would bill a standard
    /// turn at the premium price.
    pub serves_fast_mode: bool,
    /// Set by the providers whose rates move with the wall clock, so the hours
    /// sit next to the prices they scale. Everyone else bills flat.
    pub pricing_schedule: Option<&'static PricingSchedule>,
    /// This provider's id in the models.dev catalog, where it differs from the
    /// caudra slug. `None` for providers the catalog has no entry for: local
    /// runtimes and gateways that route somewhere else.
    pub catalog_slug: Option<&'static str>,
}

impl ProviderManifest {
    /// What models.dev publishes about this model. Never triggers a fetch, so a
    /// cold catalog simply leaves every decision to the static table and
    /// discovery. Returned whole rather than field by field because each lookup
    /// takes the catalog guard and clones, and `Model::from_base` needs four of
    /// them for the same model.
    pub fn catalog_meta(&self, model_id: &str) -> Option<CatalogMetaView> {
        crate::providers::catalog::model_meta_if_available(self.catalog_slug?, model_id)
    }
}

/// An entry the catalog has not classified carries an empty ladder, which must
/// not outrank the static table's.
pub(crate) fn catalog_reasoning_options(
    meta: Option<&CatalogMetaView>,
) -> Option<ReasoningOptions> {
    meta.map(|meta| meta.reasoning_options.clone())
        .filter(|options| !options.is_empty())
}

/// Rates models.dev publishes, tiers included.
pub(crate) fn catalog_pricing(meta: Option<&CatalogMetaView>) -> Option<ModelPricing> {
    meta.map(|meta| ModelPricing {
        input: meta.input_price,
        output: meta.output_price,
        cache_write: meta.cache_write,
        cache_read: meta.cache_read,
        fast: None,
        tiers: Cow::Owned(meta.pricing_tiers.clone()),
    })
}

const ANTHROPIC: ProviderManifest = ProviderManifest {
    slug: "anthropic",
    display_name: "Anthropic",
    family: ModelFamily::Claude,
    supports_thinking: true,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(128_000),
    fallback_context_window: 200_000,
    models: anthropic::models(),
    generations: &[],
    serves_fast_mode: true,
    pricing_schedule: None,
    catalog_slug: Some("anthropic"),
};

const OPENAI: ProviderManifest = ProviderManifest {
    slug: "openai",
    display_name: "OpenAI",
    family: ModelFamily::Gpt,
    supports_thinking: true,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(100_000),
    fallback_context_window: 200_000,
    models: openai::models(),
    generations: openai::generations(),
    serves_fast_mode: true,
    pricing_schedule: None,
    catalog_slug: Some("openai"),
};

const GOOGLE: ProviderManifest = ProviderManifest {
    slug: "google",
    display_name: "Google",
    family: ModelFamily::Gemini,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(65_536),
    fallback_context_window: 1_000_000,
    models: google::models(),
    generations: &[],
    serves_fast_mode: false,
    pricing_schedule: None,
    catalog_slug: Some("google"),
};

const COPILOT: ProviderManifest = ProviderManifest {
    slug: copilot::PUBLIC_SLUG,
    display_name: copilot::PUBLIC_DISPLAY_NAME,
    family: ModelFamily::Generic,
    supports_thinking: false,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(100_000),
    fallback_context_window: 200_000,
    models: copilot::models(),
    generations: &[],
    serves_fast_mode: false,
    pricing_schedule: None,
    catalog_slug: Some("github-copilot"),
};

/// Shares Copilot's model table under its own slug, so discovered metadata
/// for one account never lands on the other.
const COPILOT_ENTERPRISE: ProviderManifest = ProviderManifest {
    slug: copilot::ENTERPRISE_SLUG,
    display_name: copilot::ENTERPRISE_DISPLAY_NAME,
    ..COPILOT
};

const OLLAMA: ProviderManifest = ProviderManifest {
    slug: "ollama",
    display_name: "Ollama",
    family: ModelFamily::Generic,
    supports_thinking: false,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(16_384),
    fallback_context_window: 128_000,
    models: ollama::models(),
    generations: &[],
    serves_fast_mode: false,
    pricing_schedule: None,
    catalog_slug: None,
};

const LLAMA_CPP: ProviderManifest = ProviderManifest {
    slug: "llama-cpp",
    display_name: "LlamaCpp",
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: None,
    fallback_context_window: 128_000,
    models: llama_cpp::models(),
    generations: &[],
    serves_fast_mode: false,
    pricing_schedule: None,
    catalog_slug: None,
};

const MISTRAL: ProviderManifest = ProviderManifest {
    slug: "mistral",
    display_name: "Mistral",
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: None,
    fallback_context_window: 128_000,
    models: mistral::models(),
    generations: &[],
    serves_fast_mode: false,
    pricing_schedule: None,
    catalog_slug: Some("mistral"),
};

const ZAI: ProviderManifest = ProviderManifest {
    slug: "zai",
    display_name: "Z.AI",
    family: ModelFamily::Glm,
    supports_thinking: false,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(16_000),
    fallback_context_window: 128_000,
    models: zai::models(),
    generations: &[],
    serves_fast_mode: false,
    pricing_schedule: None,
    catalog_slug: Some("zai"),
};

const DEEPSEEK: ProviderManifest = ProviderManifest {
    slug: "deepseek",
    display_name: "DeepSeek",
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(384_000),
    fallback_context_window: 1_000_000,
    models: deepseek::models(),
    generations: &[],
    serves_fast_mode: false,
    pricing_schedule: Some(&deepseek::PEAK_HOURS),
    catalog_slug: Some("deepseek"),
};

const OPENROUTER: ProviderManifest = ProviderManifest {
    slug: "openrouter",
    display_name: "OpenRouter",
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(128_000),
    fallback_context_window: 200_000,
    models: openrouter::models(),
    generations: &[],
    serves_fast_mode: false,
    pricing_schedule: None,
    catalog_slug: Some("openrouter"),
};

const SYNTHETIC: ProviderManifest = ProviderManifest {
    slug: "synthetic",
    display_name: "Synthetic",
    family: ModelFamily::Synthetic,
    supports_thinking: true,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(32_000),
    fallback_context_window: 128_000,
    models: synthetic::models(),
    generations: &[],
    serves_fast_mode: false,
    pricing_schedule: None,
    catalog_slug: Some("synthetic"),
};

const TENSORX: ProviderManifest = ProviderManifest {
    slug: "tensorx",
    display_name: "TensorX",
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: None,
    fallback_context_window: 200_000,
    models: tensorx::models(),
    generations: &[],
    serves_fast_mode: false,
    pricing_schedule: None,
    catalog_slug: Some("tensorx"),
};

const OPENCODE: ProviderManifest = ProviderManifest {
    slug: "opencode",
    display_name: "Opencode Zen",
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(128_000),
    fallback_context_window: 256_000,
    models: &[],
    generations: &[],
    serves_fast_mode: false,
    pricing_schedule: None,
    catalog_slug: Some("opencode"),
};

const XAI: ProviderManifest = ProviderManifest {
    slug: "xai",
    display_name: "xAI",
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(131_072),
    fallback_context_window: 500_000,
    models: xai::models(),
    generations: &[],
    serves_fast_mode: false,
    pricing_schedule: None,
    catalog_slug: Some("xai"),
};

const OPENCODE_GO: ProviderManifest = ProviderManifest {
    slug: "opencode-go",
    display_name: "Opencode Go",
    family: ModelFamily::Generic,
    supports_thinking: false,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(64_000),
    fallback_context_window: 128_000,
    models: &[],
    generations: &[],
    serves_fast_mode: false,
    pricing_schedule: None,
    catalog_slug: Some("opencode-go"),
};

const APERTURE: ProviderManifest = ProviderManifest {
    slug: "aperture",
    display_name: "Aperture",
    family: ModelFamily::Generic,
    supports_thinking: false,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(16_384),
    fallback_context_window: 128_000,
    models: aperture::models(),
    generations: &[],
    serves_fast_mode: false,
    pricing_schedule: None,
    catalog_slug: None,
};

const BUILTINS: &[ProviderManifest] = &[
    ANTHROPIC,
    OPENAI,
    GOOGLE,
    COPILOT,
    COPILOT_ENTERPRISE,
    OLLAMA,
    LLAMA_CPP,
    MISTRAL,
    ZAI,
    DEEPSEEK,
    OPENROUTER,
    SYNTHETIC,
    TENSORX,
    OPENCODE,
    OPENCODE_GO,
    XAI,
    APERTURE,
];

pub struct ManifestRegistry;

impl ManifestRegistry {
    pub fn get(slug: &str) -> Option<&'static ProviderManifest> {
        BUILTINS.iter().find(|m| m.slug == slug)
    }

    /// Like `get`, but resolves dynamic and custom (providers.toml) slugs to
    /// their base provider's manifest so capability lookups (thinking, display
    /// name, tier defaults) still work for stubs that declare no models. `None`
    /// for an unknown slug, so callers pick a fallback instead of silently
    /// inheriting a zeroed manifest.
    pub fn for_slug(slug: &str) -> Option<&'static ProviderManifest> {
        Self::get(slug)
            .or_else(|| dynamic::base_for_slug(slug).and_then(|base| Self::get(&base.to_string())))
            .or_else(|| custom::base_kind(slug).and_then(|base| Self::get(&base.to_string())))
    }

    pub fn builtins() -> &'static [ProviderManifest] {
        BUILTINS
    }

    fn model_supply_manifest(slug: &str) -> Option<&'static ProviderManifest> {
        let dynamic_base = dynamic::base_for_slug(slug).map(|base| base.to_string());
        Self::model_supply_manifest_with_base(slug, dynamic_base.as_deref())
    }

    fn model_supply_manifest_with_base(
        slug: &str,
        dynamic_base: Option<&str>,
    ) -> Option<&'static ProviderManifest> {
        Self::get(slug).or_else(|| dynamic_base.and_then(Self::get))
    }

    fn facts_from_manifest(manifest: &ProviderManifest, model_id: &str) -> Option<ModelFacts> {
        manifest
            .models
            .iter()
            .flat_map(|entry| entry.prefixes.iter().map(move |prefix| (prefix, entry)))
            .filter(|(prefix, _)| model_id.starts_with(*prefix))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, entry)| entry.facts())
    }

    /// The release line `model_id` belongs to, by longest matching member, or
    /// `None` for a provider that declares no lines and for an id outside every
    /// line it does declare.
    fn generation_of(
        manifest: &ProviderManifest,
        model_id: &str,
    ) -> Option<&'static ModelGeneration> {
        manifest
            .generations
            .iter()
            .flat_map(|generation| {
                generation
                    .members
                    .iter()
                    .map(move |member| (member, generation))
            })
            .filter(|(member, _)| model_id.starts_with(*member))
            .max_by_key(|(member, _)| member.len())
            .map(|(_, generation)| generation)
    }

    fn prefixes_from_manifest(
        manifest: &ProviderManifest,
        purpose: ModelPurpose,
        line: Option<&ModelGeneration>,
    ) -> Vec<&'static str> {
        let mut entries: Vec<_> = manifest
            .models
            .iter()
            .filter(|entry| entry.class() == purpose)
            .collect();
        // Stable, so table order still decides within each group: the anchor's
        // own line first, its default ahead of its siblings, then everything
        // else on the same terms.
        entries.sort_by_key(|entry| {
            (
                !line.is_some_and(|line| line.contains(entry)),
                !entry.default,
            )
        });
        entries
            .iter()
            .flat_map(|entry| entry.prefixes)
            .copied()
            .collect()
    }

    /// Builtins and wrappers around a real builtin inherit its curated default.
    /// Custom protocol aliases do not: their protocol says nothing about which
    /// model ids the operator serves.
    pub fn find_default_for_purpose(
        slug: &str,
        purpose: ModelPurpose,
    ) -> Option<&'static ModelEntry> {
        Self::model_supply_manifest(slug)?
            .models
            .iter()
            .find(|entry| entry.default && entry.class() == purpose)
    }

    /// Facts from the most specific curated prefix matching `model_id`.
    pub fn facts_for_model(slug: &str, model_id: &str) -> Option<ModelFacts> {
        Self::facts_from_manifest(Self::model_supply_manifest(slug)?, model_id)
    }

    /// Which size lane the curated table files `model_id` under.
    pub fn purpose_for_model(slug: &str, model_id: &str) -> Option<ModelPurpose> {
        Self::facts_for_model(slug, model_id).map(|facts| facts.class())
    }

    /// Every curated candidate for a slot, the declared default first, so a
    /// caller filtering on a model policy can take the next best rather than
    /// giving up on the provider. `anchor_id` is the model the conversation is
    /// already on, whose release line is preferred over the provider's.
    pub fn prefixes_for_purpose(
        slug: &str,
        purpose: ModelPurpose,
        anchor_id: &str,
    ) -> Vec<&'static str> {
        let Some(manifest) = Self::model_supply_manifest(slug) else {
            return Vec::new();
        };
        let line = Self::generation_of(manifest, anchor_id);
        Self::prefixes_from_manifest(manifest, purpose, line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PricingTier;
    use crate::provider::ProviderKind;
    use caudra_config::providers::BuiltInProvider;
    use caudra_storage::thinking::ReasoningOption;
    use std::str::FromStr;
    use strum::IntoEnumIterator;
    use test_case::test_case;

    const NO_CATALOG_NO_RATES: &str = "a cold catalog must not invent rates";
    const TIERS_COME_FROM_CATALOG: &str =
        "a model the static table does not tier must still bill the catalog's tiers";
    const EMPTY_LADDER_IS_NOT_AN_ANSWER: &str =
        "an unclassified catalog entry must not outrank the static ladder";

    fn meta_with(reasoning_options: ReasoningOptions, tiers: Vec<PricingTier>) -> CatalogMetaView {
        CatalogMetaView {
            context: 922_000,
            context_excludes_output: true,
            output: 128_000,
            input_price: 10.0,
            output_price: 50.0,
            cache_read: 1.0,
            cache_write: 12.5,
            supports_thinking: true,
            supports_vision: true,
            reasoning_options,
            pricing_tiers: tiers,
        }
    }

    #[test]
    fn catalog_pricing_is_absent_without_a_catalog() {
        assert!(catalog_pricing(None).is_none(), "{NO_CATALOG_NO_RATES}");
    }

    #[test]
    fn catalog_pricing_carries_rates_and_tiers() {
        let tier = PricingTier {
            above: 272_000,
            input: 20.0,
            output: 75.0,
            cache_write: 25.0,
            cache_read: 2.0,
            fast: None,
        };
        let meta = meta_with(ReasoningOptions::default(), vec![tier]);

        let pricing = catalog_pricing(Some(&meta)).expect(TIERS_COME_FROM_CATALOG);

        assert_eq!(pricing.input, 10.0);
        assert_eq!(pricing.tiers.len(), 1, "{TIERS_COME_FROM_CATALOG}");
        assert_eq!(pricing.tiers[0].above, 272_000, "{TIERS_COME_FROM_CATALOG}");
    }

    #[test]
    fn an_empty_catalog_ladder_does_not_answer() {
        let meta = meta_with(ReasoningOptions::default(), Vec::new());
        assert!(
            catalog_reasoning_options(Some(&meta)).is_none(),
            "{EMPTY_LADDER_IS_NOT_AN_ANSWER}"
        );

        let classified = meta_with(
            ReasoningOptions::new(vec![ReasoningOption::Effort {
                values: vec!["high".into()],
            }]),
            Vec::new(),
        );
        assert!(catalog_reasoning_options(Some(&classified)).is_some());
    }

    #[test]
    fn every_builtin_manifest_with_provider_kind_matches_kind_fields() {
        for manifest in BUILTINS {
            let Some(kind) = ProviderKind::from_str(manifest.slug).ok() else {
                continue;
            };
            assert_eq!(kind.to_string(), manifest.slug, "{}", manifest.slug);
            assert_eq!(
                manifest.display_name,
                kind.display_name(),
                "{}",
                manifest.slug
            );
            assert_eq!(manifest.family, kind.family(), "{}", manifest.slug);
            assert_eq!(
                manifest.fallback_max_output,
                kind.fallback_max_output(),
                "{}",
                manifest.slug,
            );
            assert_eq!(
                manifest.fallback_context_window,
                kind.fallback_context_window(),
                "{}",
                manifest.slug,
            );
        }
    }

    #[test]
    fn for_slug_returns_none_for_unknown_slug() {
        assert!(ManifestRegistry::for_slug("totally-unknown-slug").is_none());
    }

    #[test]
    fn for_slug_returns_builtin_directly() {
        let manifest = ManifestRegistry::for_slug("anthropic").unwrap();
        assert_eq!(manifest.slug, "anthropic");
        assert_eq!(manifest.display_name, "Anthropic");
    }

    #[test]
    fn dynamic_model_supply_inherits_base_facts_and_candidates() {
        let manifest = ManifestRegistry::model_supply_manifest_with_base(
            "test-openai-wrapper",
            Some("openai"),
        )
        .unwrap();
        let facts = ManifestRegistry::facts_from_manifest(manifest, "gpt-4.1-nano");
        let candidates =
            ManifestRegistry::prefixes_from_manifest(manifest, ModelPurpose::Fast, None);

        assert_eq!(manifest.slug, "openai");
        assert_eq!(
            facts,
            Some(ModelFacts {
                small: true,
                default: false,
            })
        );
        assert_eq!(candidates.first().copied(), Some("gpt-6-luna"));
        assert_eq!(
            format!("{}/{}", "test-openai-wrapper", candidates[0]),
            "test-openai-wrapper/gpt-6-luna"
        );
    }

    /// The anchor's line is a preference, not a filter: a policy that excludes
    /// everything in it must still be able to fall through to the rest.
    #[test]
    fn a_line_reorders_the_candidates_without_dropping_any() {
        let manifest = ManifestRegistry::get("openai").unwrap();
        let wide = ManifestRegistry::prefixes_from_manifest(manifest, ModelPurpose::Fast, None);
        let lined =
            ManifestRegistry::prefixes_for_purpose("openai", ModelPurpose::Fast, "gpt-5.6-sol");

        assert_eq!(lined.first().copied(), Some("gpt-5.6-luna"));
        assert_eq!(wide.first().copied(), Some("gpt-6-luna"));
        assert_eq!(lined.len(), wide.len());
        assert!(lined.iter().all(|prefix| wide.contains(prefix)));
    }

    /// `gpt-6-` alone does not match a point release, which would then answer
    /// its lanes provider-wide rather than from the ladder it extends.
    #[test_case("gpt-6.1-sol", Some("gpt-6") ; "a_point_release_joins_its_line")]
    #[test_case("gpt-5.5", None ; "an_older_model_sits_in_no_line")]
    fn a_model_answers_from_the_line_it_extends(model_id: &str, expected: Option<&str>) {
        let manifest = ManifestRegistry::get("openai").unwrap();
        let line = ManifestRegistry::generation_of(manifest, model_id).map(|line| line.label);
        assert_eq!(line, expected);
    }

    #[test]
    fn custom_protocol_aliases_do_not_inherit_model_supply() {
        assert!(ManifestRegistry::model_supply_manifest_with_base("custom-openai", None).is_none());
    }

    #[test]
    fn builtin_count_covers_provider_kind_variants() {
        let kind_count = ProviderKind::iter().count();
        assert!(
            BUILTINS.len() >= kind_count,
            "BUILTINS has {} manifests but ProviderKind has {} variants",
            BUILTINS.len(),
            kind_count,
        );
        for kind in ProviderKind::iter() {
            assert!(
                ManifestRegistry::get(&kind.to_string()).is_some(),
                "ProviderKind variant {:?} has no manifest",
                kind,
            );
        }
    }

    /// Opencode Zen and Opencode Go come from the fetched catalog and stay
    /// hidden until the user is authed, so they never join the inventory.
    const CATALOG_ONLY_SLUGS: &[&str] = &["opencode", "opencode-go"];

    /// The picker lists the inventory, so a manifest without an entry is a
    /// provider the user cannot reach. OpenRouter shipped that way for months.
    #[test]
    fn every_builtin_manifest_has_inventory_entry() {
        for manifest in BUILTINS {
            if CATALOG_ONLY_SLUGS.contains(&manifest.slug) {
                continue;
            }
            assert!(
                inventory::iter::<BuiltInProvider>()
                    .into_iter()
                    .any(|b| b.slug == manifest.slug),
                "manifest {:?} has no BuiltInProvider entry, so it never shows in the picker",
                manifest.slug,
            );
        }
    }

    #[test]
    fn every_builtin_provider_inventory_entry_has_matching_manifest() {
        for builtin in inventory::iter::<BuiltInProvider>() {
            let manifest = ManifestRegistry::get(builtin.slug).unwrap_or_else(|| {
                panic!(
                    "BuiltInProvider slug {:?} has no ProviderManifest",
                    builtin.slug,
                )
            });
            assert_eq!(
                manifest.display_name, builtin.display_name,
                "display_name mismatch between manifest and BuiltInProvider for slug {:?}",
                builtin.slug,
            );
        }
    }
}
