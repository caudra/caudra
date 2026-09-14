//! Elm-style `update(Msg) -> Vec<Action>`; side effects are dispatched by the caller.
//! Double-esc cancels/rewinds and double Ctrl+D exits within `flash_duration`.
//! `run_id` invalidates in-flight agent events. It bumps in exactly three
//! places, one per transition: `start_run`, `handle_cancel`, and
//! `AgentHandles::respawn`. Everything else only reads it.

mod btw;
mod delegation;
mod image_paste;
mod memory;
pub(crate) mod mode;
mod mouse;
mod queue;
mod session;
pub(crate) mod session_state;
pub(crate) mod shell;
mod stash;
pub(crate) mod tasks;
#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod view;
pub(crate) mod workflow;

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use caudra_workbench::{Layout as WorkbenchLayout, MutationGate, Workbench, WorkbenchAction};

use crate::AppSession;
use crate::agent::ModelSlot;
use crate::app::tasks::TaskOutcome;
use crate::chat::Chat;
use crate::chat::{CANCELLED_TEXT, ChatEventResult, DONE_TEXT, ERROR_TEXT};
use crate::clipboard::{ClipboardState, CopyResult};
use crate::components::btw_modal::BtwModal;
use crate::components::command::{CommandAction, CommandPalette, ParsedCommand};
use crate::components::command_modal::{CommandModal, CommandModalAction};
use crate::components::context_modal::ContextModal;
use crate::components::file_picker::{FilePickerModal, FilePickerModalAction};
use crate::components::goal_modal::GoalModal;
use crate::components::help_modal::HelpModal;
use crate::components::input::{AdmissionHit, InputAction, InputBox, Submission};
use crate::components::keybindings::{self, KeybindContext, key, leader};
use crate::components::login_picker::{LoginPicker, LoginPickerAction};
use crate::components::logs_modal::{LogsAction, LogsModal};
use crate::components::lua_float::FloatManager;
use crate::components::mcp_picker::{McpPicker, McpPickerAction};
use crate::components::memory_picker::MemoryPicker;
use crate::components::mention_popup::{MentionAction, MentionPopup};
use crate::components::message_actions::{MessageActionKind, MessageActions, MessageActionsAction};
use crate::components::messages::MessageActionTarget;
use crate::components::model_picker::{ModelPicker, ModelPickerAction};
use crate::components::paste_editor::{PasteEditor, PasteEditorAction, PasteEditorTarget};
use crate::components::permission_prompt::{PermissionDecision, PermissionPrompt};
use crate::components::permissions_picker::{PermissionsPicker, PermissionsPickerAction};
use crate::components::plan_form::{PlanForm, PlanFormAction};
use crate::components::prompt_profile_picker::{PromptProfilePicker, PromptProfilePickerAction};
use crate::components::question_form::{QuestionForm, QuestionFormAction};
use crate::components::queue_actions::{QueueActionKind, QueueActions, QueueActionsAction};
use crate::components::queue_panel::{QueueHit, QueueHitTarget};
use crate::components::review::{ReviewAction, ReviewModal};
use crate::components::rewind_picker::{RewindPicker, RewindPickerAction};
use crate::components::scrollbar;
use crate::components::search_modal::{SearchAction, SearchModal};
use crate::components::session_picker::{SessionPicker, SessionRow};
use crate::components::session_relocation::SessionRelocationPicker;
use crate::components::skills_modal::SkillsModal;
use crate::components::stash_picker::StashPicker;
use crate::components::status_bar::{StatusBar, StatusBarHit, StatusBarHitTarget};
use crate::components::storage_modal::{StorageFetchState, StorageModal};
use crate::components::task_picker::TaskPicker;
use crate::components::theme_picker::{ThemePicker, ThemePickerAction};
use crate::components::thinking_picker::{ThinkingPicker, ThinkingPickerAction};
use crate::components::todo_panel::TodoPanel;
use crate::components::tools_modal::ToolsModal;
use crate::components::usage_modal::{UsageFetchState, UsageModal, UsageScope};
use crate::components::which_key::WhichKey;
use crate::components::workbench::styles as workbench_styles;
use crate::components::workflow_catalog_picker::WorkflowCatalogPicker;
use crate::components::workflow_inspector::WorkflowInspector;
use crate::components::{
    Action, DisplayMessage, DisplayRole, DisplaySource, ExitRequest, Overlay, RetryInfo, Status,
    is_ctrl,
};
use crate::image;
use crate::input_document::InputDraft;
use crate::repaint::{Cadence, Dirty, Watch};
use crate::selection::{SelectionState, SelectionZone, ZoneRegistry};
use arc_swap::{ArcSwap, ArcSwapOption};
use caudra_agent::context::{ContextKey, ContextSnapshot, ContextStore};
use caudra_agent::mentions;
use caudra_agent::permissions::{
    PermissionAnswer, PermissionManager, PermissionPolicyError, RevokedRuleScope,
};
use caudra_agent::prompt::profile::PromptProfileCatalog;
use caudra_agent::snapshots::{
    SESSION_SNAPSHOTS_DIR, SnapshotError, SnapshotLimits, SnapshotStore, workspace_key,
};
use caudra_agent::workspace_baseline::WorkspaceBaseline;
use caudra_agent::{
    AgentEvent, AgentInput, AgentMode, Envelope, GoalVerdict, ImageSource, McpConfigErrors,
    McpPromptInfo, McpSnapshotReader, Mention, PromptAdmission, QueueItemId, SharedHistory,
    SteeringQueue, SubagentInfo,
};
use caudra_config::{ModelPolicy, PermissionsConfig, SnapshotsConfig, UiConfig};
use caudra_lua::{
    BuiltinAction, EventHandle, HintReader, HintSnapshot, KeymapReader, LuaCommandReader, WinView,
};
use caudra_providers::{
    Billing, ContentBlock, Message, Model, ModelPurpose, ResolvedThinking, ThinkingConfig,
    TokenUsage, add_cost,
};
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::input_history::InputHistory;
use caudra_storage::model::persist_model;
use caudra_storage::usage_ledger::{LedgerPurpose, LifetimeUsage, TurnUsage, UsageLedger};
use caudra_storage::view::ViewMode;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};

use crate::storage_writer::StorageWriter;
use ratatui::layout::{Position, Rect};

pub(crate) use crate::agent::QueuedMessage;
pub use crate::components::RestoreMode;
pub(crate) use mode::{Mode, PlanState, PlanTrigger};
use mouse::Autoscroll;
#[cfg(test)]
use mouse::EDGE_SCROLL_LINES;
pub(crate) use queue::{MessageQueue, SubmitOutcome};
use session::{MergedHistory, Sent};
pub(crate) use session::{
    REVERT_BUSY_MSG, reachable_subagent_ids, recover_pending_workspace_restore, session_has_content,
};
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
const FLASH_NO_CHORD: &str = "is not a chord";
const CONTEXT_USAGE: &str = "Usage: /context [all]";
const STORAGE_USAGE: &str = "Usage: /storage [all]";
const TOOLS_USAGE: &str = "Usage: /tools";
const SKILLS_USAGE: &str = "Usage: /skills";
const AUTH_EXPIRED_MSG: &str = "Authentication failed. Run `caudra auth login` in another terminal; Caudra will resume automatically, or press Enter to retry now.";
const COPY_FAILED: &str = "Copy failed: ";
const FLASH_NO_PLAN: &str = "No plan file";
const NO_FILE_REVERT_MSG: &str = "No file revert for this workspace";
const NO_FILE_CHANGES_MSG: &str =
    "Nothing has written to this workspace, so there are no file changes to revert";
const FAST_UNSUPPORTED_MSG: &str = "Fast mode requires an Anthropic Opus 4.6+ model (API only)";
const THINKING_UNSUPPORTED_MSG: &str = "Thinking requires a model that supports it";
const FAST_ON_MSG: &str = "Fast mode: on";
const FAST_OFF_MSG: &str = "Fast mode: off";
const AUTO_VIEW_MSG: &str = "View: auto (the newest card stays open)";
const COMPACT_VIEW_MSG: &str = "View: compact";
const EXPANDED_VIEW_MSG: &str = "View: expanded";
const STEER_NOT_CONSUMED_MSG: &str = "Task finished before it consumed the message";
const REVIEW_READY_MSG: &str = "Review notes added to the prompt";
const REVIEW_UNAVAILABLE_MSG: &str = "Nothing to review here yet";
const SHELL_PASTE_EXPANDED_MSG: &str = "Expanded pasted text; press Enter again to run it";
const IMPLEMENT_MSG_PREFIX: &str = "Implement the plan";
const IMPLEMENT_PARALLEL_HINT: &str = "Use batch+task to parallelize, assign each subagent a separate module and restrict its tests to that module to avoid interference.";
const PERMISSION_BLOCKER: &str = "Permission requested";
const AUTH_BLOCKER: &str = "Authentication required";
const PLAN_BLOCKER: &str = "Plan ready";
const QUESTION_BLOCKER: &str = "Question requested";
/// Never valid JSON, so the tool reads it as the dismissal it is.
const QUESTION_DISMISSED: &str = "dismissed";
const LOGIN_BLOCKER: &str = "Provider login required";
const MCP_TRUST_BLOCKER: &str = "MCP trust required";
const PROJECT_PERMISSION_CONFIG_TRUST_BLOCKER: &str = "Project permission config trust required";

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
    TurnComplete { response: Option<String> },
    PermissionRequested { tool: Option<String> },
    AuthenticationRequired,
    QuestionRequested,
    PlanReady,
}

impl Notification {
    /// Prompts blocking the agent outrank turn completions.
    pub(crate) fn is_urgent(&self) -> bool {
        !matches!(self, Self::TurnComplete { .. })
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::TurnComplete { response } => response
                .clone()
                .unwrap_or_else(|| "Agent turn complete".into()),
            Self::PermissionRequested { tool: Some(tool) } => {
                format!("Permission requested: {tool}")
            }
            Self::PermissionRequested { tool: None } => "Permission requested".into(),
            Self::AuthenticationRequired => "Authentication required".into(),
            Self::QuestionRequested => "Question requested".into(),
            Self::PlanReady => "Plan ready".into(),
        }
    }

    pub(crate) fn error_completion() -> Self {
        Self::TurnComplete {
            response: Some("Agent stopped with an error".into()),
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
    Paste(String),
    Mouse(MouseEvent),
    Scroll { column: u16, row: u16, delta: i32 },
    Agent(Box<Envelope>),
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

pub struct App {
    pub(super) chats: Vec<Chat>,
    pub(super) active_chat: usize,
    pub(super) chat_index: HashMap<String, usize>,
    pub(crate) input_box: InputBox,
    subagent_input_box: InputBox,
    subagent_input_task: Option<String>,
    subagent_drafts: HashMap<String, InputDraft>,
    pub(super) command_palette: CommandPalette,
    pub(crate) no_commands: bool,
    pub(super) command_modal: CommandModal,
    pub(super) theme_picker: ThemePicker,
    pub(super) thinking_picker: ThinkingPicker,
    pub(super) prompt_profile_picker: PromptProfilePicker,
    pub(super) model_picker: ModelPicker,
    pub(super) login_picker: LoginPicker,
    pub(super) mcp_picker: McpPicker,
    pub(super) rewind_picker: RewindPicker,
    pub(super) message_actions: MessageActions,
    pub(super) queue_actions: QueueActions,
    pub(super) review: ReviewModal,
    pub(super) help_modal: HelpModal,
    pub(super) which_key: WhichKey,
    pub(super) usage_modal: UsageModal,
    pub(super) context_modal: ContextModal,
    pub(super) logs_modal: LogsModal,
    pub(super) tools_modal: ToolsModal,
    pub(super) skills_modal: SkillsModal,
    pub(super) storage_modal: StorageModal,
    context_snapshot: Watch<ContextSnapshot>,
    /// Read from the ledger when the modal asks for it, not on every frame:
    /// the table outlives sessions and only grows.
    pub(super) lifetime_usage: Option<LifetimeUsage>,
    pub(super) goal_modal: GoalModal,
    pub(super) btw_modal: BtwModal,
    pub(super) float_mgr: FloatManager,
    pub(super) search_modal: SearchModal,
    pub(super) file_picker: FilePickerModal,
    pub(super) mention_popup: MentionPopup,
    pub(super) paste_editor: PasteEditor,
    pub(super) permission_prompt: PermissionPrompt,
    pub(super) permissions_picker: PermissionsPicker,
    permission_config_trust_deferred: bool,
    pub(super) memory_picker: MemoryPicker,
    pub(super) task_picker: TaskPicker,
    pub(super) workflow_inspector: WorkflowInspector,
    /// The run a transcript was opened from, so reopening the inspector
    /// returns to it rather than to whichever run is newest.
    pub(super) workflow_return: Option<String>,
    pub(super) workflow_catalog_picker: WorkflowCatalogPicker,
    pub(crate) workflow: workflow::WorkflowUi,
    pub(super) question_form: QuestionForm,
    pub(super) session_picker: SessionPicker,
    pub(super) session_relocation_picker: SessionRelocationPicker,
    /// Published by the event loop, which is the only thing that can see
    /// sibling sessions. Polled while the picker is open.
    pub(crate) live_sessions: Arc<ArcSwap<Vec<SessionRow>>>,
    live_session_watch: Watch<Vec<SessionRow>>,
    /// The picker's other half comes from a disk query, which no publisher
    /// covers; see [`StorageWriter::generation`].
    stored_session_generation: u64,
    question_subagent: Option<String>,
    pub(super) stash_picker: StashPicker,
    pub(super) plan_form: PlanForm,
    pub(super) todo_panel: TodoPanel,
    pub(crate) workbench: Workbench,
    pub(super) workbench_theme_gen: u64,
    /// The layout last read or written, so a tick only reaches storage when
    /// something actually moved. `None` until the workbench first opens.
    pub(super) workbench_layout: Option<WorkbenchLayout>,
    pub(super) status_bar: StatusBar,
    pub(super) status_hits: Vec<StatusBarHit>,
    pub(super) status_mouse_down: Option<StatusBarHit>,
    pub(super) status_hover: Option<StatusBarHitTarget>,
    pub(super) queue_hits: Vec<QueueHit>,
    pub(super) queue_mouse_down: Option<QueueHit>,
    pub(super) queue_hover: Option<QueueHitTarget>,
    pub(super) admission_hits: Vec<AdmissionHit>,
    pub(super) admission_mouse_down: Option<AdmissionHit>,
    pub(super) admission_hover: Option<PromptAdmission>,
    /// Zero-sized whenever the task hint is not on screen, so the pointer
    /// cannot land on a hint that a higher-priority one replaced.
    pub(super) task_hint_hit: Rect,
    pub(super) task_hint_mouse_down: bool,
    pub(super) task_hint_hover: bool,
    pub(super) message_action_mouse_down: Option<MessageActionTarget>,
    pub(super) link_mouse_down: Option<Arc<str>>,
    pub(super) mention_mouse_down: Option<Mention>,
    pub(super) key_focus: KeyFocus,
    pub status: Status,
    pub(crate) state: session_state::SessionState,
    pub exit_request: ExitRequest,
    pub(crate) exit_on_done: bool,
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
    pub(super) retry_info: Option<RetryInfo>,
    goal_deferred: bool,
    pub(super) zones: ZoneRegistry,
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
    pub(crate) snapshot_store: Arc<SnapshotStore>,
    /// Shared with this session's agents, which is what lets the first mutating
    /// tool call capture the revert point the UI later restores from.
    pub(crate) workspace_baseline: Arc<WorkspaceBaseline>,
    /// Mirrors the baseline's refusal, so the poller can tell a new verdict
    /// from the one it already reported.
    pub(crate) snapshots_unavailable: Option<String>,
    /// Set by the event loop after construction, like `live_sessions`: every
    /// store this session opens later has to agree with the one it started with.
    pub(crate) snapshots_config: SnapshotsConfig,
    pub(crate) usage_slot: Arc<ArcSwapOption<UsageFetchState>>,
    pub(crate) storage_slot: Arc<ArcSwapOption<StorageFetchState>>,
    pub(crate) shared_history: Option<SharedHistory>,
    pub(crate) btw_prompt: Option<crate::agent::SharedBtwPrompt>,
    pub(crate) context_store: Option<ContextStore>,
    pub(crate) effective_model_slot: Option<Arc<ArcSwap<ModelSlot>>>,
    pub(crate) image_paste_rx: Vec<flume::Receiver<Result<ImageSource, String>>>,
    storage_writer: Arc<StorageWriter>,
    last_sent: Option<Sent>,
    merged_history: Option<MergedHistory>,
    pub(crate) shell: shell::ShellState,
    pub(crate) ui_config: UiConfig,
    pub(crate) permissions: Arc<PermissionManager>,
    pub(crate) model_policy: Arc<ModelPolicy>,
    pub(crate) lua_event_handle: EventHandle,
    pub(super) keymap_reader: KeymapReader,
    pub(super) hint_reader: HintReader,
    hints: Watch<HintSnapshot>,
    pub(crate) restore_event_tx: Option<caudra_agent::EventSender>,
    pub(super) restoring: Arc<AtomicBool>,
    remote_restore_confirmation: Option<session::RemoteRestoreConfirmation>,
    subagent_answers: HashMap<String, flume::Sender<String>>,
    subagent_steers: HashMap<String, SteeringQueue>,
    pending_subagent_steers: HashMap<String, VecDeque<PendingSteer>>,
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
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: &Model,
        session: AppSession,
        storage: StateDir,
        snapshot_store: Arc<SnapshotStore>,
        workspace_baseline: Arc<WorkspaceBaseline>,
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
        permissions: Arc<PermissionManager>,
        custom_commands: Arc<[caudra_agent::command::CustomCommand]>,
        lua_event_handle: EventHandle,
        model_policy: Arc<ModelPolicy>,
        prompt_profiles: Arc<PromptProfileCatalog>,
        workspace_session: Option<caudra_workspace::WorkspaceSession>,
    ) -> Self {
        scrollbar::set_enabled(ui_config.scrollbar);
        let state = SessionState::from_session(session, model, &storage, &model_policy);
        let view = caudra_storage::view::read(&storage).unwrap_or_default();
        let typewriter = ui_config.typewriter_ms_per_char;
        let flash = ui_config.flash_duration();
        let status_bar = StatusBar::new(flash, &state.session.cwd, workspace_session.is_some());
        let input_box = InputBox::new(
            InputHistory::load(&storage, input_history_size),
            ui_config.max_input_lines,
        );
        let subagent_input_box = InputBox::new(InputHistory::default(), ui_config.max_input_lines);
        let mut app = Self {
            chats: vec![Chat::new(
                "Main".into(),
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
            ),
            no_commands: false,
            command_modal: CommandModal::new(),
            theme_picker: ThemePicker::new(),
            thinking_picker: ThinkingPicker::new(),
            prompt_profile_picker: PromptProfilePicker::new(Arc::clone(&prompt_profiles)),
            model_picker: ModelPicker::new(available_models),
            login_picker: LoginPicker::new(),
            mcp_picker: McpPicker::new(mcp_reader, mcp_config_errors),
            rewind_picker: RewindPicker::new(),
            message_actions: MessageActions::new(),
            queue_actions: QueueActions::new(),
            review: ReviewModal::new(),
            help_modal: HelpModal::new(),
            which_key: WhichKey::new(ui_config.which_key_delay()),
            usage_modal: UsageModal::new(),
            context_modal: ContextModal::new(),
            logs_modal: LogsModal::new(max_log_files),
            tools_modal: ToolsModal::new(),
            skills_modal: SkillsModal::new(),
            storage_modal: StorageModal::new(),
            context_snapshot: Watch::default(),
            lifetime_usage: None,
            goal_modal: GoalModal::default(),
            btw_modal: BtwModal::new(typewriter),
            float_mgr: FloatManager::new(),
            search_modal: SearchModal::new(),
            file_picker: FilePickerModal::new(),
            mention_popup: MentionPopup::new(),
            paste_editor: PasteEditor::new(),
            permission_prompt: PermissionPrompt::new(),
            permissions_picker: PermissionsPicker::new(),
            permission_config_trust_deferred: false,
            memory_picker: MemoryPicker::new(),
            task_picker: TaskPicker::new(),
            workflow_inspector: WorkflowInspector::new(),
            workflow_return: None,
            workflow_catalog_picker: WorkflowCatalogPicker::new(),
            workflow: workflow::WorkflowUi::new(),
            question_form: QuestionForm::new(),
            session_picker: SessionPicker::new(),
            session_relocation_picker: SessionRelocationPicker::new(),
            live_sessions: Arc::default(),
            live_session_watch: Watch::default(),
            stored_session_generation: 0,
            question_subagent: None,
            stash_picker: StashPicker::new(),
            plan_form: PlanForm::new(),
            todo_panel: TodoPanel::default(),
            workbench: Workbench::new(workbench_styles()),
            workbench_theme_gen: crate::theme::generation(),
            workbench_layout: None,
            status_bar,
            status_hits: Vec::new(),
            status_mouse_down: None,
            status_hover: None,
            queue_hits: Vec::new(),
            queue_mouse_down: None,
            queue_hover: None,
            admission_hits: Vec::new(),
            admission_mouse_down: None,
            admission_hover: None,
            task_hint_hit: Rect::ZERO,
            task_hint_mouse_down: false,
            task_hint_hover: false,
            message_action_mouse_down: None,
            link_mouse_down: None,
            mention_mouse_down: None,
            key_focus: KeyFocus::Composer,
            status: Status::Idle,
            state,
            exit_request: ExitRequest::None,
            exit_on_done: false,
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
            retry_info: None,
            goal_deferred: false,
            zones: ZoneRegistry::new(),
            selection_state: None,
            autoscroll: None,
            clipboard: ClipboardState::new(),
            last_esc: None,
            last_exit: None,
            storage,
            workspace_session: workspace_session.clone(),
            remote_project_context: None,
            local_documents: None,
            snapshot_store,
            workspace_baseline,
            snapshots_unavailable: None,
            snapshots_config: SnapshotsConfig::default(),
            usage_slot: Arc::new(ArcSwapOption::empty()),
            storage_slot: Arc::new(ArcSwapOption::empty()),
            shared_history: None,
            btw_prompt: None,
            context_store: None,
            effective_model_slot: None,
            image_paste_rx: vec![],
            storage_writer,
            last_sent: None,
            merged_history: None,
            shell: shell::ShellState::default(),
            ui_config,
            permissions,
            model_policy: Arc::clone(&model_policy),
            lua_event_handle,
            hints: Watch::seeded(hint_reader.load_full()),
            keymap_reader,
            hint_reader,
            restore_event_tx: None,
            restoring: Arc::new(AtomicBool::new(false)),
            remote_restore_confirmation: None,
            subagent_answers: HashMap::new(),
            subagent_steers: HashMap::new(),
            pending_subagent_steers: HashMap::new(),
            unsent_subagent_steers: HashMap::new(),
            parent_task_ids: HashMap::new(),
            pending_delegations: HashSet::new(),
            view,
        };
        app.model_picker.set_recents(
            caudra_storage::model::read_recents(&app.storage)
                .into_iter()
                .filter(|spec| model_policy.allows(spec))
                .collect(),
        );
        app.chats[0].context_window = app.state.model.context_window;
        app.sync_composer_cwd();
        if let Some(workspace) = workspace_session
            && let Err(error) = app.workbench.bind_workspace_with_gate(
                workspace,
                remote_workbench_gate(Arc::clone(&app.workspace_baseline)),
            )
        {
            tracing::warn!(%error, "remote workbench backend initialization failed");
        }
        app
    }

    pub(crate) fn snapshot_store_for(
        storage: &StateDir,
        session_id: caudra_storage::id::CaudraId,
        cwd: &std::path::Path,
        limits: SnapshotLimits,
    ) -> Result<Arc<SnapshotStore>, SnapshotError> {
        Ok(Arc::new(
            SnapshotStore::new_managed(
                storage.clone(),
                Self::snapshot_store_path(storage, session_id, cwd)?,
            )
            .with_limits(limits),
        ))
    }

    fn snapshot_store_path(
        storage: &StateDir,
        session_id: caudra_storage::id::CaudraId,
        cwd: &std::path::Path,
    ) -> Result<PathBuf, SnapshotError> {
        Ok(storage
            .path()
            .join(SESSION_SNAPSHOTS_DIR)
            .join(session_id.to_string())
            .join(workspace_key(cwd)?))
    }

    /// The store and the baseline move together: an agent mid-session must never
    /// capture one workspace's revert point into another workspace's store.
    fn rebind_workspace_baseline(&mut self, store: Arc<SnapshotStore>, cwd: PathBuf) {
        self.workspace_baseline.rebind(Arc::clone(&store), cwd);
        self.snapshot_store = store;
    }

    pub(super) fn discard_workspace_unrevert(&self) -> Result<(), SnapshotError> {
        self.snapshot_store.discard_unrevert()
    }

    pub(super) fn history_head(&self) -> Option<CaudraId> {
        self.shared_history
            .as_ref()
            .and_then(|history| history.load().messages.last().map(|item| item.id))
            .or_else(|| crate::session_history_head(&self.state.session))
    }

    /// Closes the bracket a revert needs: the baseline is one end, `head` the
    /// other. Inline, and it walks and hashes the whole working tree under a
    /// machine-global lock.
    ///
    /// A session with no baseline has nothing to revert to, so it is not made
    /// to pay for a walk it will never spend.
    pub(super) fn snapshot_history_head(&mut self) -> Result<(), String> {
        let Some(head) = self.history_head().filter(|_| self.has_revert_point()) else {
            return Ok(());
        };
        if self.workspace_baseline.is_remote() {
            let already_captured = self
                .workspace_baseline
                .remote_capture(Some(head))
                .is_ok_and(|capture| capture.is_some());
            return match smol::block_on(self.workspace_baseline.ensure(Some(head))) {
                caudra_agent::BaselineOutcome::Ready => {
                    if !already_captured {
                        self.status_bar
                            .flash("Remote workspace snapshot is available".into());
                    }
                    Ok(())
                }
                caudra_agent::BaselineOutcome::Unavailable(reason) => Err(reason.to_string()),
                caudra_agent::BaselineOutcome::Failed(error) => Err(error.to_string()),
            };
        }
        self.snapshot_store
            .snapshot(std::path::Path::new(&self.state.session.cwd), head)
            .map(drop)
            .map_err(|error| error.to_string())
    }

    /// Whether a run has captured a baseline for this workspace yet. Until one
    /// exists there is nothing for a later capture to bracket.
    pub(super) fn has_revert_point(&self) -> bool {
        if self.workspace_baseline.is_remote() {
            self.workspace_baseline
                .remote_capture(self.history_head())
                .is_ok_and(|capture| capture.is_some())
                || self
                    .workspace_baseline
                    .remote_capture(None)
                    .is_ok_and(|capture| capture.is_some())
        } else {
            self.snapshot_store.has_session_start()
        }
    }

    /// Why a file revert cannot run, in the user's terms: either this workspace
    /// is one Caudra will not snapshot, or nothing has written to it yet.
    pub(super) fn file_revert_blocker(&self) -> Option<String> {
        if self.has_revert_point() {
            return None;
        }
        Some(match self.workspace_baseline.unavailable_reason() {
            Some(reason) => format!("{NO_FILE_REVERT_MSG}: {reason}"),
            None => NO_FILE_CHANGES_MSG.to_owned(),
        })
    }

    /// [`Self::snapshot_history_head`] with a ceiling on how long it may wait
    /// for the machine-global artifact lock. Answers whether it ran.
    ///
    /// Exit is the one caller that cannot afford the unbounded form: the lock
    /// is one file for every caudra on the machine, so a capture running in an
    /// unrelated workspace is enough to hold exit open, and `flock` will
    /// happily starve the waiter. Losing the final snapshot costs rewind
    /// fidelity for one head; waiting costs the user their terminal, and
    /// blocks every other session queued behind the same lock.
    pub(super) fn snapshot_history_head_within(
        &mut self,
        budget: Duration,
    ) -> Result<bool, String> {
        if self.workspace_baseline.is_remote() {
            return self.snapshot_history_head().map(|()| true);
        }
        self.snapshot_store
            .capture_head_within(
                std::path::Path::new(&self.state.session.cwd),
                self.history_head(),
                budget,
            )
            .map_err(|error| error.to_string())
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
        if let Some(model_slot) = self.status_main_model() {
            return (snapshot.model.spec == model_slot.model.spec()
                && snapshot.window.tokens == model_slot.model.context_window)
                .then_some(snapshot);
        }
        match self.state.applied_mode {
            Mode::Build => (snapshot.model.spec == self.state.model.spec()
                && snapshot.window.tokens == self.state.model.context_window)
                .then_some(snapshot),
            Mode::Plan => {
                let expected = Model::resolve_if_available(
                    ModelPurpose::Plan,
                    &self.state.model,
                    &self.model_policy,
                )
                .ok()?;
                (snapshot.model.spec == expected.spec()
                    && snapshot.window.tokens == expected.context_window)
                    .then_some(snapshot)
            }
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
        let expected =
            Model::resolve_if_available(ModelPurpose::Plan, &self.state.model, &self.model_policy)
                .ok()?;
        self.effective_model_slot
            .as_ref()
            .map(|slot| slot.load_full())
            .filter(|slot| {
                slot.model.spec() == expected.spec()
                    && snapshot.model.spec == slot.model.spec()
                    && snapshot.window.tokens == slot.model.context_window
            })
    }

    fn active_subagent_can_steer(&self) -> bool {
        self.active_subagent_id()
            .is_some_and(|id| self.subagent_steers.contains_key(id))
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

    /// The palette follows whichever composer is on screen, and learns from
    /// the same call whether a task owns it, so the dropdown can never style
    /// itself for a chat it is no longer attached to.
    fn sync_command_palette(&mut self, text: &str) {
        let task_focused = !self.is_main_chat();
        self.command_palette.sync(text, task_focused);
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
        self.mention_popup
            .sync_workspace(&text, cursor, &cwd, self.workspace_session.clone());
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

    fn sync_subagent_input_target(&mut self) {
        let next = self.active_subagent_id().map(str::to_owned);
        if self.subagent_input_task == next {
            return;
        }
        self.paste_editor.close();
        if let Some(previous) = self.subagent_input_task.take() {
            let draft = self.subagent_input_box.draft();
            if draft.is_empty() {
                self.subagent_drafts.remove(&previous);
            } else {
                self.subagent_drafts.insert(previous, draft);
            }
        }
        let draft = next
            .as_ref()
            .and_then(|id| self.subagent_drafts.remove(id))
            .unwrap_or_default();
        self.subagent_input_box.set_draft(draft);
        self.subagent_input_task = next;
        // Runs on every chat switch, so this is the single point that stops a
        // dropdown opened over one composer from surviving into another.
        self.resync_dropdowns();
    }

    fn plan_form_active(&self) -> bool {
        self.state.mode == Mode::Plan && self.plan_form.is_visible()
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

    /// The one place a chosen level lands, so every spelling of the choice
    /// reaches the next session. A level the model forces off goes through
    /// [`SessionState::update_model`] instead, which must not overwrite what
    /// the user asked for on a model that could honor it.
    fn apply_thinking(&mut self, thinking: ThinkingConfig) {
        self.state.thinking = thinking;
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
        self.state.fast = fast;
        Ok(())
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
        let recents = caudra_storage::model::push_recent(&self.storage, spec)
            .into_iter()
            .filter(|spec| self.model_policy.allows(spec))
            .collect();
        self.model_picker.set_recents(recents);
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
        match msg {
            Msg::Key(key) => {
                self.autoscroll = None;
                // Any key drops the reasoning preview. The cycle re-flashes it
                // a moment later, so only the keys that did not ask for it lose
                // it.
                self.thinking_picker.clear_flash();
                self.handle_key(key)
            }
            Msg::Paste(text) => {
                self.sync_subagent_input_target();
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                if self.session_relocation_picker.is_open() {
                    self.route_text_paste(&text);
                    return vec![];
                }
                if self.workbench.is_open() && self.workbench.paste(&text) {
                    return vec![];
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
            Msg::Mouse(event) => self.handle_mouse(event),
            Msg::Scroll { column, row, delta } => {
                self.autoscroll = None;
                self.handle_scroll(column, row, delta);
                vec![]
            }
            Msg::Agent(envelope) => self.handle_agent_event(*envelope),
        }
    }

    /// The form's reply goes back through the answer channel the asking tool
    /// is parked on. A dismissal is an unparseable reply by design: the tool
    /// reads anything it cannot make answers of as "the user declined".
    fn handle_question_form_action(&mut self, action: QuestionFormAction) -> Vec<Action> {
        let reply = match action {
            QuestionFormAction::Consumed => return Vec::new(),
            // Nothing is sent back: the parked tool ends on its own cancel
            // path, and the run only moves again when the user says so.
            QuestionFormAction::Cancel => {
                self.question_form.close();
                return match self.question_subagent.take() {
                    Some(task_id) => self.cancel_subagent(task_id),
                    None => self.handle_cancel(),
                };
            }
            QuestionFormAction::Dismiss => QUESTION_DISMISSED.to_owned(),
            QuestionFormAction::Submit(answers) => {
                serde_json::to_string(&answers).unwrap_or_else(|_| QUESTION_DISMISSED.to_owned())
            }
        };
        self.question_form.close();
        let subagent = self.question_subagent.take();
        self.send_to_agent(subagent.as_deref(), reply);
        Vec::new()
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
        } else {
            self.send_answer(answer);
        }
    }

    /// A decision only clears the prompt once it is recorded. Transient
    /// answers have nothing to record, and a request that is no longer pending
    /// was answered elsewhere, so both clear it too.
    pub(crate) fn apply_permission_decision(&mut self, decision: PermissionDecision) {
        let transient = matches!(
            decision.answer,
            PermissionAnswer::AllowOnce
                | PermissionAnswer::Deny
                | PermissionAnswer::DenyWithGuidance(_)
        );
        if self
            .permissions
            .answer(&decision.request_id, decision.answer)
            || transient
            || self
                .permissions
                .pending_request(&decision.request_id)
                .is_none()
        {
            self.permission_prompt.resolve(&decision.request_id);
        } else {
            self.status_bar
                .flash("Could not save permission decision".into());
        }
    }

    fn scroll_at(&mut self, column: u16, row: u16, delta: i32) -> Option<SelectionZone> {
        let pos = Position::new(column, row);
        // Docked forms are not modal, so they claim the wheel only where they
        // drew: everywhere else it belongs to the transcript behind them.
        if self.permission_prompt.is_open() && self.permission_prompt.contains(pos) {
            self.permission_prompt.scroll(delta);
            return None;
        }
        if self.question_form.is_open() && self.question_form.contains(pos) {
            self.question_form.scroll(delta);
            return None;
        }
        if self.btw_modal.is_open() {
            self.btw_modal.scroll(delta);
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
        if self.context_modal.is_open() {
            self.context_modal.scroll(delta);
            return None;
        }
        if self.tools_modal.is_open() {
            self.tools_modal.scroll(delta);
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
        if self.goal_modal.is_open() {
            self.goal_modal.scroll(delta);
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
        try_picker!(self.search_modal);
        try_picker!(self.theme_picker);
        try_picker!(self.mcp_picker);
        try_picker!(self.login_picker);
        try_picker!(self.rewind_picker);
        try_picker!(self.message_actions);
        try_picker!(self.queue_actions);
        try_picker!(self.review);
        try_picker!(self.model_picker);
        try_picker!(self.prompt_profile_picker);
        try_picker!(self.thinking_picker);
        try_picker!(self.file_picker);
        try_picker!(self.permissions_picker);
        try_picker!(self.stash_picker);
        try_picker!(self.memory_picker);
        // Not `try_picker!`: the wheel walks the run list or scrolls the
        // section depending on which pane it is over.
        if self.workflow_inspector.is_open() {
            let action = self.workflow_inspector.scroll_at(pos, delta);
            self.handle_workflow_inspector_action(action);
            return None;
        }
        try_picker!(self.workflow_catalog_picker);
        // Not `try_picker!`: scrolling the task list previews the task behind
        // the float, and only the app can carry that out.
        if self.task_picker.is_open() {
            if self.task_picker.contains(pos) {
                let action = self.task_picker.scroll(delta);
                self.handle_task_picker_action(action);
            }
            return None;
        }
        try_picker!(self.session_picker);
        try_picker!(self.session_relocation_picker);
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
            self.command_palette.close();
            if !self.is_main_chat()
                && self.active_subagent_can_steer()
                && !self.subagent_input_box.is_empty()
            {
                self.subagent_input_box.discard();
                return Some(vec![]);
            }
            return Some(if !self.is_main_chat() || self.input_box.is_empty() {
                if self.status == Status::Streaming {
                    return Some(self.handle_cancel());
                }
                self.quit()
            } else {
                self.input_box.discard();
                vec![]
            });
        }
        if key::EXIT.matches(key) {
            let input_empty = if self.is_main_chat() {
                self.input_box.is_empty()
            } else {
                !self.active_subagent_can_steer() || self.subagent_input_box.is_empty()
            };
            if self.status != Status::Idle || !input_empty {
                self.last_exit = None;
                return Some(vec![]);
            }
            return Some(
                if let Some(pressed_at) = self.last_exit.take()
                    && pressed_at.elapsed() < self.status_bar.flash_duration
                {
                    self.quit()
                } else {
                    self.last_exit = Some(Instant::now());
                    self.status_bar.flash(FLASH_EXIT.into());
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
        } else if key::SCROLL_BOTTOM.matches(key) {
            self.active_chat().enable_auto_scroll();
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
        !self.plan_form_active()
            && !self.question_form.is_open()
            && (self.is_main_chat()
                || self.active_subagent_can_steer()
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

    fn dispatch_overlay(&mut self, key: KeyEvent) -> Option<Vec<Action>> {
        if self.paste_editor.is_open() {
            match self.paste_editor.handle_key(key) {
                PasteEditorAction::Consumed => {}
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

        if self.permission_prompt.is_open() {
            if let Some(decision) = self.permission_prompt.handle_key(key) {
                self.apply_permission_decision(decision);
            }
            return Some(vec![]);
        }

        // plan_form is non-modal: Passthrough falls through to the rest of dispatch
        if self.plan_form_active() {
            let action = self.plan_form.handle_key(key);
            if action != PlanFormAction::Passthrough {
                return Some(self.handle_plan_form_action(action));
            }
        }

        if self.help_modal.is_open() {
            self.help_modal.handle_key(key);
            return Some(vec![]);
        }

        if self.usage_modal.is_open() {
            if key::REFRESH.matches(key) {
                self.lifetime_usage = None;
                self.load_lifetime_usage();
                return Some(vec![Action::RefreshUsage]);
            }
            self.usage_modal.handle_key(key);
            self.load_lifetime_usage();
            return Some(vec![]);
        }

        if self.logs_modal.is_open() {
            let action = self.logs_modal.handle_key(key);
            self.handle_logs_action(action);
            return Some(vec![]);
        }

        if self.context_modal.is_open() {
            self.context_modal.handle_key(key);
            return Some(vec![]);
        }

        if self.tools_modal.is_open() {
            self.tools_modal.handle_key(key);
            return Some(vec![]);
        }

        if self.skills_modal.is_open() {
            self.skills_modal.handle_key(key);
            return Some(vec![]);
        }

        if self.storage_modal.is_open() {
            self.storage_modal.handle_key(key);
            return Some(vec![]);
        }

        if self.goal_modal.is_open() {
            if let Some(limit) = self
                .goal_modal
                .handle_key(key, self.state.goal.continuation_limit())
            {
                self.state.goal.set_continuation_limit(limit);
            }
            return Some(vec![]);
        }

        if self.btw_modal.is_open() {
            self.btw_modal.handle_key(key);
            return Some(vec![]);
        }

        if self.float_mgr.handle_key(key) {
            return Some(vec![]);
        }

        if self.search_modal.is_open() {
            let action = self.search_modal.handle_key(key);
            return Some(self.handle_search_action(action));
        }

        if self.file_picker.is_open() {
            let action = self.file_picker.handle_key(key);
            return Some(self.handle_file_picker_action(action));
        }

        if self.queue_editor_active() {
            return Some(self.handle_queue_editor_key(key));
        }

        // Ahead of the focused queue itself: the menu is drawn over the panel
        // it acts on, so it answers first.
        if self.queue_actions.is_open() {
            let action = self.queue_actions.handle_key(key);
            return Some(self.handle_queue_actions_action(action));
        }

        if self.active_queue_is_focused() {
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
            let action = self.rewind_picker.handle_key(key);
            return Some(self.handle_rewind_picker_action(action));
        }

        if self.message_actions.is_open() {
            let action = self.message_actions.handle_key(key);
            return Some(self.handle_message_actions_action(action));
        }

        if self.review.is_open() {
            let action = self.review.handle_key(key);
            return Some(self.handle_review_action(action));
        }

        if self.command_modal.is_open() {
            let action = self.command_modal.handle_key(key);
            return Some(self.handle_command_modal_action(action));
        }

        if self.theme_picker.is_open() {
            let action = self.theme_picker.handle_key(key);
            return Some(self.handle_theme_picker_action(action));
        }

        if self.prompt_profile_picker.is_open() {
            let action = self.prompt_profile_picker.handle_key(key);
            return Some(self.handle_prompt_profile_picker_action(action));
        }

        if self.thinking_picker.is_open() {
            let action = self.thinking_picker.handle_key(key);
            return Some(self.handle_thinking_picker_action(action));
        }

        if self.model_picker.is_open() {
            let action = self.model_picker.handle_key(key);
            return Some(self.handle_model_picker_action(action));
        }

        if self.login_picker.is_open() {
            let action = self.login_picker.handle_key(key);
            return Some(self.handle_login_picker_action(action));
        }

        if self.mcp_picker.is_open() {
            let action = self.mcp_picker.handle_key(key);
            return Some(self.handle_mcp_picker_action(action));
        }

        if self.permissions_picker.is_open() {
            let action = self.permissions_picker.handle_key(key);
            return Some(self.handle_permissions_picker_action(action));
        }

        if self.stash_picker.is_open() {
            let action = self.stash_picker.handle_key(key);
            return Some(self.handle_stash_picker_action(action));
        }

        if self.question_form.is_open() {
            // The form is docked, not modal: keys that only move the
            // transcript keep working while it waits for an answer.
            if self.scroll_transcript(key) {
                return Some(vec![]);
            }
            let action = self.question_form.handle_key(key);
            return Some(self.handle_question_form_action(action));
        }
        if self.session_picker.is_open() {
            let action = self.session_picker.handle_key(key);
            return Some(self.handle_session_picker_action(action));
        }
        if self.session_relocation_picker.is_open() {
            let action = self.session_relocation_picker.handle_key(key);
            return Some(self.handle_session_relocation_action(action));
        }
        if self.task_picker.is_open() {
            let action = self.task_picker.handle_key(key);
            return Some(self.handle_task_picker_action(action));
        }
        if self.memory_picker.is_open() {
            let action = self.memory_picker.handle_key(key);
            return Some(self.handle_memory_picker_action(action));
        }
        if self.workflow_inspector.is_open() {
            let action = self.workflow_inspector.handle_key(key);
            return Some(self.handle_workflow_inspector_action(action));
        }
        if self.workflow_catalog_picker.is_open() {
            let action = self.workflow_catalog_picker.handle_key(key);
            return Some(self.handle_workflow_catalog_action(action));
        }

        // Last of the overlays. The workbench is a full-screen view, not a
        // modal: anything above it draws over it and answers first, or it owns
        // the keyboard from behind an opaque screen and a permission prompt
        // becomes unanswerable.
        if self.workbench.is_open() {
            match self.workbench.handle_key(key) {
                WorkbenchAction::Passthrough => {}
                action => return Some(self.handle_workbench_action(action)),
            }
        }

        None
    }

    fn handle_model_picker_action(&mut self, action: ModelPickerAction) -> Vec<Action> {
        match action {
            ModelPickerAction::Consumed | ModelPickerAction::Close => vec![],
            ModelPickerAction::Select(spec) => vec![Action::ChangeModel(spec)],
            ModelPickerAction::Bind(purpose, binding) => vec![Action::Bind(purpose, binding)],
            ModelPickerAction::Unbind(purpose) => vec![Action::Unbind(purpose)],
        }
    }

    fn handle_command_action(&mut self, action: CommandAction) -> Option<Vec<Action>> {
        match action {
            CommandAction::Consumed => Some(Vec::new()),
            CommandAction::Execute(cmd) => {
                self.active_input_box_mut().discard();
                Some(self.execute_command(cmd, 0))
            }
            CommandAction::Complete(text) => {
                self.sync_command_palette(&text);
                let input = self.active_input_box_mut();
                input.set_input(text);
                input.buffer.move_to_end();
                Some(Vec::new())
            }
            CommandAction::Passthrough => None,
        }
    }

    fn handle_command_modal_action(&mut self, action: CommandModalAction) -> Vec<Action> {
        match action {
            CommandModalAction::Consumed | CommandModalAction::Closed => Vec::new(),
            CommandModalAction::Execute(cmd) => self.execute_command(cmd, 0),
        }
    }

    fn handle_search_action(&mut self, action: SearchAction) -> Vec<Action> {
        match action {
            SearchAction::Consumed => {}
            SearchAction::QueryChanged => {
                let chat = &mut self.chats[self.active_chat];
                let texts = chat.segment_search_texts();
                self.search_modal.update_matches(&texts);
                sync_search_highlight(&self.search_modal, chat);
            }
            SearchAction::Navigate => {
                sync_search_highlight(&self.search_modal, &mut self.chats[self.active_chat]);
            }
            SearchAction::Select(idx) => {
                let chat = &mut self.chats[self.active_chat];
                chat.scroll_to_segment(idx);
                chat.set_highlight_segment(None);
                self.search_modal.close();
            }
            SearchAction::Close(saved) => {
                let chat = &mut self.chats[self.active_chat];
                chat.set_highlight_segment(None);
                if let Some((top, auto)) = saved {
                    chat.restore_scroll(top, auto);
                }
                self.search_modal.close();
            }
        }
        Vec::new()
    }

    fn handle_workbench_action(&mut self, action: WorkbenchAction) -> Vec<Action> {
        match action {
            WorkbenchAction::Consumed | WorkbenchAction::Passthrough => {}
            WorkbenchAction::Close => self.workbench.close(),
            WorkbenchAction::Flash(message) => self.status_bar.flash(message),
            WorkbenchAction::Copy(text) => {
                let message = match self.clipboard.copy_text(&text) {
                    Ok(CopyResult::Noop) => "Nothing to copy".to_owned(),
                    Ok(CopyResult::Copied) => "Copied".to_owned(),
                    Err(e) => format!("Copy failed: {e}"),
                };
                self.status_bar.flash(message);
            }
            WorkbenchAction::SendToComposer { path, lines } => {
                self.workbench.close();
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
                if let InputAction::PaletteSync(val) =
                    self.active_input_box_mut().handle_paste_with_spaces(&path)
                {
                    self.sync_dropdowns(&val);
                }
            }
            FilePickerModalAction::Close => self.file_picker.close(),
        }
        Vec::new()
    }

    fn handle_rewind_picker_action(&mut self, action: RewindPickerAction) -> Vec<Action> {
        match action {
            RewindPickerAction::Consumed | RewindPickerAction::Close => Vec::new(),
            RewindPickerAction::Select(entry) => vec![Action::RewindSession(entry)],
        }
    }

    fn handle_message_actions_action(&mut self, action: MessageActionsAction) -> Vec<Action> {
        let MessageActionsAction::Select { source, kind } = action else {
            return Vec::new();
        };
        match kind {
            MessageActionKind::Fork => match self.fork_at(source) {
                Ok(forked) => vec![Action::ForkSession(Box::new(forked))],
                Err(error) => {
                    self.flash(error);
                    Vec::new()
                }
            },
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
        let QueueActionsAction::Select { id, kind } = action else {
            return Vec::new();
        };
        match kind {
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

    fn handle_theme_picker_action(&self, _action: ThemePickerAction) -> Vec<Action> {
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
        }
    }

    fn handle_thinking_picker_action(&mut self, action: ThinkingPickerAction) -> Vec<Action> {
        if let ThinkingPickerAction::Select(thinking) = action {
            self.apply_thinking(thinking);
        }
        Vec::new()
    }

    fn handle_login_picker_action(&mut self, action: LoginPickerAction) -> Vec<Action> {
        let closed = !matches!(&action, LoginPickerAction::Consumed);
        let actions = match action {
            LoginPickerAction::Consumed | LoginPickerAction::Close => Vec::new(),
            LoginPickerAction::Authenticated { model_spec } => {
                vec![Action::ChangeModel(model_spec), Action::RefreshModels]
            }
            LoginPickerAction::Configured { slug } => {
                vec![Action::RefreshProvider { slug }, Action::RefreshModels]
            }
            LoginPickerAction::AuthenticateProvider {
                provider,
                model_spec,
            } => vec![Action::AuthenticateProvider {
                provider,
                model_spec,
            }],
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
        if needs_login || self.mcp_picker.is_open() || self.permission_prompt.is_open() {
            self.permission_config_trust_deferred = needs_trust;
            return;
        }
        self.permission_config_trust_deferred = false;
        if needs_trust && let Err(error) = self.open_permissions_picker() {
            self.flash(error.to_string());
        }
    }

    fn open_permissions_picker(&mut self) -> Result<(), PermissionPolicyError> {
        let rules = self.permissions.structured_rule_inventory()?;
        let candidates = self.permissions.review_candidates();
        let policy = self.permissions.active_policy();
        let needs_project_config_trust = self.permissions.needs_project_permission_config_trust();
        let project_config_trusted = self.permissions.project_permission_config_trusted();
        self.permissions_picker.open(
            rules,
            &candidates,
            &policy,
            needs_project_config_trust,
            project_config_trusted,
        );
        Ok(())
    }

    fn handle_permissions_picker_action(&mut self, action: PermissionsPickerAction) -> Vec<Action> {
        match action {
            PermissionsPickerAction::Consumed => {}
            PermissionsPickerAction::Close => self.permissions_picker.close(),
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
                match self.permissions.revoke_structured_rule(&id) {
                    Ok(Some(scope)) => {
                        if scope == RevokedRuleScope::Conversation {
                            self.checkpoint_now();
                        }
                        match self.open_permissions_picker() {
                            Ok(()) => {}
                            Err(error) => {
                                self.permissions_picker.close();
                                self.flash(error.to_string());
                            }
                        }
                        self.flash("Permission revoked".into());
                    }
                    Ok(None) => self.flash("Permission is no longer active".into()),
                    Err(error) => self.flash(format!("Failed to revoke permission: {error}")),
                }
            }
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
            BuiltinAction::Search => {
                let top = self.chats[self.active_chat].scroll_top();
                let auto = self.chats[self.active_chat].auto_scroll();
                self.search_modal.open(top, auto);
            }
            BuiltinAction::Help => self.help_modal.toggle(),
            BuiltinAction::PlanToggle => {
                if self.plan_toggle_ready() {
                    self.plan_form.toggle();
                }
            }
            BuiltinAction::PlanEditor => {
                return match self.state.plan.path() {
                    Some(p) => vec![Action::OpenEditor(p.to_path_buf())],
                    None => {
                        self.flash(FLASH_NO_PLAN.into());
                        vec![]
                    }
                };
            }
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
            BuiltinAction::EditInput => return vec![Action::EditInputInEditor],
            BuiltinAction::PopQueue => {
                self.pop_active_queue();
            }
            BuiltinAction::PrevChat => {
                self.leave_active_chat();
                self.active_chat = self.active_chat.saturating_sub(1);
            }
            BuiltinAction::NextChat => {
                self.leave_active_chat();
                self.active_chat = (self.active_chat + 1).min(self.chats.len() - 1);
            }
            BuiltinAction::ModelPicker => {
                self.model_picker
                    .open(&self.state.model, &self.model_policy);
                return vec![Action::RefreshModels];
            }
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

    /// Where a click on a mention lands, from the composer or the transcript.
    /// The stored layout is skipped: the reader asked for one file, and opening
    /// a session's worth of tabs around it would bury the answer.
    pub(crate) fn open_workbench_at(&mut self, mention: &Mention) {
        if let Some(path) = mention.remote_path() {
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
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Vec<Action> {
        self.clear_selection_unless_pending_copy();
        self.sync_subagent_input_target();

        if !key::EXIT.matches(key) {
            self.last_exit = None;
        }

        // Suspend is checked ahead of the overlays so a wedged UI can always be
        // backgrounded. The workbench is the one overlay that has to win it:
        // Ctrl+Z there is undo, and losing an edit to SIGTSTP is unrecoverable.
        if key::SUSPEND.matches(key) && cfg!(unix) && !self.workbench.is_open() {
            return vec![Action::Suspend];
        }

        // Ahead of the overlays: a pending chord owns the next key outright, so
        // it can never leak into the composer or a picker behind the panel.
        if self.which_key.is_armed() {
            self.which_key.disarm();
            return self.resolve_leader(keybindings::normalize_leader_key(key));
        }

        if let Some(actions) = self.dispatch_overlay(key) {
            self.last_exit = None;
            return actions;
        }

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

        // Anything the transcript did not claim is on its way to the composer,
        // so the navigation keys go back with it.
        self.key_focus = KeyFocus::Composer;

        if key::THINKING.matches(key) || is_shift_tab(key) {
            self.cycle_reasoning_effort();
            return vec![];
        }

        // Ahead of the main/task split so the editing chords answer the same
        // way in every chat instead of only in the transcript.
        if is_ctrl(&key) {
            return self.handle_ctrl_key(key);
        }

        if !self.is_main_chat() {
            if self.active_subagent_can_steer() {
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
                        && t.elapsed() < self.status_bar.flash_duration
                    {
                        self.handle_subagent_cancel()
                    } else {
                        self.last_esc = Some(Instant::now());
                        self.status_bar.flash(FLASH_CANCEL.into());
                        vec![]
                    }
                }
                _ => vec![],
            };
        }

        self.handle_main_chat_key(key)
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
            match self.workbench.handle_leader(key) {
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
        if self.status == Status::Streaming {
            for (bind, admission) in [
                (leader::STEER_PROMPT, PromptAdmission::Steer),
                (leader::INTERRUPT_PROMPT, PromptAdmission::Interrupt),
            ] {
                if bind.matches(key) {
                    return self.handle_streaming_admission(admission);
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
        if self.plan_toggle_ready() {
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
        if self.status == Status::Streaming {
            contexts.push(KeybindContext::Streaming);
        }
        contexts
    }

    fn handle_subagent_chat_key(&mut self, key: KeyEvent) -> Vec<Action> {
        let command_action = self
            .command_palette
            .handle_key(key, &self.subagent_input_box.buffer.value());
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
            InputAction::Passthrough(key) => match key.code {
                KeyCode::Esc => {
                    if let Some(t) = self.last_esc.take()
                        && t.elapsed() < self.status_bar.flash_duration
                    {
                        self.handle_subagent_cancel()
                    } else {
                        self.last_esc = Some(Instant::now());
                        self.status_bar.flash(FLASH_CANCEL.into());
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
        } else if let InputAction::PaletteSync(val) = self.active_input_box_mut().handle_key(key) {
            self.sync_dropdowns(&val);
        }
        vec![]
    }

    /// Whether a composer is drawn, which is what
    /// [`crate::app::view`] renders on and therefore what may take text.
    fn composer_is_visible(&self) -> bool {
        self.is_main_chat() || self.active_subagent_can_steer() || self.queue_editor_active()
    }

    fn handle_subagent_submit(&mut self, sub: Submission) -> Vec<Action> {
        if self.intercept_inspection_submission(&sub.text) {
            return vec![];
        }
        if sub.images.is_empty() && sub.text.trim() == "/queue" {
            self.focus_active_queue();
            return vec![];
        }
        if sub.text.trim().is_empty() {
            return vec![];
        }
        let Some(task_id) = self.active_subagent_id().map(str::to_owned) else {
            return vec![];
        };
        let Submission {
            text,
            images,
            mentions,
            draft,
        } = sub;
        self.steer_task(&task_id, text, images, mentions, draft)
    }

    /// The one place a steer reaches a running task, shared by the composer
    /// and by commands that expand into a prompt, so neither can drift from
    /// the queue bookkeeping the panel renders from. Returns the draft to the
    /// composer when the task stopped being steerable.
    fn steer_task(
        &mut self,
        task_id: &str,
        text: String,
        images: Vec<ImageSource>,
        mentions: Vec<Mention>,
        draft: InputDraft,
    ) -> Vec<Action> {
        let Some(tx) = self.subagent_steers.get(task_id) else {
            self.subagent_input_box.set_draft(draft);
            return vec![];
        };
        let input = AgentInput {
            message: text.clone(),
            mode: AgentMode::Build,
            images,
            mentions,
            preamble: Vec::new(),
            thinking: self.state.thinking.clone(),
            fast: self.state.fast,
            prompt: None,
            resume: false,
        };
        let id = tx.push(input);
        self.pending_subagent_steers
            .entry(task_id.to_owned())
            .or_default()
            .push_back(PendingSteer { id, text, draft });
        vec![]
    }

    fn intercept_inspection_submission(&mut self, text: &str) -> bool {
        let (token, args) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
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
            self.load_lifetime_usage();
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
            LogsAction::Copy { text, label } => match self.clipboard.copy_text(&text) {
                Ok(CopyResult::Noop) => {}
                Ok(CopyResult::Copied) => self.flash(label.into()),
                Err(e) => self.flash(format!("{COPY_FAILED}{e}")),
            },
        }
    }

    fn execute_tools(&mut self) {
        self.context_snapshot = Watch::seeded(self.active_context_snapshot());
        self.tools_modal.open();
    }

    fn execute_skills(&mut self) {
        self.context_snapshot = Watch::seeded(self.active_context_snapshot());
        self.skills_modal.open();
    }

    fn preserve_unconsumed_steers(&mut self, task_id: &str) {
        let remaining = self
            .subagent_steers
            .remove(task_id)
            .map(|queue| queue.drain())
            .unwrap_or_default();
        let Some(pending) = self.pending_subagent_steers.remove(task_id) else {
            return;
        };
        let remaining_ids: std::collections::HashSet<_> =
            remaining.into_iter().map(|(id, _)| id).collect();
        let unsent: VecDeque<_> = pending
            .into_iter()
            .filter(|item| remaining_ids.contains(&item.id))
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

        let command_action = self
            .command_palette
            .handle_key(key, &self.input_box.buffer.value());
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
                            && t.elapsed() < self.status_bar.flash_duration
                        {
                            if streaming {
                                self.handle_cancel()
                            } else {
                                self.open_rewind_picker()
                            }
                        } else {
                            self.last_esc = Some(Instant::now());
                            self.status_bar.flash(
                                if streaming {
                                    FLASH_CANCEL
                                } else {
                                    FLASH_REWIND
                                }
                                .into(),
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

    fn quit(&mut self) -> Vec<Action> {
        self.quit_with(ExitRequest::Success)
    }

    fn quit_with(&mut self, req: ExitRequest) -> Vec<Action> {
        self.save_input_history();
        self.exit_request = req;
        vec![Action::ManualExit]
    }

    pub(crate) fn clear_exit_request(&mut self) {
        self.exit_request = ExitRequest::None;
    }

    pub(crate) fn handle_submit(&mut self, sub: Submission) -> Vec<Action> {
        self.handle_submit_with_admission(sub, PromptAdmission::Queue)
    }

    fn handle_streaming_admission(&mut self, admission: PromptAdmission) -> Vec<Action> {
        if !self.is_main_chat() || self.status != Status::Streaming || self.queue_editor_active() {
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
                    self.send_to_agent(subagent_id.as_deref(), String::new());
                }
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
        self.submit_or_queue_with_admission(sub.into(), admission)
    }

    fn handle_cancel(&mut self) -> Vec<Action> {
        if self.cancelling_run.is_some() {
            return Vec::new();
        }
        let cancelled_run = self.begin_main_cancel(false, true);
        vec![Action::CancelAgent {
            run_id: cancelled_run,
        }]
    }

    pub(super) fn begin_main_cancel(&mut self, preserve_queue: bool, await_terminal: bool) -> u64 {
        self.cancel_queue_edit();
        let cancelled_run = self.run_id;
        self.run_id += 1;
        self.cancelling_run = await_terminal.then_some(cancelled_run);
        self.retry_info = None;
        self.close_all_overlays();
        self.pending_input = PendingInput::None;
        self.finish_subagents(TaskOutcome::Killed, CANCELLED_TEXT);
        self.subagent_answers.clear();
        self.question_subagent = None;
        self.preserve_all_unconsumed_steers();
        self.subagent_steers.clear();
        self.pending_subagent_steers.clear();
        self.shell.cancel_all();
        for chat in &mut self.chats {
            chat.flush();
            chat.cancel_in_progress();
        }
        self.main_chat()
            .push(DisplayMessage::new(DisplayRole::Error, CANCEL_MSG.into()));
        if !preserve_queue {
            self.queue.clear();
            self.replacement_item = None;
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
        self.subagent_answers.remove(&task_id);
        self.clear_auth_waiter(Some(&task_id));
        self.preserve_unconsumed_steers(&task_id);
        // The tool the form was parked on is gone, so the form goes with it.
        if self.question_subagent.as_deref() == Some(task_id.as_str()) {
            self.question_subagent = None;
            self.question_form.close();
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
                &model,
                LedgerPurpose::Title,
                usage,
                cost,
                billing,
            );
            self.state.goal.record_external_usage(usage, cost, billing);
            if let Some(title) = title {
                self.state.session_mut().set_title_if_auto(title);
            }
            return vec![];
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
            if envelope.run_id != self.run_id
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
            if envelope.run_id == self.run_id {
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
                            answer_tx: None,
                            steer_tx: None,
                        })
                    });
                self.chats[sub_idx].mark_finished(TaskOutcome::Unknown, DONE_TEXT);
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
        if envelope.run_id != self.run_id {
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
                    if self.replacement_item.is_some() || !self.queue.panel_entries().is_empty() {
                        Status::Streaming
                    } else {
                        Status::Idle
                    };
                if let Err(error) = self.snapshot_history_head() {
                    self.flash(format!("Failed to snapshot cancelled run: {error}"));
                }
                if cancelled_error {
                    self.queue.resume();
                }
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

        let snapshot_top_level = envelope.subagent.is_none()
            && matches!(
                &envelope.event,
                AgentEvent::Done { .. } | AgentEvent::Error { .. }
            );

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

        if let AgentEvent::ToolDone(ref e) = envelope.event {
            // Whatever the call opened and never claimed was a prediction the
            // call disagreed with: bad arguments, or a child `batch` refused.
            if subagent_id.is_none() {
                self.discard_pending_delegations_under(&e.id);
            }
            if self.state.mode == Mode::Plan
                && (self.state.plan.path().is_some_and(|pp| e.wrote_to(pp))
                    || self
                        .state
                        .plan
                        .document_ref()
                        .is_some_and(|reference| e.wrote_document(&reference)))
            {
                self.transition_plan(PlanTrigger::WriteDone);
            }
            self.state
                .session_mut()
                .insert_tool_output(e.id.clone(), e.output.clone());
            // Subagents get `todo_write` too, but their plans are private:
            // letting one repaint the panel would erase the main list the user
            // is watching.
            if subagent_id.is_none()
                && let caudra_agent::types::ToolOutput::TodoList(items) = &e.output
            {
                self.todo_panel.set_items(items.clone());
            }
            if subagent_id.is_none()
                && let Some(task_id) = self.parent_task_ids.get(&e.id)
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
            self.retry_info = None;
            return vec![];
        }

        if let AgentEvent::Retry {
            attempt,
            message,
            delay_ms,
        } = envelope.event
        {
            self.chats[chat_idx].stream_reset();
            self.discard_stream_delegations(chat_idx);
            if chat_idx == 0 {
                self.retry_info = Some(RetryInfo {
                    attempt,
                    message,
                    deadline: Instant::now() + Duration::from_millis(delay_ms),
                });
            }
            return vec![];
        }

        self.retry_info = None;

        if let AgentEvent::TurnComplete(ref tc) = envelope.event {
            self.state.token_usage += tc.usage;
            self.add_session_spend(tc.cost, tc.billing);
            add_chat_spend(&mut self.chats[chat_idx], tc.cost, tc.billing);
            if subagent_id.is_some() {
                self.state
                    .goal
                    .record_external_usage(tc.usage, tc.cost, tc.billing);
            }
            self.record_model_usage(
                &tc.provider,
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
            if let Some(tool_id) = &parent_tool_use_id {
                self.chats[0].set_tool_progress(tool_id, progress);
            }
            return vec![];
        }

        let event = match envelope.event {
            AgentEvent::GoalEvaluating { evaluation } => {
                self.flash(format!("Evaluating goal (#{evaluation})..."));
                return vec![];
            }
            AgentEvent::GoalEvaluation {
                verdict,
                reason,
                evaluation,
                applied,
                usage,
                cost,
                billing,
                model,
            } => {
                self.state.token_usage += usage;
                self.add_session_spend(cost, billing);
                add_chat_spend(&mut self.chats[chat_idx], cost, billing);
                self.record_goal_usage(&model, usage, cost, billing);
                if applied && verdict == GoalVerdict::NotMet {
                    self.main_chat().push(DisplayMessage::new(
                        DisplayRole::Notice,
                        format!("Goal not yet met (#{evaluation}): {reason}"),
                    ));
                }
                return vec![];
            }
            AgentEvent::GoalFinished { result } => {
                let (role, label) = match result.verdict {
                    GoalVerdict::Met => (DisplayRole::Done, "Goal achieved"),
                    GoalVerdict::Impossible | GoalVerdict::NotMet => {
                        (DisplayRole::Error, "Goal could not be achieved")
                    }
                };
                self.main_chat().push(DisplayMessage::new(
                    role,
                    format!("{label}: {}", result.reason),
                ));
                return vec![];
            }
            AgentEvent::GoalDeferred {
                active_background_tasks,
            } => {
                self.goal_deferred = true;
                self.main_chat().push(DisplayMessage::new(
                    DisplayRole::Notice,
                    format!(
                        "Goal evaluation deferred while {active_background_tasks} background task(s) run."
                    ),
                ));
                return vec![];
            }
            AgentEvent::GoalLoopCap {
                evaluations,
                continuations,
                limit,
            } => {
                self.main_chat().push(DisplayMessage::new(
                    DisplayRole::Notice,
                    format!(
                        "Goal remains active after {continuations} automatic continuations in this run ({evaluations} total evaluations); the session limit is {limit}. Send another message to resume."
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
                evaluation,
                message,
                applied,
                usage,
                cost,
                billing,
                model,
            } => {
                self.state.token_usage += usage;
                self.add_session_spend(cost, billing);
                add_chat_spend(&mut self.chats[chat_idx], cost, billing);
                self.record_goal_usage(&model, usage, cost, billing);
                if applied {
                    self.main_chat().push(DisplayMessage::new(
                        DisplayRole::Error,
                        format!(
                            "Goal evaluation #{evaluation} failed; the goal remains active: {message}"
                        ),
                    ));
                }
                return vec![];
            }
            AgentEvent::GoalClearedAfterError { condition, message } => {
                self.main_chat().push(DisplayMessage::new(
                    DisplayRole::Error,
                    format!("Goal cleared after an unrecoverable error: {condition} ({message})"),
                ));
                return vec![];
            }
            event => event,
        };

        let plan_path = if self.state.mode == Mode::Plan {
            self.state.plan.path()
        } else {
            None
        };
        let result = self.chats[chat_idx].handle_event(event, plan_path);

        let result = match result {
            ChatEventResult::QueueBatchConsumed { items } => {
                if chat_idx == 0 {
                    self.on_queue_batch_consumed(&items);
                } else if let Some(task_id) = subagent_id {
                    let messages = items
                        .iter()
                        .map(|item| format_with_images(&item.text, item.image_count));
                    self.chats[chat_idx].show_user_messages(messages);
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

        if let ChatEventResult::QueueItemConsumed {
            id,
            text,
            image_count,
        } = result
        {
            if chat_idx == 0 {
                self.on_queue_item_consumed(id, &text, image_count);
            } else if let Some(task_id) = subagent_id {
                self.chats[chat_idx].show_user_message(text.clone());
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
            self.context_modal.close();
            self.tools_modal.close();
            self.skills_modal.close();
            self.storage_modal.close();
            if self.permissions_picker.is_open() {
                self.permissions_picker.close();
                self.permission_config_trust_deferred =
                    self.permissions.needs_project_permission_config_trust();
            }
            self.permission_prompt.enqueue(request, subagent_id);
            return vec![];
        }

        if let ChatEventResult::PermissionRequestResolved { request_id } = result {
            self.permission_prompt.resolve_pending(&request_id);
            return vec![];
        }

        // A subagent's question routes back through that subagent's own
        // answer channel, so the form remembers which chat asked.
        if let ChatEventResult::Question(event) = result {
            self.question_form.open(event.questions);
            self.question_subagent = subagent_id;
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
                    if !self.goal_deferred {
                        self.state.turns += 1;
                        self.terminalize_turn(MISSING_TOOL_COMPLETION);
                        self.preserve_all_unconsumed_steers();
                        self.chat_index.clear();
                        self.subagent_answers.clear();
                        self.subagent_steers.clear();
                    }
                    self.status = Status::Idle;
                    self.fire_session_autocmd("TurnEnd", serde_json::json!({}));
                    if self.exit_on_done {
                        self.exit_request = ExitRequest::Success;
                    }
                }
                ChatEventResult::Error(message) => {
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
                    self.subagent_answers.clear();
                    self.terminalize_turn(&message);
                    self.preserve_all_unconsumed_steers();
                    self.subagent_steers.clear();
                    self.recoverable_queue = self.queue.pending_prompts();
                    self.recoverable_queue_together =
                        self.queue.delivery() == caudra_agent::QueueDelivery::TogetherNextTurn;
                    self.queue.clear();
                    self.chat_index.clear();
                    self.fire_session_autocmd(
                        "TurnError",
                        serde_json::json!({ "message": message }),
                    );
                    if self.exit_on_done {
                        self.exit_request = ExitRequest::Error;
                    }
                }
                ChatEventResult::AuthRequired
                | ChatEventResult::AuthRestored
                | ChatEventResult::Question(_)
                | ChatEventResult::PermissionRequest(_)
                | ChatEventResult::PermissionRequestResolved { .. }
                | ChatEventResult::QueueItemConsumed { .. }
                | ChatEventResult::QueueBatchConsumed { .. } => unreachable!(),
                ChatEventResult::Continue => {}
            }
        }
        if snapshot_top_level && let Err(error) = self.snapshot_history_head() {
            self.flash(format!("Failed to snapshot completed run: {error}"));
        }
        vec![]
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
        if first_parent_event {
            self.chats[0].update_tool_summary(parent_tool_use_id, &subagent.name);
            if let Some(ref model) = subagent.model {
                self.chats[0].update_tool_model(parent_tool_use_id, model);
            }
        }

        if let Some(idx) = self.adopt_pending_delegation(parent_tool_use_id, task_id) {
            let chat = &mut self.chats[idx];
            chat.name.clone_from(&subagent.name);
            chat.model_id.clone_from(&subagent.model);
            self.chat_index.insert(task_id.clone(), idx);
            self.sync_subagents();
            return idx;
        }

        if let Some(&idx) = self.chat_index.get(task_id.as_str()) {
            self.chats[idx].set_parent_tool_use_id(parent_tool_use_id.clone());
            if self.chats[idx].is_finished() {
                let chat = &mut self.chats[idx];
                chat.resume();
                chat.name.clone_from(&subagent.name);
                chat.model_id.clone_from(&subagent.model);
                if let Some(ref prompt) = subagent.prompt {
                    chat.push_user_message(prompt);
                }
                self.sync_subagents();
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
            if let Some(ref prompt) = subagent.prompt {
                chat.push_user_message(prompt);
            }
            idx
        } else {
            let mut chat = Chat::subagent(
                task_id,
                subagent.name.clone(),
                self.ui_config.clone(),
                self.lua_event_handle.clone(),
            );
            chat.set_parent_tool_use_id(parent_tool_use_id.clone());
            chat.set_restore_channel(self.restore_event_tx.clone());
            chat.model_id = subagent.model.clone();
            if let Some(ref prompt) = subagent.prompt {
                chat.push_user_message(prompt);
            }
            self.chats.push(chat);
            self.chats.len() - 1
        };
        self.chat_index.insert(task_id.clone(), idx);
        self.sync_subagents();
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
        let resolved = self
            .command_palette
            .resolve(&format!("/{}", name.trim_start_matches('/')))
            .ok_or_else(|| format!("unknown command '{name}'"))?;
        if self.command_is_out_of_scope(&resolved) {
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
    fn command_is_out_of_scope(&self, name: &str) -> bool {
        !self.is_main_chat() && self.command_palette.is_main_only(name)
    }

    /// {depth} is the `caudra.api.run_command` hop count, forwarded to a Lua
    /// handler so an alias cycle keeps counting. 0 when the user typed it.
    fn execute_command(&mut self, cmd: ParsedCommand, depth: u8) -> Vec<Action> {
        if self.command_is_out_of_scope(&cmd.name) {
            self.flash(MAIN_ONLY_CMD_MSG.into());
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
            "/tools" => {
                self.execute_tools();
                vec![]
            }
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
            "/goal" => self.execute_goal(&cmd.args),
            "/goal-clear" => self.clear_goal(),
            "/goal-model" => self.open_goal_model_picker(),
            "/continue" => self.continue_run(),
            "/new" => vec![Action::RequestNewSession],
            "/queue" => {
                self.focus_active_queue();
                vec![]
            }
            "/stash" => self.run_builtin(BuiltinAction::StashPush),
            "/stash-pop" => self.run_builtin(BuiltinAction::StashPop),
            "/stash-list" => self.run_builtin(BuiltinAction::StashList),
            "/memory" => self.memory_browse(),
            "/tasks" => self.tasks_browse(),
            "/workflows" => self.workflows_browse(),
            "/workflow" => self.execute_workflow(&cmd.args),
            "/deep-research" => self.execute_deep_research(&cmd.args),
            "/sessions" => self.sessions_browse(),
            "/move-session" | "/migrate-sessions" => {
                let destination = cmd.args.trim();
                vec![Action::OpenSessionRelocation {
                    bulk: cmd.name == "/migrate-sessions",
                    destination: (!destination.is_empty()).then(|| destination.to_owned()),
                }]
            }
            "/rename" => self.rename_session(&cmd.args),
            "/model" => {
                self.model_picker
                    .open(&self.state.model, &self.model_policy);
                vec![Action::RefreshModels]
            }
            "/system-prompt" => {
                self.prompt_profile_picker
                    .open(&self.state.system_prompt_profile_name);
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
            "/permissions" => {
                match self.open_permissions_picker() {
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
            "/remote" => {
                vec![Action::RemoteControl(cmd.args)]
            }
            "/yolo" => {
                let enabled = self.permissions.toggle_yolo();
                let msg = if enabled {
                    "YOLO mode enabled"
                } else {
                    "YOLO mode disabled"
                };
                self.flash(msg.into());
                vec![]
            }
            "/thinking" => {
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
            self.goal_modal.open();
            return vec![];
        }
        if condition.eq_ignore_ascii_case("model") {
            return self.open_goal_model_picker();
        }
        if matches!(
            condition.to_ascii_lowercase().as_str(),
            "clear" | "stop" | "off" | "reset" | "none" | "cancel"
        ) {
            return self.clear_goal();
        }

        let replacing = self.state.goal.snapshot().is_some();
        match self.state.goal.set(condition) {
            Ok(_) => {
                self.flash(
                    if replacing {
                        "Goal replaced"
                    } else {
                        "Goal set"
                    }
                    .into(),
                );
                self.submit_goal(condition)
            }
            Err(error) => {
                self.flash(error.to_string());
                vec![]
            }
        }
    }

    fn open_goal_model_picker(&mut self) -> Vec<Action> {
        self.model_picker.open_purpose(
            &self.state.model,
            &self.model_policy,
            caudra_providers::ModelPurpose::Goal,
        );
        vec![Action::RefreshModels]
    }

    fn clear_goal(&mut self) -> Vec<Action> {
        let message = self.state.goal.clear().map_or_else(
            || "No goal set".to_string(),
            |goal| format!("Goal cleared: {}", goal.condition),
        );
        self.flash(message);
        vec![]
    }

    fn execute_mcp_prompt(&mut self, name: &str, args: &str) -> Vec<Action> {
        let prompt = self.command_palette.find_mcp_prompt(name).unwrap().clone();

        let arguments = Self::parse_prompt_args(&prompt, args);
        let missing: Vec<_> = prompt
            .arguments
            .iter()
            .filter(|a| a.required && !arguments.contains_key(&a.name))
            .map(|a| format!("<{}>", a.name))
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
            paste_ranges: Vec::new(),
        });
        input.prompt = Some(Box::new(prompt_ref));

        if self.status == Status::Streaming {
            self.flash("Agent is busy, try again later".into());
            vec![]
        } else {
            self.start_run(input, display_text)
        }
    }

    fn parse_prompt_args(prompt: &McpPromptInfo, args: &str) -> HashMap<String, String> {
        let mut result = HashMap::new();
        let mut remaining = args.trim();
        if remaining.is_empty() || prompt.arguments.is_empty() {
            return result;
        }
        let last_idx = prompt.arguments.len() - 1;
        for (i, arg) in prompt.arguments.iter().enumerate() {
            if remaining.is_empty() {
                break;
            }
            if i == last_idx {
                result.insert(arg.name.clone(), remaining.to_string());
            } else if let Some((word, rest)) = remaining.split_once(char::is_whitespace) {
                result.insert(arg.name.clone(), word.to_string());
                remaining = rest.trim_start();
            } else {
                result.insert(arg.name.clone(), remaining.to_string());
                break;
            }
        }
        result
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
        if let Some(task_id) = self.active_subagent_id().map(str::to_owned)
            && self.subagent_steers.contains_key(&task_id)
        {
            return self.steer_task(&task_id, text, Vec::new(), mentions, InputDraft::default());
        }
        self.submit_or_queue(QueuedMessage {
            text,
            images: Vec::new(),
            mentions,
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
        snapshot_store: Arc<SnapshotStore>,
        permissions: PermissionsConfig,
    ) {
        self.release_remote_restore_confirmation();
        self.file_picker.close();
        self.mention_popup.close();
        self.workbench.bind_local();
        self.permissions_picker.close();
        self.permission_config_trust_deferred = false;
        self.permissions.set_project_with_config(cwd, permissions);
        self.state
            .session_mut()
            .set_cwd(cwd.to_string_lossy().into_owned());
        self.rebind_workspace_baseline(snapshot_store, cwd.to_path_buf());
        self.status_bar.refresh_cwd(&cwd.to_string_lossy());
        self.sync_composer_cwd();
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
        self.release_remote_restore_confirmation();
        self.file_picker.close();
        self.mention_popup.close();
        self.state.session = candidate;
        self.command_palette
            .set_custom_commands(if self.no_commands {
                Arc::from([])
            } else {
                Arc::from(caudra_agent::command::discover_remote_commands(
                    &change.context,
                ))
            });
        self.workspace_baseline.rebind_workspace_session(
            self.storage.clone(),
            self.state.session.id,
            change.workspace.clone(),
            change.binding,
        );
        self.workspace_baseline
            .set_current_head(self.history_head());
        self.workspace_session = Some(change.workspace);
        if let Some(workspace) = self.workspace_session.clone()
            && let Err(error) = self.workbench.bind_workspace_with_gate(
                workspace,
                remote_workbench_gate(Arc::clone(&self.workspace_baseline)),
            )
        {
            tracing::warn!(%error, "remote workbench backend rebind failed");
        }
        self.remote_project_context = Some(change.context);
        self.status_bar.set_remote_cwd(change.display_path);
        Ok(())
    }

    /// Points both composers at the session's working directory, which is what
    /// decides whether an `@path` resolves to a file or stays prose.
    fn sync_composer_cwd(&mut self) {
        if self.workspace_session.is_some() {
            self.input_box.set_remote_cwd();
            self.subagent_input_box.set_remote_cwd();
            return;
        }
        let cwd = PathBuf::from(&self.state.session.cwd);
        self.input_box.set_cwd(cwd.clone());
        self.subagent_input_box.set_cwd(cwd);
    }

    fn overlays(&self) -> [&dyn Overlay; 35] {
        [
            &self.workbench,
            &self.logs_modal,
            &self.help_modal,
            &self.usage_modal,
            &self.context_modal,
            &self.tools_modal,
            &self.skills_modal,
            &self.storage_modal,
            &self.goal_modal,
            &self.btw_modal,
            &self.float_mgr,
            &self.search_modal,
            &self.file_picker,
            &self.paste_editor,
            &self.rewind_picker,
            &self.message_actions,
            &self.queue_actions,
            &self.review,
            &self.command_modal,
            &self.theme_picker,
            &self.thinking_picker,
            &self.prompt_profile_picker,
            &self.model_picker,
            &self.login_picker,
            &self.mcp_picker,
            &self.permissions_picker,
            &self.stash_picker,
            &self.memory_picker,
            &self.task_picker,
            &self.workflow_inspector,
            &self.workflow_catalog_picker,
            &self.question_form,
            &self.session_picker,
            &self.session_relocation_picker,
            &self.permission_prompt,
        ]
    }

    fn overlays_mut(&mut self) -> [&mut dyn Overlay; 35] {
        [
            &mut self.workbench,
            &mut self.logs_modal,
            &mut self.help_modal,
            &mut self.usage_modal,
            &mut self.context_modal,
            &mut self.tools_modal,
            &mut self.skills_modal,
            &mut self.storage_modal,
            &mut self.goal_modal,
            &mut self.btw_modal,
            &mut self.float_mgr,
            &mut self.search_modal,
            &mut self.file_picker,
            &mut self.paste_editor,
            &mut self.rewind_picker,
            &mut self.message_actions,
            &mut self.queue_actions,
            &mut self.review,
            &mut self.command_modal,
            &mut self.theme_picker,
            &mut self.thinking_picker,
            &mut self.prompt_profile_picker,
            &mut self.model_picker,
            &mut self.login_picker,
            &mut self.mcp_picker,
            &mut self.permissions_picker,
            &mut self.stash_picker,
            &mut self.memory_picker,
            &mut self.task_picker,
            &mut self.workflow_inspector,
            &mut self.workflow_catalog_picker,
            &mut self.question_form,
            &mut self.session_picker,
            &mut self.session_relocation_picker,
            &mut self.permission_prompt,
        ]
    }

    pub fn any_overlay_open(&self) -> bool {
        self.overlays().iter().any(|o| o.is_open())
    }

    /// True when the agent is parked on user input. Drives the `needs_input`
    /// session status.
    pub(crate) fn awaiting_input(&self) -> bool {
        self.permission_prompt.is_open()
            || self.pending_input != PendingInput::None
            || self.float_mgr.needs_input()
            || self.question_form.is_open()
    }

    pub(crate) fn lifecycle_blocker(&self) -> Option<&'static str> {
        [
            (self.permission_prompt.is_open(), PERMISSION_BLOCKER),
            (
                matches!(self.pending_input, PendingInput::AuthRetry { .. }),
                AUTH_BLOCKER,
            ),
            (
                self.status != Status::Streaming && self.plan_form_active(),
                PLAN_BLOCKER,
            ),
            (
                self.float_mgr.needs_input() || self.question_form.is_open(),
                QUESTION_BLOCKER,
            ),
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
        .find_map(|(blocked, message)| blocked.then_some(message))
    }

    pub(crate) fn has_lifecycle_work(&self) -> bool {
        self.status == Status::Streaming
            || self.retry_info.is_some()
            || self.restoring.load(Ordering::Relaxed)
            || self.btw_modal.is_streaming()
            || self
                .state
                .session
                .meta
                .pending_revert
                .as_ref()
                .is_some_and(|pending| pending.restore_operation.is_some())
    }

    /// True while `recoverable_queue` holds user text captured at an agent
    /// error; a background run would wipe it (`start_run` clears the queue).
    pub(crate) fn holds_recovery_text(&self) -> bool {
        !self.recoverable_queue.is_empty()
    }

    pub(crate) fn prepare_shutdown(&mut self) {
        self.release_remote_restore_confirmation();
        if self.recoverable_queue.is_empty() {
            self.recoverable_queue = self.queue.pending_prompts();
            self.recoverable_queue_together =
                self.queue.delivery() == caudra_agent::QueueDelivery::TogetherNextTurn;
        }
        self.queue.clear();
        self.shell.cancel_all();
    }

    pub(super) fn release_remote_restore_confirmation(&mut self) {
        let Some(confirmation) = self.remote_restore_confirmation.take() else {
            return;
        };
        if let Err(error) = smol::block_on(
            self.workspace_baseline
                .release_remote_prepared(&confirmation.prepared),
        ) {
            tracing::warn!(%error, "failed to release remote restore preview");
        }
    }

    pub(crate) fn disconnect_agent_queue(&mut self) {
        self.queue.disconnect();
    }

    pub(crate) fn goal_checkin_due(&self) -> bool {
        self.goal_deferred && self.state.goal.snapshot().is_some()
    }

    pub(crate) fn attention(&self) -> Option<Notification> {
        if let Some(tool) = self.permission_prompt.tool() {
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

    pub fn has_modal_overlay(&self) -> bool {
        self.overlays().iter().any(|o| o.is_open() && o.is_modal())
    }

    pub fn close_all_overlays(&mut self) {
        self.overlays_mut().iter_mut().for_each(|o| o.close());
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
            | self.tick_btw()
            | self.status_bar.poll_branch_update()
            | self.status_bar.clear_expired_hint()
            | self.mcp_picker.refresh()
            | self.tick_permission_config_trust()
            | self.poll_snapshot_refusal()
            | self.model_picker.refresh()
            | self.usage_modal.poll(&self.usage_slot)
            | self.storage_modal.poll(&self.storage_slot)
            | self.poll_context_snapshot()
            | self.logs_modal.poll()
            | self.hints.poll(self.hint_reader.load_full())
            | self.tick_file_picker()
            | self.mention_popup.tick()
            | self.refresh_memory_picker_if_stale()
            | self.refresh_session_picker()
            | self.poll_workflow_replies()
            | self.tick_workbench()
            | self.thinking_picker.tick()
            | Dirty::any(self.chats.iter_mut().map(Chat::tick))
    }

    fn poll_context_snapshot(&mut self) -> Dirty {
        if !self.context_modal.is_open()
            && !self.tools_modal.is_open()
            && !self.skills_modal.is_open()
        {
            return Dirty::NO;
        }
        let snapshot = self.active_context_snapshot();
        self.context_snapshot.poll(snapshot)
    }

    fn tick_workbench(&mut self) -> Dirty {
        if !self.workbench.is_open() {
            return Dirty::NO;
        }
        self.sync_workbench_theme();
        // The workbench paints its own bars, so the setting reaches it here
        // rather than through the shared helper every other surface calls.
        self.workbench.set_scrollbars(scrollbar::enabled());
        let (dirty, flash) = self.workbench.tick();
        if let Some(flash) = flash {
            self.status_bar.flash(flash);
        }
        self.persist_workbench_layout();
        Dirty::from(dirty)
    }

    /// A layout moves when a tab opens or a pane resizes, which is rare enough
    /// to write on the change itself rather than on a timer or on each of the
    /// several ways an overlay can leave the screen.
    fn persist_workbench_layout(&mut self) {
        let layout = self.workbench.layout();
        if self.workbench_layout.as_ref() == Some(&layout) {
            return;
        }
        let cwd = PathBuf::from(&self.state.session.cwd);
        caudra_storage::workbench::persist(&self.storage, &cwd, &layout);
        self.workbench_layout = Some(layout);
    }

    /// A refusal is decided inside a tool call, so the UI learns it by polling.
    /// Reported once per verdict: it is a standing property of the workspace,
    /// not an event, and repeating it every turn would be noise.
    fn poll_snapshot_refusal(&mut self) -> Dirty {
        if self.workspace_baseline.is_remote() && self.workspace_baseline.is_capturing() {
            self.status_bar
                .flash("Capturing remote workspace snapshot".into());
            return Dirty::YES;
        }
        let reason = self.workspace_baseline.refusal();
        if reason.as_deref().map(String::as_str) == self.snapshots_unavailable.as_deref() {
            return Dirty::NO;
        }
        self.snapshots_unavailable = reason.as_deref().cloned();
        let Some(reason) = reason else {
            return Dirty::NO;
        };
        self.status_bar
            .flash(format!("{NO_FILE_REVERT_MSG}: {reason}"));
        self.checkpoint();
        Dirty::YES
    }

    fn tick_permission_config_trust(&mut self) -> Dirty {
        if !self.permission_config_trust_deferred
            || self.login_picker.is_open()
            || self.mcp_picker.is_open()
            || self.permission_prompt.is_open()
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
        if self.usage_modal.scope() != UsageScope::Lifetime || self.lifetime_usage.is_some() {
            return;
        }
        match UsageLedger::open(&self.storage).and_then(|ledger| ledger.lifetime()) {
            Ok(lifetime) => self.lifetime_usage = Some(lifetime),
            Err(error) => tracing::warn!(%error, "lifetime usage unavailable"),
        }
    }

    /// The one place a turn's spend is recorded. The session keeps its own
    /// per-model breakdown for `/usage`, and the ledger keeps the money: a
    /// forgotten session must not take what it cost with it.
    ///
    /// `provider` comes from whoever answered rather than from the chat model.
    /// Goal evaluation, compaction and titles can each resolve to another
    /// provider, and billing them to the conversation's provider would make
    /// `--group-by provider` describe a session that never happened.
    fn record_model_usage(
        &mut self,
        provider: &str,
        model: &str,
        purpose: LedgerPurpose,
        usage: TokenUsage,
        cost: Option<f64>,
        billing: Billing,
    ) {
        let key = session_usage_model(provider, model);
        self.state
            .session_mut()
            .add_model_usage(&key, usage.billed(cost, billing));
        self.storage_writer.record_usage(TurnUsage {
            provider: provider.to_owned(),
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
        self.record_model_usage(&provider, &model, LedgerPurpose::Goal, usage, cost, billing);
    }

    /// The session's running spend, filed under whoever pays for it. Every
    /// arrival goes through here so the two totals never mix.
    fn add_session_spend(&mut self, cost: Option<f64>, billing: Billing) {
        match billing {
            Billing::Api => add_cost(&mut self.state.cost, cost),
            Billing::Subscription => add_cost(&mut self.state.subscription_cost, cost),
        }
    }

    /// btw spends real tokens outside any turn, so it settles into the same
    /// ledger as compaction and the goal evaluator. It deliberately leaves
    /// `context_size` alone: the question never enters history.
    fn tick_btw(&mut self) -> Dirty {
        let dirty = self.btw_modal.poll();
        if let Some(btw) = self.btw_modal.take_usage() {
            self.state.token_usage += btw.usage;
            self.add_session_spend(btw.cost, btw.billing);
            add_chat_spend(self.main_chat(), btw.cost, btw.billing);
            self.record_model_usage(
                &btw.provider,
                &btw.model,
                LedgerPurpose::Btw,
                btw.usage,
                btw.cost,
                btw.billing,
            );
            self.state
                .goal
                .record_external_usage(btw.usage, btw.cost, btw.billing);
        }
        dirty
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
                self.retry_info.is_some(),
                self.state.goal.snapshot().is_some(),
            ),
            self.selection_state
                .as_ref()
                .map_or(Cadence::IDLE, SelectionState::cadence),
            Cadence::when(self.autoscroll.is_some(), Cadence::SMOOTH),
            Cadence::any(self.chats.iter().map(Chat::cadence)),
            self.which_key.cadence(),
        ])
    }

    fn finish_subagents(&mut self, outcome: TaskOutcome, text: &str) {
        self.discard_all_pending_delegations();
        self.retain_resolved_subagents(outcome, text);
        self.chat_index.clear();
    }

    /// Terminalizes every tool left in progress when a turn ends, sparing
    /// shell commands that outlive the agent.
    fn terminalize_turn(&mut self, message: &str) {
        self.discard_all_pending_delegations();
        self.retain_resolved_subagents(TaskOutcome::Error, ERROR_TEXT);
        self.chats[0].fail_in_progress_except(message.into(), self.shell.active_ids());
        for chat in self.chats.iter_mut().skip(1) {
            chat.fail_in_progress_with_message(message.into());
        }
    }

    /// Marks unfinished subagent chats as ended and drops them from
    /// `chat_index`, so the session records only the children that really
    /// completed.
    fn retain_resolved_subagents(&mut self, outcome: TaskOutcome, text: &str) {
        self.chat_index.retain(|_, &mut sub_idx| {
            if self.chats[sub_idx].is_finished() {
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
        self.sync_subagent_input_target();
        if self.plan_form_active() {
            return;
        }
        if self.permission_prompt.handle_paste(text) {
            return;
        }
        if self.float_mgr.handle_paste(text) {
            return;
        }
        if self.logs_modal.is_open() {
            self.logs_modal.handle_paste(text);
            return;
        }
        if self.search_modal.is_open() {
            self.search_modal.handle_paste(text);
            let chat = &mut self.chats[self.active_chat];
            let texts = chat.segment_search_texts();
            self.search_modal.update_matches(&texts);
            sync_search_highlight(&self.search_modal, chat);
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
        try_picker!(self.command_modal);
        try_picker!(self.theme_picker);
        try_picker!(self.prompt_profile_picker);
        try_picker!(self.model_picker);
        try_picker!(self.thinking_picker);
        try_picker!(self.mcp_picker);
        try_picker!(self.permissions_picker);
        try_picker!(self.stash_picker);
        try_picker!(self.memory_picker);
        try_picker!(self.task_picker);
        if let Some(action) = self.workflow_inspector.handle_paste(text) {
            self.handle_workflow_inspector_action(action);
            return;
        }
        try_picker!(self.workflow_catalog_picker);
        try_picker!(self.session_picker);
        try_picker!(self.session_relocation_picker);
        try_picker!(self.question_form);
        try_picker!(self.login_picker);
        if !self.is_main_chat() && !(self.active_subagent_can_steer() || self.queue_editor_active())
        {
            return;
        }
        self.key_focus = KeyFocus::Composer;
        if let InputAction::PaletteSync(val) = self.active_input_box_mut().handle_paste(text) {
            self.sync_dropdowns(&val);
        }
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

    /// What the external editor opens on, paired with
    /// [`App::apply_external_input`] so a round trip cannot read one composer
    /// and write another.
    pub(crate) fn active_input_text(&self) -> String {
        self.active_input_box().expanded_text()
    }

    pub(crate) fn apply_external_input(&mut self, previous: &str, edited: String) {
        let edited = edited.replace("\r\n", "\n").replace('\r', "\n");
        if edited == previous {
            return;
        }
        let input = self.active_input_box_mut();
        input.set_input(edited);
        input.move_to_end();
        self.resync_dropdowns();
    }

    fn handle_plan_form_action(&mut self, action: PlanFormAction) -> Vec<Action> {
        match action {
            PlanFormAction::Consumed | PlanFormAction::Passthrough => vec![],
            PlanFormAction::Hide => {
                self.plan_form.hide();
                vec![]
            }
            PlanFormAction::OpenEditor => match self.state.plan.path() {
                Some(p) => vec![Action::OpenEditor(p.to_path_buf())],
                None => {
                    self.flash(FLASH_NO_PLAN.into());
                    vec![]
                }
            },
            PlanFormAction::Implement => self.implement_plan(false),
            PlanFormAction::ClearAndImplement => self.implement_plan(true),
        }
    }

    fn implement_plan(&mut self, clear_context: bool) -> Vec<Action> {
        let parallel = self.plan_form.parallel();
        self.plan_form.reset();
        let plan_snapshot = match std::mem::take(&mut self.state.plan) {
            PlanState::Ready(p) => Some((
                std::fs::read_to_string(&p).unwrap_or_default(),
                p.display().to_string(),
            )),
            PlanState::RemoteReady(reference) => self
                .workspace_session
                .as_ref()
                .zip(self.local_documents.as_ref())
                .and_then(|(workspace, store)| {
                    store
                        .read(
                            workspace.binding().project().key(),
                            Some(&self.state.session.id.to_string()),
                            &caudra_workspace::LocalDocumentRef::Plan(reference.clone()),
                        )
                        .ok()
                })
                .map(|document| {
                    (
                        document.content,
                        format!("plan reference {}", reference.as_str()),
                    )
                }),
            _ => None,
        };

        self.state.mode = Mode::Build;

        let mut actions = if clear_context {
            vec![Action::RequestNewSession]
        } else {
            vec![]
        };

        let text = if let Some((content, path_str)) = plan_snapshot {
            let text = if parallel {
                format!("{IMPLEMENT_MSG_PREFIX} at `{path_str}`. {IMPLEMENT_PARALLEL_HINT}")
            } else {
                format!("{IMPLEMENT_MSG_PREFIX} at `{path_str}`.")
            };
            self.main_chat()
                .push(DisplayMessage::plan(content, path_str));
            text
        } else {
            format!("{}.", IMPLEMENT_MSG_PREFIX)
        };
        let msg = QueuedMessage {
            text,
            images: vec![],
            mentions: Vec::new(),
            paste_ranges: Vec::new(),
        };
        actions.extend(self.start_from_queue(&msg));
        actions
    }
}

fn remote_workbench_gate(baseline: Arc<WorkspaceBaseline>) -> MutationGate {
    MutationGate::new(move || {
        let baseline = Arc::clone(&baseline);
        Box::pin(async move {
            match baseline.ensure_current().await {
                caudra_agent::BaselineOutcome::Ready => Ok(()),
                caudra_agent::BaselineOutcome::Unavailable(reason) => Err(reason.to_string()),
                caudra_agent::BaselineOutcome::Failed(error) => Err(error.to_string()),
            }
        })
    })
}

/// The key `/usage` groups a session's spend under: always the full spec, so a
/// row names whoever served it even after the conversation moves to another
/// provider, and two providers serving the same model id stay apart. The modal
/// is what drops the prefix when it matches the session's own provider, so the
/// common case still reads as the model alone.
fn session_usage_model(provider: &str, model: &str) -> String {
    format!("{provider}/{model}")
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

fn sync_search_highlight(modal: &SearchModal, chat: &mut Chat) {
    let idx = modal.current_segment_index();
    if let Some(i) = idx {
        chat.scroll_to_segment(i);
    }
    chat.set_highlight_segment(idx);
}

fn format_with_images(text: &str, image_count: usize) -> String {
    match image_count {
        0 => text.to_string(),
        1 => format!("{text} [1 image]"),
        n => format!("{text} [{n} images]"),
    }
}
