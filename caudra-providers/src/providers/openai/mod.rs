pub mod auth;
pub mod images;
mod platform;
pub(crate) mod responses;

pub use platform::OpenAi;

use std::borrow::Cow;

use crate::model::{
    FastPricing, ModelEntry, ModelFamily, ModelGeneration, ModelPricing, PricingTier,
    StaticReasoningOption,
};

/// Working window for the long-context OpenAI models: gpt-5.6 luna/terra/sol
/// and the gpt-astra family. Deliberately below what the API accepts — astra
/// alone advertises 1,050,000 — because the window is what caudra fills before
/// compacting, and cost and latency scale with it.
const WIDE_CONTEXT_WINDOW: u32 = 372_000;
const WIDE_MAX_OUTPUT_TOKENS: u32 = 128_000;
/// A request whose prompt runs past this many tokens bills in full at twice
/// the input and cache rates and one and a half times the output rate. The
/// wide window sits above it, so the last stretch before compaction always
/// pays the long rate.
const LONG_CONTEXT_ABOVE: u32 = 272_000;

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

/// The two ladders OpenAI currently sells side by side. Each carries its own
/// Fast and Best, so a conversation on one never has a lane answered from the
/// other while its own line still has a model for the job.
pub(crate) const fn generations() -> &'static [ModelGeneration] {
    const GENERATIONS: &[ModelGeneration] = &[
        ModelGeneration {
            label: "gpt-6",
            members: &["gpt-6-"],
        },
        ModelGeneration {
            label: "gpt-5.6",
            members: &["gpt-5.6-"],
        },
    ];
    GENERATIONS
}

pub(crate) const fn models() -> &'static [ModelEntry] {
    const MODELS: &[ModelEntry] = &[
        ModelEntry {
            prefixes: &["gpt-6-astra"],
            small: false,
            family: ModelFamily::Gpt,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 10.00,
                output: 50.00,
                cache_write: 12.50,
                cache_read: 1.00,
                fast: Some(FastPricing {
                    input: 20.00,
                    output: 100.00,
                    cache_write: 25.00,
                    cache_read: 2.00,
                }),
                tiers: Cow::Borrowed(&[PricingTier {
                    above: LONG_CONTEXT_ABOVE,
                    input: 20.00,
                    output: 75.00,
                    cache_write: 25.00,
                    cache_read: 2.00,
                    fast: Some(FastPricing {
                        input: 40.00,
                        output: 150.00,
                        cache_write: 50.00,
                        cache_read: 4.00,
                    }),
                }]),
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
            default: true,
            pricing: ModelPricing {
                input: 0.10,
                output: 0.50,
                cache_write: 0.125,
                cache_read: 0.01,
                fast: Some(FastPricing {
                    input: 0.20,
                    output: 1.00,
                    cache_write: 0.25,
                    cache_read: 0.02,
                }),
                tiers: Cow::Borrowed(&[PricingTier {
                    above: LONG_CONTEXT_ABOVE,
                    input: 0.20,
                    output: 0.75,
                    cache_write: 0.25,
                    cache_read: 0.02,
                    fast: Some(FastPricing {
                        input: 0.40,
                        output: 1.50,
                        cache_write: 0.50,
                        cache_read: 0.04,
                    }),
                }]),
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
                fast: Some(FastPricing {
                    input: 4.00,
                    output: 20.00,
                    cache_write: 5.00,
                    cache_read: 0.40,
                }),
                tiers: Cow::Borrowed(&[PricingTier {
                    above: LONG_CONTEXT_ABOVE,
                    input: 4.00,
                    output: 15.00,
                    cache_write: 5.00,
                    cache_read: 0.40,
                    fast: Some(FastPricing {
                        input: 8.00,
                        output: 30.00,
                        cache_write: 10.00,
                        cache_read: 0.80,
                    }),
                }]),
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
                input: 0.20,
                output: 1.20,
                cache_write: 0.25,
                cache_read: 0.02,
                fast: Some(FastPricing {
                    input: 0.40,
                    output: 2.40,
                    cache_write: 0.50,
                    cache_read: 0.04,
                }),
                tiers: Cow::Borrowed(&[PricingTier {
                    above: LONG_CONTEXT_ABOVE,
                    input: 0.40,
                    output: 1.80,
                    cache_write: 0.50,
                    cache_read: 0.04,
                    fast: Some(FastPricing {
                        input: 0.80,
                        output: 3.60,
                        cache_write: 1.00,
                        cache_read: 0.08,
                    }),
                }]),
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
                input: 2.00,
                output: 12.00,
                cache_write: 2.50,
                cache_read: 0.20,
                fast: Some(FastPricing {
                    input: 4.00,
                    output: 24.00,
                    cache_write: 5.00,
                    cache_read: 0.40,
                }),
                tiers: Cow::Borrowed(&[PricingTier {
                    above: LONG_CONTEXT_ABOVE,
                    input: 4.00,
                    output: 18.00,
                    cache_write: 5.00,
                    cache_read: 0.40,
                    fast: Some(FastPricing {
                        input: 8.00,
                        output: 36.00,
                        cache_write: 10.00,
                        cache_read: 0.80,
                    }),
                }]),
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
            // OpenAI's promotional rate, offered through at least 2026-11-21.
            pricing: ModelPricing {
                input: 4.00,
                output: 20.00,
                cache_write: 5.00,
                cache_read: 0.40,
                fast: Some(FastPricing {
                    input: 8.00,
                    output: 40.00,
                    cache_write: 10.00,
                    cache_read: 0.80,
                }),
                tiers: Cow::Borrowed(&[PricingTier {
                    above: LONG_CONTEXT_ABOVE,
                    input: 8.00,
                    output: 30.00,
                    cache_write: 10.00,
                    cache_read: 0.80,
                    fast: Some(FastPricing {
                        input: 16.00,
                        output: 60.00,
                        cache_write: 20.00,
                        cache_read: 1.60,
                    }),
                }]),
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
                tiers: ModelPricing::UNTIERED,
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
                tiers: ModelPricing::UNTIERED,
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
                tiers: ModelPricing::UNTIERED,
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
                tiers: ModelPricing::UNTIERED,
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
                tiers: ModelPricing::UNTIERED,
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
                tiers: ModelPricing::UNTIERED,
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
                tiers: Cow::Borrowed(&[PricingTier {
                    above: LONG_CONTEXT_ABOVE,
                    input: 10.00,
                    output: 45.00,
                    cache_write: 0.00,
                    cache_read: 1.00,
                    fast: None,
                }]),
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
                tiers: Cow::Borrowed(&[PricingTier {
                    above: LONG_CONTEXT_ABOVE,
                    input: 5.00,
                    output: 22.50,
                    cache_write: 0.00,
                    cache_read: 0.50,
                    fast: None,
                }]),
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
                cache_read: 0.50,
                fast: None,
                tiers: ModelPricing::UNTIERED,
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
                tiers: ModelPricing::UNTIERED,
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
                tiers: ModelPricing::UNTIERED,
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
                tiers: ModelPricing::UNTIERED,
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
                tiers: ModelPricing::UNTIERED,
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
                tiers: ModelPricing::UNTIERED,
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
                tiers: ModelPricing::UNTIERED,
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
    use crate::model::lookup_entry;

    /// A quoted rate times 1.5 is not always exact in binary.
    const RATE_TOLERANCE: f64 = 1e-9;
    const LONG_INPUT_AND_CACHE: f64 = 2.0;
    const LONG_OUTPUT: f64 = 1.5;
    const FAST_MULTIPLIER: f64 = 2.0;
    const LONG_CONTEXT_RULE: &str =
        "past 272K OpenAI bills twice the input and cache rates and 1.5x the output rate";
    const FAST_RULE: &str = "OpenAI's fast mode costs twice whichever rate applies";

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
        for model in models() {
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
        for model in models() {
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
    #[test_case("gpt-6-sol", true)]
    #[test_case("gpt-6-luna", true)]
    #[test_case("gpt-5.6-sol", true)]
    #[test_case("gpt-5.6-terra", true)]
    #[test_case("gpt-5.6-luna", true)]
    #[test_case("gpt-5.5", true)]
    #[test_case("gpt-5.4", true)]
    #[test_case("gpt-5.4-mini", false)]
    #[test_case("gpt-5.4-nano", false)]
    #[test_case("gpt-5.2", false)]
    fn only_long_context_models_state_a_tier(model_id: &str, tiered: bool) {
        let entry = lookup_entry(models(), model_id).unwrap();
        assert_eq!(!entry.pricing.tiers.is_empty(), tiered, "{LONG_CONTEXT_RULE}");
    }

    #[test_case("gpt-5.6-luna", true, 0.2, 0.02, 0.25, 1.2)]
    #[test_case("gpt-5.6-terra", false, 2.0, 0.2, 2.5, 12.0)]
    #[test_case("gpt-5.6-sol", false, 4.0, 0.4, 5.0, 20.0)]
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
