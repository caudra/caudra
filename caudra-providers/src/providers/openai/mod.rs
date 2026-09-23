pub mod auth;
pub mod images;
mod platform;
pub(crate) mod responses;

pub use platform::OpenAi;

use crate::model::{ModelEntry, ModelFamily, ModelPricing, StaticReasoningOption};

/// Working window for the long-context OpenAI models: gpt-5.6 luna/terra/sol
/// and the gpt-astra family. Deliberately below what the API accepts — astra
/// alone advertises 1,050,000 — because the window is what caudra fills before
/// compacting, and cost and latency scale with it.
const WIDE_CONTEXT_WINDOW: u32 = 372_000;
const WIDE_MAX_OUTPUT_TOKENS: u32 = 128_000;

inventory::submit!(caudra_config::providers::BuiltInProvider {
    slug: "openai",
    display_name: "OpenAI",
    protocol: caudra_config::providers::Protocol::Openai,
    default_base_url: "https://api.openai.com/v1",
    default_api_key_env: "OPENAI_API_KEY",
    default_model: "openai/gpt-5.5",
    plans: None,
    login_url: Some("https://platform.openai.com/api-keys"),
    needs_url: false,
});

/// Levels these models declare, matching the models.dev catalog. Kept in the
/// table so a cold catalog still resolves an effort correctly, and so the Codex
/// plan models the catalog has not reached resolve at all.
const EFFORT_WITH_MAX: &[StaticReasoningOption] = &[StaticReasoningOption::Effort(&[
    "none", "low", "medium", "high", "xhigh", "max",
])];
const EFFORT_TO_XHIGH: &[StaticReasoningOption] = &[StaticReasoningOption::Effort(&[
    "none", "low", "medium", "high", "xhigh",
])];
const EFFORT_TO_HIGH: &[StaticReasoningOption] = &[StaticReasoningOption::Effort(&[
    "none", "low", "medium", "high",
])];
/// The o-series predates the explicit opt-out, so it always reasons.
const EFFORT_TO_HIGH_NO_NONE: &[StaticReasoningOption] =
    &[StaticReasoningOption::Effort(&["low", "medium", "high"])];
/// The gpt-astra family reasons unconditionally, so it drops the `none` the
/// gpt-5.6 ladder offers while keeping the rungs above it.
const EFFORT_TO_MAX_NO_NONE: &[StaticReasoningOption] = &[StaticReasoningOption::Effort(&[
    "low", "medium", "high", "xhigh", "max",
])];

pub(crate) const fn models() -> &'static [ModelEntry] {
    const MODELS: &[ModelEntry] = &[
        ModelEntry {
            prefixes: &["gpt-6-astra"],
            small: false,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            // The tier above 272k that models.dev publishes cannot be spelled
            // here: this table is a const and a non-empty `tiers` is not
            // const-constructible. `from_base` reads it from the catalog.
            pricing: ModelPricing {
                input: 10.00,
                output: 50.00,
                cache_write: 12.50,
                cache_read: 1.00,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(WIDE_MAX_OUTPUT_TOKENS),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_TO_MAX_NO_NONE),
        },
        ModelEntry {
            prefixes: &["gpt-6-luna"],
            small: true,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.10,
                output: 0.50,
                cache_write: 0.125,
                cache_read: 0.01,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(WIDE_MAX_OUTPUT_TOKENS),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_WITH_MAX),
        },
        ModelEntry {
            prefixes: &["gpt-6-sol"],
            small: false,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 2.00,
                output: 10.00,
                cache_write: 2.50,
                cache_read: 0.20,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(WIDE_MAX_OUTPUT_TOKENS),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_WITH_MAX),
        },
        ModelEntry {
            prefixes: &["gpt-5.6-luna"],
            small: true,
            family: ModelFamily::Gpt,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 1.00,
                output: 6.00,
                cache_write: 1.25,
                cache_read: 0.10,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(WIDE_MAX_OUTPUT_TOKENS),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_WITH_MAX),
        },
        ModelEntry {
            prefixes: &["gpt-5.6-terra"],
            small: false,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 2.50,
                output: 15.00,
                cache_write: 3.125,
                cache_read: 0.25,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(WIDE_MAX_OUTPUT_TOKENS),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_WITH_MAX),
        },
        ModelEntry {
            prefixes: &["gpt-5.6-sol"],
            small: false,
            family: ModelFamily::Gpt,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 5.00,
                output: 30.00,
                cache_write: 6.25,
                cache_read: 0.50,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(WIDE_MAX_OUTPUT_TOKENS),
            context_window: WIDE_CONTEXT_WINDOW,
            reasoning_options: Some(EFFORT_WITH_MAX),
        },
        ModelEntry {
            prefixes: &["gpt-5.4-nano"],
            small: true,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.20,
                output: 1.25,
                cache_write: 0.00,
                cache_read: 0.02,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(128_000),
            context_window: 400_000,
            reasoning_options: Some(EFFORT_TO_XHIGH),
        },
        ModelEntry {
            prefixes: &["gpt-5.4-mini"],
            small: true,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.75,
                output: 4.50,
                cache_write: 0.00,
                cache_read: 0.075,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(128_000),
            context_window: 400_000,
            reasoning_options: Some(EFFORT_TO_XHIGH),
        },
        ModelEntry {
            prefixes: &["gpt-4.1-nano"],
            small: true,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.10,
                output: 0.40,
                cache_write: 0.00,
                cache_read: 0.025,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(32_768),
            context_window: 1_047_576,
            reasoning_options: None,
        },
        ModelEntry {
            prefixes: &["gpt-4.1-mini"],
            small: false,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.40,
                output: 1.60,
                cache_write: 0.00,
                cache_read: 0.10,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(32_768),
            context_window: 1_047_576,
            reasoning_options: None,
        },
        ModelEntry {
            prefixes: &["gpt-4.1"],
            small: false,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 2.00,
                output: 8.00,
                cache_write: 0.00,
                cache_read: 0.50,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(32_768),
            context_window: 1_047_576,
            reasoning_options: None,
        },
        ModelEntry {
            prefixes: &["o4-mini"],
            small: false,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 1.10,
                output: 4.40,
                cache_write: 0.00,
                cache_read: 0.275,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_TO_HIGH_NO_NONE),
        },
        ModelEntry {
            prefixes: &["gpt-5.5"],
            small: false,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 30.00,
                cache_write: 0.00,
                cache_read: 0.50,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(128_000),
            context_window: 1_050_000,
            reasoning_options: Some(EFFORT_TO_XHIGH),
        },
        ModelEntry {
            prefixes: &["gpt-5.4"],
            small: false,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 2.50,
                output: 15.00,
                cache_write: 0.00,
                cache_read: 0.25,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(128_000),
            context_window: 1_050_000,
            reasoning_options: Some(EFFORT_TO_XHIGH),
        },
        ModelEntry {
            prefixes: &["o3"],
            small: false,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 2.00,
                output: 8.00,
                cache_write: 0.00,
                cache_read: 1.00,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(100_000),
            context_window: 200_000,
            reasoning_options: Some(EFFORT_TO_HIGH_NO_NONE),
        },
        ModelEntry {
            prefixes: &["gpt-5.3-codex"],
            small: false,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 1.75,
                output: 14.00,
                cache_write: 0.00,
                cache_read: 0.175,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(128_000),
            context_window: 400_000,
            reasoning_options: Some(EFFORT_TO_XHIGH),
        },
        ModelEntry {
            prefixes: &["gpt-5.2-codex"],
            small: false,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 1.75,
                output: 14.00,
                cache_write: 0.00,
                cache_read: 0.175,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(128_000),
            context_window: 400_000,
            reasoning_options: Some(EFFORT_TO_XHIGH),
        },
        ModelEntry {
            prefixes: &["gpt-5.2"],
            small: false,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 1.75,
                output: 14.00,
                cache_write: 0.00,
                cache_read: 0.175,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(128_000),
            context_window: 400_000,
            reasoning_options: Some(EFFORT_TO_XHIGH),
        },
        ModelEntry {
            prefixes: &["gpt-5.1-codex-mini"],
            small: false,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.25,
                output: 2.00,
                cache_write: 0.00,
                cache_read: 0.025,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(128_000),
            context_window: 400_000,
            reasoning_options: Some(EFFORT_TO_HIGH),
        },
        ModelEntry {
            prefixes: &["gpt-5.1-codex-max"],
            small: false,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 1.25,
                output: 10.00,
                cache_write: 0.00,
                cache_read: 0.125,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(128_000),
            context_window: 400_000,
            reasoning_options: Some(EFFORT_TO_XHIGH),
        },
        ModelEntry {
            prefixes: &["gpt-5.1-codex"],
            small: false,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 1.25,
                output: 10.00,
                cache_write: 0.00,
                cache_read: 0.125,
                fast: None,
                tiers: Vec::new(),
            },
            max_output_tokens: Some(128_000),
            context_window: 400_000,
            reasoning_options: Some(EFFORT_TO_HIGH),
        },
    ];
    MODELS
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("gpt-5.6-luna", true, 1.0, 0.1, 1.25, 6.0)]
    #[test_case("gpt-5.6-terra", false, 2.5, 0.25, 3.125, 15.0)]
    #[test_case("gpt-5.6-sol", false, 5.0, 0.5, 6.25, 30.0)]
    fn gpt_5_6_models_have_expected_tier_and_short_context_pricing(
        model_id: &str,
        small: bool,
        input: f64,
        cache_read: f64,
        cache_write: f64,
        output: f64,
    ) {
        let model = models()
            .iter()
            .find(|model| model.prefixes.contains(&model_id))
            .expect("GPT-5.6 model should be registered");

        assert_eq!(model.small, small);
        assert_eq!(model.context_window, WIDE_CONTEXT_WINDOW);
        assert_eq!(model.pricing.input, input);
        assert_eq!(model.pricing.cache_read, cache_read);
        assert_eq!(model.pricing.cache_write, cache_write);
        assert_eq!(model.pricing.output, output);
    }
}
