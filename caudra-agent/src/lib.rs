//! Async agent loop with tools.

pub mod agent;
pub mod background;
mod background_reminder;
pub mod bounded_process;
pub use background_reminder::BackgroundReminderContext;
pub mod cancel;
pub mod child_guard;
pub mod nudge;
pub use child_guard::ChildGuard;
pub mod headless;
pub mod herdr;
pub mod mailbox;
pub mod mcp;
pub mod peers;
pub use mcp::config::{McpConfigError, McpConfigErrors, McpServerInfo, McpServerStatus};
pub use mcp::protocol::PromptRole;
pub use mcp::{
    McpCommand, McpHandle, McpPromptArg, McpPromptInfo, McpSession, McpSnapshot, McpSnapshotReader,
};
pub mod commits;
pub use commits::CommitRef;
pub mod mentions;
pub use mentions::Mention;
pub(crate) mod sigil;
pub(crate) mod task_set;
pub use agent::{
    Agent, AgentParams, AgentRunParams, COMPACTION_ANCHOR, DEFAULT_GOAL_CONTINUATION_LIMIT,
    EMPTY_RESPONSE_RULE, GoalError, GoalHandle, GoalResult, GoalSnapshot, GoalStatus, GoalVerdict,
    History, HistorySnapshot, InstructionBaseline, InstructionSource, Instructions,
    LoadedInstructions, MAX_GOAL_CHARS, MAX_GOAL_CONTINUATION_LIMIT, ProjectedHistory,
    SharedHistory, UNAVAILABLE_RESULT, find_subdirectory_instructions, goal_checkin_message,
    goal_kickoff_message, is_instruction_file, is_run_failure_marker, project_for_inspection,
    project_request, stored_todos,
};
pub use cancel::{CancelMap, CancelToken, CancelTrigger};
pub use caudra_config::{AgentConfig, PermissionsConfig, ToolOutputLines};
pub use mailbox::{MailboxError, SessionMailbox};
pub use nudge::Nudge;
pub mod command;
pub mod context;
pub mod decisions;
pub mod diff;
pub mod editable_queue;
pub mod patch;
pub mod permissions;
pub mod prompt;
pub mod remote_project_context;
pub mod scratch;
mod stored_session;
mod subagent_history;
pub mod template;
mod tool_output;
pub mod tools;
pub use tools::ToolFilter;
pub mod types;
pub mod workflow;
pub mod workspace_transfer;
pub mod worktree;
pub use stored_session::{
    StoredSession, latest_stored_session, load_stored_session, open_stored_session,
    open_stored_session_with_cursor, resolve_resume_workspace, resume_workspace_session,
    workspace_logical_cwd,
};
pub use subagent_history::{
    SubagentHistoryError, SubagentHistoryLease, SubagentHistoryRecord, SubagentHistorySnapshot,
    SubagentHistoryStore, SubagentTaskMode, SubagentTaskSpec, SubagentTaskSpecCandidate,
    active_task_history_versions, active_task_history_versions_with_batch_state,
    active_task_history_versions_with_outputs, batch_task_history_versions, history_tool_call_ids,
};

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use caudra_providers::AgentError;
pub use caudra_providers::{ContentBlock, Message};
pub use caudra_providers::{EMPTY_RESPONSE_MARKER, ImageMediaType, ImageSource, ThinkingConfig};
pub use editable_queue::{
    EditableQueue, EditableQueueReceiver, PromptAdmission, QueueDelivery, QueueItemId,
    SteeringQueue, SteeringQueueEntry, SteeringQueueReceiver, editable_queue, steering_queue,
};
pub use types::{
    ActivityChild, AgentEvent, BatchProgressEvent, BatchToolEntry, BatchToolStatus, BufferSnapshot,
    CallStage, CodeGraphRow, CodeGraphSource, Delegation, DoneReason, ENVIRONMENT_COMMANDS_LABEL,
    ENVIRONMENT_MISSING_LABEL, ENVIRONMENT_NO_VERSION, Envelope, EnvironmentCommand,
    EnvironmentFact, EventSender, GrepFileEntry, GrepLine, GrepMatchGroup, INDEX_TRUNCATED,
    IndexDirectoryEntry, IndexDirectoryEntryKind, IndexLine, IndexLineSemantic, IndexOutput,
    IndexSourceRange, InstructionBlock, LuaToolProvenance, MEMORY_DIRECTORY_LABEL,
    MEMORY_REFERENCE_LABEL, MEMORY_REVISION_LABEL, MEMORY_TAG_SEPARATOR, MemoryNote,
    MemoryNoteEntry, MemoryOrigin, MemoryOutput, MemoryTagGroup, NO_FILES_FOUND, PatchedFile,
    PeerOutput, QueueConsumedItem, SearchCap, SharedBuf, ShellFilterInfo, ShellOutput, SkillOutput,
    SnapshotLine, SnapshotSpan, SpanStyle, SubagentActivity, SubagentInfo, SubagentProgress,
    TaskCard, TaskOutput, TaskProvenance, TextOutput, ToolAccounting, ToolDoneEvent, ToolInput,
    ToolOutput, ToolOutputLimits, ToolStartEvent, TurnCompleteEvent,
};
pub use types::{
    ReasoningSummary, format_live_duration, format_settled_duration, reasoning_summary,
    streaming_reasoning_summary,
};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum AgentMode {
    #[default]
    Build,
    ReadOnly,
    Plan(PathBuf),
    RemotePlan(caudra_workspace::PlanRef),
}

impl AgentMode {
    pub fn plan_path(&self) -> Option<&Path> {
        match self {
            Self::Plan(p) => Some(p),
            Self::Build | Self::ReadOnly | Self::RemotePlan(_) => None,
        }
    }

    pub fn plan_ref(&self) -> Option<&caudra_workspace::PlanRef> {
        match self {
            Self::RemotePlan(reference) => Some(reference),
            Self::Build | Self::ReadOnly | Self::Plan(_) => None,
        }
    }

    pub fn is_planning(&self) -> bool {
        matches!(self, Self::Plan(_) | Self::RemotePlan(_))
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

    fn has_pending_input(&self) -> bool {
        false
    }
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
    /// Files the caller asked to have inlined, declared rather than parsed out
    /// of `message`: `@` is too common in prose for a scan to be safe here, and
    /// the composer already knows which of them resolved to a real path.
    pub mentions: Vec<Mention>,
    /// Commits the caller asked to have inlined, declared for the same reason
    /// and resolved the same way: the composer already matched each hash
    /// against the log window it searched.
    pub commits: Vec<CommitRef>,
    pub preamble: Vec<Message>,
    pub thinking: ThinkingConfig,
    pub fast: bool,
    /// No `Default` on this struct so adding a field forces every call site to update.
    pub prompt: Option<Box<McpPromptRef>>,
    /// Resume with no turn of the caller's own: the run starts from history as
    /// it stands, and the agent decides what the request tail still needs.
    pub resume: bool,
}
