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

pub mod batch;
pub mod image_generate;
mod local_document;
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
            local_document::LocalDocumentRead,
            ToolEffect::ReadOnly,
            local_document::READ_DESCRIPTION,
        ),
        entry(
            local_document::LocalDocumentWrite,
            ToolEffect::Mutating,
            local_document::WRITE_DESCRIPTION,
        ),
        entry(
            local_document::LocalDocumentApplyPatch,
            ToolEffect::Mutating,
            local_document::PATCH_DESCRIPTION,
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
    use super::*;
    use test_case::test_case;

    const DUPLICATE_NAME: &str = "two native tools share a name";
    const NAME_DRIFT: &str = "CAUDRA_NATIVE_TOOL_NAMES must list exactly what `entries` registers; \
         it drives tool classification, prompt filters, and permission defaults";

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

    #[test]
    fn workflow_registers_only_when_its_experiment_is_on() {
        let registers_workflow = |features| {
            entries(skill::SkillTool::default(), features)
                .iter()
                .any(|(tool, ..)| tool.name() == crate::tools::WORKFLOW_TOOL_NAME)
        };
        assert!(!registers_workflow(FeatureFlags::NONE));
        assert!(registers_workflow(
            FeatureFlags::NONE.with(Feature::Workflows)
        ));
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
