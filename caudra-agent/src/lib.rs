//! Async agent loop with tools.

pub mod agent;
pub mod cancel;
pub mod child_guard;
pub use child_guard::ChildGuard;
pub mod headless;
pub mod mailbox;
pub mod mcp;
pub use mcp::config::{McpConfigError, McpConfigErrors, McpServerInfo, McpServerStatus};
pub use mcp::protocol::PromptRole;
pub use mcp::{
    McpCommand, McpHandle, McpPromptArg, McpPromptInfo, McpSession, McpSnapshot, McpSnapshotReader,
};
pub(crate) mod task_set;
pub use agent::{
    Agent, AgentParams, AgentRunParams, GoalError, GoalHandle, GoalResult, GoalSnapshot,
    GoalStatus, GoalVerdict, History, HistorySnapshot, Instructions, LoadedInstructions,
    MAX_GOAL_CHARS, SharedHistory, UNAVAILABLE_RESULT, close_dangling_tool_calls,
    find_subdirectory_instructions, goal_checkin_message, goal_kickoff_message,
    is_instruction_file, project_for_provider, project_for_target,
};
pub use cancel::{CancelMap, CancelToken, CancelTrigger};
pub use caudra_config::{AgentConfig, PermissionsConfig, ToolOutputLines};
pub use mailbox::{MailboxError, SessionMailbox};
pub mod command;
pub mod diff;
pub mod editable_queue;
pub mod permissions;
pub mod prompt;
pub mod snapshots;
mod stored_session;
mod subagent_history;
pub mod template;
mod tool_output;
pub mod tools;
pub use tools::ToolFilter;
pub mod types;
pub use stored_session::{StoredSession, latest_stored_session, load_stored_session};
pub use subagent_history::{
    SubagentHistoryError, SubagentHistoryLease, SubagentHistoryRecord, SubagentHistorySnapshot,
    SubagentHistoryStore, SubagentTaskMode, SubagentTaskSpec, SubagentTaskSpecCandidate,
    active_task_history_versions, active_task_history_versions_with_batch_state,
    batch_task_history_versions, history_tool_call_ids,
};

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use caudra_providers::AgentError;
use caudra_providers::Message;
pub use caudra_providers::{EMPTY_RESPONSE_MARKER, ImageMediaType, ImageSource, ThinkingConfig};
pub use editable_queue::{
    EditableQueue, EditableQueueReceiver, PromptAdmission, QueueDelivery, QueueItemId,
    SteeringQueue, SteeringQueueEntry, SteeringQueueReceiver, editable_queue, steering_queue,
};
pub use types::{
    AgentEvent, BufferSnapshot, DoneReason, Envelope, EventSender, GrepFileEntry, GrepLine,
    GrepMatchGroup, INDEX_TRUNCATED, IndexDirectoryEntry, IndexDirectoryEntryKind, IndexLine,
    IndexLineSemantic, IndexOutput, IndexSourceRange, InstructionBlock, LuaToolProvenance,
    NO_FILES_FOUND, PatchedFile, QueueConsumedItem, SharedBuf, ShellFilterInfo, ShellOutput,
    SnapshotLine, SnapshotSpan, SpanStyle, SubagentActivity, SubagentInfo, SubagentProgress, TextOutput,
    ToolDoneEvent, ToolInput, ToolOutput, ToolOutputLimits, ToolStartEvent, TurnCompleteEvent,
};
pub use types::{ReasoningSummary, format_live_duration, reasoning_summary};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum AgentMode {
    #[default]
    Build,
    ReadOnly,
    Plan(PathBuf),
}

impl AgentMode {
    pub fn plan_path(&self) -> Option<&Path> {
        match self {
            Self::Plan(p) => Some(p),
            Self::Build | Self::ReadOnly => None,
        }
    }

    pub fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadOnly)
    }
}

pub enum ExtractedCommand {
    Interrupt(AgentInput, u64, QueueItemId),
    InterruptBatch(Vec<QueuedInterrupt>),
    Compact(u64),
}

pub struct QueuedInterrupt {
    pub id: QueueItemId,
    pub input: AgentInput,
    pub run_id: u64,
}

pub trait InterruptSource: Send + Sync {
    fn poll(&self) -> Option<ExtractedCommand>;
}

#[derive(Clone)]
pub struct McpPromptRef {
    pub qualified_name: String,
    pub arguments: HashMap<String, String>,
}

pub struct AgentInput {
    pub message: String,
    pub mode: AgentMode,
    pub images: Vec<ImageSource>,
    pub preamble: Vec<Message>,
    pub thinking: ThinkingConfig,
    pub fast: bool,
    /// No `Default` on this struct so adding a field forces every call site to update.
    pub workflow: bool,
    pub prompt: Option<Box<McpPromptRef>>,
}
