//! Durable, runtime-neutral workflow scripting: scripts orchestrate agents through a small host
//! ABI, and every result-bearing host call is keyed so a run can be replayed from its journal.
//!
//! The `rhai` feature (default) provides the script engine and validation; without it the crate
//! still offers the metadata, outcome, host, and journal types.

#![forbid(unsafe_code)]

pub mod catalog;
#[cfg(feature = "rhai")]
pub mod engine;
pub mod host;
pub mod journal;
pub mod meta;
pub mod request;
pub mod run;
pub mod snapshot;
#[cfg(feature = "rhai")]
pub mod validate;

pub const DEEP_RESEARCH_NAME: &str = "deep-research";
pub const DEEP_RESEARCH_SOURCE: &str = include_str!("../builtins/deep-research.rhai");
/// The authoring guide the `skill` tool offers as `caudra-workflow-dev`, with
/// its frontmatter. It lives beside the engine so its examples are tested
/// against the ABI they describe.
pub const WORKFLOW_SKILL: &str = include_str!("../skill/SKILL.md");

pub use catalog::{CatalogEntry, InvalidEntry, LaunchRequest, WorkflowCatalog};
#[cfg(feature = "rhai")]
pub use engine::{EngineError, RhaiEngine, RunParams, WorkflowEngine};
pub use host::{
    AgentRequest, AgentResult, CapabilityMode, HostError, UnknownCapabilityMode, WorkflowHost,
};
pub use journal::{
    CallKey, CallKind, CallSignature, Journal, JournalEntry, JournalError, RequestHash,
    agent_request_value, canonical_json, hash_request, scratch_request_value,
};
#[cfg(feature = "rhai")]
pub use meta::parse_meta;
pub use meta::{MetaError, PhaseMeta, WorkflowMeta, is_valid_workflow_name};
pub use request::{WorkflowError, WorkflowRequest, WorkflowResponse};
pub use run::{EngineLimits, PauseKind, PauseKindError, WorkflowOutcome};
pub use snapshot::{
    AgentRosterEntry, DEFAULT_AGENT_BUDGET, MAX_ACTIVE_RUNS, MAX_AGENT_BUDGET, MAX_RUN_LOG_ENTRIES,
    RosterState, RunSnapshot, RunStatus, RunUsage, SourceKind, WORKFLOW_ABI_VERSION,
    WORKFLOW_LANGUAGE_VERSION, WorkflowEvent, WorkflowState,
};
#[cfg(feature = "rhai")]
pub use validate::{SmokeResult, ValidationError, ValidationReport, validate};

#[cfg(all(test, feature = "rhai"))]
mod tests {
    use super::*;

    const RHAI_FENCE: &str = "```rhai";
    const FENCE: &str = "```";
    const EXAMPLES_MSG: &str = "the skill must carry complete examples";

    /// Every fenced `rhai` block that starts with a `meta` header. Fragments
    /// without one are illustrations, not scripts.
    fn skill_scripts() -> Vec<String> {
        WORKFLOW_SKILL
            .split(RHAI_FENCE)
            .skip(1)
            .filter_map(|rest| {
                rest.split_once(FENCE)
                    .map(|(block, _)| block.trim().to_owned())
            })
            .filter(|block| block.starts_with("let meta"))
            .collect()
    }

    /// The guide promises that its examples run. A change to the ABI that
    /// breaks one must update the guide in the same change.
    #[test]
    fn every_complete_example_in_the_skill_validates() {
        let scripts = skill_scripts();
        assert!(scripts.len() >= 3, "{EXAMPLES_MSG}");
        for script in scripts {
            let name = parse_meta(&script).map(|meta| meta.name);
            let report = validate(&script).unwrap_or_else(|error| panic!("{name:?}: {error}"));
            assert!(
                matches!(report.smoke.outcome, WorkflowOutcome::Completed(_)),
                "{name:?}: {:?}",
                report.smoke.outcome
            );
        }
    }

    #[test]
    fn bundled_deep_research_header_matches_its_name() {
        let meta = parse_meta(DEEP_RESEARCH_SOURCE).expect("bundled script has a valid header");
        assert_eq!(meta.name, DEEP_RESEARCH_NAME);
        assert_eq!(
            meta.phases
                .iter()
                .map(|phase| phase.title.as_str())
                .collect::<Vec<_>>(),
            ["Plan", "Research", "Verify", "Report"]
        );
    }
}
