pub mod auth;
pub mod images;
mod platform;
pub(crate) mod responses;

pub use platform::OpenAi;

pub(crate) const SETUP_MODEL_SPEC: &str = "openai/gpt-6.1-sol";

inventory::submit!(caudra_config::providers::BuiltInProvider {
    slug: "openai",
    display_name: "OpenAI",
    protocol: caudra_config::providers::Protocol::Openai,
    default_base_url: "https://api.openai.com/v1",
    default_api_key_env: "OPENAI_API_KEY",
    default_model: SETUP_MODEL_SPEC,
    plans: None,
    login_url: Some("https://platform.openai.com/api-keys"),
    needs_url: false,
});

#[cfg(test)]
mod tests {
    use crate::manifest::ManifestRegistry;
    use caudra_config::providers::{ProviderDef, resolve_default_model};
    use std::ptr;
    use test_case::test_case;

    use crate::model::lookup_entry;
    use crate::model::{FastPricing, ModelPricing, PricingTier};

    const WIDE_CONTEXT_WINDOW: u32 = 372_000;
    const LONG_CONTEXT_ABOVE: u32 = 272_000;
    /// A quoted rate times 1.5 is not always exact in binary.
    const RATE_TOLERANCE: f64 = 1e-9;
    const LONG_INPUT_AND_CACHE: f64 = 2.0;
    const LONG_OUTPUT: f64 = 1.5;
    const FAST_MULTIPLIER: f64 = 2.0;
    const LONG_CONTEXT_RULE: &str =
        "past 272K OpenAI bills twice the input and cache rates and 1.5x the output rate";
    const FAST_RULE: &str = "OpenAI's fast mode costs twice whichever rate applies";
    const NO_CACHE_DISCOUNT: &str = "OpenAI's pro models offer no cached input discount";

    #[test_case("gpt-4.1-nano")]
    #[test_case("gpt-4.1-nano-2025-04-14")]
    #[test_case("o4-mini")]
    #[test_case("o4-mini-2025-04-16")]
    fn retired_models_are_not_listed(model_id: &str) {
        assert!(
            !ManifestRegistry::get("openai")
                .unwrap()
                .models
                .iter()
                .any(|entry| entry.prefixes.contains(&model_id))
        );
    }

    #[test]
    fn gpt_5_6_alias_shares_sol_metadata_and_generation() {
        let entries = ManifestRegistry::get("openai").unwrap().models;
        let alias = lookup_entry(entries, "gpt-5.6").unwrap();
        let sol = lookup_entry(entries, "gpt-5.6-sol").unwrap();
        assert!(ptr::eq(alias, sol));
        let generation = ManifestRegistry::get("openai")
            .unwrap()
            .generations
            .iter()
            .find(|generation| generation.label == "gpt-5.6")
            .unwrap();
        assert!(
            generation
                .members
                .iter()
                .any(|prefix| "gpt-5.6".starts_with(prefix))
        );
    }

    #[test_case(None, "openai/gpt-6.1-sol" ; "builtin_setup_default")]
    #[test_case(Some(ProviderDef::default()), "openai/gpt-6.1-sol" ; "unconfigured_model_uses_builtin")]
    #[test_case(Some(ProviderDef { default_model: Some("openai/gpt-5.5".into()), ..ProviderDef::default() }), "openai/gpt-5.5" ; "configured_model_takes_precedence")]
    fn setup_default_respects_provider_configuration(def: Option<ProviderDef>, expected: &str) {
        assert_eq!(
            resolve_default_model("openai", def.as_ref()).as_deref(),
            Some(expected)
        );
    }

    fn base(pricing: &ModelPricing) -> [f64; 4] {
        [
            pricing.input,
            pricing.output,
            pricing.cache_write,
            pricing.cache_read,
        ]
    }

    fn of_tier(tier: &PricingTier) -> [f64; 4] {
        [tier.input, tier.output, tier.cache_write, tier.cache_read]
    }

    fn of_fast(fast: &FastPricing) -> [f64; 4] {
        [fast.input, fast.output, fast.cache_write, fast.cache_read]
    }

    fn scaled(rates: [f64; 4], input_and_cache: f64, output: f64) -> [f64; 4] {
        let [input, output_rate, cache_write, cache_read] = rates;
        [
            input * input_and_cache,
            output_rate * output,
            cache_write * input_and_cache,
            cache_read * input_and_cache,
        ]
    }

    fn assert_rates(model_id: &str, actual: [f64; 4], expected: [f64; 4], rule: &str) {
        assert!(
            actual
                .iter()
                .zip(expected)
                .all(|(actual, expected)| (actual - expected).abs() < RATE_TOLERANCE),
            "{model_id}: {actual:?} against {expected:?}: {rule}"
        );
    }

    #[test]
    fn long_context_tiers_follow_openais_rule() {
        for model in ManifestRegistry::get("openai").unwrap().models {
            let model_id = model.prefixes[0];
            for tier in model.pricing.tiers.iter() {
                assert_eq!(
                    tier.above, LONG_CONTEXT_ABOVE,
                    "{model_id}: {LONG_CONTEXT_RULE}"
                );
                let expected = scaled(base(&model.pricing), LONG_INPUT_AND_CACHE, LONG_OUTPUT);
                assert_rates(model_id, of_tier(tier), expected, LONG_CONTEXT_RULE);
            }
        }
    }

    #[test]
    fn fast_is_twice_the_applicable_rate() {
        for model in ManifestRegistry::get("openai").unwrap().models {
            let Some(fast) = &model.pricing.fast else {
                continue;
            };
            let model_id = model.prefixes[0];
            let doubled = scaled(base(&model.pricing), FAST_MULTIPLIER, FAST_MULTIPLIER);
            assert_rates(model_id, of_fast(fast), doubled, FAST_RULE);
            for tier in model.pricing.tiers.iter() {
                let tier_fast = tier.fast.as_ref().expect(FAST_RULE);
                let doubled = scaled(of_tier(tier), FAST_MULTIPLIER, FAST_MULTIPLIER);
                assert_rates(model_id, of_fast(tier_fast), doubled, FAST_RULE);
            }
        }
    }

    #[test_case("gpt-6-astra", true)]
    #[test_case("gpt-6.1-sol", true)]
    #[test_case("gpt-6.1-terra", true)]
    #[test_case("gpt-6-sol", true)]
    #[test_case("gpt-6-luna", true)]
    #[test_case("gpt-5.6-sol", true)]
    #[test_case("gpt-5.6", true)]
    #[test_case("gpt-5.6-terra", true)]
    #[test_case("gpt-5.6-luna", true)]
    #[test_case("gpt-5.5", true)]
    #[test_case("gpt-5.4", true)]
    #[test_case("gpt-5.4-mini", false)]
    #[test_case("gpt-5.4-nano", false)]
    #[test_case("gpt-5.2", false)]
    #[test_case("gpt-5.5-pro", true)]
    #[test_case("gpt-5.4-pro", true)]
    #[test_case("gpt-5.2-pro", false)]
    #[test_case("o3-pro", false)]
    fn only_long_context_models_state_a_tier(model_id: &str, tiered: bool) {
        let entry =
            lookup_entry(ManifestRegistry::get("openai").unwrap().models, model_id).unwrap();
        assert_eq!(
            !entry.pricing.tiers.is_empty(),
            tiered,
            "{LONG_CONTEXT_RULE}"
        );
    }

    /// Lookup is longest-prefix, so a pro id without its own entry would bill
    /// at its base model's rates.
    #[test_case("gpt-5.5-pro", "gpt-5.5-pro", 30.0, 180.0, 1_050_000)]
    #[test_case("gpt-5.5-pro-2026-04-23", "gpt-5.5-pro", 30.0, 180.0, 1_050_000 ; "gpt_5_5_pro_snapshot")]
    #[test_case("gpt-5.4-pro", "gpt-5.4-pro", 30.0, 180.0, 1_050_000)]
    #[test_case("gpt-5.2-pro", "gpt-5.2-pro", 21.0, 168.0, 400_000)]
    #[test_case("o3-pro", "o3-pro", 20.0, 80.0, 200_000)]
    #[test_case("o3-pro-2025-06-10", "o3-pro", 20.0, 80.0, 200_000 ; "o3_pro_snapshot")]
    fn pro_models_bill_at_their_own_rates(
        model_id: &str,
        entry_prefix: &str,
        input: f64,
        output: f64,
        context_window: u32,
    ) {
        let entry =
            lookup_entry(ManifestRegistry::get("openai").unwrap().models, model_id).unwrap();

        assert_eq!(entry.prefixes, [entry_prefix]);
        assert_eq!(entry.pricing.input, input);
        assert_eq!(entry.pricing.output, output);
        assert_eq!(entry.pricing.cache_read, 0.0, "{NO_CACHE_DISCOUNT}");
        assert_eq!(entry.context_window, context_window);
    }

    #[test_case("gpt-6.1-terra", false, 1.0, 0.05, 1.25, 5.0)]
    #[test_case("gpt-5.6-luna", true, 0.2, 0.02, 0.25, 1.2)]
    #[test_case("gpt-5.6-terra", false, 2.0, 0.2, 2.5, 12.0)]
    #[test_case("gpt-5.6-sol", false, 4.0, 0.4, 5.0, 20.0)]
    #[test_case("gpt-5.6", false, 4.0, 0.4, 5.0, 20.0)]
    fn models_have_expected_tier_and_short_context_pricing(
        model_id: &str,
        small: bool,
        input: f64,
        cache_read: f64,
        cache_write: f64,
        output: f64,
    ) {
        let model = ManifestRegistry::get("openai")
            .unwrap()
            .models
            .iter()
            .find(|model| model.prefixes.contains(&model_id))
            .expect("model should be registered");

        assert_eq!(model.small, small);
        assert_eq!(model.context_window, WIDE_CONTEXT_WINDOW);
        assert_eq!(model.pricing.input, input);
        assert_eq!(model.pricing.cache_read, cache_read);
        assert_eq!(model.pricing.cache_write, cache_write);
        assert_eq!(model.pricing.output, output);
    }
}
