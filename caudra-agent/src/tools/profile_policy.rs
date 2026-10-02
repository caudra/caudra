use caudra_config::{
    ProfileToolExposure, ProfileToolPolicy, ProfileToolSource, effective_shell_execution,
    effective_task_execution,
};
use serde_json::Value;
use std::mem;

use crate::AgentMode;
use crate::mcp::McpSession;

use super::{
    DescriptionContext, LocalToolEntry, LocalTools, RegisteredTool, TOOL_OUTPUT_TOOL_NAME,
    ToolAudience, ToolContext, ToolDefinitions, ToolFilter, ToolRegistry, ToolSource,
    deferral::DeferredTool,
};

pub const PROFILE_DISABLED: &str = "disabled by the selected profile";
pub const CEILING_DISABLED: &str = "excluded by the availability ceiling";
pub const MODE_DISABLED: &str = "unavailable in the executing mode or audience";
pub const REQUIRED_INFRASTRUCTURE: &str = "required host infrastructure";
pub const PROFILE_LOADING: &str = "selected profile loading policy";
pub const LEGACY_LOADING: &str = "legacy loading policy";
pub const PLAN_TOOL_NAME: &str = "plan";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExposureDecision {
    pub exposure: ProfileToolExposure,
    pub reason: &'static str,
}

impl ToolExposureDecision {
    pub fn available(&self) -> bool {
        self.exposure != ProfileToolExposure::Disabled
    }
}

pub fn source_kind(source: &ToolSource) -> ProfileToolSource {
    match source {
        ToolSource::Native { trusted: true, .. } | ToolSource::RemoteWorkcell { .. } => {
            ProfileToolSource::Native
        }
        ToolSource::Mcp { .. } => ProfileToolSource::Mcp,
        _ => ProfileToolSource::Custom,
    }
}

pub fn registered_decision(
    entry: &RegisteredTool,
    ctx: &DescriptionContext,
    profile: &ProfileToolPolicy,
    mode: &AgentMode,
    legacy_lazy: bool,
) -> ToolExposureDecision {
    let source = source_kind(&entry.source);
    if !entry.tool.audience().contains(ctx.audience)
        || (ctx.policy().is_read_only() && !entry.is_visible_in_read_only())
        || (source == ProfileToolSource::Native
            && entry.name() == PLAN_TOOL_NAME
            && (!mode.is_planning() || ctx.audience != ToolAudience::MAIN))
    {
        return disabled(MODE_DISABLED);
    }
    if source == ProfileToolSource::Native && entry.name() == TOOL_OUTPUT_TOOL_NAME {
        return ToolExposureDecision {
            exposure: ProfileToolExposure::Eager,
            reason: REQUIRED_INFRASTRUCTURE,
        };
    }
    if !ctx.filter.matches(entry.name()) {
        return disabled(CEILING_DISABLED);
    }
    exposure_decision(profile, entry.name(), source, legacy_lazy)
}

pub fn local_decision(
    name: &str,
    entry: &LocalToolEntry,
    filter: &ToolFilter,
    profile: &ProfileToolPolicy,
) -> ToolExposureDecision {
    if filter.is_read_only() && !entry.effect.is_safe_in_read_only() {
        return disabled(MODE_DISABLED);
    }
    if entry.is_required() {
        return ToolExposureDecision {
            exposure: ProfileToolExposure::Eager,
            reason: REQUIRED_INFRASTRUCTURE,
        };
    }
    if !filter.matches(name) {
        return disabled(CEILING_DISABLED);
    }
    exposure_decision(profile, name, ProfileToolSource::Custom, false)
}

fn exposure_decision(
    profile: &ProfileToolPolicy,
    name: &str,
    source: ProfileToolSource,
    legacy_lazy: bool,
) -> ToolExposureDecision {
    match profile.exposure(name, source) {
        Some(ProfileToolExposure::Disabled) => disabled(PROFILE_DISABLED),
        Some(exposure) => ToolExposureDecision {
            exposure,
            reason: PROFILE_LOADING,
        },
        None => ToolExposureDecision {
            exposure: if legacy_lazy {
                ProfileToolExposure::Lazy
            } else {
                ProfileToolExposure::Eager
            },
            reason: LEGACY_LOADING,
        },
    }
}

fn disabled(reason: &'static str) -> ToolExposureDecision {
    ToolExposureDecision {
        exposure: ProfileToolExposure::Disabled,
        reason,
    }
}

pub fn append_local_definitions(
    definitions: &mut ToolDefinitions,
    locals: impl IntoIterator<Item = Value>,
    bindings: &LocalTools,
    filter: &ToolFilter,
    profile: &ProfileToolPolicy,
) {
    for definition in locals {
        let Some(name) = definition["name"].as_str() else {
            continue;
        };
        let Some(entry) = bindings.get(name) else {
            continue;
        };
        match local_decision(name, entry, filter, profile).exposure {
            ProfileToolExposure::Disabled => {}
            ProfileToolExposure::Eager => {
                if let Some(declared) = definitions.declared.as_array_mut() {
                    declared.push(definition);
                }
            }
            ProfileToolExposure::Lazy => {
                definitions
                    .deferred
                    .push(DeferredTool::new(name, None, definition.clone()))
            }
        }
    }
}

pub fn configure_definitions(definitions: &mut ToolDefinitions, ctx: &ToolContext) {
    let filter = ctx
        .tool_filter
        .clone()
        .intersect(&ctx.tool_ceiling)
        .for_mode(&ctx.mode);
    let declared = definitions
        .declared
        .take()
        .as_array()
        .cloned()
        .unwrap_or_default();
    let deferred = mem::take(&mut definitions.deferred);
    let mut eager = Vec::new();
    for (definition, was_lazy, group) in declared
        .into_iter()
        .map(|definition| (definition, false, None))
        .chain(
            deferred
                .into_iter()
                .map(|tool| (tool.definition, true, tool.group)),
        )
    {
        let Some(name) = definition["name"].as_str() else {
            continue;
        };
        let decision = if let Some(local) = ctx.local_tools.get(name) {
            local_decision(name, local, &filter, &ctx.profile_tool_policy)
        } else if let Some(entry) = ctx.registry.get(name) {
            if !ctx.tool_available(name) {
                continue;
            }
            registered_decision(
                &entry,
                &DescriptionContext {
                    filter: &filter,
                    audience: ctx.audience,
                    workflows_available: ctx.workflow.is_some(),
                },
                &ctx.profile_tool_policy,
                &ctx.mode,
                was_lazy,
            )
        } else {
            exposure_decision(
                &ctx.profile_tool_policy,
                name,
                ProfileToolSource::Custom,
                was_lazy,
            )
        };
        match decision.exposure {
            ProfileToolExposure::Disabled => {}
            ProfileToolExposure::Eager => eager.push(definition),
            ProfileToolExposure::Lazy => {
                definitions
                    .deferred
                    .push(DeferredTool::new(name, group, definition.clone()))
            }
        }
    }
    definitions.declared = Value::Array(eager);
    super::execution::configure_tools(
        &mut definitions.declared,
        &mut definitions.deferred,
        &ctx.config,
        ctx.background.is_some(),
        ctx.job_scope().is_some(),
    );
}

impl ToolContext {
    pub fn tool_available(&self, name: &str) -> bool {
        tool_available(&self.registry, self.mcp.as_ref(), self, name)
    }
}

pub fn tool_available(
    registry: &ToolRegistry,
    mcp: Option<&McpSession>,
    ctx: &ToolContext,
    name: &str,
) -> bool {
    let name = ctx.resolve_tool_name_alias(name);
    let filter = ctx
        .tool_filter
        .clone()
        .intersect(&ctx.tool_ceiling)
        .for_mode(&ctx.mode);
    if let Some(local) = ctx.local_tools.get(name) {
        return local_decision(name, local, &filter, &ctx.profile_tool_policy).available();
    }
    if let Some(entry) = registry.get(name) {
        if super::feature_exclusions(ctx.config.features).contains(&name)
            || (name == super::TASK_TOOL_NAME
                && effective_task_execution(&ctx.config, ctx.background.is_some()).is_none())
            || (name == super::SHELL_TOOL_NAME
                && effective_shell_execution(&ctx.config, ctx.job_scope().is_some()).is_none())
        {
            return false;
        }
        return registered_decision(
            &entry,
            &DescriptionContext {
                filter: &filter,
                audience: ctx.audience,
                workflows_available: ctx.workflow.is_some(),
            },
            &ctx.profile_tool_policy,
            &ctx.mode,
            false,
        )
        .available();
    }
    let canonical = crate::mcp::internal_tool_name(name);
    mcp.is_some_and(|mcp| {
        mcp.has_tool(&canonical)
            && !mcp.is_disabled(&canonical)
            && !ctx.policy().is_read_only()
            && !ctx.mode.is_planning()
            && ctx
                .profile_tool_policy
                .exposure(&canonical, ProfileToolSource::Mcp)
                != Some(ProfileToolExposure::Disabled)
    })
}

#[cfg(test)]
mod tests {
    use crate::tools::{
        DeferralSession, DescriptionContext, TOOL_SEARCH_TOOL_NAME, ToolAudience, ToolDefinitions,
        ToolEffect, ToolFilter, ToolRegistry, ToolSource, audited_local_tool,
        registry::{ParseError, Tool, ToolInvocation},
        test_support::{NamedMock, stub_ctx},
    };
    use crate::{
        AgentMode,
        agent::tool_dispatch::{self, Emit},
        template::Vars,
    };
    use caudra_config::{ProfileToolExposure, ProfileToolPolicy};
    use caudra_storage::tool_ledger::ToolOutcome;
    use serde_json::{Value, json};
    use std::{
        borrow::Cow,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use test_case::test_case;

    const CUSTOM: &str = "custom_reader";
    const READ: &str = "file_read";
    const WRITE: &str = "file_write";
    const PAGER: &str = "tool_output";
    const CALL: &str = "profile-call";
    const OK: &str = "completed";
    const PLAN_PATH: &str = "plan.md";

    struct CountedTool(Arc<AtomicUsize>);

    impl Tool for CountedTool {
        fn name(&self) -> &str {
            CUSTOM
        }
        fn description(&self, _: &DescriptionContext) -> Cow<'_, str> {
            CUSTOM.into()
        }
        fn schema(&self) -> Value {
            json!({"type":"object"})
        }
        fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            NamedMock::new(CUSTOM, ToolAudience::all()).parse(input)
        }
    }

    fn profile(value: Value) -> Arc<ProfileToolPolicy> {
        Arc::new(serde_json::from_value(value).unwrap())
    }

    fn registry() -> Arc<ToolRegistry> {
        let registry = Arc::new(ToolRegistry::new());
        for (name, effect) in [
            (CUSTOM, ToolEffect::ReadOnly),
            (READ, ToolEffect::ReadOnly),
            (WRITE, ToolEffect::Mutating),
            (PAGER, ToolEffect::ReadOnly),
        ] {
            registry
                .register_audited(
                    Arc::new(NamedMock::new(name, ToolAudience::all())),
                    NamedMock::source(),
                    effect,
                )
                .unwrap();
        }
        registry
    }

    #[test_case(ProfileToolExposure::Eager, false; "explicit_eager")]
    #[test_case(ProfileToolExposure::Lazy, true; "explicit_lazy")]
    fn explicit_loading_overrides_legacy(exposure: ProfileToolExposure, lazy: bool) {
        let registry = registry();
        let mut policy = ProfileToolPolicy::default();
        policy.overrides.insert(READ.into(), exposure);
        let split = registry.definitions_split_with_policy(
            &Vars::new(),
            &DescriptionContext {
                filter: &ToolFilter::All,
                audience: ToolAudience::MAIN,
                workflows_available: false,
            },
            false,
            if lazy { &[] } else { &[READ] },
            &policy,
            &AgentMode::Build,
        );
        assert_eq!(
            split.deferred.iter().any(|tool| tool.name.as_ref() == READ),
            lazy
        );
    }

    #[test_case(AgentMode::Build, ToolAudience::MAIN, false; "build")]
    #[test_case(AgentMode::ReadOnly, ToolAudience::MAIN, false; "readonly")]
    #[test_case(AgentMode::Plan(PLAN_PATH.into()), ToolAudience::MAIN, true; "main_plan")]
    #[test_case(AgentMode::Plan(PLAN_PATH.into()), ToolAudience::GENERAL_SUB, false; "child_plan")]
    fn plan_exposure_requires_executing_main_target(
        mode: AgentMode,
        audience: ToolAudience,
        available: bool,
    ) {
        let mut ctx = stub_ctx(&mode);
        ctx.audience = audience;
        ctx.registry
            .register_audited(
                Arc::new(NamedMock::new(super::PLAN_TOOL_NAME, ToolAudience::all())),
                NamedMock::source(),
                ToolEffect::ReadOnly,
            )
            .unwrap();
        ctx.profile_tool_policy = profile(json!({"default":"eager"}));
        assert_eq!(ctx.tool_available(super::PLAN_TOOL_NAME), available);
    }

    #[test_case("workflow", false; "feature_off")]
    #[test_case("shell", false; "async_without_scope")]
    #[test_case("shell", true; "sync_without_scope")]
    fn profile_cannot_enable_absent_runtime(name: &'static str, sync: bool) {
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.registry
            .register_audited(
                Arc::new(NamedMock::new(name, ToolAudience::all())),
                NamedMock::source(),
                ToolEffect::ReadOnly,
            )
            .unwrap();
        ctx.profile_tool_policy = profile(json!({"default":"eager"}));
        if !sync {
            ctx.config.shell_execution = caudra_config::ExecutionMode::Async;
        }
        assert_eq!(ctx.tool_available(name), sync);
    }

    #[test_case(false; "native")]
    #[test_case(true; "plugin")]
    fn disabled_registered_binding_never_parses(plugin: bool) {
        smol::block_on(async {
            let mut ctx = stub_ctx(&AgentMode::Build);
            let calls = Arc::new(AtomicUsize::new(0));
            let source = if plugin {
                ToolSource::Lua {
                    plugin: CUSTOM.into(),
                    contract: CUSTOM.into(),
                    bundled: false,
                }
            } else {
                NamedMock::source()
            };
            ctx.registry
                .register_audited(
                    Arc::new(CountedTool(Arc::clone(&calls))),
                    source,
                    ToolEffect::ReadOnly,
                )
                .unwrap();
            ctx.profile_tool_policy = profile(json!({"default":"disabled"}));
            let done = tool_dispatch::run(
                &ctx.registry,
                None,
                CALL.into(),
                CUSTOM,
                &json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert_eq!(done.accounting.outcome, Some(ToolOutcome::Denied));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        });
    }

    #[test_case(false; "ordinary_shadow")]
    #[test_case(true; "host_required_sink")]
    fn only_host_bound_local_sinks_are_required(required: bool) {
        smol::block_on(async {
            let mut ctx = stub_ctx(&AgentMode::ReadOnly);
            let calls = Arc::new(AtomicUsize::new(0));
            let called = Arc::clone(&calls);
            let entry = audited_local_tool(ToolEffect::ReadOnly, move |_, _| {
                called.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(OK.into()) })
            });
            let entry = if required {
                entry.required_output()
            } else {
                entry
            };
            ctx.local_tools = Arc::new([(PAGER.into(), entry)].into());
            ctx.tool_filter = ToolFilter::Only(Vec::new());
            ctx.profile_tool_policy = profile(json!({"default":"disabled"}));
            let done = tool_dispatch::run(
                &ctx.registry,
                None,
                CALL.into(),
                PAGER,
                &json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert_eq!(done.is_error, !required);
            assert_eq!(calls.load(Ordering::SeqCst), usize::from(required));
        });
    }

    #[test_case(false; "local_schema_available")]
    #[test_case(true; "local_schema_ceiling")]
    fn local_definitions_obey_the_same_ceiling_as_dispatch(blocked: bool) {
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.local_tools = Arc::new(
            [(
                CUSTOM.into(),
                audited_local_tool(ToolEffect::ReadOnly, |_, _| {
                    Box::pin(async { Ok(OK.into()) })
                }),
            )]
            .into(),
        );
        ctx.profile_tool_policy = profile(json!({"default":"lazy"}));
        if blocked {
            ctx.tool_ceiling = ToolFilter::Only(Vec::new());
        }
        let mut definitions = ToolDefinitions {
            declared: json!([{"name":CUSTOM,"input_schema":{"type":"object"}}]),
            deferred: Vec::new(),
        };
        super::configure_definitions(&mut definitions, &ctx);
        assert_eq!(definitions.available_filter().matches(CUSTOM), !blocked);
        assert_eq!(ctx.tool_available(CUSTOM), !blocked);
        assert_eq!(definitions.deferred.len(), usize::from(!blocked));
    }

    #[test_case(false; "trusted_pager")]
    #[test_case(true; "plugin_pager_shadow")]
    fn infrastructure_exemption_uses_the_resolved_source(plugin: bool) {
        let mut ctx = stub_ctx(&AgentMode::Build);
        let source = if plugin {
            ToolSource::Lua {
                plugin: CUSTOM.into(),
                contract: CUSTOM.into(),
                bundled: false,
            }
        } else {
            NamedMock::source()
        };
        ctx.registry
            .register_audited(
                Arc::new(NamedMock::new(PAGER, ToolAudience::all())),
                source,
                ToolEffect::ReadOnly,
            )
            .unwrap();
        ctx.profile_tool_policy = profile(json!({"default":"disabled"}));
        ctx.tool_ceiling = ToolFilter::Only(Vec::new());
        assert_eq!(ctx.tool_available(PAGER), !plugin);
    }

    #[test]
    fn readonly_loader_discovers_only_safe_eligible_bindings() {
        smol::block_on(async {
            let mut ctx = stub_ctx(&AgentMode::ReadOnly);
            ctx.registry = registry();
            ctx.profile_tool_policy = profile(json!({"default":"lazy"}));
            ctx.tool_filter = ToolFilter::All.for_mode(&ctx.mode);
            let split = ctx.registry.definitions_split_with_policy(
                &Vars::new(),
                &DescriptionContext {
                    filter: &ctx.tool_filter,
                    audience: ToolAudience::RESEARCH_SUB,
                    workflows_available: false,
                },
                false,
                &[],
                &ctx.profile_tool_policy,
                &ctx.mode,
            );
            assert!(!split.available_filter().matches(WRITE));
            ctx.deferral = Some(DeferralSession::new(
                split.deferred,
                [Arc::from(WRITE)].into_iter(),
            ));
            ctx.mcp = Some(crate::mcp::stub_session(&[(
                "srv.observe",
                "Observe external data",
            )]));
            let found = tool_dispatch::run(
                &ctx.registry,
                ctx.mcp.as_ref(),
                CALL.into(),
                TOOL_SEARCH_TOOL_NAME,
                &json!({"query":CUSTOM}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(!found.is_error, "{}", found.output.as_text());
            let read = tool_dispatch::run(
                &ctx.registry,
                ctx.mcp.as_ref(),
                CALL.into(),
                CUSTOM,
                &json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert!(!read.is_error, "{}", read.output.as_text());
            for name in [WRITE, "srv__observe"] {
                let denied = tool_dispatch::run(
                    &ctx.registry,
                    ctx.mcp.as_ref(),
                    CALL.into(),
                    name,
                    &json!({}),
                    &ctx,
                    Emit::Silent,
                )
                .await;
                assert_eq!(denied.accounting.outcome, Some(ToolOutcome::Denied));
            }
        });
    }

    #[test_case("srv.observe"; "canonical")]
    #[test_case("srv__observe"; "wire")]
    fn disabled_mcp_dispatch_is_typed(name: &str) {
        smol::block_on(async {
            let mut ctx = stub_ctx(&AgentMode::Build);
            ctx.profile_tool_policy = profile(json!({"overrides":{"srv.*":"disabled"}}));
            let mcp = crate::mcp::stub_session(&[("srv.observe", "Observe external data")]);
            let done = tool_dispatch::run(
                &ctx.registry,
                Some(&mcp),
                CALL.into(),
                name,
                &json!({}),
                &ctx,
                Emit::Silent,
            )
            .await;
            assert_eq!(done.accounting.outcome, Some(ToolOutcome::Denied));
        });
    }
}
