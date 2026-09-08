use caudra_config::AgentConfig;
use caudra_providers::Model;

use crate::tools::{
    ToolFilter, VIEW_IMAGE_TOOL_NAME, capability_exclusions, credential_exclusions, deferral,
};

pub const REASON_DISALLOWED_FLAG: &str = "--disallowed-tools";
pub const REASON_CONFIG: &str = "disabled by config";
pub const REASON_NO_VISION: &str = "model has no vision support";
pub const REASON_NOT_ALLOWED: &str = "not in --allowed-tools";
pub const REASON_COMPANION: &str = "always on (internal companion)";
pub const REASON_NO_SUBSCRIPTION: &str = "no ChatGPT subscription";
pub const REASON_OTHER_EDITOR: &str = "model uses the other editing tool";
pub const REASON_DEFERRED: &str = "deferred behind tool_search";
/// `tool_search` has no registry entry. The request array grows one whenever
/// something is deferred, so every listing derives the row from that rather
/// than looking it up.
pub const REASON_CATALOG: &str = "loads the lazy tools on request";
pub const CATALOG_SOURCE: &str = "native:caudra";

/// What a tool is doing in this run. `Lazy` is enabled and absent from the
/// request array at once, which neither `On` nor `Off` can express.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolState {
    On,
    Lazy,
    Off,
}

impl ToolState {
    pub fn label(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Lazy => "lazy",
            Self::Off => "off",
        }
    }

    pub fn reaches_model(self) -> bool {
        !matches!(self, Self::Off)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolReport {
    pub state: ToolState,
    pub reason: Option<&'static str>,
}

/// Why a built-in would not reach the model, in the order a user would ask:
/// what they typed, then what their config says, then what the model and the
/// allow list leave behind. A tool that is on only earns a reason when
/// something asked for it to be off and did not get it.
///
/// `cli_disallowed` is separate from `config.disabled_tools` only so the
/// report can name the flag the user just typed. A caller that has already
/// merged the two passes an empty slice and reads `disabled by config`.
pub fn builtin_report(
    name: &str,
    filter: &ToolFilter,
    cli_disallowed: &[String],
    config: &AgentConfig,
    model: &Model,
) -> ToolReport {
    let named_off = cli_disallowed.iter().any(|tool| tool == name);
    let config_off = config.disabled_tools.iter().any(|tool| tool == name);

    if !filter.matches(name) {
        let reason = if named_off {
            REASON_DISALLOWED_FLAG
        } else if config_off {
            REASON_CONFIG
        } else if capability_exclusions(model).contains(&name) {
            match name {
                VIEW_IMAGE_TOOL_NAME => REASON_NO_VISION,
                _ => REASON_OTHER_EDITOR,
            }
        } else if credential_exclusions().contains(&name) {
            REASON_NO_SUBSCRIPTION
        } else if config.allowed_tools.is_empty() {
            return ToolReport {
                state: ToolState::Off,
                reason: None,
            };
        } else {
            REASON_NOT_ALLOWED
        };
        return ToolReport {
            state: ToolState::Off,
            reason: Some(reason),
        };
    }

    if named_off || config_off {
        return ToolReport {
            state: ToolState::On,
            reason: Some(REASON_COMPANION),
        };
    }
    if deferral::is_deferred(name, &config.allowed_tools) {
        return ToolReport {
            state: ToolState::Lazy,
            reason: Some(REASON_DEFERRED),
        };
    }
    ToolReport {
        state: ToolState::On,
        reason: None,
    }
}

#[cfg(test)]
mod tests {
    use caudra_config::INTERNAL_COMPANION_TOOL_NAMES;
    use test_case::test_case;

    use super::{
        AgentConfig, Model, REASON_COMPANION, REASON_CONFIG, REASON_DEFERRED,
        REASON_DISALLOWED_FLAG, REASON_NOT_ALLOWED, REASON_OTHER_EDITOR, ToolReport, ToolState,
        builtin_report,
    };
    use crate::tools::{
        FILE_APPLY_PATCH_TOOL_NAME, FILE_READ_TOOL_NAME, SHELL_TOOL_NAME, ToolFilter,
    };

    const MODEL_SPEC: &str = "anthropic/claude-opus-4-8";
    const DEFERRED_TOOL: &str = "code_map";

    fn config(disabled: &[&str], allowed: &[&str]) -> AgentConfig {
        AgentConfig {
            disabled_tools: disabled.iter().map(|tool| (*tool).to_string()).collect(),
            allowed_tools: allowed.iter().map(|tool| (*tool).to_string()).collect(),
            ..AgentConfig::default()
        }
    }

    fn report(name: &str, cli: &[&str], config: &AgentConfig) -> ToolReport {
        let model = Model::from_spec(MODEL_SPEC).unwrap();
        let cli: Vec<String> = cli.iter().map(|tool| (*tool).to_string()).collect();
        let filter = ToolFilter::from_config(config, &model, &[]);
        builtin_report(name, &filter, &cli, config, &model)
    }

    #[test_case(SHELL_TOOL_NAME, &[], &["shell"], &[], ToolState::Off, Some(REASON_CONFIG) ; "config_disabled")]
    #[test_case(SHELL_TOOL_NAME, &["shell"], &["shell"], &[], ToolState::Off, Some(REASON_DISALLOWED_FLAG) ; "flag_wins_over_config")]
    #[test_case(SHELL_TOOL_NAME, &[], &[], &["file_read"], ToolState::Off, Some(REASON_NOT_ALLOWED) ; "outside_the_allow_list")]
    #[test_case(SHELL_TOOL_NAME, &[], &[], &[], ToolState::On, None ; "ordinary_tool")]
    #[test_case(DEFERRED_TOOL, &[], &[], &[], ToolState::Lazy, Some(REASON_DEFERRED) ; "deferred_tool")]
    #[test_case(DEFERRED_TOOL, &[], &[], &[DEFERRED_TOOL], ToolState::On, None ; "allow_listed_deferred_tool_loads_upfront")]
    #[test_case(FILE_APPLY_PATCH_TOOL_NAME, &[], &[], &[], ToolState::Off, Some(REASON_OTHER_EDITOR) ; "the_other_editor")]
    fn a_tool_reports_the_rule_that_decided_it(
        name: &str,
        cli: &[&str],
        disabled: &[&str],
        allowed: &[&str],
        state: ToolState,
        reason: Option<&'static str>,
    ) {
        let report = report(name, cli, &config(disabled, allowed));
        assert_eq!(report.state, state);
        assert_eq!(report.reason, reason);
    }

    /// Asking for a companion to turn off is not an error, but the report has
    /// to say the request did not take.
    #[test]
    fn a_companion_asked_to_turn_off_stays_on_with_a_reason() {
        for name in INTERNAL_COMPANION_TOOL_NAMES {
            let report = report(name, &[], &config(&[name], &[]));
            assert_eq!(report.state, ToolState::On);
            assert_eq!(report.reason, Some(REASON_COMPANION));
        }
    }

    #[test]
    fn a_lazy_tool_still_reaches_the_model() {
        assert!(ToolState::Lazy.reaches_model());
        assert!(!ToolState::Off.reaches_model());
        assert!(
            report(FILE_READ_TOOL_NAME, &[], &config(&[], &[]))
                .state
                .reaches_model()
        );
    }
}
