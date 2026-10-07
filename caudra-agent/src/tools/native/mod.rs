//! Caudra's own first-party tools.
//!
//! Workcell owns the protocol-neutral file, web, shell, and code contracts.
//! Everything here is work only Caudra can do: it reaches into session state,
//! the agent loop, or the terminal, so there is nothing to delegate.
//!
//! These were Lua plugins until this migration. They are native again so their
//! results survive a session reload as structured `ToolOutput` that Rust
//! renders directly, instead of a `BufferSnapshot` that has to be repainted by
//! the Lua thread on every load, click, and theme change.

pub mod automation;
pub mod batch;
pub mod image_generate;
pub mod memory;
pub mod peers;
pub mod plan;
pub mod question;
pub(crate) mod report_to_parent;
pub mod skill;
pub mod task;
pub mod task_control;
pub mod todo_write;
mod tool_output;
mod view_image;
pub mod workflow;

use std::sync::Arc;

use caudra_config::{Feature, FeatureFlags};
use serde_json::json;

use super::registry::{RegistryError, Tool, ToolEffect, ToolRegistry, ToolSource};
use super::{DescriptionContext, ToolAudience, ToolFilter};
use crate::permissions::canonical_json_sha256;
use crate::remote_project_context::RemoteSkill;

pub const OWNER: &str = "caudra";

pub fn review_contracts() -> Vec<(String, String)> {
    [
        entry(
            tool_output::ToolOutputTool,
            ToolEffect::ReadOnly,
            tool_output::DESCRIPTION,
        ),
        entry(
            view_image::ViewImage,
            ToolEffect::ReadOnly,
            view_image::DESCRIPTION,
        ),
    ]
    .into_iter()
    .filter_map(|(tool, source, _)| {
        if let ToolSource::Native { contract, .. } = source {
            Some((contract.to_string(), tool.name().to_owned()))
        } else {
            None
        }
    })
    .collect()
}

/// A tool whose experiment is off is never registered, so no catalog, allowlist,
/// or batch can reach it.
pub fn register(registry: &ToolRegistry, features: FeatureFlags) -> Result<(), RegistryError> {
    registry.register_many_audited(entries(skill::SkillTool::default(), features))
}

pub fn register_remote(
    registry: &ToolRegistry,
    skills: &[RemoteSkill],
    features: FeatureFlags,
) -> Result<(), RegistryError> {
    registry.register_many_audited(entries(skill::SkillTool::remote(skills), features))
}

fn entries(
    skill: skill::SkillTool,
    features: FeatureFlags,
) -> Vec<(Arc<dyn Tool>, ToolSource, ToolEffect)> {
    let mut entries = vec![
        entry(
            tool_output::ToolOutputTool,
            ToolEffect::ReadOnly,
            tool_output::DESCRIPTION,
        ),
        entry(
            batch::BatchTool,
            ToolEffect::Orchestrator,
            batch::DESCRIPTION,
        ),
        entry(
            image_generate::ImageGenerate,
            ToolEffect::Mutating,
            image_generate::DESCRIPTION,
        ),
        entry(
            memory::MemoryTool,
            ToolEffect::Mutating,
            memory::DESCRIPTION,
        ),
        entry(plan::PlanTool, ToolEffect::Mutating, plan::DESCRIPTION),
        entry(
            question::QuestionTool,
            ToolEffect::Isolated,
            question::DESCRIPTION,
        ),
        entry(skill, ToolEffect::ReadOnly, skill::DESCRIPTION),
        entry(task::TaskTool, ToolEffect::Orchestrator, task::DESCRIPTION),
        entry(
            task_control::TaskControl,
            ToolEffect::Orchestrator,
            task_control::DESCRIPTION,
        ),
        entry(
            todo_write::TodoWrite,
            ToolEffect::Isolated,
            todo_write::DESCRIPTION,
        ),
        entry(
            view_image::ViewImage,
            ToolEffect::ReadOnly,
            view_image::DESCRIPTION,
        ),
    ];
    if features.enabled(Feature::Workflows) {
        entries.push(entry(
            workflow::WorkflowTool,
            ToolEffect::Orchestrator,
            workflow::DESCRIPTION,
        ));
    }
    if features.enabled(Feature::Automations) {
        entries.push(entry(
            automation::AutomationTool,
            ToolEffect::ReadOnly,
            automation::DESCRIPTION,
        ));
    }
    if features.enabled(Feature::CrossSessionMessaging) {
        entries.extend([
            entry(
                peers::ListSessions,
                ToolEffect::ReadOnly,
                peers::LIST_DESCRIPTION,
            ),
            entry(
                peers::SendMessage,
                ToolEffect::Mutating,
                peers::SEND_DESCRIPTION,
            ),
            entry(
                peers::PublishMessage,
                ToolEffect::Mutating,
                peers::PUBLISH_DESCRIPTION,
            ),
            entry(
                peers::ReadTopic,
                ToolEffect::ReadOnly,
                peers::READ_DESCRIPTION,
            ),
            entry(
                peers::WorkAssignment,
                ToolEffect::Mutating,
                peers::WORK_DESCRIPTION,
            ),
        ]);
    }
    entries
}

/// `description` is passed separately because a tool may augment its live
/// description with discovered context (skills on disk, configured profiles).
/// Permission memory keys on the contract, so only the fixed text belongs in
/// it: a new skill directory must not silently revoke an approval.
fn entry(
    tool: impl Tool,
    effect: ToolEffect,
    description: &str,
) -> (Arc<dyn Tool>, ToolSource, ToolEffect) {
    let tool: Arc<dyn Tool> = Arc::new(tool);
    let contract = permission_contract(tool.as_ref(), effect, description);
    let source = ToolSource::Native {
        owner: OWNER.into(),
        contract: contract.into(),
        trusted: true,
    };
    (tool, source, effect)
}

fn permission_contract(tool: &dyn Tool, effect: ToolEffect, description: &str) -> String {
    canonical_json_sha256(&json!({
        "tool": tool.name(),
        "effect": effect.as_str(),
        "description": description,
        "schema": tool.schema(),
    }))
}

/// Neutral context for callers that need a description outside a live turn.
pub fn static_description(tool: &dyn Tool) -> String {
    let filter = ToolFilter::All;
    tool.description(&DescriptionContext {
        filter: &filter,
        audience: ToolAudience::MAIN,
        workflows_available: false,
    })
    .into_owned()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::fs::Permissions;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use caudra_config::{Feature, FeatureFlags, ProfileToolDefault, ProfileToolPolicy};
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CwdHandle, ProjectIdentity, ProjectKey,
        ResourceId, ResourceScope, SessionBindingId, SessionWorkspaceBinding, SourceTrustAnchor,
        WorkspaceCapabilities, WorkspaceCursor, WorkspaceHandle, WorkspaceServices,
        WorkspaceSession,
    };
    use serde_json::json;
    use tempfile::{Builder, TempDir};
    use test_case::test_case;

    use super::{entries, peers, register, register_remote, skill, static_description};
    use crate::AgentMode;
    use crate::agent::tool_dispatch::{Emit, run};
    use crate::template::Vars;
    use crate::tools::registry::{Tool, ToolEffect, ToolSource};
    use crate::tools::test_support::stub_ctx;
    use crate::tools::{BATCH_TOOL_NAME, DescriptionContext, ToolAudience, ToolFilter};
    use crate::types::{BatchToolStatus, ToolOutput};

    const DUPLICATE_NAME: &str = "two native tools share a name";
    const NAME_DRIFT: &str = "CAUDRA_NATIVE_TOOL_NAMES must list exactly what `entries` registers; \
         it drives tool classification, prompt filters, and permission defaults";
    const REMOVED_TOOLS: &[&str] = &[
        "local_document_read",
        "local_document_write",
        "local_document_apply_patch",
    ];
    const UNKNOWN_TOOL: &str = "unknown tool";
    const PROJECT: &str = "project-a";
    #[cfg(unix)]
    const DIRECTORY_MODE: u32 = 0o700;

    pub(super) fn tempdir() -> TempDir {
        let builder = &mut Builder::new();
        #[cfg(unix)]
        builder.permissions(Permissions::from_mode(DIRECTORY_MODE));
        builder
            .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .unwrap()
    }

    pub(super) fn workspace_for_principal(subject: &str) -> WorkspaceSession {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("test-source").expect("trust anchor"),
            "authority",
            "workspace",
            "generation",
            "namespace",
        )
        .expect("authority");
        let principal =
            AuthenticatedPrincipalId::new(authority.clone(), subject).expect("principal");
        let project = ProjectIdentity::new(
            authority.clone(),
            ProjectKey::new(PROJECT).expect("project key"),
        );
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new("binding").expect("binding id"),
            authority.clone(),
            principal,
            project,
        )
        .expect("binding");
        let cursor = WorkspaceCursor::new(
            &binding,
            ResourceScope::root(ResourceId::new("root").expect("resource id")),
            1,
            CwdHandle::new("cwd").expect("cwd handle"),
        );
        let handle = WorkspaceHandle::new(
            authority,
            WorkspaceCapabilities::new([]),
            WorkspaceServices::default(),
        )
        .expect("workspace handle");
        WorkspaceSession::new(handle, binding, cursor).expect("workspace session")
    }

    #[test_case(false; "local")]
    #[test_case(true; "remote")]
    fn removed_document_tools_are_absent_and_refused(remote: bool) {
        smol::block_on(async {
            let mut ctx = stub_ctx(&AgentMode::Build);
            if remote {
                ctx.workspace_session = Some(workspace_for_principal("principal"));
                register_remote(&ctx.registry, &[], FeatureFlags::all()).unwrap();
            } else {
                register(&ctx.registry, FeatureFlags::all()).unwrap();
            }
            for default in [
                ProfileToolDefault::Eager,
                ProfileToolDefault::Lazy,
                ProfileToolDefault::Disabled,
            ] {
                let definitions = ctx.registry.definitions_split_with_policy(
                    &Vars::new(),
                    &DescriptionContext {
                        filter: &ToolFilter::All,
                        audience: ToolAudience::MAIN,
                        workflows_available: true,
                    },
                    false,
                    &[],
                    &ProfileToolPolicy {
                        default,
                        ..ProfileToolPolicy::default()
                    },
                    ctx.has_session_plan(),
                );
                for name in REMOVED_TOOLS {
                    assert!(!ctx.registry.has(name));
                    assert!(!definitions.available_filter().matches(name));
                }
            }
            for name in REMOVED_TOOLS {
                let done = run(
                    &ctx.registry,
                    None,
                    (*name).into(),
                    name,
                    &json!({}),
                    &ctx,
                    Emit::Silent,
                )
                .await;
                assert!(done.is_error);
                assert_eq!(done.output.as_text(), format!("{UNKNOWN_TOOL}: {name}"));
            }
            let calls: Vec<_> = REMOVED_TOOLS
                .iter()
                .map(|name| json!({"tool": name, "parameters": {}}))
                .collect();
            let done = run(
                &ctx.registry,
                None,
                BATCH_TOOL_NAME.into(),
                BATCH_TOOL_NAME,
                &json!({"tool_calls": calls}),
                &ctx,
                Emit::Silent,
            )
            .await;
            let ToolOutput::Batch { entries, .. } = done.output else {
                panic!("expected batch output");
            };
            assert_eq!(entries.len(), REMOVED_TOOLS.len());
            for (entry, name) in entries.iter().zip(REMOVED_TOOLS) {
                assert_eq!(entry.status, BatchToolStatus::Error);
                assert_eq!(
                    entry.output.as_ref().unwrap().as_text(),
                    format!("{UNKNOWN_TOOL}: {name}")
                );
            }
        });
    }

    fn every_entry() -> Vec<(Arc<dyn Tool>, ToolSource, ToolEffect)> {
        entries(skill::SkillTool::default(), FeatureFlags::all())
    }

    #[test]
    fn registered_names_match_the_declared_list() {
        let registered = every_entry();
        let mut registered: Vec<&str> = registered.iter().map(|(t, ..)| t.name()).collect();
        registered.sort_unstable();
        let mut declared = caudra_config::CAUDRA_NATIVE_TOOL_NAMES.to_vec();
        declared.sort_unstable();
        assert_eq!(registered, declared, "{NAME_DRIFT}");
    }

    #[test]
    fn every_native_tool_registers_under_a_unique_name() {
        let registered = every_entry();
        let mut names: Vec<&str> = registered.iter().map(|(t, ..)| t.name()).collect();
        names.sort_unstable();
        let unique = names.len();
        names.dedup();
        assert_eq!(names.len(), unique, "{DUPLICATE_NAME}");
    }

    #[test]
    fn contracts_are_stable_across_calls() {
        let contract = |source: &ToolSource| match source {
            ToolSource::Native { contract, .. } => contract.to_string(),
            other => panic!("native tools must register as native, got {other:?}"),
        };
        let first: Vec<String> = every_entry().iter().map(|(_, s, _)| contract(s)).collect();
        let second: Vec<String> = every_entry().iter().map(|(_, s, _)| contract(s)).collect();
        assert_eq!(first, second);
    }

    #[test]
    fn descriptions_are_non_empty() {
        for (tool, ..) in every_entry() {
            assert!(
                !static_description(tool.as_ref()).trim().is_empty(),
                "{} has no description",
                tool.name()
            );
        }
    }

    #[test_case(Feature::Workflows, crate::tools::WORKFLOW_TOOL_NAME; "workflow")]
    #[test_case(Feature::Automations, crate::tools::AUTOMATION_TOOL_NAME; "automation")]
    fn experimental_tool_registers_only_when_its_experiment_is_on(feature: Feature, name: &str) {
        let registers = |features| {
            entries(skill::SkillTool::default(), features)
                .iter()
                .any(|(tool, ..)| tool.name() == name)
        };
        assert!(!registers(FeatureFlags::all().without(feature)));
        assert!(registers(FeatureFlags::NONE.with(feature)));
    }

    #[test_case(FeatureFlags::NONE, false; "disabled")]
    #[test_case(FeatureFlags::NONE.with(Feature::Workflows), false; "independent_of_workflows")]
    #[test_case(FeatureFlags::NONE.with(Feature::CrossSessionMessaging), true; "enabled")]
    fn messaging_registers_only_when_its_experiment_is_on(features: FeatureFlags, enabled: bool) {
        let entries = entries(skill::SkillTool::default(), features);
        for name in peers::TOOL_NAMES {
            assert_eq!(
                entries.iter().any(|(tool, ..)| tool.name() == *name),
                enabled
            );
        }
    }
}
