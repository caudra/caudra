use caudra_config::{AgentConfig, CODE_HOSTED_TOOLS, ProfileToolExposure, ProfileToolPolicy};
use caudra_providers::Model;

use crate::tools::profile_policy::{
    CEILING_DISABLED, LEGACY_LOADING, PROFILE_DISABLED, registered_decision, source_kind,
};
use crate::tools::{
    DescriptionContext, RegisteredTool, ToolFilter, VIEW_IMAGE_TOOL_NAME, capability_exclusions,
    credential_exclusions,
};

pub const REASON_DISALLOWED_FLAG: &str = "--disallowed-tools";
pub const REASON_CONFIG: &str = "disabled by config";
pub const REASON_NO_VISION: &str = "model has no vision support";
pub const REASON_NOT_ALLOWED: &str = "not in --allowed-tools";
pub const REASON_COMPANION: &str = "always on (internal companion)";
pub const REASON_NO_SUBSCRIPTION: &str = "no ChatGPT subscription";
pub const REASON_OTHER_EDITOR: &str = "model uses the other editing tool";
pub const REASON_CODE_MODULE: &str = "reached through a `code` module";

/// What a tool is doing in this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolState {
    On,
    Off,
}

impl ToolState {
    pub fn label(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
        }
    }

    pub fn reaches_model(self) -> bool {
        self == Self::On
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolReport {
    pub state: ToolState,
    pub reason: Option<&'static str>,
}

pub fn profile_report(
    entry: &RegisteredTool,
    profile: &ProfileToolPolicy,
    legacy: ToolReport,
    ctx: &DescriptionContext,
    session_plan: bool,
) -> ToolReport {
    let decision = registered_decision(entry, ctx, profile, session_plan);
    if !decision.available() {
        return ToolReport {
            state: ToolState::Off,
            reason: Some(if decision.reason == CEILING_DISABLED {
                legacy.reason.unwrap_or_else(|| {
                    if profile.exposure(entry.name(), source_kind(&entry.source))
                        == Some(ProfileToolExposure::Disabled)
                    {
                        PROFILE_DISABLED
                    } else {
                        CEILING_DISABLED
                    }
                })
            } else {
                decision.reason
            }),
        };
    }
    if decision.reason == LEGACY_LOADING {
        return legacy;
    }
    ToolReport {
        state: match decision.exposure {
            ProfileToolExposure::Eager => ToolState::On,
            ProfileToolExposure::Disabled => ToolState::Off,
        },
        reason: Some(decision.reason),
    }
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
    ToolReport {
        state: ToolState::On,
        reason: CODE_HOSTED_TOOLS
            .contains(&name)
            .then_some(REASON_CODE_MODULE),
    }
}

#[cfg(test)]
mod tests {
    use caudra_config::{INTERNAL_COMPANION_TOOL_NAMES, ProfileToolDefault, ProfileToolPolicy};
    use std::sync::Arc;
    use test_case::test_case;

    use super::{
        AgentConfig, Model, REASON_CODE_MODULE, REASON_CONFIG, REASON_DISALLOWED_FLAG,
        REASON_NOT_ALLOWED, REASON_OTHER_EDITOR, ToolReport, ToolState, builtin_report,
        profile_report,
    };
    use crate::AgentMode;
    use crate::tools::profile_policy::{
        MODE_DISABLED, PLAN_REQUIRED, PLAN_TOOL_NAME, PROFILE_LOADING, REQUIRED_INFRASTRUCTURE,
    };
    use crate::tools::{
        DescriptionContext, FILE_APPLY_PATCH_TOOL_NAME, RegisteredTool, SHELL_TOOL_NAME,
        ToolAudience, ToolEffect, ToolFilter, ToolSource, test_support::NamedMock,
    };

    const MODEL_SPEC: &str = "anthropic/claude-opus-4-8";
    const HOSTED_TOOL: &str = "execution_environment";
    const PLAN_PATH: &str = "plan.md";
    const CUSTOM_SOURCE: &str = "custom_plan";

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
    #[test_case(HOSTED_TOOL, &[], &[], &[], ToolState::On, Some(REASON_CODE_MODULE) ; "code_hosted_tool")]
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
            let entry = RegisteredTool {
                tool: Arc::new(NamedMock::new(name, ToolAudience::all())),
                source: NamedMock::source(),
                effect: ToolEffect::ReadOnly,
            };
            let report = profile_report(
                &entry,
                &ProfileToolPolicy::default(),
                report(name, &[], &config(&[name], &[])),
                &DescriptionContext {
                    filter: &ToolFilter::All,
                    audience: ToolAudience::MAIN,
                    workflows_available: false,
                },
                false,
            );
            assert_eq!(report.state, ToolState::On);
            assert_eq!(report.reason, Some(REQUIRED_INFRASTRUCTURE));
        }
    }

    /// `bound` is the session binding a main agent carries or a task inherits.
    #[test_case(true, AgentMode::Plan(PLAN_PATH.into()), false, ToolAudience::MAIN, ToolState::On, PROFILE_LOADING; "native_main_plan")]
    #[test_case(true, AgentMode::Build, true, ToolAudience::MAIN, ToolState::On, PROFILE_LOADING; "native_main_build_with_binding")]
    #[test_case(true, AgentMode::Build, false, ToolAudience::MAIN, ToolState::Off, PLAN_REQUIRED; "native_main_build_without_binding")]
    #[test_case(true, AgentMode::ReadOnly, true, ToolAudience::RESEARCH_SUB, ToolState::On, PROFILE_LOADING; "native_task_with_inherited_plan")]
    #[test_case(true, AgentMode::Build, false, ToolAudience::GENERAL_SUB, ToolState::Off, PLAN_REQUIRED; "native_task_without_plan")]
    #[test_case(false, AgentMode::Build, false, ToolAudience::MAIN, ToolState::On, PROFILE_LOADING; "custom_build")]
    #[test_case(false, AgentMode::ReadOnly, false, ToolAudience::MAIN, ToolState::Off, MODE_DISABLED; "custom_read_only")]
    #[test_case(false, AgentMode::Build, false, ToolAudience::GENERAL_SUB, ToolState::On, PROFILE_LOADING; "custom_task")]
    fn plan_reason_belongs_to_the_native_source(
        native: bool,
        mode: AgentMode,
        bound: bool,
        audience: ToolAudience,
        state: ToolState,
        reason: &'static str,
    ) {
        let entry = RegisteredTool {
            tool: Arc::new(NamedMock::new(PLAN_TOOL_NAME, ToolAudience::all())),
            source: if native {
                NamedMock::source()
            } else {
                ToolSource::Lua {
                    plugin: CUSTOM_SOURCE.into(),
                    contract: CUSTOM_SOURCE.into(),
                    bundled: false,
                }
            },
            effect: ToolEffect::ReadOnly,
        };
        let policy = ProfileToolPolicy {
            default: ProfileToolDefault::Eager,
            ..ProfileToolPolicy::default()
        };
        let filter = ToolFilter::All.for_mode(&mode);
        let report = profile_report(
            &entry,
            &policy,
            ToolReport {
                state: ToolState::On,
                reason: None,
            },
            &DescriptionContext {
                filter: &filter,
                audience,
                workflows_available: false,
            },
            mode.is_planning() || bound,
        );
        assert_eq!(
            report,
            ToolReport {
                state,
                reason: Some(reason),
            }
        );
    }
}
