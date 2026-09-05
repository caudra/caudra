mod compaction;
mod goal;
mod history;
mod instructions;
mod provider_projection;
mod run;
mod streaming;
pub mod subagent;
mod title;
pub mod tool_dispatch;
mod tool_preview;

pub use compaction::compact;
pub use goal::{
    GoalError, GoalHandle, GoalResult, GoalSnapshot, GoalStatus, GoalVerdict, MAX_GOAL_CHARS,
    goal_checkin_message, goal_kickoff_message,
};
pub use history::{
    History, HistorySnapshot, SharedHistory, UNAVAILABLE_RESULT, close_dangling_tool_calls,
};
pub use instructions::{
    Instructions, LoadedInstructions, build_system_prompt, find_subdirectory_instructions,
    is_instruction_file, load_instruction_text, load_instructions,
};
pub use provider_projection::{project as project_for_provider, project_for_target};
pub use run::{
    Agent, AgentParams, AgentRunParams, estimate_message_tokens, resolve_compaction_model,
};
