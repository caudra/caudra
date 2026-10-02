use caudra_config::{AgentConfig, ProfileToolExposure, ProfileToolPolicy};
use caudra_providers::Model;

use crate::AgentMode;
use crate::tools::profile_policy::{
    CEILING_DISABLED, LEGACY_LOADING, PROFILE_DISABLED, registered_decision, source_kind,
};
use crate::tools::{
    BuiltinDeferral, DescriptionContext, RegisteredTool, ToolFilter, VIEW_IMAGE_TOOL_NAME,
    capability_exclusions, credential_exclusions, deferral,
};

pub const REASON_DISALLOWED_FLAG: &str = "--disallowed-tools";
pub const REASON_CONFIG: &str = "disabled by config";
pub const REASON_NO_VISION: &str = "model has no vision support";
pub const REASON_NOT_ALLOWED: &str = "not in --allowed-tools";
pub const REASON_COMPANION: &str = "always on (internal companion)";
pub const REASON_NO_SUBSCRIPTION: &str = "no ChatGPT subscription";
pub const REASON_OTHER_EDITOR: &str = "model uses the other editing tool";
pub const REASON_DEFERRED: &str = "deferred behind tool_search";
pub const REASON_EAGER_CLASS: &str = "declared upfront on a known non-small model";
pub const REASON_EAGER_CONFIG: &str = "lazy loading disabled by config";
/// `tool_search` has no registry entry. The request array grows one whenever
/// something is deferred, so every listing derives the row from that rather
/// than looking it up.
pub const REASON_CATALOG: &str = "loads the lazy tools on request";
pub const CATALOG_SOURCE: &str = "native:caudra";
pub const REASON_PROFILE_LOADED: &str = "profile lazy tool loaded in this session";

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

pub fn profile_report(
    entry: &RegisteredTool,
    profile: &ProfileToolPolicy,
    legacy: ToolReport,
    ctx: &DescriptionContext,
    mode: &AgentMode,
) -> ToolReport {
    let decision = registered_decision(entry, ctx, profile, mode, legacy.state == ToolState::Lazy);
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
            ProfileToolExposure::Lazy => ToolState::Lazy,
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
///
/// `deferral` is passed rather than resolved, because this runs once per
/// registry entry and resolving it parses `providers.toml`.
pub fn builtin_report(
    name: &str,
    filter: &ToolFilter,
    cli_disallowed: &[String],
    config: &AgentConfig,
    model: &Model,
    deferral: BuiltinDeferral,
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
    if deferral::is_deferred(name, &config.allowed_tools, deferral) {
        return ToolReport {
            state: ToolState::Lazy,
            reason: Some(REASON_DEFERRED),
        };
    }
    ToolReport {
        state: ToolState::On,
        reason: eager_reason(name, deferral),
    }
}

/// Why a tool that would normally be lazy is in the array anyway. Nothing for
/// a tool that was never deferrable, and nothing when an allow list asked for
/// it upfront, which the caller already got what it wanted from.
fn eager_reason(name: &str, deferral: BuiltinDeferral) -> Option<&'static str> {
    if !caudra_config::is_deferred_builtin(name) {
        return None;
    }
    match deferral {
        BuiltinDeferral::Lazy => None,
        BuiltinDeferral::EagerByClass => Some(REASON_EAGER_CLASS),
        BuiltinDeferral::EagerByConfig => Some(REASON_EAGER_CONFIG),
    }
}

#[cfg(test)]
mod tests {
    use caudra_config::{INTERNAL_COMPANION_TOOL_NAMES, ProfileToolDefault, ProfileToolPolicy};
    use std::sync::Arc;
    use test_case::test_case;

    use super::{
        AgentConfig, BuiltinDeferral, Model, REASON_CONFIG, REASON_DEFERRED,
        REASON_DISALLOWED_FLAG, REASON_EAGER_CLASS, REASON_EAGER_CONFIG, REASON_NOT_ALLOWED,
        REASON_OTHER_EDITOR, ToolReport, ToolState, builtin_report, profile_report,
    };
    use crate::AgentMode;
    use crate::tools::profile_policy::{
        MODE_DISABLED, PLAN_MODE_REQUIRED, PLAN_TOOL_NAME, PROFILE_LOADING, REQUIRED_INFRASTRUCTURE,
    };
    use crate::tools::{
        DescriptionContext, FILE_APPLY_PATCH_TOOL_NAME, FILE_READ_TOOL_NAME, RegisteredTool,
        SHELL_TOOL_NAME, ToolAudience, ToolEffect, ToolFilter, ToolSource, test_support::NamedMock,
    };

    const MODEL_SPEC: &str = "anthropic/claude-opus-4-8";
    const DEFERRED_TOOL: &str = "code_map";
    const PLAN_PATH: &str = "plan.md";
    const CUSTOM_SOURCE: &str = "custom_plan";

    fn config(disabled: &[&str], allowed: &[&str]) -> AgentConfig {
        AgentConfig {
            disabled_tools: disabled.iter().map(|tool| (*tool).to_string()).collect(),
            allowed_tools: allowed.iter().map(|tool| (*tool).to_string()).collect(),
            ..AgentConfig::default()
        }
    }

    /// The deferral is handed in rather than resolved from `MODEL_SPEC`, so
    /// these cases test the ordering of the rules and not the class lookup,
    /// which owns its own tests and reads `providers.toml`.
    fn report(
        name: &str,
        cli: &[&str],
        config: &AgentConfig,
        deferral: BuiltinDeferral,
    ) -> ToolReport {
        let model = Model::from_spec(MODEL_SPEC).unwrap();
        let cli: Vec<String> = cli.iter().map(|tool| (*tool).to_string()).collect();
        let filter = ToolFilter::from_config(config, &model, &[]);
        builtin_report(name, &filter, &cli, config, &model, deferral)
    }

    fn lazy_report(name: &str, cli: &[&str], config: &AgentConfig) -> ToolReport {
        report(name, cli, config, BuiltinDeferral::Lazy)
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
        let report = lazy_report(name, cli, &config(disabled, allowed));
        assert_eq!(report.state, state);
        assert_eq!(report.reason, reason);
    }

    /// A run that defers nothing still has to say why a tool the docs call
    /// lazy is sitting in the array.
    #[test_case(BuiltinDeferral::EagerByClass, REASON_EAGER_CLASS ; "the_model_did_not_need_the_help")]
    #[test_case(BuiltinDeferral::EagerByConfig, REASON_EAGER_CONFIG ; "the_user_turned_it_off")]
    fn an_eager_run_names_what_declared_the_lazy_tools(
        deferral: BuiltinDeferral,
        reason: &'static str,
    ) {
        let report = report(DEFERRED_TOOL, &[], &config(&[], &[]), deferral);
        assert_eq!(report.state, ToolState::On);
        assert_eq!(report.reason, Some(reason));
    }

    /// The reason belongs to the tools deferral would have withheld. A tool
    /// that was never lazy has nothing to explain.
    #[test]
    fn an_ordinary_tool_gains_no_reason_from_an_eager_run() {
        let report = report(
            SHELL_TOOL_NAME,
            &[],
            &config(&[], &[]),
            BuiltinDeferral::EagerByClass,
        );
        assert_eq!(report.state, ToolState::On);
        assert_eq!(report.reason, None);
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
                lazy_report(name, &[], &config(&[name], &[])),
                &DescriptionContext {
                    filter: &ToolFilter::All,
                    audience: ToolAudience::MAIN,
                    workflows_available: false,
                },
                &crate::AgentMode::Build,
            );
            assert_eq!(report.state, ToolState::On);
            assert_eq!(report.reason, Some(REQUIRED_INFRASTRUCTURE));
        }
    }

    #[test_case(true, AgentMode::Build, ToolAudience::MAIN, ToolState::Off, PLAN_MODE_REQUIRED; "native_build")]
    #[test_case(true, AgentMode::ReadOnly, ToolAudience::MAIN, ToolState::Off, PLAN_MODE_REQUIRED; "native_read_only")]
    #[test_case(true, AgentMode::Plan(PLAN_PATH.into()), ToolAudience::GENERAL_SUB, ToolState::Off, PLAN_MODE_REQUIRED; "native_task")]
    #[test_case(true, AgentMode::Plan(PLAN_PATH.into()), ToolAudience::MAIN, ToolState::On, PROFILE_LOADING; "native_main_plan")]
    #[test_case(false, AgentMode::Build, ToolAudience::MAIN, ToolState::On, PROFILE_LOADING; "custom_build")]
    #[test_case(false, AgentMode::ReadOnly, ToolAudience::MAIN, ToolState::Off, MODE_DISABLED; "custom_read_only")]
    #[test_case(false, AgentMode::Plan(PLAN_PATH.into()), ToolAudience::GENERAL_SUB, ToolState::On, PROFILE_LOADING; "custom_task")]
    fn plan_mode_reason_belongs_to_the_native_source(
        native: bool,
        mode: AgentMode,
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
            &mode,
        );
        assert_eq!(
            report,
            ToolReport {
                state,
                reason: Some(reason),
            }
        );
    }

    #[test]
    fn a_lazy_tool_still_reaches_the_model() {
        assert!(ToolState::Lazy.reaches_model());
        assert!(!ToolState::Off.reaches_model());
        assert!(
            lazy_report(FILE_READ_TOOL_NAME, &[], &config(&[], &[]))
                .state
                .reaches_model()
        );
    }
}
