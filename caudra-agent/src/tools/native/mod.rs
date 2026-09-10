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
pub mod memory;
pub mod question;
pub mod skill;
pub mod task;
pub mod todo_write;
mod tool_output;
mod view_image;
pub mod workflow;

use std::sync::Arc;

use serde_json::json;

use super::registry::{RegistryError, Tool, ToolEffect, ToolRegistry, ToolSource};
use super::{DescriptionContext, ToolAudience, ToolFilter};
use crate::permissions::canonical_json_sha256;

pub const OWNER: &str = "caudra";

pub fn register(registry: &ToolRegistry) -> Result<(), RegistryError> {
    registry.register_many_audited(entries())
}

fn entries() -> Vec<(Arc<dyn Tool>, ToolSource, ToolEffect)> {
    vec![
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
        entry(
            question::QuestionTool,
            ToolEffect::Isolated,
            question::DESCRIPTION,
        ),
        entry(
            skill::SkillTool::default(),
            ToolEffect::ReadOnly,
            skill::DESCRIPTION,
        ),
        entry(task::TaskTool, ToolEffect::Orchestrator, task::DESCRIPTION),
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
        entry(
            workflow::WorkflowTool,
            ToolEffect::Orchestrator,
            workflow::DESCRIPTION,
        ),
    ]
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
    let contract = canonical_json_sha256(&json!({
        "tool": tool.name(),
        "effect": effect.as_str(),
        "description": description,
        "schema": tool.schema(),
    }));
    let source = ToolSource::Native {
        owner: OWNER.into(),
        contract: contract.into(),
        trusted: true,
    };
    (tool, source, effect)
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

    const DUPLICATE_NAME: &str = "two native tools share a name";
    const NAME_DRIFT: &str = "CAUDRA_NATIVE_TOOL_NAMES must list exactly what `entries` registers; \
         it drives tool classification, prompt filters, and permission defaults";

    #[test]
    fn registered_names_match_the_declared_list() {
        let registered = entries();
        let mut registered: Vec<&str> = registered.iter().map(|(t, ..)| t.name()).collect();
        registered.sort_unstable();
        let mut declared = caudra_config::CAUDRA_NATIVE_TOOL_NAMES.to_vec();
        declared.sort_unstable();
        assert_eq!(registered, declared, "{NAME_DRIFT}");
    }

    #[test]
    fn every_native_tool_registers_under_a_unique_name() {
        let registered = entries();
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
        let first: Vec<String> = entries().iter().map(|(_, s, _)| contract(s)).collect();
        let second: Vec<String> = entries().iter().map(|(_, s, _)| contract(s)).collect();
        assert_eq!(first, second);
    }

    #[test]
    fn descriptions_are_non_empty() {
        for (tool, ..) in entries() {
            assert!(
                !static_description(tool.as_ref()).trim().is_empty(),
                "{} has no description",
                tool.name()
            );
        }
    }
}
