pub mod auth;
pub(crate) mod catalog;
mod platform;
mod usage;

pub use platform::Xai;

inventory::submit!(caudra_config::providers::BuiltInProvider {
    slug: "xai",
    display_name: "xAI",
    protocol: caudra_config::providers::Protocol::Openai,
    default_base_url: "https://api.x.ai/v1",
    default_api_key_env: auth::API_KEY_ENV,
    default_model: "xai/grok-4.7",
    plans: None,
    login_url: Some("https://console.x.ai"),
    needs_url: false,
});

#[cfg(test)]
mod tests {
    use crate::manifest::ManifestRegistry;
    use test_case::test_case;

    use crate::model::ModelPricing;
    use crate::model::{Model, ModelInfo, TokenUsage, lookup_entry};
    use crate::model_registry;

    const GROK_CONTEXT_WINDOW: u32 = 500_000;
    const GROK_4_3_CONTEXT_WINDOW: u32 = 1_000_000;
    const GROK_MAX_OUTPUT_TOKENS: u32 = 131_072;
    const LONG_CONTEXT_FROM: u32 = 200_000;
    const XAI_SLUG: &str = "xai";
    const GROK_4_6: &str = "grok-4.6";
    const LONG_CONTEXT_MULTIPLIER: f64 = 2.0;
    const PER_MILLION: f64 = 1_000_000.0;
    const RATE_TOLERANCE: f64 = 1e-9;
    const LONG_CONTEXT_RULE: &str = "from 200K prompt tokens xAI doubles every rate";
    const TIER_SURVIVES_DISCOVERY: &str =
        "a listing that reports base rates alone must not erase the table's tier";

    #[test]
    fn every_tier_doubles_the_base_rates_from_200k() {
        for model in ManifestRegistry::get("xai").unwrap().models {
            let pricing = &model.pricing;
            let [tier] = &*pricing.tiers else {
                panic!("{}: {LONG_CONTEXT_RULE}", model.prefixes[0]);
            };
            assert_eq!(tier.above, LONG_CONTEXT_FROM - 1, "{LONG_CONTEXT_RULE}");
            assert_eq!(
                (tier.input, tier.output, tier.cache_write, tier.cache_read),
                (
                    pricing.input * LONG_CONTEXT_MULTIPLIER,
                    pricing.output * LONG_CONTEXT_MULTIPLIER,
                    pricing.cache_write * LONG_CONTEXT_MULTIPLIER,
                    pricing.cache_read * LONG_CONTEXT_MULTIPLIER,
                ),
                "{}: {LONG_CONTEXT_RULE}",
                model.prefixes[0]
            );
        }
    }

    #[test_case(LONG_CONTEXT_FROM - 1, 2.0 ; "a_prompt_short_of_200k_bills_the_base_rate")]
    #[test_case(LONG_CONTEXT_FROM, 4.0     ; "a_200k_prompt_bills_the_tier")]
    fn xai_doubles_every_rate_from_200k(prompt_tokens: u32, input_rate: f64) {
        let model = Model::from_spec(&format!("{XAI_SLUG}/{GROK_4_6}")).unwrap();
        let usage = TokenUsage {
            input: prompt_tokens,
            ..Default::default()
        };
        let cost = model.billed_cost(&usage, false).unwrap();
        let expected = f64::from(prompt_tokens) * input_rate / PER_MILLION;
        assert!(
            (cost - expected).abs() < RATE_TOLERANCE,
            "{cost} is not {expected}: {LONG_CONTEXT_RULE}"
        );
    }

    /// xAI's model listing copies the table's base rates and nothing else, and
    /// discovery outranks the table.
    #[test]
    fn discovered_rates_keep_the_static_tier() {
        let entry = lookup_entry(ManifestRegistry::get("xai").unwrap().models, GROK_4_6).unwrap();
        model_registry::set_known_models(
            XAI_SLUG,
            vec![ModelInfo {
                pricing: Some(ModelPricing {
                    tiers: ModelPricing::UNTIERED,
                    ..entry.pricing.clone()
                }),
                ..ModelInfo::id_only(GROK_4_6.into())
            }],
        );

        let model = Model::from_spec(&format!("{XAI_SLUG}/{GROK_4_6}")).unwrap();

        assert_eq!(
            model.pricing.tiers, entry.pricing.tiers,
            "{TIER_SURVIVES_DISCOVERY}"
        );
    }

    #[test_case("grok-4.7", 2.0, 6.0, 0.5, GROK_CONTEXT_WINDOW)]
    #[test_case("grok-4.6", 2.0, 6.0, 0.5, GROK_CONTEXT_WINDOW)]
    #[test_case("grok-4.5", 2.0, 6.0, 0.3, GROK_CONTEXT_WINDOW)]
    #[test_case("grok-4.3", 1.25, 2.5, 0.2, GROK_4_3_CONTEXT_WINDOW)]
    fn curated_models_have_expected_metadata(
        model_id: &str,
        input: f64,
        output: f64,
        cache_read: f64,
        context_window: u32,
    ) {
        let model = ManifestRegistry::get("xai")
            .unwrap()
            .models
            .iter()
            .find(|model| model.prefixes.contains(&model_id))
            .expect("curated xAI model should be registered");

        assert!(!model.small);
        assert!(model.vision);
        assert_eq!(model.context_window, context_window);
        assert_eq!(model.max_output_tokens, Some(GROK_MAX_OUTPUT_TOKENS));
        assert_eq!(model.pricing.input, input);
        assert_eq!(model.pricing.output, output);
        assert_eq!(model.pricing.cache_read, cache_read);
    }
}
