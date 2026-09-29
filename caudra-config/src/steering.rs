//! Optional fields preserve inheritance across config layers until the model is resolved.
//! Resolving defaults earlier would turn omitted fields into explicit overrides.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ConfigError, ConfigField, ConfigValue};

const DEFAULT_ENABLED: bool = true;
const DEFAULT_MAX_RECOVERIES: u32 = 32;
const DEFAULT_MAX_ADVISORIES: u32 = 4;
/// Longer than any single rule's episode, so a stalling rule still ends on its
/// own terms and reports itself. Every rule is bounded alone; nothing bounded
/// the turns that interleaved rules spend between them.
const DEFAULT_MAX_STALLED_TURNS: u32 = 5;
// Bound recovery work, retained history, and user-supplied configuration size.
const MAX_COUNT: usize = 1024;
const MAX_WINDOW: usize = 4096;
const MAX_PROMPT_BYTES: usize = 16 * 1024;
const MAX_MODELS: usize = 256;
const MAX_MODEL_ID_BYTES: usize = 512;

macro_rules! override_fields {
    ($target:ident, $source:ident, $($field:ident),+ $(,)?) => {
        $(if let Some(value) = &$source.$field {
            $target.$field = *value;
        })+
    };
}

macro_rules! merge_fields {
    ($target:ident, $source:ident, $($field:ident),+ $(,)?) => {
        $(if $source.$field.is_some() {
            $target.$field = $source.$field;
        })+
    };
}

macro_rules! rule {
    ($config:ident, $policy:ident, {
        $($field:ident: $ty:ty = $default:literal, $min:literal..=$max:ident, $description:literal);+ $(;)?
    }) => {
        #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        pub struct $config {
            pub enabled: Option<bool>,
            pub prompt: Option<String>,
            $(pub $field: Option<$ty>,)+
        }

        impl $config {
            fn merge(&mut self, overlay: Self) {
                merge_fields!(self, overlay, enabled, prompt, $($field),+);
            }

            fn apply(&self, policy: &mut $policy) {
                override_fields!(policy, self, enabled, $($field),+);
                if let Some(prompt) = &self.prompt {
                    policy.prompt = Some(prompt.clone());
                }
            }
        }

        #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
        pub struct $policy {
            pub enabled: bool,
            pub prompt: Option<String>,
            $(pub $field: $ty,)+
        }

        impl Default for $policy {
            fn default() -> Self {
                Self {
                    enabled: DEFAULT_ENABLED,
                    prompt: None,
                    $($field: $default,)+
                }
            }
        }

        impl $policy {
            const FIELDS: &[ConfigField] = &[$(ConfigField {
                name: stringify!($field),
                ty: "integer",
                default: ConfigValue::U64($default),
                min: Some($min),
                max: Some($max as u64),
                env: None,
                description: $description,
            }),+];

            fn validate(&self, path: &str) -> Result<(), ConfigError> {
                if let Some(prompt) = &self.prompt
                    && (prompt.trim().is_empty() || prompt.len() > MAX_PROMPT_BYTES)
                {
                    return Err(invalid(
                        format!("{path}.prompt"),
                        format!("must be nonblank and at most {MAX_PROMPT_BYTES} bytes"),
                    ));
                }
                $(validate_range(
                    &format!("{path}.{}", stringify!($field)),
                    self.$field as usize,
                    $min,
                    $max,
                )?;)+
                Ok(())
            }
        }
    };
}

rule!(TruncationConfig, TruncationPolicy, {
    max_attempts: u32 = 3, 1..=MAX_COUNT,
        "Actual truncation-correction requests per externally initiated invocation, shared across truncation episodes.";
});
rule!(EmptyResponseConfig, EmptyResponsePolicy, {
    max_after_tools: u32 = 3, 1..=MAX_COUNT,
        "Empty-output continuations per episode after recent tool results.";
    max_idle: u32 = 2, 1..=MAX_COUNT,
        "Empty-output continuations per episode without recent tool results.";
    max_barren: u32 = 1, 1..=MAX_COUNT,
        "Continuations per episode after a response that carried no content at all. Clamped by the limit above; repeating an unchanged request is not a retry.";
    recent_tool_window: usize = 5, 1..=MAX_WINDOW,
        "Non-padding history messages inspected for recent tool results.";
});
rule!(RepeatedToolCallConfig, RepeatedToolCallPolicy, {
    threshold: usize = 3, 2..=MAX_COUNT,
        "Consecutive identical top-level calls. Refuse the call reaching this threshold.";
});
rule!(ProtocolMismatchConfig, ProtocolMismatchPolicy, {
    max_attempts: u32 = 2, 1..=MAX_COUNT,
        "Protocol corrective continuations per episode.";
});
rule!(MissingTaskReportConfig, MissingTaskReportPolicy, {
    max_attempts: u32 = 2, 1..=MAX_COUNT,
        "Additional report-correction prompts per task invocation.";
});
rule!(AbandonedTurnConfig, AbandonedTurnPolicy, {
    max_attempts: u32 = 2, 1..=MAX_COUNT,
        "Continuations per episode after a turn that announced work instead of doing it.";
});
rule!(RepetitionConfig, RepetitionPolicy, {
    window: usize = 24, 1..=MAX_WINDOW,
        "Recent normalized leaf tool calls retained for cycle detection.";
    cycle_repeats: usize = 3, 2..=MAX_COUNT,
        "Exact repetitions of a tool cycle needed for an advisory.";
    max_cycle: usize = 4, 2..=MAX_COUNT,
        "Maximum cycle length in leaf calls. Candidate cycle lengths start at 2.";
    text_window: usize = 8, 1..=MAX_WINDOW,
        "Recent completed assistant responses retained for text repetition.";
    text_repeats: usize = 3, 2..=MAX_COUNT,
        "Matching nontrivial normalized assistant responses needed for an advisory.";
    cooldown: u32 = 3, 1..=MAX_COUNT,
        "Completed model responses between this rule's advisories.";
});
rule!(ToolPlanningConfig, ToolPlanningPolicy, {
    after_calls: usize = 6, 1..=MAX_COUNT,
        "Number of most recent leaf tool calls that must all have failed since the last response containing a successful result.";
    after_responses: usize = 3, 1..=MAX_COUNT,
        "Distinct completed model responses represented by those failed calls.";
    cooldown: u32 = 3, 1..=MAX_COUNT,
        "Completed model responses between this rule's advisories.";
});

/// One `[agent.steering.rules.<name>]` table. Every rule also takes
/// [`SteeringRule::COMMON_FIELDS`].
pub struct SteeringRule {
    pub name: &'static str,
    pub description: &'static str,
    pub fields: &'static [ConfigField],
}

impl SteeringRule {
    pub const COMMON_FIELDS: &[ConfigField] = &[
        ConfigField {
            name: "enabled",
            ty: "boolean",
            default: ConfigValue::Bool(DEFAULT_ENABLED),
            min: None,
            max: None,
            env: None,
            description: "Explicit `false` disables this rule.",
        },
        ConfigField {
            name: "prompt",
            ty: "string",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            description: "Use built-in guidance when omitted. Custom text must be nonblank and at most 16,384 UTF-8 bytes.",
        },
    ];
}

macro_rules! rules {
    ($($field:ident: $config:ident => $policy:ident, $description:literal);+ $(;)?) => {
        #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        pub struct SteeringRulesConfig {
            $(pub $field: $config,)+
        }

        impl SteeringRulesConfig {
            pub const RULES: &[SteeringRule] = &[$(SteeringRule {
                name: stringify!($field),
                description: $description,
                fields: $policy::FIELDS,
            }),+];

            fn merge(&mut self, overlay: Self) {
                $(self.$field.merge(overlay.$field);)+
            }

            fn apply(&self, policy: &mut SteeringRules) {
                $(self.$field.apply(&mut policy.$field);)+
            }
        }

        #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
        pub struct SteeringRules {
            $(pub $field: $policy,)+
        }

        impl SteeringRules {
            fn validate(&self, path: &str) -> Result<(), ConfigError> {
                $(self.$field.validate(&format!("{path}.{}", stringify!($field)))?;)+
                // Observation windows must fit their trigger; planning cannot require fewer
                // tool calls than tool-bearing responses.
                validate_range(
                    &format!("{path}.repetition.window"),
                    self.repetition.window,
                    self.repetition.max_cycle * self.repetition.cycle_repeats,
                    MAX_WINDOW,
                )?;
                validate_range(
                    &format!("{path}.repetition.text_window"),
                    self.repetition.text_window,
                    self.repetition.text_repeats,
                    MAX_WINDOW,
                )?;
                validate_range(
                    &format!("{path}.tool_planning.after_calls"),
                    self.tool_planning.after_calls,
                    self.tool_planning.after_responses,
                    MAX_COUNT,
                )
            }
        }
    };
}

rules! {
    truncation: TruncationConfig => TruncationPolicy,
        "Continue output cut off by the response token limit, up to 3 corrective requests per externally initiated invocation.";
    empty_response: EmptyResponseConfig => EmptyResponsePolicy,
        "Continue after empty output, with separate per-episode limits after recent tools and while idle.";
    repeated_tool_call: RepeatedToolCallConfig => RepeatedToolCallPolicy,
        "Refuse the third consecutive identical top-level tool name/input before execution. Native batch children do not acquire this hard blocker.";
    protocol_mismatch: ProtocolMismatchConfig => ProtocolMismatchPolicy,
        "Correct an explicit provider tool-use indication with no actual tool calls, up to 2 continuations per episode.";
    missing_task_report: MissingTaskReportConfig => MissingTaskReportPolicy,
        "Request a missing task summary or required structured report, up to 2 corrections.";
    abandoned_turn: AbandonedTurnConfig => AbandonedTurnPolicy,
        "Continue a turn that ended by announcing work the response never performed, up to 2 continuations per episode. Spending the allowance accepts the text rather than failing the turn.";
    repetition: RepetitionConfig => RepetitionPolicy,
        "Advise on short exact tool cycles, including normalized native batch leaf calls, or repeated normalized assistant text.";
    tool_planning: ToolPlanningConfig => ToolPlanningPolicy,
        "Advise after consecutive failed tool attempts across responses, including attempts with different tools or inputs. Any successful tool result ends the failure episode. Repeating a successful call is insufficient.";
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SteeringConfig {
    pub enabled: Option<bool>,
    pub max_recoveries: Option<u32>,
    pub max_advisories: Option<u32>,
    pub max_stalled_turns: Option<u32>,
    pub rules: SteeringRulesConfig,
    pub models: BTreeMap<String, SteeringModelConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SteeringModelConfig {
    pub enabled: Option<bool>,
    pub max_recoveries: Option<u32>,
    pub max_advisories: Option<u32>,
    pub max_stalled_turns: Option<u32>,
    pub rules: SteeringRulesConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SteeringPolicy {
    pub enabled: bool,
    pub max_recoveries: u32,
    pub max_advisories: u32,
    pub max_stalled_turns: u32,
    pub rules: SteeringRules,
}

impl SteeringModelConfig {
    pub fn merge(&mut self, overlay: Self) {
        merge_fields!(
            self,
            overlay,
            enabled,
            max_recoveries,
            max_advisories,
            max_stalled_turns
        );
        self.rules.merge(overlay.rules);
    }
}

impl SteeringConfig {
    pub const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: "enabled",
            ty: "boolean",
            default: ConfigValue::Bool(DEFAULT_ENABLED),
            min: None,
            max: None,
            env: None,
            description: "Master switch for automatic steering, including truncation recovery and repeat-policy blocking.",
        },
        ConfigField {
            name: "max_recoveries",
            ty: "integer",
            default: ConfigValue::U64(DEFAULT_MAX_RECOVERIES as u64),
            min: Some(0),
            max: Some(MAX_COUNT as u64),
            env: None,
            description: "Corrective continuations per externally initiated invocation. Zero prevents optional recovery continuations.",
        },
        ConfigField {
            name: "max_advisories",
            ty: "integer",
            default: ConfigValue::U64(DEFAULT_MAX_ADVISORIES as u64),
            min: Some(0),
            max: Some(MAX_COUNT as u64),
            env: None,
            description: "Advisory injections per invocation. Zero suppresses advisories.",
        },
        ConfigField {
            name: "max_stalled_turns",
            ty: "integer",
            default: ConfigValue::U64(DEFAULT_MAX_STALLED_TURNS as u64),
            min: Some(0),
            max: Some(MAX_COUNT as u64),
            env: None,
            description: "Consecutive turns carrying neither a tool call nor visible text before the run ends, whichever rule intervened. Zero disables the backstop.",
        },
        ConfigField {
            name: "rules",
            ty: "table",
            default: ConfigValue::Toml("{}"),
            min: None,
            max: None,
            env: None,
            description: "Overrides by rule name, listed below. Omission uses built-in defaults.",
        },
        ConfigField {
            name: "models",
            ty: "table",
            default: ConfigValue::Toml("{}"),
            min: None,
            max: None,
            env: None,
            description: "Up to 256 exact `provider/model-id` keys, each with its own overrides.",
        },
    ];

    /// Later layers replace explicit fields, while rules and exact model entries merge
    /// field by field so omitted settings continue to inherit.
    pub fn merge(&mut self, overlay: Self) {
        merge_fields!(
            self,
            overlay,
            enabled,
            max_recoveries,
            max_advisories,
            max_stalled_turns
        );
        self.rules.merge(overlay.rules);
        for (model, policy) in overlay.models {
            self.models.entry(model).or_default().merge(policy);
        }
    }

    /// Explicit global settings override built-in defaults, then exact model settings
    /// take final precedence.
    pub fn resolve(&self, model: &str) -> SteeringPolicy {
        self.resolve_override(self.models.get(model))
    }

    fn resolve_override(&self, model: Option<&SteeringModelConfig>) -> SteeringPolicy {
        let mut policy = SteeringPolicy {
            enabled: DEFAULT_ENABLED,
            max_recoveries: DEFAULT_MAX_RECOVERIES,
            max_advisories: DEFAULT_MAX_ADVISORIES,
            max_stalled_turns: DEFAULT_MAX_STALLED_TURNS,
            rules: SteeringRules::default(),
        };
        override_fields!(
            policy,
            self,
            enabled,
            max_recoveries,
            max_advisories,
            max_stalled_turns
        );
        self.rules.apply(&mut policy.rules);
        if let Some(model) = model {
            override_fields!(
                policy,
                model,
                enabled,
                max_recoveries,
                max_advisories,
                max_stalled_turns
            );
            model.rules.apply(&mut policy.rules);
        }
        policy
    }

    /// Check fallback and model-specific policies after inheritance, including disabled
    /// rules, so enabling them later cannot expose invalid thresholds or windows.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_range("models", self.models.len(), 0, MAX_MODELS)?;
        self.resolve_override(None).validate("")?;
        for (model, policy) in &self.models {
            validate_range("models.model_id_bytes", model.len(), 1, MAX_MODEL_ID_BYTES)?;
            let path = format!("models[{model:?}]");
            if !model.contains('/')
                || model.split('/').any(str::is_empty)
                || model.chars().any(|character| {
                    character.is_whitespace()
                        || character.is_control()
                        || matches!(character, '*' | '?' | '[' | ']' | '{' | '}' | '\\')
                })
            {
                return Err(invalid(
                    path,
                    format!(
                        "expected an exact provider/model ID without whitespace or patterns, at most {MAX_MODEL_ID_BYTES} bytes"
                    ),
                ));
            }
            self.resolve_override(Some(policy))
                .validate(&format!("{path}."))?;
        }
        Ok(())
    }
}

impl SteeringPolicy {
    fn validate(&self, prefix: &str) -> Result<(), ConfigError> {
        // Zero budgets disable interventions; rule thresholds still require usable values.
        validate_range(
            &format!("{prefix}max_recoveries"),
            self.max_recoveries as usize,
            0,
            MAX_COUNT,
        )?;
        validate_range(
            &format!("{prefix}max_advisories"),
            self.max_advisories as usize,
            0,
            MAX_COUNT,
        )?;
        validate_range(
            &format!("{prefix}max_stalled_turns"),
            self.max_stalled_turns as usize,
            0,
            MAX_COUNT,
        )?;
        self.rules.validate(&format!("{prefix}rules"))
    }
}

fn invalid(field: String, message: String) -> ConfigError {
    ConfigError::InvalidSteering { field, message }
}

fn validate_range(field: &str, value: usize, min: usize, max: usize) -> Result<(), ConfigError> {
    if !(min..=max).contains(&value) {
        return Err(invalid(
            field.to_owned(),
            format!("must be between {min} and {max}, got {value}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{MAX_COUNT, MAX_MODEL_ID_BYTES, MAX_MODELS, MAX_PROMPT_BYTES, MAX_WINDOW};
    use super::{SteeringConfig, SteeringModelConfig};
    use crate::{AgentConfig, ConfigError, RawConfig};

    const MODEL: &str = "provider/model";
    const OTHER_MODEL: &str = "provider/other";
    const PROMPT: &str = "  Reassess the next useful action. {{literal}}  ";
    const GLOBAL_PROMPT: &str = "Global guidance";
    const INVALID_FIELD: &str = "rules.repetition.text_window";
    const INVALID_MESSAGE: &str = "must be between 3 and 4096, got 2";
    const UNKNOWN_FIELD: &str = "unknown field";
    const RULES: [&str; 8] = [
        "truncation",
        "empty_response",
        "repeated_tool_call",
        "protocol_mismatch",
        "missing_task_report",
        "abandoned_turn",
        "repetition",
        "tool_planning",
    ];

    fn config(value: Value) -> SteeringConfig {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn policy_defaults() {
        let config = SteeringConfig::default();
        config.validate().unwrap();
        assert_eq!(
            serde_json::to_value(config.resolve(MODEL)).unwrap(),
            json!({
                "enabled": true, "max_recoveries": 32, "max_advisories": 4,
                "max_stalled_turns": 5,
                "rules": {
                    "truncation": {"enabled": true, "prompt": null, "max_attempts": 3},
                    "empty_response": {
                        "enabled": true, "prompt": null, "max_after_tools": 3,
                        "max_idle": 2, "max_barren": 1, "recent_tool_window": 5
                    },
                    "repeated_tool_call": {"enabled": true, "prompt": null, "threshold": 3},
                    "protocol_mismatch": {"enabled": true, "prompt": null, "max_attempts": 2},
                    "missing_task_report": {"enabled": true, "prompt": null, "max_attempts": 2},
                    "abandoned_turn": {"enabled": true, "prompt": null, "max_attempts": 2},
                    "repetition": {
                        "enabled": true, "prompt": null, "window": 24, "cycle_repeats": 3,
                        "max_cycle": 4, "text_window": 8, "text_repeats": 3, "cooldown": 3
                    },
                    "tool_planning": {
                        "enabled": true, "prompt": null, "after_calls": 6,
                        "after_responses": 3, "cooldown": 3
                    }
                }
            })
        );
    }

    #[test_case(false, true; "model_enables")]
    #[test_case(true, false; "model_disables")]
    #[test_case(true, true; "both_enabled")]
    fn resolution_precedence(global: bool, model: bool) {
        let config = config(json!({
            "enabled": false,
            "max_recoveries": 7,
            "max_advisories": 0,
            "rules": {
                "repetition": {"enabled": global},
                "tool_planning": {"enabled": false, "after_responses": 4},
                "empty_response": {"max_idle": 5, "prompt": GLOBAL_PROMPT}
            },
            "models": {(MODEL): {
                "enabled": true,
                "max_recoveries": 0,
                "rules": {
                    "repetition": {"enabled": model},
                    "tool_planning": {"after_responses": 6},
                    "empty_response": {"prompt": PROMPT}
                }
            }}
        }));
        config.validate().unwrap();
        let policy = config.resolve(MODEL);
        assert!(policy.enabled);
        assert_eq!(policy.max_recoveries, 0);
        assert_eq!(policy.max_advisories, 0);
        assert_eq!(policy.rules.repetition.enabled, model);
        assert!(!policy.rules.tool_planning.enabled);
        assert_eq!(policy.rules.tool_planning.after_responses, 6);
        assert_eq!(policy.rules.empty_response.max_idle, 5);
        assert_eq!(policy.rules.empty_response.prompt.as_deref(), Some(PROMPT));
        let unmatched = config.resolve(OTHER_MODEL);
        assert_eq!(unmatched.rules.repetition.enabled, global);
        assert!(!unmatched.enabled);
        assert_eq!(unmatched.max_recoveries, 7);
        assert_eq!(unmatched.rules.tool_planning.after_responses, 4);
        assert_eq!(
            unmatched.rules.empty_response.prompt.as_deref(),
            Some(GLOBAL_PROMPT)
        );
    }

    #[test_case(MODEL, false; "exact")]
    #[test_case("provider/model/suffix", true; "not_prefix")]
    #[test_case("Provider/model", true; "case_sensitive")]
    #[test_case("other/model", true; "not_suffix")]
    fn model_matching_is_exact(model: &str, enabled: bool) {
        let config = config(json!({"models": {(MODEL): {"rules": {
            "tool_planning": {"enabled": false}
        }}}}));
        assert_eq!(config.resolve(model).rules.tool_planning.enabled, enabled);
    }

    #[test_case(true; "omitted")]
    #[test_case(false; "explicit_false")]
    fn raw_merge_preserves_inheritance_and_explicit_values(omitted: bool) {
        let mut base: RawConfig = serde_json::from_value(json!({"agent": {"steering": {
            "rules": {
                "tool_planning": {"after_responses": 4, "prompt": GLOBAL_PROMPT},
                "truncation": {"max_attempts": 5, "prompt": GLOBAL_PROMPT}
            },
            "models": {
                (MODEL): {"rules": {
                    "empty_response": {"max_idle": 5, "prompt": PROMPT},
                    "truncation": {"max_attempts": 7}
                }},
                (OTHER_MODEL): {"max_advisories": 1}
            }
        }}}))
        .unwrap();
        let overlay = if omitted {
            json!({"agent": {"steering": {"models": {}, "rules": {}}}})
        } else {
            json!({"agent": {"steering": {
                "enabled": false, "max_recoveries": 0, "max_advisories": 0,
                "rules": {
                    "tool_planning": {"enabled": false},
                    "truncation": {"enabled": false}
                },
                "models": {(MODEL): {
                    "enabled": false, "max_recoveries": 0, "max_advisories": 0,
                    "rules": {
                        "empty_response": {"enabled": false},
                        "truncation": {"prompt": PROMPT}
                    }
                }}
            }}})
        };
        base.merge(serde_json::from_value(overlay).unwrap());
        let config = base.into_config(false).unwrap();
        config.validate().unwrap();
        let steering = &config.agent.steering;
        assert_eq!(steering.models.len(), 2);
        assert_eq!(steering.rules.empty_response.max_idle, None);
        assert_eq!(steering.models[MODEL].rules.truncation.enabled, None);
        assert_eq!(
            steering.models[MODEL].rules.empty_response.max_idle,
            Some(5)
        );
        let policy = steering.resolve(MODEL);
        assert_eq!(policy.rules.truncation.enabled, omitted);
        assert_eq!(policy.rules.truncation.max_attempts, 7);
        assert_eq!(
            policy.rules.truncation.prompt.as_deref(),
            Some(if omitted { GLOBAL_PROMPT } else { PROMPT })
        );
        assert_eq!(
            steering.resolve(OTHER_MODEL).rules.truncation.max_attempts,
            5
        );
        assert_eq!(policy.enabled, omitted);
        assert_eq!(policy.rules.empty_response.enabled, omitted);
        assert_eq!(policy.rules.tool_planning.enabled, omitted);
        assert_eq!(policy.rules.tool_planning.after_responses, 4);
        assert_eq!(
            policy.rules.tool_planning.prompt.as_deref(),
            Some(GLOBAL_PROMPT)
        );
        assert_eq!(policy.rules.empty_response.prompt.as_deref(), Some(PROMPT));
        if !omitted {
            assert_eq!(policy.max_recoveries, 0);
            assert_eq!(policy.max_advisories, 0);
        }
        let child = config.agent.clone();
        assert_eq!(child.steering, *steering);
        assert!(Arc::ptr_eq(&child.steering, steering));
        assert_eq!(child.steering.resolve(OTHER_MODEL).max_advisories, 1);
        let serialized = serde_json::to_value(steering).unwrap();
        assert_eq!(self::config(serialized), **steering);
    }

    #[test_case("{}"; "no_agent")]
    #[test_case(r#"{"agent":{}}"#; "no_steering")]
    #[test_case(r#"{"agent":{"steering":{}}}"#; "empty_steering")]
    fn omitted_settings_stay_unresolved(raw: &str) {
        let config = serde_json::from_str::<RawConfig>(raw)
            .unwrap()
            .into_config(false)
            .unwrap();
        assert_eq!(*config.agent.steering, SteeringConfig::default());
        assert_eq!(AgentConfig::default().steering, config.agent.steering);
        let rules = serde_json::to_value(config.agent.steering.resolve(MODEL).rules).unwrap();
        assert_eq!(rules.as_object().unwrap().len(), RULES.len());
        for rule in RULES {
            assert_eq!(rules[rule]["enabled"], true);
        }
    }

    #[test_case(r#"{"unknown":true}"#; "global")]
    #[test_case(r#"{"rules":{"unknown":{}}}"#; "rule_name")]
    #[test_case(r#"{"rules":{"empty_response":{"threshold":3}}}"#; "wrong_rule_parameter")]
    #[test_case(r#"{"models":{"provider/model":{"unknown":true}}}"#; "model")]
    #[test_case(r#"{"models":{"provider/model":{"models":{}}}}"#; "recursive_models")]
    #[test_case(r#"{"models":{"provider/model":{"rules":{"tool_planning":{"unknown":3}}}}}"#; "model_rule")]
    #[test_case(r#"{"max_recoveries":-1}"#; "negative_budget")]
    #[test_case(r#"{"rules":{"repetition":{"window":1.5}}}"#; "fractional_count")]
    #[test_case(r#"{"rules":{"truncation":{"max_attempts":-1}}}"#; "negative_attempts")]
    #[test_case(r#"{"rules":{"truncation":{"max_attempts":1.5}}}"#; "fractional_attempts")]
    #[test_case(r#"{"rules":{"truncation":{"max_attempts":4294967296}}}"#; "overflow_attempts")]
    #[test_case(r#"{"rules":{"truncation":{"max_idle":3}}}"#; "wrong_truncation_parameter")]
    fn rejects_invalid_schema(raw: &str) {
        assert!(serde_json::from_str::<SteeringConfig>(raw).is_err());
        assert!(
            serde_json::from_str::<RawConfig>(&format!("{{\"agent\":{{\"steering\":{raw}}}}}"))
                .is_err()
        );
    }

    #[test_case(json!({"steering": {"preset": "conservative"}}), "preset"; "global_conservative")]
    #[test_case(json!({"steering": {"preset": "enhanced"}}), "preset"; "global_enhanced")]
    #[test_case(json!({"steering": {"models": {(MODEL): {"preset": "conservative"}}}}), "preset"; "model_conservative")]
    #[test_case(json!({"steering": {"models": {(MODEL): {"preset": "enhanced"}}}}), "preset"; "model_enhanced")]
    #[test_case(json!({"max_continuation_turns": 3}), "max_continuation_turns"; "old_truncation_setting")]
    fn raw_config_rejects_removed_keys(agent: Value, key: &str) {
        let error = serde_json::from_value::<RawConfig>(json!({"agent": agent})).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&format!("{UNKNOWN_FIELD} `{key}`"))
        );
    }

    #[test_case(false; "global")]
    #[test_case(true; "model")]
    fn each_rule_can_be_disabled(model_override: bool) {
        for rule in RULES {
            let settings = json!({"rules": {(rule): {"enabled": false}}});
            let config = config(if model_override {
                json!({"models": {(MODEL): settings}})
            } else {
                settings
            });
            config.validate().unwrap();
            for model in [MODEL, OTHER_MODEL] {
                let rules = serde_json::to_value(config.resolve(model).rules).unwrap();
                for other in RULES {
                    assert_eq!(
                        rules[other]["enabled"],
                        other != rule || (model_override && model != MODEL)
                    );
                }
            }
        }
    }

    #[test_case("empty_response", "max_after_tools", 0; "empty_after_tools_zero")]
    #[test_case("empty_response", "max_idle", 0; "empty_idle_zero")]
    #[test_case("empty_response", "recent_tool_window", 0; "empty_window_zero")]
    #[test_case("repeated_tool_call", "threshold", 1; "repeat_threshold_one")]
    #[test_case("protocol_mismatch", "max_attempts", 0; "protocol_zero")]
    #[test_case("truncation", "max_attempts", 0; "truncation_zero")]
    #[test_case("truncation", "max_attempts", MAX_COUNT + 1; "truncation_too_large")]
    #[test_case("missing_task_report", "max_attempts", 0; "report_zero")]
    #[test_case("abandoned_turn", "max_attempts", 0; "abandoned_zero")]
    #[test_case("abandoned_turn", "max_attempts", MAX_COUNT + 1; "abandoned_too_large")]
    #[test_case("repetition", "window", MAX_WINDOW + 1; "oversized_window")]
    #[test_case("repetition", "max_cycle", 1; "single_cycle")]
    #[test_case("repetition", "cycle_repeats", 1; "single_cycle_repeat")]
    #[test_case("repetition", "text_repeats", 1; "single_text_repeat")]
    #[test_case("repetition", "cooldown", 0; "repetition_cooldown_zero")]
    #[test_case("tool_planning", "cooldown", 0; "planning_cooldown_zero")]
    #[test_case("tool_planning", "after_calls", MAX_COUNT + 1; "oversized_count")]
    #[test_case("tool_planning", "after_responses", 0; "planning_zero_responses")]
    fn rejects_invalid_rule_ranges(rule: &str, field: &str, value: usize) {
        for model_override in [false, true] {
            let invalid = json!({"enabled": false, "rules": {(rule): {(field): value}}});
            let config = config(if model_override {
                json!({"models": {(MODEL): invalid}})
            } else {
                invalid
            });
            let expected = if model_override {
                format!("models[{MODEL:?}].rules.{rule}.{field}")
            } else {
                format!("rules.{rule}.{field}")
            };
            assert!(
                matches!(config.validate(), Err(ConfigError::InvalidSteering {field, ..}) if field == expected)
            );
        }
    }

    #[test_case("repetition", "window", 11, false; "cycle_does_not_fit")]
    #[test_case("repetition", "window", 12, true; "cycle_fits")]
    #[test_case("repetition", "text_window", 2, false; "text_does_not_fit")]
    #[test_case("repetition", "text_window", 3, true; "text_fits")]
    #[test_case("tool_planning", "after_calls", 2, false; "responses_exceed_calls")]
    #[test_case("tool_planning", "after_calls", 3, true; "responses_equal_calls")]
    fn validates_combination_boundaries(rule: &str, field: &str, value: usize, valid: bool) {
        let config = config(json!({"rules": {(rule): {(field): value}}}));
        assert_eq!(config.validate().is_ok(), valid);
    }

    #[test_case(0, true; "zero")]
    #[test_case(MAX_COUNT, true; "maximum")]
    #[test_case(MAX_COUNT + 1, false; "too_large")]
    fn raw_conversion_validates_budget_bounds(value: usize, valid: bool) {
        for field in ["max_recoveries", "max_advisories"] {
            assert_eq!(config(json!({(field): value})).validate().is_ok(), valid);
            for model_override in [false, true] {
                let steering = if model_override {
                    json!({"models": {(MODEL): {(field): value}}})
                } else {
                    json!({(field): value})
                };
                let mut raw: RawConfig = serde_json::from_value(json!({"agent": {"steering": {
                    "models": {(MODEL): {"rules": {"tool_planning": {"enabled": false}}}}
                }}}))
                .unwrap();
                raw.merge(
                    serde_json::from_value(json!({"agent": {"steering": steering}})).unwrap(),
                );
                let result = raw.into_config(false);
                if valid {
                    let policy = result.unwrap().agent.steering.resolve(MODEL);
                    assert_eq!(serde_json::to_value(policy).unwrap()[field], json!(value));
                } else {
                    let expected = if model_override {
                        format!("models[{MODEL:?}].{field}")
                    } else {
                        field.to_owned()
                    };
                    assert!(
                        matches!(result, Err(ConfigError::InvalidSteering { field, .. }) if field == expected)
                    );
                }
            }
        }
    }

    #[test_case(0, false; "zero")]
    #[test_case(1, true; "minimum")]
    #[test_case(MAX_COUNT, true; "maximum")]
    #[test_case(MAX_COUNT + 1, false; "too_large")]
    fn raw_conversion_validates_truncation_bounds(value: usize, valid: bool) {
        for model_override in [false, true] {
            let settings = json!({"rules": {"truncation": {
                "enabled": false, "max_attempts": value
            }}});
            let steering = if model_override {
                json!({"models": {(MODEL): settings}})
            } else {
                settings
            };
            let raw: RawConfig =
                serde_json::from_value(json!({"agent": {"steering": steering}})).unwrap();
            let result = raw.into_config(false);
            if valid {
                let policy = result.unwrap().agent.steering.resolve(MODEL);
                assert_eq!(policy.rules.truncation.max_attempts as usize, value);
                assert!(!policy.rules.truncation.enabled);
            } else {
                let expected = if model_override {
                    format!("models[{MODEL:?}].rules.truncation.max_attempts")
                } else {
                    "rules.truncation.max_attempts".to_owned()
                };
                assert!(
                    matches!(result, Err(ConfigError::InvalidSteering { field, .. }) if field == expected)
                );
            }
        }
    }

    #[test_case(0, false; "zero")]
    #[test_case(2, false; "trigger_does_not_fit")]
    #[test_case(3, true; "trigger_fits")]
    #[test_case(MAX_WINDOW, true; "maximum")]
    #[test_case(MAX_WINDOW + 1, false; "too_large")]
    fn raw_conversion_validates_merged_windows(window: usize, valid: bool) {
        for model_override in [false, true] {
            let mut raw: RawConfig = serde_json::from_value(json!({"agent": {"steering": {
                "rules": {"repetition": {"text_repeats": 3}}
            }}}))
            .unwrap();
            let overlay = json!({"rules": {"repetition": {"text_window": window}}});
            let steering = if model_override {
                json!({"models": {(MODEL): overlay}})
            } else {
                overlay
            };
            raw.merge(serde_json::from_value(json!({"agent": {"steering": steering}})).unwrap());
            let result = raw.into_config(false);
            if valid {
                let policy = result.unwrap().agent.steering.resolve(MODEL);
                assert_eq!(policy.rules.repetition.text_window, window);
                assert_eq!(policy.rules.repetition.text_repeats, 3);
            } else {
                let expected = if model_override {
                    format!("models[{MODEL:?}].{INVALID_FIELD}")
                } else {
                    INVALID_FIELD.to_owned()
                };
                assert!(
                    matches!(result, Err(ConfigError::InvalidSteering { field, .. }) if field == expected)
                );
            }
        }
    }

    #[test_case("max_output_bytes"; "output_bytes")]
    #[test_case("max_output_lines"; "output_lines")]
    fn raw_conversion_does_not_extend_unrelated_validation(field: &str) {
        let raw: RawConfig = serde_json::from_value(json!({"agent": {(field): 0}})).unwrap();
        let config = raw.into_config(false).unwrap();
        assert!(
            matches!(config.validate(), Err(ConfigError::BelowMinimum { field: actual, .. }) if actual == field)
        );
    }

    #[test_case("", false; "empty")]
    #[test_case("model", false; "no_provider")]
    #[test_case("/model", false; "empty_provider")]
    #[test_case("provider/", false; "empty_suffix")]
    #[test_case("provider//model", false; "empty_segment")]
    #[test_case("provider/model name", false; "whitespace")]
    #[test_case("provider/model\u{0000}", false; "control")]
    #[test_case("provider/*", false; "wildcard")]
    #[test_case("provider/model?", false; "single_wildcard")]
    #[test_case("provider/[model]", false; "character_class")]
    #[test_case("provider/{a,b}", false; "alternation")]
    #[test_case("provider/model\\*", false; "escape")]
    #[test_case("provider/owner/model:version", true; "nested_suffix")]
    fn validates_exact_ids(model: &str, valid: bool) {
        assert_eq!(
            config(json!({"models": {(model): {}}})).validate().is_ok(),
            valid
        );
    }

    #[test_case(MAX_MODEL_ID_BYTES, true; "maximum")]
    #[test_case(MAX_MODEL_ID_BYTES + 1, false; "too_large")]
    fn validates_id_length(length: usize, valid: bool) {
        let model = format!("p/{}", "m".repeat(length - 2));
        assert_eq!(
            config(json!({"models": {(model): {}}})).validate().is_ok(),
            valid
        );
    }

    #[test_case(MAX_MODELS, true; "maximum")]
    #[test_case(MAX_MODELS + 1, false; "too_large")]
    fn validates_map_size(count: usize, valid: bool) {
        let config = SteeringConfig {
            models: (0..count)
                .map(|index| (format!("provider/{index}"), SteeringModelConfig::default()))
                .collect(),
            ..SteeringConfig::default()
        };
        assert_eq!(config.validate().is_ok(), valid);
    }

    #[test_case("", false; "empty")]
    #[test_case(" \n\t", false; "blank")]
    #[test_case(PROMPT, true; "literal_text")]
    fn validates_prompt(prompt: &str, valid: bool) {
        check_prompts(prompt, valid);
    }

    #[test_case(MAX_PROMPT_BYTES, true; "maximum")]
    #[test_case(MAX_PROMPT_BYTES + 1, false; "too_large")]
    fn validates_prompt_bytes(bytes: usize, valid: bool) {
        check_prompts(&"x".repeat(bytes), valid);
    }

    fn check_prompts(prompt: &str, valid: bool) {
        for rule in RULES {
            for model_override in [false, true] {
                let settings = json!({"rules": {(rule): {"enabled": false, "prompt": prompt}}});
                let steering = if model_override {
                    json!({"models": {(MODEL): settings}})
                } else {
                    settings
                };
                let raw: RawConfig =
                    serde_json::from_value(json!({"agent": {"steering": steering}})).unwrap();
                let result = raw.into_config(false);
                if valid {
                    let rules =
                        serde_json::to_value(result.unwrap().agent.steering.resolve(MODEL).rules)
                            .unwrap();
                    assert_eq!(rules[rule]["prompt"], prompt);
                } else {
                    let expected = if model_override {
                        format!("models[{MODEL:?}].rules.{rule}.prompt")
                    } else {
                        format!("rules.{rule}.prompt")
                    };
                    assert!(
                        matches!(result, Err(ConfigError::InvalidSteering { field, .. }) if field == expected)
                    );
                }
            }
        }
    }

    #[test_case(false; "global")]
    #[test_case(true; "resolved_model")]
    fn config_validation_checks_inherited_combinations(model_override: bool) {
        let steering = if model_override {
            json!({"rules": {"repetition": {"text_repeats": 3}}, "models": {
                (MODEL): {"rules": {"repetition": {"text_window": 2}}}
            }})
        } else {
            json!({"rules": {"repetition": {"text_window": 2}}})
        };
        let mut config = RawConfig::default().into_config(false).unwrap();
        config.agent.steering = self::config(steering).into();
        let expected_field = if model_override {
            format!("models[{MODEL:?}].{INVALID_FIELD}")
        } else {
            INVALID_FIELD.to_owned()
        };
        let error = config.validate().unwrap_err();
        match error {
            ConfigError::InvalidSteering { field, message } => {
                assert_eq!(field, expected_field);
                assert_eq!(message, INVALID_MESSAGE);
            }
            error => panic!("unexpected config error: {error}"),
        }
    }
}
