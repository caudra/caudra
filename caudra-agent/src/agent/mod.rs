mod compaction;
mod goal;
mod history;
mod instructions;
pub mod mention_preamble;
mod provider_projection;
mod run;
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
pub use compaction::{COMPACTION_ANCHOR, auto_compact_enabled, compact};
pub use goal::{
    DEFAULT_GOAL_CONTINUATION_LIMIT, GoalError, GoalHandle, GoalResult, GoalSnapshot, GoalStatus,
    GoalVerdict, MAX_GOAL_CHARS, MAX_GOAL_CONTINUATION_LIMIT, goal_checkin_message,
    goal_kickoff_message,
};
pub use history::{
    History, HistorySnapshot, SharedHistory, UNAVAILABLE_RESULT, close_dangling_tool_calls,
    is_run_failure_marker,
};
pub use instructions::{
    InstructionBaseline, InstructionScope, InstructionSource, Instructions, LoadedInstructions,
    build_system_prompt, build_system_prompt_for_remote, environment_block,
    find_remote_nested_instructions, find_subdirectory_instructions, is_instruction_file,
    load_instruction_text, load_instructions, load_remote_instructions,
};
pub use provider_projection::{project as project_for_provider, project_for_target};
pub use run::{
    Agent, AgentParams, AgentRunParams, ModelRoute, estimate_message_tokens,
    resolve_compaction_model, resolve_model_for_purpose, resolve_purpose_model,
};
