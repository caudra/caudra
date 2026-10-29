//! Elm-style `update(Msg) -> Vec<Action>`; side effects are dispatched by the caller.
//! Double-esc cancels/rewinds and double Ctrl+D exits within `CONFIRMATION_DURATION`.
//! `run_id` invalidates in-flight agent events. It bumps in exactly three
//! places, one per transition: `start_run`, `handle_cancel`, and
//! `AgentHandles::respawn`. Everything else only reads it.

mod accounts;
pub(crate) mod automation;
pub(crate) mod autopilot;
pub(crate) mod background_delivery;
mod btw;
mod command_sources;
pub(crate) mod decisions;
mod delegation;
mod env;
mod extract;
pub(crate) mod file_revert;
mod goal;
#[cfg(debug_assertions)]
mod grab;
#[cfg(test)]
mod grammar_agreement;
mod image_paste;
mod learning;
mod live_switch;
mod memory;
pub(crate) mod mode;
mod mouse;
pub(crate) mod permission_editor;
pub(crate) mod questions;
mod queue;
pub(crate) mod restart;
mod sandbox;
mod search;
mod session;
pub(crate) mod session_state;
mod settings;
pub(crate) mod shell;
mod shells;
mod stash;
mod task_history;
pub(crate) mod tasks;
#[cfg(test)]
pub(crate) mod tests;
mod tooltip;
mod transfer;
pub(crate) mod view;
mod workbench;
pub(crate) mod workflow;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::mem;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use caudra_workbench::text_field::{self, FieldKind, TextCommand};
use caudra_workbench::{DocumentKey, Layout as WorkbenchLayout, Workbench, WorkbenchAction};

use crate::agent::ModelSlot;
use crate::agent::SharedMode;
use crate::agent::ToolsPreviewSource;
use crate::app::decisions::decision_choices;
use crate::app::env::ShellEnvLink;
use crate::app::settings::SettingsLink;
use crate::app::tasks::TaskOutcome;
use crate::app::workbench::{MountScope, StoredDocument};
use crate::chat::Chat;
use crate::chat::{CANCELLED_TEXT, ChatEventResult, DONE_TEXT, ERROR_TEXT, format_with_images};
use crate::clipboard::{ClipboardState, CopyResult};
use crate::components::accounts_modal::AccountsModal;
use crate::components::audit_log::PanelTab;
use crate::components::automation_inspector::{AutomationAction, AutomationInspector};
use crate::components::autopilot_panel::AutopilotPanel;
use crate::components::changelog_modal::{
    self, ChangelogAction, ChangelogModal, View as ChangelogView,
};
use crate::components::command::words::{
    ALL_ARG, CHAT_ARG, CLEAR_ARG, DISCOVER_ARG, DISMISS_ARG, GOAL_CLEAR_ALIASES, LIST_ARG, LOG_ARG,
    MIGRATE_ARG, MODEL_ARG, MOVE_ARG, NEXT_ARG, POP_ARG, PROJECT_ARG, SUGGESTED_ARG,
};
use crate::components::command::{
    CommandAction, CommandPalette, ParsedCommand, SEARCH_COMMAND, disabled_feature,
};
use crate::components::command_modal::{CommandModal, CommandModalAction};
use crate::components::commit_popup::{CommitAction, CommitIndex, CommitPopup};
use crate::components::context_modal::ContextModal;
use crate::components::decisions_modal::{DecisionsFetchState, DecisionsModal};
use crate::components::docs_link::DocsTopic;
use crate::components::docs_modal::{DocsAction, DocsModal};
use crate::components::document_view::COPIED_SELECTION;
use crate::components::env_modal::EnvModal;
use crate::components::file_picker::{FilePickerModal, FilePickerModalAction};
use crate::components::goal_panel::{
    GoalPanel, cleared_after_error_row, evaluation_failed_row, finished_row, not_met_row,
};
use crate::components::help_modal::HelpModal;
use crate::components::image_viewer::{ImageViewer, ImageViewerAction};
use crate::components::input::{
    AdmissionHit, ChordHint, ChordHintHit, InputAction, InputBox, InputState, Submission,
};
use crate::components::keybindings::{self, KeybindContext, key, leader};
use crate::components::learn_inspector::LearnInspector;
use crate::components::learn_transcript::LearnTranscripts;
use crate::components::login_picker::{LoginPicker, LoginPickerAction};
use crate::components::logs_modal::{LogsAction, LogsModal};
use crate::components::lua_float::FloatManager;
use crate::components::mcp_picker::{McpPicker, McpPickerAction};
use crate::components::memory_inspector::MemoryInspector;
use crate::components::mention_popup::{MentionAction, MentionPopup};
use crate::components::message_actions::{MessageActionKind, MessageActions, MessageActionsAction};
use crate::components::messages::MessageActionTarget;
use crate::components::mode_submission::ModeSubmission;
use crate::components::model_picker::{ModelPicker, ModelPickerAction};
use crate::components::paste_editor::{PasteEditor, PasteEditorAction, PasteEditorTarget};
use crate::components::peer_manager::{PeerManager, PeerManagerAction, PeerView};
use crate::components::permission_prompt::{PermissionDecision, PermissionPrompt, PromptMouse};
use crate::components::permissions_picker::{
    DiscoveryState, PermissionsPicker, PermissionsPickerAction,
};
use crate::components::plan_form::{PlanForm, PlanFormAction};
use crate::components::projection_modal::{Projection, ProjectionAction, ProjectionModal};
use crate::components::prompt_profile_picker::{PromptProfilePicker, PromptProfilePickerAction};
use crate::components::question_form::{QuestionForm, QuestionFormAction};
use crate::components::queue_actions::{QueueActionKind, QueueActions, QueueActionsAction};
use crate::components::queue_panel::{QueueHit, QueueHitTarget};
use crate::components::review::{ReviewAction, ReviewModal};
use crate::components::rewind_picker::{RewindPicker, RewindPickerAction};
use crate::components::sandbox_manager::SandboxManager;
use crate::components::scrollbar;
use crate::components::search_modal::{SearchAction, SearchModal, SearchTab};
use crate::components::session_picker::{SessionPicker, SessionRow};
use crate::components::session_relocation::SessionRelocationPicker;
use crate::components::settings_modal::{RoleEdit, SettingsModal};
use crate::components::shell_modal::ShellModal;
use crate::components::skills_modal::SkillsModal;
use crate::components::stash_picker::StashPicker;
use crate::components::status_bar::{
    PendingTrust, StatusBar, StatusBarHit, StatusBarHitTarget, context_share, model_leaf,
};
use crate::components::storage_modal::{StorageFetchState, StorageModal};
use crate::components::stream_modal::{StreamAction, StreamModal};
use crate::components::system_prompt_modal::{SystemPromptAction, SystemPromptModal};
use crate::components::task_picker::TaskPicker;
use crate::components::theme_picker::{ThemePicker, ThemePickerAction};
use crate::components::thinking_picker::{ThinkingPicker, ThinkingPickerAction};
use crate::components::todo_panel::TodoPanel;
use crate::components::tools_modal::{ToolsModal, ToolsScope};
use crate::components::tooltip::{TipArea, Tooltip};
use crate::components::update_prompt::{UpdateChoice, UpdatePrompt, UpdatePromptAction};
use crate::components::usage_modal::{UsageFetchState, UsageModal, UsageScope};
use crate::components::which_key::WhichKey;
use crate::components::workbench::{paint_image, paint_markdown, styles as workbench_styles};
use crate::components::workflow_catalog_picker::WorkflowCatalogPicker;
use crate::components::workflow_inspector::WorkflowInspector;
use crate::components::worktree_picker::{WorktreePicker, WorktreeView};
use crate::components::{
    Action, DisplayMessage, DisplayRole, DisplaySource, ExitRequest, GoalTone, Overlay,
    PlanHandoff, RetryInfo, Status, format_elapsed, is_ctrl,
};
use crate::config_problems::ConfigProblems;
use crate::image;
use crate::inline_image::{self, ImageRef};
use crate::input_document::InputDraft;
use crate::repaint::{Cadence, Dirty, Watch};
use crate::sandbox::{SandboxWorkers, StoreReply};
use crate::selection::{SelectionState, SelectionZone, ZoneRegistry};
use crate::{AppSession, PatternDiscoveryMode, PatternDiscoveryOutcome, PatternSuggestionLoader};
use arc_swap::{ArcSwap, ArcSwapOption};
use caudra_agent::agent::task_runner::WorkflowHostContext;
use caudra_agent::autopilot::{ActionName, DecisionKind};
use caudra_agent::background::{BackgroundTasks, ShellSnapshot};
use caudra_agent::commits::repo;
use caudra_agent::context::{ContextKey, ContextSnapshot, ContextStore};
use caudra_agent::learning::LearnerHandle;
use caudra_agent::mcp::McpSnapshot;
use caudra_agent::mentions;
use caudra_agent::permissions::{AutopilotNote, PermissionManager, PermissionPolicyError};
use caudra_agent::prompt::profile::PromptProfileCatalog;
use caudra_agent::tools::ToolAudience;
use caudra_agent::tools::native::plan::{self, PlanTarget};
use caudra_agent::types::{
    AutopilotEvent, BACKGROUND_EVENT_RUN_ID, QUESTION_ACCEPTED_REPLY, QUESTION_DECLINED_REPLY,
    QuestionDelivery, WorkflowProvenance,
};
use caudra_agent::{
    AgentEvent, AgentInput, AgentMode, CommitRef, Envelope, GoalVerdict, ImageSource,
    McpConfigErrors, McpSnapshotReader, Mention, PromptAdmission, QueueItemId, SharedHistory,
    SteeringQueue, SubagentHistoryStore, SubagentInfo, ToolOutput, project_for_inspection,
};
use caudra_automation::event::{StartedBy, TurnOutcome};
use caudra_config::providers::ProvidersConfig;
use caudra_config::{
    Feature, FeatureDisabled, FeatureFlags, ModelPolicy, PermissionsConfig, SkillPacks,
    SnapshotsConfig, UiConfig,
};
use caudra_docs::{DocsLibrary, ReleaseNotesSource};
use caudra_lua::{
    BuiltinAction, EventHandle, HintReader, HintSnapshot, KeymapReader, LuaCommandReader, WinView,
};
use caudra_providers::accounts::{AccountStatus, RoutedAccount};
use caudra_providers::calibration;
use caudra_providers::model_registry::{self, Binding};
use caudra_providers::provider::{adjust_model_for_inspection, seed_setup_thinking};
use caudra_providers::{
    Billing, CacheKey, ContentBlock, Message, Model, ModelPurpose, RequestOptions,
    ResolvedThinking, ThinkingConfig, TokenUsage, add_cost, project_messages,
};
use caudra_storage::StateDir;
use caudra_storage::autopilot::AutopilotStopper;
use caudra_storage::background::JobKind;
use caudra_storage::cache_warming::{CacheWarmLedger, WarmingSummary};
use caudra_storage::id::SessionRef;
use caudra_storage::input_history::InputHistory;
use caudra_storage::media::MediaStore;
use caudra_storage::model::persist_model;
use caudra_storage::tool_ledger::{ToolCall, ToolLedger, ToolStats};
use caudra_storage::usage_ledger::{LedgerPurpose, LifetimeUsage, TurnUsage, UsageLedger};
use caudra_storage::view::ViewMode;
use caudra_terminal_host::status::{BlockKind, PaneMetadata, PaneState, PaneStatus};
use caudra_workspace::WorkspaceChangeService;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use flume::{Receiver, TryRecvError};
use serde_json::Value;
use smol::Task;

use crate::storage_writer::{StorageWriter, WarmingWrite};
use crate::update::{self, Available, UpdateFlow, UpdateNotice};
use ratatui::layout::{Position, Rect};

pub(crate) use crate::agent::QueuedMessage;
pub use crate::components::RestoreMode;
use mode::PLAN_COPY_FAILED;
pub(crate) use mode::{Mode, PlanState, PlanTrigger};
use mouse::Autoscroll;
#[cfg(test)]
use mouse::EDGE_SCROLL_LINES;
pub(crate) use permission_editor::ConversationPermissions;
pub(crate) use queue::{MODE_DECISION_ERR, MessageQueue, SubmitOutcome};
use session::{MergedHistory, Sent};
pub(crate) use session::{REVERT_BUSY_MSG, reachable_subagent_ids, session_has_content};
use session_state::SessionState;

const CANCEL_MSG: &str = "Cancelled.";
/// Columns a diagram travels per pan keystroke or wheel notch.
const PAN_STEP: i32 = 4;
/// Bypasses the per-run staleness filter because re-bake replies
/// don't belong to any real agent run.
pub(crate) const RESTORE_RUN_ID: u64 = u64::MAX;
const FLASH_CANCEL: &str = "Press esc again to stop...";
const FLASH_REWIND: &str = "Press esc again to rewind...";
const FLASH_EXIT: &str = "Press Ctrl+D again to exit...";
const CONFIRMATION_DURATION: Duration = Duration::from_secs(3);
const FLASH_NO_CHORD: &str = "is not a chord";
const RETRY_COUNTDOWN_UNAVAILABLE: &str =
    "Retry countdown unavailable: delay exceeds the platform clock range";
const CONTEXT_USAGE: &str = "Usage: /context [all]";
const CHANGELOG_USAGE: &str = "Usage: /changelog [all]";
const STORAGE_USAGE: &str = "Usage: /storage [all]";
const TOOLS_USAGE: &str = "Usage: /tools";
const SKILLS_USAGE: &str = "Usage: /skills";
const DECISIONS_USAGE: &str = "Usage: /decisions [model|next]";
const STASH_USAGE: &str = "Usage: /stash [pop|list]";
const UPDATE_USAGE: &str = "Usage: /update [dismiss]";
const MODEL_USAGE: &str = "Usage: /model [next]";
const SESSIONS_USAGE: &str = "Usage: /sessions [move|migrate [directory]]";
const PERMISSIONS_USAGE: &str = "Usage: /permissions [discover]";
const AUTH_EXPIRED_MSG: &str = "Authentication failed. Run `caudra auth login` in another terminal; Caudra will resume automatically, or press Enter to retry now.";
const WORKFLOW_AUTH_REQUIRED: &str =
    "A workflow agent needs authentication. Run `caudra auth login` in another terminal.";
const WORKFLOW_REQUESTER_PREFIX: &str = "workflow";
const COPY_FAILED: &str = "Copy failed: ";
const HISTORY_UNREADABLE: &str = "Failed to read session history: ";
const UNRECORDED_MSG: &str = "A call ran without file revert";
const FAST_UNSUPPORTED_MSG: &str = "Fast mode needs a model that sells a fast tier (API only)";
const THINKING_UNSUPPORTED_MSG: &str = "Thinking requires a model that supports it";
const FAVORITE_SAVE_ERR: &str = "Could not save favorite models";
const MODEL_SWITCH_PREFIX: &str = "Model: ";
const FAST_ON_MSG: &str = "Fast mode: on";
const FAST_OFF_MSG: &str = "Fast mode: off";
const YOLO_ON_MSG: &str = "YOLO mode enabled";
const YOLO_OFF_MSG: &str = "YOLO mode disabled";
const AUTO_ON_MSG: &str = "Auto permission mode enabled";
const AUTO_OFF_MSG: &str = "Auto permission mode disabled";
const NO_UPDATE_MSG: &str = "No update notice to dismiss";
const NO_UPDATE_AVAILABLE_MSG: &str = "No update is available";
const AUTO_VIEW_MSG: &str = "View: auto (the newest card stays open)";
const COMPACT_VIEW_MSG: &str = "View: compact";
const EXPANDED_VIEW_MSG: &str = "View: expanded";
const STEER_NOT_CONSUMED_MSG: &str = "Task finished before it consumed the message";
pub(super) const TASK_NOT_RUNNING: &str = "Only a running task can have its turn replaced";
const REVIEW_READY_MSG: &str = "Review notes added to the prompt";
const REVIEW_UNAVAILABLE_MSG: &str = "Nothing to review here yet";
const SHELL_PASTE_EXPANDED_MSG: &str = "Expanded pasted text; press Enter again to run it";
const IMPLEMENT_VERB: &str = "Implement";
const IMPLEMENT_MSG_PREFIX: &str = "Implement the plan";
const IMPLEMENT_CAPTURE_FAILED: &str = "Cannot implement plan";
const IMPLEMENT_PARALLEL_HINT: &str = "Use batch+task to parallelize, assign each subagent a separate module and restrict its tests to that module to avoid interference.";
const PERMISSION_BLOCKER: &str = "Permission requested";
const AUTH_BLOCKER: &str = "Authentication required";
const PLAN_BLOCKER: &str = "Plan ready";
const QUESTION_BLOCKER: &str = "Question requested";
const AUTOPILOT_BLOCKER: &str = "Autopilot needs you";
const AUTOPILOT_ENDED_NOTICE: &str = "Autopilot ended";
const AUTOMATION_NOTICE_PREFIX: &str = "automation";
const LOGIN_BLOCKER: &str = "Provider login required";
const MCP_TRUST_BLOCKER: &str = "MCP trust required";
const PROJECT_PERMISSION_CONFIG_TRUST_BLOCKER: &str = "Project permission config trust required";
const SANDBOX_DRAFT_BLOCKER: &str =
    "Open /sandbox and Save or Discard the unsaved configuration draft";
const PATTERN_DISCOVERY_DISABLED: &str =
    "Local history discovery is disabled for remote or ephemeral sessions.";
const PATTERN_WORKER_UNAVAILABLE: &str =
    "History discovery worker is unavailable. Restart Caudra to retry.";
const PATTERN_INVALID_CONTEXT: &str =
    "Discovery returned proposals for an invalid context. Refresh to retry.";
const PATTERN_WORKER_DISCONNECTED: &str =
    "Discovery worker stopped or its queue is full. Refresh to retry.";

const MISSING_TOOL_COMPLETION: &str = "Tool did not report completion before the turn ended";
const NOTIFICATION_PREVIEW_CHARS: usize = 200;

/// Depth budget for `caudra.api.run_command` chains. Aliases nest a level or two
/// in practice; the cap only exists so a command aliasing itself reports an
/// error instead of ping-ponging with the Lua thread forever.
pub(crate) const MAX_COMMAND_DEPTH: u8 = 8;
pub(crate) const COMMAND_DEPTH_MSG: &str = "slash command nested too deeply (alias cycle?)";
pub(crate) const MAIN_ONLY_CMD_MSG: &str = "Command applies to the main session";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Notification {
    TurnComplete {
        response: Option<String>,
    },
    PermissionRequested {
        tool: Option<String>,
    },
    AuthenticationRequired,
    QuestionRequested,
    PlanReady,
    /// What an automation's `notify()` asked to show.
    AutomationNotice {
        automation: String,
        text: String,
    },
    /// A card autopilot left to the user. Its reason is the delegate's own
    /// text, so it stays on the card.
    AutopilotNeedsYou {
        wait: DecisionKind,
    },
    AutopilotEnded {
        reason: &'static str,
    },
}

impl Notification {
    /// Prompts blocking the agent outrank turn completions and notices.
    pub(crate) fn is_urgent(&self) -> bool {
        !matches!(
            self,
            Self::TurnComplete { .. } | Self::AutomationNotice { .. } | Self::AutopilotEnded { .. }
        )
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::TurnComplete { response } => response
                .clone()
                .unwrap_or_else(|| "Agent turn complete".into()),
            Self::PermissionRequested { tool: Some(tool) } => {
                format!("{PERMISSION_BLOCKER}: {tool}")
            }
            Self::PermissionRequested { tool: None } => PERMISSION_BLOCKER.into(),
            Self::AuthenticationRequired => AUTH_BLOCKER.into(),
            Self::QuestionRequested => QUESTION_BLOCKER.into(),
            Self::PlanReady => PLAN_BLOCKER.into(),
            Self::AutomationNotice { automation, text } => {
                format!("{AUTOMATION_NOTICE_PREFIX} {automation}: {text}")
            }
            Self::AutopilotNeedsYou { wait } => format!("{AUTOPILOT_BLOCKER}: {}", wait.name()),
            Self::AutopilotEnded { reason } => format!("{AUTOPILOT_ENDED_NOTICE}: {reason}"),
        }
    }

    pub(crate) fn error_completion() -> Self {
        Self::TurnComplete {
            response: Some("Agent stopped with an error".into()),
        }
    }

    /// Cut like a response preview, since both can land in the system's notification history.
    pub(crate) fn automation_notice(automation: &str, text: &str) -> Self {
        Self::AutomationNotice {
            automation: automation.to_owned(),
            text: normalize_preview(text).unwrap_or_default(),
        }
    }
}

/// Lazy, so a huge response only costs the first `NOTIFICATION_PREVIEW_CHARS`
/// characters.
fn notification_preview<'a>(chunks: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut preview: String = chunks
        .flat_map(str::split_whitespace)
        .enumerate()
        .flat_map(|(i, word)| (i > 0).then_some(' ').into_iter().chain(word.chars()))
        .take(NOTIFICATION_PREVIEW_CHARS)
        .collect();
    if preview.ends_with(' ') {
        preview.pop();
    }
    (!preview.is_empty()).then_some(preview)
}

fn normalize_preview(text: &str) -> Option<String> {
    notification_preview(std::iter::once(text))
}

fn tools_mode_name(mode: &AgentMode) -> &'static str {
    match mode {
        AgentMode::Build => "Build",
        AgentMode::ReadOnly => "Read-only",
        AgentMode::Plan(_) | AgentMode::RemotePlan(_) => "Plan",
    }
}

pub(crate) fn turn_response(message: &Message) -> Option<String> {
    if message.has_tool_calls() {
        return None;
    }

    notification_preview(message.content.iter().filter_map(|block| match block {
        ContentBlock::Text { text } => Some(text.as_str()),
        _ => None,
    }))
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) enum PendingInput {
    #[default]
    None,
    AuthRetry {
        waiters: HashSet<Option<String>>,
    },
}

pub enum Msg {
    Key(KeyEvent),
    KeyWithText { key: KeyEvent, text: Option<String> },
    Paste(String),
    Mouse(MouseEvent),
    Scroll { column: u16, row: u16, delta: i32 },
    Agent(Box<Envelope>),
}

/// A log read the `#` popup is waiting on. At most one runs at a time, so a
/// second `#` while one is in flight rides on it rather than stacking work.
enum CommitReload {
    Idle,
    /// A local `gix` walk on a blocking thread, since a repository read has no
    /// business happening on the thread that draws.
    Local(Receiver<Vec<repo::CommitSummary>>),
    /// A remote refresh the bound workbench is running. It owns the only client
    /// that can page a remote log, so its answer is the composer's answer.
    Workbench,
}

/// Who `PageUp`, `PageDown`, `Home` and `End` act on once no overlay has
/// claimed them. Everything else keeps reaching the composer, and any key that
/// does hands the focus straight back, so the transcript can never hold the
/// keyboard hostage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyFocus {
    Composer,
    Transcript,
}

#[derive(PartialEq, Eq)]
struct ToolsPreviewKey {
    mode: AgentMode,
    session_plan: bool,
    chat_model: String,
    model: String,
    bindings: [Option<Binding>; ModelPurpose::ALL.len()],
    window: u32,
    thinking: ThinkingConfig,
    cwd: String,
    registry_revision: u64,
}

#[derive(Default)]
struct ToolsPreviewCache {
    key: Option<ToolsPreviewKey>,
    source: Watch<ToolsPreviewSource>,
    mcp: Watch<McpSnapshot>,
    snapshot: Option<Arc<ContextSnapshot>>,
}

pub struct App {
    pub(super) chats: Vec<Chat>,
    pub(super) active_chat: usize,
    pub(super) chat_index: HashMap<String, usize>,
    pub(crate) input_box: InputBox,
    subagent_input_box: InputBox,
    subagent_input_task: Option<String>,
    subagent_drafts: HashMap<String, InputState>,
    pub(super) command_palette: CommandPalette,
    pub(crate) no_commands: bool,
    pub(super) command_modal: CommandModal,
    pub(super) theme_picker: ThemePicker,
    pub(super) thinking_picker: ThinkingPicker,
    pub(super) accounts_modal: AccountsModal,
    /// Where the main agent's latest request went, for the status chip.
    pub(super) routed_account: Option<RoutedAccount>,
    /// Filled once a `/accounts` refresh has asked every subscription for
    /// its quota, with the failures to show.
    accounts_refresh: Arc<ArcSwapOption<Vec<String>>>,
    /// What `/usage` lists per account, read when it opens and after each
    /// quota fetch.
    pub(super) usage_accounts: Vec<AccountStatus>,
    pub(super) prompt_profile_picker: PromptProfilePicker,
    pub(super) model_picker: ModelPicker,
    pub(super) login_picker: LoginPicker,
    pub(super) mcp_picker: McpPicker,
    pub(super) env_modal: EnvModal,
    pub(crate) shell_env: ShellEnvLink,
    pub(super) settings_modal: SettingsModal,
    pub(crate) settings: SettingsLink,
    pub(super) rewind_picker: RewindPicker,
    pub(super) message_actions: MessageActions,
    pub(super) queue_actions: QueueActions,
    pub(super) mode_submission: ModeSubmission,
    pub(super) update_prompt: UpdatePrompt,
    pending_plan_submission: Option<queue::PendingPlanSubmission>,
    pub(super) review: ReviewModal,
    pub(super) help_modal: HelpModal,
    pub(super) which_key: WhichKey,
    pub(super) tooltip: Tooltip,
    pub(super) usage_modal: UsageModal,
    pub(super) context_modal: ContextModal,
    pub(super) logs_modal: LogsModal,
    pub(super) docs_modal: DocsModal,
    /// The topmost modal's `?` as last drawn, and the docs it opens.
    pub(super) docs_badge: Option<(DocsTopic, Rect)>,
    pub(super) changelog_modal: ChangelogModal,
    pub(super) tools_modal: ToolsModal,
    pub(super) decisions_modal: DecisionsModal,
    pub(super) skills_modal: SkillsModal,
    pub(super) storage_modal: StorageModal,
    pub(super) system_prompt_modal: SystemPromptModal,
    pub(super) projection_modal: ProjectionModal,
    context_snapshot: Watch<ContextSnapshot>,
    tools_snapshot: Watch<ContextSnapshot>,
    tools_basis: String,
    tools_preview_cache: ToolsPreviewCache,
    pub(crate) tools_preview_source: Option<Arc<ToolsPreviewSource>>,
    /// Read from the ledger when the modal asks for it, not on every frame:
    /// the table outlives sessions and only grows.
    pub(super) lifetime_usage: Option<LifetimeUsage>,
    /// Cache-warming totals for the scope they were read for, re-read when
    /// the modal opens, switches scope or reloads.
    pub(super) warming_summary: Option<(UsageScope, WarmingSummary)>,
    /// Recorded tool activity for the scope `/tools` is showing. `None` means
    /// not loaded yet or unreadable; the modal tells those apart by scope.
    pub(super) tool_stats: Option<ToolStats>,
    pub(super) goal_panel: GoalPanel,
    pub(super) autopilot_panel: AutopilotPanel,
    pub(super) stream_modal: StreamModal,
    pub(super) float_mgr: FloatManager,
    pub(super) search_modal: SearchModal,
    history_search: search::HistorySearchRun,
    pub(super) file_picker: FilePickerModal,
    pub(super) mention_popup: MentionPopup,
    pub(super) commit_popup: CommitPopup,
    commit_reload: CommitReload,
    pub(super) paste_editor: PasteEditor,
    pub(super) image_viewer: ImageViewer,
    pub(super) permission_prompt: PermissionPrompt,
    pub(super) permissions_picker: PermissionsPicker,
    pub(super) sandbox_manager: SandboxManager,
    sandbox_reply: Option<Receiver<StoreReply>>,
    pub(crate) sandbox_live: SandboxWorkers,
    permission_ui: permission_editor::PermissionUi,
    parked_workbench: Option<Workbench>,
    permission_config_trust_deferred: bool,
    /// Whether project permission config awaits trust, cached because asking
    /// opens the trust store. `None` until the next frame asks again.
    permission_review: Option<bool>,
    pub(super) memory_inspector: MemoryInspector,
    memory_reads: memory::MemoryReads,
    pub(super) learn_inspector: LearnInspector,
    pub(super) learn_transcripts: LearnTranscripts,
    learn_reads: learning::LearnReads,
    /// The session's continual learner, while one runs.
    pub(crate) learner: Option<Arc<ArcSwapOption<LearnerHandle>>>,
    /// Set by the event loop after construction: whether a summarizer writes
    /// the memory's lines, which the inspector says when it does not.
    pub(crate) summarize_memory: bool,
    /// Set by the event loop: `builtin_skills.packs()`, for the commands a remote `cd` rediscovers.
    pub(crate) skill_packs: SkillPacks,
    pub(super) task_picker: TaskPicker,
    task_picker_inputs: Option<tasks::PickerInputs>,
    pub(super) shell_modal: ShellModal,
    pub(crate) peer_manager: PeerManager,
    pub(super) workflow_inspector: WorkflowInspector,
    /// The run a transcript was opened from, so reopening the inspector
    /// returns to it rather than to whichever run is newest.
    pub(super) workflow_return: Option<String>,
    pub(super) workflow_catalog_picker: WorkflowCatalogPicker,
    pub(crate) workflow: workflow::WorkflowUi,
    pub(crate) automation: automation::AutomationLink,
    pub(super) automation_inspector: AutomationInspector,
    /// The docked question form, whichever kind of ask it is.
    pub(super) question_form: QuestionForm,
    /// Deferred questions off the dock, and an ask held behind one.
    questions: questions::QuestionQueue,
    pub(super) session_picker: SessionPicker,
    pub(super) session_relocation_picker: SessionRelocationPicker,
    pub(super) worktree_picker: WorktreePicker,
    /// Published by the event loop, which is the only thing that can see
    /// sibling sessions. Polled while the picker is open.
    pub(crate) live_sessions: Arc<ArcSwap<Vec<SessionRow>>>,
    live_session_watch: Watch<Vec<SessionRow>>,
    /// Set by the event loop after construction, like `live_sessions`: one
    /// notice for every session, so dismissing it anywhere hides it everywhere.
    pub(crate) update_notice: Arc<UpdateNotice>,
    update_banner: Watch<Available>,
    /// Set by the event loop after construction, like `update_notice`: the
    /// config files that failed to load, which every session reports.
    pub(crate) config_problems: Arc<ConfigProblems>,
    /// The banner's close control as last drawn, `None` while it is not.
    pub(super) update_close: Option<Rect>,
    /// The banner's text as last drawn, which opens the update prompt.
    pub(super) update_text: Option<Rect>,
    /// The picker's other half comes from a disk query, which no publisher
    /// covers; see [`StorageWriter::generation`].
    stored_session_generation: u64,
    question_subagent: Option<String>,
    pub(super) stash_picker: StashPicker,
    pub(super) plan_form: PlanForm,
    pub(super) todo_panel: TodoPanel,
    pub(crate) workbench: Workbench,
    pub(super) workbench_theme_gen: u64,
    /// The picture generation the workbench last painted, so a decode that
    /// lands while an image tab is showing repaints it.
    pub(super) workbench_image_gen: u64,
    /// The layout last read or written, so a tick only reaches storage when
    /// something actually moved. `None` until the workbench first opens.
    pub(super) workbench_layout: Option<WorkbenchLayout>,
    /// What the workbench's Caudra section was last pointed at.
    workbench_mounts: Option<MountScope>,
    /// The plans and notes from the local document store that workbench tabs
    /// hold, by the key of each one's tab.
    stored_documents: HashMap<DocumentKey, StoredDocument>,
    pub(super) status_bar: StatusBar,
    pub(super) status_hits: Vec<StatusBarHit>,
    pub(super) status_mouse_down: Option<StatusBarHit>,
    pub(super) status_hover: Option<StatusBarHitTarget>,
    pub(super) queue_hits: Vec<QueueHit>,
    pub(super) queue_mouse_down: Option<QueueHit>,
    pub(super) queue_hover: Option<QueueHitTarget>,
    pub(super) queue_cut_rows: Vec<TipArea>,
    pub(super) admission_hits: Vec<AdmissionHit>,
    pub(super) admission_mouse_down: Option<AdmissionHit>,
    pub(super) admission_hover: Option<PromptAdmission>,
    /// `None` whenever no chord hint is on screen, so the pointer cannot land
    /// on a hint that a higher-priority one replaced. A press and a hover are
    /// tagged with the hint they were taken against, so neither can act on a
    /// different hint that takes the row before the release or the next frame.
    pub(super) chord_hint_hit: Option<ChordHintHit>,
    pub(super) chord_hint_down: Option<ChordHint>,
    pub(super) chord_hint_hover: Option<ChordHint>,
    pub(super) message_action_mouse_down: Option<MessageActionTarget>,
    pub(super) link_mouse_down: Option<Arc<str>>,
    pub(super) mention_mouse_down: Option<Mention>,
    pub(super) commit_mouse_down: Option<CommitRef>,
    pub(super) key_focus: KeyFocus,
    pub status: Status,
    pub(crate) state: session_state::SessionState,
    pub exit_request: ExitRequest,
    pub(crate) exit_on_done: bool,
    pub(crate) background: Option<BackgroundTasks>,
    pub(crate) task_host: Option<Arc<WorkflowHostContext>>,
    task_continuation: Option<tasks::TaskContinuation>,
    /// Work Caudra stopping cut off, until it is resumed or a new turn
    /// supersedes it.
    interrupted: Option<caudra_agent::restart::Interruption>,
    /// Resume [`Self::interrupted`] once the session settles.
    resume_due: bool,
    /// The reminder a reopened main chat reads with its next run.
    resume_preamble: Option<caudra_providers::Message>,
    /// The foreground shell tracker's last snapshot, polled every tick for the
    /// footer chip and the `/shells` modal.
    shell_snapshot: Watch<ShellSnapshot>,
    task_interactions: tasks::TaskInteractions,
    task_controls: tasks::TaskControls,
    task_history: task_history::TaskHistory,
    pub(crate) background_claims: Vec<Message>,
    background_saved_revision: Option<u64>,
    pub(crate) background_delivery: background_delivery::BackgroundDelivery,
    pub(crate) automatic_wakes_suppressed: bool,
    /// The site that suppressed automatic wakes, kept with the session.
    pub(crate) wakes_suppressed_reason: Option<String>,
    pub(crate) queue: MessageQueue,
    queue_editor: Option<queue::QueueEditor>,
    task_queue_selection: Option<(String, QueueItemId)>,
    task_queue_viewport: usize,
    recoverable_queue: Vec<crate::agent::shared_queue::PendingPrompt>,
    recoverable_queue_together: bool,
    pub answer_tx: Option<flume::Sender<String>>,
    pub(crate) cmd_tx: Option<flume::Sender<super::AgentCommand>>,
    pub(super) pending_input: PendingInput,
    pub(crate) run_id: u64,
    pub(crate) cancelling_run: Option<u64>,
    replacement_item: Option<QueueItemId>,
    /// A goal set with "stop work" waits for the stopped run to end before
    /// its first run may start.
    goal_replaces_run: bool,
    goal_deferred: bool,
    /// A resumed engaged session owes its goal one check-in once it settles.
    autopilot_checkin_due: bool,
    /// An engagement started or resumed is owed a check of what it runs without.
    autopilot_check_due: bool,
    /// The implement handoff autopilot chose, with the run that chose it. It
    /// starts once that run is done, and any other run's end drops it.
    autopilot_implement: Option<(u64, u32)>,
    /// Decisions autopilot announced and has not made yet, by seq. A
    /// permission card carries its own state, so none is kept here.
    autopilot_deciding: Vec<(u32, DecisionKind)>,
    /// The seq of the goal check last reported, so the row a goal's end
    /// draws carries the verdict that ended it.
    goal_verdict_seq: Option<u64>,
    /// The plan decision autopilot left to the user, settled by how they
    /// answer the card and forgotten when it closes otherwise.
    autopilot_plan_escalation: Option<u32>,
    /// The end of an engagement, for the terminal to announce.
    autopilot_notice: Option<Notification>,
    pub(super) zones: ZoneRegistry,
    #[cfg(debug_assertions)]
    grab: grab::GrabState,
    pub(super) selection_state: Option<SelectionState>,
    /// Velocity scrolling anchored on a middle press, which reaches the far end
    /// of a long document without a bar and without a wheel.
    pub(super) autoscroll: Option<Autoscroll>,
    pub(super) clipboard: ClipboardState,
    pub(super) last_esc: Option<Instant>,
    pub(super) last_exit: Option<Instant>,

    pub(crate) storage: StateDir,
    pub(crate) workspace_session: Option<caudra_workspace::WorkspaceSession>,
    pub(crate) remote_project_context:
        Option<Arc<caudra_agent::remote_project_context::RemoteProjectContext>>,
    pub(crate) local_documents: Option<Arc<caudra_storage::local_documents::LocalDocumentStore>>,
    /// Set by the event loop after construction, like `live_sessions`.
    pub(crate) change_factory: Option<crate::ChangeServiceFactory>,
    /// The local store's handle for this session's directory. A remote
    /// session binds one per use instead.
    pub(crate) local_changes: Option<Arc<dyn WorkspaceChangeService>>,
    /// Shared with this session's agents, which read it when a run starts.
    pub(crate) change_recorder: file_revert::RecorderSlot,
    /// The session's records as last read, or why there are none to revert.
    pub(crate) record_index: Option<Result<file_revert::RecordIndex, String>>,
    pub(crate) record_index_refresh: Option<Task<Result<file_revert::RecordIndex, String>>>,
    revert_confirmation: Option<file_revert::RevertConfirmation>,
    /// Set by the event loop after construction, like `live_sessions`.
    pub(crate) snapshots_config: SnapshotsConfig,
    pub(crate) usage_slot: Arc<ArcSwapOption<UsageFetchState>>,
    pub(crate) storage_slot: Arc<ArcSwapOption<StorageFetchState>>,
    decisions_slot: Arc<ArcSwapOption<DecisionsFetchState>>,
    pub(crate) shared_history: Option<SharedHistory>,
    pub(crate) btw_prompt: Option<crate::agent::SharedBtwPrompt>,
    /// Every finished task's transcript, which a `/btw` over a task reads once
    /// the task is no longer open.
    pub(crate) subagent_history: Option<SubagentHistoryStore>,
    /// The `/btw` thread behind the stream modal while it is open; `/extract`
    /// shares the modal and never has one.
    pub(super) btw_thread: Option<btw::BtwThread>,
    /// Threads whose modal was closed, by the task whose conversation each
    /// reads (`None` for the main one), until cleared or the session changes.
    pub(super) parked_btw: HashMap<Option<String>, btw::ParkedBtw>,
    pub(crate) context_store: Option<ContextStore>,
    pub(crate) effective_model_slot: Option<Arc<ArcSwap<ModelSlot>>>,
    /// The main run's picks and backoff wake, while an agent loop serves it.
    pub(crate) main_control: Option<caudra_agent::ConversationControl>,
    /// Each running task's picks and backoff wake, by task id.
    pub(crate) subagent_controls: HashMap<String, caudra_agent::ConversationControl>,
    /// The task the spent-account card came up for, `None` for the main chat.
    exhausted_task: Option<String>,
    pub(crate) execution_mode: Option<SharedMode>,
    pub(crate) image_paste_rx: Vec<flume::Receiver<Result<ImageSource, String>>>,
    storage_writer: Arc<StorageWriter>,
    last_sent: Option<Sent>,
    merged_history: Option<MergedHistory>,
    pub(crate) shell: shell::ShellState,
    pub(crate) ui_config: UiConfig,
    /// The experiments this process started with, which decide what the UI
    /// offers and what an explicit request may open.
    pub(crate) features: FeatureFlags,
    pub(crate) permissions: Arc<PermissionManager>,
    pub(crate) conversation_permissions: ConversationPermissions,
    pub(crate) permission_authority_factory: Option<crate::PermissionAuthorityFactory>,
    pattern_suggestion_loader: Option<PatternSuggestionLoader>,
    pending_pattern_suggestions: Option<PendingPatternSuggestions>,
    pattern_discovery_context: Option<(PathBuf, u64, Weak<PermissionManager>)>,
    pub(crate) model_policy: Arc<ModelPolicy>,
    pub(crate) lua_event_handle: EventHandle,
    pub(super) keymap_reader: KeymapReader,
    pub(super) hint_reader: HintReader,
    hints: Watch<HintSnapshot>,
    pub(crate) restore_event_tx: Option<caudra_agent::EventSender>,
    pub(super) restoring: Arc<AtomicBool>,
    subagent_answers: HashMap<String, flume::Sender<String>>,
    subagent_steers: HashMap<String, SteeringQueue>,
    pending_subagent_steers: HashMap<String, VecDeque<PendingSteer>>,
    /// Each running task's waiting replacement, the task chat's
    /// counterpart of `replacement_item`.
    subagent_replacements: HashMap<String, QueueItemId>,
    unsent_subagent_steers: HashMap<String, VecDeque<PendingSteer>>,
    parent_task_ids: HashMap<String, String>,
    /// Chats opened from a delegation whose call has not run yet, by the
    /// `parent_tool_use_id` their subagent is predicted to publish under. They
    /// stay out of `chat_index`, which routes events for subagents that exist.
    pending_delegations: HashSet<String>,
    /// How much of each card is open, shared by every chat and pushed at
    /// render time.
    pub(crate) view: ViewMode,
}

struct PendingSteer {
    id: QueueItemId,
    text: String,
    draft: InputDraft,
    /// Sent by the agent, whose task hands it back if never read, so the
    /// user is never asked to resend it.
    from_agent: bool,
}

struct PendingPatternSuggestions {
    project: PathBuf,
    revision: u64,
    owner: Weak<PermissionManager>,
    reply: Receiver<Arc<PatternDiscoveryOutcome>>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: &Model,
        session: AppSession,
        storage: StateDir,
        available_models: Arc<ArcSwapOption<Vec<String>>>,
        mcp_reader: McpSnapshotReader,
        mcp_config_errors: McpConfigErrors,
        lua_command_reader: LuaCommandReader,
        keymap_reader: KeymapReader,
        hint_reader: HintReader,
        storage_writer: Arc<StorageWriter>,
        ui_config: UiConfig,
        input_history_size: usize,
        max_log_files: u32,
        docs: DocsLibrary,
        release_notes: ReleaseNotesSource,
        permissions: Arc<PermissionManager>,
        custom_commands: Arc<[caudra_agent::command::CustomCommand]>,
        lua_event_handle: EventHandle,
        model_policy: Arc<ModelPolicy>,
        prompt_profiles: Arc<PromptProfileCatalog>,
        workspace_session: Option<caudra_workspace::WorkspaceSession>,
        features: FeatureFlags,
    ) -> Self {
        scrollbar::set_enabled(ui_config.scrollbar);
        inline_image::set_media_store(MediaStore::new(&storage));
        let state = SessionState::from_session(
            session,
            model,
            &storage,
            &model_policy,
            ui_config.default_mode.into(),
        );
        let view = caudra_storage::view::read(&storage).unwrap_or_default();
        let typewriter = ui_config.typewriter_ms_per_char;
        let flash = ui_config.flash_duration();
        let status_bar = StatusBar::new(flash, &state.session.cwd, workspace_session.is_some());
        let mut input_box = InputBox::new(
            InputHistory::load(&storage, input_history_size),
            ui_config.max_input_lines,
        );
        input_box.set_mcp_reader(mcp_reader.clone());
        let mut mention_popup = MentionPopup::new();
        mention_popup.set_mcp_reader(mcp_reader.clone());
        let subagent_input_box = InputBox::new(InputHistory::default(), ui_config.max_input_lines);
        let mut app = Self {
            chats: vec![Chat::new(
                "Main".into(),
                Path::new(&state.session.cwd),
                ui_config.clone(),
                lua_event_handle.clone(),
            )],
            active_chat: 0,
            chat_index: HashMap::new(),
            input_box,
            subagent_input_box,
            subagent_input_task: None,
            subagent_drafts: HashMap::new(),
            command_palette: CommandPalette::new(
                custom_commands,
                mcp_reader.clone(),
                lua_command_reader,
                features,
            )
            .with_lua_handle(lua_event_handle.clone()),
            no_commands: false,
            command_modal: CommandModal::new(),
            theme_picker: ThemePicker::new(),
            thinking_picker: ThinkingPicker::new(),
            accounts_modal: AccountsModal::new(),
            routed_account: None,
            accounts_refresh: Arc::new(ArcSwapOption::empty()),
            usage_accounts: Vec::new(),
            prompt_profile_picker: PromptProfilePicker::new(Arc::clone(&prompt_profiles)),
            model_picker: ModelPicker::new(available_models).with_decision_source({
                let storage = storage.clone();
                move || decision_choices(&ProvidersConfig::load(), &storage)
            }),
            login_picker: LoginPicker::new(),
            mcp_picker: McpPicker::new(mcp_reader, mcp_config_errors),
            env_modal: EnvModal::new(),
            shell_env: ShellEnvLink::default(),
            settings_modal: SettingsModal::new(),
            settings: SettingsLink::default(),
            rewind_picker: RewindPicker::new(),
            message_actions: MessageActions::new(),
            queue_actions: QueueActions::new(),
            mode_submission: ModeSubmission::new(),
            update_prompt: UpdatePrompt::new(),
            pending_plan_submission: None,
            review: ReviewModal::new(),
            help_modal: HelpModal::new(),
            which_key: WhichKey::new(ui_config.which_key_delay()),
            tooltip: Tooltip::new(ui_config.tooltips),
            usage_modal: UsageModal::new(),
            context_modal: ContextModal::new(),
            logs_modal: LogsModal::new(max_log_files),
            docs_modal: DocsModal::new(docs),
            docs_badge: None,
            changelog_modal: ChangelogModal::new(release_notes, update::CURRENT),
            tools_modal: ToolsModal::new(),
            decisions_modal: DecisionsModal::new(),
            skills_modal: SkillsModal::new(),
            storage_modal: StorageModal::new(),
            system_prompt_modal: SystemPromptModal::default(),
            projection_modal: ProjectionModal::default(),
            context_snapshot: Watch::default(),
            tools_snapshot: Watch::default(),
            tools_basis: String::new(),
            tools_preview_cache: ToolsPreviewCache::default(),
            tools_preview_source: None,
            lifetime_usage: None,
            warming_summary: None,
            tool_stats: None,
            goal_panel: GoalPanel::default(),
            autopilot_panel: AutopilotPanel::default(),
            stream_modal: StreamModal::new(typewriter),
            float_mgr: FloatManager::new(),
            search_modal: SearchModal::new(),
            history_search: search::HistorySearchRun::default(),
            file_picker: FilePickerModal::new(),
            mention_popup,
            commit_popup: CommitPopup::new(),
            commit_reload: CommitReload::Idle,
            paste_editor: PasteEditor::new(),
            image_viewer: ImageViewer::default(),
            permission_prompt: PermissionPrompt::new(),
            permissions_picker: PermissionsPicker::new(),
            sandbox_manager: SandboxManager::default(),
            sandbox_reply: None,
            sandbox_live: SandboxWorkers::default(),
            permission_ui: permission_editor::PermissionUi::default(),
            parked_workbench: None,
            permission_config_trust_deferred: false,
            permission_review: None,
            memory_inspector: MemoryInspector::new(),
            memory_reads: memory::MemoryReads::default(),
            learn_inspector: LearnInspector::new(),
            learn_transcripts: LearnTranscripts::default(),
            learn_reads: learning::LearnReads::default(),
            learner: None,
            summarize_memory: true,
            skill_packs: SkillPacks::default(),
            task_picker: TaskPicker::new(),
            task_picker_inputs: None,
            shell_modal: ShellModal::new(),
            peer_manager: PeerManager::new(),
            workflow_inspector: WorkflowInspector::new(),
            workflow_return: None,
            workflow_catalog_picker: WorkflowCatalogPicker::new(),
            workflow: workflow::WorkflowUi::new(),
            automation: automation::AutomationLink::default(),
            automation_inspector: AutomationInspector::new(),
            question_form: QuestionForm::new(),
            questions: questions::QuestionQueue::default(),
            session_picker: SessionPicker::new(),
            session_relocation_picker: SessionRelocationPicker::new(),
            worktree_picker: WorktreePicker::new(),
            live_sessions: Arc::default(),
            live_session_watch: Watch::default(),
            update_notice: Arc::default(),
            update_banner: Watch::default(),
            config_problems: Arc::default(),
            update_close: None,
            update_text: None,
            stored_session_generation: 0,
            question_subagent: None,
            stash_picker: StashPicker::new(),
            plan_form: PlanForm::new(),
            todo_panel: TodoPanel::default(),
            workbench: Workbench::new(workbench_styles()),
            workbench_theme_gen: crate::theme::generation(),
            workbench_image_gen: 0,
            workbench_layout: None,
            workbench_mounts: None,
            stored_documents: HashMap::new(),
            status_bar,
            status_hits: Vec::new(),
            status_mouse_down: None,
            status_hover: None,
            queue_hits: Vec::new(),
            queue_mouse_down: None,
            queue_hover: None,
            queue_cut_rows: Vec::new(),
            admission_hits: Vec::new(),
            admission_mouse_down: None,
            admission_hover: None,
            chord_hint_hit: None,
            chord_hint_down: None,
            chord_hint_hover: None,
            message_action_mouse_down: None,
            link_mouse_down: None,
            mention_mouse_down: None,
            commit_mouse_down: None,
            key_focus: KeyFocus::Composer,
            status: Status::Idle,
            automatic_wakes_suppressed: tasks::restored_wake_suppression(&state.session.meta).0,
            wakes_suppressed_reason: tasks::restored_wake_suppression(&state.session.meta).1,
            state,
            exit_request: ExitRequest::None,
            exit_on_done: false,
            background: None,
            task_host: None,
            task_continuation: None,
            interrupted: None,
            resume_due: false,
            resume_preamble: None,
            shell_snapshot: Watch::default(),
            task_interactions: tasks::TaskInteractions::default(),
            task_controls: tasks::TaskControls::default(),
            task_history: task_history::TaskHistory::default(),
            background_claims: Vec::new(),
            background_saved_revision: None,
            background_delivery: background_delivery::BackgroundDelivery::default(),
            queue: MessageQueue::default(),
            queue_editor: None,
            task_queue_selection: None,
            task_queue_viewport: 0,
            recoverable_queue: Vec::new(),
            recoverable_queue_together: false,
            answer_tx: None,
            cmd_tx: None,
            pending_input: PendingInput::None,
            run_id: 0,
            cancelling_run: None,
            replacement_item: None,
            goal_replaces_run: false,
            goal_deferred: false,
            autopilot_checkin_due: false,
            autopilot_check_due: false,
            autopilot_implement: None,
            autopilot_deciding: Vec::new(),
            goal_verdict_seq: None,
            autopilot_plan_escalation: None,
            autopilot_notice: None,
            zones: ZoneRegistry::new(),
            #[cfg(debug_assertions)]
            grab: grab::GrabState::default(),
            selection_state: None,
            autoscroll: None,
            clipboard: ClipboardState::new(),
            last_esc: None,
            last_exit: None,
            storage,
            workspace_session: workspace_session.clone(),
            remote_project_context: None,
            local_documents: None,
            change_factory: None,
            local_changes: None,
            change_recorder: file_revert::RecorderSlot::default(),
            record_index: None,
            record_index_refresh: None,
            revert_confirmation: None,
            snapshots_config: SnapshotsConfig::default(),
            usage_slot: Arc::new(ArcSwapOption::empty()),
            storage_slot: Arc::new(ArcSwapOption::empty()),
            decisions_slot: Arc::new(ArcSwapOption::empty()),
            shared_history: None,
            btw_prompt: None,
            subagent_history: None,
            btw_thread: None,
            parked_btw: HashMap::new(),
            context_store: None,
            effective_model_slot: None,
            main_control: None,
            subagent_controls: HashMap::new(),
            exhausted_task: None,
            execution_mode: None,
            image_paste_rx: vec![],
            storage_writer,
            last_sent: None,
            merged_history: None,
            shell: shell::ShellState::default(),
            ui_config,
            features,
            permissions,
            conversation_permissions: ConversationPermissions::Detached,
            permission_authority_factory: None,
            pattern_suggestion_loader: None,
            pending_pattern_suggestions: None,
            pattern_discovery_context: None,
            model_policy: Arc::clone(&model_policy),
            lua_event_handle,
            hints: Watch::seeded(hint_reader.load_full()),
            keymap_reader,
            hint_reader,
            restore_event_tx: None,
            restoring: Arc::new(AtomicBool::new(false)),
            subagent_answers: HashMap::new(),
            subagent_steers: HashMap::new(),
            pending_subagent_steers: HashMap::new(),
            subagent_replacements: HashMap::new(),
            unsent_subagent_steers: HashMap::new(),
            parent_task_ids: HashMap::new(),
            pending_delegations: HashSet::new(),
            view,
        };
        let recents = app.allowed_models(caudra_storage::model::read_recents(&app.storage));
        app.model_picker.set_recents(recents);
        let favorites = app.allowed_models(caudra_storage::model::read_favorites(&app.storage));
        app.model_picker.set_favorites(favorites);
        app.chats[0].context_window = app.state.model.context_window;
        app.sync_composer_cwd();
        app.workbench.set_markdown_painter(paint_markdown);
        app.workbench.set_image_painter(paint_image);
        if let Some(workspace) = workspace_session
            && let Err(error) = app.workbench.bind_workspace(workspace)
        {
            tracing::warn!(%error, "remote workbench backend initialization failed");
        }
        app
    }

    pub(crate) fn set_pattern_suggestion_loader(
        &mut self,
        loader: Option<PatternSuggestionLoader>,
    ) {
        self.pending_pattern_suggestions = None;
        self.pattern_suggestion_loader = loader;
        self.request_pattern_suggestions();
    }

    fn can_load_pattern_suggestions(&self) -> bool {
        !self.storage.is_ephemeral()
            && self.workspace_session.is_none()
            && self.remote_project_context.is_none()
            && self
                .state
                .session
                .workspace_binding()
                .is_none_or(|binding| binding.is_local())
    }

    pub(crate) fn request_pattern_suggestions(&mut self) {
        self.load_pattern_suggestions(PatternDiscoveryMode::Cached);
    }

    fn sync_pattern_discovery_context(&mut self) -> bool {
        let (project, revision) = self.permissions.pattern_candidate_context();
        let owner = Arc::downgrade(&self.permissions);
        if self
            .pattern_discovery_context
            .as_ref()
            .is_some_and(|(previous, version, manager)| {
                previous == &project && *version == revision && manager.ptr_eq(&owner)
            })
        {
            return false;
        }
        self.pending_pattern_suggestions = None;
        if self.pattern_discovery_context.is_some() {
            self.suspend_permission_editor();
        }
        self.pattern_discovery_context = Some((project, revision, owner));
        self.permissions_picker.set_discovery(DiscoveryState::Idle);
        self.refresh_permission_suggestions();
        true
    }

    fn discovery_unavailable(&mut self, reason: &'static str) {
        self.permissions_picker
            .set_discovery(DiscoveryState::Complete(Arc::new(
                PatternDiscoveryOutcome::Unavailable(reason),
            )));
    }

    fn load_pattern_suggestions(&mut self, mode: PatternDiscoveryMode) {
        self.sync_pattern_discovery_context();
        if mode == PatternDiscoveryMode::Cached && self.permissions_picker.discovery_cancelled() {
            return;
        }
        if !self.can_load_pattern_suggestions() {
            self.pending_pattern_suggestions = None;
            self.discovery_unavailable(PATTERN_DISCOVERY_DISABLED);
            return;
        }
        let Some(loader) = &self.pattern_suggestion_loader else {
            self.discovery_unavailable(PATTERN_WORKER_UNAVAILABLE);
            return;
        };
        let (project, revision) = self.permissions.pattern_candidate_context();
        let owner = Arc::downgrade(&self.permissions);
        if self
            .pending_pattern_suggestions
            .as_ref()
            .is_some_and(|pending| {
                pending.project == project
                    && pending.revision == revision
                    && pending.owner.ptr_eq(&owner)
            })
        {
            return;
        }
        self.pending_pattern_suggestions = Some(PendingPatternSuggestions {
            reply: loader(project.clone(), mode),
            project,
            revision,
            owner,
        });
        self.permissions_picker
            .set_discovery(DiscoveryState::Loading);
    }

    fn poll_pattern_suggestions(&mut self) -> Dirty {
        let was_open = self.permissions_picker.is_open();
        let changed = self.sync_pattern_discovery_context();
        let Some(pending) = &self.pending_pattern_suggestions else {
            return Dirty::from(changed && was_open);
        };
        if !self.can_load_pattern_suggestions()
            || !pending.owner.ptr_eq(&Arc::downgrade(&self.permissions))
        {
            self.pending_pattern_suggestions = None;
            self.discovery_unavailable(PATTERN_DISCOVERY_DISABLED);
            return Dirty::from(self.permissions_picker.is_open());
        }
        let (project, revision) = self.permissions.pattern_candidate_context();
        if project != pending.project || revision != pending.revision {
            self.pending_pattern_suggestions = None;
            return Dirty::NO;
        }
        match pending.reply.try_recv() {
            Ok(outcome) => {
                let installed = match outcome.as_ref() {
                    PatternDiscoveryOutcome::Ready(report) => {
                        self.permissions.set_pattern_candidates_for_project(
                            &pending.project,
                            pending.revision,
                            report.candidates.to_vec(),
                        )
                    }
                    PatternDiscoveryOutcome::Unavailable(_) => Ok(()),
                };
                self.pending_pattern_suggestions = None;
                if installed.is_err() {
                    tracing::debug!(
                        "discarded history pattern suggestions for stale or invalid context"
                    );
                    self.discovery_unavailable(PATTERN_INVALID_CONTEXT);
                    return Dirty::YES;
                }
                self.permissions_picker
                    .set_discovery(DiscoveryState::Complete(outcome));
                self.refresh_permission_suggestions();
                return Dirty::YES;
            }
            Err(TryRecvError::Disconnected) => {
                self.pending_pattern_suggestions = None;
                self.discovery_unavailable(PATTERN_WORKER_DISCONNECTED);
                return Dirty::YES;
            }
            Err(TryRecvError::Empty) => {}
        }
        Dirty::NO
    }

    pub(crate) fn main_chat(&mut self) -> &mut Chat {
        &mut self.chats[0]
    }

    fn is_main_chat(&self) -> bool {
        self.active_chat == 0
    }

    fn active_subagent_id(&self) -> Option<&str> {
        self.chats
            .get(self.active_chat)?
            .task_id()
            .map(|id| id.as_ref())
    }

    pub(super) fn active_context_snapshot(&self) -> Option<Arc<ContextSnapshot>> {
        if let Some(task_id) = self.active_subagent_id() {
            return self
                .context_store
                .as_ref()?
                .latest(&ContextKey::task(task_id));
        }
        self.main_context_snapshot()
    }

    /// The main chat's snapshot, whatever chat is focused, and only while it
    /// still describes the model in force.
    pub(super) fn main_context_snapshot(&self) -> Option<Arc<ContextSnapshot>> {
        let snapshot = self.context_store.as_ref()?.latest(&ContextKey::Main)?;
        if snapshot.mode != self.execution_agent_mode() || snapshot.audience != ToolAudience::MAIN {
            return None;
        }
        if let Some(model_slot) = self.status_main_model() {
            return (snapshot.model.spec == model_slot.model.spec()
                && snapshot.window.tokens == model_slot.model.context_window)
                .then_some(snapshot);
        }
        let expected = self.route_model(self.state.applied_mode)?;
        (snapshot.model.spec == expected.spec()
            && snapshot.window.tokens == expected.context_window)
            .then_some(snapshot)
    }

    /// The model a request in `mode` runs on, given the current selection.
    pub(super) fn route_model(&self, mode: Mode) -> Option<Cow<'_, Model>> {
        match mode {
            Mode::Build => Some(Cow::Borrowed(&self.state.model)),
            Mode::Plan => Model::resolve_if_available(
                ModelPurpose::Plan,
                &self.state.model,
                &self.model_policy,
            )
            .ok()
            .map(Cow::Owned),
        }
    }

    fn status_main_model(&self) -> Option<Arc<ModelSlot>> {
        if matches!(self.status, Status::Streaming) {
            return self
                .effective_model_slot
                .as_ref()
                .map(|slot| slot.load_full());
        }
        if self.state.applied_mode != Mode::Plan {
            return None;
        }
        let snapshot = self.context_store.as_ref()?.latest(&ContextKey::Main)?;
        let expected = self.route_model(Mode::Plan)?;
        self.effective_model_slot
            .as_ref()
            .map(|slot| slot.load_full())
            .filter(|slot| {
                slot.model.spec() == expected.spec()
                    && snapshot.model.spec == slot.model.spec()
                    && snapshot.window.tokens == slot.model.context_window
            })
    }

    /// Tokens in use and the window they fill, counted the way the status bar
    /// counts them for `chat`.
    fn context_usage(
        &self,
        chat: &Chat,
        effective_model: Option<&ModelSlot>,
        snapshot: Option<&ContextSnapshot>,
    ) -> (u32, u32) {
        let window = match effective_model {
            Some(slot) => slot.model.context_window,
            None if chat.context_window > 0 => chat.context_window,
            None => self.state.model.context_window,
        };
        (
            snapshot.map_or(chat.context_size, ContextSnapshot::used),
            window,
        )
    }

    /// The session as a terminal host shows it: its title, the model the
    /// main chat runs on, and how full that model's window is. Each host
    /// takes only what it discloses.
    pub(crate) fn pane_metadata(&self) -> PaneMetadata<'_> {
        let chat = &self.chats[0];
        let effective_model = self.status_main_model();
        let snapshot = self.main_context_snapshot();
        let (used, window) =
            self.context_usage(chat, effective_model.as_deref(), snapshot.as_deref());
        PaneMetadata {
            title: Cow::Borrowed(&self.state.session.title),
            model: match effective_model {
                Some(slot) => Cow::Owned(model_leaf(&slot.model.spec()).to_owned()),
                None => Cow::Borrowed(model_leaf(
                    chat.model_id
                        .as_deref()
                        .unwrap_or(&self.state.session.model),
                )),
            },
            context_percent: (window > 0).then(|| context_share(used, window)),
            session_id: None,
            subagents: self.running_subagents(),
        }
    }

    fn active_subagent_has_composer(&self) -> bool {
        self.active_subagent_id().is_some_and(|id| {
            self.subagent_steers.contains_key(id)
                || self.task_continuation_target(id).is_some()
                || self
                    .task_continuation
                    .as_ref()
                    .is_some_and(|pending| pending.task_id() == id)
        })
    }

    fn active_input_box(&self) -> &InputBox {
        if self.is_main_chat() {
            &self.input_box
        } else {
            &self.subagent_input_box
        }
    }

    fn active_input_box_mut(&mut self) -> &mut InputBox {
        if self.is_main_chat() {
            &mut self.input_box
        } else {
            &mut self.subagent_input_box
        }
    }

    /// The caret moved inside the composer, which changes the command word
    /// the slash palette is completing.
    fn resync_command_palette(&mut self) {
        let text = self.active_input_box().palette_text();
        self.sync_command_palette(&text);
    }

    /// Hands the log window to everything that has to agree about which `#`
    /// hashes are commits: the two composers decide what to highlight and send,
    /// and every transcript decides what is clickable.
    pub(super) fn set_commit_index(&mut self, index: CommitIndex) {
        self.input_box.set_commit_index(index.clone());
        self.subagent_input_box.set_commit_index(index.clone());
        for chat in &mut self.chats {
            chat.set_commit_index(index.clone());
        }
    }

    /// Re-reads the log behind the `#` the reader just typed, so a commit made
    /// during the session can be mentioned without leaving it. A window already
    /// held stays on screen while this runs: stale rows beat a spinner.
    fn reload_commit_index(&mut self) {
        if !matches!(self.commit_reload, CommitReload::Idle) {
            return;
        }
        if self.workspace_session.is_some() {
            // Only the workbench can walk a remote log, and it already does on
            // bind, so the composer reads its answer instead of asking twice.
            self.bound_workbench_mut().refresh_scm();
            self.commit_reload = CommitReload::Workbench;
            return;
        }
        let cwd = PathBuf::from(&self.state.session.cwd);
        let (sender, receiver) = flume::bounded(1);
        smol::spawn(async move {
            let commits = smol::unblock(move || repo::log(&cwd, repo::LOG_WINDOW)).await;
            let _ = sender.send(commits.unwrap_or_default());
        })
        .detach();
        self.commit_reload = CommitReload::Local(receiver);
    }

    /// Adopts a window once its read lands. The index is replaced wholesale, so
    /// an open popup notices the new one and re-seeds its rows.
    fn poll_commit_index(&mut self) -> Dirty {
        let commits = match &self.commit_reload {
            CommitReload::Idle => return Dirty::NO,
            CommitReload::Local(receiver) => match receiver.try_recv() {
                Ok(commits) => commits,
                Err(_) => return Dirty::NO,
            },
            CommitReload::Workbench => {
                if self.bound_workbench().scm_refreshing() {
                    return Dirty::NO;
                }
                self.bound_workbench()
                    .scm_log()
                    .iter()
                    .take(repo::LOG_WINDOW)
                    .map(|commit| repo::CommitSummary {
                        id: commit.id.clone(),
                        subject: commit.summary.clone(),
                        author: commit.author.clone(),
                        committed_unix_seconds: commit.committed,
                    })
                    .collect()
            }
        };
        self.commit_reload = CommitReload::Idle;
        self.set_commit_index(CommitIndex::loaded(commits));
        // An open popup is waiting on this. Nothing else will touch it until the
        // next keystroke, and a spinner that never becomes a list is a hang.
        self.resync_dropdowns();
        Dirty::YES
    }

    /// The workbench holding the session's binding. The permission-source editor
    /// parks it and puts a local one in its place, so the parked one is the one
    /// that can answer for the project.
    fn bound_workbench(&self) -> &Workbench {
        self.parked_workbench.as_ref().unwrap_or(&self.workbench)
    }

    fn bound_workbench_mut(&mut self) -> &mut Workbench {
        self.parked_workbench
            .as_mut()
            .unwrap_or(&mut self.workbench)
    }

    fn resync_dropdowns(&mut self) {
        let text = self.active_input_box().palette_text();
        self.sync_dropdowns(&text);
    }

    /// Every edit to the composer reaches both dropdowns through here, so the
    /// `@` popup can never be left open over text that moved on without it.
    fn sync_dropdowns(&mut self, palette_text: &str) {
        self.sync_command_palette(palette_text);
        let cwd = self.state.session.cwd.clone();
        let input = self.active_input_box();
        let (text, cursor) = (input.buffer.display_text(), input.buffer.cursor_offset());
        let index = input.commit_index().clone();
        self.mention_popup
            .sync_workspace(&text, cursor, &cwd, self.workspace_session.clone());
        let was_active = self.commit_popup.is_active();
        self.commit_popup.sync(&text, cursor, &index);
        // The `#` itself is the request for a log. Reading on the edge rather
        // than on every keystroke is what keeps the scanner free.
        if !was_active && self.commit_popup.is_active() {
            self.reload_commit_index();
        }
    }

    /// Splices a completed path over the `@query` that opened the popup. The
    /// palette is resynced but the popup is not: it has already decided whether
    /// to stay open for a directory the reader is still drilling into.
    fn complete_mention(&mut self, range: std::ops::Range<usize>, path: &str) {
        let input = self.active_input_box_mut();
        input.buffer.replace_range(range, path);
        let text = self.active_input_box().palette_text();
        self.sync_command_palette(&text);
    }

    /// `None` means the popup did not want the event, so the composer still
    /// gets its turn at it.
    fn handle_mention_action(&mut self, action: MentionAction) -> Option<Vec<Action>> {
        match action {
            MentionAction::Consumed => Some(Vec::new()),
            MentionAction::Insert { range, path } => {
                self.complete_mention(range, &path);
                Some(Vec::new())
            }
            MentionAction::Passthrough => None,
        }
    }

    fn handle_commit_action(&mut self, action: CommitAction) -> Option<Vec<Action>> {
        match action {
            CommitAction::Consumed => Some(Vec::new()),
            CommitAction::Insert { range, hash } => {
                self.complete_mention(range, &hash);
                Some(Vec::new())
            }
            CommitAction::Passthrough => None,
        }
    }

    fn sync_subagent_input_target(&mut self) {
        let next = self.active_subagent_id().map(str::to_owned);
        if self.subagent_input_task == next {
            return;
        }
        self.paste_editor.close();
        if let Some(previous) = self.subagent_input_task.take() {
            if self.subagent_input_box.is_empty() {
                self.subagent_drafts.remove(&previous);
            } else {
                self.subagent_drafts
                    .insert(previous, self.subagent_input_box.take_state());
            }
        }
        let draft = next
            .as_ref()
            .and_then(|id| self.subagent_drafts.remove(id))
            .unwrap_or_else(|| InputState::new(InputDraft::default(), Vec::new()));
        self.subagent_input_box.set_state(draft);
        self.subagent_input_task = next;
        // Runs on every chat switch, so this is the single point that stops a
        // dropdown opened over one composer from surviving into another.
        self.resync_dropdowns();
    }

    fn plan_form_active(&self) -> bool {
        self.state.mode == Mode::Plan && self.plan_form.is_visible()
    }

    fn plan_form_in_view(&self) -> bool {
        self.is_main_chat() && self.plan_form_active()
    }

    /// The form stays up while the plan is open in the workbench or a `/btw`
    /// is asked over it, but that is what the reader is looking at. A form
    /// that kept the keys from behind it would implement the plan on the Enter
    /// meant for the text.
    fn plan_form_takes_input(&self) -> bool {
        self.plan_form_in_view() && !self.workbench.is_open() && !self.stream_modal.is_open()
    }

    /// Reconciles the session to a model it was handed. Every live session is
    /// pushed through here whenever the shared slot moves, so it must not record
    /// a choice: use [`Self::select_model`] for one the user made.
    pub(crate) fn update_model(&mut self, model: &Model) {
        // A level chosen for the model being switched to outranks whatever the
        // previous model was on; a level is only meaningful against the ladder
        // that declared it. Without a choice of its own the current level
        // carries over and is snapped per request, as before.
        if let Some(stored) = caudra_storage::thinking::read(&self.storage, &model.spec()) {
            self.state.thinking = stored.into();
        }
        self.state.update_model(model);
        self.chats[0].context_window = model.context_window;
    }

    /// A model the user chose, remembered against the mode they chose it in.
    /// That is `state.mode` rather than `applied_mode`: while a switch is
    /// pending the mode on screen is the one the pick is meant for.
    pub(crate) fn select_model(&mut self, model: &Model) {
        self.update_model(model);
        persist_model(
            &self.storage,
            self.state.mode.into(),
            &self.state.session.model,
        );
        // A pick made with nothing pending is the model the next turn runs on,
        // so it becomes the baseline the status bar measures a later toggle
        // against. Leaving it stale would draw the transition from a model that
        // was already replaced.
        if self.state.applied_mode == self.state.mode {
            self.state
                .applied_model
                .clone_from(&self.state.session.model);
        }
    }

    pub(crate) fn select_setup_model(&mut self, model: &Model) {
        let thinking = self
            .state
            .thinking_explicit
            .then(|| self.state.thinking.clone());
        seed_setup_thinking(&self.storage, &model.spec());
        self.select_model(model);
        if let Some(thinking) = thinking.filter(|_| model.supports_thinking()) {
            self.state.thinking = thinking;
        }
    }

    /// The one place a chosen level lands, so every spelling of the choice
    /// reaches the next session. A level the model forces off goes through
    /// [`SessionState::update_model`] instead, which must not overwrite what
    /// the user asked for on a model that could honor it.
    fn apply_thinking(&mut self, thinking: ThinkingConfig) {
        if self.state.thinking != thinking {
            self.prefix_changed();
            self.forward_main(caudra_agent::SettingsChange::thinking(thinking.clone()));
        }
        self.state.thinking = thinking;
        self.state.thinking_explicit = true;
        caudra_storage::thinking::persist(
            &self.storage,
            &self.state.model.spec(),
            &self.state.thinking.clone().into(),
        );
    }

    /// Takes the spelling both `/thinking` and `caudra.model.set` accept; a
    /// blank {input} toggles.
    pub(crate) fn set_thinking(&mut self, input: &str) -> Result<ThinkingConfig, String> {
        if !self.state.model.supports_thinking() {
            return Err(THINKING_UNSUPPORTED_MSG.into());
        }
        let parsed =
            ThinkingConfig::parse(input.trim(), &self.state.thinking).map_err(str::to_owned)?;
        self.apply_thinking(parsed);
        Ok(self.state.thinking.clone())
    }

    /// Steps through the levels the current model declares, so the ladder is
    /// the model's own rather than a fixed list caudra offers everywhere. Wraps
    /// through `off` unless the model cannot be asked to stop.
    fn cycle_reasoning_effort(&mut self) {
        if !self.state.model.supports_thinking() {
            self.flash(THINKING_UNSUPPORTED_MSG.into());
            return;
        }
        let options = self.state.model.reasoning_options();
        let ladder = options.effort_ladder();
        let Some(first) = ladder.first() else {
            self.flash(THINKING_UNSUPPORTED_MSG.into());
            return;
        };
        let resolved = self.state.thinking.resolve(&self.state.model);
        let next = match &resolved {
            ResolvedThinking::Effort(current) => ladder
                .iter()
                .position(|level| *level == current)
                .and_then(|index| ladder.get(index + 1)),
            _ => None,
        };
        let stepped = match next {
            Some(level) => ThinkingConfig::Effort((*level).into()),
            None if self.state.thinking.is_enabled() && !self.state.model.requires_thinking() => {
                ThinkingConfig::Off
            }
            None => ThinkingConfig::Effort((*first).into()),
        };
        self.apply_thinking(stepped);
        self.thinking_picker
            .flash(&self.state.model, &self.state.thinking);
        self.flash(format!("Reasoning effort: {}", self.state.thinking));
    }

    pub(crate) fn set_fast(&mut self, fast: bool) -> Result<(), String> {
        if fast && !self.state.model.supports_fast() {
            return Err(FAST_UNSUPPORTED_MSG.into());
        }
        if self.state.fast != fast {
            self.prefix_changed();
        }
        self.state.fast = fast;
        Ok(())
    }

    /// Tells the agent its last request's cache will not be read again, so
    /// warming it would be spent for nothing.
    fn prefix_changed(&self) {
        if let Some(cmd_tx) = &self.cmd_tx {
            let _ = cmd_tx.try_send(super::AgentCommand::PrefixChanged);
        }
    }

    /// What `caudra.model.get` hands to Lua.
    pub(crate) fn model_state(&self) -> serde_json::Value {
        let model = &self.state.model;
        serde_json::json!({
            "spec": model.spec(),
            "id": model.id,
            "provider": model.provider.to_string(),
            "thinking": self.state.thinking.to_string(),
            "fast": self.state.fast,
            "supports_thinking": model.supports_thinking(),
            "supports_fast": model.supports_fast(),
        })
    }

    pub(crate) fn record_recent_model(&mut self, spec: &str) {
        let recents = self.allowed_models(caudra_storage::model::push_recent(&self.storage, spec));
        self.model_picker.set_recents(recents);
    }

    fn allowed_models(&self, specs: Vec<String>) -> Vec<String> {
        specs
            .into_iter()
            .filter(|spec| self.model_policy.allows(spec))
            .collect()
    }

    fn toggle_favorite_model(&mut self, spec: &str) {
        match model_registry::toggle_favorite_and_persist(spec, &self.storage) {
            Ok(favorites) => {
                let favorites = self.allowed_models(favorites);
                self.model_picker.set_favorites(favorites);
            }
            Err(error) => self.flash(format!("{FAVORITE_SAVE_ERR}: {error}")),
        }
    }

    /// The favorite after the running model, wrapping, or the first when the
    /// running model is not one.
    fn cycle_favorite_model(&mut self) -> Vec<Action> {
        let favorites = self.model_picker.favorites();
        let current = self.state.model.spec();
        let next = favorites
            .iter()
            .position(|spec| *spec == current)
            .map_or(0, |index| (index + 1) % favorites.len());
        let Some(spec) = favorites.get(next).cloned() else {
            self.flash(no_favorites_msg());
            return vec![];
        };
        self.flash(format!("{MODEL_SWITCH_PREFIX}{spec}"));
        vec![Action::ChangeModel(spec)]
    }

    pub(crate) fn flash(&mut self, msg: String) {
        self.status_bar.flash(msg);
    }

    pub(crate) fn fire_session_autocmd(&self, event: &str, mut data: serde_json::Value) {
        if let Some(map) = data.as_object_mut() {
            map.insert(
                "session_id".into(),
                serde_json::Value::String(self.state.session.id.to_string()),
            );
        }
        self.lua_event_handle.fire_autocmd(event, data);
    }

    pub fn tick_error_expiry(&mut self) -> Dirty {
        if !self.status.is_error_expired() {
            return Dirty::NO;
        }
        self.status = Status::Idle;
        Dirty::YES
    }

    fn active_chat(&mut self) -> &mut Chat {
        &mut self.chats[self.active_chat]
    }

    pub(crate) fn win_view(&self) -> WinView {
        self.chats[self.active_chat].win_view()
    }

    pub(crate) fn set_scroll_top(&mut self, top: u32) {
        self.active_chat().set_scroll_top(top);
    }

    fn clear_selection_unless_pending_copy(&mut self) {
        if !self
            .selection_state
            .as_ref()
            .is_some_and(|s| s.is_pending_copy())
        {
            self.selection_state = None;
        }
    }

    pub fn update(&mut self, msg: Msg) -> Vec<Action> {
        if matches!(&msg, Msg::Key(key) | Msg::KeyWithText { key, .. } if matches!(key.code, KeyCode::Modifier(_)))
        {
            return vec![];
        }
        self.sync_execution_mode();
        if crate::sandbox::transfer::active() && !matches!(msg, Msg::Agent(_)) {
            return self.transfer_input(msg);
        }
        match &msg {
            Msg::Mouse(event) if event.kind == MouseEventKind::Moved => {
                self.tooltip
                    .pointer_moved(Position::new(event.column, event.row));
            }
            Msg::Key(_)
            | Msg::KeyWithText { .. }
            | Msg::Paste(_)
            | Msg::Mouse(_)
            | Msg::Scroll { .. } => {
                self.tooltip.dismiss();
            }
            _ => {}
        }
        match msg {
            Msg::Key(key) => {
                self.autoscroll = None;
                // Any key drops the reasoning preview. The cycle re-flashes it
                // a moment later, so only the keys that did not ask for it lose
                // it.
                self.thinking_picker.clear_flash();
                self.handle_key(key)
            }
            Msg::KeyWithText { key, text } => {
                self.autoscroll = None;
                self.thinking_picker.clear_flash();
                self.handle_keyboard(key, Some(text.as_deref()))
            }
            Msg::Paste(text) => {
                if self.docs_holds_input() {
                    self.docs_modal.handle_paste(&text);
                    return vec![];
                }
                if (self.mode_submission.is_open() || self.update_prompt.is_open())
                    && !self.permission_prompt_holds_input()
                    && !self.question_form.is_open()
                {
                    return Vec::new();
                }
                self.sync_subagent_input_target();
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                if self.permission_prompt_holds_input() {
                    if !self.claim_deciding_permission() {
                        self.permission_prompt.handle_paste(&text);
                    }
                    return vec![];
                }
                if self.sandbox_manager.handle_paste(&text) {
                    return vec![];
                }
                if self.permissions_picker.is_open() {
                    if !self.permission_mutation_pending() {
                        self.permissions_picker.handle_paste(&text);
                    }
                    return vec![];
                }
                if self.peer_manager.is_open() && !self.paste_editor.is_open() {
                    self.peer_manager.handle_paste(&text);
                    return vec![];
                }
                if self.stream_modal.is_open() {
                    self.stream_modal.handle_paste(&text);
                    return vec![];
                }
                if self.session_relocation_picker.is_open()
                    || self.worktree_picker.is_open()
                    || self.accounts_modal.is_open()
                    || self.logs_modal.is_open()
                    || self.help_modal.is_open()
                {
                    self.route_text_paste(&text);
                    return vec![];
                }
                if self.workbench.is_open() {
                    if self.question_form.is_open() {
                        self.question_form.handle_paste(&text);
                        return vec![];
                    }
                    if self.workbench.paste(&text) {
                        return vec![];
                    }
                }
                if self.paste_editor.handle_paste(&text) {
                    return vec![];
                }
                if text.is_empty() {
                    if self.is_main_chat() && self.image_paste_rx.is_empty() {
                        self.start_image_paste();
                    }
                } else {
                    let mut any_image = false;
                    if self.is_main_chat() {
                        for line in text.lines() {
                            if let Some((path, mt)) = image::try_parse_image_path(line) {
                                self.start_file_image_paste(path, mt);
                                any_image = true;
                            }
                        }
                    }
                    if !any_image {
                        self.route_text_paste(&text);
                    }
                }
                vec![]
            }
            Msg::Mouse(event) if self.permission_prompt_holds_input() => {
                self.autoscroll = None;
                if matches!(event.kind, MouseEventKind::Down(_))
                    && self
                        .permission_prompt
                        .contains(Position::new(event.column, event.row))
                    && self.claim_deciding_permission()
                {
                    return vec![];
                }
                if let PromptMouse::Decided(decision) = self.permission_prompt.handle_mouse(event) {
                    self.apply_permission_decision(decision);
                }
                vec![]
            }
            Msg::Mouse(event) => self.handle_mouse(event),
            Msg::Scroll { column, row, delta } => {
                self.autoscroll = None;
                if self.permission_prompt_holds_input() {
                    if self.permission_prompt.contains(Position::new(column, row)) {
                        self.permission_prompt.scroll(delta);
                    } else {
                        self.active_chat().scroll(delta);
                    }
                } else {
                    self.handle_scroll(column, row, delta);
                }
                vec![]
            }
            Msg::Agent(envelope) => self.handle_agent_event(*envelope),
        }
    }

    /// The form's reply goes back through the answer channel the asking tool
    /// is parked on. A dismissal is an unparseable reply by design: the tool
    /// reads anything it cannot make answers of as "the user declined".
    fn handle_question_form_action(&mut self, action: QuestionFormAction) -> Vec<Action> {
        if self.question_form.deferred_task().is_some() {
            return self.handle_deferred_question_action(action);
        }
        let answered = matches!(
            action,
            QuestionFormAction::Submit(_)
                | QuestionFormAction::Accept
                | QuestionFormAction::Decline
                | QuestionFormAction::Dismiss
                | QuestionFormAction::Cancel
        );
        if answered {
            self.discard_question_btw();
        }
        if answered
            && self
                .task_interactions
                .question
                .as_ref()
                .is_some_and(|origin| !self.task_response_current(origin))
        {
            let note = self.question_form.note_text();
            self.close_blocking_question();
            if matches!(action, QuestionFormAction::Cancel) {
                self.keep_question_note(note);
            }
            return Vec::new();
        }
        if answered {
            self.task_interactions.question = None;
        }
        let reply = match action {
            QuestionFormAction::Consumed => return Vec::new(),
            QuestionFormAction::AskBtw => {
                self.open_question_btw();
                return Vec::new();
            }
            QuestionFormAction::Copy(text) => {
                self.copy_to_clipboard(&text);
                return Vec::new();
            }
            QuestionFormAction::CopySelection(text) => {
                self.copy_labelled(&text, COPIED_SELECTION);
                return Vec::new();
            }
            QuestionFormAction::Later => {
                self.defer_question();
                return Vec::new();
            }
            // Nothing is sent back: the parked tool ends on its own cancel
            // path, and the run only moves again when the user says so.
            QuestionFormAction::Cancel => {
                self.user_stopped(AutopilotStopper::QuestionCancel);
                let note = self.question_form.note_text();
                let subagent = self.question_subagent.take();
                self.close_blocking_question();
                let actions = match subagent {
                    Some(task_id) => self.cancel_subagent(task_id),
                    None => self.handle_cancel(),
                };
                self.keep_question_note(note);
                return actions;
            }
            QuestionFormAction::Dismiss | QuestionFormAction::PutAside => {
                self.question_form.reply(None)
            }
            QuestionFormAction::Decline => QUESTION_DECLINED_REPLY.to_owned(),
            QuestionFormAction::Accept => QUESTION_ACCEPTED_REPLY.to_owned(),
            QuestionFormAction::Submit(answers) => self.question_form.reply(Some(answers)),
        };
        let subagent = self.question_subagent.take();
        self.close_blocking_question();
        self.send_to_agent(subagent.as_deref(), reply);
        Vec::new()
    }

    /// Closes the form an agent withdrew, and only if it is the one on screen:
    /// a later question from someone else must stay up for its own answer.
    /// Nothing is sent back, since nothing waits for this form any more.
    fn close_withdrawn_question(&mut self, envelope: &Envelope) {
        let subagent = envelope.subagent.as_ref().map(|s| s.task_id.as_str());
        let same_asker = self.question_subagent.as_deref() == subagent
            && self.task_interactions.question.as_deref() == envelope.task.as_deref();
        if !self.has_blocking_question() || !same_asker {
            return;
        }
        self.close_blocking_question();
    }

    fn send_answer(&self, answer: String) {
        if let Some(tx) = &self.answer_tx {
            let _ = tx.try_send(answer);
        }
    }

    fn send_to_agent(&self, subagent_id: Option<&str>, answer: String) {
        let routed = subagent_id.and_then(|id| self.subagent_answers.get(id));
        if let Some(tx) = routed {
            let _ = tx.try_send(answer);
        } else if !subagent_id.is_some_and(|id| self.session_owns_task(id)) {
            self.send_answer(answer);
        }
    }

    /// A decision only clears the prompt once it is recorded. Transient
    /// answers have nothing to record, and a request that is no longer pending
    /// was answered elsewhere, so both clear it too.
    pub(crate) fn apply_permission_decision(&mut self, decision: PermissionDecision) {
        self.request_permission_answer(decision);
    }

    pub(super) fn permission_prompt_key(&mut self, key: KeyEvent) {
        self.permission_prompt_keyboard(key, None);
    }

    fn permission_prompt_keyboard(&mut self, key: KeyEvent, text: Option<Option<&str>>) {
        if let Some(decision) = self.permission_prompt.handle_keyboard(key, text) {
            self.apply_permission_decision(decision);
        }
        if let Some(text) = self.permission_prompt.take_copied() {
            self.copy_to_clipboard(&text);
        }
    }

    /// A card the user has to answer takes the screen from the views it
    /// would otherwise sit behind.
    fn make_way_for_permission_prompt(&mut self) {
        self.context_modal.close();
        self.tools_modal.close();
        self.decisions_modal.close();
        self.skills_modal.close();
        self.storage_modal.close();
        self.learn_inspector.close();
        self.learn_transcripts.close();
        self.system_prompt_modal.close();
        self.projection_modal.close();
        self.docs_modal.close();
        self.changelog_modal.close();
        if self.permissions_picker.is_open() {
            self.suspend_permission_editor();
            self.permission_config_trust_deferred =
                self.permissions.needs_project_permission_config_trust();
        }
    }

    /// The card takes every key and click, except one autopilot is still
    /// deciding: that one waits behind an open modal rather than take its
    /// input.
    pub(super) fn permission_prompt_holds_input(&self) -> bool {
        self.permission_prompt.is_open()
            && !(self.permission_prompt.autopilot_note() == Some(&AutopilotNote::Deciding)
                && self.has_modal_overlay())
    }

    fn scroll_at(&mut self, column: u16, row: u16, delta: i32) -> Option<SelectionZone> {
        let pos = Position::new(column, row);
        // Docked forms are not modal, so they claim the wheel only where they
        // drew: everywhere else it belongs to the transcript behind them.
        if self.permission_prompt.is_open() && self.permission_prompt.contains(pos) {
            self.permission_prompt.scroll(delta);
            return None;
        }
        if self.docs_modal.is_open() {
            self.docs_modal.scroll_at(pos, delta);
            return None;
        }
        if self.peer_manager.is_open() {
            if self.peer_manager.contains(pos) {
                self.peer_manager.scroll_at(pos, delta);
            }
            return None;
        }
        if self.stream_modal.is_open() {
            self.stream_modal.scroll(delta);
            return None;
        }
        if self.question_form.is_open() && self.question_form.contains(pos) {
            self.question_form.scroll(delta);
            return None;
        }
        if self.help_modal.is_open() {
            self.help_modal.scroll(delta);
            return None;
        }
        if self.usage_modal.is_open() {
            self.usage_modal.scroll(delta);
            return None;
        }
        if self.logs_modal.is_open() {
            self.logs_modal.scroll(delta);
            return None;
        }
        if self.changelog_modal.is_open() {
            self.changelog_modal.scroll(delta);
            return None;
        }
        if self.context_modal.is_open() {
            self.context_modal.scroll(delta);
            return None;
        }
        if self.tools_modal.is_open() {
            self.tools_modal.scroll(delta);
            return None;
        }
        if self.decisions_modal.is_open() {
            self.decisions_modal.scroll_at(pos, delta);
            return None;
        }
        if self.skills_modal.is_open() {
            self.skills_modal.scroll(delta);
            return None;
        }
        if self.storage_modal.is_open() {
            self.storage_modal.scroll(delta);
            return None;
        }
        if self.learn_transcripts.is_open() {
            self.learn_transcripts.scroll_at(pos, delta);
            return None;
        }
        if self.learn_inspector.is_open() {
            self.learn_inspector.scroll(delta);
            return None;
        }
        if self.system_prompt_modal.is_open() {
            self.system_prompt_modal.scroll(delta);
            return None;
        }
        if self.projection_modal.is_open() {
            self.projection_modal.scroll(delta);
            return None;
        }
        if self.goal_panel.is_open() {
            self.goal_panel.scroll_at(pos, delta);
            return None;
        }
        if self.autopilot_panel.is_open() {
            self.autopilot_panel.scroll_at(pos, delta);
            return None;
        }
        if self.float_mgr.is_open() && self.float_mgr.contains(pos) {
            self.float_mgr.scroll(delta);
            return None;
        }
        macro_rules! try_picker {
            ($picker:expr) => {
                if $picker.is_open() {
                    if $picker.contains(pos) {
                        $picker.scroll(delta);
                    }
                    return None;
                }
            };
        }
        try_picker!(self.command_modal);
        if self.search_modal.is_open() {
            if self.search_modal.contains(pos) {
                self.search_modal.scroll_at(pos, delta);
            }
            return None;
        }
        try_picker!(self.theme_picker);
        try_picker!(self.mcp_picker);
        try_picker!(self.env_modal);
        if self.settings_modal.is_open() {
            if self.settings_modal.contains(pos) {
                self.settings_modal.scroll_at(pos, delta);
            }
            return None;
        }
        try_picker!(self.login_picker);
        try_picker!(self.rewind_picker);
        try_picker!(self.message_actions);
        try_picker!(self.queue_actions);
        try_picker!(self.review);
        try_picker!(self.model_picker);
        try_picker!(self.prompt_profile_picker);
        try_picker!(self.thinking_picker);
        try_picker!(self.file_picker);
        if self.permissions_picker.is_open() {
            if self.permissions_picker.contains(pos) {
                self.permissions_picker.scroll_at(pos, delta);
            }
            return None;
        }
        try_picker!(self.stash_picker);
        // Not `try_picker!`: the wheel scrolls the pane under the pointer.
        if self.memory_inspector.is_open() {
            self.memory_inspector.scroll_at(pos, delta);
            return None;
        }
        // Not `try_picker!`: the wheel scrolls the run list or the section,
        // whichever pane it is over.
        if self.workflow_inspector.is_open() {
            self.workflow_inspector.scroll_at(pos, delta);
            return None;
        }
        try_picker!(self.workflow_catalog_picker);
        if self.automation_inspector.is_open() {
            self.automation_inspector.scroll_at(pos, delta);
            return None;
        }
        // Not `try_picker!`: scrolling the task list previews the task behind
        // the float, and only the app can carry that out.
        if self.task_picker.is_open() {
            if self.task_picker.contains(pos) {
                let action = self.task_picker.scroll(delta);
                self.handle_task_picker_action(action);
            }
            return None;
        }
        if self.shell_modal.is_open() {
            if self.shell_modal.contains(pos) {
                self.shell_modal.scroll(delta);
            }
            return None;
        }
        try_picker!(self.session_picker);
        try_picker!(self.session_relocation_picker);
        try_picker!(self.worktree_picker);
        // Not modal: the palette floats over the transcript, so it claims the
        // wheel only where it actually drew.
        if self.command_palette.is_active() && self.command_palette.contains(pos) {
            self.command_palette.scroll(delta);
            return None;
        }
        if self.mention_popup.contains(pos) {
            self.mention_popup.scroll(delta);
            return None;
        }
        if self.commit_popup.contains(pos) {
            self.commit_popup.scroll(delta);
            return None;
        }
        let zone = self.zone_at(row, column)?.zone;
        // An armed scroll card under the pointer takes what it can use and
        // hands back the rest, so the transcript still moves once the card is
        // at an edge and the reader is never stuck inside one.
        let delta = match zone {
            SelectionZone::Messages => {
                self.chats[self.active_chat].scroll_card_at(column, row, delta)
            }
            _ => delta,
        };
        if delta != 0 {
            self.scroll_zone(zone, delta);
        }
        Some(zone)
    }

    fn handle_global_key(&mut self, key: KeyEvent) -> Option<Vec<Action>> {
        if key::QUIT.matches(key) {
            // A selection is a copy target before it is anything else: select
            // all then copy must not be the gesture that throws the draft away.
            // The selection survives, so discarding means clearing it first.
            if self.composer_holds_keys()
                && let Some(text) = self.active_input_box().selected_text()
            {
                self.copy_to_clipboard(&text);
                return Some(vec![]);
            }
            self.command_palette.close();
            if !self.is_main_chat()
                && self.active_subagent_has_composer()
                && !self.subagent_input_box.is_empty()
            {
                self.subagent_input_box.clear_undoably();
                return Some(vec![]);
            }
            return Some(if !self.is_main_chat() || self.input_box.is_empty() {
                if self.status == Status::Streaming {
                    self.user_stopped(AutopilotStopper::Interrupt);
                    return Some(self.handle_cancel());
                }
                self.quit()
            } else {
                self.input_box.clear_undoably();
                vec![]
            });
        }
        if key::EXIT.matches(key) {
            let input_empty = if self.is_main_chat() {
                self.input_box.is_empty()
            } else {
                !self.active_subagent_has_composer() || self.subagent_input_box.is_empty()
            };
            if self.status != Status::Idle || !input_empty {
                self.last_exit = None;
                return Some(vec![]);
            }
            return Some(
                if let Some(pressed_at) = self.last_exit.take()
                    && pressed_at.elapsed() < CONFIRMATION_DURATION
                {
                    self.quit()
                } else {
                    self.last_exit = Some(Instant::now());
                    self.status_bar
                        .flash_for(FLASH_EXIT.into(), CONFIRMATION_DURATION);
                    vec![]
                },
            );
        }
        if key::COMMAND_PALETTE.matches(key) {
            return Some(self.run_builtin(BuiltinAction::CommandPalette));
        }
        if key::HELP.matches(key) {
            return Some(self.run_builtin(BuiltinAction::Help));
        }
        if key::ASK_BTW.matches(key) {
            self.open_btw_prompt();
            return Some(vec![]);
        }
        if key::POP_QUEUE.matches(key) {
            return Some(self.run_builtin(BuiltinAction::PopQueue));
        }
        if self.scroll_transcript(key) {
            return Some(vec![]);
        }
        // Reading is the only thing Esc backs out of here, so it never reaches
        // the rewind it would otherwise arm.
        if key.code == KeyCode::Esc
            && self.key_focus == KeyFocus::Transcript
            && self.composer_is_interactive()
        {
            self.key_focus = KeyFocus::Composer;
            return Some(vec![]);
        }
        // Only claimed when a diagram actually moves, so the binding stays
        // out of the way in a transcript that has none.
        for (bind, delta) in [(key::PAN_LEFT, -PAN_STEP), (key::PAN_RIGHT, PAN_STEP)] {
            if bind.matches(key) && self.active_chat().pan_visible_diagram(delta) {
                return Some(vec![]);
            }
        }
        None
    }

    /// Keys that only move the transcript. A form waiting on an answer hands
    /// these through so the chat behind it can still be read, which is why
    /// they live apart from the rest of the global binds.
    ///
    /// The Ctrl binds are unconditional. The four bare navigation keys are
    /// left to the composer while it holds the focus, and taking them is what
    /// hands the focus over, so a reader never has to arm a mode first.
    fn scroll_transcript(&mut self, key: KeyEvent) -> bool {
        let composer_owns = self.nav_owner() == KeyFocus::Composer;
        if key::SCROLL_HALF_UP.matches(key) {
            let half = self.chats[self.active_chat].half_page();
            self.active_chat().scroll(half);
        } else if key::SCROLL_TOP.matches(key) {
            self.active_chat().scroll_to_top();
        } else if key::PAGE_UP.matches(key) || key::PAGE_DOWN.matches(key) {
            let up = key::PAGE_UP.matches(key);
            if composer_owns && self.active_input_box_mut().page(up) {
                return true;
            }
            let half = self.chats[self.active_chat].half_page();
            self.active_chat().scroll(if up { half } else { -half });
            self.key_focus = KeyFocus::Transcript;
        } else if key::DOC_TOP.matches(key) && !composer_owns {
            self.active_chat().scroll_to_top();
        } else if key::DOC_BOTTOM.matches(key) && !composer_owns {
            self.active_chat().enable_auto_scroll();
        } else {
            return false;
        }
        true
    }

    /// The composer is on screen and taking keys. The same condition gates the
    /// `Input` zone in `register_zones`, so a click and a key agree on where
    /// the keyboard is.
    pub(super) fn composer_is_interactive(&self) -> bool {
        !self.plan_form_in_view()
            && !self.question_form.is_open()
            && (self.is_main_chat()
                || self.active_subagent_has_composer()
                || self.queue_editor_active())
    }

    /// Whether the composer is the thing taking keys, which is what draws its
    /// cursor block and what hit testing has to assume that block occupies.
    /// An open overlay takes the keyboard outright; a transcript that has been
    /// handed the navigation keys takes only those, but the block goes with
    /// them because it is the one sign of where they land.
    pub(super) fn composer_holds_keys(&self) -> bool {
        !self.any_overlay_open() && self.key_focus == KeyFocus::Composer
    }

    /// Who the four bare navigation keys act on. With no composer drawn there
    /// is nothing to compete with the transcript, whatever the focus says.
    fn nav_owner(&self) -> KeyFocus {
        match self.key_focus == KeyFocus::Composer && self.composer_is_interactive() {
            true => KeyFocus::Composer,
            false => KeyFocus::Transcript,
        }
    }

    fn dispatch_overlay(
        &mut self,
        key: KeyEvent,
        text: Option<Option<&str>>,
    ) -> Option<Vec<Action>> {
        let generated = text_field::keyboard_text(key, text);
        let physical_key = key;
        let command_key = text_field::command_key(key, text);
        let key = command_key.unwrap_or(key);
        macro_rules! keyboard_text {
            ($owner:ident, $editing:expr) => {
                if $editing && let Some(text) = generated {
                    if !text.is_empty() {
                        self.$owner.handle_text(text, false);
                    }
                    return Some(vec![]);
                }
            };
        }
        macro_rules! guard_repeat {
            ($editing:expr) => {
                if command_key.is_none() {
                    return Some(vec![]);
                }
                if key.kind == KeyEventKind::Repeat && !Self::repeat_key_allowed(key, $editing) {
                    return Some(vec![]);
                }
            };
        }
        // Above whichever modal opened it, so it answers first.
        if self.docs_modal.is_open() {
            keyboard_text!(docs_modal, self.docs_modal.text_input_active());
            guard_repeat!(self.docs_modal.text_input_active());
            let action = self.docs_modal.handle_key(key);
            return Some(self.handle_docs_action(action));
        }
        if self.mode_submission.is_open()
            && !self.permission_prompt_holds_input()
            && !self.question_form.is_open()
        {
            if command_key.is_none() {
                return Some(vec![]);
            }
            let action = self.mode_submission.handle_key(key);
            return Some(self.handle_mode_submission(action));
        }
        if self.update_prompt.is_open()
            && !self.permission_prompt_holds_input()
            && !self.question_form.is_open()
        {
            if command_key.is_none() {
                return Some(vec![]);
            }
            let action = self.update_prompt.handle_key(key);
            return Some(self.handle_update_prompt(action));
        }
        if self.sandbox_manager.is_open() {
            keyboard_text!(sandbox_manager, self.sandbox_manager.text_input_active());
            if command_key.is_none() {
                return Some(vec![]);
            }
            let action = self.sandbox_manager.handle_key(key);
            self.handle_sandbox_action(action);
            return Some(vec![]);
        }
        if self.image_viewer.is_open() {
            guard_repeat!(false);
            let action = self.image_viewer.handle_key(key);
            if matches!(action, ImageViewerAction::Passthrough) {
                return None;
            }
            return Some(self.handle_image_viewer_action(action));
        }
        if self.paste_editor.is_open() {
            keyboard_text!(paste_editor, true);
            guard_repeat!(true);
            match self.paste_editor.handle_key(key) {
                PasteEditorAction::Consumed => {}
                // Nothing in the editor wanted it, so the key keeps whatever it
                // means to the app: `Ctrl+C` with no selection still quits.
                PasteEditorAction::Passthrough => return None,
                PasteEditorAction::Copy(text) => self.copy_to_clipboard(&text),
                PasteEditorAction::Cancel => self.paste_editor.close(),
                PasteEditorAction::Save { target, id, text } => {
                    if self.active_input_target() == Some(target)
                        && self.active_input_box_mut().update_paste(id, &text)
                    {
                        self.resync_dropdowns();
                    }
                    self.paste_editor.close();
                }
            }
            return Some(vec![]);
        }

        if self.peer_manager.is_open() {
            let action = self.peer_manager.handle_keyboard(physical_key, text);
            return Some(self.handle_peer_manager_action(action));
        }

        // plan_form is non-modal: Passthrough falls through to the rest of dispatch
        if self.plan_form_takes_input() {
            guard_repeat!(false);
            self.claim_plan_card();
            let action = self.plan_form.handle_key(key);
            if action != PlanFormAction::Passthrough {
                return Some(self.handle_plan_form_action(action));
            }
        }

        if self.help_modal.is_open() {
            keyboard_text!(help_modal, self.help_modal.text_input_active());
            guard_repeat!(self.help_modal.text_input_active());
            if let Some(text) = self.help_modal.handle_key(key) {
                self.copy_to_clipboard(&text);
            }
            return Some(vec![]);
        }

        if self.usage_modal.is_open() {
            guard_repeat!(false);
            if key::REFRESH.matches(key) {
                return Some(self.reload_usage());
            }
            self.usage_modal.handle_key(key);
            self.load_lifetime_usage();
            return Some(vec![]);
        }

        if self.logs_modal.is_open() {
            keyboard_text!(logs_modal, self.logs_modal.text_input_active());
            guard_repeat!(self.logs_modal.text_input_active());
            let action = self.logs_modal.handle_key(key);
            self.handle_logs_action(action);
            return Some(vec![]);
        }

        if self.changelog_modal.is_open() {
            guard_repeat!(false);
            let action = self.changelog_modal.handle_key(key);
            return Some(self.handle_changelog_action(action));
        }

        if self.context_modal.is_open() {
            guard_repeat!(false);
            self.context_modal.handle_key(key);
            return Some(vec![]);
        }

        if self.tools_modal.is_open() {
            guard_repeat!(false);
            self.on_tools_modal(|modal| modal.handle_key(key));
            return Some(vec![]);
        }

        if self.decisions_modal.is_open() {
            guard_repeat!(false);
            let action = self.decisions_modal.handle_key(key);
            return Some(self.handle_decisions_action(action));
        }

        if self.skills_modal.is_open() {
            guard_repeat!(false);
            self.skills_modal.handle_key(key);
            return Some(vec![]);
        }

        if self.storage_modal.is_open() {
            guard_repeat!(false);
            self.storage_modal.handle_key(key);
            return Some(vec![]);
        }

        if self.learn_transcripts.is_open() {
            self.learn_transcripts.handle_key(key);
            return Some(vec![]);
        }

        if self.learn_inspector.is_open() {
            guard_repeat!(false);
            let action = self.learn_inspector.handle_key(key);
            return Some(self.handle_learn_action(action));
        }

        if self.system_prompt_modal.is_open() {
            guard_repeat!(false);
            let action = self.system_prompt_modal.handle_key(key);
            self.handle_system_prompt_action(action);
            return Some(vec![]);
        }

        if self.projection_modal.is_open() {
            guard_repeat!(false);
            let action = self.projection_modal.handle_key(key);
            self.handle_projection_action(action);
            return Some(vec![]);
        }

        if self.goal_panel.is_open() {
            guard_repeat!(false);
            return Some(
                self.goal_panel
                    .handle_key(key, self.state.goal.continuation_limit())
                    .map(|action| self.on_goal_panel(action))
                    .unwrap_or_default(),
            );
        }

        if self.autopilot_panel.is_open() {
            guard_repeat!(false);
            return Some(
                self.autopilot_panel
                    .handle_key(key)
                    .map(|action| self.on_autopilot_panel(action))
                    .unwrap_or_default(),
            );
        }

        if self.stream_modal.is_open() {
            keyboard_text!(stream_modal, self.stream_modal.text_input_active());
            guard_repeat!(self.stream_modal.text_input_active());
            match self.stream_modal.handle_key(key) {
                StreamAction::Ignored | StreamAction::Consumed => {}
                StreamAction::Copy(text) => self.copy_to_clipboard(&text),
                StreamAction::CopySelection(text) => self.copy_labelled(&text, COPIED_SELECTION),
                StreamAction::Submit(question) => self.continue_btw(question),
                StreamAction::Clear => self.clear_btw_thread(),
            }
            return Some(vec![]);
        }

        if Overlay::is_open(&self.float_mgr) {
            if let Some(text) = generated {
                self.float_mgr.handle_keyboard_text(text);
                return Some(vec![]);
            }
            guard_repeat!(false);
        }
        if self.float_mgr.handle_key(key) {
            return Some(vec![]);
        }

        if self.search_modal.is_open() {
            if let Some(text) = generated {
                if text.is_empty() {
                    return Some(vec![]);
                }
                self.search_modal.handle_text(text, false);
                return Some(self.handle_search_action(SearchAction::QueryChanged));
            }
            guard_repeat!(true);
            let action = self.search_modal.handle_key(key);
            return Some(self.handle_search_action(action));
        }

        if self.file_picker.is_open() {
            keyboard_text!(file_picker, true);
            guard_repeat!(true);
            let action = self.file_picker.handle_key(key);
            return Some(self.handle_file_picker_action(action));
        }

        if self.queue_editor_active() {
            if let Some(text) = generated {
                return Some(self.composer_keyboard_text(text));
            }
            guard_repeat!(true);
            return Some(self.handle_queue_editor_key(key));
        }

        // Ahead of the focused queue itself: the menu is drawn over the panel
        // it acts on, so it answers first.
        if self.queue_actions.is_open() {
            keyboard_text!(queue_actions, true);
            guard_repeat!(false);
            let action = self.queue_actions.handle_key(key);
            return Some(self.handle_queue_actions_action(action));
        }

        if self.active_queue_is_focused() {
            guard_repeat!(false);
            match key.code {
                KeyCode::Up if key.modifiers == KeyModifiers::SHIFT => {
                    self.move_focused_queue_item(true);
                }
                KeyCode::Down if key.modifiers == KeyModifiers::SHIFT => {
                    self.move_focused_queue_item(false);
                }
                KeyCode::Up if key.modifiers.is_empty() => self.move_active_queue_focus(-1),
                KeyCode::Down if key.modifiers.is_empty() => self.move_active_queue_focus(1),
                KeyCode::Enter => {
                    if let Some(id) = self
                        .active_queue_entries()
                        .get(self.active_queue_focus().unwrap_or(0))
                        .filter(|entry| entry.editable)
                        .map(|entry| entry.id)
                    {
                        self.begin_queue_edit(id);
                    }
                }
                KeyCode::Delete => self.delete_focused_queue_item(),
                KeyCode::Char('d') if key.modifiers.is_empty() => {
                    self.delete_focused_queue_item();
                }
                KeyCode::Char('m') if key.modifiers.is_empty() => {
                    if let Some(id) = self
                        .active_queue_entries()
                        .get(self.active_queue_focus().unwrap_or(0))
                        .filter(|entry| entry.movable)
                        .map(|entry| entry.id)
                    {
                        self.move_unsent_to_main(id);
                    }
                }
                KeyCode::Char('g') if key.modifiers.is_empty() => {
                    self.set_focused_queue_admission(PromptAdmission::Steer);
                }
                KeyCode::Char('n') if key.modifiers.is_empty() => {
                    self.set_focused_queue_admission(PromptAdmission::Queue);
                }
                KeyCode::Char('b') if key.modifiers.is_empty() => {
                    self.toggle_active_queue_delivery();
                }
                KeyCode::Char('.') if key.modifiers.is_empty() => {
                    self.open_focused_queue_actions();
                }
                KeyCode::Char('r') if key.modifiers.is_empty() => {
                    return Some(self.replace_with_focused_queue_item());
                }
                KeyCode::Esc => self.unfocus_active_queue(),
                _ if key::QUIT.matches(key) => self.unfocus_active_queue(),
                _ if key::POP_QUEUE.matches(key) => self.pop_active_queue(),
                _ => {}
            }
            return Some(vec![]);
        }

        if self.rewind_picker.is_open() {
            keyboard_text!(rewind_picker, true);
            guard_repeat!(false);
            let action = self.rewind_picker.handle_key(key);
            return Some(self.handle_rewind_picker_action(action));
        }

        if self.message_actions.is_open() {
            keyboard_text!(message_actions, true);
            guard_repeat!(false);
            let action = self.message_actions.handle_key(key);
            return Some(self.handle_message_actions_action(action));
        }

        if self.review.is_open() {
            keyboard_text!(review, self.review.text_input_active());
            guard_repeat!(self.review.text_input_active());
            let action = self.review.handle_key(key);
            // The note editor hands back only the chord it must not swallow, so
            // `Ctrl+C` with nothing selected still reaches the app.
            if matches!(action, ReviewAction::Passthrough) {
                return None;
            }
            return Some(self.handle_review_action(action));
        }

        if self.command_modal.is_open() {
            if let Some(text) = generated {
                if !text.is_empty() {
                    self.command_modal.handle_text(text, false);
                    self.resync_command_palette();
                }
                return Some(vec![]);
            }
            guard_repeat!(true);
            let action = self
                .command_modal
                .handle_key(key, &mut self.command_palette);
            return Some(self.handle_command_modal_action(action));
        }

        if self.theme_picker.is_open() {
            keyboard_text!(theme_picker, true);
            guard_repeat!(false);
            let action = self.theme_picker.handle_key(key);
            return Some(self.handle_theme_picker_action(action));
        }

        if self.prompt_profile_picker.is_open() {
            keyboard_text!(prompt_profile_picker, true);
            guard_repeat!(false);
            let action = self.prompt_profile_picker.handle_key(key);
            return Some(self.handle_prompt_profile_picker_action(action));
        }

        if self.thinking_picker.is_open() {
            keyboard_text!(thinking_picker, true);
            guard_repeat!(false);
            let action = self.thinking_picker.handle_key(key);
            return Some(self.handle_thinking_picker_action(action));
        }

        if self.model_picker.is_open() {
            keyboard_text!(model_picker, self.model_picker.text_key(command_key));
            guard_repeat!(false);
            let action = self.model_picker.handle_key(key);
            return Some(self.handle_model_picker_action(action));
        }

        if self.login_picker.is_open() {
            keyboard_text!(login_picker, true);
            guard_repeat!(self.login_picker.text_input_active());
            let action = self.login_picker.handle_key(key);
            return Some(self.handle_login_picker_action(action));
        }

        if self.mcp_picker.is_open() {
            keyboard_text!(mcp_picker, self.mcp_picker.text_key(command_key));
            guard_repeat!(false);
            let action = self.mcp_picker.handle_key(key);
            return Some(self.handle_mcp_picker_action(action));
        }

        if self.env_modal.is_open() {
            keyboard_text!(env_modal, self.env_modal.text_key(command_key));
            guard_repeat!(self.env_modal.text_input_active());
            let action = self.env_modal.handle_key(key);
            return Some(self.handle_env_action(action));
        }

        if self.settings_modal.is_open() {
            keyboard_text!(settings_modal, self.settings_modal.text_input_active());
            guard_repeat!(self.settings_modal.text_input_active());
            let action = self.settings_modal.handle_key(key);
            return Some(self.handle_settings_action(action));
        }

        if self.permissions_picker.is_open() {
            keyboard_text!(
                permissions_picker,
                self.permissions_picker.text_input_active()
            );
            guard_repeat!(
                self.permissions_picker
                    .editor_mut()
                    .is_some_and(|editor| editor.is_editing())
            );
            let action = self.permissions_picker.handle_key(key);
            return Some(self.handle_permissions_picker_action(action));
        }

        if self.stash_picker.is_open() {
            keyboard_text!(stash_picker, true);
            guard_repeat!(false);
            let action = self.stash_picker.handle_key(key);
            return Some(self.handle_stash_picker_action(action));
        }

        if self.question_form.is_open() {
            let typing = self.question_form.text_input_active();
            if typing && let Some(text) = generated {
                self.claim_question_form();
                if !text.is_empty() {
                    self.question_form.handle_text(text, false);
                }
                return Some(vec![]);
            }
            guard_repeat!(typing);
            // The form is docked, not modal: keys that only move the
            // transcript keep working while it waits for an answer, except
            // `Home` and `End`, which move the caret of an answer being typed.
            let caret_key = typing && (key::DOC_TOP.matches(key) || key::DOC_BOTTOM.matches(key));
            if !caret_key && self.scroll_transcript(key) {
                return Some(vec![]);
            }
            self.claim_question_form();
            let action = self.question_form.handle_key(key);
            return Some(self.handle_question_form_action(action));
        }
        if self.session_picker.is_open() {
            keyboard_text!(session_picker, true);
            guard_repeat!(true);
            let action = self.session_picker.handle_key(key);
            return Some(self.handle_session_picker_action(action));
        }
        if self.session_relocation_picker.is_open() {
            keyboard_text!(
                session_relocation_picker,
                self.session_relocation_picker.text_input_active()
            );
            guard_repeat!(self.session_relocation_picker.text_input_active());
            let action = self.session_relocation_picker.handle_key(key);
            return Some(self.handle_session_relocation_action(action));
        }
        if self.worktree_picker.is_open() {
            keyboard_text!(worktree_picker, self.worktree_picker.text_input_active());
            guard_repeat!(true);
            let action = self.worktree_picker.handle_key(key);
            return Some(self.handle_worktree_action(action));
        }
        if self.accounts_modal.is_open() {
            keyboard_text!(accounts_modal, self.accounts_modal.text_input_active());
            guard_repeat!(self.accounts_modal.text_input_active());
            return Some(self.accounts_key(key));
        }
        if self.task_picker.is_open() {
            keyboard_text!(task_picker, true);
            guard_repeat!(false);
            let action = self.task_picker.handle_key(key);
            return Some(self.handle_task_picker_action(action));
        }
        if self.shell_modal.is_open() {
            keyboard_text!(shell_modal, self.shell_modal.text_input_active());
            guard_repeat!(false);
            let action = self.shell_modal.handle_key(key);
            return Some(self.handle_shell_modal_action(action));
        }
        if self.memory_inspector.is_open() {
            if self.memory_inspector.text_input_active()
                && let Some(text) = generated
            {
                if !text.is_empty()
                    && let Some(action) = self.memory_inspector.handle_text(text, false)
                {
                    return Some(self.handle_memory_action(action));
                }
                return Some(vec![]);
            }
            guard_repeat!(self.memory_inspector.text_input_active());
            let action = self.memory_inspector.handle_key(key);
            return Some(self.handle_memory_action(action));
        }
        if self.workflow_inspector.is_open() {
            if self.workflow_inspector.text_input_active()
                && let Some(text) = generated
            {
                if !text.is_empty()
                    && let Some(action) = self.workflow_inspector.handle_text(text, false)
                {
                    return Some(self.handle_workflow_inspector_action(action));
                }
                return Some(vec![]);
            }
            guard_repeat!(self.workflow_inspector.text_input_active());
            let action = self.workflow_inspector.handle_key(key);
            return Some(self.handle_workflow_inspector_action(action));
        }
        if self.workflow_catalog_picker.is_open() {
            keyboard_text!(
                workflow_catalog_picker,
                self.workflow_catalog_picker.text_input_active()
            );
            guard_repeat!(false);
            let action = self.workflow_catalog_picker.handle_key(key);
            return Some(self.handle_workflow_catalog_action(action));
        }
        if self.automation_inspector.is_open() {
            if self.automation_inspector.text_input_active()
                && let Some(text) = generated
            {
                if !text.is_empty()
                    && let Some(action) = self.automation_inspector.handle_text(text, false)
                {
                    self.handle_automation_action(action);
                }
                return Some(vec![]);
            }
            guard_repeat!(self.automation_inspector.text_input_active());
            // The editor hands back only the chord it must not swallow, so
            // `Ctrl+C` with nothing selected still reaches the app.
            match self
                .automation_inspector
                .handle_key(key, self.workflow.runs())
            {
                AutomationAction::Passthrough => return None,
                action => self.handle_automation_action(action),
            }
            return Some(vec![]);
        }

        // Last of the overlays. The workbench is a full-screen view, not a
        // modal: anything above it draws over it and answers first, or it owns
        // the keyboard from behind an opaque screen and a permission prompt
        // becomes unanswerable.
        if self.workbench.is_open() {
            if let Some(text) = generated
                && self.workbench.text_input_active()
            {
                if !text.is_empty() {
                    self.workbench.handle_text(text, false);
                }
                return Some(vec![]);
            }
            guard_repeat!(self.workbench.text_input_active());
            match self.workbench.handle_key(key) {
                WorkbenchAction::Passthrough if generated.is_some() => return Some(vec![]),
                WorkbenchAction::Passthrough => {}
                action => return Some(self.handle_workbench_action(action)),
            }
        }

        None
    }

    pub(crate) fn open_peer_manager(&mut self, view: PeerView) {
        self.close_all_overlays();
        self.command_palette.close();
        self.mention_popup.close();
        self.commit_popup.close();
        self.which_key.disarm();
        self.autoscroll = None;
        self.selection_state = None;
        self.status_mouse_down = None;
        self.queue_mouse_down = None;
        self.admission_mouse_down = None;
        self.chord_hint_down = None;
        self.message_action_mouse_down = None;
        self.link_mouse_down = None;
        self.mention_mouse_down = None;
        self.commit_mouse_down = None;
        self.last_esc = None;
        self.last_exit = None;
        self.peer_manager.open(view);
    }

    fn handle_peer_manager_action(&mut self, action: PeerManagerAction) -> Vec<Action> {
        match action {
            PeerManagerAction::Consumed => {}
            PeerManagerAction::Close => self.peer_manager.close(),
            PeerManagerAction::Copy(text) => self.copy_to_clipboard(&text),
            PeerManagerAction::Refresh => return vec![Action::RefreshPeers],
            PeerManagerAction::Review(message) => return vec![Action::ReviewPeerMessage(message)],
            PeerManagerAction::Decide { token, decision } => {
                return vec![Action::DecidePeerMessage { token, decision }];
            }
            PeerManagerAction::SetInbound(policy) => return vec![Action::SetPeerInbound(policy)],
            PeerManagerAction::Subscribe(change) => return vec![Action::PeerSubscribe(change)],
        }
        vec![]
    }

    fn handle_model_picker_action(&mut self, action: ModelPickerAction) -> Vec<Action> {
        let actions = self.model_picker_actions(action);
        self.unpark_settings();
        actions
    }

    fn model_picker_actions(&mut self, action: ModelPickerAction) -> Vec<Action> {
        match action {
            ModelPickerAction::Consumed | ModelPickerAction::Close => vec![],
            ModelPickerAction::AssignPstack { kind, seat, value } => {
                self.save_pstack_pick(RoleEdit::Seat { kind, seat, value });
                vec![]
            }
            ModelPickerAction::ClearPstack(kind) => {
                self.save_pstack_pick(RoleEdit::Reset(kind));
                vec![]
            }
            ModelPickerAction::Select(spec) => vec![Action::ChangeModel(spec)],
            ModelPickerAction::SelectForTask { task_id, spec } => {
                vec![Action::SwitchTaskModel { task_id, spec }]
            }
            ModelPickerAction::Bind(purpose, binding) => vec![Action::Bind(purpose, binding)],
            ModelPickerAction::Unbind(purpose) => vec![Action::Unbind(purpose)],
            ModelPickerAction::ToggleFavorite(spec) => {
                self.toggle_favorite_model(&spec);
                vec![]
            }
            ModelPickerAction::SelectDecision(name) => vec![Action::SetDecisionEndpoint(name)],
            ModelPickerAction::AddDecisionEndpoint => {
                self.model_picker.close();
                self.login_picker.open_decisions(self.storage.clone());
                vec![]
            }
            ModelPickerAction::Copy(text) => {
                self.copy_to_clipboard(&text);
                vec![]
            }
        }
    }

    fn handle_command_action(&mut self, action: CommandAction) -> Option<Vec<Action>> {
        match action {
            CommandAction::Consumed => Some(Vec::new()),
            CommandAction::Run(completion) => {
                let input = self.active_input_box_mut();
                input.complete(&completion);
                let line = input.buffer.value();
                input.discard();
                let command = self.command_palette.parse(&line);
                Some(self.execute_command(command, 0))
            }
            CommandAction::Complete(completion) => {
                self.active_input_box_mut().complete(&completion);
                self.resync_dropdowns();
                Some(Vec::new())
            }
            CommandAction::Usage(usage) => Some(self.flash_usage(&usage)),
            CommandAction::Passthrough => None,
        }
    }

    /// The palette follows the argument prompt while it is open, and the
    /// composer again once it is gone.
    fn handle_command_modal_action(&mut self, action: CommandModalAction) -> Vec<Action> {
        self.resync_command_palette();
        match action {
            CommandModalAction::Consumed | CommandModalAction::Closed => Vec::new(),
            CommandModalAction::Execute(cmd) => self.execute_command(cmd, 0),
            CommandModalAction::Copy(text) => {
                self.copy_to_clipboard(&text);
                Vec::new()
            }
            CommandModalAction::Usage(usage) => self.flash_usage(&usage),
        }
    }

    /// The one way a surface that holds the mouse hands text to the system
    /// clipboard, so the workbench, the paste editor and the review modal all
    /// report a copy the same way.
    pub(super) fn copy_to_clipboard(&mut self, text: &str) {
        let message = match self.clipboard.copy_text(text) {
            Ok(CopyResult::Noop) => "Nothing to copy".to_owned(),
            Ok(CopyResult::Copied) => "Copied".to_owned(),
            Err(e) => format!("Copy failed: {e}"),
        };
        self.status_bar.flash(message);
    }

    fn handle_workbench_action(&mut self, action: WorkbenchAction) -> Vec<Action> {
        if crate::sandbox::transfer::active()
            && !matches!(
                action,
                WorkbenchAction::Transfer(_)
                    | WorkbenchAction::Consumed
                    | WorkbenchAction::Passthrough
                    | WorkbenchAction::Flash(_)
                    | WorkbenchAction::Copy(_)
            )
        {
            return Vec::new();
        }
        match action {
            WorkbenchAction::Transfer(action) => self.handle_transfer_action(action),
            WorkbenchAction::Consumed | WorkbenchAction::Passthrough => {}
            WorkbenchAction::Close => self.close_permission_source(),
            WorkbenchAction::Flash(message) => self.status_bar.flash(message),
            WorkbenchAction::Copy(text) => self.copy_to_clipboard(&text),
            WorkbenchAction::SaveDocument { key, text, close } => {
                self.save_document(&key, text, close);
            }
            WorkbenchAction::RevertDocument(key) => self.revert_document(&key),
            WorkbenchAction::OpenHostFile { path, preview } => {
                self.open_caudra_path(&path, preview);
            }
            WorkbenchAction::SendToComposer { path, lines } => {
                self.close_permission_source();
                let text = match path {
                    caudra_workbench::WorkbenchPath::Local(path) => {
                        mentions::format(&path, lines.as_ref())
                    }
                    caudra_workbench::WorkbenchPath::Remote(path) => {
                        mentions::format(std::path::Path::new(path.as_str()), lines.as_ref())
                    }
                };
                if let InputAction::PaletteSync(val) =
                    self.active_input_box_mut().handle_paste_with_spaces(&text)
                {
                    self.sync_dropdowns(&val);
                }
            }
        }
        Vec::new()
    }

    fn handle_file_picker_action(&mut self, action: FilePickerModalAction) -> Vec<Action> {
        match action {
            FilePickerModalAction::Consumed => {}
            FilePickerModalAction::Select(path) => {
                self.file_picker.close();
                let mention = mentions::format(Path::new(&path), None);
                if let InputAction::PaletteSync(val) = self
                    .active_input_box_mut()
                    .handle_paste_with_spaces(&mention)
                {
                    self.sync_dropdowns(&val);
                }
            }
            FilePickerModalAction::Copy(text) => self.copy_to_clipboard(&text),
            FilePickerModalAction::Close => self.file_picker.close(),
        }
        Vec::new()
    }

    fn handle_rewind_picker_action(&mut self, action: RewindPickerAction) -> Vec<Action> {
        match action {
            RewindPickerAction::Consumed | RewindPickerAction::Close => Vec::new(),
            RewindPickerAction::Select(entry) => vec![Action::RewindSession(entry)],
            RewindPickerAction::Copy(text) => {
                self.copy_to_clipboard(&text);
                Vec::new()
            }
        }
    }

    fn handle_message_actions_action(&mut self, action: MessageActionsAction) -> Vec<Action> {
        if let MessageActionsAction::Copy(text) = &action {
            self.copy_to_clipboard(text);
        }
        let MessageActionsAction::Select { source, kind } = action else {
            return Vec::new();
        };
        match kind {
            MessageActionKind::Fork => {
                let forked = self.fork_at(source);
                self.fork_actions(forked)
            }
            MessageActionKind::RevertBoth => vec![Action::RevertSession {
                source,
                mode: RestoreMode::Both,
            }],
            MessageActionKind::RevertConversation => vec![Action::RevertSession {
                source,
                mode: RestoreMode::Conversation,
            }],
            MessageActionKind::RevertFiles => vec![Action::RevertSession {
                source,
                mode: RestoreMode::Files,
            }],
            MessageActionKind::Unrevert => vec![Action::UnrevertSession],
            MessageActionKind::Review => {
                self.open_review(source);
                Vec::new()
            }
        }
    }

    fn handle_queue_actions_action(&mut self, action: QueueActionsAction) -> Vec<Action> {
        if let QueueActionsAction::Copy(text) = &action {
            self.copy_to_clipboard(text);
        }
        let QueueActionsAction::Select { id, kind } = action else {
            return Vec::new();
        };
        match kind {
            QueueActionKind::Send => return self.send_held_queue_item(id),
            QueueActionKind::Edit => {
                self.select_active_queue_item(id);
                self.begin_queue_edit(id);
            }
            QueueActionKind::MoveUp => {
                self.move_active_queue_item(id, true);
            }
            QueueActionKind::MoveDown => {
                self.move_active_queue_item(id, false);
            }
            QueueActionKind::Guide => self.set_queue_admission(id, PromptAdmission::Steer),
            QueueActionKind::Next => self.set_queue_admission(id, PromptAdmission::Queue),
            QueueActionKind::Replace => return self.replace_with_queued_item(id),
            QueueActionKind::MoveMain => self.move_unsent_to_main(id),
            QueueActionKind::Delete => {
                self.delete_active_queue_item(id);
            }
        }
        Vec::new()
    }

    /// Compiled notes land in the prompt editor as a collapsed paste, so the
    /// user can add context or drop the batch before sending it.
    fn handle_review_action(&mut self, action: ReviewAction) -> Vec<Action> {
        match action {
            ReviewAction::Consumed | ReviewAction::Passthrough => {}
            ReviewAction::Copy(text) => self.copy_to_clipboard(&text),
            ReviewAction::Submit(text) => {
                self.input_box.handle_paste(&text);
                self.flash(REVIEW_READY_MSG.into());
            }
            ReviewAction::Close => {
                self.review.close();
                let pending = self.review.notes_pending();
                if pending > 0 {
                    self.flash(format!(
                        "{pending} review note{} pending. {} to resume.",
                        if pending == 1 { "" } else { "s" },
                        leader::REVIEW.label
                    ));
                }
            }
        }
        Vec::new()
    }

    pub(crate) fn open_review(&mut self, source: DisplaySource) {
        match self.chats[self.active_chat].review_target(source) {
            Some(target) => self.review.open(source, target),
            None => self.flash(REVIEW_UNAVAILABLE_MSG.into()),
        }
    }

    fn handle_theme_picker_action(&mut self, action: ThemePickerAction) -> Vec<Action> {
        if let ThemePickerAction::Copy(text) = action {
            self.copy_to_clipboard(&text);
        }
        Vec::new()
    }

    fn handle_prompt_profile_picker_action(
        &mut self,
        action: PromptProfilePickerAction,
    ) -> Vec<Action> {
        match action {
            PromptProfilePickerAction::Consumed | PromptProfilePickerAction::Closed => Vec::new(),
            PromptProfilePickerAction::Select(name) => {
                vec![Action::ChangeSystemPromptProfile(name)]
            }
            PromptProfilePickerAction::Copy(text) => {
                self.copy_to_clipboard(&text);
                Vec::new()
            }
        }
    }

    fn handle_thinking_picker_action(&mut self, action: ThinkingPickerAction) -> Vec<Action> {
        match action {
            ThinkingPickerAction::Select(thinking) => self.apply_thinking(thinking),
            ThinkingPickerAction::SelectForTask { task_id, thinking } => {
                self.switch_task(&task_id, caudra_agent::SettingsChange::thinking(thinking));
            }
            ThinkingPickerAction::Copy(text) => self.copy_to_clipboard(&text),
            ThinkingPickerAction::Consumed | ThinkingPickerAction::Closed => {}
        }
        Vec::new()
    }

    fn handle_login_picker_action(&mut self, action: LoginPickerAction) -> Vec<Action> {
        let closed = !matches!(
            &action,
            LoginPickerAction::Consumed
                | LoginPickerAction::Copy(_)
                | LoginPickerAction::DecisionConfigured { .. }
        );
        let actions = match action {
            LoginPickerAction::Consumed | LoginPickerAction::Close => Vec::new(),
            LoginPickerAction::Copy(text) => {
                self.copy_to_clipboard(&text);
                Vec::new()
            }
            LoginPickerAction::Authenticated { model_spec } => {
                vec![
                    Action::CompleteProviderSetup(model_spec),
                    Action::RefreshModels,
                ]
            }
            LoginPickerAction::Configured { slug } => {
                vec![Action::RefreshProvider { slug }, Action::RefreshModels]
            }
            LoginPickerAction::AuthenticateProvider {
                provider,
                model_spec,
                account,
            } => vec![Action::AuthenticateProvider {
                provider,
                model_spec,
                account,
            }],
            LoginPickerAction::DecisionConfigured { name, activate } => activate
                .then(|| Action::SetDecisionEndpoint(Some(name)))
                .into_iter()
                .collect(),
        };
        if closed {
            self.login_picker.close();
            self.open_awaiting_mcp_trust(false);
            self.open_awaiting_permission_config_trust(false);
        }
        actions
    }

    fn handle_mcp_picker_action(&mut self, action: McpPickerAction) -> Vec<Action> {
        let closed = matches!(&action, McpPickerAction::Close);
        let actions = match action {
            McpPickerAction::Consumed | McpPickerAction::Close => Vec::new(),
            McpPickerAction::Toggle {
                server_name,
                enabled,
            } => vec![Action::ToggleMcp(server_name, enabled)],
            McpPickerAction::TrustOnce { server_name } => {
                vec![Action::TrustMcpOnce(server_name)]
            }
            McpPickerAction::TrustProject { server_name } => {
                vec![Action::TrustMcpProject(server_name)]
            }
            McpPickerAction::Reject { server_name } => vec![Action::RejectMcp(server_name)],
            McpPickerAction::Copy(text) => {
                self.copy_to_clipboard(&text);
                Vec::new()
            }
        };
        if closed {
            self.mcp_picker.close();
            self.open_awaiting_permission_config_trust(false);
        }
        actions
    }

    pub(crate) fn open_awaiting_mcp_trust(&mut self, needs_login: bool) {
        if !needs_login && self.mcp_picker.has_awaiting_trust() {
            self.mcp_picker.open();
        }
    }

    pub(crate) fn open_awaiting_permission_config_trust(&mut self, needs_login: bool) {
        let needs_trust = self.permissions.needs_project_permission_config_trust();
        self.permission_review = Some(needs_trust);
        if needs_login
            || self.mcp_picker.is_open()
            || self.permission_prompt.is_open()
            || self.permission_job_pending()
        {
            self.permission_config_trust_deferred = needs_trust;
            return;
        }
        self.permission_config_trust_deferred = false;
        if needs_trust && let Err(error) = self.open_permissions_picker() {
            self.flash(error.to_string());
        }
    }

    fn open_permissions_picker(&mut self) -> Result<(), PermissionPolicyError> {
        self.request_permission_inventory();
        Ok(())
    }

    /// Trust the footer keeps asking for while its picker is out of sight.
    /// The permission half is read again only after the picker closes or the
    /// policy changes, since answering it opens the trust store.
    fn pending_trust(&mut self) -> PendingTrust {
        let permissions = match self.permissions_picker.is_open() {
            true => {
                self.permission_review = None;
                false
            }
            false => *self
                .permission_review
                .get_or_insert_with(|| self.permissions.needs_project_permission_config_trust()),
        };
        PendingTrust {
            permissions,
            mcp: !self.mcp_picker.is_open() && self.mcp_picker.has_awaiting_trust(),
        }
    }

    /// Opens whichever picker the trust chip names, MCP first as at startup.
    fn review_pending_trust(&mut self) {
        let pending = self.pending_trust();
        if pending.mcp {
            self.mcp_picker.open();
        } else if pending.permissions
            && let Err(error) = self.open_permissions_picker()
        {
            self.flash(error.to_string());
        }
    }

    fn refresh_permission_suggestions(&mut self) {
        let (project, revision, candidates) = self.permissions.pattern_proposal_inventory();
        self.permissions_picker
            .set_suggestions(&project, revision, &candidates);
    }

    fn finish_permission_suggestion_action(
        &mut self,
        result: Result<(), PermissionPolicyError>,
        success: &str,
    ) {
        self.refresh_permission_suggestions();
        self.flash(match result {
            Ok(()) => success.into(),
            Err(error) => format!("Could not hide suggested pattern: {error}"),
        });
    }

    fn handle_permissions_picker_action(&mut self, action: PermissionsPickerAction) -> Vec<Action> {
        match action {
            PermissionsPickerAction::Consumed => self.refresh_permission_suggestions(),
            PermissionsPickerAction::Close => self.suspend_permission_editor(),
            PermissionsPickerAction::RefreshDiscovery => {
                self.load_pattern_suggestions(PatternDiscoveryMode::Refresh)
            }
            PermissionsPickerAction::CancelDiscovery => {
                self.pending_pattern_suggestions = None;
                self.permissions_picker
                    .set_discovery(DiscoveryState::Cancelled);
            }
            PermissionsPickerAction::DismissSuggestion(target) => {
                let result = self.permissions.dismiss_pattern_proposal(
                    &target.project,
                    target.revision,
                    &target.definition_id,
                );
                self.finish_permission_suggestion_action(
                    result,
                    "Suggested pattern dismissed for this project",
                );
            }
            PermissionsPickerAction::SnoozeSuggestion(target) => {
                let result = self.permissions.snooze_pattern_proposal(
                    &target.project,
                    target.revision,
                    &target.definition_id,
                );
                self.finish_permission_suggestion_action(
                    result,
                    "Suggested pattern snoozed for 24 hours in this project",
                );
            }
            PermissionsPickerAction::TrustProjectConfig => {
                match self.permissions.trust_project_permission_config() {
                    Ok(()) => match self.open_permissions_picker() {
                        Ok(()) => self.flash("Project permission config trusted".into()),
                        Err(error) => {
                            self.permissions_picker.close();
                            self.flash(format!(
                                    "Project permission config trusted, but permissions could not be refreshed: {error}"
                                ));
                        }
                    },
                    Err(error) => {
                        let _ = self.open_permissions_picker();
                        self.flash(format!(
                            "Failed to trust project permission config: {error}"
                        ));
                    }
                }
            }
            PermissionsPickerAction::RevokeProjectConfigTrust => {
                match self.permissions.revoke_project_permission_config_trust() {
                    Ok(()) => match self.open_permissions_picker() {
                        Ok(()) => self.flash("Project permission config trust revoked".into()),
                        Err(error) => {
                            self.permissions_picker.close();
                            self.flash(format!(
                                "Project permission config trust revoked, but permissions could not be refreshed: {error}"
                            ));
                        }
                    },
                    Err(error) => {
                        let _ = self.open_permissions_picker();
                        self.flash(format!(
                            "Failed to revoke project permission config trust: {error}"
                        ));
                    }
                }
            }
            PermissionsPickerAction::Revoke(id) => {
                self.request_permission_revoke(id);
            }
            PermissionsPickerAction::Editor(event) => self.handle_permission_editor(event),
            PermissionsPickerAction::EditSource(locator) => self.open_permission_source(locator),
            PermissionsPickerAction::Copy(text) => self.copy_to_clipboard(&text),
        }
        Vec::new()
    }

    fn plan_toggle_ready(&self) -> bool {
        self.state.mode == Mode::Plan && self.state.plan.is_ready()
    }

    /// Single implementation behind both the default keybindings and
    /// `caudra.ui.action`, so a Lua rebind can never drift from the
    /// original key's behavior.
    pub(crate) fn run_builtin(&mut self, action: BuiltinAction) -> Vec<Action> {
        match action {
            BuiltinAction::CommandPalette => {
                let task_focused = !self.is_main_chat();
                let rows = self.command_palette.rows(task_focused);
                self.command_modal.open(rows);
            }
            BuiltinAction::FilePicker => {
                if let Some(workspace) = self.workspace_session.clone() {
                    if let Err(error) = self.file_picker.open_workspace(workspace) {
                        self.flash(error.to_string());
                    }
                } else {
                    self.file_picker.open(&self.state.session.cwd);
                }
            }
            BuiltinAction::Search => self.open_search(SearchTab::ThisChat, ""),
            BuiltinAction::Help => self.help_modal.toggle(),
            BuiltinAction::PlanToggle => {
                if self.plan_toggle_ready() {
                    self.plan_form.toggle();
                }
            }
            BuiltinAction::PlanEditor => self.open_plan(),
            BuiltinAction::CopyMessage => {
                let source = self.chats[self.active_chat].last_reply_source();
                let message = match source {
                    None => "Nothing to copy".to_owned(),
                    Some(text) => match self.clipboard.copy_text(&text) {
                        Ok(CopyResult::Noop) => "Nothing to copy".to_owned(),
                        Ok(CopyResult::Copied) => "Copied reply as markdown".to_owned(),
                        Err(e) => format!("Copy failed: {e}"),
                    },
                };
                self.status_bar.flash(message);
            }
            BuiltinAction::Review => match self.chats[self.active_chat].last_assistant_source() {
                Some(source) => self.open_review(source),
                None => self.flash(REVIEW_UNAVAILABLE_MSG.into()),
            },
            BuiltinAction::EditInput => self.open_prompt_draft(),
            BuiltinAction::PopQueue => {
                self.pop_active_queue();
            }
            BuiltinAction::PrevChat => {
                self.leave_active_chat();
                self.active_chat = self.active_chat.saturating_sub(1);
                self.sync_todo_panel();
            }
            BuiltinAction::NextChat => {
                self.leave_active_chat();
                self.active_chat = (self.active_chat + 1).min(self.chats.len() - 1);
                self.sync_todo_panel();
            }
            BuiltinAction::ModelPicker => return self.open_model_picker_here(),
            BuiltinAction::ViewToggle => {
                self.view = self.view.next();
                for chat in &mut self.chats {
                    chat.set_view(self.view);
                }
                caudra_storage::view::persist(&self.storage, self.view);
                self.flash(
                    match self.view {
                        ViewMode::Auto => AUTO_VIEW_MSG,
                        ViewMode::Compact => COMPACT_VIEW_MSG,
                        ViewMode::Expanded => EXPANDED_VIEW_MSG,
                    }
                    .into(),
                );
            }
            BuiltinAction::StashPush => return self.stash_push(),
            BuiltinAction::StashPop => return self.stash_pop(),
            BuiltinAction::StashList => return self.stash_list(),
            BuiltinAction::Workbench => {
                self.sync_workbench_theme();
                self.toggle_workbench();
            }
        }
        vec![]
    }

    /// The stored layout is read once per run: a reader who closed every tab
    /// and came back does not want them all opened again.
    fn toggle_workbench(&mut self) {
        if self.parked_workbench.is_some() {
            self.close_permission_source();
            return;
        }
        let cwd = PathBuf::from(&self.state.session.cwd);
        let opening = !self.workbench.is_open();
        if let Some(workspace) = self.workspace_session.clone() {
            if let Err(error) = self.workbench.toggle_workspace(workspace) {
                self.flash(error.to_string());
                return;
            }
        } else {
            self.workbench.toggle(&cwd);
        }
        if opening {
            self.sync_workbench_mounts();
        }
        if !opening || self.workbench_layout.is_some() {
            return;
        }
        if self.workspace_session.is_some() {
            self.workbench_layout = Some(WorkbenchLayout::default());
            return;
        }
        let layout: WorkbenchLayout =
            caudra_storage::workbench::read(&self.storage, &cwd).unwrap_or_default();
        self.workbench.restore(layout.clone());
        self.workbench_layout = Some(layout);
    }

    /// Opens source control on a commit the transcript named, the way a
    /// mention opens its file.
    pub(crate) fn open_workbench_commit(&mut self, commit: &CommitRef) {
        if self.workbench_lent() {
            return;
        }
        self.sync_workbench_theme();
        let cwd = PathBuf::from(&self.state.session.cwd);
        self.workbench.open_at_commit(&cwd, &commit.id);
    }

    /// Where a click on a mention lands, from the composer or the transcript.
    /// The stored layout is skipped: the reader asked for one file, and opening
    /// a session's worth of tabs around it would bury the answer.
    pub(crate) fn open_workbench_at(&mut self, mention: &Mention) {
        if let Some(path) = mention.remote_path() {
            if self.workbench_lent() {
                return;
            }
            self.sync_workbench_theme();
            self.workbench
                .open_remote_at(path.clone(), mention.lines.clone());
        } else if let Some(path) = mention.local_path() {
            self.open_workbench_file(path, mention.lines.clone());
        }
    }

    /// A file Caudra named itself, such as a workflow's scratch file, opened
    /// the way a mention is.
    pub(crate) fn open_workbench_file(
        &mut self,
        path: &Path,
        lines: Option<RangeInclusive<usize>>,
    ) {
        if self.workbench_lent() {
            return;
        }
        self.sync_workbench_theme();
        let cwd = PathBuf::from(&self.state.session.cwd);
        self.workbench.open_at(&cwd, path, lines);
    }

    /// The workbench paints from a palette resolved once, so a theme change
    /// while it is open has to be handed over rather than read mid render.
    fn sync_workbench_theme(&mut self) {
        let generation = crate::theme::generation();
        if self.workbench_theme_gen != generation {
            self.workbench_theme_gen = generation;
            self.workbench.set_styles(workbench_styles());
            if let Some(parked) = &mut self.parked_workbench {
                parked.set_styles(workbench_styles());
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Vec<Action> {
        self.handle_keyboard(key, None)
    }

    fn handle_keyboard(&mut self, key: KeyEvent, text: Option<Option<&str>>) -> Vec<Action> {
        if matches!(key.code, KeyCode::Modifier(_)) {
            return vec![];
        }
        if self.permission_prompt_holds_input() {
            if key.kind != KeyEventKind::Press || !self.claim_deciding_permission() {
                self.permission_prompt_keyboard(key, text);
            }
            return vec![];
        }
        if !self.permission_prompt.is_open() {
            self.permission_prompt.handle_key(key);
        }
        if key.kind == KeyEventKind::Press && self.open_badge_docs(key) {
            return vec![];
        }
        if self.permissions_picker.is_open()
            && self.permissions_picker.editor_mut().is_some()
            && !self.docs_modal.is_open()
        {
            if self.permission_mutation_pending() {
                return vec![];
            }
            if self
                .permissions_picker
                .editor_mut()
                .is_some_and(|editor| editor.is_editing())
                && let Some(text) = text_field::keyboard_text(key, text)
            {
                if !text.is_empty() {
                    self.permissions_picker.handle_text(text, false);
                }
                return vec![];
            }
            let Some(key) = text_field::command_key(key, text) else {
                return vec![];
            };
            let action = self.permissions_picker.handle_key(key);
            return self.handle_permissions_picker_action(action);
        }
        match key.kind {
            KeyEventKind::Release if self.peer_manager.is_open() && !self.docs_modal.is_open() => {
                let action = self.peer_manager.handle_key(key);
                return self.handle_peer_manager_action(action);
            }
            KeyEventKind::Release => return vec![],
            KeyEventKind::Repeat => return self.handle_key_repeat(key, text),
            KeyEventKind::Press => {}
        }
        #[cfg(debug_assertions)]
        if self.handle_grab_key(key) {
            return vec![];
        }
        self.clear_selection_unless_pending_copy();
        self.sync_subagent_input_target();
        self.chats[self.active_chat].clear_landing();

        if !key::EXIT.matches(key) {
            self.last_exit = None;
        }

        // Ahead of the overlays: a pending chord owns the next key outright, so
        // it can never leak into the composer or a picker behind the panel.
        if self.which_key.is_armed() {
            let Some(key) = text_field::command_key(key, text) else {
                return vec![];
            };
            self.which_key.disarm();
            return self.resolve_leader(keybindings::normalize_leader_key(key));
        }

        if let Some(actions) = self.dispatch_overlay(key, text) {
            self.last_exit = None;
            return actions;
        }

        let generated = text_field::keyboard_text(key, text);
        let Some(key) = text_field::command_key(key, text) else {
            return generated.map_or_else(Vec::new, |text| self.composer_keyboard_text(text));
        };

        // After the overlays, so the workbench editor keeps `Ctrl+X` for a cut
        // whenever there is a selection to cut.
        if key::LEADER.matches(key) {
            self.which_key.arm();
            return vec![];
        }

        if !(self.status == Status::Streaming && is_streaming_stop_key(key))
            && self.dispatch_override(key, false)
        {
            self.last_exit = None;
            return vec![];
        }

        if let Some(actions) = self.handle_global_key(key) {
            return actions;
        }

        if let Some(text) = generated {
            return self.composer_keyboard_text(text);
        }

        // Anything the transcript did not claim is on its way to the composer,
        // so the navigation keys go back with it.
        self.key_focus = KeyFocus::Composer;

        if key::THINKING.matches(key) || is_shift_tab(key) {
            self.cycle_reasoning_effort();
            return vec![];
        }

        // Cut is a copy plus the delete the composer already performs, so it
        // only has to take the copy on the way past. Ahead of the main/task
        // split so both composers cut the same way.
        if key::CUT.matches(key)
            && self.composer_holds_keys()
            && let Some(text) = self.active_input_box().selected_text()
        {
            self.copy_to_clipboard(&text);
        }

        // Ahead of the main/task split so the editing chords answer the same
        // way in every chat instead of only in the transcript.
        if is_ctrl(&key) {
            return self.handle_ctrl_key(key);
        }

        if !self.is_main_chat() {
            if self.active_subagent_has_composer() {
                return self.handle_subagent_chat_key(key);
            }
            return match key.code {
                KeyCode::Enter if !self.active_queue_entries().is_empty() => {
                    self.focus_active_queue();
                    vec![]
                }
                KeyCode::Tab if !self.is_bash_input() => self.toggle_mode(),
                KeyCode::Esc if !self.chats[self.active_chat].is_finished() => {
                    if let Some(t) = self.last_esc.take()
                        && t.elapsed() < CONFIRMATION_DURATION
                    {
                        self.handle_subagent_cancel()
                    } else {
                        self.last_esc = Some(Instant::now());
                        self.status_bar
                            .flash_for(FLASH_CANCEL.into(), CONFIRMATION_DURATION);
                        vec![]
                    }
                }
                _ => vec![],
            };
        }

        self.handle_main_chat_key(key)
    }

    fn handle_key_repeat(&mut self, key: KeyEvent, text: Option<Option<&str>>) -> Vec<Action> {
        if self.which_key.is_armed() {
            return vec![];
        }
        self.clear_selection_unless_pending_copy();
        self.sync_subagent_input_target();
        if let Some(actions) = self.dispatch_overlay(key, text) {
            return actions;
        }
        if let Some(text) = text_field::keyboard_text(key, text) {
            return self.composer_keyboard_text(text);
        }
        let Some(key) = text_field::command_key(key, text) else {
            return vec![];
        };
        if self.scroll_transcript(key)
            || !self.composer_is_interactive()
            || !Self::repeat_key_allowed(key, true)
        {
            return vec![];
        }
        self.key_focus = KeyFocus::Composer;
        // The same branch the press path takes, so a held word chord reaches
        // the composer by the route its first press took. Only the chords
        // `repeat_key_allowed` passed get here, and none of them is one of the
        // commands `handle_ctrl_key` checks first.
        if is_ctrl(&key) {
            return self.handle_ctrl_key(key);
        }
        if self.is_main_chat() {
            self.handle_main_chat_key(key)
        } else {
            self.handle_subagent_chat_key(key)
        }
    }

    fn composer_keyboard_text(&mut self, text: &str) -> Vec<Action> {
        if self.composer_is_interactive() && !text.is_empty() {
            self.key_focus = KeyFocus::Composer;
            if let InputAction::PaletteSync(value) =
                self.active_input_box_mut().handle_keyboard_text(text)
            {
                self.sync_dropdowns(&value);
            }
        }
        vec![]
    }

    /// A held key keeps acting when holding it means something: a navigation
    /// key, or, while a field is being typed in, whatever the field keymap
    /// repeats. Reading the keymap keeps the two from drifting apart. Every
    /// other chord runs a command, and a command must answer a press rather
    /// than a hold.
    fn repeat_key_allowed(key: KeyEvent, editing: bool) -> bool {
        let navigation = matches!(
            key.code,
            KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
        );
        if navigation
            && (key.modifiers.is_empty() || (editing && key.modifiers == KeyModifiers::SHIFT))
        {
            return true;
        }
        editing && text_field::decode(key, FieldKind::Block).is_some_and(TextCommand::repeats)
    }

    /// The key the leader was waiting for. `Esc` and `Ctrl+C` back out
    /// quietly; anything unbound says so rather than acting on a typo.
    fn resolve_leader(&mut self, key: KeyEvent) -> Vec<Action> {
        if key.code == KeyCode::Esc || key::QUIT.matches(key) {
            return vec![];
        }

        // The workbench answers first so a pane can claim a letter the
        // transcript spends elsewhere, and passes back what it does not want.
        if self.workbench.is_open() {
            match self.workbench_leader(key) {
                WorkbenchAction::Passthrough => {}
                action => return self.handle_workbench_action(action),
            }
        }

        if self.dispatch_override(key, true) {
            return vec![];
        }

        for (bind, builtin) in [
            (leader::COPY_MESSAGE, BuiltinAction::CopyMessage),
            (leader::EDIT_INPUT, BuiltinAction::EditInput),
            (leader::FILE_PICKER, BuiltinAction::FilePicker),
            (leader::HELP, BuiltinAction::Help),
            (leader::MODEL_PICKER, BuiltinAction::ModelPicker),
            (leader::PLAN_EDITOR, BuiltinAction::PlanEditor),
            (leader::POP_QUEUE, BuiltinAction::PopQueue),
            (leader::REVIEW, BuiltinAction::Review),
            (leader::STASH_POP, BuiltinAction::StashPop),
            (leader::STASH_PUSH, BuiltinAction::StashPush),
            (leader::VIEW_TOGGLE, BuiltinAction::ViewToggle),
            (leader::WORKBENCH, BuiltinAction::Workbench),
        ] {
            if bind.matches(key) {
                return self.run_builtin(builtin);
            }
        }

        if leader::TASKS.matches(key) {
            return self.tasks_browse();
        }
        if leader::CYCLE_FAVORITE.matches(key) {
            return self.cycle_favorite_model();
        }
        if leader::CYCLE_DECISION.matches(key) {
            return self.cycle_decision_endpoint(ProvidersConfig::load);
        }
        if leader::ACCOUNTS.matches(key) {
            self.open_accounts();
            return vec![];
        }
        if leader::WORKFLOWS.matches(key) {
            return self.execute_workflow("");
        }
        if leader::SESSION_PICKER.matches(key) {
            return self.sessions_browse();
        }
        if leader::NEW_SESSION.matches(key) {
            return vec![Action::RequestNewSession];
        }
        if leader::PLAN_TOGGLE.matches(key) {
            self.toggle_plan_or_todo();
            return vec![];
        }
        if leader::UNANSWERED_QUESTION.matches(key) {
            self.reopen_question(None);
            return vec![];
        }
        if leader::SCROLL_BOTTOM.matches(key) {
            self.active_chat().enable_auto_scroll();
            return vec![];
        }
        let steerable_task = self.active_task_is_steerable();
        if steerable_task || self.prompt_defers() {
            for (bind, admission) in [
                (leader::STEER_PROMPT, PromptAdmission::Steer),
                (leader::INTERRUPT_PROMPT, PromptAdmission::Interrupt),
            ] {
                if bind.matches(key) {
                    return if steerable_task {
                        self.handle_task_admission(admission)
                    } else {
                        self.handle_deferred_admission(admission)
                    };
                }
            }
        }

        let pressed = keybindings::key_event_to_string(&key);
        self.flash(format!("{} {pressed} {FLASH_NO_CHORD}", key::LEADER.label));
        vec![]
    }

    /// `Ctrl+T` and `Ctrl+X t` both mean "show me the plan", and which panel
    /// that is depends on whether a plan is ready or a todo list is live.
    fn toggle_plan_or_todo(&mut self) {
        if self.is_main_chat() && self.plan_toggle_ready() {
            self.run_builtin(BuiltinAction::PlanToggle);
            return;
        }
        self.todo_panel.toggle();
    }

    /// Which chords the panel offers, which is the transcript's set unless the
    /// workbench has taken the screen.
    pub(super) fn leader_contexts(&self) -> Vec<KeybindContext> {
        if self.workbench.is_open() {
            return vec![
                KeybindContext::Workbench,
                KeybindContext::WorkbenchEditor,
                KeybindContext::WorkbenchSourceControl,
                KeybindContext::WorkbenchSearch,
            ];
        }
        let mut contexts = vec![KeybindContext::General, KeybindContext::Editing];
        if self.prompt_defers() || self.active_task_is_steerable() {
            contexts.push(KeybindContext::Streaming);
        }
        contexts
    }

    fn handle_subagent_chat_key(&mut self, key: KeyEvent) -> Vec<Action> {
        let command_action = self.command_palette.handle_key(key);
        if let Some(actions) = self.handle_command_action(command_action) {
            return actions;
        }

        match self.subagent_input_box.handle_key(key) {
            InputAction::Submit(sub) => self.handle_subagent_submit(sub),
            InputAction::EditPaste(id) => {
                self.open_paste_editor(id);
                vec![]
            }
            InputAction::PaletteSync(val) => {
                self.sync_dropdowns(&val);
                vec![]
            }
            InputAction::CaretMoved => {
                self.resync_command_palette();
                vec![]
            }
            InputAction::Passthrough(key) => match key.code {
                KeyCode::Esc => {
                    if let Some(t) = self.last_esc.take()
                        && t.elapsed() < CONFIRMATION_DURATION
                    {
                        self.handle_subagent_cancel()
                    } else {
                        self.last_esc = Some(Instant::now());
                        self.status_bar
                            .flash_for(FLASH_CANCEL.into(), CONFIRMATION_DURATION);
                        vec![]
                    }
                }
                _ => vec![],
            },
            InputAction::ContinueLine | InputAction::None => vec![],
        }
    }

    /// Editing chords belong to whichever composer is on screen, so the
    /// transcript and a focused task get the same set rather than the task
    /// view silently dropping half of them. The plan editor and search need no
    /// composer; the rest would otherwise type into a box nobody is drawing.
    fn handle_ctrl_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if key::OPEN_EDITOR.matches(key) {
            return self.run_builtin(BuiltinAction::PlanEditor);
        }
        if key::SEARCH.matches(key) {
            return self.run_builtin(BuiltinAction::Search);
        }
        if !self.composer_is_visible() {
            return vec![];
        }
        if key::FILE_PICKER.matches(key) {
            return self.run_builtin(BuiltinAction::FilePicker);
        }
        if key.code == KeyCode::Char('v') && self.image_paste_rx.is_empty() {
            self.start_image_paste();
        } else {
            match self.active_input_box_mut().handle_key(key) {
                InputAction::PaletteSync(val) => self.sync_dropdowns(&val),
                InputAction::CaretMoved => self.resync_command_palette(),
                _ => {}
            }
        }
        vec![]
    }

    /// Whether a composer is drawn, which is what
    /// [`crate::app::view`] renders on and therefore what may take text.
    fn composer_is_visible(&self) -> bool {
        self.is_main_chat() || self.active_subagent_has_composer() || self.queue_editor_active()
    }

    fn handle_subagent_submit(&mut self, sub: Submission) -> Vec<Action> {
        self.submit_to_task(sub, PromptAdmission::Steer)
    }

    /// Whether the task on screen is running and so takes guidance or a
    /// replacement rather than a continuation.
    pub(super) fn active_task_is_steerable(&self) -> bool {
        !self.is_main_chat()
            && self
                .active_subagent_id()
                .is_some_and(|id| self.subagent_steers.contains_key(id))
    }

    /// `Ctrl+X g` and `Ctrl+X x`, and their controls, in a task's chat.
    pub(super) fn handle_task_admission(&mut self, admission: PromptAdmission) -> Vec<Action> {
        if self.queue_editor_active() {
            return vec![];
        }
        let Some(sub) = self.subagent_input_box.take_submission() else {
            return vec![];
        };
        self.submit_to_task(sub, admission)
    }

    fn submit_to_task(&mut self, sub: Submission, admission: PromptAdmission) -> Vec<Action> {
        if self.intercept_inspection_submission(&sub.text) {
            return vec![];
        }
        if sub.images.is_empty() && sub.text.trim() == "/queue" {
            self.focus_active_queue();
            return vec![];
        }
        if sub.text.trim().is_empty() && sub.images.is_empty() {
            return vec![];
        }
        let Some(task_id) = self.active_subagent_id().map(str::to_owned) else {
            return vec![];
        };
        self.send_to_task(&task_id, sub, admission)
    }

    /// The one place a prompt reaches a running task, shared by the composer
    /// and by commands that expand into a prompt, so neither can drift from
    /// the queue bookkeeping the panel renders from. Guidance to a task that
    /// stopped continues it; a replacement needs it still running and
    /// otherwise returns the draft to the composer.
    fn send_to_task(
        &mut self,
        task_id: &str,
        sub: Submission,
        admission: PromptAdmission,
    ) -> Vec<Action> {
        let continuing = self
            .task_continuation
            .as_ref()
            .is_some_and(|pending| pending.task_id() == task_id);
        let tx = match self.subagent_steers.get(task_id) {
            Some(tx) if !continuing => tx.clone(),
            _ if admission == PromptAdmission::Interrupt => {
                self.subagent_input_box
                    .set_state(InputState::new(sub.draft, sub.images));
                self.flash(TASK_NOT_RUNNING.into());
                return vec![];
            }
            _ => {
                self.start_task_continuation(task_id, sub);
                return vec![];
            }
        };
        let Submission {
            text,
            images,
            mentions,
            commits,
            draft,
        } = sub;
        let input = AgentInput {
            message: text.clone(),
            mode: AgentMode::Build,
            plan: self.state.plan.target(),
            images,
            mentions,
            commits,
            preamble: Vec::new(),
            thinking: self.state.thinking.clone(),
            fast: self.state.fast,
            prompt: None,
            resume: false,
        };
        let pending = self
            .pending_subagent_steers
            .entry(task_id.to_owned())
            .or_default();
        if admission != PromptAdmission::Interrupt {
            let id = tx.push(input);
            pending.push_back(PendingSteer {
                id,
                text,
                draft,
                from_agent: false,
            });
            return vec![];
        }
        let replacement = tx.replace(input);
        pending.push_back(PendingSteer {
            id: replacement.id,
            text,
            draft,
            from_agent: false,
        });
        self.adopt_task_replacement(task_id, replacement.id, replacement.superseded);
        vec![]
    }

    /// Lists guidance the agent queued on a running task beside the user's
    /// own, unless the task already read it.
    fn task_guidance_queued(
        &mut self,
        task_id: &str,
        id: QueueItemId,
        text: String,
        interrupt: bool,
        superseded: Option<QueueItemId>,
    ) {
        if self
            .subagent_steers
            .get(task_id)
            .is_some_and(|queue| queue.entries().iter().all(|entry| entry.id != id))
        {
            return;
        }
        let draft = InputDraft {
            text: text.clone(),
            paste_ranges: Vec::new(),
        };
        self.pending_subagent_steers
            .entry(task_id.to_owned())
            .or_default()
            .push_back(PendingSteer {
                id,
                text,
                draft,
                from_agent: true,
            });
        if interrupt {
            self.adopt_task_replacement(task_id, id, superseded);
        }
        self.clamp_active_queue_focus();
    }

    /// Records `id` as the task's replacement, which leads its queue the way
    /// Replacing leads the panel, so moving guidance never crosses it.
    pub(super) fn adopt_task_replacement(
        &mut self,
        task_id: &str,
        id: QueueItemId,
        superseded: Option<QueueItemId>,
    ) {
        if let Some(pending) = self.pending_subagent_steers.get_mut(task_id) {
            pending.retain(|item| Some(item.id) != superseded);
            if let Some(item) = pending
                .iter()
                .position(|item| item.id == id)
                .and_then(|index| pending.remove(index))
            {
                pending.push_front(item);
            }
        }
        self.subagent_replacements.insert(task_id.to_owned(), id);
        self.release_task_interactions(task_id);
    }

    /// A replaced turn takes its open question and auth wait with it, as a
    /// cancelled task does, though the task itself keeps running.
    pub(super) fn release_task_interactions(&mut self, task_id: &str) {
        self.clear_auth_waiter(Some(task_id));
        if self.question_subagent.as_deref() == Some(task_id) {
            self.close_blocking_question();
        }
    }

    fn intercept_inspection_submission(&mut self, text: &str) -> bool {
        let (token, args) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        if self.refuse_disabled_command(token) {
            return true;
        }
        if token.eq_ignore_ascii_case("/decisions") && args.trim().is_empty() {
            self.execute_decisions();
            return true;
        }
        if token.eq_ignore_ascii_case("/context") {
            self.execute_context(args);
            return true;
        }
        if token.eq_ignore_ascii_case("/tools") {
            if args.trim().is_empty() {
                self.execute_tools();
            } else {
                self.flash(TOOLS_USAGE.into());
            }
            return true;
        }
        if token.eq_ignore_ascii_case("/skills") {
            if args.trim().is_empty() {
                self.execute_skills();
            } else {
                self.flash(SKILLS_USAGE.into());
            }
            return true;
        }
        false
    }

    /// Shared by `/usage` and the footer's spend control, so a click cannot
    /// open the modal without the lifetime figures the command loads.
    fn toggle_usage_modal(&mut self) -> Vec<Action> {
        self.usage_modal.toggle();
        if self.usage_modal.is_open() {
            self.warming_summary = None;
            self.load_lifetime_usage();
            self.reload_usage_accounts();
            vec![Action::RefreshUsage]
        } else {
            vec![]
        }
    }

    fn execute_context(&mut self, args: &str) {
        let expanded = match args.trim() {
            "" => false,
            arg if arg.eq_ignore_ascii_case("all") => true,
            _ => {
                self.flash(CONTEXT_USAGE.into());
                return;
            }
        };
        self.context_snapshot = Watch::seeded(self.active_context_snapshot());
        self.context_modal.open(expanded);
    }

    /// Measuring is a directory walk, so the modal opens on `Loading` and the
    /// answer arrives through the slot rather than blocking the command.
    fn execute_storage(&mut self, args: &str) -> Vec<Action> {
        let expanded = match args.trim() {
            "" => false,
            arg if arg.eq_ignore_ascii_case("all") => true,
            _ => {
                self.flash(STORAGE_USAGE.into());
                return vec![];
            }
        };
        self.storage_modal.open(expanded);
        vec![Action::RefreshStorage]
    }

    fn handle_logs_action(&mut self, action: LogsAction) {
        match action {
            LogsAction::Consumed => {}
            LogsAction::Close => self.logs_modal.close(),
            LogsAction::Flash(message) => self.flash(message.into()),
            LogsAction::Copy { text, label } => self.copy_labelled(&text, label),
        }
    }

    /// The docs take every input while open, whatever they were opened over,
    /// short of a permission prompt that holds it.
    pub(super) fn docs_holds_input(&self) -> bool {
        self.docs_modal.is_open() && !self.permission_prompt_holds_input()
    }

    /// Opens over the modal that asked, which stays open beneath so closing
    /// the docs returns to it.
    pub(super) fn open_docs(&mut self, topic: DocsTopic) {
        self.docs_modal.open_at(topic.address());
    }

    /// `F1` on a modal that links to the docs opens them, as its `?` does.
    /// Help keeps `F1` as the key that closes it.
    fn open_badge_docs(&mut self, key: KeyEvent) -> bool {
        if !key::HELP.matches(key)
            || self.help_modal.is_open()
            || self.docs_modal.is_open()
            || self.which_key.is_armed()
        {
            return false;
        }
        let Some((topic, _)) = self.docs_badge else {
            return false;
        };
        self.open_docs(topic);
        true
    }

    fn handle_docs_action(&mut self, action: DocsAction) -> Vec<Action> {
        match action {
            DocsAction::Consumed => {}
            DocsAction::Close => self.docs_modal.close(),
            DocsAction::Flash(message) => self.flash(message.into()),
            DocsAction::Copy { text, label } => self.copy_labelled(&text, label),
            DocsAction::OpenUrl(url) => return vec![Action::OpenUrl(url)],
        }
        vec![]
    }

    /// A link into the docs opens there, above the notes; any other goes to
    /// the browser.
    pub(super) fn handle_changelog_action(&mut self, action: ChangelogAction) -> Vec<Action> {
        match action {
            ChangelogAction::Consumed => {}
            ChangelogAction::Close { auto } => {
                self.changelog_modal.close();
                if auto {
                    self.flash(changelog_modal::REOPEN_HINT.into());
                }
            }
            ChangelogAction::Copy { text, label } => self.copy_labelled(&text, label),
            ChangelogAction::Follow(target) if self.docs_modal.knows(&target) => {
                self.docs_modal.open(&target);
            }
            ChangelogAction::Follow(target) => return vec![Action::OpenUrl(target)],
        }
        vec![]
    }

    /// `/changelog` opens on the releases since the one run before this,
    /// which only holds while this is the release the mark was last advanced
    /// to; `/changelog all` opens on every release.
    fn execute_changelog(&mut self, args: &str) {
        let view = match args.trim() {
            "" => ChangelogView::Since,
            arg if arg.eq_ignore_ascii_case("all") => ChangelogView::All,
            _ => {
                self.flash(CHANGELOG_USAGE.into());
                return;
            }
        };
        let since = caudra_storage::changelog::read(&self.storage)
            .filter(|mark| mark.seen == update::CURRENT)
            .and_then(|mark| mark.since);
        caudra_storage::changelog::acknowledge(&self.storage);
        self.changelog_modal.open(view, since);
    }

    /// Shows the notes owed after an update, claimed by this start.
    pub(crate) fn open_changelog_after_update(&mut self, since: Option<String>) {
        self.changelog_modal.open_after_update(since);
    }

    /// Copies `text` and says so in `label`'s words. An empty copy stays
    /// quiet, since there was nothing to hand over.
    fn copy_labelled(&mut self, text: &str, label: &str) {
        match self.clipboard.copy_text(text) {
            Ok(CopyResult::Noop) => {}
            Ok(CopyResult::Copied) => self.flash(label.into()),
            Err(e) => self.flash(format!("{COPY_FAILED}{e}")),
        }
    }

    fn execute_tools(&mut self) {
        self.tools_modal.open();
        let _ = self.poll_tools_snapshot();
        self.load_tool_stats();
    }

    fn execute_skills(&mut self) {
        self.context_snapshot = Watch::seeded(self.active_context_snapshot());
        self.skills_modal.open();
    }

    /// Takes profiles read again from disk: the picker lists them, and the next
    /// agent this tab spawns, after `/new` or `/cd`, builds from the edited
    /// copy of its profile rather than the one the tab started with.
    pub(crate) fn adopt_prompt_profiles(&mut self, catalog: &Arc<PromptProfileCatalog>) {
        catalog.refresh(&mut self.state.system_prompt_profile);
        self.prompt_profile_picker.set_catalog(Arc::clone(catalog));
    }

    /// Shows the prompt the run bound, which is empty only until the agent has
    /// published one. Rebuilding it here would show a prompt nothing was sent,
    /// so an edit to its profile since is noted above it instead.
    fn execute_system_prompt(&mut self) {
        let bound = self.btw_prompt.as_ref().map(|prompt| prompt.load_full());
        let system = bound
            .as_ref()
            .map(|prompt| Arc::from(prompt.system.as_str()))
            .unwrap_or_else(|| Arc::from(""));
        let edited = bound
            .is_some_and(|prompt| prompt.outdated_by(self.state.system_prompt_profile.as_deref()));
        self.system_prompt_modal
            .open(system, &self.state.system_prompt_profile_name, edited);
    }

    fn handle_system_prompt_action(&mut self, action: SystemPromptAction) {
        match action {
            SystemPromptAction::Consumed => {}
            SystemPromptAction::Copy { text, label } => self.copy_labelled(&text, label),
            SystemPromptAction::Profile => {
                self.system_prompt_modal.close();
                self.prompt_profile_picker
                    .open(&self.state.system_prompt_profile_name);
            }
        }
    }

    /// Shows the next request as the live turn would project it, taken now so
    /// the view never shifts: reopening is how to see a newer one. Nothing is
    /// shown before the agent has bound a prompt, since no request exists yet.
    fn execute_projection(&mut self) {
        let prompt = self
            .btw_prompt
            .as_ref()
            .map(|prompt| prompt.load_full())
            .filter(|prompt| !prompt.system.is_empty());
        let snapshot = self
            .shared_history
            .as_ref()
            .map(|history| history.load_full());
        let (Some(prompt), Some(snapshot)) = (prompt, snapshot) else {
            self.projection_modal.open(None);
            return;
        };
        let history = match project_messages(&snapshot.messages) {
            Ok(history) => history,
            Err(error) => {
                self.flash(format!("{HISTORY_UNREADABLE}{error}"));
                return;
            }
        };
        let transport = prompt.provider.reasoning_transport(&prompt.model);
        let projected = project_for_inspection(&history, &prompt.tools, &prompt.model, transport);
        self.projection_modal.open(Some(Projection {
            prompt,
            messages: projected.messages,
            running_calls: projected.running_calls,
            cache_key: CacheKey::session(&SessionRef::from(self.state.session.id)),
            opts: RequestOptions {
                thinking: self.state.thinking.clone(),
                fast: self.state.fast,
                ..RequestOptions::default()
            },
        }));
    }

    fn handle_projection_action(&mut self, action: ProjectionAction) {
        match action {
            ProjectionAction::Consumed => {}
            ProjectionAction::Copy { text, label } => self.copy_labelled(&text, label),
        }
    }

    /// Withdraws the user's unread steers into the unsent list. The agent's
    /// stay queued: the task's settlement hands them back to the agent.
    fn preserve_unconsumed_steers(&mut self, task_id: &str) {
        self.subagent_replacements.remove(task_id);
        self.subagent_controls.remove(task_id);
        let queue = self.subagent_steers.remove(task_id);
        let Some(pending) = self.pending_subagent_steers.remove(task_id) else {
            return;
        };
        let unsent: VecDeque<_> = pending
            .into_iter()
            .filter(|item| {
                !item.from_agent
                    && queue
                        .as_ref()
                        .is_some_and(|queue| queue.remove(item.id).is_some())
            })
            .collect();
        if !unsent.is_empty() {
            self.unsent_subagent_steers
                .entry(task_id.to_owned())
                .or_default()
                .extend(unsent);
            self.flash(STEER_NOT_CONSUMED_MSG.into());
        }
    }

    fn preserve_all_unconsumed_steers(&mut self) {
        let task_ids: Vec<_> = self.pending_subagent_steers.keys().cloned().collect();
        for task_id in task_ids {
            self.preserve_unconsumed_steers(&task_id);
        }
    }

    fn dispatch_override(&self, key: KeyEvent, leader: bool) -> bool {
        let snap = self.keymap_reader.load();
        for entry in &snap.entries {
            if entry.leader == leader
                && entry.key == key.code
                && entry.modifiers == key.modifiers
                && self.lua_event_handle.run_keybind_callback(entry.id)
            {
                return true;
            }
        }
        false
    }

    fn handle_main_chat_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if key.code == KeyCode::Enter
            && key.modifiers.is_empty()
            && let Some(id) = self.input_box.buffer.focused_paste()
        {
            self.open_paste_editor(id);
            return vec![];
        }

        let mention_action = self.mention_popup.handle_key(key);
        if let Some(actions) = self.handle_mention_action(mention_action) {
            return actions;
        }

        let commit_action = self.commit_popup.handle_key(key);
        if let Some(actions) = self.handle_commit_action(commit_action) {
            return actions;
        }

        let command_action = self.command_palette.handle_key(key);
        if let Some(actions) = self.handle_command_action(command_action) {
            return actions;
        }

        if key.code == KeyCode::Enter
            && key.modifiers.is_empty()
            && self.input_box.has_pastes()
            && self.input_box.buffer.focused_paste().is_none()
            && shell::parse_shell_prefix(&self.input_box.expanded_text()).is_some()
        {
            self.input_box.expand_pastes();
            self.command_palette.close();
            self.flash(SHELL_PASTE_EXPANDED_MSG.into());
            return vec![];
        }

        let streaming = self.status == Status::Streaming;
        match self.input_box.handle_key(key) {
            InputAction::Submit(sub) => self.handle_submit(sub),
            InputAction::EditPaste(id) => {
                self.open_paste_editor(id);
                vec![]
            }
            InputAction::PaletteSync(val) => {
                self.sync_dropdowns(&val);
                vec![]
            }
            InputAction::CaretMoved => {
                self.resync_command_palette();
                vec![]
            }
            InputAction::Passthrough(key) => {
                if key.code != KeyCode::Esc {
                    self.last_esc = None;
                }
                match key.code {
                    KeyCode::Up if streaming => {
                        self.active_chat().scroll(1);
                        vec![]
                    }
                    KeyCode::Down if streaming => {
                        self.active_chat().scroll(-1);
                        vec![]
                    }
                    KeyCode::Tab if !self.is_bash_input() => self.toggle_mode(),
                    KeyCode::Esc => {
                        if let Some(t) = self.last_esc.take()
                            && t.elapsed() < CONFIRMATION_DURATION
                        {
                            if streaming || self.has_session_work() {
                                self.user_stopped(AutopilotStopper::Interrupt);
                                self.handle_cancel()
                            } else {
                                self.open_rewind_picker()
                            }
                        } else {
                            self.last_esc = Some(Instant::now());
                            self.status_bar.flash_for(
                                if streaming || self.has_session_work() {
                                    FLASH_CANCEL
                                } else {
                                    FLASH_REWIND
                                }
                                .into(),
                                CONFIRMATION_DURATION,
                            );
                            vec![]
                        }
                    }
                    _ => vec![],
                }
            }
            InputAction::ContinueLine | InputAction::None => vec![],
        }
    }

    fn flash_usage(&mut self, usage: &str) -> Vec<Action> {
        self.flash(usage.into());
        vec![]
    }

    fn quit(&mut self) -> Vec<Action> {
        self.quit_with(ExitRequest::Success)
    }

    fn quit_with(&mut self, req: ExitRequest) -> Vec<Action> {
        self.save_input_history();
        self.flush_goal_log();
        self.exit_request = req;
        vec![Action::ManualExit]
    }

    pub(crate) fn clear_exit_request(&mut self) {
        self.exit_request = ExitRequest::None;
    }

    pub(crate) fn handle_submit(&mut self, sub: Submission) -> Vec<Action> {
        self.handle_submit_with_admission(sub, PromptAdmission::Queue)
    }

    fn handle_deferred_admission(&mut self, admission: PromptAdmission) -> Vec<Action> {
        if !self.is_main_chat() || !self.prompt_defers() || self.queue_editor_active() {
            return Vec::new();
        }
        if !self.queue.is_connected() {
            self.flash(queue::NO_QUEUE_ERR.into());
            return Vec::new();
        }
        if admission == PromptAdmission::Interrupt
            && self.cancelling_run.is_some()
            && self.replacement_item.is_none()
        {
            self.flash(queue::REPLACE_BUSY_ERR.into());
            return Vec::new();
        }
        let submission = self
            .input_box
            .take_submission()
            .unwrap_or_else(Submission::empty);
        self.handle_submit_with_admission(submission, admission)
    }

    fn handle_submit_with_admission(
        &mut self,
        sub: Submission,
        admission: PromptAdmission,
    ) -> Vec<Action> {
        match std::mem::take(&mut self.pending_input) {
            PendingInput::AuthRetry { waiters } => {
                for subagent_id in waiters {
                    if subagent_id
                        .as_ref()
                        .and_then(|id| self.task_interactions.auth.get(id))
                        .is_some_and(|origin| !self.task_response_current(origin))
                    {
                        continue;
                    }
                    self.send_to_agent(subagent_id.as_deref(), String::new());
                }
                self.task_interactions.auth.clear();
                return vec![];
            }
            PendingInput::None => {}
        }
        if sub.is_empty() {
            return vec![];
        }
        if self.intercept_inspection_submission(&sub.text) {
            return vec![];
        }
        if sub.draft.paste_ranges.is_empty() && sub.text.trim() == "exit" {
            return self.quit();
        }

        if let Some(prefix) = shell::parse_shell_prefix(&sub.text) {
            if let Some(reason) = self.sandbox_network_dispatch_blocker() {
                self.flash(reason.into());
                return Vec::new();
            }
            let cmd = prefix.command.trim();
            if cmd == "cd" || cmd.starts_with("cd ") {
                self.flash("Only /cd can change the working directory".into());
            }
            let id = self.shell.reserve_id();
            let sigil = if prefix.visible { "!" } else { "!!" };
            let display = format!("{sigil} {}", prefix.command);
            self.main_chat().show_user_message(display);
            return vec![Action::ShellCommand {
                id,
                command: prefix.command,
                visible: prefix.visible,
            }];
        }
        let draft = sub.draft.clone();
        let images = sub.images.clone();
        let outcome = self.submit_prompt_with_admission(sub.into(), admission);
        let preserve_draft = matches!(
            outcome,
            SubmitOutcome::NeedsModeDecision(_) | SubmitOutcome::Rejected(_)
        ) || matches!(&outcome, SubmitOutcome::Started(actions) if actions.is_empty());
        let actions = self.handle_submit_outcome(outcome);
        if preserve_draft {
            self.input_box.set_state(InputState::new(draft, images));
        }
        actions
    }

    pub(crate) fn handle_cancel(&mut self) -> Vec<Action> {
        if let Err(error) = self.poll_background_delivery() {
            self.flash(error.to_string());
        }
        if self.background_delivery.fence.admission_error().is_some() {
            self.stop_background_work();
            return Vec::new();
        }
        if self.cancelling_run.is_some() {
            return Vec::new();
        }
        let cancelled_run = self.cancel_main_run();
        vec![Action::CancelAgent {
            run_id: cancelled_run,
        }]
    }

    pub(super) fn begin_main_cancel(&mut self, preserve_queue: bool, await_terminal: bool) -> u64 {
        self.automatic_wakes_suppressed = true;
        self.release_background_claims();
        self.workflow.suppress_completions();
        self.cancel_queue_edit();
        let cancelled_run = self.run_id;
        self.run_id += 1;
        self.cancelling_run = await_terminal.then_some(cancelled_run);
        self.close_all_overlays();
        self.pending_input = PendingInput::None;
        self.finish_subagents(TaskOutcome::Killed, CANCELLED_TEXT);
        self.subagent_answers.clear();
        self.question_subagent = None;
        self.preserve_all_unconsumed_steers();
        self.subagent_steers.clear();
        self.subagent_controls.clear();
        self.pending_subagent_steers.clear();
        self.subagent_replacements.clear();
        self.shell.cancel_all();
        for chat in &mut self.chats {
            chat.flush();
            chat.cancel_in_progress();
            chat.clear_retry();
        }
        self.sync_todo_panel();
        self.main_chat()
            .push(DisplayMessage::new(DisplayRole::Error, CANCEL_MSG.into()));
        if !preserve_queue {
            self.queue.clear();
            self.state.goal.take_kickoff();
            self.replacement_item = None;
            if !await_terminal {
                self.status = Status::Idle;
            }
        }
        self.recoverable_queue.clear();
        self.recoverable_queue_together = false;
        cancelled_run
    }

    fn handle_subagent_cancel(&mut self) -> Vec<Action> {
        let Some(task_id) = self.active_subagent_id().map(str::to_owned) else {
            return vec![];
        };
        self.cancel_subagent(task_id)
    }

    /// Keyed by task id rather than by the chat on screen: a subagent's
    /// question is docked over whatever chat the user is looking at, so the
    /// one being cancelled is not always the active one.
    fn cancel_subagent(&mut self, task_id: String) -> Vec<Action> {
        let Some(index) = self
            .chats
            .iter()
            .position(|chat| chat.task_id().is_some_and(|id| &**id == task_id.as_str()))
        else {
            return vec![];
        };

        self.chats[index].flush();
        self.chats[index].cancel_in_progress();
        self.chats[index].mark_finished(TaskOutcome::Killed, CANCELLED_TEXT);
        self.sync_subagents();
        self.sync_todo_panel();
        self.subagent_answers.remove(&task_id);
        self.clear_auth_waiter(Some(&task_id));
        self.preserve_unconsumed_steers(&task_id);
        // The tool the form was parked on is gone, so the form goes with it.
        if self.question_subagent.as_deref() == Some(task_id.as_str()) {
            self.close_blocking_question();
        }

        vec![Action::CancelSubagent {
            tool_use_id: task_id,
        }]
    }

    fn clear_auth_waiter(&mut self, subagent_id: Option<&str>) {
        let empty = match &mut self.pending_input {
            PendingInput::AuthRetry { waiters } => {
                waiters.retain(|waiter| waiter.as_deref() != subagent_id);
                waiters.is_empty()
            }
            PendingInput::None => false,
        };
        if empty {
            self.pending_input = PendingInput::None;
        }
    }

    fn handle_agent_event(&mut self, mut envelope: Envelope) -> Vec<Action> {
        let session_owned = self
            .background
            .as_ref()
            .is_some_and(|background| background.owns_event(&envelope));
        if (envelope.task.is_some() || envelope.run_id == BACKGROUND_EVENT_RUN_ID) && !session_owned
        {
            return Vec::new();
        }
        if let Some(origin) = &envelope.task
            && let Some(runtime) = &self.background
            && let Some(card) =
                runtime.resident_invocation_status(&origin.task_id, &origin.invocation_id)
            && card.kind == JobKind::Shell
            && matches!(
                envelope.event,
                AgentEvent::SubagentProgress { .. }
                    | AgentEvent::ToolOutput { .. }
                    | AgentEvent::ToolAnnotation { .. }
                    | AgentEvent::ToolSnapshot { .. }
                    | AgentEvent::ToolHeaderSnapshot { .. }
                    | AgentEvent::LiveToolBuf { .. }
                    | AgentEvent::Done { .. }
                    | AgentEvent::Error { .. }
            )
        {
            if let Some(index) = self.job_owner_chat(&card) {
                let call_id = card.call_id.clone();
                let active = card.active();
                self.chats[index].task_card_update(card);
                match &envelope.event {
                    AgentEvent::SubagentProgress { progress } if active => {
                        self.chats[index].set_tool_progress(&call_id, progress.clone());
                    }
                    AgentEvent::ToolOutput { id, .. }
                    | AgentEvent::ToolAnnotation { id, .. }
                    | AgentEvent::ToolSnapshot { id, .. }
                    | AgentEvent::ToolHeaderSnapshot { id, .. }
                    | AgentEvent::LiveToolBuf { id, .. }
                        if active && id == &call_id =>
                    {
                        self.chats[index].handle_event(envelope.event, None);
                    }
                    _ => {}
                }
            }
            return Vec::new();
        }
        // A deferred question's job asks under its own provenance, and
        // nothing is parked on it, so it never becomes the task's question.
        match envelope.event {
            AgentEvent::Question(event)
                if matches!(event.delivery, QuestionDelivery::Deferred { .. }) =>
            {
                self.receive_deferred_question(*event);
                return Vec::new();
            }
            other => envelope.event = other,
        }
        if let AgentEvent::QuestionClosed = envelope.event
            && let Some(origin) = &envelope.task
            && self.close_deferred_question(&origin.task_id)
        {
            return Vec::new();
        }
        if let Some(origin) = &envelope.task {
            match &envelope.event {
                AgentEvent::PermissionRequest(request)
                | AgentEvent::PermissionRequestUpdated(request) => {
                    self.task_interactions
                        .permissions
                        .insert(request.id.clone(), Arc::clone(origin));
                }
                AgentEvent::Question(_) => {
                    self.task_interactions.question = Some(Arc::clone(origin))
                }
                AgentEvent::AuthRequired => {
                    self.task_interactions
                        .auth
                        .insert(origin.task_id.clone(), Arc::clone(origin));
                }
                _ => {}
            }
        } else if matches!(envelope.event, AgentEvent::Question(_)) {
            self.task_interactions.question = None;
        }
        if matches!(envelope.event, AgentEvent::QuestionClosed) {
            self.close_withdrawn_question(&envelope);
            return Vec::new();
        }
        // Sent under their own run id, ahead of the stale-run filter: a run
        // outlives the turn that launched it and reports through every one
        // after it.
        if let AgentEvent::Workflow(event) = envelope.event {
            self.on_workflow_event(*event);
            return vec![];
        }
        // A workflow agent reports under the workflow's run id like the run
        // itself, so its digest is read here rather than behind the
        // stale-run filter. It has no task header to land on either: the
        // inspector's roster row is where a reader looks for it.
        if let AgentEvent::SubagentProgress { progress } = &envelope.event
            && let Some(workflow) = &envelope.workflow
        {
            self.workflow_inspector.set_progress(
                &workflow.run_id,
                workflow.call_key,
                progress.clone(),
            );
            return vec![];
        }
        // A workflow run outlives the turn that launched it, so its agents
        // report under the run's own id and the stale-run filter below would
        // drop them. An interaction nobody can answer is a run that hangs
        // forever, so these are hoisted like the workflow events above. They
        // deliberately do not resolve a chat: a workflow agent has no task
        // card, and the inspector's roster row is where it is read.
        if let Some(workflow) = envelope.workflow.clone() {
            match envelope.event {
                AgentEvent::PermissionRequest(request) => {
                    let requester = workflow_requester(&workflow, envelope.subagent.as_ref());
                    self.permission_prompt.enqueue(request, Some(requester));
                    return vec![];
                }
                AgentEvent::PermissionRequestUpdated(request) => {
                    self.permission_prompt.update(request);
                    return vec![];
                }
                AgentEvent::PermissionRequestResolved { request_id, .. } => {
                    self.permission_prompt.resolve_pending(&request_id);
                    return vec![];
                }
                AgentEvent::AuthRequired => {
                    self.flash(WORKFLOW_AUTH_REQUIRED.into());
                    return vec![];
                }
                other => envelope.event = other,
            }
        }
        // Hoisted like the workflow events above: a workflow agent's prompt is
        // decided under the workflow's run id.
        if let AgentEvent::Autopilot(event) = envelope.event {
            if let AutopilotEvent::Deciding {
                seq,
                kind: DecisionKind::Question,
                ..
            } = &event
                && let Some(origin) = &envelope.task
            {
                self.note_question_decision(*seq, &origin.task_id);
            }
            if session_owned || envelope.workflow.is_some() || envelope.run_id == self.run_id {
                self.on_autopilot_event(event, envelope.run_id);
            }
            return vec![];
        }
        if envelope.run_id == RESTORE_RUN_ID {
            let (id, snapshot, theme_gen, is_header) = match envelope.event {
                AgentEvent::ToolSnapshot {
                    id,
                    snapshot,
                    theme_gen,
                } => (id, snapshot, theme_gen, false),
                AgentEvent::ToolHeaderSnapshot {
                    id,
                    snapshot,
                    theme_gen,
                } => (id, snapshot, theme_gen, true),
                _ => return vec![],
            };
            for chat in &mut self.chats {
                if is_header {
                    chat.tool_header_snapshot(&id, snapshot.clone(), theme_gen);
                } else {
                    chat.tool_snapshot(&id, snapshot.clone(), theme_gen);
                }
            }
            return vec![];
        }
        // Generated off the turn's critical path, so it can land after the run
        // that asked for it retired; a title is session state, not a frame.
        // The spend is recorded even when the answer was unusable, and
        // `context_size` is left alone: the request never enters history.
        if let AgentEvent::SessionTitle {
            title,
            usage,
            cost,
            billing,
            model,
            provider,
        } = envelope.event
        {
            self.state.token_usage += usage;
            self.add_session_spend(cost, billing);
            self.record_model_usage(
                &provider,
                None,
                &model,
                LedgerPurpose::Title,
                usage,
                cost,
                billing,
            );
            self.record_external_spend(usage, cost, billing, LedgerPurpose::Title);
            if let Some(title) = title {
                self.state.session_mut().set_title_if_auto(title);
            }
            return vec![];
        }
        // The summarizer reports under a run id of its own whenever it writes
        // a line, which an open inspector reads again.
        if let AgentEvent::MemoryChanged = envelope.event {
            self.memory_changed();
            return vec![];
        }
        // Session state too, and a background shell reports its own gap long
        // after the run that started it retired.
        if let AgentEvent::Unrecorded { gap, notice } = envelope.event {
            if notice {
                self.flash(format!("{UNRECORDED_MSG}: {}", gap.reason));
            }
            self.state.session_mut().meta.push_unrecorded(gap);
            return vec![];
        }
        if let AgentEvent::TaskAdmitted(card) = &envelope.event {
            if !session_owned && envelope.run_id != self.run_id {
                return Vec::new();
            }
            if card.kind != JobKind::Agent {
                if let Some(index) = self.job_owner_chat(card) {
                    self.chats[index].task_card_update(card.clone());
                }
                let _ = self.refresh_task_picker();
                return Vec::new();
            }
            let mut info = envelope.subagent.clone().unwrap_or_else(|| SubagentInfo {
                parent_tool_use_id: card.call_id.clone(),
                task_id: card.task_id.clone(),
                name: card.label.clone(),
                prompt: self.chats[0].task_prompt(&card.call_id),
                model: None,
                thinking: None,
                fast: false,
                answer_tx: None,
                steer_tx: None,
                control: None,
            });
            info.name.clone_from(&card.label);
            if info.prompt.is_none() {
                info.prompt = self
                    .task_continuation_prompt(card)
                    .or_else(|| self.chats[0].task_prompt(&card.call_id));
            }
            self.resolve_or_create_chat(&info);
            self.chats[0].task_card_update(card.clone());
            return Vec::new();
        }
        if let AgentEvent::SubagentHistory {
            task_id,
            parent_tool_use_id,
            root_tool_use_id,
            name,
            model,
            messages,
            spec,
        } = &envelope.event
        {
            if !session_owned
                && envelope.run_id != self.run_id
                && !crate::active_session_history(&self.state.session).is_ok_and(|items| {
                    caudra_agent::history_tool_call_ids(&items).contains(root_tool_use_id)
                        || reachable_subagent_ids(
                            &items,
                            self.state.session.subagent_messages(),
                            self.state.session.tool_outputs(),
                            self.state.session.subagents(),
                        )
                        .contains(root_tool_use_id)
                })
                && !self.chats.iter().any(|chat| {
                    chat.parent_tool_use_id().is_some_and(|parent| {
                        parent.as_ref() == parent_tool_use_id || parent.as_ref() == root_tool_use_id
                    })
                })
            {
                return vec![];
            }
            self.subagent_answers.remove(task_id);
            self.preserve_unconsumed_steers(task_id);
            let items = crate::history_items(messages);
            if parent_tool_use_id == task_id {
                self.state
                    .session_mut()
                    .set_subagent_history(task_id.clone(), items, spec.clone());
            } else {
                if let Some(previous) = self.state.session.subagent_messages().get(task_id).cloned()
                {
                    self.state.session_mut().set_subagent_history(
                        task_id.clone(),
                        previous.as_ref().clone(),
                        spec.clone(),
                    );
                } else {
                    self.state.session_mut().set_subagent_history(
                        task_id.clone(),
                        items.clone(),
                        spec.clone(),
                    );
                }
                self.state.session_mut().set_subagent_history(
                    parent_tool_use_id.clone(),
                    items,
                    Some(caudra_storage::sessions::StoredSubagentTaskSpec::version()),
                );
            }
            if session_owned || envelope.run_id == self.run_id {
                let sub_idx = self
                    .chat_index
                    .get(task_id.as_str())
                    .copied()
                    .unwrap_or_else(|| {
                        self.resolve_or_create_chat(&SubagentInfo {
                            parent_tool_use_id: parent_tool_use_id.clone(),
                            task_id: task_id.clone(),
                            name: name.clone(),
                            prompt: None,
                            model: Some(model.clone()),
                            thinking: None,
                            fast: false,
                            answer_tx: None,
                            steer_tx: None,
                            control: None,
                        })
                    });
                if !session_owned {
                    self.chats[sub_idx].mark_finished(TaskOutcome::Unknown, DONE_TEXT);
                }
                self.sync_subagents();
            } else {
                let mut subagents = self.state.session.subagents().to_vec();
                if let Some(stored) = subagents
                    .iter_mut()
                    .find(|stored| stored.tool_use_id == *task_id)
                {
                    stored.parent_tool_use_id = Some(parent_tool_use_id.clone());
                    stored.root_tool_use_id = Some(root_tool_use_id.clone());
                    stored.name.clone_from(name);
                    stored.model = Some(model.clone());
                } else {
                    subagents.push(caudra_storage::sessions::StoredSubagent {
                        tool_use_id: task_id.clone(),
                        parent_tool_use_id: Some(parent_tool_use_id.clone()),
                        root_tool_use_id: Some(root_tool_use_id.clone()),
                        name: name.clone(),
                        model: Some(model.clone()),
                        thinking: None,
                        fast: false,
                        outcome: caudra_storage::sessions::StoredSubagentOutcome::Unknown,
                    });
                }
                self.state.session_mut().set_subagents(subagents);
            }
            let mut subagents = self.state.session.subagents().to_vec();
            if let Some(stored) = subagents
                .iter_mut()
                .find(|stored| stored.tool_use_id == *task_id)
            {
                stored.root_tool_use_id = Some(root_tool_use_id.clone());
                self.state.session_mut().set_subagents(subagents);
            }
            return vec![];
        }
        if !session_owned
            && envelope.run_id != self.run_id
            && !matches!(
                envelope.event,
                AgentEvent::ModelUsage { .. }
                    | AgentEvent::CacheWarmEpisode(_)
                    | AgentEvent::CacheHitSamples(_)
                    | AgentEvent::TokenCalibration(_)
                    | AgentEvent::Learned(_)
                    | AgentEvent::LearnerActivity(_)
            )
        {
            if envelope.task.is_none()
                && envelope.workflow.is_none()
                && self.cancelling_run == Some(envelope.run_id)
                && match &envelope.event {
                    AgentEvent::UserExchangeAdmitted { .. } => envelope.subagent.is_none(),
                    AgentEvent::TurnComplete(turn) => {
                        matches!(turn.purpose, LedgerPurpose::Chat) && !turn.interrupted
                    }
                    _ => false,
                }
            {
                self.state
                    .session_mut()
                    .meta
                    .activity
                    .get_or_insert_default()
                    .complete = false;
            }
            let cancelled_terminal = envelope.subagent.is_none()
                && self.cancelling_run == Some(envelope.run_id)
                && matches!(
                    &envelope.event,
                    AgentEvent::Done { .. } | AgentEvent::Error { .. }
                );
            if cancelled_terminal {
                let cancelled_error = matches!(&envelope.event, AgentEvent::Error { .. });
                self.cancelling_run = None;
                self.status =
                    if self.replacement_item.is_some() || !self.queue.live_entries().is_empty() {
                        Status::Streaming
                    } else {
                        Status::Idle
                    };
                self.refresh_record_index();
                if cancelled_error {
                    self.queue.resume();
                }
                if mem::take(&mut self.goal_replaces_run) {
                    self.rearm_background();
                }
                self.automation_run_ended(TurnOutcome::Cancelled, None);
            }
            // A snapshot dropped here degrades the tool body to llm_output.
            if let AgentEvent::ToolSnapshot { id, .. }
            | AgentEvent::ToolHeaderSnapshot { id, .. }
            | AgentEvent::LiveToolBuf { id, .. } = &envelope.event
            {
                tracing::debug!(
                    tool_id = %id,
                    event_run_id = envelope.run_id,
                    current_run_id = self.run_id,
                    "tool render event dropped: stale run_id"
                );
            }
            return vec![];
        }

        if let AgentEvent::UserExchangeAdmitted { group_id } = envelope.event {
            if envelope.subagent.is_none() && envelope.task.is_none() && envelope.workflow.is_none()
            {
                self.state
                    .session_mut()
                    .meta
                    .activity
                    .get_or_insert_default()
                    .record_user_exchange(group_id);
            }
            return vec![];
        }

        let run_ended = envelope.subagent.is_none()
            && matches!(
                &envelope.event,
                AgentEvent::Done { .. } | AgentEvent::Error { .. }
            );
        let run_id = envelope.run_id;
        let automation_run_end = (run_ended && envelope.task.is_none() && self.has_automations())
            .then(|| automation::run_end(&envelope.event))
            .flatten();
        if envelope.subagent.is_none()
            && let AgentEvent::Injected {
                automation_event,
                peer_event,
                ..
            } = &envelope.event
        {
            if let Some(origin) = automation_event {
                self.automation_injected(origin);
            }
            if let Some(origin) = peer_event {
                self.automation_peer_injected(origin);
            }
        }

        if envelope.subagent.is_none() {
            let site = match &envelope.event {
                AgentEvent::Done {
                    reason: caudra_agent::DoneReason::MaxTurns,
                    ..
                } => Some(tasks::WAKES_SUPPRESSED_RUN_MAX_TURNS),
                AgentEvent::Done {
                    reason: caudra_agent::DoneReason::MaxTokens,
                    ..
                } => Some(tasks::WAKES_SUPPRESSED_RUN_MAX_TOKENS),
                AgentEvent::Done {
                    reason: caudra_agent::DoneReason::Cancelled,
                    ..
                } => Some(tasks::WAKES_SUPPRESSED_RUN_CANCELLED),
                AgentEvent::Error { .. } => Some(tasks::WAKES_SUPPRESSED_RUN_ERROR),
                _ => None,
            };
            if let Some(site) = site
                && !self.shutting_down()
            {
                self.suppress_background_wakes(site);
            }
        }

        match &envelope.event {
            AgentEvent::ToolStart(event) => self.fire_session_autocmd(
                "ToolStart",
                serde_json::json!({
                    "tool_id": event.id,
                    "tool": event.tool,
                }),
            ),
            AgentEvent::ToolDone(event) => self.fire_session_autocmd(
                "ToolDone",
                serde_json::json!({
                    "tool_id": event.id,
                    "tool": event.tool,
                }),
            ),
            _ => {}
        }

        let subagent_id = envelope.subagent.as_ref().map(|s| s.task_id.clone());
        let parent_tool_use_id = envelope
            .subagent
            .as_ref()
            .map(|s| s.parent_tool_use_id.clone());

        let chat_idx = match envelope.subagent {
            Some(ref subagent) => self.resolve_or_create_chat(subagent),
            None => 0,
        };

        // A brief is written by the main chat and read into the task's own,
        // which is neither of the chats this event is otherwise addressed to.
        if let AgentEvent::ToolInputDelta {
            ref mut delegations,
            ..
        } = envelope.event
            && subagent_id.is_none()
        {
            for delegation in std::mem::take(delegations) {
                self.delegation_delta(delegation);
            }
        }

        let main_agent = subagent_id.is_none() && envelope.task.is_none();
        let active_plan =
            main_agent && self.execution_agent_mode() == self.agent_mode_for(Mode::Plan);
        let mut plan_write = None;
        if let AgentEvent::ToolDone(ref mut e) = envelope.event {
            let native_plan = &*e.tool == plan::NAME;
            if native_plan && self.state.session.tool_outputs().contains_key(&e.id) {
                return vec![];
            }
            let committed_plan = e.plan_write_result();
            if committed_plan.is_some()
                && let Some(annotation) = &e.annotation
                && let ToolOutput::Markdown(text) = &mut e.output
                && text.state.is_none()
            {
                text.state = Some(Value::String(annotation.clone()));
            }
            self.record_tool_call(e);
            if !native_plan {
                self.reload_written(e);
            }
            // Whatever the call opened and never claimed was a prediction the
            // call disagreed with: bad arguments, or a child `batch` refused.
            if subagent_id.is_none() {
                self.discard_pending_delegations_under(&e.id);
            }
            if native_plan {
                if main_agent
                    && let Some(plan) = committed_plan
                    && self.take_plan_write(&plan)
                {
                    plan_write = Some((e.id.clone(), plan));
                }
            } else if active_plan
                && self.state.plan.path().is_some_and(|pp| {
                    e.wrote_to(pp) || e.wrote_to(&Path::new(&self.state.session.cwd).join(pp))
                })
            {
                self.transition_plan(PlanTrigger::WriteDone);
            }
            self.state
                .session_mut()
                .insert_tool_output(e.id.clone(), e.output.clone());
            if subagent_id.is_none()
                && let Some(task_id) = self.parent_task_ids.get(&e.id)
                && !self.session_owns_task(task_id)
                && let Some(&sub_idx) = self.chat_index.get(task_id)
            {
                let (outcome, text) = if e.is_error {
                    (TaskOutcome::Error, ERROR_TEXT)
                } else {
                    (TaskOutcome::Done, DONE_TEXT)
                };
                self.chats[sub_idx].mark_finished(outcome, text);
                self.sync_subagents();
            }
        }

        if matches!(envelope.event, AgentEvent::StreamReset) {
            self.chats[chat_idx].stream_reset();
            self.discard_stream_delegations(chat_idx);
            self.sync_todo_panel();
            return vec![];
        }

        if let AgentEvent::Retry {
            attempt,
            message,
            delay_ms,
        } = envelope.event
        {
            self.wait_out_retry(chat_idx, attempt, message, delay_ms);
            return vec![];
        }

        // The wait is over for the stream that produced this, and only for it:
        // a subagent's traffic says nothing about a backoff the main chat or a
        // sibling task is still sitting in.
        self.chats[chat_idx].clear_retry();

        if let AgentEvent::TurnComplete(ref tc) = envelope.event {
            if matches!(tc.purpose, LedgerPurpose::Chat) && !tc.interrupted {
                self.state
                    .session_mut()
                    .meta
                    .activity
                    .get_or_insert_default()
                    .record_model_turn(!main_agent);
            }
            self.state.token_usage += tc.usage;
            self.add_session_spend(tc.cost, tc.billing);
            add_chat_spend(&mut self.chats[chat_idx], tc.cost, tc.billing);
            if main_agent && matches!(tc.purpose, LedgerPurpose::Chat) {
                self.automation_turn_complete(&tc.message);
            }
            if subagent_id.is_some() {
                self.record_external_spend(tc.usage, tc.cost, tc.billing, tc.purpose);
            }
            self.record_model_usage(
                &tc.provider,
                tc.account.as_deref(),
                &tc.model,
                tc.purpose,
                tc.usage,
                tc.cost,
                tc.billing,
            );
            let ctx_size = tc.context_size.unwrap_or_else(|| tc.usage.context_tokens());
            self.chats[chat_idx].context_size = ctx_size;
            if matches!(tc.purpose, LedgerPurpose::Chat) {
                self.chats[chat_idx].context_window = tc.context_window;
            }
            if chat_idx == 0 {
                self.state.context_size = ctx_size;
            }
            self.chats[chat_idx].set_pending_turn_usage(tc.usage.format(tc.cost));
            if let Some(tool_id) = &parent_tool_use_id {
                let formatted = tc.usage.format_sum_cost(self.chats[chat_idx].cost);
                self.chats[0].set_tool_turn_usage(tool_id, formatted);
            }
        }

        // Belongs on the parent's task header, not in the subagent transcript
        // that already shows the work itself. A batch child has no header, so
        // the same report is addressed to its row in the roster instead.
        if let AgentEvent::SubagentProgress { progress } = envelope.event {
            if let Some(origin) = &envelope.task {
                if self.task_response_current(origin)
                    && let Some(runtime) = &self.background
                    && let Some(card) =
                        runtime.resident_invocation_status(&origin.task_id, &origin.invocation_id)
                    && card.active()
                {
                    let call_id = card.call_id.clone();
                    self.chats[0].task_card_update(card);
                    self.chats[0].set_tool_progress(&call_id, progress);
                }
                return Vec::new();
            }
            if let Some(tool_id) = &parent_tool_use_id {
                self.chats[0].set_tool_progress(tool_id, progress);
            }
            return vec![];
        }

        let event = match envelope.event {
            AgentEvent::ModelUsage {
                usage,
                cost,
                billing,
                provider,
                model,
                purpose,
                account,
            } => {
                self.state.token_usage += usage;
                self.add_session_spend(cost, billing);
                add_chat_spend(&mut self.chats[chat_idx], cost, billing);
                if subagent_id.is_some() {
                    self.record_external_spend(usage, cost, billing, purpose);
                }
                self.record_model_usage(
                    &provider,
                    account.as_deref(),
                    &model,
                    purpose,
                    usage,
                    cost,
                    billing,
                );
                return vec![];
            }
            AgentEvent::CacheWarmEpisode(episode) => {
                self.storage_writer
                    .record_cache_warm(WarmingWrite::Episode(episode));
                return vec![];
            }
            AgentEvent::CacheHitSamples(samples) => {
                self.storage_writer
                    .record_cache_warm(WarmingWrite::Samples(samples));
                return vec![];
            }
            AgentEvent::TokenCalibration(calibration) => {
                self.storage_writer.record_calibration(calibration);
                return vec![];
            }
            AgentEvent::Learned(notice) => {
                self.flash(format!("{} (/learn)", notice.headline()));
                return vec![];
            }
            AgentEvent::LearnerActivity(activity) => {
                self.learner_activity(*activity);
                return vec![];
            }
            AgentEvent::GoalEvaluating { evaluation } => {
                self.flash(format!("Evaluating goal (#{evaluation})..."));
                return vec![];
            }
            AgentEvent::GoalSkipped { .. } => {
                self.flush_goal_log();
                return vec![];
            }
            AgentEvent::GoalEvaluation {
                seq,
                verdict,
                reason,
                evaluation,
                applied,
                usage,
                cost,
                billing,
                model,
                ..
            } => {
                self.state.token_usage += usage;
                self.add_session_spend(cost, billing);
                add_chat_spend(&mut self.chats[chat_idx], cost, billing);
                self.record_goal_usage(&model, usage, cost, billing);
                match verdict {
                    _ if !applied => {}
                    GoalVerdict::NotMet => self.main_chat().push(DisplayMessage::goal_evaluation(
                        seq,
                        GoalTone::Notice,
                        not_met_row(evaluation, &reason),
                    )),
                    GoalVerdict::Met | GoalVerdict::Impossible => {
                        self.goal_verdict_seq = Some(seq);
                    }
                }
                self.flush_goal_log();
                return vec![];
            }
            AgentEvent::GoalFinished {
                result,
                autopilot_owned,
            } => {
                self.automation_goal_finished(&result, autopilot_owned);
                let (tone, text) = finished_row(result.verdict, &result.reason);
                let seq = self.goal_verdict_seq.take().unwrap_or_default();
                self.main_chat()
                    .push(DisplayMessage::goal_evaluation(seq, tone, text));
                self.flush_goal_log();
                return vec![];
            }
            AgentEvent::GoalDeferred {
                active_background_tasks: _,
            } => {
                self.goal_deferred = true;
                self.main_chat().push(DisplayMessage::new(
                    DisplayRole::Notice,
                    "Goal evaluation deferred until outstanding work, result delivery, and queued input settle.".into(),
                ));
                return vec![];
            }
            AgentEvent::GoalLoopCap {
                evaluations,
                continuations,
                limit,
                autopilot_owned,
            } => {
                self.automation_goal_stalled(autopilot_owned);
                self.main_chat().push(DisplayMessage::new(
                    DisplayRole::Notice,
                    format!(
                        "Goal remains active after {continuations} automatic continuations in this run ({evaluations} total evaluations); the session limit is {limit}. Send another message to resume."
                    ),
                ));
                return vec![];
            }
            AgentEvent::GoalStalled {
                evaluations,
                turns,
                autopilot_owned,
            } => {
                self.automation_goal_stalled(autopilot_owned);
                self.main_chat().push(DisplayMessage::new(
                    DisplayRole::Notice,
                    format!(
                        "Goal remains active, but its last {turns} turns made no progress ({evaluations} total evaluations), so the run stopped. Send another message to resume."
                    ),
                ));
                return vec![];
            }
            AgentEvent::GoalWaiting {
                evaluations,
                attempt,
                limit,
                delay_ms,
            } => {
                let delay = Duration::from_millis(delay_ms);
                self.main_chat().push(DisplayMessage::new(
                    DisplayRole::Notice,
                    format!(
                        "Goal waits on a change outside the session ({evaluations} total evaluations); checking again in {} (wait {attempt}/{limit}). Send a message to go on sooner.",
                        format_elapsed(delay.as_secs())
                    ),
                ));
                return vec![];
            }
            AgentEvent::GoalTurnLimit { evaluations } => {
                self.main_chat().push(DisplayMessage::new(
                    DisplayRole::Error,
                    format!(
                        "Goal remains active after {evaluations} evaluations; the agent turn limit was reached. Send another message to resume."
                    ),
                ));
                return vec![];
            }
            AgentEvent::GoalEvaluationFailed {
                seq,
                evaluation,
                message,
                applied,
                usage,
                cost,
                billing,
                model,
                ..
            } => {
                self.state.token_usage += usage;
                self.add_session_spend(cost, billing);
                add_chat_spend(&mut self.chats[chat_idx], cost, billing);
                self.record_goal_usage(&model, usage, cost, billing);
                if applied {
                    self.main_chat().push(DisplayMessage::goal_evaluation(
                        seq,
                        GoalTone::Error,
                        evaluation_failed_row(evaluation, &message),
                    ));
                }
                self.flush_goal_log();
                return vec![];
            }
            AgentEvent::GoalClearedAfterError {
                condition,
                message,
                autopilot_owned,
            } => {
                self.automation_goal_cleared(&condition, &message, autopilot_owned);
                self.main_chat().push(DisplayMessage::new(
                    DisplayRole::Error,
                    cleared_after_error_row(&condition, &message),
                ));
                self.flush_goal_log();
                return vec![];
            }
            AgentEvent::AccountRouted { account, switch } => {
                self.account_routed(chat_idx, main_agent, &account, switch);
                return vec![];
            }
            AgentEvent::AccountExhausted(exhaustion) => {
                self.account_exhausted(chat_idx, main_agent, *exhaustion);
                return vec![];
            }
            AgentEvent::ModelRerouted(reroute) => {
                return self.model_rerouted(chat_idx, main_agent, *reroute);
            }
            AgentEvent::SettingsChanged { model, thinking } => {
                return self.settings_changed(chat_idx, main_agent, model, thinking);
            }
            AgentEvent::TaskGuidanceQueued {
                task_id,
                id,
                text,
                interrupt,
                superseded,
            } => {
                self.task_guidance_queued(&task_id, id, text, interrupt, superseded);
                return vec![];
            }
            event => event,
        };

        if let AgentEvent::Decision(note) = &event {
            self.record_engine_decision(note, subagent_id.clone());
        }
        let plan_path = if active_plan {
            self.state.plan.path()
        } else {
            None
        };
        let todo_changed = Chat::touches_todos(&event);
        if matches!(event, AgentEvent::ToolDone(_))
            && let Some(scale) = self
                .route_model(self.state.applied_mode)
                .map(|model| calibration::scale(&model))
        {
            self.chats[chat_idx].set_token_scale(scale);
        }
        let result = self.chats[chat_idx].handle_event(event, plan_path);
        if todo_changed && chat_idx == self.active_chat {
            self.sync_todo_panel();
        }
        if let Some((id, plan)) = plan_write {
            self.chats[chat_idx].plan_written(&id, &plan);
        }

        let result = match result {
            ChatEventResult::PlanWritten(plan) => {
                if main_agent {
                    self.take_plan_write(&plan);
                }
                return vec![];
            }
            ChatEventResult::QueueBatchConsumed { items } => {
                if chat_idx == 0 {
                    self.on_queue_batch_consumed(&items);
                } else if let Some(task_id) = subagent_id {
                    self.chats[chat_idx].show_consumed_prompts(&items);
                    if let Some(pending) = self.pending_subagent_steers.get_mut(&task_id) {
                        pending.retain(|queued| items.iter().all(|item| item.id != queued.id));
                        if pending.is_empty() {
                            self.pending_subagent_steers.remove(&task_id);
                        }
                    }
                    self.clamp_active_queue_focus();
                }
                return vec![];
            }
            result => result,
        };

        if let ChatEventResult::QueueItemConsumed { id, text, images } = result {
            let display = format_with_images(&text, images.len());
            let images = Arc::from(images);
            if chat_idx == 0 {
                self.on_queue_item_consumed(id, display, images);
            } else if let Some(task_id) = subagent_id {
                if self.subagent_replacements.get(&task_id) == Some(&id) {
                    self.subagent_replacements.remove(&task_id);
                    let chat = &mut self.chats[chat_idx];
                    chat.flush();
                    chat.cancel_in_progress();
                    chat.push(DisplayMessage::new(DisplayRole::Error, CANCEL_MSG.into()));
                    self.sync_todo_panel();
                }
                self.chats[chat_idx].show_user_prompt(display, images);
                if let Some(pending) = self.pending_subagent_steers.get_mut(&task_id) {
                    pending.retain(|queued| queued.id != id);
                    if pending.is_empty() {
                        self.pending_subagent_steers.remove(&task_id);
                    }
                }
                self.clamp_active_queue_focus();
            }
            return vec![];
        }

        if let ChatEventResult::PermissionRequest(request) = result {
            if request.presentation.autopilot != Some(AutopilotNote::Deciding) {
                self.make_way_for_permission_prompt();
            }
            let call_id = request.id.clone();
            if self
                .permission_prompt
                .enqueue(request, subagent_id.map(|id| format!("subtask {id}")))
            {
                self.chats[chat_idx].await_approval(&call_id);
            }
            return vec![];
        }

        if let ChatEventResult::PermissionRequestUpdated(request) = result {
            let escalated = matches!(
                request.presentation.autopilot,
                Some(AutopilotNote::Escalated { .. })
            );
            if self.permission_prompt.update(request) && escalated {
                self.make_way_for_permission_prompt();
            }
            return vec![];
        }

        if let ChatEventResult::PermissionRequestResolved { request_id } = result {
            self.task_interactions.permissions.remove(&request_id);
            self.permission_prompt.resolve_pending(&request_id);
            self.chats[chat_idx].approval_settled(&request_id);
            return vec![];
        }

        // A subagent's question routes back through that subagent's own
        // answer channel, so the form remembers which chat asked.
        if let ChatEventResult::Question(event) = result {
            self.receive_question(*event, subagent_id);
            return vec![];
        }

        if let ChatEventResult::AuthRequired = result {
            let inserted = match &mut self.pending_input {
                PendingInput::None => {
                    self.pending_input = PendingInput::AuthRetry {
                        waiters: HashSet::from([subagent_id.clone()]),
                    };
                    true
                }
                PendingInput::AuthRetry { waiters } => waiters.insert(subagent_id.clone()),
            };
            if !inserted {
                return vec![];
            }
            self.chats[chat_idx].push(DisplayMessage::new(
                DisplayRole::Error,
                AUTH_EXPIRED_MSG.into(),
            ));
            if chat_idx != 0 {
                self.main_chat().push(DisplayMessage::new(
                    DisplayRole::Error,
                    AUTH_EXPIRED_MSG.into(),
                ));
            }
            return vec![];
        }

        if let ChatEventResult::AuthRestored = result {
            self.clear_auth_waiter(subagent_id.as_deref());
            return vec![];
        }

        if chat_idx == 0 {
            match result {
                ChatEventResult::Done => {
                    self.status_bar.clear_flash();
                    self.settle_open_run();
                    self.bind_main_chat_sources();
                    if !self.goal_deferred {
                        self.terminalize_turn(MISSING_TOOL_COMPLETION);
                        self.retain_session_task_routes();
                    }
                    self.status = Status::Idle;
                    self.escalate_questions();
                    self.fire_session_autocmd("TurnEnd", serde_json::json!({}));
                    if self.exit_on_done {
                        self.exit_request = ExitRequest::Success;
                    }
                }
                ChatEventResult::Error(message) => {
                    self.settle_open_run();
                    self.bind_main_chat_sources();
                    self.cancel_queue_edit();
                    // The transcript keeps what the turn managed to write, so say so here: the
                    // remedy is one command and nothing else in the UI names it.
                    let message = if self.died_mid_turn() {
                        format!("{message} ({})", queue::CONTINUE_HINT)
                    } else {
                        message
                    };
                    self.status = Status::error(message.clone());
                    self.status_bar.clear_flash();
                    self.escalate_questions();
                    self.automatic_wakes_suppressed = true;
                    self.terminalize_turn(&message);
                    self.retain_session_task_routes();
                    self.recoverable_queue = self.queue.pending_prompts();
                    self.recoverable_queue_together =
                        self.queue.delivery() == caudra_agent::QueueDelivery::TogetherNextTurn;
                    self.queue.clear();
                    self.fire_session_autocmd(
                        "TurnError",
                        serde_json::json!({ "message": message }),
                    );
                    if self.exit_on_done {
                        self.exit_request = ExitRequest::Error;
                    }
                }
                ChatEventResult::AuthRequired
                | ChatEventResult::PlanWritten(_)
                | ChatEventResult::AuthRestored
                | ChatEventResult::Question(_)
                | ChatEventResult::PermissionRequest(_)
                | ChatEventResult::PermissionRequestUpdated(_)
                | ChatEventResult::PermissionRequestResolved { .. }
                | ChatEventResult::QueueItemConsumed { .. }
                | ChatEventResult::QueueBatchConsumed { .. } => unreachable!(),
                ChatEventResult::Continue => {}
            }
        }
        if run_ended {
            self.refresh_record_index();
            self.autopilot_run_ended();
        }
        if let Some((outcome, error)) = automation_run_end {
            self.automation_run_ended(outcome, error);
        }
        if run_ended
            && let Some((chosen_by, seq)) = self.autopilot_implement.take()
            && chosen_by == run_id
        {
            return self.implement_for_autopilot(seq);
        }
        vec![]
    }

    /// A stream waits out a backoff: what it streamed is dropped, and the
    /// countdown takes its place until the next event from that stream.
    pub(super) fn wait_out_retry(
        &mut self,
        chat_idx: usize,
        attempt: u32,
        message: String,
        delay_ms: u64,
    ) {
        self.chats[chat_idx].stream_reset();
        self.discard_stream_delegations(chat_idx);
        self.sync_todo_panel();
        let Some(deadline) = Instant::now().checked_add(Duration::from_millis(delay_ms)) else {
            self.chats[chat_idx].clear_retry();
            tracing::warn!(
                delay_ms,
                attempt,
                chat_idx,
                "retry deadline is not representable"
            );
            self.flash(RETRY_COUNTDOWN_UNAVAILABLE.into());
            return;
        };
        self.chats[chat_idx].set_retry(RetryInfo {
            attempt,
            message,
            deadline,
        });
    }

    fn resolve_or_create_chat(&mut self, subagent: &SubagentInfo) -> usize {
        let task_id = &subagent.task_id;
        let parent_tool_use_id = &subagent.parent_tool_use_id;
        let first_parent_event = self
            .parent_task_ids
            .insert(parent_tool_use_id.clone(), task_id.clone())
            .is_none();
        if let Some(ref tx) = subagent.answer_tx {
            self.subagent_answers.insert(task_id.clone(), tx.clone());
        }
        if let Some(ref tx) = subagent.steer_tx {
            self.subagent_steers.insert(task_id.clone(), tx.clone());
        }
        if let Some(control) = &subagent.control {
            self.subagent_controls
                .insert(task_id.clone(), control.clone());
        }
        if first_parent_event {
            self.chats[0].update_tool_summary(parent_tool_use_id, &subagent.name);
            if let Some(model) = &subagent.model {
                self.chats[0].set_tool_model(parent_tool_use_id, model);
            }
        }

        if let Some(idx) = self.adopt_pending_delegation(parent_tool_use_id, task_id) {
            let chat = &mut self.chats[idx];
            chat.name.clone_from(&subagent.name);
            chat.model_id.clone_from(&subagent.model);
            chat.thinking.clone_from(&subagent.thinking);
            chat.fast = subagent.fast;
            self.chat_index.insert(task_id.clone(), idx);
            self.sync_subagents();
            if idx == self.active_chat {
                self.sync_todo_panel();
            }
            return idx;
        }

        if let Some(&idx) = self.chat_index.get(task_id.as_str()) {
            self.chats[idx].set_parent_tool_use_id(parent_tool_use_id.clone());
            if self.chats[idx].is_finished() {
                let chat = &mut self.chats[idx];
                chat.resume();
                chat.name.clone_from(&subagent.name);
                chat.model_id.clone_from(&subagent.model);
                chat.thinking.clone_from(&subagent.thinking);
                chat.fast = subagent.fast;
                if let Some(ref prompt) = subagent.prompt {
                    chat.push_user_message(prompt);
                }
                self.sync_subagents();
                if idx == self.active_chat {
                    self.sync_todo_panel();
                }
            } else if self.chats[idx].message_count() == 0
                && let Some(prompt) = &subagent.prompt
            {
                self.chats[idx].push_user_message(prompt);
            }
            // An admitted job opens its chat before the subagent resolves its
            // model, so the first report of it is adopted here.
            let chat = &mut self.chats[idx];
            if let Some(model) = &subagent.model {
                chat.thinking.clone_from(&subagent.thinking);
                chat.fast = subagent.fast;
                if chat.model_id.as_ref() != Some(model) {
                    chat.model_id = Some(model.clone());
                    self.chats[0].set_tool_model(parent_tool_use_id, model);
                }
            }
            return idx;
        }
        let idx = if let Some(idx) = self
            .chats
            .iter()
            .position(|chat| chat.task_id().is_some_and(|id| id.as_ref() == task_id))
        {
            let chat = &mut self.chats[idx];
            chat.set_parent_tool_use_id(parent_tool_use_id.clone());
            chat.resume();
            chat.name.clone_from(&subagent.name);
            chat.model_id.clone_from(&subagent.model);
            chat.thinking.clone_from(&subagent.thinking);
            chat.fast = subagent.fast;
            if let Some(ref prompt) = subagent.prompt {
                chat.push_user_message(prompt);
            }
            idx
        } else {
            let mut chat = Chat::subagent(
                task_id,
                subagent.name.clone(),
                Path::new(&self.state.session.cwd),
                self.ui_config.clone(),
                self.lua_event_handle.clone(),
            );
            chat.set_parent_tool_use_id(parent_tool_use_id.clone());
            chat.set_restore_channel(self.restore_event_tx.clone());
            chat.model_id = subagent.model.clone();
            chat.thinking = subagent.thinking.clone();
            chat.fast = subagent.fast;
            if let Some(ref prompt) = subagent.prompt {
                chat.push_user_message(prompt);
            }
            self.chats.push(chat);
            self.chats.len() - 1
        };
        self.chat_index.insert(task_id.clone(), idx);
        self.sync_subagents();
        if idx == self.active_chat {
            self.sync_todo_panel();
        }
        idx
    }

    /// Entry point for `caudra.api.run_command`: splits a command line into the
    /// name and args the input bar would hand over, leading slash optional.
    /// `Err` means nothing ran at all, so the Lua caller can say why.
    pub(crate) fn run_cmdline(&mut self, cmdline: &str, depth: u8) -> Result<Vec<Action>, String> {
        if depth > MAX_COMMAND_DEPTH {
            return Err(COMMAND_DEPTH_MSG.to_string());
        }
        let trimmed = cmdline.trim();
        let (name, args) = trimmed
            .split_once(char::is_whitespace)
            .unwrap_or((trimmed, ""));
        let requested = format!("/{}", name.trim_start_matches('/'));
        if let Some(feature) = disabled_feature(&requested, self.features) {
            return Err(FeatureDisabled(feature).to_string());
        }
        let resolved = self
            .command_palette
            .resolve(&requested)
            .ok_or_else(|| format!("unknown command '{name}'"))?;
        if self.command_is_out_of_scope(&resolved, args) {
            return Err(MAIN_ONLY_CMD_MSG.to_string());
        }
        Ok(self.execute_command(
            ParsedCommand {
                name: resolved,
                args: args.trim().to_string(),
            },
            depth,
        ))
    }

    /// A command whose effect lands on the main session's turn or history has
    /// no meaning while a task owns the screen, so it says so instead of
    /// acting somewhere the user is not looking.
    fn command_is_out_of_scope(&self, name: &str, args: &str) -> bool {
        !self.is_main_chat() && self.command_palette.is_main_only(name, args)
    }

    /// A built-in whose experiment is off is hidden, but typing its exact
    /// name still earns the key that turns it on rather than a prompt.
    fn refuse_disabled_command(&mut self, name: &str) -> bool {
        disabled_feature(name, self.features).is_some_and(|feature| self.refuse(feature))
    }

    /// Every way into an experiment's surface, from a shortcut to a footer
    /// chip, asks here first.
    pub(crate) fn refuse_disabled(&mut self, feature: Feature) -> bool {
        !self.features.enabled(feature) && self.refuse(feature)
    }

    fn refuse(&mut self, feature: Feature) -> bool {
        self.flash(FeatureDisabled(feature).to_string());
        true
    }

    /// {depth} is the `caudra.api.run_command` hop count, forwarded to a Lua
    /// handler so an alias cycle keeps counting. 0 when the user typed it.
    fn execute_command(&mut self, cmd: ParsedCommand, depth: u8) -> Vec<Action> {
        if self.command_is_out_of_scope(&cmd.name, &cmd.args) {
            self.flash(MAIN_ONLY_CMD_MSG.into());
            return vec![];
        }
        if self.refuse_disabled_command(&cmd.name) {
            return vec![];
        }
        match cmd.name.as_str() {
            "/compact" => {
                if self.status == Status::Streaming {
                    self.queue_compact();
                    return vec![];
                }
                self.status = Status::Streaming;
                vec![Action::Compact]
            }
            "/help" => {
                self.help_modal.toggle();
                vec![]
            }
            "/usage" => self.toggle_usage_modal(),
            "/context" => {
                self.execute_context(&cmd.args);
                vec![]
            }
            "/storage" => self.execute_storage(&cmd.args),
            "/logs" => {
                self.logs_modal.open();
                vec![]
            }
            "/docs" => {
                self.docs_modal.open(&cmd.args);
                vec![]
            }
            SEARCH_COMMAND => {
                let (tab, words) = search_tab(&cmd.args);
                self.open_search(tab, words);
                vec![]
            }
            "/changelog" => {
                self.execute_changelog(&cmd.args);
                vec![]
            }
            "/tools" => {
                self.execute_tools();
                vec![]
            }
            "/decisions" => match subcommand(&cmd.args) {
                (word, _) if word.is_empty() => {
                    self.execute_decisions();
                    vec![]
                }
                (word, "") if word == MODEL_ARG => self.open_decision_model_picker(),
                (word, "") if word == NEXT_ARG => {
                    self.cycle_decision_endpoint(ProvidersConfig::load)
                }
                _ => self.flash_usage(DECISIONS_USAGE),
            },
            "/skills" => {
                self.execute_skills();
                vec![]
            }
            "/btw" => {
                let question = cmd.args.trim().to_string();
                if question.is_empty() {
                    self.flash("Usage: /btw <question>".into());
                    vec![]
                } else {
                    vec![Action::Btw(question)]
                }
            }
            "/catchup" => vec![Action::Catchup(cmd.args.trim().into())],
            "/extract" => vec![Action::Extract],
            "/goal" => self.execute_goal(&cmd.args),
            "/autopilot" => self.execute_autopilot(&cmd.args),
            "/continue" => self.continue_run(),
            "/new" => vec![Action::RequestNewSession],
            "/queue" => {
                self.focus_active_queue();
                vec![]
            }
            "/stash" => match subcommand(&cmd.args) {
                (word, _) if word.is_empty() => self.run_builtin(BuiltinAction::StashPush),
                (word, "") if word == POP_ARG => self.run_builtin(BuiltinAction::StashPop),
                (word, "") if word == LIST_ARG => self.run_builtin(BuiltinAction::StashList),
                _ => self.flash_usage(STASH_USAGE),
            },
            "/update" => match subcommand(&cmd.args) {
                (word, _) if word.is_empty() => self.request_update(),
                (word, "") if word == DISMISS_ARG => {
                    self.dismiss_update();
                    vec![]
                }
                _ => self.flash_usage(UPDATE_USAGE),
            },
            "/memory" => self.open_memory_inspector(),
            "/learn" => self.open_learn_inspector(),
            "/tasks" => self.execute_task_control(&cmd.args),
            "/shells" => self.shells_browse(),
            "/peers" => vec![Action::ListPeers],
            "/messages" => vec![Action::PeerMessages(cmd.args)],
            "/topics" => vec![Action::PeerTopics(cmd.args)],
            "/groups" => vec![Action::PeerGroups(cmd.args)],
            "/workflows" => self.workflows_browse(),
            "/workflow" => self.execute_workflow(&cmd.args),
            "/deep-research" => {
                self.execute_builtin_workflow(workflow::DEEP_RESEARCH_WORKFLOW, &cmd.args)
            }
            "/review-changes" => {
                self.execute_builtin_workflow(workflow::REVIEW_CHANGES_WORKFLOW, &cmd.args)
            }
            "/root-cause" => {
                self.execute_builtin_workflow(workflow::ROOT_CAUSE_WORKFLOW, &cmd.args)
            }
            "/automations" => self.execute_automations(&cmd.args),
            "/sessions" => match subcommand(&cmd.args) {
                (word, _) if word.is_empty() => self.sessions_browse(),
                (word, destination) if word == MOVE_ARG || word == MIGRATE_ARG => {
                    vec![Action::OpenSessionRelocation {
                        bulk: word == MIGRATE_ARG,
                        destination: (!destination.is_empty()).then(|| destination.to_owned()),
                    }]
                }
                _ => self.flash_usage(SESSIONS_USAGE),
            },
            "/worktree" => match WorktreeView::parse(&cmd.args) {
                Ok(view) => vec![Action::OpenWorktrees(view)],
                Err(usage) => {
                    self.flash(usage.into());
                    vec![]
                }
            },
            "/rename" => self.rename_session(&cmd.args),
            "/fork" => self.fork_latest(),
            "/model" => match subcommand(&cmd.args) {
                (word, _) if word.is_empty() => self.open_model_picker_here(),
                (word, "") if word == NEXT_ARG => self.cycle_favorite_model(),
                _ => self.flash_usage(MODEL_USAGE),
            },
            "/accounts" => {
                self.open_accounts();
                vec![]
            }
            "/system-prompt" => {
                self.execute_system_prompt();
                vec![]
            }
            "/projection" => {
                self.execute_projection();
                vec![]
            }
            "/review" => self.run_builtin(BuiltinAction::Review),
            "/view" => self.run_builtin(BuiltinAction::ViewToggle),
            "/theme" => {
                self.theme_picker.open();
                vec![]
            }
            "/mcp" => {
                self.mcp_picker.open();
                vec![]
            }
            "/env" => self.open_env(),
            "/settings" => self.open_settings(),
            "/permissions" => {
                let discover = match subcommand(&cmd.args) {
                    (word, _) if word.is_empty() => false,
                    (word, "") if word == DISCOVER_ARG || word == SUGGESTED_ARG => true,
                    _ => return self.flash_usage(PERMISSIONS_USAGE),
                };
                match self.open_permissions_picker() {
                    Ok(()) if discover => {
                        self.permission_ui.show_discovery = true;
                        self.permissions_picker.show_discovery();
                    }
                    Ok(()) => {}
                    Err(error) => self.flash(error.to_string()),
                }
                vec![]
            }
            "/login" => {
                self.login_picker.open(self.storage.clone());
                vec![]
            }
            "/cd" => self.cmd_cd(&cmd.args),
            "/remote" => match self
                .features
                .require_source(self.state.session.workspace_binding())
            {
                Ok(()) => vec![Action::RemoteControl(cmd.args)],
                Err(disabled) => {
                    self.flash(disabled.to_string());
                    vec![]
                }
            },
            "/sandbox" => {
                self.open_sandbox(&cmd.args);
                vec![]
            }
            "/auto" => {
                let msg = if self.permissions.toggle_auto() {
                    AUTO_ON_MSG
                } else {
                    AUTO_OFF_MSG
                };
                self.flash(msg.into());
                vec![]
            }
            "/yolo" => {
                let msg = if self.permissions.toggle_yolo() {
                    YOLO_ON_MSG
                } else {
                    YOLO_OFF_MSG
                };
                self.flash(msg.into());
                vec![]
            }
            "/thinking" => {
                if let Some(task_id) = self.active_subagent_id().map(str::to_owned) {
                    self.set_task_thinking(task_id, &cmd.args);
                    return vec![];
                }
                match self.set_thinking(&cmd.args) {
                    Ok(thinking) => self.flash(format!("Thinking: {thinking}")),
                    Err(msg) => self.flash(msg),
                }
                vec![]
            }
            "/fast" => {
                let fast = !self.state.fast;
                match self.set_fast(fast) {
                    Ok(()) => self.flash(if fast { FAST_ON_MSG } else { FAST_OFF_MSG }.into()),
                    Err(msg) => self.flash(msg),
                }
                vec![]
            }
            "/exit" => self.quit(),
            "/reload" => self.quit_with(ExitRequest::Reload),
            "/workbench" => self.run_builtin(BuiltinAction::Workbench),
            name if name.starts_with("/project:") || name.starts_with("/user:") => {
                self.execute_custom_command(name, &cmd.args)
            }
            name if self.command_palette.find_mcp_prompt(name).is_some() => {
                self.execute_mcp_prompt(name, &cmd.args)
            }
            name if self.command_palette.find_custom_command(name).is_some() => {
                self.execute_custom_command(name, &cmd.args)
            }
            name if self.command_palette.find_lua_command(name).is_some() => {
                self.run_lua_command(name, cmd.args, depth);
                vec![]
            }
            _ => vec![],
        }
    }

    fn run_lua_command(&self, name: &str, args: String, depth: u8) {
        let Some(lua_cmd) = self.command_palette.find_lua_command(name) else {
            return;
        };
        self.lua_event_handle.run_command(
            Arc::clone(&lua_cmd.plugin),
            Arc::clone(&lua_cmd.name),
            args,
            depth,
        );
    }

    fn execute_goal(&mut self, args: &str) -> Vec<Action> {
        let condition = args.trim();
        if condition.is_empty() {
            self.open_goal_panel(PanelTab::Status);
            return vec![];
        }
        if condition.eq_ignore_ascii_case(MODEL_ARG) {
            return self.open_goal_model_picker();
        }
        if condition.eq_ignore_ascii_case(LOG_ARG) {
            self.open_goal_log();
            return vec![];
        }
        if is_goal_clear(condition) {
            return self.clear_goal();
        }

        self.submit_goal(condition)
    }

    pub(super) fn open_goal_model_picker(&mut self) -> Vec<Action> {
        self.model_picker.open_purpose(
            &self.state.model,
            &self.model_policy,
            self.active_decision_endpoint(),
            caudra_providers::ModelPurpose::Goal,
        );
        vec![Action::RefreshModels]
    }

    fn clear_goal(&mut self) -> Vec<Action> {
        self.state.autopilot.drop_goal_ownership();
        let message = self.state.goal.clear().map_or_else(
            || "No goal set".to_string(),
            |goal| format!("Goal cleared: {}", goal.condition),
        );
        self.flush_goal_log();
        self.flash(message);
        vec![]
    }

    fn execute_mcp_prompt(&mut self, name: &str, args: &str) -> Vec<Action> {
        let prompt = self.command_palette.find_mcp_prompt(name).unwrap().clone();

        let arguments = prompt.parse_arguments(args);
        let missing: Vec<_> = prompt
            .missing_arguments(&arguments)
            .into_iter()
            .map(|name| format!("<{name}>"))
            .collect();
        if !missing.is_empty() {
            self.flash(format!("Usage: {} {}", name, missing.join(" ")));
            return vec![];
        }

        let prompt_ref = caudra_agent::McpPromptRef {
            qualified_name: prompt.qualified_name.clone(),
            arguments,
        };
        let display_text = if args.trim().is_empty() {
            name.to_string()
        } else {
            format!("{name} {args}")
        };
        let mut input = self.build_agent_input(&QueuedMessage {
            text: display_text.clone(),
            images: Vec::new(),
            mentions: Vec::new(),
            commits: Vec::new(),
            paste_ranges: Vec::new(),
        });
        input.prompt = Some(Box::new(prompt_ref));

        self.submit_explicit_input(input, display_text)
    }

    /// A template expands into a prompt, so it follows the composer it was
    /// typed in: a steerable task takes it as a steer, and everything else
    /// falls back to the main session.
    fn execute_custom_command(&mut self, name: &str, args: &str) -> Vec<Action> {
        let Some(cmd) = self.command_palette.find_custom_command(name) else {
            self.flash(format!("Unknown command: {name}"));
            return vec![];
        };
        let text = cmd.render(args);
        let mentions = self.scan_mentions(&text);
        let commits = self.scan_commits(&text);
        if let Some(task_id) = self.active_subagent_id().map(str::to_owned)
            && self.active_subagent_has_composer()
        {
            let draft = InputDraft {
                text: text.clone(),
                paste_ranges: Vec::new(),
            };
            let sub = Submission {
                text,
                images: Vec::new(),
                mentions,
                commits,
                draft,
            };
            return self.send_to_task(&task_id, sub, PromptAdmission::Steer);
        }
        self.submit_or_queue(QueuedMessage {
            text,
            images: Vec::new(),
            mentions,
            commits,
            paste_ranges: Vec::new(),
        })
    }

    fn cmd_cd(&mut self, args: &str) -> Vec<Action> {
        if self.workspace_session.is_some() {
            let requested = if args.trim().is_empty() {
                "."
            } else {
                args.trim()
            };
            return match caudra_workspace::DirectoryNavigation::new(requested) {
                Ok(path) => vec![Action::ChangeRemoteWorkingDirectory(path)],
                Err(_) => {
                    self.flash("cd: invalid remote workspace path".into());
                    Vec::new()
                }
            };
        }
        let path = if args.is_empty() {
            caudra_storage::paths::home().unwrap_or_default()
        } else {
            match args.strip_prefix('~') {
                Some(rest) => {
                    let home = caudra_storage::paths::home().unwrap_or_default();
                    if rest.is_empty() {
                        home
                    } else {
                        home.join(rest.trim_start_matches('/'))
                    }
                }
                None => PathBuf::from(args),
            }
        };
        let canonical = match std::fs::canonicalize(&path) {
            Ok(path) if path.is_dir() => path,
            Ok(_) => {
                self.flash(format!("cd: not a directory: {}", path.display()));
                return Vec::new();
            }
            Err(error) => {
                self.flash(format!("cd: {error}"));
                return Vec::new();
            }
        };
        vec![Action::ChangeWorkingDirectory(canonical)]
    }

    pub(crate) fn install_working_directory(
        &mut self,
        cwd: &std::path::Path,
        permissions: PermissionsConfig,
    ) {
        self.release_revert_confirmation();
        self.file_picker.close();
        self.mention_popup.close();
        self.commit_popup.close();
        // Another project, another history. The next `#` reads it.
        self.commit_reload = CommitReload::Idle;
        self.set_commit_index(CommitIndex::Pending);
        self.bound_workbench_mut().bind_local();
        self.suspend_permission_editor();
        self.permission_config_trust_deferred = false;
        self.permission_review = None;
        self.permissions.set_project_with_config(cwd, permissions);
        self.state
            .session_mut()
            .set_cwd(cwd.to_string_lossy().into_owned());
        self.bind_change_recorder();
        self.refresh_record_index();
        self.status_bar.refresh_cwd(&cwd.to_string_lossy());
        self.sync_composer_cwd();
        self.sync_transcript_cwd();
        self.pending_pattern_suggestions = None;
        self.request_pattern_suggestions();
    }

    /// Adopts permissions files edited while the session runs. A remote
    /// workspace's policy comes from the workspace, so it keeps that one.
    pub(crate) fn reload_permissions(&mut self, cwd: &Path, permissions: PermissionsConfig) {
        if self.workspace_session.is_none() {
            self.permissions.set_project_with_config(cwd, permissions);
            self.permission_review = None;
        }
    }

    pub(crate) fn reload_commands(
        &mut self,
        commands: Arc<[caudra_agent::command::CustomCommand]>,
    ) {
        if self.workspace_session.is_none() {
            self.command_palette.set_custom_commands(commands);
        }
    }

    /// `builtin_skills.packs()` as saved now. A remote tab reads its commands from its own
    /// project, so it rediscovers them here; a local one takes them from [`Self::reload_commands`].
    pub(crate) fn set_skill_packs(&mut self, packs: SkillPacks) {
        self.skill_packs = packs;
        if self.settings_modal.is_open() {
            self.settings_modal.set_pstack(self.pstack_roles());
        }
        if !self.no_commands
            && let Some(context) = &self.remote_project_context
        {
            self.command_palette.set_custom_commands(Arc::from(
                caudra_agent::command::discover_remote_commands(context, &self.skill_packs),
            ));
        }
    }

    pub(crate) fn set_mcp_config_errors(&mut self, errors: McpConfigErrors) {
        self.mcp_picker.set_config_errors(errors);
    }

    /// Points the MCP picker, the palette's prompts, and the composer's
    /// resource mentions at a restarted manager.
    pub(crate) fn adopt_mcp_reader(&mut self, reader: McpSnapshotReader) {
        self.mcp_picker.set_reader(reader.clone());
        self.input_box.set_mcp_reader(reader.clone());
        self.mention_popup.set_mcp_reader(reader.clone());
        self.command_palette.set_mcp_reader(reader);
    }

    pub(crate) fn install_remote_working_directory(
        &mut self,
        change: shell::RemoteDirectoryChange,
    ) -> Result<(), String> {
        caudra_workspace::WorkspacePath::new(&change.display_path)
            .map_err(|error| error.to_string())?;
        let mut candidate = self.state.session.as_ref().clone();
        candidate
            .replace_workspace_cursor(change.binding.clone())
            .map_err(|_| "cd: session workspace identity changed".to_owned())?;
        candidate.set_cwd(change.display_path.clone());
        let candidate = Arc::new(candidate);
        self.permissions
            .replace_remote_permission_asset_after(change.context.permissions(), || {
                self.storage_writer
                    .save_sync(Arc::clone(&candidate))
                    .map_err(|_| {
                        "cd: session persistence failed; directory was not changed".to_owned()
                    })
            })
            .map_err(|error| error.to_string())?;
        self.release_revert_confirmation();
        self.file_picker.close();
        self.mention_popup.close();
        self.commit_popup.close();
        self.state.session = candidate;
        self.sync_transcript_cwd();
        self.command_palette
            .set_custom_commands(if self.no_commands {
                Arc::from([])
            } else {
                Arc::from(caudra_agent::command::discover_remote_commands(
                    &change.context,
                    &self.skill_packs,
                ))
            });
        self.workspace_session = Some(change.workspace);
        self.bind_change_recorder();
        self.refresh_record_index();
        self.invalidate_permission_authority();
        self.permission_review = None;
        if let Some(workspace) = self.workspace_session.clone()
            && let Err(error) = self
                .parked_workbench
                .as_mut()
                .unwrap_or(&mut self.workbench)
                .bind_workspace(workspace)
        {
            tracing::warn!(%error, "remote workbench backend rebind failed");
        }
        // Said once per manifest revision rather than once per turn: a rebind
        // is the only edge on which the declared assets can have changed.
        if let Some(warning) = change.context.skipped_warning() {
            self.flash(warning);
        }
        self.remote_project_context = Some(change.context);
        self.pending_pattern_suggestions = None;
        self.status_bar.set_remote_cwd(change.display_path);
        Ok(())
    }

    /// Points both composers at the session's working directory, which is what
    /// decides whether an `@path` resolves to a file or stays prose.
    fn sync_composer_cwd(&mut self) {
        // The log window belongs to the project, so moving to another one
        // retires it. Nothing is read here: the next `#` asks.
        self.set_commit_index(CommitIndex::Pending);
        if self.workspace_session.is_some() {
            self.input_box.set_remote_cwd();
            self.subagent_input_box.set_remote_cwd();
            return;
        }
        let cwd = PathBuf::from(&self.state.session.cwd);
        self.input_box.set_cwd(cwd.clone());
        self.subagent_input_box.set_cwd(cwd);
    }

    /// Points every transcript at the session's working directory, which is
    /// what a command that has not run yet resolves its `workdir` against.
    fn sync_transcript_cwd(&mut self) {
        let cwd = Path::new(&self.state.session.cwd);
        for chat in &mut self.chats {
            chat.set_cwd(cwd);
        }
    }

    /// Where a markdown image's relative path resolves, and whether local
    /// paths are read at all: a remote workspace's paths mean nothing here.
    /// Pictures are drawn for one session at a time, so the focused one sets
    /// it before each frame.
    pub(crate) fn sync_image_root(&self) {
        let local = self.workspace_session.is_none();
        inline_image::set_local_root(local.then(|| Path::new(&self.state.session.cwd)));
    }

    fn overlays(&self) -> [&dyn Overlay; 54] {
        [
            &self.workbench,
            &self.logs_modal,
            &self.docs_modal,
            &self.changelog_modal,
            &self.help_modal,
            &self.usage_modal,
            &self.context_modal,
            &self.tools_modal,
            &self.decisions_modal,
            &self.skills_modal,
            &self.storage_modal,
            &self.learn_inspector,
            &self.learn_transcripts,
            &self.system_prompt_modal,
            &self.projection_modal,
            &self.goal_panel,
            &self.autopilot_panel,
            &self.stream_modal,
            &self.float_mgr,
            &self.search_modal,
            &self.file_picker,
            &self.paste_editor,
            &self.image_viewer,
            &self.rewind_picker,
            &self.message_actions,
            &self.queue_actions,
            &self.mode_submission,
            &self.update_prompt,
            &self.review,
            &self.command_modal,
            &self.theme_picker,
            &self.thinking_picker,
            &self.prompt_profile_picker,
            &self.model_picker,
            &self.login_picker,
            &self.mcp_picker,
            &self.env_modal,
            &self.settings_modal,
            &self.permissions_picker,
            &self.sandbox_manager,
            &self.stash_picker,
            &self.memory_inspector,
            &self.task_picker,
            &self.shell_modal,
            &self.peer_manager,
            &self.workflow_inspector,
            &self.workflow_catalog_picker,
            &self.automation_inspector,
            &self.question_form,
            &self.session_picker,
            &self.session_relocation_picker,
            &self.worktree_picker,
            &self.accounts_modal,
            &self.permission_prompt,
        ]
    }

    fn overlays_mut(&mut self) -> [&mut dyn Overlay; 54] {
        [
            &mut self.workbench,
            &mut self.logs_modal,
            &mut self.docs_modal,
            &mut self.changelog_modal,
            &mut self.help_modal,
            &mut self.usage_modal,
            &mut self.context_modal,
            &mut self.tools_modal,
            &mut self.decisions_modal,
            &mut self.skills_modal,
            &mut self.storage_modal,
            &mut self.learn_inspector,
            &mut self.learn_transcripts,
            &mut self.system_prompt_modal,
            &mut self.projection_modal,
            &mut self.goal_panel,
            &mut self.autopilot_panel,
            &mut self.stream_modal,
            &mut self.float_mgr,
            &mut self.search_modal,
            &mut self.file_picker,
            &mut self.paste_editor,
            &mut self.image_viewer,
            &mut self.rewind_picker,
            &mut self.message_actions,
            &mut self.queue_actions,
            &mut self.mode_submission,
            &mut self.update_prompt,
            &mut self.review,
            &mut self.command_modal,
            &mut self.theme_picker,
            &mut self.thinking_picker,
            &mut self.prompt_profile_picker,
            &mut self.model_picker,
            &mut self.login_picker,
            &mut self.mcp_picker,
            &mut self.env_modal,
            &mut self.settings_modal,
            &mut self.permissions_picker,
            &mut self.sandbox_manager,
            &mut self.stash_picker,
            &mut self.memory_inspector,
            &mut self.task_picker,
            &mut self.shell_modal,
            &mut self.peer_manager,
            &mut self.workflow_inspector,
            &mut self.workflow_catalog_picker,
            &mut self.automation_inspector,
            &mut self.question_form,
            &mut self.session_picker,
            &mut self.session_relocation_picker,
            &mut self.worktree_picker,
            &mut self.accounts_modal,
            &mut self.permission_prompt,
        ]
    }

    pub fn any_overlay_open(&self) -> bool {
        self.overlays().iter().any(|o| o.is_open())
    }

    /// True when the agent is parked on user input. Drives the `needs_input`
    /// session status. A deferred question parks nothing: its answer has to
    /// be able to wake the agent.
    pub(crate) fn awaiting_input(&self) -> bool {
        (self.permission_prompt.is_open()
            && self.permission_prompt.autopilot_note() != Some(&AutopilotNote::Deciding))
            || self.pending_input != PendingInput::None
            || self.float_mgr.needs_input()
            || self.blocking_question_waits()
    }

    /// What keeps this session from closing or changing, read the way its
    /// notifications do.
    pub(crate) fn lifecycle_blocker(&self) -> Option<Cow<'static, str>> {
        if self.sandbox_network_reconciliation_pending() {
            return Some(Cow::Borrowed(
                "Wait for saved-network reconciliation to finish before closing this session; its results must be retained",
            ));
        }
        if crate::sandbox::transfer::active() {
            return Some(Cow::Borrowed(
                "Close/cancel the transfer and await cleanup before changing sessions or exiting",
            ));
        }
        if self.sandbox_manager.pending() {
            return Some(Cow::Borrowed(
                "Waiting for durable sandbox configuration acknowledgment",
            ));
        }
        if self.sandbox_manager.dirty() {
            return Some(Cow::Borrowed(SANDBOX_DRAFT_BLOCKER));
        }
        if self.permission_mutation_pending() {
            return Some(Cow::Borrowed(
                "Waiting for durable permission acknowledgment",
            ));
        }
        if let Some(attention) = self.attention() {
            return Some(Cow::Owned(attention.message()));
        }
        [
            (self.question_form_waits(), QUESTION_BLOCKER),
            (self.login_picker.is_open(), LOGIN_BLOCKER),
            (
                self.mcp_picker.is_open() && self.mcp_picker.has_awaiting_trust(),
                MCP_TRUST_BLOCKER,
            ),
            (
                self.permissions_picker.is_open()
                    && self.permissions.needs_project_permission_config_trust(),
                PROJECT_PERMISSION_CONFIG_TRUST_BLOCKER,
            ),
        ]
        .into_iter()
        .find_map(|(blocked, message)| blocked.then_some(Cow::Borrowed(message)))
    }

    /// What the pane shows for this session. The user acting is the only way
    /// out of a blocked state, which names what waits for them; Caudra
    /// waiting on itself is work.
    pub(crate) fn pane_status(&self) -> PaneStatus {
        let attention = self.attention().and_then(|attention| {
            let kind = match &attention {
                Notification::PermissionRequested { .. } if !self.permission_mutation_pending() => {
                    BlockKind::Permission
                }
                Notification::AuthenticationRequired => BlockKind::Auth,
                Notification::QuestionRequested | Notification::PlanReady => BlockKind::Question,
                Notification::AutopilotNeedsYou {
                    wait: DecisionKind::Permission,
                    ..
                } if !self.permission_mutation_pending() => BlockKind::Permission,
                Notification::AutopilotNeedsYou { wait, .. }
                    if *wait != DecisionKind::Permission =>
                {
                    BlockKind::Question
                }
                Notification::PermissionRequested { .. }
                | Notification::AutopilotNeedsYou { .. }
                | Notification::TurnComplete { .. }
                | Notification::AutomationNotice { .. }
                | Notification::AutopilotEnded { .. } => return None,
            };
            Some((Some(kind), Cow::Owned(attention.message())))
        });
        let blocker = attention.or_else(|| {
            [
                (
                    self.question_form_waits(),
                    Some(BlockKind::Question),
                    QUESTION_BLOCKER,
                ),
                (
                    self.mcp_picker.is_open() && self.mcp_picker.has_awaiting_trust(),
                    Some(BlockKind::Permission),
                    MCP_TRUST_BLOCKER,
                ),
                (
                    self.permissions_picker.is_open()
                        && !self.permission_mutation_pending()
                        && self.permissions.needs_project_permission_config_trust(),
                    Some(BlockKind::Permission),
                    PROJECT_PERMISSION_CONFIG_TRUST_BLOCKER,
                ),
                (
                    self.login_picker.is_open(),
                    Some(BlockKind::Auth),
                    LOGIN_BLOCKER,
                ),
                (
                    self.sandbox_manager.dirty() && !self.sandbox_manager.pending(),
                    None,
                    SANDBOX_DRAFT_BLOCKER,
                ),
            ]
            .into_iter()
            .find_map(|(blocked, kind, label)| blocked.then_some((kind, Cow::Borrowed(label))))
        });
        if let Some((kind, label)) = blocker {
            return PaneStatus {
                label: Some(label),
                ..PaneStatus::new(PaneState::Blocked(kind))
            };
        }
        PaneStatus::new(
            if self.has_lifecycle_work()
                || self.sandbox_network_reconciliation_pending()
                || crate::sandbox::transfer::active()
                || self.sandbox_manager.pending()
                || self.permission_mutation_pending()
            {
                PaneState::Working
            } else {
                PaneState::Idle
            },
        )
    }

    pub(crate) fn has_lifecycle_work(&self) -> bool {
        self.status == Status::Streaming
            || self.chats.iter().any(|chat| chat.retry().is_some())
            || self.restoring.load(Ordering::Relaxed)
            || self.stream_modal.is_streaming()
    }

    /// True while `recoverable_queue` holds user text captured at an agent
    /// error; a background run would wipe it (`start_run` clears the queue).
    pub(crate) fn holds_recovery_text(&self) -> bool {
        !self.recoverable_queue.is_empty()
    }

    pub(crate) fn prepare_shutdown(&mut self) {
        self.pending_pattern_suggestions = None;
        self.pattern_suggestion_loader = None;
        self.release_revert_confirmation();
        if self.recoverable_queue.is_empty() {
            self.recoverable_queue = self.queue.pending_prompts();
            self.recoverable_queue_together =
                self.queue.delivery() == caudra_agent::QueueDelivery::TogetherNextTurn;
        }
        self.queue.clear();
        self.shell.cancel_all();
        self.mark_open_run_shut_down();
    }

    pub(crate) fn disconnect_agent_queue(&mut self) {
        self.queue.disconnect();
    }

    pub(crate) fn goal_checkin_due(&self) -> bool {
        (self.goal_deferred
            || self.autopilot_checkin_due
            || self.state.goal.wait_over()
            || self.state.goal.kickoff_pending())
            && self.state.goal.snapshot().is_some()
    }

    /// The goal waits on an outside change, and its check-in is still to come.
    pub(crate) fn goal_waits(&self) -> bool {
        self.state
            .goal
            .wait_left()
            .is_some_and(|left| !left.is_zero())
    }

    pub(crate) fn attention(&self) -> Option<Notification> {
        if let Some(wait) = self.autopilot_wait() {
            return Some(wait);
        }
        if let Some(tool) = self.permission_prompt.tool()
            && self.permission_prompt.autopilot_note().is_none()
        {
            let tool = (!matches!(tool, caudra_config::ToolKey::Wildcard))
                .then(|| normalize_preview(&tool.to_string()))
                .flatten();
            return Some(Notification::PermissionRequested { tool });
        }
        if matches!(self.pending_input, PendingInput::AuthRetry { .. }) {
            return Some(Notification::AuthenticationRequired);
        }
        if self.status != Status::Streaming && self.plan_form_active() {
            return Some(Notification::PlanReady);
        }
        self.float_mgr
            .needs_input()
            .then_some(Notification::QuestionRequested)
    }

    /// A resize moves every anchor, so a box drawn for the old layout would
    /// point at whatever took its place.
    pub(crate) fn dismiss_tooltip(&mut self) {
        self.tooltip.dismiss();
    }

    pub fn has_modal_overlay(&self) -> bool {
        self.overlays().iter().any(|o| o.is_open() && o.is_modal())
    }

    pub fn close_all_overlays(&mut self) {
        self.shelve_deferred_question();
        self.pending_plan_submission = None;
        self.suspend_permission_editor();
        self.close_stream_modal();
        self.overlays_mut().iter_mut().for_each(|o| o.close());
        self.settings.parked = None;
    }

    /// Hides the update banner in every session until Caudra restarts. Nothing
    /// is saved, so the next start announces the same release again.
    pub(super) fn dismiss_update(&mut self) {
        if self.update_banner.get().is_none() {
            self.flash(NO_UPDATE_MSG.into());
            return;
        }
        self.update_notice.dismiss();
        self.update_banner = Watch::default();
        self.update_close = None;
        self.update_text = None;
    }

    /// The prompt needs every session's state, which only the event loop
    /// holds, so it opens from there.
    pub(super) fn request_update(&mut self) -> Vec<Action> {
        if self.update_banner.get().is_none() {
            self.flash(NO_UPDATE_AVAILABLE_MSG.into());
            return Vec::new();
        }
        vec![Action::OpenUpdatePrompt]
    }

    /// `working` counts the sessions the update would stop mid-work.
    pub(crate) fn open_update_prompt(&mut self, flow: &UpdateFlow, working: usize) {
        let Some(available) = self.update_banner.get() else {
            self.flash(NO_UPDATE_AVAILABLE_MSG.into());
            return;
        };
        match flow {
            UpdateFlow::Unavailable(reason) => self.flash(reason.clone()),
            flow => {
                self.update_prompt
                    .open(available, *flow == UpdateFlow::Restart, working);
            }
        }
    }

    pub(super) fn handle_update_prompt(&mut self, action: UpdatePromptAction) -> Vec<Action> {
        let choice = match action {
            UpdatePromptAction::Consumed => return Vec::new(),
            UpdatePromptAction::Copy(text) => {
                self.copy_to_clipboard(&text);
                return Vec::new();
            }
            UpdatePromptAction::Close => UpdateChoice::NotNow,
            UpdatePromptAction::Select(choice) => choice,
        };
        self.update_prompt.close();
        match (choice, self.update_banner.get()) {
            (UpdateChoice::Restart | UpdateChoice::Exit, Some(available)) => {
                let channel = available.channel;
                self.quit_with(ExitRequest::Update(channel))
            }
            _ => Vec::new(),
        }
    }

    /// Every poller that feeds the screen, in one place and never in `view`;
    /// see [`crate::repaint`] for why.
    pub fn tick(&mut self) -> Dirty {
        // `|` never short-circuits: every poller must run on every tick.
        self.float_mgr.tick()
            | self.tick_edge_scroll()
            | self.tick_autoscroll()
            | self.tick_error_expiry()
            | self.poll_image_paste()
            | self.tick_stream_modal()
            | self.status_bar.poll_branch_update()
            | self.status_bar.clear_expired_hint()
            | self.refresh_mcp_picker()
            | self.tick_permission_config_trust()
            | self.poll_record_index()
            | self.poll_pattern_suggestions()
            | self.poll_permission_editor()
            | self.poll_sandbox()
            | self.poll_env()
            | self.poll_settings()
            | self.model_picker.refresh()
            | self.poll_usage()
            | self.storage_modal.poll(&self.storage_slot)
            | self.decisions_modal.poll(&self.decisions_slot)
            | self.autopilot_panel.poll()
            | self.goal_panel.poll()
            | self.poll_goal_log()
            | self.poll_context_snapshot()
            | self.poll_tools_snapshot()
            | self.logs_modal.poll()
            | self.hints.poll(self.hint_reader.load_full())
            | self.update_banner.poll(self.update_notice.latest())
            | self.tick_file_picker()
            | self.mention_popup.tick()
            | self.command_palette.poll_completion()
            | self.commit_popup.tick()
            | self.refresh_session_picker()
            | self.poll_workflow_replies()
            | self.poll_automations()
            | self.poll_memory()
            | self.poll_learning()
            | self.learn_transcripts.tick()
            | self.poll_task_controls()
            | self.poll_task_continuation()
            | self.poll_task_history()
            | self.poll_history_search(Instant::now())
            | self.poll_shells()
            | self.tick_workbench()
            | self.poll_commit_index()
            | self.thinking_picker.tick()
            | self.poll_accounts_refresh()
            | Dirty::any(self.chats.iter_mut().map(Chat::tick))
    }

    /// A settled quota fetch also moved the readings `/usage` lists per account.
    fn poll_usage(&mut self) -> Dirty {
        let dirty = self.usage_modal.poll(&self.usage_slot);
        if dirty == Dirty::YES {
            self.reload_usage_accounts();
        }
        dirty
    }

    fn refresh_mcp_picker(&mut self) -> Dirty {
        self.mcp_picker.refresh()
    }

    fn poll_context_snapshot(&mut self) -> Dirty {
        if !self.context_modal.is_open() && !self.skills_modal.is_open() {
            return Dirty::NO;
        }
        let snapshot = self.active_context_snapshot();
        self.context_snapshot.poll(snapshot)
    }

    fn poll_tools_snapshot(&mut self) -> Dirty {
        if !self.tools_modal.is_open() {
            return Dirty::NO;
        }
        let (snapshot, basis) = if self.active_subagent_id().is_some() {
            self.tools_preview_cache.key = None;
            let snapshot = self.active_context_snapshot();
            let basis = snapshot.as_ref().map_or_else(
                || "Task · awaiting tools snapshot".to_owned(),
                |snapshot| format!("Task · {}", tools_mode_name(&snapshot.mode)),
            );
            (snapshot, basis)
        } else if self.status == Status::Streaming
            || self.queue.is_processing()
            || self.chats[0].retry().is_some()
            || self.cancelling_run.is_some()
        {
            self.tools_preview_cache.key = None;
            let mode = self.execution_agent_mode();
            let mut basis = format!("Executing {}", tools_mode_name(&mode));
            if self.agent_mode() != mode {
                let selected = if self.state.mode == Mode::Plan {
                    "Plan"
                } else {
                    "Build"
                };
                basis.push_str(&format!(" · Selected {selected} pending"));
            }
            (self.main_context_snapshot(), basis)
        } else {
            let selected = if self.state.mode == Mode::Plan {
                "Plan"
            } else {
                "Build"
            };
            (
                self.selected_tools_snapshot(),
                format!("Selected {selected} · next user message"),
            )
        };
        let changed = self.tools_basis != basis;
        self.tools_basis = basis;
        self.tools_snapshot.poll(snapshot) | Dirty::from(changed)
    }

    fn selected_tools_snapshot(&mut self) -> Option<Arc<ContextSnapshot>> {
        let source = Arc::clone(self.tools_preview_source.as_ref()?);
        if source.profile_name() != self.state.system_prompt_profile_name {
            return None;
        }
        let mode = self.agent_mode();
        let mut model = if self.state.mode == Mode::Plan {
            Model::resolve_if_available(ModelPurpose::Plan, &self.state.model, &self.model_policy)
                .ok()?
        } else {
            self.state.model.clone()
        };
        adjust_model_for_inspection(&mut model);
        let key = ToolsPreviewKey {
            session_plan: mode.has_session_plan(self.state.plan.target().as_ref()),
            mode,
            chat_model: self.state.model.spec(),
            model: model.spec(),
            bindings: ModelPurpose::ALL.map(model_registry::binding),
            window: model.context_window,
            thinking: self.state.thinking.clone(),
            cwd: self.state.session.cwd.clone(),
            registry_revision: source.registry.authority_snapshot().revision(),
        };
        let cache = &mut self.tools_preview_cache;
        let changed = cache.source.poll(Some(Arc::clone(&source)))
            | cache
                .mcp
                .poll(source.mcp.as_ref().map(|mcp| mcp.reader().load_full()));
        if cache.key.as_ref() != Some(&key) || changed == Dirty::YES {
            cache.snapshot = Some(Arc::new(source.snapshot(
                &model,
                &self.state.model,
                &key.thinking,
                &key.mode,
                key.session_plan,
                &key.cwd,
            )));
            cache.key = Some(key);
        }
        cache.snapshot.clone()
    }

    fn tick_workbench(&mut self) -> Dirty {
        let mut parked_dirty = Dirty::NO;
        if let Some(parked) = &mut self.parked_workbench {
            let (changed, flash) = parked.tick();
            parked_dirty = Dirty::from(changed);
            if let Some(flash) = flash {
                self.status_bar.flash(flash);
            }
        }
        if !self.workbench.is_open() && !self.workbench.is_busy() {
            return parked_dirty;
        }
        self.sync_workbench_theme();
        // The workbench paints its own bars, so the setting reaches it here
        // rather than through the shared helper every other surface calls.
        self.workbench.set_scrollbars(scrollbar::enabled());
        let moved = self.sync_workbench_mounts();
        let (dirty, flash) = self.workbench.tick();
        if let Some(flash) = flash {
            self.status_bar.flash(flash);
        }
        self.persist_workbench_layout();
        let images = inline_image::generation();
        let decoded = self.workbench_image_gen != images;
        self.workbench_image_gen = images;
        parked_dirty | Dirty::from(dirty || moved || decoded)
    }

    /// A layout moves when a tab opens or a pane resizes, which is rare enough
    /// to write on the change itself rather than on a timer or on each of the
    /// several ways an overlay can leave the screen.
    ///
    /// A workspace session stores nothing: its layout was never read back, and
    /// would be filed under a path that names no directory of this machine.
    fn persist_workbench_layout(&mut self) {
        if self.parked_workbench.is_some() || self.workspace_session.is_some() {
            return;
        }
        let layout = self.workbench.layout();
        if self.workbench_layout.as_ref() == Some(&layout) {
            return;
        }
        let cwd = PathBuf::from(&self.state.session.cwd);
        caudra_storage::workbench::persist(&self.storage, &cwd, &layout);
        self.workbench_layout = Some(layout);
    }

    fn tick_permission_config_trust(&mut self) -> Dirty {
        if !self.permission_config_trust_deferred
            || self.login_picker.is_open()
            || self.mcp_picker.is_open()
            || self.permission_prompt.is_open()
            || self.permission_job_pending()
        {
            return Dirty::NO;
        }
        self.open_awaiting_permission_config_trust(false);
        Dirty::YES
    }

    fn tick_file_picker(&mut self) -> Dirty {
        let (dirty, flash) = self.file_picker.tick();
        if let Some(flash) = flash {
            self.status_bar.flash(flash);
        }
        dirty
    }

    /// Reads the ledger the first time the lifetime view is asked for. A
    /// failure leaves `None`, which the modal renders as unavailable rather
    /// than as zero spend.
    fn load_lifetime_usage(&mut self) {
        self.load_warming_summary();
        if self.usage_modal.scope() != UsageScope::Lifetime || self.lifetime_usage.is_some() {
            return;
        }
        match UsageLedger::open(&self.storage).and_then(|ledger| ledger.lifetime()) {
            Ok(lifetime) => self.lifetime_usage = Some(lifetime),
            Err(error) => tracing::warn!(%error, "lifetime usage unavailable"),
        }
    }

    fn load_warming_summary(&mut self) {
        let scope = self.usage_modal.scope();
        if self
            .warming_summary
            .as_ref()
            .is_some_and(|(loaded, _)| *loaded == scope)
        {
            return;
        }
        let session = (scope == UsageScope::Session).then(|| self.state.session.id.to_string());
        self.warming_summary = match CacheWarmLedger::open(&self.storage)
            .and_then(|ledger| ledger.summary(session.as_deref()))
        {
            Ok(summary) => Some((scope, summary)),
            Err(error) => {
                tracing::warn!(%error, "cache warming summary unavailable");
                None
            }
        };
    }

    /// Rereads what `/usage` shows, as `Ctrl+R` or a click on its hint asks.
    pub(super) fn reload_usage(&mut self) -> Vec<Action> {
        self.lifetime_usage = None;
        self.warming_summary = None;
        self.load_lifetime_usage();
        vec![Action::RefreshUsage]
    }

    /// Hands the event to `/tools`, then loads what a scope it moved to reads.
    pub(super) fn on_tools_modal(&mut self, handle: impl FnOnce(&mut ToolsModal)) {
        let scope = self.tools_modal.scope();
        handle(&mut self.tools_modal);
        if self.tools_modal.scope() != scope {
            self.load_tool_stats();
        }
    }

    /// Reads the ledger for the scope `/tools` is showing, which is a snapshot
    /// of when it was asked for rather than a live view: re-querying per tool
    /// call would put a synchronous read on the event loop for a panel nobody
    /// is usually watching. Called on open and on every scope change, so an
    /// answer is never older than the panel showing it.
    ///
    /// A failure leaves `None`, which the modal renders as unavailable rather
    /// than as no activity.
    fn load_tool_stats(&mut self) {
        self.tool_stats = None;
        let cwd = match self.tools_modal.scope() {
            ToolsScope::Project => Some(self.state.session.cwd.clone()),
            ToolsScope::Global => None,
            ToolsScope::Inventory | ToolsScope::Session => return,
        };
        let stats = ToolLedger::open(&self.storage).and_then(|ledger| match &cwd {
            Some(cwd) => ledger.project(cwd),
            None => ledger.lifetime(),
        });
        match stats {
            Ok(stats) => self.tool_stats = Some(stats),
            Err(error) => tracing::warn!(%error, "tool activity unavailable"),
        }
    }

    /// The one place a finished call is recorded. The session keeps its own
    /// breakdown so `/tools` can still answer for it after a resume, and the
    /// ledger keeps the project's history: a forgotten session must not take
    /// what it did with it.
    ///
    /// A call with no accounting never reached the dispatch site that fills it
    /// in — a synthetic cancellation, say — and recording a zero for it would
    /// invent activity.
    ///
    /// Each server call an `code` snippet made counts as a call of its own,
    /// under its `server.tool` name. The model never reads those results
    /// directly, so they cost the context window nothing.
    ///
    /// Each child a batch ran counts as a call of its own too. The batch's
    /// tokens are what the model read of the whole answer, the children's
    /// sections included, so each child takes its own share out of them and
    /// the batch keeps the remainder: the rows still add up to what the
    /// context window was charged.
    fn record_tool_call(&mut self, done: &caudra_agent::ToolDoneEvent) {
        let (Some(outcome), Some(source)) = (done.accounting.outcome, &done.accounting.source)
        else {
            return;
        };
        let mut tokens = done.accounting.model_tokens;
        if let ToolOutput::Batch { entries, .. } = &done.output {
            for entry in entries {
                let accounting = &entry.accounting;
                let (Some(outcome), Some(source)) = (accounting.outcome, &accounting.source) else {
                    continue;
                };
                let child_tokens = accounting.model_tokens.min(tokens);
                tokens -= child_tokens;
                self.record_tool_usage(ToolCall {
                    tool: entry.tool.clone(),
                    source: source.to_string(),
                    cwd: self.state.session.cwd.clone(),
                    outcome,
                    duration_ms: accounting.duration_ms,
                    tokens: child_tokens,
                });
                if let Some(output) = &entry.output {
                    self.record_code_calls(output);
                }
            }
        }
        self.record_tool_usage(ToolCall {
            tool: done.tool.to_string(),
            source: source.to_string(),
            cwd: self.state.session.cwd.clone(),
            outcome,
            duration_ms: done.accounting.duration_ms,
            tokens,
        });
        self.record_code_calls(&done.output);
    }

    fn record_code_calls(&mut self, output: &ToolOutput) {
        let ToolOutput::Code(run) = output else {
            return;
        };
        for call in &run.calls {
            let (tool, source) = call.ledger_name();
            self.record_tool_usage(ToolCall {
                tool,
                source,
                cwd: self.state.session.cwd.clone(),
                outcome: call.ledger_outcome(),
                duration_ms: call.duration_ms,
                tokens: 0,
            });
        }
    }

    fn record_tool_usage(&mut self, call: ToolCall) {
        self.state.session_mut().add_tool_usage(
            &call.tool,
            &call.source,
            call.outcome,
            call.duration_ms,
            call.tokens,
        );
        self.storage_writer.record_tool_call(call);
    }

    /// The one place a turn's spend is recorded. The session keeps its own
    /// per-model breakdown for `/usage`, and the ledger keeps the money: a
    /// forgotten session must not take what it cost with it.
    ///
    /// `provider` comes from whoever answered rather than from the chat model.
    /// Goal evaluation, compaction and titles can each resolve to another
    /// provider, and billing them to the conversation's provider would make
    /// `--group-by provider` describe a session that never happened. `account`
    /// is the provider account that served the turn, when routing named one.
    #[allow(clippy::too_many_arguments)]
    fn record_model_usage(
        &mut self,
        provider: &str,
        account: Option<&str>,
        model: &str,
        purpose: LedgerPurpose,
        usage: TokenUsage,
        cost: Option<f64>,
        billing: Billing,
    ) {
        let key = session_usage_model(provider, model);
        self.state
            .session_mut()
            .add_model_usage(&key, purpose, usage.billed(cost, billing));
        if purpose.ledgered_at_source() {
            return;
        }
        self.storage_writer.record_usage(TurnUsage {
            provider: provider.to_owned(),
            account: account.unwrap_or_default().to_owned(),
            model: model.to_owned(),
            cwd: self.state.session.cwd.clone(),
            purpose,
            input: usage.input,
            output: usage.output,
            cache_creation: usage.cache_creation,
            cache_read: usage.cache_read,
            cost,
            subscription: billing.is_subscription(),
        });
    }

    /// The evaluator reports a fully qualified spec because it may have been
    /// served by a provider this session never chose. A resolution that failed
    /// before reaching a model reports no spec and no spend, and recording
    /// zeroes would invent a row for a request that never happened.
    fn record_goal_usage(
        &mut self,
        spec: &str,
        usage: TokenUsage,
        cost: Option<f64>,
        billing: Billing,
    ) {
        if usage.context_tokens() == 0 && cost.is_none() {
            return;
        }
        let (provider, model) = match spec.split_once('/') {
            Some((provider, model)) => (provider.to_owned(), model.to_owned()),
            None => (self.state.model.provider.to_string(), spec.to_owned()),
        };
        self.record_model_usage(
            &provider,
            None,
            &model,
            LedgerPurpose::Goal,
            usage,
            cost,
            billing,
        );
    }

    /// The session's running spend, filed under whoever pays for it. Every
    /// arrival goes through here so the two totals never mix.
    fn add_session_spend(&mut self, cost: Option<f64>, billing: Billing) {
        match billing {
            Billing::Api => add_cost(&mut self.state.cost, cost),
            Billing::Subscription => add_cost(&mut self.state.subscription_cost, cost),
        }
    }

    /// A side request (`/btw`, `/extract`) spends real tokens outside any
    /// turn, so it settles into the same ledger as compaction and the goal
    /// evaluator. It deliberately leaves `context_size` alone: the request
    /// never enters history.
    fn tick_stream_modal(&mut self) -> Dirty {
        if self.stream_modal.is_clarification() && !self.question_form.is_open() {
            self.discard_question_btw();
            return Dirty::YES;
        }
        let dirty = self.stream_modal.poll();
        if self.settle_stream_modal()
            && self.stream_modal.is_open()
            && let Some(question) = self.stream_modal.take_queued()
            && self.btw_thread.is_some()
        {
            self.continue_btw(question);
        }
        if self.btw_thread.is_some()
            && !self.stream_modal.is_open()
            && !self.stream_modal.is_clarification()
        {
            self.end_btw_thread(true);
            return Dirty::YES;
        }
        dirty
    }

    fn settle_stream_modal(&mut self) -> bool {
        let Some(done) = self.stream_modal.take_done() else {
            return false;
        };
        let spend = done.usage;
        self.state.token_usage += spend.usage;
        self.add_session_spend(spend.cost, spend.billing);
        add_chat_spend(self.main_chat(), spend.cost, spend.billing);
        self.record_model_usage(
            &spend.provider,
            None,
            &spend.model,
            spend.purpose,
            spend.usage,
            spend.cost,
            spend.billing,
        );
        self.record_external_spend(spend.usage, spend.cost, spend.billing, spend.purpose);
        if matches!(spend.purpose, LedgerPurpose::Btw) {
            self.settle_btw(done.answer);
        }
        true
    }

    /// What moves with the clock alone; changes that come from arriving data
    /// are reported by [`Self::tick`] instead. Overlays answer as a group, so
    /// adding one to [`Self::overlays`] is enough.
    pub fn cadence(&self) -> Cadence {
        Cadence::any([
            Cadence::any(self.overlays().into_iter().map(Overlay::cadence)),
            self.status_bar.cadence(
                &self.status,
                self.restoring.load(Ordering::Relaxed),
                self.state.goal.snapshot().is_some(),
                self.waiting_for_background(),
            ),
            self.selection_state
                .as_ref()
                .map_or(Cadence::IDLE, SelectionState::cadence),
            Cadence::when(self.autoscroll.is_some(), Cadence::SMOOTH),
            Cadence::any(self.chats.iter().map(Chat::cadence)),
            self.which_key.cadence(),
            self.tooltip.cadence(Instant::now()),
            self.automation_cadence(),
            self.memory_cadence(),
            self.learning_cadence(),
            Cadence::when(self.shell_env.reply.is_some(), Cadence::PENDING),
            // The `#` popup is not an overlay, so its spinner has to be asked
            // for here rather than through `overlays`.
            self.commit_popup.cadence(),
        ])
    }

    fn finish_subagents(&mut self, outcome: TaskOutcome, text: &str) {
        self.discard_all_pending_delegations();
        self.retain_resolved_subagents(outcome, text, false);
        self.chat_index.clear();
    }

    /// Terminalizes every tool left in progress when a turn ends, sparing
    /// shell commands that outlive the agent.
    fn terminalize_turn(&mut self, message: &str) {
        self.discard_all_pending_delegations();
        self.retain_resolved_subagents(TaskOutcome::Error, ERROR_TEXT, true);
        self.chats[0].fail_in_progress_except(message.into(), self.shell.active_ids());
        for chat in self.chats.iter_mut().skip(1) {
            if chat.task_id().is_some_and(|id| {
                self.background
                    .as_ref()
                    .is_some_and(|background| background.resident_status(id).is_some())
            }) {
                continue;
            }
            chat.fail_in_progress_with_message(message.into());
        }
        self.sync_todo_panel();
    }

    /// Marks unfinished subagent chats as ended and drops them from
    /// `chat_index`, so the session records only the children that really
    /// completed.
    fn retain_resolved_subagents(
        &mut self,
        outcome: TaskOutcome,
        text: &str,
        preserve_session_tasks: bool,
    ) {
        self.chat_index.retain(|_, &mut sub_idx| {
            if self.chats[sub_idx].is_finished()
                || preserve_session_tasks
                    && self.chats[sub_idx].task_id().is_some_and(|id| {
                        self.background
                            .as_ref()
                            .is_some_and(|background| background.resident_status(id).is_some())
                    })
            {
                true
            } else {
                self.chats[sub_idx].mark_finished(outcome, text);
                false
            }
        });
        self.sync_subagents();
    }

    pub fn flush_all_chats(&mut self) {
        for chat in &mut self.chats {
            chat.flush();
        }
    }

    fn route_text_paste(&mut self, text: &str) {
        if self.docs_holds_input() {
            self.docs_modal.handle_paste(text);
            return;
        }
        if (self.mode_submission.is_open() || self.update_prompt.is_open())
            && !self.permission_prompt_holds_input()
            && !self.question_form.is_open()
        {
            return;
        }
        if self.sandbox_manager.handle_paste(text) {
            return;
        }
        self.sync_subagent_input_target();
        if self.permission_prompt.handle_paste(text) {
            return;
        }
        if self.peer_manager.handle_paste(text) {
            return;
        }
        if self.plan_form_in_view() {
            return;
        }
        if self.float_mgr.handle_paste(text) {
            return;
        }
        if self.stream_modal.handle_paste(text) {
            return;
        }
        if self.logs_modal.is_open() {
            self.logs_modal.handle_paste(text);
            return;
        }
        if self.help_modal.is_open() {
            self.help_modal.handle_paste(text);
            return;
        }
        if self.search_modal.is_open() {
            self.search_modal.handle_paste(text);
            self.handle_search_action(SearchAction::QueryChanged);
            return;
        }
        macro_rules! try_picker {
            ($picker:expr) => {
                if $picker.handle_paste(text) {
                    return;
                }
            };
        }
        try_picker!(self.file_picker);
        try_picker!(self.rewind_picker);
        try_picker!(self.message_actions);
        try_picker!(self.queue_actions);
        try_picker!(self.review);
        if self.command_modal.handle_paste(text) {
            self.resync_command_palette();
            return;
        }
        try_picker!(self.theme_picker);
        try_picker!(self.prompt_profile_picker);
        try_picker!(self.model_picker);
        try_picker!(self.thinking_picker);
        try_picker!(self.mcp_picker);
        try_picker!(self.env_modal);
        try_picker!(self.settings_modal);
        try_picker!(self.permissions_picker);
        try_picker!(self.stash_picker);
        if let Some(action) = self.memory_inspector.handle_paste(text) {
            self.handle_memory_action(action);
            return;
        }
        try_picker!(self.task_picker);
        try_picker!(self.shell_modal);
        if let Some(action) = self.workflow_inspector.handle_paste(text) {
            self.handle_workflow_inspector_action(action);
            return;
        }
        try_picker!(self.workflow_catalog_picker);
        if let Some(action) = self.automation_inspector.handle_paste(text) {
            self.handle_automation_action(action);
            return;
        }
        try_picker!(self.session_picker);
        try_picker!(self.session_relocation_picker);
        try_picker!(self.worktree_picker);
        try_picker!(self.accounts_modal);
        try_picker!(self.question_form);
        try_picker!(self.login_picker);
        if !self.is_main_chat()
            && !(self.active_subagent_has_composer() || self.queue_editor_active())
        {
            return;
        }
        self.key_focus = KeyFocus::Composer;
        if let InputAction::PaletteSync(val) = self.active_input_box_mut().handle_paste(text) {
            self.sync_dropdowns(&val);
        }
    }

    pub(crate) fn open_image_viewer(&mut self, image: ImageRef) {
        self.selection_state = None;
        self.image_viewer.open(image);
    }

    pub(super) fn handle_image_viewer_action(&mut self, action: ImageViewerAction) -> Vec<Action> {
        match action {
            ImageViewerAction::Consumed | ImageViewerAction::Passthrough => {}
            ImageViewerAction::Close => self.image_viewer.close(),
            ImageViewerAction::CopyPath(path) => self.copy_to_clipboard(&path),
            ImageViewerAction::OpenExternal(path) => return vec![Action::OpenFile(path)],
            ImageViewerAction::OpenInWorkbench(path) => {
                self.image_viewer.close();
                self.open_workbench_file(&path, None);
            }
        }
        Vec::new()
    }

    pub(super) fn open_paste_editor(&mut self, id: crate::input_document::PasteId) {
        let Some(text) = self.active_input_box().paste_text(id).map(str::to_owned) else {
            return;
        };
        let Some(target) = self.active_input_target() else {
            return;
        };
        self.paste_editor.open(target, id, text);
    }

    fn active_input_target(&self) -> Option<PasteEditorTarget> {
        if self.is_main_chat() {
            Some(PasteEditorTarget::Main)
        } else {
            self.active_subagent_id()
                .map(|id| PasteEditorTarget::Subagent(id.to_owned()))
        }
    }

    /// What a prompt draft opens on, with every folded paste spelled out.
    fn active_input_text(&self) -> String {
        self.active_input_box().expanded_text()
    }

    /// Puts a saved prompt draft into the composer. A draft that still reads
    /// the way the composer does is left alone, so saving an untouched one
    /// keeps its pastes folded.
    fn apply_prompt_draft(&mut self, text: String) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if text == self.active_input_text() {
            return;
        }
        let input = self.active_input_box_mut();
        input.buffer.replace_all(&text);
        input.move_to_end();
        self.resync_dropdowns();
    }

    fn handle_plan_form_action(&mut self, action: PlanFormAction) -> Vec<Action> {
        match action {
            PlanFormAction::Consumed | PlanFormAction::Passthrough => vec![],
            PlanFormAction::Hide => {
                self.plan_form.hide();
                self.arm_autopilot_stop();
                vec![]
            }
            PlanFormAction::OpenEditor => {
                self.open_plan();
                vec![]
            }
            PlanFormAction::AskBtw => {
                self.open_btw_prompt();
                vec![]
            }
            PlanFormAction::Implement => self.implement_plan(false),
            PlanFormAction::ClearAndImplement => self.implement_plan(true),
        }
    }

    fn implement_plan(&mut self, clear_context: bool) -> Vec<Action> {
        if self.plan_unsaved() {
            return vec![];
        }
        let snapshot = match self.capture_plan() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.flash(format!("{IMPLEMENT_CAPTURE_FAILED}: {error}"));
                return vec![];
            }
        };
        let header = self.implement_header(&snapshot.target);
        let msg = QueuedMessage {
            text: format!("{header}\n\n{}", snapshot.content),
            images: Vec::new(),
            mentions: Vec::new(),
            commits: Vec::new(),
            paste_ranges: Vec::new(),
        };
        let mut input = self.build_agent_input(&msg);
        input.mode = AgentMode::Build;
        let handoff = PlanHandoff {
            input,
            content: snapshot.content,
            source: snapshot.source,
            header,
            target: snapshot.target,
        };
        if clear_context {
            if let Err(error) = self.check_run_admission() {
                self.flash(error);
                return vec![];
            }
            self.answer_plan_escalation(ActionName::Implement);
            return vec![Action::ClearAndImplement(Box::new(handoff))];
        }
        if let Err(error) = self.admit_run() {
            self.flash(error);
            return vec![];
        }
        self.answer_plan_escalation(ActionName::Implement);
        self.finish_plan_handoff(handoff)
    }

    fn implement_header(&self, target: &PlanTarget) -> String {
        let header = match target {
            PlanTarget::Local(path) => format!("{IMPLEMENT_MSG_PREFIX} from `{}`.", path.display()),
            PlanTarget::Remote(_) => format!("{IMPLEMENT_VERB} {}.", plan::SESSION_PLAN_LABEL),
        };
        if self.plan_form.parallel() {
            format!("{header} {IMPLEMENT_PARALLEL_HINT}")
        } else {
            header
        }
    }

    /// For the session a Clear-and-Implement leaves behind: its plan goes
    /// with the work, so it keeps none.
    pub(crate) fn consume_plan(&mut self) {
        self.state.plan = PlanState::None;
        self.leave_plan();
    }

    /// Back to Build with the plan drafting again: the session keeps it, and
    /// a return to Plan revises the same document.
    fn leave_plan(&mut self) {
        self.plan_form.reset();
        self.autopilot_plan_escalation = None;
        self.state.plan.mark_drafting();
        self.state.mode = Mode::Build;
    }

    /// The session a Clear-and-Implement starts takes the plan over. A remote
    /// one is copied, and a failed copy still lets the run start: its message
    /// holds the plan.
    pub(crate) fn adopt_plan(&mut self, handoff: &PlanHandoff) {
        self.state.plan = match &handoff.target {
            PlanTarget::Local(path) => PlanState::Drafting(path.clone()),
            PlanTarget::Remote(_) => {
                match self.create_remote_plan(self.state.session.id, &handoff.content) {
                    Ok(reference) => PlanState::RemoteDrafting(reference),
                    Err(error) => {
                        self.flash(format!("{PLAN_COPY_FAILED}: {error}"));
                        PlanState::None
                    }
                }
            }
        };
    }

    /// A Clear-and-Implement builds the handoff before the session is
    /// replaced, so the input takes its plan from the session that runs it.
    pub(crate) fn finish_plan_handoff(&mut self, mut handoff: PlanHandoff) -> Vec<Action> {
        handoff.input.plan = self.state.plan.target();
        let actions = self.start_admitted_run(handoff.input, String::new(), StartedBy::User);
        self.leave_plan();
        self.main_chat()
            .push(DisplayMessage::plan(handoff.content, handoff.source));
        self.main_chat().show_user_message(handoff.header);
        actions
    }
}

/// The key `/usage` groups a session's spend under: always the full spec, so a
/// row names whoever served it even after the conversation moves to another
/// provider, and two providers serving the same model id stay apart. The modal
/// is what drops the prefix when it matches the session's own provider, so the
/// common case still reads as the model alone.
fn session_usage_model(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
}

pub(crate) fn no_favorites_msg() -> String {
    format!(
        "No favorite models yet: press {} on a model in /model",
        key::TOGGLE_FAVORITE.label
    )
}

fn is_goal_clear(args: &str) -> bool {
    args.eq_ignore_ascii_case(CLEAR_ARG)
        || GOAL_CLEAR_ALIASES
            .iter()
            .any(|clear| args.eq_ignore_ascii_case(clear))
}

/// The tab `/search` opens on, named by its first word, and the words to
/// search for.
fn search_tab(args: &str) -> (SearchTab, &str) {
    let (word, rest) = subcommand(args);
    match word.as_str() {
        CHAT_ARG => (SearchTab::ThisChat, rest),
        PROJECT_ARG => (SearchTab::Project, rest),
        ALL_ARG => (SearchTab::AllProjects, rest),
        _ => (SearchTab::Project, args.trim()),
    }
}

/// A command's first argument word, lowercased for matching, and the rest.
fn subcommand(args: &str) -> (String, &str) {
    let args = args.trim();
    let (word, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    (word.to_ascii_lowercase(), rest.trim())
}

/// Names a workflow agent the way the inspector does, by phase and label,
/// rather than by the opaque `<run>:<key>` task id a subtask prompt carries.
fn workflow_requester(workflow: &WorkflowProvenance, subagent: Option<&SubagentInfo>) -> String {
    let mut requester = String::from(WORKFLOW_REQUESTER_PREFIX);
    if let Some(phase) = &workflow.phase {
        requester.push_str(" · ");
        requester.push_str(phase);
    }
    if let Some(label) = subagent.map(|subagent| subagent.name.as_str()) {
        requester.push_str(" · ");
        requester.push_str(label);
    }
    requester
}

/// A chat's running spend, filed under whoever pays for it. Free of `&self` so
/// it composes with the borrows that reach a chat by index or through
/// `main_chat`.
fn add_chat_spend(chat: &mut Chat, cost: Option<f64>, billing: Billing) {
    match billing {
        Billing::Api => add_cost(&mut chat.cost, cost),
        Billing::Subscription => add_cost(&mut chat.subscription_cost, cost),
    }
}

fn is_streaming_stop_key(key: KeyEvent) -> bool {
    key::QUIT.matches(key) || key.code == KeyCode::Esc
}

fn is_shift_tab(key: KeyEvent) -> bool {
    key.code == KeyCode::BackTab
        || (key.code == KeyCode::Tab && key.modifiers == KeyModifiers::SHIFT)
}
