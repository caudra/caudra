pub mod change_recording;
pub mod commit_preamble;
mod compaction;
mod goal;
mod history;
mod instructions;
pub mod mention_preamble;
mod provider_projection;
pub(crate) mod relative_paths;
pub mod requirements;
mod run;
pub mod side_model;
pub mod speculative;
pub(crate) mod steering;
mod streaming;
pub mod subagent;
pub mod task_runner;
pub mod title;
mod tool_body;
mod tool_delegation;
pub mod tool_dispatch;
mod tool_preview;
mod tool_roster;

pub(crate) use compaction::compaction_reserve;
pub use compaction::{
    COMPACTION_ANCHOR, CompactionSpend, Spend, auto_compact_enabled, compact, compact_with_session,
    resolve_extractor,
};
pub use goal::{
    DEFAULT_GOAL_CONTINUATION_LIMIT, GoalError, GoalHandle, GoalResult, GoalSnapshot, GoalStatus,
    GoalVerdict, MAX_GOAL_CHARS, MAX_GOAL_CONTINUATION_LIMIT, goal_checkin_message,
    goal_kickoff_message,
};
pub use history::{
    History, HistorySnapshot, SharedHistory, UNAVAILABLE_RESULT, is_run_failure_marker,
    stored_todos,
};
pub(crate) use instructions::{INSTRUCTION_FILES, LOCAL_INSTRUCTION_FILE};
pub use instructions::{
    InstructionBaseline, InstructionOrigin, InstructionScope, InstructionSource, Instructions,
    LoadedInstructions, build_system_prompt, environment_block, find_remote_nested_instructions,
    find_subdirectory_instructions, is_instruction_file, load_instruction_text, load_instructions,
    load_remote_instructions,
};
pub use provider_projection::{ProjectedHistory, project_for_inspection, project_request};
pub use run::{
    Agent, AgentParams, AgentRunParams, ModelRoute, estimate_message_tokens,
    resolve_compaction_model, resolve_model_for_purpose, resolve_purpose_model,
};
#[cfg(test)]
pub(crate) use run::{
    GOAL_MET_QUESTION, goal_prescreen_state, should_skip_goal, skill_questions, skill_shortlist,
    skill_state, suggested_skill,
};
pub(crate) use run::{last_announced, resolve_purpose_model_for_inspection};
pub use steering::EMPTY_RULE as EMPTY_RESPONSE_RULE;
