//! What the decision endpoints charge per call. Decision models answer typed
//! questions rather than chat, so they live apart from the provider manifests:
//! they never appear in `/model`, model-watch, or the chat price tables.

use caudra_config::decisions::DecisionProtocol;

use crate::model::{ModelPricing, TokenUsage};

#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct DecisionModelEntry {
    pub protocol: DecisionProtocol,
    pub prefixes: &'static [&'static str],
    pub pricing: ModelPricing,
    /// The provider page that states the prices, which model-watch reads
    /// because models.dev does not cover decision models.
    pub source: &'static str,
}

pub fn entries() -> &'static [DecisionModelEntry] {
    crate::model_data::current().decision_models
}

/// The cost of one decision call, priced by the first of `models` the table
/// knows for `protocol`: the id that answered, then the one requested. `None`
/// for a model outside the table, such as a local server's own model.
pub fn decision_cost(
    protocol: &DecisionProtocol,
    models: &[&str],
    usage: &TokenUsage,
) -> Option<f64> {
    models
        .iter()
        .find_map(|model| lookup(protocol, model))
        .map(|entry| usage.cost(&entry.pricing, false))
}

fn lookup(protocol: &DecisionProtocol, model: &str) -> Option<&'static DecisionModelEntry> {
    entries()
        .iter()
        .filter(|entry| entry.protocol == *protocol)
        .flat_map(|entry| entry.prefixes.iter().map(move |prefix| (prefix, entry)))
        .filter(|(prefix, _)| model.starts_with(**prefix))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, entry)| entry)
}

#[cfg(test)]
mod tests {
    use caudra_config::decisions::DecisionProtocol;
    use test_case::test_case;

    use super::{decision_cost, lookup};
    use crate::manifest::ManifestRegistry;
    use crate::model::{TokenUsage, lookup_entry};

    const TOLERANCE: f64 = 1e-12;
    const MILLION: u32 = 1_000_000;
    const SHORT_PROMPT: u32 = 50_000;
    const LONG_PROMPT: u32 = 300_000;
    const LUNA: &str = "gpt-6-luna";
    const CACHE_RATES_DRIFTED: &str =
        "OpenAI bills Decisions cache tokens at gpt-6-luna's standard cache rates";

    fn usage(input: u32, cache_read: u32, cache_creation: u32, output: u32) -> TokenUsage {
        TokenUsage {
            input,
            output,
            cache_creation,
            cache_read,
        }
    }

    #[test_case(DecisionProtocol::TypeSafe, &["jev-1.14.0", "jev-latest"], 0.042 ; "jev_versioned_id")]
    #[test_case(DecisionProtocol::TypeSafe, &["jev-latest"], 0.042 ; "jev_alias")]
    #[test_case(DecisionProtocol::TypeSafe, &["jev-1.13.0"], 0.042 ; "jev_previous_release")]
    #[test_case(DecisionProtocol::TypeSafe, &["jev-9.0.0", "jev-preview"], 0.042 ; "unknown_answer_falls_back_to_request")]
    fn a_million_input_tokens_cost_the_listed_input_rate(
        protocol: DecisionProtocol,
        models: &[&str],
        expected: f64,
    ) {
        let cost = decision_cost(&protocol, models, &usage(MILLION, 0, 0, MILLION)).unwrap();
        assert!((cost - expected).abs() < TOLERANCE, "{cost} != {expected}");
    }

    #[test_case(DecisionProtocol::OpenAI, "decision-1" ; "local_model")]
    #[test_case(DecisionProtocol::TypeSafe, "jev-1.15.0" ; "unreleased_jev")]
    #[test_case(DecisionProtocol::OpenAI, "jev-latest" ; "jev_under_openai")]
    #[test_case(DecisionProtocol::TypeSafe, LUNA ; "luna_under_typesafe")]
    fn models_outside_the_table_are_unpriced(protocol: DecisionProtocol, model: &str) {
        assert_eq!(
            decision_cost(&protocol, &[model], &usage(MILLION, 0, 0, 0)),
            None
        );
    }

    #[test_case(LUNA, usage(SHORT_PROMPT, SHORT_PROMPT, SHORT_PROMPT, MILLION), (0.10 + 0.01 + 0.125) * 0.05 ; "short_prompt")]
    #[test_case("gpt-6-luna-2026-10-06", usage(SHORT_PROMPT, 0, 0, 0), 0.10 * 0.05 ; "snapshot")]
    #[test_case(LUNA, usage(LONG_PROMPT, 0, 0, MILLION), 0.20 * 0.3 ; "long_prompt")]
    #[test_case(LUNA, usage(LONG_PROMPT, LONG_PROMPT, LONG_PROMPT, 0), (0.20 + 0.02 + 0.25) * 0.3 ; "long_prompt_with_cache")]
    fn luna_splits_cache_and_never_bills_output(model: &str, usage: TokenUsage, expected: f64) {
        let cost = decision_cost(&DecisionProtocol::OpenAI, &[model], &usage).unwrap();
        assert!((cost - expected).abs() < TOLERANCE, "{cost} != {expected}");
    }

    #[test]
    fn luna_cache_rates_follow_the_chat_entry() {
        let decision = &lookup(&DecisionProtocol::OpenAI, LUNA).unwrap().pricing;
        let chat = &lookup_entry(ManifestRegistry::get("openai").unwrap().models, LUNA)
            .unwrap()
            .pricing;
        assert_eq!(
            (decision.cache_read, decision.cache_write),
            (chat.cache_read, chat.cache_write),
            "{CACHE_RATES_DRIFTED}"
        );
        let pairs: Vec<_> = decision
            .tiers
            .iter()
            .zip(chat.tiers.iter())
            .map(|(decision, chat)| {
                (
                    (decision.above, decision.cache_read, decision.cache_write),
                    (chat.above, chat.cache_read, chat.cache_write),
                )
            })
            .collect();
        assert_eq!(pairs.len(), chat.tiers.len(), "{CACHE_RATES_DRIFTED}");
        for (decision, chat) in pairs {
            assert_eq!(decision, chat, "{CACHE_RATES_DRIFTED}");
        }
    }
}
