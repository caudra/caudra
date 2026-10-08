//! Multi-session supervisor: every session owns an `App` + `AgentHandles` and
//! keeps draining agent events while backgrounded; only the focused session
//! renders and receives input. `SpawnCtx` carries the shared resources needed
//! to spawn session runtimes at any point.
//!
//! Terminal input arrives on a channel (see [`InputReader`]), so the loop
//! waits on every event source at once and wakes the moment a plugin action,
//! agent event, or keypress arrives instead of sleeping in `event::poll`.

use std::borrow::Cow;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod peers;

use arc_swap::{ArcSwap, ArcSwapOption};
use caudra_agent::peers::PeerHost;
use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};
use peers::PeerRegistration;

use crate::sandbox::{
    LiveOperation, NETWORK_RECOVERY, NetworkGate, SandboxAttachment, SandboxConnector,
    SandboxControl, SandboxReadiness,
};
use caudra_agent::automation::frontend::wake_origin;
use caudra_agent::background::BackgroundTransition;
use caudra_agent::command::CustomCommand;
use caudra_agent::herdr::{HerdrEnv, resume_command_line};
use caudra_agent::permissions::PermissionManager;
use caudra_agent::prompt::profile::{
    BUILTIN_PROFILE_NAME, PromptProfileCatalog, SystemPromptProfile,
};
use caudra_agent::tools::ToolRegistry;
use caudra_agent::workflow::WorkflowTransition;
use caudra_agent::worktree::git::Git;
use caudra_agent::worktree::{Backend, Request as WorktreeRequest, counterpart, label};
use caudra_agent::{
    AgentConfig, AgentEvent, CancelToken, DoneReason, Envelope, McpCommand, McpConfigErrors,
    McpHandle, mcp,
};
use caudra_automation::request::ProfileArming;
use caudra_automation::snapshot::{AutomationEvent, SettleBlocker};
use caudra_config::providers::ProvidersConfig;
use caudra_config::sandbox::SandboxName;
use caudra_config::{
    AutomationsConfig, Feature, ModelPolicy, SnapshotsConfig, UiConfig, load_permissions,
};
use caudra_docs::DocsLibrary;
use caudra_lua::{
    EventHandle, HintReader, KeymapReader, LuaCommandReader, ModelRequest, SessionRequest,
    TaskRequest, UiAction, UiReply,
};
use caudra_providers::Timeouts;
use caudra_providers::manifest::ManifestRegistry;
use caudra_providers::provider::{Provider, fetch_all_models, from_model};
use caudra_providers::{HistoryItem, Message, Model};
use caudra_storage::StateDir;
use caudra_storage::StorageError;
use caudra_storage::checkout;
use caudra_storage::id::{CaudraId, CaudraIdParseError, SessionRef};
use caudra_storage::remote_operation_journal::RemoteOperationJournal;
use caudra_storage::sessions::change_stores::{registered_change_stores, store_summaries};
use caudra_storage::sessions::{
    SessionDatabase, SessionError, SessionLease, SessionLocation, SessionRelocation, StoredImage,
    StoredMode, TitleSource, normalize_title,
};
use caudra_storage::state::WorkspaceTabs;
use caudra_storage::workflow::WorkflowRunStatus;
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_storage::worktrees;
use caudra_workbench::WorkbenchAction;
use caudra_workspace::WorkspaceControlCommand;
use crossterm::event::KeyEventKind;
use crossterm::event::{
    Event, KeyModifiers, MouseButton, MouseEvent as CtMouseEvent, MouseEventKind,
};
use serde_json::json;
use tracing::{info, warn};

use crate::agent::{AgentCommand, AgentHandles, ModelSlot, shared_queue::QueueItem};
use crate::app::background_delivery::DeliveryReply;
use crate::app::file_revert::{self, RecorderSlot};
use crate::app::permission_editor::{ConversationPermissions, attach_session_permissions};
use crate::app::shell::{
    RemoteShellTarget, ShellEvent, spawn_remote_cd, spawn_remote_control, spawn_remote_shell,
    spawn_shell,
};
use crate::app::tasks::{TaskStatus, diff_task_states};
use crate::app::{
    App, Msg, Notification, QueuedMessage, SubmitOutcome, session_has_content, turn_response,
};
use crate::appearance::{self, AutoSwitch, Observation};
use crate::color_compat;
use crate::components::input::Submission;
use crate::components::session_picker::{SessionActivity, SessionRow};
use crate::components::storage_modal::{StorageFetchState, StorageReport};
use crate::components::usage_modal::UsageFetchState;
use crate::components::worktree_picker::{WorktreeOverview, WorktreeView};
use crate::components::{Action, ExitRequest, ForkDraft, ForkedSession, PlanHandoff, Status};
use crate::herdr::{
    HerdrObservation, HerdrReporterHandle, HerdrResume, HerdrStatus, aggregate_observations,
};
use crate::input::InputReader;
use crate::repaint::{Dirty, FrameLimiter, IDLE_POLL};
use crate::theme;
use crate::{
    AppSession, ChangeServiceFactory, PatternSuggestionLoader, PermissionAuthorityFactory,
    SessionRelocationHandoff, SessionTab,
};
use crate::{load_app_session, open_app_session_with_cursor};

use crate::storage_writer::StorageWriter;
use crate::terminal::{self, ProgramBlockKind, ProgramStatus, ProgramStatusReporter};

/// Max events handled per frame so a flood cannot starve rendering.
const DRAIN_BUDGET: usize = 256;
/// How much further one notch of an Alt-held wheel carries.
const FAST_SCROLL_FACTOR: u32 = 4;
/// One row of finger travel moves the content one row.
const TOUCH_SCROLL_LINES: u32 = 1;
const AGENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const DELETE_FOCUSED_ERR: &str = "cannot delete the focused session";
const DELETE_BUSY_ERR: &str = "wait for the session to become idle before deleting it";
const MODEL_POLICY_ERR: &str = "Model is not allowed by policy";
const INVALID_MODEL_ERR: &str = "Invalid model";
const PROVIDER_INIT_ERR: &str = "Failed to create provider";
const NOT_LIVE_ERR: &str = "session not live";
const CWD_BUSY_ERR: &str = "Wait for all sessions to become idle before changing directory";
const CWD_REVERT_ERR: &str = "Resolve pending reverts before changing directory";
const NOTHING_TO_NAME_ERR: &str = "Nothing said yet, so there is nothing to name";
const NO_TITLE_ERR: &str = "The model returned nothing usable";
const NAMING_SESSION: &str = "Naming the session…";
const UNBOUND_FLASH: &str = "default";
const RELOCATION_LOCAL_ERR: &str = "Session relocation requires persistent local sessions";
const RELOCATION_WORKBENCH_ERR: &str =
    "Save workbench buffers and wait for workbench operations before moving sessions";
const RELOCATION_CHANGED_ERR: &str = "Session selection changed; reopen the relocation picker";
const RELOCATION_WORKFLOW_ERR: &str = "Stop active or paused workflows before moving sessions";
const RELOCATION_SHUTDOWN_ERR: &str = "Session relocation aborted before changing directories";
const RELOCATION_ADMISSION_ERR: &str =
    "Session relocation is in progress; retry after the workspace restarts";
const SANDBOX_WORK_BUSY: &str =
    "Wait for agents, queues, background tasks and shells before a sandbox action";
const WORKTREE_REPOSITORY_ERR: &str =
    "Worktrees need a git repository, and this session works outside one";
const SESSION_OPEN_ELSEWHERE: &str =
    "Another Caudra has that session open, so only its workspace was focused";
const NO_CHANGE_STORES: &str = "The file change record stores could not be opened";
const ANTHROPIC_LOGIN_ARGS: [&str; 5] = ["auth", "login", "anthropic", "--method", "oauth"];
const AUTH_EXECUTABLE_ERR: &str = "could not locate the Caudra executable for login";
const AUTH_PROCESS_ERR: &str = "could not run the OAuth login process";
const AUTH_EXIT_ERR: &str = "OAuth login process failed";

fn anthropic_login_command() -> Result<Command> {
    let mut command = Command::new(env::current_exe().wrap_err(AUTH_EXECUTABLE_ERR)?);
    command
        .args(ANTHROPIC_LOGIN_ARGS)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    Ok(command)
}

fn run_oauth_login(mut command: Command) -> Result<()> {
    let status = command.status().wrap_err(AUTH_PROCESS_ERR)?;
    if !status.success() {
        return Err(eyre!("{AUTH_EXIT_ERR}: {status}"));
    }
    Ok(())
}

/// The prompt that opened a session, which is what its title is about.
fn opening_prompt<M: TitleSource>(messages: &[M]) -> Result<String, String> {
    messages
        .iter()
        .find_map(TitleSource::first_user_text)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| NOTHING_TO_NAME_ERR.to_owned())
}

/// Tabs carry their in-memory sessions so `/reload` reopens them without a
/// disk round-trip; `session_has_content` tells which ones were saved.
pub(crate) struct ShutdownReport {
    pub exit: ExitRequest,
    pub tabs: Vec<SessionTab>,
    pub focused: usize,
    /// This generation only: `/reload` builds a new loop, so a reloaded run
    /// reports the time since the reload rather than since launch.
    pub run_time: Duration,
    pub relocation: Option<Handoff>,
    pub sandbox: Option<SandboxAttachment>,
    pub sandbox_control: Option<SandboxControl>,
}

pub struct EventLoopParams {
    pub peer_host: Option<Arc<PeerHost>>,
    pub model: Model,
    pub needs_login: bool,
    pub commands: Vec<CustomCommand>,
    pub no_commands: bool,
    pub sessions: Vec<SessionTab>,
    pub focused: usize,
    pub startup_warnings: Vec<String>,
    /// The launch joined the focused session to consumer groups, so it warns
    /// once registered if its inbound policy skips some of their work.
    pub joined_groups: bool,
    /// `--automation` entries, armed in the session focused at launch only.
    pub launch_automations: Vec<ProfileArming>,
    pub storage: StateDir,
    pub config: AgentConfig,
    pub automations: AutomationsConfig,
    pub ui_config: UiConfig,
    pub snapshots: SnapshotsConfig,
    /// Opens the change records of a local session's directory.
    pub change_factory: Option<ChangeServiceFactory>,
    pub allow_workspace_recovery: bool,
    pub input_history_size: usize,
    pub max_log_files: u32,
    pub docs: DocsLibrary,
    pub permissions: Arc<PermissionManager>,
    pub pattern_suggestion_loader: Option<PatternSuggestionLoader>,
    pub permission_authority_factory: Option<PermissionAuthorityFactory>,
    pub sandbox_connector: Option<SandboxConnector>,
    pub transfer_connector: Option<crate::sandbox::transfer::TransferConnector>,
    pub initial_seed: Option<crate::sandbox::InitialSeed>,
    pub sandbox_readiness: Option<SandboxReadiness>,
    /// The sandbox the runtime was built on. Carried rather than looked up,
    /// because the selection that built the runtime already named it and the
    /// binding a session stores keeps only an opaque record id.
    pub sandbox_name: Option<SandboxName>,
    pub timeouts: Timeouts,
    pub exit_on_done: bool,
    pub lua_command_reader: LuaCommandReader,
    pub keymap_reader: KeymapReader,
    pub hint_reader: HintReader,
    pub ui_action_rx: flume::Receiver<UiAction>,
    pub lua_event_handle: EventHandle,
    pub model_policy: Arc<ModelPolicy>,
    pub prompt_profiles: Arc<PromptProfileCatalog>,
    pub default_prompt_profile: Option<Arc<SystemPromptProfile>>,
    pub prompt_profile_override: Option<String>,
    pub herdr_reporter: Option<HerdrReporterHandle>,
    pub worktrees: Backend,
    pub workspace_session: Option<caudra_workspace::WorkspaceSession>,
    pub remote_project_context:
        Option<Arc<caudra_agent::remote_project_context::RemoteProjectContext>>,
    pub local_documents: Option<Arc<caudra_storage::local_documents::LocalDocumentStore>>,
}

const NEEDS_INPUT_MARK: &str = "\u{25c6}";
const FINISHED_MARK: &str = "\u{2713}";
const SESSIONS_COMMAND: &str = "/sessions";

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionStatus {
    Working,
    NeedsInput,
    Idle,
}

impl SessionStatus {
    fn activity(self) -> SessionActivity {
        match self {
            Self::Working => SessionActivity::Working,
            Self::NeedsInput => SessionActivity::NeedsInput,
            Self::Idle => SessionActivity::Idle,
        }
    }
}

enum PendingCompletion {
    WaitingForQueueDrain(Notification),
    Due(Notification),
}

#[derive(Default)]
struct RunNotificationState {
    response_candidate: Option<String>,
    pending_completion: Option<PendingCompletion>,
    outcome: Option<ProgramStatus>,
    last_attention: Option<Notification>,
    /// The newest `notify()` text since the last reconcile.
    pending_notice: Option<Notification>,
}

impl RunNotificationState {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn on_queue_item_consumed(&mut self) {
        self.response_candidate = None;
        self.pending_completion = None;
        self.outcome = None;
    }

    fn on_turn_complete(&mut self, message: &Message) {
        self.response_candidate = turn_response(message);
    }

    fn on_done(&mut self, event: &AgentEvent) {
        let (notification, outcome) = match event {
            AgentEvent::Done { reason, .. } => (
                Notification::TurnComplete {
                    response: self.response_candidate.take(),
                },
                match reason {
                    DoneReason::EndTurn => Some(ProgramStatus::Done),
                    DoneReason::Cancelled => None,
                    DoneReason::MaxTurns | DoneReason::MaxTokens => Some(ProgramStatus::Error),
                },
            ),
            AgentEvent::Error { .. } => {
                self.response_candidate = None;
                (Notification::error_completion(), Some(ProgramStatus::Error))
            }
            _ => return,
        };
        self.outcome = outcome;
        self.pending_completion = Some(PendingCompletion::WaitingForQueueDrain(notification));
    }

    fn on_drain(&mut self) {
        self.pending_completion = match self.pending_completion.take() {
            Some(PendingCompletion::WaitingForQueueDrain(notification)) => {
                Some(PendingCompletion::Due(notification))
            }
            pending => pending,
        };
    }

    fn on_manual_exit(&mut self) {
        self.pending_completion = None;
        self.outcome = None;
    }

    fn program_status(&self, status: ProgramStatus, busy: bool) -> ProgramStatus {
        if matches!(status, ProgramStatus::Blocked(_)) {
            status
        } else if status == ProgramStatus::Working || busy {
            ProgramStatus::Working
        } else {
            self.outcome.unwrap_or(ProgramStatus::Idle)
        }
    }

    fn reconcile_program_status(&mut self, terminal_focused: bool) -> Option<Notification> {
        let notice = self.pending_notice.take();
        self.reconcile(None, SessionStatus::Idle, true, true);
        (!terminal_focused).then_some(notice).flatten()
    }

    /// Only a firing's `notify()` reaches the terminal. An arming refusal carries error details,
    /// which notifications leave out.
    fn on_automation_event(&mut self, event: &AutomationEvent) {
        if let AutomationEvent::Notice {
            automation,
            fire_id: Some(_),
            text,
        } = event
        {
            self.pending_notice = Some(Notification::automation_notice(automation, text));
        }
    }

    /// True between `Done`/`Error` and the run's `QueueDrained`. An exit must
    /// not fire in that window: a queued follow-up may still start a new run.
    fn waiting_for_drain(&self) -> bool {
        matches!(
            self.pending_completion,
            Some(PendingCompletion::WaitingForQueueDrain(_))
        )
    }

    fn reconcile(
        &mut self,
        attention: Option<Notification>,
        status: SessionStatus,
        queue_empty: bool,
        terminal_focused: bool,
    ) -> Option<Notification> {
        let settled = attention.is_none() && status == SessionStatus::Idle && queue_empty;
        let prompt = (attention != self.last_attention)
            .then(|| attention.clone())
            .flatten();
        self.last_attention = attention;

        // A due completion is decided on its first reconcile: fire if the
        // session settled, otherwise drop it for good.
        let completion = match self.pending_completion.take() {
            Some(PendingCompletion::Due(notification)) => settled.then_some(notification),
            waiting => {
                self.pending_completion = waiting;
                None
            }
        };
        let notice = self.pending_notice.take();
        (!terminal_focused)
            .then(|| prompt.or(notice).or(completion))
            .flatten()
    }
}

impl SessionStatus {
    fn of(app: &App) -> Self {
        if app.awaiting_input() {
            Self::NeedsInput
        } else if app.status == Status::Streaming || app.waiting_for_background() {
            Self::Working
        } else {
            Self::Idle
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::NeedsInput => "needs_input",
            Self::Idle => "idle",
        }
    }
}

fn prepend_preamble(preamble: &mut Vec<Message>, mut leading: Vec<Message>) {
    leading.append(preamble);
    *preamble = leading;
}

fn is_current_top_level(current_run_id: u64, envelope: &Envelope) -> bool {
    envelope.run_id == current_run_id && envelope.subagent.is_none()
}

/// The `--automation` entries each launch tab arms: all of them in the tab that takes focus,
/// clamped the way the loop clamps it, and none elsewhere.
fn launch_armings(
    tabs: usize,
    focused: usize,
    mut automations: Vec<ProfileArming>,
) -> Vec<Vec<ProfileArming>> {
    let focused = focused.min(tabs.saturating_sub(1));
    (0..tabs)
        .map(|index| {
            if index == focused {
                std::mem::take(&mut automations)
            } else {
                Vec::new()
            }
        })
        .collect()
}

fn select_notification(
    selected: Option<Notification>,
    candidate: Option<Notification>,
) -> Option<Notification> {
    match (selected, candidate) {
        (Some(current), Some(candidate)) if candidate.is_urgent() && !current.is_urgent() => {
            Some(candidate)
        }
        (selected @ Some(_), _) => selected,
        (None, candidate) => candidate,
    }
}

#[cfg(not(windows))]
fn terminal_focus_event(event: &Event) -> Option<bool> {
    match event {
        Event::FocusGained => Some(true),
        Event::FocusLost => Some(false),
        _ => None,
    }
}

#[cfg(windows)]
fn terminal_focus_event(_event: &Event) -> Option<bool> {
    None
}

#[cfg(not(windows))]
fn terminal_input_proves_focus(event: &Event) -> bool {
    match event {
        Event::Key(key) => key.kind == KeyEventKind::Press,
        Event::Paste(_) | Event::Mouse(_) => true,
        _ => false,
    }
}

#[cfg(windows)]
fn terminal_input_proves_focus(_event: &Event) -> bool {
    false
}

fn parse_session_id(id: &str) -> Result<CaudraId, String> {
    id.parse().map_err(|e: CaudraIdParseError| e.to_string())
}

struct SessionRuntime {
    peer: Option<PeerRegistration>,
    app: App,
    lease: Arc<SessionLease>,
    handles: AgentHandles,
    shell_tx: flume::Sender<ShellEvent>,
    shell_rx: flume::Receiver<ShellEvent>,
    last_status: SessionStatus,
    /// Keyed by task id, never by position: a session reset reuses positions,
    /// so a new task would inherit the old one's status.
    last_tasks: Vec<(Arc<str>, TaskStatus)>,
    notifications: RunNotificationState,
    restore_transitions: Vec<SessionTransition>,
}

impl SessionRuntime {
    fn id(&self) -> CaudraId {
        self.app.state.session.id
    }

    /// New work cancels an `exit_on_done` exit still waiting on its drain.
    fn reset_run_notifications(&mut self) {
        if self.notifications.waiting_for_drain() {
            self.app.clear_exit_request();
        }
        self.notifications.reset();
    }

    /// A wake may only start a background run when the session is fully
    /// quiescent. Idle status alone is not enough: restored queue items start
    /// runs without `start_run` (the app only learns of them via
    /// `QueueItemConsumed`), and `start_run` destroys text held for recovery
    /// after an agent error.
    fn quiescent(&self) -> bool {
        SessionStatus::of(&self.app) == SessionStatus::Idle && self.work_quiescent()
    }

    fn work_quiescent(&self) -> bool {
        self.handles.queue.is_empty() && self.running_quiescent()
    }

    /// Nothing runs or waits to report back, whatever the queue holds.
    fn running_quiescent(&self) -> bool {
        self.parent_ready()
            && !self.app.has_session_work()
            && self.handles.active_background_tasks() == 0
            && self
                .handles
                .background
                .as_ref()
                .is_none_or(|background| !background.has_pending())
            && self.app.background_claims.is_empty()
            && !self.app.background_delivery.pending()
            && self.app.shell.active_ids().is_empty()
    }

    fn shutdown_quiescent(&self) -> bool {
        SessionStatus::of(&self.app) == SessionStatus::Idle
            && self.handles.active_background_tasks() == 0
            && self.app.shell.active_ids().is_empty()
            && self.handles.agent_rx.is_empty()
            && self.shell_rx.is_empty()
    }

    fn delivery_idle(&self) -> bool {
        !self.handles.queue.has_priority_input() && !self.handles.queue.is_processing()
    }

    fn parent_ready(&self) -> bool {
        self.delivery_idle() && !self.app.holds_recovery_text()
    }

    /// What keeps the session from settling, as its automations see it; empty once it has.
    fn settle_blockers(&self) -> Vec<SettleBlocker> {
        let status = SessionStatus::of(&self.app);
        let input = (status == SessionStatus::NeedsInput)
            .then(|| self.app.input_wait(false))
            .flatten();
        [
            (status == SessionStatus::Working || !self.running_quiescent())
                .then_some(SettleBlocker::Busy),
            input.map(|wait| SettleBlocker::NeedsInput(wait.input)),
            (!self.handles.queue.is_empty()).then_some(SettleBlocker::PromptQueued),
            self.peer_work_pending()
                .then_some(SettleBlocker::PeerMessages),
            self.handles
                .mailbox_wake_pending()
                .then_some(SettleBlocker::MailboxWake),
            self.app
                .goal_checkin_due()
                .then_some(SettleBlocker::GoalCheckin),
            (!self.handles.agent_rx.is_empty()).then_some(SettleBlocker::AgentEvents),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// Tells the session's automations what this tick saw, then claims the turn its next `next`
    /// delivery asks for once nothing waits ahead of it. The claim marks the item delivered, so
    /// it comes only after every check that could stop the turn.
    fn sync_automations(&mut self) -> Option<Vec<Action>> {
        if !self.app.has_automations() {
            return None;
        }
        self.sync_peer_facts();
        let blockers = self.settle_blockers();
        let held_messages = self.holds_peer_messages();
        let name = self.messaging_name();
        self.app.observe_automations(held_messages, name, blockers);
        if !self.parent_ready()
            || crate::sandbox::transfer::active()
            || !self.app.automation_delivery_due()
        {
            return None;
        }
        self.app.deliver_automation_item()
    }

    fn herdr_observation(&self) -> HerdrObservation {
        runtime_observation(
            self.app.lifecycle_blocker(),
            self.app.has_lifecycle_work(),
            self.handles.queue.is_empty(),
            self.handles.queue.is_processing(),
            self.handles.active_background_tasks(),
            self.app.shell.active_ids().len(),
        )
    }

    fn program_status(&self) -> ProgramStatus {
        self.program_status_with_agent_stopped(
            self.handles.is_finished() && self.handles.agent_rx.is_empty(),
        )
    }

    fn program_status_with_agent_stopped(&self, agent_stopped: bool) -> ProgramStatus {
        let status = if self.holds_peer_messages() {
            ProgramStatus::Blocked(Some(ProgramBlockKind::Permission))
        } else {
            self.app.program_status()
        };
        self.notifications.program_status(
            status,
            runtime_busy(
                self.app.has_session_work(),
                agent_stopped || self.handles.queue.is_empty(),
                !agent_stopped && self.handles.queue.is_processing(),
                self.handles.active_background_tasks(),
                self.app.shell.active_ids().len(),
            ) || self
                .handles
                .background
                .as_ref()
                .is_some_and(|background| background.has_pending())
                || !self.app.background_claims.is_empty()
                || self.app.background_delivery.pending()
                || (self.notifications.waiting_for_drain() && !agent_stopped),
        )
    }
}

fn aggregate_program_status(statuses: impl IntoIterator<Item = ProgramStatus>) -> ProgramStatus {
    statuses
        .into_iter()
        .fold(ProgramStatus::Idle, |selected, candidate| {
            let priority = |status| match status {
                ProgramStatus::Idle => 0,
                ProgramStatus::Done => 1,
                ProgramStatus::Error => 2,
                ProgramStatus::Working => 3,
                ProgramStatus::Blocked(_) => 4,
            };
            if priority(candidate) > priority(selected) {
                candidate
            } else {
                selected
            }
        })
}

fn acknowledges_program_status(event: &Event) -> bool {
    match event {
        Event::Key(key) => key.kind != KeyEventKind::Release,
        Event::Paste(_) => true,
        Event::Mouse(mouse) => matches!(
            mouse.kind,
            MouseEventKind::Down(_) | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ),
        _ => false,
    }
}

fn exit_program_status(status: ProgramStatus, exit_on_done: bool, failed: bool) -> ProgramStatus {
    if failed {
        ProgramStatus::Error
    } else if exit_on_done && matches!(status, ProgramStatus::Done | ProgramStatus::Error) {
        status
    } else {
        ProgramStatus::Idle
    }
}

fn runtime_busy(
    app_working: bool,
    queue_empty: bool,
    queue_processing: bool,
    background_tasks: usize,
    shell_commands: usize,
) -> bool {
    app_working || !queue_empty || queue_processing || background_tasks > 0 || shell_commands > 0
}

fn runtime_observation(
    blocker: Option<Cow<'static, str>>,
    app_working: bool,
    queue_empty: bool,
    queue_processing: bool,
    background_tasks: usize,
    shell_commands: usize,
) -> HerdrObservation {
    if let Some(message) = blocker {
        HerdrObservation::blocked(message)
    } else if runtime_busy(
        app_working,
        queue_empty,
        queue_processing,
        background_tasks,
        shell_commands,
    ) {
        HerdrObservation::working()
    } else {
        HerdrObservation::idle()
    }
}

#[cfg(test)]
fn runtime_state_quiescent(
    status: SessionStatus,
    queue_empty: bool,
    background_tasks: usize,
    shell_commands: usize,
    holds_recovery_text: bool,
) -> bool {
    status == SessionStatus::Idle
        && queue_empty
        && background_tasks == 0
        && shell_commands == 0
        && !holds_recovery_text
}

fn cwd_change_blocker(states: impl IntoIterator<Item = (bool, bool)>) -> Option<&'static str> {
    let mut pending_revert = false;
    for (quiescent, has_pending_revert) in states {
        if !quiescent {
            return Some(CWD_BUSY_ERR);
        }
        pending_revert |= has_pending_revert;
    }
    pending_revert.then_some(CWD_REVERT_ERR)
}

fn relocation_inventory<'a>(
    storage: &StateDir,
    sessions: impl IntoIterator<Item = &'a AppSession>,
) -> Result<Vec<SessionLocation>, String> {
    let mut locations = SessionDatabase::open_state(storage)
        .and_then(|database| database.local_session_locations())
        .map_err(|error| error.to_string())?;
    for session in sessions {
        if !locations.iter().any(|location| location.id == session.id) {
            locations.push(SessionLocation {
                id: session.id,
                title: session.title.clone(),
                cwd: session.cwd.clone(),
                updated_at: session.updated_at,
                write_version: session.clone().persisted_write_version().unwrap_or(0),
            });
        }
    }
    Ok(locations)
}

fn validate_relocation_selection(
    request: &SessionRelocation,
    current: &[SessionLocation],
    donor: Option<&(CaudraId, String)>,
) -> Result<(), String> {
    let ids: HashSet<_> = request.sessions.iter().map(|entry| entry.id).collect();
    if ids.is_empty() || ids.len() != request.sessions.len() {
        return Err(RELOCATION_CHANGED_ERR.into());
    }
    for expected in &request.sessions {
        if !current.iter().any(|entry| {
            entry.id == expected.id
                && entry.cwd == expected.cwd
                && entry.write_version == expected.write_version
        }) {
            return Err(RELOCATION_CHANGED_ERR.into());
        }
    }
    if let Some(source) = &request.source_cwd {
        let members: HashSet<_> = current
            .iter()
            .filter(|entry| &entry.cwd == source)
            .map(|entry| entry.id)
            .collect();
        if members != ids {
            return Err(RELOCATION_CHANGED_ERR.into());
        }
    }
    if let Some((id, cwd)) = donor
        && !current
            .iter()
            .any(|entry| entry.id == *id && entry.cwd == *cwd)
    {
        return Err(RELOCATION_CHANGED_ERR.into());
    }
    Ok(())
}

fn reconcile_relocation_live_version(
    expected: &mut SessionLocation,
    session: &AppSession,
    current: &[SessionLocation],
) -> Result<(), String> {
    let committed = session.clone().persisted_write_version().unwrap_or(0);
    if session.id != expected.id
        || session.cwd != expected.cwd
        || committed < expected.write_version
        || !current.iter().any(|entry| {
            entry.id == expected.id && entry.cwd == expected.cwd && entry.write_version == committed
        })
    {
        return Err(RELOCATION_CHANGED_ERR.into());
    }
    expected.write_version = committed;
    Ok(())
}

fn check_relocation_destination(storage: &StateDir, destination: &Path) -> Result<(), String> {
    let database = SessionDatabase::open_state(storage).map_err(|error| error.to_string())?;
    let ids: HashSet<_> = database
        .local_session_locations()
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|entry| {
            Path::new(&entry.cwd)
                .canonicalize()
                .is_ok_and(|cwd| cwd == destination)
        })
        .map(|entry| entry.id)
        .collect();
    for facts in database
        .session_facts(None)
        .map_err(|error| error.to_string())?
    {
        if ids.contains(&facts.id) && facts.pending_revert {
            return Err(format!(
                "Destination session {} has a pending restore; resolve it before moving sessions",
                facts.id
            ));
        }
    }
    Ok(())
}

fn refuse_resumable_workflows(database: &SessionDatabase, id: CaudraId) -> Result<(), String> {
    let resumable = database
        .load_workflow_runs(id)
        .map_err(|error| error.to_string())?
        .iter()
        .any(|run| {
            matches!(
                run.status,
                WorkflowRunStatus::Active
                    | WorkflowRunStatus::Paused
                    | WorkflowRunStatus::BudgetLimited
            )
        });
    if resumable {
        return Err(format!("{RELOCATION_WORKFLOW_ERR}: {id}"));
    }
    Ok(())
}

/// Opens the checkout at `root` in a Herdr workspace grouped with its
/// repository's, or focuses the one it is open in.
fn open_in_herdr(herdr: &HerdrEnv, root: &Path) -> Result<String, String> {
    let checkout = checkout::discover(root).ok_or(WORKTREE_REPOSITORY_ERR)?;
    let opened = herdr
        .cli()
        .worktree_open(&checkout.main_root, &checkout.root)
        .map_err(|error| error.to_string())?;
    Ok(format!(
        "Opened {} in Herdr",
        label(opened.worktree.branch.as_deref(), &checkout.root)
    ))
}

/// Resumes session `id` in the Herdr workspace of the checkout `cwd` is in:
/// in its root pane when the workspace has just opened, else in a new pane
/// beside the focused one. A session another Caudra has open only has its
/// workspace focused.
fn resume_in_herdr(
    herdr: &HerdrEnv,
    storage: &StateDir,
    id: CaudraId,
    cwd: &Path,
) -> Result<String, String> {
    let checkout = checkout::discover(cwd).ok_or(WORKTREE_REPOSITORY_ERR)?;
    let cli = herdr.cli();
    let opened = cli
        .worktree_open(&checkout.main_root, &checkout.root)
        .map_err(|error| error.to_string())?;
    match SessionLease::acquire(storage, id) {
        Ok(_) => {}
        Err(SessionError::SessionInUse { .. }) => return Ok(SESSION_OPEN_ELSEWHERE.into()),
        Err(error) => return Err(format!("Failed to open session: {error}")),
    }
    let pane = if opened.workspace.already_open {
        cli.split_workspace(&opened.workspace.workspace_id, cwd)
            .map_err(|error| error.to_string())?
    } else {
        opened.workspace.pane_id
    };
    cli.pane_run(&pane, &resume_command_line(&id.to_string()))
        .map_err(|error| error.to_string())?;
    Ok(format!(
        "Resumed the session in {}",
        label(opened.worktree.branch.as_deref(), &checkout.root)
    ))
}

fn canonical_cwd(path: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(path).map_err(|error| {
        format!(
            "failed to resolve working directory {}: {error}",
            path.display()
        )
    })
}

fn matching_workspace_quiescent<'a>(
    target: &Path,
    sessions: impl IntoIterator<Item = (&'a Path, bool)>,
) -> bool {
    sessions
        .into_iter()
        .all(|(cwd, quiescent)| canonical_cwd(cwd).is_ok_and(|cwd| cwd != target || quiescent))
}

fn matching_remote_workspace_quiescent<'a>(
    target: &StoredWorkspaceBinding,
    sessions: impl IntoIterator<Item = (Option<&'a StoredWorkspaceBinding>, bool)>,
) -> bool {
    sessions.into_iter().all(|(binding, quiescent)| {
        binding.is_none_or(|binding| !binding.exact_scope_eq(target) || quiescent)
    })
}

fn validate_session_focus(
    session: &AppSession,
    workspace: Option<&caudra_workspace::WorkspaceSession>,
) -> Result<(), String> {
    if let Some(workspace) = workspace {
        let expected = StoredWorkspaceBinding::new_with_cursor(
            workspace.binding().clone(),
            workspace.cursor().clone(),
            session
                .workspace_binding()
                .and_then(|binding| binding.cursor_label().map(str::to_owned)),
        )
        .map_err(|error| error.to_string())?;
        return StoredWorkspaceBinding::validate_resume(
            session.workspace_binding(),
            Some(&expected),
        )
        .map_err(|error| error.to_string());
    }
    StoredWorkspaceBinding::validate_resume(session.workspace_binding(), None)
        .map_err(|error| error.to_string())?;
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    validate_session_cwd(session, &canonical_cwd(&cwd)?)
}

fn validate_session_cwd(session: &AppSession, process_cwd: &Path) -> Result<(), String> {
    StoredWorkspaceBinding::validate_resume(session.workspace_binding(), None)
        .map_err(|error| error.to_string())?;
    let session_cwd = canonical_cwd(Path::new(&session.cwd))?;
    if session_cwd == process_cwd {
        return Ok(());
    }
    Err(format!(
        "Session {} belongs to {}; current working directory is {}. Use /cd {} before opening it",
        session.id,
        session_cwd.display(),
        process_cwd.display(),
        session_cwd.display()
    ))
}

/// Blocking, and deliberately so: both halves are disk work. Reported together
/// because the question `/storage` answers is which of the two is spending the
/// disk, and one half without the other cannot answer it.
fn measure_storage(storage: &StateDir, unavailable: Option<String>) -> StorageFetchState {
    let measured = SessionDatabase::open_read_only(storage)
        .and_then(|database| Ok((database.stats()?, database.session_directories()?)));
    let (stats, sessions) = match measured {
        Ok(measured) => measured,
        Err(error) => return StorageFetchState::Error(error.to_string()),
    };
    let Some(stores) = registered_change_stores() else {
        return StorageFetchState::Error(NO_CHANGE_STORES.to_owned());
    };
    let stores = match store_summaries(stores.as_ref(), storage, &sessions) {
        Ok(stores) => stores,
        Err(error) => return StorageFetchState::Error(error.to_string()),
    };
    StorageFetchState::Ready(Box::new(StorageReport {
        stats,
        stores,
        unavailable,
    }))
}

fn prepare_session_for_runtime(
    mut session: AppSession,
    workspace: Option<&caudra_workspace::WorkspaceSession>,
    allow_workspace_recovery: bool,
) -> Result<AppSession, String> {
    validate_session_focus(&session, workspace)?;
    if let Some(workspace) = workspace {
        let binding = StoredWorkspaceBinding::new_with_cursor(
            workspace.binding().clone(),
            workspace.cursor().clone(),
            session
                .workspace_binding()
                .and_then(|binding| binding.cursor_label().map(str::to_owned)),
        )
        .map_err(|error| error.to_string())?;
        session
            .replace_workspace_cursor(binding)
            .map_err(|error| error.to_string())?;
        return Ok(session);
    }
    let process_cwd = canonical_cwd(
        &std::env::current_dir()
            .map_err(|error| format!("failed to read current directory: {error}"))?,
    )?;
    validate_session_cwd(&session, &process_cwd)?;
    if !allow_workspace_recovery && session.meta.pending_revert.is_some() {
        return Err(format!(
            "Session {} has a pending restore; resolve it before moving sessions",
            session.id
        ));
    }
    Ok(session)
}

/// Everything needed to bring up a new session runtime after startup.
struct SpawnCtx {
    peer_host: Option<Arc<PeerHost>>,
    storage: StateDir,
    background_enabled: bool,
    config: AgentConfig,
    automations: AutomationsConfig,
    ui_config: UiConfig,
    snapshots: SnapshotsConfig,
    change_factory: Option<ChangeServiceFactory>,
    allow_workspace_recovery: bool,
    input_history_size: usize,
    max_log_files: u32,
    docs: DocsLibrary,
    /// Prototype only: every runtime forks its own manager so session
    /// rules stay per-session.
    permissions: Arc<PermissionManager>,
    pattern_suggestion_loader: Option<PatternSuggestionLoader>,
    permission_authority_factory: Option<PermissionAuthorityFactory>,
    sandbox_connector: Option<SandboxConnector>,
    transfer_connector: Option<crate::sandbox::transfer::TransferConnector>,
    sandbox_readiness: Option<SandboxReadiness>,
    sandbox_name: Option<SandboxName>,
    network_gate: Arc<Mutex<NetworkGate>>,
    timeouts: Timeouts,
    custom_commands: Arc<[CustomCommand]>,
    no_commands: bool,
    lua_command_reader: LuaCommandReader,
    keymap_reader: KeymapReader,
    hint_reader: HintReader,
    lua_event_handle: EventHandle,
    mcp_handle: Option<McpHandle>,
    mcp_config_errors: McpConfigErrors,
    model_slot: Arc<ArcSwap<ModelSlot>>,
    available_models: Arc<ArcSwapOption<Vec<String>>>,
    storage_writer: Arc<StorageWriter>,
    model_policy: Arc<ModelPolicy>,
    prompt_profiles: Arc<PromptProfileCatalog>,
    default_prompt_profile: Option<Arc<SystemPromptProfile>>,
    prompt_profile_override: Option<String>,
    /// One slot shared by every runtime: an `App` can only see its own
    /// session, so the loop publishes the rest here for the picker to read.
    live_sessions: Arc<ArcSwap<Vec<SessionRow>>>,
    workspace_session: Option<caudra_workspace::WorkspaceSession>,
    /// The directory Caudra itself runs in, kept only for a sandbox session:
    /// there `{cwd}` names a path inside the VM, and the host checkout is a
    /// second, separate source of instruction files.
    host_cwd: Option<PathBuf>,
    local_documents: Option<Arc<caudra_storage::local_documents::LocalDocumentStore>>,
}

impl SpawnCtx {
    fn spawn_fresh_runtime(
        &self,
        current: &AppSession,
        mode: Option<StoredMode>,
    ) -> Result<SessionRuntime, String> {
        let mut session = current.workspace_binding().map_or_else(
            || AppSession::new(&current.model, &current.cwd),
            |binding| AppSession::new_with_workspace(&current.model, &current.cwd, binding.clone()),
        );
        session.meta.mode = mode;
        let lease = SessionLease::acquire(&self.storage, session.id)
            .map_err(|error| format!("Failed to reserve new session: {error}"))?;
        self.spawn_runtime(
            SessionTab {
                session,
                lease: Arc::new(lease),
                cursor: None,
            },
            Vec::new(),
        )
    }

    /// The session that implements a plan starts in Build, so it never makes
    /// a plan of its own before it takes the handed-over one.
    fn spawn_plan_runtime(&self, source: &App) -> Result<SessionRuntime, String> {
        source.check_run_admission()?;
        if source.permission_mutation_pending() {
            return Err(crate::app::permission_editor::PERMISSION_WORKER_BUSY.into());
        }
        let mut runtime =
            self.spawn_fresh_runtime(&source.state.session, Some(StoredMode::Build))?;
        if let Err(error) = runtime.app.admit_run() {
            runtime.app.discard_unstarted_session(runtime.id());
            runtime.handles.cancel();
            return Err(error);
        }
        Ok(runtime)
    }

    fn resolve_prompt_profile(
        &self,
        session: &AppSession,
    ) -> Result<(String, Option<Arc<SystemPromptProfile>>), String> {
        let requested_name = self
            .prompt_profile_override
            .as_deref()
            .or(session.meta.system_prompt_profile.as_deref())
            .or_else(|| {
                self.default_prompt_profile
                    .as_deref()
                    .map(SystemPromptProfile::name)
            });
        let profile = self.resolve_bound_profile(requested_name)?;
        Ok((
            requested_name.unwrap_or(BUILTIN_PROFILE_NAME).to_owned(),
            profile,
        ))
    }

    fn resolve_bound_profile(
        &self,
        name: Option<&str>,
    ) -> Result<Option<Arc<SystemPromptProfile>>, String> {
        let profile = self
            .prompt_profiles
            .resolve(name)
            .map_err(|error| error.to_string())?;
        if let Some(profile) = &profile {
            profile
                .tools()
                .validate_bindings(ToolRegistry::global().names().iter().map(AsRef::as_ref))
                .map_err(|error| format!("System prompt profile {:?}: {error}", profile.name()))?;
        }
        Ok(profile)
    }

    /// `automations` are `--automation` entries, which only the session focused at launch takes.
    fn spawn_runtime(
        &self,
        tab: SessionTab,
        automations: Vec<ProfileArming>,
    ) -> Result<SessionRuntime, String> {
        let started = Instant::now();
        let mut phase_start = started;
        let mut lap = || {
            let elapsed = phase_start.elapsed().as_millis() as u64;
            phase_start = Instant::now();
            elapsed
        };
        let SessionTab {
            mut session,
            lease,
            cursor,
        } = tab;
        // Before anything writes this session back. The first save otherwise
        // finds no cursor and rewrites every payload the session holds.
        if let Some(cursor) = cursor {
            self.storage_writer.adopt_cursor(cursor);
        }
        lease
            .validate(&self.storage, session.id)
            .map_err(|error| error.to_string())?;
        let session_id = session.id;
        let workspace_session = if let Some(workspace) = &self.workspace_session {
            let workspace = smol::block_on(caudra_agent::resume_workspace_session(
                &mut session,
                workspace,
            ))?;
            Some(workspace)
        } else {
            None
        };
        let remote_project_context = workspace_session
            .as_ref()
            .map(|workspace| {
                smol::block_on(
                    caudra_agent::remote_project_context::load_remote_project_context(
                        workspace,
                        self.config.features,
                    ),
                )
                .map_err(|error| error.to_string())
            })
            .transpose()?;
        let mut session = prepare_session_for_runtime(
            session,
            workspace_session.as_ref(),
            self.allow_workspace_recovery,
        )?;
        if let Some(workspace) = &workspace_session {
            let stored =
                caudra_storage::workspace_binding::StoredWorkspaceBinding::new_with_cursor(
                    workspace.binding().clone(),
                    workspace.cursor().clone(),
                    session
                        .workspace_binding()
                        .and_then(|binding| binding.cursor_label().map(str::to_owned)),
                )
                .map_err(|error| format!("Remote workspace identity is invalid: {error}"))?;
            if let Some(binding) = session.workspace_binding()
                && !binding.exact_scope_eq(&stored)
            {
                return Err("Remote session belongs to a different workspace cursor".into());
            }
        }
        let changes = file_revert::session_changes(
            &mut session,
            workspace_session.as_ref(),
            self.change_factory.as_ref(),
            &self.snapshots,
        );
        let change_recorder =
            RecorderSlot::new(ArcSwapOption::from(changes.recorder.map(Arc::new)));
        let prepare_ms = lap();
        let initial_history = match crate::active_session_history(&session) {
            Ok(history) => history,
            Err(error) => {
                tracing::error!(%error, session_id = %session.id, "failed to restore active history");
                Vec::new()
            }
        };
        let active_history_ms = lap();
        let archived_history = crate::archived_session_history(&session);
        let todos = crate::session_todos(&session, &archived_history, &initial_history);
        let archived_history_ms = lap();
        let restore_session = !initial_history.is_empty() || session_has_content(&session);
        let (system_prompt_profile_name, system_prompt_profile) =
            self.resolve_prompt_profile(&session)?;
        let permissions = Arc::new(self.permissions.fork_session(session.id));
        // Publishing writes the session out so a conversation grant has a row
        // to be fenced against. A session holding nothing has nothing to keep,
        // so it earns its row at its first run instead of at startup.
        let conversation_permissions = if restore_session {
            ConversationPermissions::Published(attach_session_permissions(
                &self.storage,
                &self.storage_writer,
                &mut session,
                &permissions,
            )?)
        } else {
            ConversationPermissions::Pending
        };
        permissions.set_session_mode(session.meta.permission_mode.clone());
        let goal = caudra_agent::GoalHandle::restored(session.meta.active_goal.as_deref());
        if let Some(limit) = session.meta.goal_continuation_limit {
            goal.set_continuation_limit(limit);
        }
        let permissions_ms = lap();
        let subagent_history = crate::agent::stored_subagent_history(&session);
        let subagent_history_ms = lap();
        let mut handles = AgentHandles::spawn(
            &self.model_slot,
            initial_history,
            archived_history,
            todos,
            self.config.clone(),
            self.ui_config.tool_output_lines,
            &permissions,
            Some(SessionRef::from(session.id)),
            Some(Arc::clone(&lease)),
            self.timeouts,
            self.lua_event_handle.clone(),
            self.mcp_handle.clone(),
            self.mcp_config_errors.clone(),
            Arc::clone(&self.model_policy),
            goal,
            subagent_history,
            system_prompt_profile.clone(),
            Arc::clone(&self.prompt_profiles),
            Some(self.storage.clone()),
            Arc::clone(&change_recorder),
            workspace_session.clone(),
            remote_project_context.clone(),
            self.host_cwd.clone(),
            self.local_documents.clone(),
            self.background_enabled,
        );
        let agent_spawn_ms = lap();
        let mut app = App::new(
            &self.model_slot.load().model,
            session,
            self.storage.clone(),
            Arc::clone(&self.available_models),
            handles.mcp_reader(),
            handles.mcp_config_errors.clone(),
            self.lua_command_reader.clone(),
            self.keymap_reader.clone(),
            self.hint_reader.clone(),
            Arc::clone(&self.storage_writer),
            self.ui_config.clone(),
            self.input_history_size,
            self.max_log_files,
            self.docs,
            permissions,
            if self.no_commands {
                Arc::from([])
            } else if let Some(context) = &remote_project_context {
                Arc::from(caudra_agent::command::discover_remote_commands(context))
            } else {
                Arc::clone(&self.custom_commands)
            },
            self.lua_event_handle.clone(),
            Arc::clone(&self.model_policy),
            Arc::clone(&self.prompt_profiles),
            workspace_session.clone(),
            self.config.features,
        );
        app.local_documents = self.local_documents.clone();
        app.sandbox_live.connector = self.sandbox_connector.clone();
        app.sandbox_live.transfer_connector = self.transfer_connector.clone();
        app.sandbox_live.readiness = self.sandbox_readiness.clone();
        app.sandbox_live.name = self.sandbox_name.clone();
        app.sandbox_live.network_gate = self.network_gate.clone();
        app.conversation_permissions = conversation_permissions;
        app.permission_authority_factory = self.permission_authority_factory.clone();
        app.sync_permission_authority()
            .map_err(|error| error.to_string())?;
        app.no_commands = self.no_commands;
        if let Some(warning) = remote_project_context
            .as_ref()
            .and_then(|context| context.skipped_warning())
        {
            app.state.warnings.push(warning);
        }
        app.remote_project_context = remote_project_context;
        app.reconcile_plan_target();
        let app_new_ms = lap();
        app.snapshots_config = self.snapshots;
        app.change_factory = self.change_factory.clone();
        app.local_changes = changes.local;
        app.change_recorder = change_recorder;
        // Publishing already saved the coverage of a session worth saving.
        app.reconcile_loaded_session(changes.first, false);
        app.refresh_record_index();
        app.live_sessions = Arc::clone(&self.live_sessions);
        app.state.system_prompt_profile_name = system_prompt_profile_name;
        app.state.system_prompt_profile = system_prompt_profile;
        app.state.system_prompt_profile_override = self.prompt_profile_override.is_some();
        handles.apply_to_app(&mut app);
        if restore_session {
            app.restore_resumed_session();
        }
        // After the transcript exists: a card can only be brought up to date
        // once the restore that draws it has run.
        app.refresh_workflow_cards();
        app.set_pattern_suggestion_loader(self.pattern_suggestion_loader.clone());
        handles.start_automations(&mut app, &self.automations, automations);
        info!(
            session_id = %session_id,
            prepare_ms,
            active_history_ms,
            archived_history_ms,
            permissions_ms,
            subagent_history_ms,
            agent_spawn_ms,
            app_new_ms,
            restore_ms = lap(),
            total_ms = started.elapsed().as_millis() as u64,
            "session runtime spawned"
        );
        let (shell_tx, shell_rx) = flume::unbounded::<ShellEvent>();
        Ok(SessionRuntime {
            peer: None,
            app,
            lease,
            handles,
            shell_tx,
            shell_rx,
            last_status: SessionStatus::Idle,
            last_tasks: Vec::new(),
            notifications: RunNotificationState::default(),
            restore_transitions: Vec::new(),
        })
    }
}

/// Installs the startup theme, following the terminal background when the
/// theme in use has a light half. Only the in-memory name is set, so the pick
/// the user saved interactively survives.
fn start_theme(ui_config: &UiConfig, warnings: &mut Vec<String>) -> Option<AutoSwitch> {
    // Without `ui.theme` this is the pick from `/theme`, or the default.
    let chosen = ui_config
        .theme
        .clone()
        .unwrap_or_else(theme::current_theme_name);
    if let Err(e) = theme::load_by_name(&chosen) {
        warnings.push(format!("config ui.theme: {e}"));
        return None;
    }

    let Some(auto) = pair_switch(&chosen, ui_config.theme_light.as_deref(), warnings) else {
        theme::set_current_name(&chosen);
        apply_theme(&chosen, warnings);
        return None;
    };
    if let Err(e) = auto.apply() {
        warnings.push(format!("config ui.theme: {e}"));
        return None;
    }
    Some(auto)
}

/// `ui.theme_light` names the light half outright. Otherwise the pairing
/// table decides, and `chosen` may be either half of it.
fn pair_switch(
    chosen: &str,
    configured_light: Option<&str>,
    warnings: &mut Vec<String>,
) -> Option<AutoSwitch> {
    if let Some(light) = configured_light {
        // Report a typo in the half not being installed now, rather than
        // leaving it to surface the first time the terminal turns light.
        if let Err(e) = theme::load_by_name(light) {
            warnings.push(format!("config ui.theme_light: {e}"));
            return None;
        }
        // The probe must precede `InputReader::spawn`, which eats the reply.
        return Some(AutoSwitch::new(
            chosen.to_owned(),
            light.to_owned(),
            appearance::detect(),
        ));
    }
    let pair = theme::pair_for(chosen)?;
    Some(AutoSwitch::new(
        pair.dark.to_owned(),
        pair.light.to_owned(),
        appearance::detect(),
    ))
}

fn apply_theme(name: &str, warnings: &mut Vec<String>) {
    match theme::load_by_name(name) {
        Ok(theme) => theme::set(theme),
        Err(e) => warnings.push(format!("config ui.theme: {e}")),
    }
}

pub(crate) struct EventLoop<'t> {
    terminal: &'t mut ratatui::DefaultTerminal,
    sessions: Vec<SessionRuntime>,
    focused: usize,
    started: Instant,
    last_focused: Option<CaudraId>,
    last_workspace_tabs: Option<WorkspaceTabsSnapshot>,
    terminal_focused: bool,
    notifier: Option<terminal::TerminalNotifier>,
    program_status_reporter: Option<ProgramStatusReporter>,
    ctx: SpawnCtx,
    input: InputReader,
    auto_theme: Option<AutoSwitch>,
    warn_rx: flume::Receiver<String>,
    warn_tx: flume::Sender<String>,
    title_rx: flume::Receiver<GeneratedTitle>,
    title_tx: flume::Sender<GeneratedTitle>,
    ui_action_rx: flume::Receiver<UiAction>,
    herdr_reporter: Option<HerdrReporterHandle>,
    worktrees: Backend,
    _model_fetch_task: smol::Task<()>,
    relocation: Option<PendingRelocation>,
    sandbox: Option<SandboxAttachment>,
    sandbox_control: Option<SandboxControl>,
    sandbox_workflows: Vec<SessionTransition>,
}

struct SessionTransition {
    _background: Option<BackgroundTransition>,
    _workflow: Option<WorkflowTransition>,
}

impl SessionTransition {
    fn release_settled<'a>(
        reservations: &mut Vec<Self>,
        apps: impl IntoIterator<Item = &'a App>,
    ) -> bool {
        if reservations.is_empty()
            || apps.into_iter().any(|app| {
                app.sandbox_live.reply.is_some()
                    || app.sandbox_live.transfer.is_some()
                    || app.sandbox_live.attachment.is_some()
            })
        {
            return false;
        }
        reservations.clear();
        true
    }

    fn reserve(handles: &AgentHandles) -> Result<Self, String> {
        let background = handles
            .background
            .as_ref()
            .map(crate::agent::reserve_background_transition)
            .transpose()?;
        let workflow = handles
            .workflow_handle()
            .map(|workflow| smol::block_on(workflow.suspend()).map_err(|error| error.to_string()))
            .transpose()?;
        Ok(Self {
            _background: background,
            _workflow: workflow,
        })
    }
}

/// What the stopped sessions go on to once every one of them is saved.
pub(crate) enum Handoff {
    Relocation(SessionRelocationHandoff),
    Worktree(WorktreeRequest),
}

struct PendingRelocation {
    handoff: Handoff,
    _workflows: Vec<SessionTransition>,
}

/// Empty sessions are deleted only after becoming idle, so both transitions
/// participate in the persistence trigger.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TabStorageState {
    Content,
    EmptyBusy,
    EmptyIdle,
}

#[derive(Clone, PartialEq, Eq)]
struct WorkspaceTabsSnapshot {
    cwd: PathBuf,
    tabs: WorkspaceTabs,
    storage_states: Vec<TabStorageState>,
}

/// One item from any of the event loop's sources; `None` from `next_wake`
/// means the wait timed out (animation/idle tick).
enum Wake {
    Input(Event),
    InputGone,
    Ui(UiAction),
    Agent(usize, Box<caudra_agent::Envelope>),
    Automation(usize, Box<AutomationEvent>),
    Background,
    Delivery(usize, Box<DeliveryReply>),
    Shell(usize, ShellEvent),
    Warn(String),
    Title(GeneratedTitle),
}

/// What a model-written title came back as, for the session that asked. The
/// error is carried rather than logged because this title is one the user
/// asked for by hand and is waiting on.
struct GeneratedTitle {
    id: CaudraId,
    result: Result<String, String>,
}

struct BackgroundModels {
    available: Arc<ArcSwapOption<Vec<String>>>,
    warn_rx: flume::Receiver<String>,
    warn_tx: flume::Sender<String>,
    task: smol::Task<()>,
}

fn merge_batch(
    available: &Arc<ArcSwapOption<Vec<String>>>,
    batch: caudra_providers::provider::ModelBatch,
    warn_tx: &flume::Sender<String>,
) {
    for w in batch.warnings {
        let _ = warn_tx.try_send(w);
    }
    if batch.models.is_empty() {
        return;
    }
    let mut merged = available.load().as_deref().cloned().unwrap_or_default();
    for spec in &batch.models {
        if !merged.contains(spec) {
            merged.push(spec.clone());
        }
    }
    available.store(Some(Arc::new(merged)));
}

fn spawn_model_fetch(
    model_slot: &Arc<ArcSwap<ModelSlot>>,
    timeouts: Timeouts,
    policy: Arc<ModelPolicy>,
) -> BackgroundModels {
    let available: Arc<ArcSwapOption<Vec<String>>> = Arc::new(ArcSwapOption::empty());
    let bg = Arc::clone(&available);
    let (warn_tx, warn_rx) = flume::unbounded::<String>();
    let warn_tx_bg = warn_tx.clone();
    let model_slot = Arc::clone(model_slot);
    let task = smol::spawn(async move {
        let warn_tx = warn_tx_bg;
        let done = Box::new(move || {
            let spec = model_slot.load().model.spec();
            let mut resolved = match Model::from_spec(&spec) {
                Ok(m) => m,
                Err(e) => {
                    warn!(spec = %spec, error = %e, "failed to resolve model after discovery");
                    return;
                }
            };
            let provider = match from_model(&mut resolved, timeouts) {
                Ok(p) => p,
                Err(e) => {
                    warn!(spec = %spec, error = %e, "failed to create provider after discovery");
                    return;
                }
            };
            model_slot.store(Arc::new(ModelSlot {
                model: resolved,
                provider: Arc::from(provider),
            }));
        });
        fetch_all_models(
            &policy,
            |batch| merge_batch(&bg, batch, &warn_tx),
            Some(done),
        )
        .await;
    });
    BackgroundModels {
        available,
        warn_rx,
        warn_tx,
        task,
    }
}

fn should_open_startup_login(needs_login: bool, providers: &ProvidersConfig) -> bool {
    needs_login
        && !providers.providers.iter().any(|(slug, definition)| {
            definition.protocol.is_some() && ManifestRegistry::get(slug).is_none()
        })
}

impl<'t> EventLoop<'t> {
    pub(crate) fn new(
        terminal: &'t mut ratatui::DefaultTerminal,
        params: EventLoopParams,
    ) -> Result<Self> {
        let EventLoopParams {
            peer_host,
            mut model,
            needs_login,
            commands,
            no_commands,
            sessions,
            focused,
            mut startup_warnings,
            joined_groups,
            launch_automations,
            storage,
            config,
            automations,
            ui_config,
            snapshots,
            change_factory,
            allow_workspace_recovery,
            input_history_size,
            max_log_files,
            docs,
            permissions,
            pattern_suggestion_loader,
            permission_authority_factory,
            sandbox_connector,
            transfer_connector,
            initial_seed,
            sandbox_readiness,
            sandbox_name,
            timeouts,
            exit_on_done,
            lua_command_reader,
            keymap_reader,
            hint_reader,
            ui_action_rx,
            lua_event_handle,
            model_policy,
            prompt_profiles,
            default_prompt_profile,
            prompt_profile_override,
            herdr_reporter,
            worktrees,
            workspace_session,
            remote_project_context: _,
            local_documents,
        } = params;

        let started = Instant::now();
        let mut phase_start = started;
        let mut lap = || {
            let elapsed = phase_start.elapsed().as_millis() as u64;
            phase_start = Instant::now();
            elapsed
        };

        // Apply the config theme before the warmup thread spawns, or warmup
        // could bake the syntax palette from the old theme.
        let auto_theme = start_theme(&ui_config, &mut startup_warnings);

        static PROCESS_WARMUP: std::sync::Once = std::sync::Once::new();
        PROCESS_WARMUP.call_once(|| {
            std::thread::spawn(crate::highlight::warmup);
            crate::update::spawn_check(ui_config.update_check, ui_config.update_channel.clone());
        });

        let cwd =
            canonical_cwd(&std::env::current_dir().context("read current working directory")?)
                .map_err(|error| eyre!(error))?;
        let (mcp_handle, mcp_config_errors) = smol::block_on(mcp::start(&cwd));
        let mcp_ms = lap();

        let provider: Arc<dyn Provider> = if needs_login {
            Arc::from(caudra_providers::provider::from_model_fallback(
                &mut model, timeouts,
            ))
        } else {
            Arc::from(from_model(&mut model, timeouts).context("create provider")?)
        };
        let model_slot = Arc::new(ArcSwap::from_pointee(ModelSlot {
            model: model.clone(),
            provider,
        }));
        let bg = spawn_model_fetch(&model_slot, timeouts, Arc::clone(&model_policy));
        let storage_writer = Arc::new(StorageWriter::new(storage.clone(), bg.warn_tx.clone()));
        let (title_tx, title_rx) = flume::unbounded::<GeneratedTitle>();

        match ui_config.math {
            caudra_config::MathStyle::Unicode => caudra_markdown::render::MathStyle::Unicode,
            caudra_config::MathStyle::Raw => caudra_markdown::render::MathStyle::Raw,
        }
        .set_global();
        match ui_config.mermaid {
            caudra_config::MermaidStyle::Unicode => caudra_markdown::render::MermaidStyle::Unicode,
            caudra_config::MermaidStyle::Off => caudra_markdown::render::MermaidStyle::Off,
        }
        .set_global();

        caudra_workbench::scroll::set_touch(ui_config.touch.enabled(terminal::detect_touch));

        let notifier =
            terminal::TerminalNotifier::new(ui_config.notifications, herdr_reporter.is_some());
        let program_status_reporter =
            ProgramStatusReporter::detect(ui_config.notifications, herdr_reporter.is_some());
        let mut ctx = SpawnCtx {
            peer_host: peer_host.filter(|_| {
                config.features.enabled(Feature::CrossSessionMessaging)
                    && workspace_session.is_none()
                    && sandbox_name.is_none()
            }),
            storage,
            background_enabled: !exit_on_done,
            config,
            automations,
            ui_config,
            snapshots,
            change_factory,
            allow_workspace_recovery,
            input_history_size,
            max_log_files,
            docs,
            permissions,
            pattern_suggestion_loader,
            permission_authority_factory,
            sandbox_connector,
            transfer_connector,
            sandbox_readiness,
            sandbox_name,
            timeouts,
            custom_commands: Arc::from(commands),
            network_gate: Arc::default(),
            no_commands,
            lua_command_reader,
            keymap_reader,
            hint_reader,
            lua_event_handle,
            mcp_handle,
            mcp_config_errors,
            model_slot,
            available_models: bg.available,
            storage_writer,
            model_policy,
            prompt_profiles,
            default_prompt_profile,
            prompt_profile_override,
            live_sessions: Arc::default(),
            host_cwd: workspace_session.as_ref().map(|_| cwd.clone()),
            workspace_session,
            local_documents,
        };

        let provider_ms = lap();

        if !ctx.allow_workspace_recovery {
            check_relocation_destination(&ctx.storage, &cwd).map_err(|error| eyre!(error))?;
        }
        let recover_sessions_ms = lap();

        let armings = launch_armings(sessions.len(), focused, launch_automations);
        let mut runtimes: Vec<SessionRuntime> = sessions
            .into_iter()
            .zip(armings)
            .map(|(tab, cli)| ctx.spawn_runtime(tab, cli))
            .collect::<Result<_, _>>()
            .map_err(|error| eyre!(error))?;
        ctx.allow_workspace_recovery = true;
        info!(
            mcp_ms,
            provider_ms,
            recover_sessions_ms,
            spawn_runtimes_ms = lap(),
            total_ms = started.elapsed().as_millis() as u64,
            "event loop startup phases"
        );
        if runtimes.is_empty() {
            return Err(eyre!("event loop needs at least one session"));
        }
        let focused = focused.min(runtimes.len() - 1);
        let app = &mut runtimes[focused].app;
        app.install_initial_seed(initial_seed);
        app.exit_on_done = exit_on_done;
        let show_login =
            needs_login && should_open_startup_login(needs_login, &ProvidersConfig::load());
        if show_login {
            app.login_picker.open(app.storage.clone());
        }
        app.open_awaiting_mcp_trust(show_login);
        app.open_awaiting_permission_config_trust(show_login);
        if !ctx.mcp_config_errors.is_empty() {
            let msg = format!("MCP config error: {}", ctx.mcp_config_errors);
            app.flash(msg);
        }
        for w in startup_warnings {
            app.flash(w);
        }
        for runtime in &mut runtimes {
            runtime.install_peer(&ctx);
        }
        if joined_groups {
            runtimes[focused].warn_joined_groups();
        }

        let input = InputReader::spawn();
        terminal::set_appearance_reporting(true);

        Ok(Self {
            terminal,
            sessions: runtimes,
            focused,
            started: Instant::now(),
            last_focused: None,
            last_workspace_tabs: None,
            terminal_focused: false,
            notifier,
            program_status_reporter,
            ctx,
            input,
            auto_theme,
            warn_rx: bg.warn_rx,
            warn_tx: bg.warn_tx,
            title_rx,
            title_tx,
            ui_action_rx,
            herdr_reporter,
            worktrees,
            _model_fetch_task: bg.task,
            relocation: None,
            sandbox: None,
            sandbox_control: None,
            sandbox_workflows: Vec::new(),
        })
    }

    fn focused_app(&mut self) -> &mut App {
        &mut self.sessions[self.focused].app
    }

    fn herdr_resume(&self) -> HerdrResume {
        let session = &self.sessions[self.focused].app.state.session;
        if self.ctx.storage.is_ephemeral() {
            HerdrResume::Never
        } else if session.is_persisted() {
            HerdrResume::Session(session.id)
        } else {
            HerdrResume::Fresh
        }
    }

    pub(crate) fn run(mut self, mut initial_prompt: Option<String>) -> Result<ShutdownReport> {
        // The first frame always paints. After that only a poller, an event or
        // an animation tick owes another.
        let mut dirty = Dirty::YES;
        // The limiter owes nothing until it has painted once, so the first
        // frame needs no exemption. Cleared only by a paint, never per pass: a
        // keystroke arriving while a frame is held back keeps its exemption
        // until that frame is the one on screen.
        let mut limiter = FrameLimiter::default();
        let mut urgent = false;
        let result = loop {
            if self.relocation.is_some() || self.sandbox.is_some() || self.sandbox_control.is_some()
            {
                break Ok(());
            }
            dirty |= self.tick();
            if self.sandbox_control.is_some() {
                break Ok(());
            }
            match self.drain_channels() {
                Ok(d) => dirty |= d,
                Err(e) => break Err(e),
            }
            if self.relocation.is_some() || self.sandbox.is_some() {
                break Ok(());
            }
            if self.focused_app().lifecycle_blocker().is_none()
                && let Some(prompt) = initial_prompt.take()
            {
                let sub = Submission::from_text(prompt);
                let actions = self.focused_app().handle_submit(sub);
                self.dispatch(self.focused, actions);
                dirty = Dirty::YES;
            }
            self.checkpoint_all();
            self.persist_workspace_tabs_if_changed();
            // An exit leaves the loop right below, so its frame can never be
            // the one the cap holds back.
            urgent |= self
                .sessions
                .iter()
                .any(|rt| rt.app.exit_request != ExitRequest::None);
            let owed = dirty.take();
            let painted = owed && limiter.admit(Instant::now(), urgent);
            if painted {
                urgent = false;
                let app = &mut self.sessions[self.focused].app;
                if let Err(e) = self.terminal.draw(|f| {
                    app.view(f);
                    color_compat::downgrade_if_needed(f.buffer_mut());
                    app.apply_terminal_links(f.buffer_mut());
                }) {
                    break Err(e.into());
                }
                self.sessions[self.focused].capture_peer_review();
            }
            let held = owed && !painted;
            if held {
                dirty = Dirty::YES;
            }

            if let Some(i) = self.sessions.iter().position(|rt| {
                rt.app.exit_request != ExitRequest::None && !rt.notifications.waiting_for_drain()
            }) {
                // A backgrounded session can finish an `exit_on_done` turn;
                // focus it so shutdown reports its exit code and id.
                self.focused = i;
                self.emit_notifications();
                break Ok(());
            }

            // Sleeping a whole frame instead of a fraction of one is what
            // makes a spinner cost 12 paints a second instead of 62.
            let cadence = self.sessions[self.focused].app.cadence();
            let mut timeout = cadence.frame().unwrap_or(IDLE_POLL);
            if held {
                timeout = limiter.hold(Instant::now(), timeout);
            }
            match self.next_wake(timeout) {
                // Any event can change the screen, so paint after handling it
                // rather than asking every handler to prove it did.
                Some(wake) => {
                    dirty = Dirty::YES;
                    urgent |= matches!(wake, Wake::Input(_));
                    if let Err(e) = self.handle_wake(wake) {
                        break Err(e);
                    }
                }
                // Only the clock moved, so motion alone owes the frame. The
                // cadence is the one from before the sleep, so motion that
                // just stopped still gets a last paint to clear itself off
                // the screen.
                None => dirty |= Dirty::from(cadence.moves()),
            }
        };
        // Fatal errors still save every session, kill MCP process groups,
        // and drain the storage writer before the process exits.
        let status = exit_program_status(
            self.program_status(),
            self.sessions[self.focused].app.exit_on_done,
            result.is_err(),
        );
        let report = self.shutdown(status)?;
        result.map(|()| report)
    }

    /// Wait for the next event from any source, or time out so animations
    /// and periodic polls keep running. `Duration::ZERO` drains whatever is
    /// already pending.
    fn next_wake(&self, timeout: Duration) -> Option<Wake> {
        let mut sel = flume::Selector::new().recv(self.input.receiver(), |res| match res {
            Ok(ev) => Some(Wake::Input(ev)),
            Err(_) => Some(Wake::InputGone),
        });
        if !self.ui_action_rx.is_disconnected() {
            sel = sel.recv(&self.ui_action_rx, |res| res.ok().map(Wake::Ui));
        }
        sel = sel.recv(&self.warn_rx, |res| res.ok().map(Wake::Warn));
        sel = sel.recv(&self.title_rx, |res| res.ok().map(Wake::Title));
        for (i, rt) in self.sessions.iter().enumerate() {
            sel = sel.recv(&rt.app.background_delivery.replies, move |res| {
                res.ok().map(|reply| Wake::Delivery(i, Box::new(reply)))
            });
            if rt.handles.background.is_some() {
                sel = sel.recv(&rt.handles.background_wake_rx, |_| Some(Wake::Background));
            }
            if !rt.handles.agent_rx.is_disconnected() {
                sel = sel.recv(&rt.handles.agent_rx, move |res| {
                    res.ok().map(|env| Wake::Agent(i, Box::new(env)))
                });
            }
            if let Some(events) = rt.handles.automation_events() {
                sel = sel.recv(events, move |res| {
                    res.ok().map(|event| Wake::Automation(i, Box::new(event)))
                });
            }
            sel = sel.recv(&rt.shell_rx, move |res| {
                res.ok().map(|ev| Wake::Shell(i, ev))
            });
        }
        sel.wait_timeout(timeout).ok().flatten()
    }

    fn handle_wake(&mut self, wake: Wake) -> Result<()> {
        match wake {
            Wake::Input(ev) => self.handle_input(ev),
            Wake::InputGone => return Err(eyre!("terminal input reader stopped")),
            Wake::Ui(action) => self.handle_ui_action(action),
            Wake::Agent(i, envelope) => self.handle_agent(i, envelope),
            Wake::Automation(i, event) => {
                let session = &mut self.sessions[i];
                session.notifications.on_automation_event(&event);
                let _ = session.app.handle_automation_event(*event);
            }
            Wake::Background => {}
            Wake::Delivery(index, reply) => {
                if let Err(error) = self.sessions[index].app.apply_delivery_reply(*reply) {
                    self.sessions[index].app.suppress_background_wakes();
                    self.sessions[index].app.flash(error);
                }
            }
            Wake::Shell(i, event) => self.handle_shell_event(i, event),
            Wake::Warn(warning) => {
                // The one place every background warning passes through. A
                // flash fades and the user may not be looking, so the log is
                // what a bug report can still be read out of.
                warn!(%warning, "background warning shown to the user");
                self.focused_app().flash(warning);
            }
            Wake::Title(GeneratedTitle { id, result }) => self.apply_generated_title(id, result),
        }
        Ok(())
    }

    fn handle_shell_event(&mut self, index: usize, event: ShellEvent) {
        let ShellEvent::RemoteDirectoryResolved { previous, result } = event else {
            self.sessions[index].app.handle_shell_event(event);
            return;
        };
        let Some(current) = self.sessions[index].app.workspace_session.as_ref() else {
            return;
        };
        if current.cursor() != &previous {
            self.sessions[index]
                .app
                .flash("cd: workspace cursor changed while resolving the directory".into());
            return;
        }
        let change = match *result {
            Ok(change) => change,
            Err(error) => {
                self.sessions[index].app.flash(error);
                return;
            }
        };
        if !self.sessions[index].quiescent() {
            self.sessions[index].app.flash(
                "cd: session acquired active or queued work while resolving the directory".into(),
            );
            return;
        }
        let workspace = change.workspace.clone();
        let _transition = match SessionTransition::reserve(&self.sessions[index].handles) {
            Ok(transition) => transition,
            Err(error) => {
                self.sessions[index].app.flash(error);
                return;
            }
        };
        let context = Arc::clone(&change.context);
        let display_path = change.display_path.clone();
        if let Err(error) = self.sessions[index]
            .app
            .install_remote_working_directory(change)
        {
            self.sessions[index].app.flash(error);
            return;
        }
        let history = self.sessions[index]
            .app
            .shared_history
            .as_ref()
            .map(|history| history.load().messages.as_ref().clone())
            .unwrap_or_default();
        self.sessions[index]
            .handles
            .rebind_workspace(workspace, context);
        self.respawn_agent(index, history);
        self.sessions[index].app.flash(format!("cd {display_path}"));
    }

    /// The one save trigger. A checkpoint writes only on a real change, so
    /// every tool result reaches disk within a frame while an idle session
    /// writes nothing.
    fn checkpoint_all(&mut self) {
        for rt in &mut self.sessions {
            rt.app.checkpoint();
            if !rt.restore_transitions.is_empty() && !rt.app.has_lifecycle_work() {
                match self
                    .ctx
                    .storage_writer
                    .save_sync(Arc::clone(&rt.app.state.session))
                {
                    Ok(()) => rt.restore_transitions.clear(),
                    Err(error) => rt.app.flash(error.to_string()),
                }
            }
            let final_save = rt.delivery_idle();
            if let Err(error) = rt.app.persist_background_delivery(final_save) {
                rt.app.suppress_background_wakes();
                rt.app
                    .flash(format!("Failed to persist background delivery: {error}"));
            }
        }
    }

    fn workspace_tabs_snapshot(&self) -> WorkspaceTabsSnapshot {
        let focused = self.sessions[self.focused].id();
        WorkspaceTabsSnapshot {
            cwd: PathBuf::from(&self.sessions[self.focused].app.state.session.cwd),
            tabs: WorkspaceTabs {
                open: self.sessions.iter().map(SessionRuntime::id).collect(),
                focused: Some(focused),
            },
            storage_states: self
                .sessions
                .iter()
                .map(|runtime| {
                    if runtime.app.has_content() {
                        TabStorageState::Content
                    } else if runtime.app.status == Status::Idle {
                        TabStorageState::EmptyIdle
                    } else {
                        TabStorageState::EmptyBusy
                    }
                })
                .collect(),
        }
    }

    fn persist_workspace_tabs_if_changed(&mut self) {
        let snapshot = self.workspace_tabs_snapshot();
        if self.last_workspace_tabs.as_ref() == Some(&snapshot) {
            return;
        }
        if !self.ctx.storage.is_ephemeral() {
            self.ctx
                .storage_writer
                .persist_workspace_tabs(snapshot.cwd.clone(), snapshot.tabs.clone());
        }
        self.last_workspace_tabs = Some(snapshot);
    }

    /// Only the focused session is drawn, so only it can owe a frame; focusing
    /// another is an event, and events always repaint. Background sessions
    /// still drain their floats, or a plugin writing to a window nobody is
    /// looking at would lose the output.
    fn tick(&mut self) -> Dirty {
        let blocked: Vec<_> = self
            .sessions
            .iter()
            .map(|runtime| runtime.app.sandbox_network_dispatch_blocker())
            .collect();
        self.sync_auto_theme();
        let mut dirty = self.poll_appearance();
        for (i, rt) in self.sessions.iter_mut().enumerate() {
            if i == self.focused {
                dirty |= rt.app.tick();
            } else {
                let _ = rt.app.float_mgr.tick();
                dirty |= rt.app.poll_sandbox();
            }
        }
        dirty |= self.poll_sandbox_actions();
        for (runtime, previous) in self.sessions.iter_mut().zip(blocked) {
            let current = runtime.app.sandbox_network_dispatch_blocker();
            if previous.is_some() && current.is_none() {
                runtime.handles.queue.wake_dispatch();
            }
            if current == Some(NETWORK_RECOVERY) && previous != current {
                runtime.app.flash(NETWORK_RECOVERY.into());
                if runtime.app.status == Status::Streaming {
                    let run_id = runtime.app.begin_main_cancel(true, true);
                    let _ = runtime
                        .handles
                        .cmd_tx
                        .try_send(AgentCommand::Cancel { run_id });
                }
            }
        }
        dirty
    }

    fn sandbox_gate(&mut self, transition: bool) -> Result<Vec<SessionTransition>, String> {
        self.sandbox_admission(transition, None)
    }

    fn sandbox_work_idle(&self) -> Result<(), String> {
        if self.sessions.iter().all(SessionRuntime::work_quiescent) {
            Ok(())
        } else {
            Err(SANDBOX_WORK_BUSY.into())
        }
    }

    fn sandbox_admission(
        &mut self,
        transition: bool,
        transfer_owner: Option<usize>,
    ) -> Result<Vec<SessionTransition>, String> {
        self.sandbox_work_idle()?;
        for (index, runtime) in self.sessions.iter().enumerate() {
            if let Some(reason) = if transfer_owner == Some(index) {
                runtime.app.transfer_start_blocker()
            } else {
                runtime.app.sandbox_action_blocker(transition)
            } {
                return Err(reason.into());
            }
            if let Some(binding) = runtime.app.state.session.workspace_binding()
                && RemoteOperationJournal::open(&self.ctx.storage)
                    .map_err(|error| error.to_string())?
                    .list_pending(binding)
                    .map_err(|error| error.to_string())?
                    .iter()
                    .any(|record| record.reachable_from(binding))
            {
                return Err("Reconcile pending Workcell mutations before a sandbox action".into());
            }
        }
        let mut workflows = Vec::new();
        for runtime in &self.sessions {
            workflows.push(SessionTransition::reserve(&runtime.handles)?);
        }
        let database =
            SessionDatabase::open_state(&self.ctx.storage).map_err(|error| error.to_string())?;
        for runtime in &self.sessions {
            if database
                .load_workflow_runs(runtime.id())
                .map_err(|error| error.to_string())?
                .iter()
                .any(|run| {
                    matches!(
                        run.status,
                        WorkflowRunStatus::Active
                            | WorkflowRunStatus::Paused
                            | WorkflowRunStatus::BudgetLimited
                    ) || run.outbox_pending
                })
            {
                return Err("Finish or explicitly cancel active/pending workflows before changing sandbox authority".into());
            }
        }
        if transition {
            for runtime in &mut self.sessions {
                runtime.app.checkpoint_now();
                self.ctx
                    .storage_writer
                    .save_sync_timeout(
                        Arc::clone(&runtime.app.state.session),
                        AGENT_SHUTDOWN_TIMEOUT,
                    )
                    .map_err(|error| {
                        format!("Session save failed; workspace unchanged: {error}")
                    })?;
            }
        }
        Ok(workflows)
    }

    fn poll_sandbox_actions(&mut self) -> Dirty {
        if let Err(disabled) = self.ctx.config.features.require(Feature::Sandboxes) {
            return Dirty::any(
                self.sessions
                    .iter_mut()
                    .map(|runtime| runtime.app.refuse_sandbox_work(&disabled)),
            );
        }
        self.release_settled_sandbox_reservations();
        let mut dirty = Dirty::NO;
        for index in 0..self.sessions.len() {
            if self.sessions[index].app.sandbox_live.transfer.is_some()
                && self.sessions[index]
                    .app
                    .sandbox_live
                    .transfer_queued
                    .as_ref()
                    .is_some_and(|(_, command)| {
                        matches!(
                            command,
                            crate::sandbox::transfer::TransferCommand::Open { .. }
                        )
                    })
            {
                continue;
            }
            if let Some((scope, command)) =
                self.sessions[index].app.sandbox_live.transfer_queued.take()
            {
                if !self.sessions[index].app.transfer_scope_current(&scope) {
                    self.sessions[index].app.transfer_failed(
                        "Attachment or transfer roots changed; compare again".into(),
                    );
                    continue;
                }
                let opening = matches!(
                    command,
                    crate::sandbox::transfer::TransferCommand::Open { .. }
                );
                // Reserving blocks on tasks of the global executor, whose agent dispatchers
                // block that thread while they wait for a claim. So reserve first, then take
                // the claims and confirm quiescence again under them.
                let admitted = if opening {
                    self.sandbox_admission(false, Some(index))
                } else {
                    Ok(Vec::new())
                };
                let queues: Vec<_> = self
                    .sessions
                    .iter()
                    .map(|runtime| runtime.handles.queue.clone())
                    .collect();
                let _claims: Vec<_> = queues.iter().map(|queue| queue.lock_dispatch()).collect();
                let admitted = admitted.and_then(|reservations| {
                    if opening {
                        self.sandbox_work_idle()?;
                    }
                    Ok(reservations)
                });
                match admitted {
                    Ok(reservations) => {
                        self.sessions[index].app.start_transfer(command);
                        if opening && self.sessions[index].app.sandbox_live.transfer.is_some() {
                            self.sandbox_workflows = reservations;
                        }
                    }
                    Err(error) => self.sessions[index].app.transfer_failed(error),
                }
                dirty = Dirty::YES;
            }
            if let Some(request) = self.sessions[index].app.sandbox_live.queued.take() {
                if let Some(control) = request.control() {
                    let result = caudra_sandbox::Controller::new(&self.ctx.storage)
                        .and_then(|controller| controller.store().get(&control.name));
                    match result {
                        Ok(record)
                            if self.sessions[index]
                                .app
                                .state
                                .session
                                .workspace_binding()
                                .and_then(StoredWorkspaceBinding::sandbox_record)
                                == Some(record.id) =>
                        {
                            let other_holder =
                                self.sessions.iter().enumerate().any(|(other, runtime)| {
                                    other != index
                                        && runtime
                                            .app
                                            .state
                                            .session
                                            .workspace_binding()
                                            .and_then(StoredWorkspaceBinding::sandbox_record)
                                            == Some(record.id)
                                });
                            let admitted = if other_holder {
                                Err("Another tab holds this sandbox; close it before exclusive control".into())
                            } else {
                                self.sandbox_gate(true)
                            };
                            match admitted {
                                Ok(workflows) => {
                                    self.sandbox_workflows = workflows;
                                    self.sandbox_control = Some(control);
                                    return Dirty::YES;
                                }
                                Err(error) => self.sessions[index].app.sandbox_failed(error),
                            }
                            continue;
                        }
                        Err(error) => {
                            self.sessions[index].app.sandbox_failed(error.to_string());
                            continue;
                        }
                        Ok(record) => {
                            let other_holder = self.sessions.iter().any(|runtime| {
                                runtime
                                    .app
                                    .state
                                    .session
                                    .workspace_binding()
                                    .and_then(StoredWorkspaceBinding::sandbox_record)
                                    == Some(record.id)
                            });
                            let blocker = if other_holder {
                                Some("Another tab holds this sandbox; control it from that tab")
                            } else {
                                self.sessions[index].app.sandbox_detached_action_blocker()
                            };
                            if let Some(reason) = blocker {
                                self.sessions[index].app.sandbox_failed(reason.into());
                            } else {
                                self.sessions[index].app.start_sandbox_live(*request);
                            }
                            dirty = Dirty::YES;
                            continue;
                        }
                    }
                }
                let transition = matches!(
                    request.operation,
                    LiveOperation::Attach { .. } | LiveOperation::Borrow { .. }
                );
                let readonly = matches!(
                    request.operation,
                    LiveOperation::Doctor { .. }
                        | LiveOperation::Reconcile { .. }
                        | LiveOperation::Credential { .. }
                );
                let admitted = if readonly {
                    Ok(Vec::new())
                } else {
                    self.sandbox_gate(transition)
                };
                match admitted {
                    Ok(workflows) => {
                        if transition {
                            self.sandbox_workflows = workflows;
                        }
                        self.sessions[index].app.start_sandbox_live(*request);
                    }
                    Err(error) => self.sessions[index].app.sandbox_failed(error),
                }
                dirty = Dirty::YES;
            }
            if let Some(attachment) = self.sessions[index].app.sandbox_live.attachment.take() {
                let reserved = if self.sandbox_workflows.is_empty() {
                    self.sandbox_gate(true)
                } else if self.sessions.iter().all(SessionRuntime::quiescent) {
                    Ok(std::mem::take(&mut self.sandbox_workflows))
                } else {
                    self.sandbox_workflows.clear();
                    Err(CWD_BUSY_ERR.into())
                };
                match reserved {
                    Ok(workflows) => {
                        self.sandbox_workflows = workflows;
                        self.sandbox = Some(attachment);
                    }
                    Err(error) => self.sessions[index].app.sandbox_failed(error),
                }
                dirty = Dirty::YES;
            }
        }
        self.release_settled_sandbox_reservations();
        dirty
    }

    fn release_settled_sandbox_reservations(&mut self) {
        if self.sandbox.is_none()
            && self.sandbox_control.is_none()
            && SessionTransition::release_settled(
                &mut self.sandbox_workflows,
                self.sessions.iter().map(|runtime| &runtime.app),
            )
        {
            for runtime in &self.sessions {
                runtime.handles.queue.wake_dispatch();
            }
        }
    }

    /// Keep the switch pointed at the theme actually in use. Picking from
    /// `/theme` re-points it at that theme's pair, or turns it off when the
    /// new theme has no light half. Previewing does not count: the picker
    /// only names a theme once the choice is committed.
    fn sync_auto_theme(&mut self) {
        let current = theme::current_theme_name();
        if self
            .auto_theme
            .as_ref()
            .is_some_and(|auto| auto.theme_name() == current)
        {
            return;
        }
        match (self.auto_theme.as_mut(), theme::pair_for(&current)) {
            (Some(auto), Some(pair)) => auto.retarget(pair, &current),
            (None, Some(pair)) => self.auto_theme = Some(AutoSwitch::adopt(pair, &current)),
            (_, None) => self.auto_theme = None,
        }
    }

    /// Re-ask the terminal for its appearance so a session left open across a
    /// light/dark switch follows it.
    ///
    /// The explicit query is answered through the input reader. The background
    /// probe reads the tty directly, so it parks the input reader first and
    /// waits for a lull: bytes arriving mid-probe would be consumed instead of
    /// delivered as keystrokes.
    fn poll_appearance(&mut self) -> Dirty {
        let Some(auto) = self.auto_theme.as_mut() else {
            return Dirty::NO;
        };
        if !auto.due(Instant::now()) {
            return Dirty::NO;
        }
        if auto.uses_explicit() {
            auto.request_explicit();
            return Dirty::NO;
        }
        if !self.input.receiver().is_empty() {
            auto.defer();
            return Dirty::NO;
        }
        let observed = {
            let _pause = match self.input.try_pause() {
                Ok(pause) => pause,
                Err(error) => {
                    warn!(%error, "terminal appearance probe deferred");
                    auto.defer();
                    return Dirty::NO;
                }
            };
            appearance::detect()
        };
        auto.observe(observed)
    }

    fn handle_agent(&mut self, idx: usize, envelope: Box<caudra_agent::Envelope>) {
        let rt = &mut self.sessions[idx];
        let current = is_current_top_level(rt.app.run_id, &envelope);
        let terminal = current
            && matches!(
                &envelope.event,
                AgentEvent::Done { .. } | AgentEvent::Error { .. }
            );
        match &envelope.event {
            AgentEvent::QueueDrained => {
                if current {
                    rt.notifications.on_drain();
                    let final_save = rt.delivery_idle();
                    if let Err(error) = rt.app.persist_background_delivery(final_save) {
                        rt.app.suppress_background_wakes();
                        rt.app.flash(error);
                    }
                }
                return;
            }
            AgentEvent::QueueItemConsumed { .. } | AgentEvent::QueueBatchConsumed { .. }
                if current =>
            {
                rt.notifications.on_queue_item_consumed();
                if rt.app.exit_on_done {
                    rt.app.clear_exit_request();
                }
            }
            AgentEvent::TurnComplete(turn) if current => {
                rt.notifications.on_turn_complete(&turn.message);
            }
            event if current => rt.notifications.on_done(event),
            _ => {}
        }
        let actions = self.sessions[idx].app.update(Msg::Agent(envelope));
        if terminal {
            let rt = &mut self.sessions[idx];
            let final_save = rt.delivery_idle();
            if let Err(error) = rt.app.persist_background_delivery(final_save) {
                rt.app.suppress_background_wakes();
                rt.app.flash(error);
            }
        }
        self.dispatch(idx, actions);
    }

    fn drain_channels(&mut self) -> Result<Dirty> {
        let mut dirty = Dirty::NO;
        // Leftovers beyond the budget are picked up right after the next draw.
        for _ in 0..DRAIN_BUDGET {
            match self.next_wake(Duration::ZERO) {
                Some(wake) => {
                    self.handle_wake(wake)?;
                    dirty = Dirty::YES;
                    if self.relocation.is_some() {
                        return Ok(dirty);
                    }
                }
                None => break,
            }
        }

        let slot_model = self.ctx.model_slot.load();
        let spec = slot_model.model.spec();
        for rt in &mut self.sessions {
            if rt.app.state.session.model != spec
                || rt.app.state.model.context_window != slot_model.model.context_window
            {
                rt.app.update_model(&slot_model.model);
                dirty = Dirty::YES;
            }
        }
        drop(slot_model);

        // `emit_focus_change` and `emit_status_changes` only fire Lua
        // autocmds. Anything a handler does comes back as a `UiAction` on the
        // next wake, which repaints then.
        self.emit_focus_change();
        dirty |= self.sync_peers();
        dirty |= self.sync_peer_work();
        dirty |= self.start_mailbox_runs();
        dirty |= self.start_peer_runs();
        for runtime in &self.sessions {
            let ready = runtime.app.status == Status::Idle
                && !runtime.app.awaiting_input()
                && !runtime.app.has_session_work()
                && !runtime.app.automatic_wakes_suppressed
                && !runtime.app.holds_recovery_text()
                && runtime.app.shell.active_ids().is_empty();
            runtime.handles.queue.allow_next_turn(ready);
        }
        dirty |= self.start_goal_checkins();
        dirty |= self.sync_automations();
        self.emit_status_changes();
        self.publish_live_sessions();
        dirty |= self.emit_task_changes();
        self.emit_program_status();
        self.emit_notifications();
        if let Some(reporter) = &self.herdr_reporter {
            reporter.observe(HerdrStatus {
                observation: aggregate_observations(
                    self.sessions.iter().map(SessionRuntime::herdr_observation),
                ),
                resume: self.herdr_resume(),
            });
            reporter.describe(self.sessions[self.focused].app.herdr_metadata());
        }
        // An `exit_on_done` exit waits on `QueueDrained`; a dead agent loop
        // can never send it, so fail instead of hanging forever.
        if let Some(runtime) = self.sessions.iter().find(|rt| {
            rt.app.exit_request != ExitRequest::None
                && rt.notifications.waiting_for_drain()
                && rt.handles.is_finished()
                && rt.handles.agent_rx.is_empty()
        }) {
            return Err(eyre!(
                "agent for session {} stopped before queue drain",
                runtime.id()
            ));
        }
        Ok(dirty)
    }

    fn handle_ui_action(&mut self, action: UiAction) {
        if crate::sandbox::transfer::active() {
            let message = "Cancel Transfer and await cleanup before editing sessions";
            match &action {
                UiAction::OpenEditor { reply_tx, .. } => {
                    let _ = reply_tx.send(-1);
                    return;
                }
                UiAction::Session {
                    req: SessionRequest::Focus { .. },
                    ..
                } => self.cancel_transfers(),
                UiAction::Session { reply_tx, .. }
                | UiAction::Model { reply_tx, .. }
                | UiAction::Task { reply_tx, .. } => {
                    let _ = reply_tx.send(Err(message.into()));
                    return;
                }
                UiAction::RunCommand { reply_tx, .. } => {
                    let _ = reply_tx.send(Err(message.into()));
                    return;
                }
                UiAction::OpenWorkbench { .. }
                | UiAction::OpenWin { .. }
                | UiAction::Builtin(_) => {
                    self.focused_app().flash(message.into());
                    return;
                }
                _ => {}
            }
        }
        match action {
            UiAction::Flash(msg) => {
                self.focused_app().flash(msg);
            }
            UiAction::SetWindowTitle(title) => {
                if let Err(error) = terminal::set_window_title(&title) {
                    warn!(%error, "failed to set window title");
                }
            }
            UiAction::OpenEditor { path, reply_tx } => {
                let code = self.open_editor(&path);
                let _ = reply_tx.send(code);
            }
            UiAction::OpenWorkbench { path, line } => {
                self.focused_app()
                    .open_workbench_file(&path, line.map(|line| line..=line));
            }
            UiAction::OpenWin {
                buf,
                config,
                focus,
                event_tx,
                cmd_rx,
            } => {
                let app = self.focused_app();
                app.float_mgr.open(buf, config, focus, event_tx, cmd_rx);
                if focus {
                    app.transition_plan(crate::app::mode::PlanTrigger::InteractivePrompt);
                }
            }
            UiAction::Session { req, reply_tx } => {
                self.handle_session_request(req, reply_tx);
            }
            UiAction::Model { req, reply_tx } => {
                let _ = reply_tx.send(self.handle_model_request(req));
            }
            UiAction::Task { req, reply_tx } => {
                let _ = reply_tx.send(self.handle_task_request(req));
            }
            UiAction::WinSaveView { reply_tx } => {
                let _ = reply_tx.send(self.focused_app().win_view());
            }
            UiAction::WinRestView { scroll_top } => {
                self.focused_app().set_scroll_top(scroll_top);
            }
            UiAction::Builtin(action) => {
                let actions = self.focused_app().run_builtin(action);
                self.dispatch(self.focused, actions);
            }
            UiAction::RunCommand {
                cmdline,
                depth,
                reply_tx,
            } => {
                // Answer before dispatching: the caller only waits on the name
                // resolving, and dispatch may take a while (or exit the app).
                match self.focused_app().run_cmdline(&cmdline, depth) {
                    Ok(actions) => {
                        let _ = reply_tx.send(Ok(()));
                        self.dispatch(self.focused, actions);
                    }
                    Err(e) => {
                        let _ = reply_tx.send(Err(e));
                    }
                }
            }
        }
    }

    /// `caudra.ui.open_editor`, the one caller left that wants `$EDITOR`.
    /// Exits with the editor's status code; `-1` (flashed on the focused
    /// session) when the editor could not be launched.
    fn open_editor(&mut self, path: &std::path::Path) -> i32 {
        let result = {
            let _pause = self.input.pause();
            terminal::open_in_editor(path, self.terminal)
        };
        self.terminal_focused = false;
        if let Some(reporter) = &mut self.program_status_reporter {
            reporter.invalidate();
        }
        match result {
            Ok(code) => code,
            Err(e) => {
                self.focused_app().flash(e);
                -1
            }
        }
    }

    fn emit_status_changes(&mut self) {
        let mut background = Vec::new();
        let handle = &self.ctx.lua_event_handle;
        for (i, rt) in self.sessions.iter_mut().enumerate() {
            let status = rt.display_status();
            if status == rt.last_status {
                continue;
            }
            let previous = std::mem::replace(&mut rt.last_status, status);
            let focused = i == self.focused;
            if !focused {
                background.push((rt.app.state.session.title.clone(), previous, status));
            }
            handle.fire_autocmd(
                "SessionStatusChanged",
                json!({
                    "session_id": rt.id(),
                    "title": rt.app.state.session.title,
                    "status": status.as_str(),
                    "focused": focused,
                }),
            );
        }
        // A session you cannot see has no other way to say it wants you, so
        // the news lands on whichever one you are looking at.
        for (title, previous, status) in background {
            if let Some(flash) = background_flash(&title, previous, status) {
                self.focused_app().flash(flash);
            }
        }
    }

    /// One diff per frame covers every path that finishes, cancels or errors a
    /// chat, so none of them has to remember to fire an event. An open
    /// `/tasks` picker is refreshed off the same diff, so a subagent that
    /// starts or ends shows up there without polling.
    fn emit_task_changes(&mut self) -> Dirty {
        let handle = &self.ctx.lua_event_handle;
        let mut dirty = Dirty::NO;
        for rt in &mut self.sessions {
            dirty |= rt.app.reconcile_tasks();
            let session_id = rt.app.state.session.id;
            diff_task_states(&mut rt.last_tasks, rt.app.task_states(), |task| {
                handle.fire_autocmd(
                    "TaskStatusChanged",
                    json!({
                        "session_id": session_id,
                        "id": task.id,
                        "name": task.name,
                        "status": task.status,
                    }),
                );
            });
        }
        dirty | self.focused_app().refresh_task_picker()
    }

    fn program_status(&self) -> ProgramStatus {
        aggregate_program_status(self.sessions.iter().map(SessionRuntime::program_status))
    }

    fn emit_program_status(&mut self) {
        if self.program_status_reporter.is_none() {
            return;
        }
        let status = self.program_status();
        if let Some(reporter) = &mut self.program_status_reporter
            && let Err(error) = reporter.observe(status)
        {
            warn!(%error, "program status reporting disabled after write failure");
            self.program_status_reporter = None;
        }
    }

    fn emit_notifications(&mut self) {
        let Some(notifier) = &self.notifier else {
            return;
        };
        let mut selected = None;
        for rt in &mut self.sessions {
            let candidate = if self.program_status_reporter.is_some() {
                rt.notifications
                    .reconcile_program_status(self.terminal_focused)
            } else {
                rt.notifications.reconcile(
                    rt.app.attention(),
                    rt.last_status,
                    rt.handles.queue.is_empty(),
                    self.terminal_focused,
                )
            };
            selected = select_notification(selected, candidate);
        }
        if let Some(notification) = selected
            && let Err(error) = notifier.notify(&notification.message())
        {
            warn!(notifier = ?notifier.notifier(), %error, "terminal notifications disabled after write failure");
            self.notifier = None;
        }
    }

    /// Republished only on a real change: the picker watches this slot by
    /// pointer, so a fresh `Arc` every frame would rebuild its rows for
    /// nothing.
    fn publish_live_sessions(&self) {
        let rows: Vec<SessionRow> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(i, rt)| SessionRow {
                id: rt.id(),
                title: rt.app.state.session.title.clone(),
                updated_at: rt.app.state.session.updated_at,
                activity: Some(rt.display_status().activity()),
                focused: i == self.focused,
                checkout: None,
            })
            .collect();
        if self.ctx.live_sessions.load().as_slice() != rows {
            self.ctx.live_sessions.store(Arc::new(rows));
        }
    }

    fn emit_focus_change(&mut self) {
        let id = self.sessions[self.focused].id();
        if self.last_focused == Some(id) {
            return;
        }
        // The pickers only ever list the focused session, so a session switch
        // closes them rather than leaving ids from elsewhere on screen.
        self.focused_app().task_picker.close();
        self.focused_app().shell_modal.close();
        self.focused_app().request_pattern_suggestions();
        let mut data = json!({ "session_id": id });
        if let Some(previous) = self.last_focused {
            data["previous_session_id"] = json!(previous.to_string());
        }
        self.last_focused = Some(id);
        self.ctx
            .lua_event_handle
            .fire_autocmd("SessionFocusChanged", data);
    }

    /// One turn per quiet session for everything that arrived while it was
    /// busy: mailbox wakes and settled workflow runs share the preamble, so a
    /// burst of either wakes the model once.
    fn start_mailbox_runs(&mut self) -> Dirty {
        if crate::sandbox::transfer::active() {
            return Dirty::NO;
        }
        let ready: Vec<_> = self
            .sessions
            .iter_mut()
            .enumerate()
            .filter_map(|(index, runtime)| {
                if !runtime.parent_ready()
                    || runtime.app.status != Status::Idle
                    || runtime.app.awaiting_input()
                    || runtime.app.automatic_wakes_suppressed
                    || runtime.app.sandbox_network_dispatch_blocker().is_some()
                {
                    return None;
                }
                if let Err(error) = runtime.app.persist_background_delivery(true) {
                    runtime.app.suppress_background_wakes();
                    runtime.app.flash(error);
                    return None;
                }
                if runtime.app.background_delivery.pending() {
                    return None;
                }
                let mut preamble = runtime.handles.claim_mailbox_wake();
                let woken = !preamble.is_empty();
                match runtime.app.claim_workflow_completions() {
                    Ok(messages) => preamble.extend(messages),
                    Err(error) => {
                        runtime.app.suppress_background_wakes();
                        runtime.app.release_background_claims();
                        runtime.app.flash(error);
                        return None;
                    }
                }
                let claimed_before_background = preamble.len();
                if let Some(background) = &runtime.handles.background {
                    match background.claim_messages() {
                        Ok(messages) => {
                            runtime.app.background_claims = messages.clone();
                            preamble.extend(messages);
                        }
                        Err(error) => {
                            runtime.app.suppress_background_wakes();
                            runtime.app.release_background_claims();
                            runtime.app.flash(error);
                            return None;
                        }
                    }
                }
                let started_by = wake_origin(
                    woken,
                    claimed_before_background,
                    preamble.len() - claimed_before_background,
                );
                (!preamble.is_empty()).then_some((index, preamble, started_by))
            })
            .collect();

        let dirty = Dirty::from(!ready.is_empty());
        for (index, preamble, started_by) in ready {
            let actions = self.sessions[index]
                .app
                .start_mailbox_run(preamble, started_by);
            if actions.is_empty() {
                self.sessions[index].app.suppress_background_wakes();
                self.sessions[index].app.release_background_claims();
            }
            self.dispatch(index, actions);
        }
        dirty
    }

    /// Lets each session's automations see this tick, and a settled session start the turn its
    /// next `next` delivery asks for. Unlike the wakes above, a delivery goes on while automatic
    /// wakes are suppressed: the runtime's own latch and limits hold it.
    fn sync_automations(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        for index in 0..self.sessions.len() {
            if let Some(actions) = self.sessions[index].sync_automations() {
                self.dispatch(index, actions);
                dirty = Dirty::YES;
            }
        }
        dirty
    }

    fn start_goal_checkins(&mut self) -> Dirty {
        if crate::sandbox::transfer::active() {
            return Dirty::NO;
        }
        let ready: Vec<_> = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, runtime)| {
                (runtime.quiescent()
                    && runtime.handles.queue.is_empty()
                    && !runtime.app.automatic_wakes_suppressed
                    && runtime.app.sandbox_network_dispatch_blocker().is_none()
                    && runtime.app.goal_checkin_due()
                    && runtime.handles.active_background_tasks() == 0)
                    .then_some(index)
            })
            .collect();
        let dirty = Dirty::from(!ready.is_empty());
        for index in ready {
            let actions = self.sessions[index].app.start_goal_checkin();
            self.dispatch(index, actions);
        }
        dirty
    }

    /// `List` replies from a background task (the scan can be slow); every
    /// other request is answered synchronously by the event loop, which owns
    /// the live runtimes.
    fn handle_session_request(&mut self, req: SessionRequest, reply_tx: flume::Sender<UiReply>) {
        if self.relocation.is_some() {
            let _ = reply_tx.send(Err(RELOCATION_ADMISSION_ERR.into()));
            return;
        }
        match req {
            SessionRequest::List => {
                let storage = self.ctx.storage.clone();
                smol::unblock(move || {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    let reply = AppSession::list(&cwd.to_string_lossy(), &storage)
                        .map_err(|e| e.to_string())
                        .and_then(|list| serde_json::to_value(list).map_err(|e| e.to_string()));
                    let _ = reply_tx.send(reply);
                })
                .detach();
            }
            // Deletes run on the storage writer thread after any queued
            // flushes, so the loop never blocks on disk and a queued save
            // cannot resurrect the files.
            SessionRequest::Delete { id } => {
                let lease = match parse_session_id(&id)
                    .and_then(|id| self.release_for_delete(id).map(|lease| (id, lease)))
                {
                    Ok(pair) => pair,
                    Err(error) => {
                        let _ = reply_tx.send(Err(error));
                        return;
                    }
                };
                let (id, lease) = lease;
                self.ctx.storage_writer.delete(id, move |res| {
                    let _lease = lease;
                    let reply = match res {
                        Ok(()) | Err(SessionError::Storage(StorageError::NotFound(_))) => {
                            Ok(json!(true))
                        }
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = reply_tx.send(reply);
                });
            }
            SessionRequest::Live => {
                let list: Vec<_> = self
                    .sessions
                    .iter()
                    .enumerate()
                    .map(|(i, rt)| {
                        json!({
                            "id": rt.id(),
                            "title": rt.app.state.session.title,
                            "status": SessionStatus::of(&rt.app).as_str(),
                            "updated_at": rt.app.state.session.updated_at,
                            "focused": i == self.focused,
                        })
                    })
                    .collect();
                let _ = reply_tx.send(Ok(json!(list)));
            }
            SessionRequest::Current => {
                let _ = reply_tx.send(Ok(json!(self.sessions[self.focused].id())));
            }
            SessionRequest::New { prompt, focus } => {
                let session = {
                    let slot = self.ctx.model_slot.load();
                    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                    AppSession::new(&slot.model.spec(), &cwd.to_string_lossy())
                };
                let lease = match SessionLease::acquire(&self.ctx.storage, session.id) {
                    Ok(lease) => Arc::new(lease),
                    Err(error) => {
                        let _ = reply_tx.send(Err(error.to_string()));
                        return;
                    }
                };
                let runtime = match self.ctx.spawn_runtime(
                    SessionTab {
                        session,
                        lease,
                        cursor: None,
                    },
                    Vec::new(),
                ) {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = reply_tx.send(Err(error));
                        return;
                    }
                };
                let idx = self.push_runtime(runtime);
                let id = self.sessions[idx].id();
                caudra_otel::emit::session_started(
                    caudra_otel::emit::START_FRESH,
                    Some(&id.to_string()),
                );
                if let Some(prompt) = prompt {
                    let _ = self.submit_text(idx, prompt, caudra_agent::PromptAdmission::Queue);
                }
                if focus {
                    self.focused = idx;
                }
                let _ = reply_tx.send(Ok(json!(id)));
            }
            SessionRequest::Prompt {
                id,
                text,
                admission,
            } => {
                let idx = match id {
                    None => Ok(self.focused),
                    Some(id) => parse_session_id(&id).and_then(|id| {
                        self.position(id)
                            .ok_or_else(|| format!("{NOT_LIVE_ERR}: {id}"))
                    }),
                };
                let _ = reply_tx.send(idx.and_then(|idx| self.submit_text(idx, text, admission)));
            }
            SessionRequest::Focus { id } => {
                let reply = parse_session_id(&id)
                    .and_then(|id| self.focus_session(id))
                    .map(|()| json!(true));
                let _ = reply_tx.send(reply);
            }
            SessionRequest::SetTitle { id, title } => {
                let reply = parse_session_id(&id)
                    .and_then(|id| self.set_session_title(id, &title))
                    .map(|()| json!(true));
                let _ = reply_tx.send(reply);
            }
        }
    }

    /// Clears the way for a delete: a live session is torn down first, a
    /// stored one is claimed by lease. The lease must outlive the erase, or
    /// another process could reopen the session halfway through it.
    fn release_for_delete(&mut self, id: CaudraId) -> Result<Arc<SessionLease>, String> {
        let Some(i) = self.position(id) else {
            return SessionLease::acquire(&self.ctx.storage, id)
                .map(Arc::new)
                .map_err(|error| error.to_string());
        };
        if i == self.focused {
            return Err(DELETE_FOCUSED_ERR.into());
        }
        if !self.sessions[i].quiescent() {
            return Err(DELETE_BUSY_ERR.into());
        }
        let SessionRuntime {
            mut handles, lease, ..
        } = self.remove_runtime(i);
        handles.shutdown_workflow();
        handles.cancel();
        Ok(lease)
    }

    /// A live session is retitled in memory and saved on its own schedule; a
    /// stored one is loaded, retitled, and written back under a lease.
    fn set_session_title(&mut self, id: CaudraId, title: &str) -> Result<(), String> {
        let title = normalize_title(title);
        if let Some(i) = self.position(id) {
            self.sessions[i].app.state.session_mut().set_title(title);
            return Ok(());
        }
        let _lease =
            SessionLease::acquire(&self.ctx.storage, id).map_err(|error| error.to_string())?;
        let mut session = load_app_session(id, &self.ctx.storage).map_err(|e| e.to_string())?;
        session.set_title(title);
        self.ctx
            .storage_writer
            .save_sync(Arc::new(session))
            .map_err(|error| error.to_string())
    }

    /// The prompt that opened the session, which is what a title is about. A
    /// live session answers from memory; a stored one is read under a lease,
    /// the same way [`Self::set_session_title`] writes one back.
    fn session_opening_prompt(&self, id: CaudraId) -> Result<String, String> {
        if let Some(i) = self.position(id) {
            return opening_prompt(self.sessions[i].app.state.session.messages());
        }
        let _lease =
            SessionLease::acquire(&self.ctx.storage, id).map_err(|error| error.to_string())?;
        let session = load_app_session(id, &self.ctx.storage).map_err(|e| e.to_string())?;
        opening_prompt(session.messages())
    }

    /// Detached, because a title takes a network round trip and the picker
    /// stays usable while it runs. The result comes back as [`Wake::Title`].
    fn spawn_title(&self, id: CaudraId, prompt: String) {
        let slot = self.ctx.model_slot.load_full();
        let timeouts = self.ctx.timeouts;
        let model_policy = Arc::clone(&self.ctx.model_policy);
        let title_tx = self.title_tx.clone();
        // A token nothing can fire, not a fresh pair: `CancelTrigger` cancels
        // on drop, so a trigger left behind here would kill the request before
        // it left the ground. The request's own timeout bounds it.
        let cancel = CancelToken::none();
        smol::spawn(async move {
            let result = caudra_agent::agent::title::for_prompt(
                &slot.provider,
                &slot.model,
                timeouts,
                &model_policy,
                &prompt,
                &cancel,
            )
            .await
            .map_err(|error| error.user_message())
            .and_then(|(_, outcome)| outcome.title.ok_or_else(|| NO_TITLE_ERR.to_owned()));
            let _ = title_tx.send(GeneratedTitle { id, result });
        })
        .detach();
    }

    fn apply_generated_title(&mut self, id: CaudraId, result: Result<String, String>) {
        let outcome = result.and_then(|title| self.set_session_title(id, &title));
        if let Err(error) = outcome {
            self.focused_app().flash(error);
        }
    }

    /// Lua acts on the focused session, the same target the model picker and
    /// `/thinking` write to.
    fn handle_model_request(&mut self, req: ModelRequest) -> UiReply {
        match req {
            ModelRequest::Get => Ok(self.focused_app().model_state()),
            ModelRequest::Available => {
                let available = self.ctx.available_models.load();
                Ok(json!(
                    available.as_deref().map(Vec::as_slice).unwrap_or(&[])
                ))
            }
            ModelRequest::Set {
                spec,
                thinking,
                fast,
            } => {
                if let Some(spec) = spec {
                    self.change_model(&spec)?;
                }
                let app = self.focused_app();
                if let Some(thinking) = thinking {
                    app.set_thinking(&thinking)?;
                }
                if let Some(fast) = fast {
                    app.set_fast(fast)?;
                }
                Ok(app.model_state())
            }
        }
    }

    fn handle_task_request(&mut self, req: TaskRequest) -> UiReply {
        match req {
            TaskRequest::List => Ok(json!(self.focused_app().tasks())),
            TaskRequest::Focus { id } => self.focused_app().focus_task(&id).map(|()| json!(true)),
        }
    }

    fn submit_text(
        &mut self,
        idx: usize,
        text: String,
        admission: caudra_agent::PromptAdmission,
    ) -> UiReply {
        let msg = QueuedMessage {
            text,
            images: Vec::new(),
            mentions: Vec::new(),
            commits: Vec::new(),
            paste_ranges: Vec::new(),
        };
        match self.sessions[idx]
            .app
            .submit_prompt_with_admission(msg, admission)
        {
            SubmitOutcome::Started(actions) => {
                self.dispatch(idx, actions);
                Ok(json!("started"))
            }
            SubmitOutcome::Queued => Ok(json!(match admission {
                caudra_agent::PromptAdmission::Queue => "queued",
                caudra_agent::PromptAdmission::Steer => "steered",
                caudra_agent::PromptAdmission::Interrupt => unreachable!(),
            })),
            SubmitOutcome::Replacing(actions) => {
                self.dispatch(idx, actions);
                Ok(json!("replacing"))
            }
            SubmitOutcome::Rejected(e) => Err(e.into()),
            SubmitOutcome::NeedsModeDecision(_) => Err(crate::app::MODE_DECISION_ERR.into()),
        }
    }

    fn position(&self, id: CaudraId) -> Option<usize> {
        self.sessions.iter().position(|rt| rt.id() == id)
    }

    /// The single place that removes a runtime: keeps `focused` pointing at
    /// the same session afterwards. The focused runtime itself is never
    /// removable, so `sessions` stays non-empty.
    fn remove_runtime(&mut self, idx: usize) -> SessionRuntime {
        debug_assert_ne!(idx, self.focused);
        let mut rt = self.sessions.remove(idx);
        rt.close_peer();
        if idx < self.focused {
            self.focused -= 1;
        }
        rt
    }

    fn push_runtime(&mut self, mut rt: SessionRuntime) -> usize {
        rt.install_peer(&self.ctx);
        self.sessions.push(rt);
        self.sessions.len() - 1
    }

    /// Focus a live session, or bring a stored one up: in place when the
    /// focused session is a blank idle one (nothing worth keeping), otherwise
    /// as a new runtime so the session you came from stays live.
    fn focus_session(&mut self, id: CaudraId) -> Result<(), String> {
        if crate::sandbox::transfer::active() && self.sessions[self.focused].id() != id {
            self.cancel_transfers();
            return Err(
                "Cancelling Transfer; retry switching sessions after cleanup completes".into(),
            );
        }
        if let Some(i) = self.position(id) {
            validate_session_focus(
                &self.sessions[i].app.state.session,
                self.sessions[i].app.workspace_session.as_ref(),
            )?;
            self.focused = i;
            return Ok(());
        }
        let lease = Arc::new(
            SessionLease::acquire(&self.ctx.storage, id)
                .map_err(|error| format!("Failed to open session: {error}"))?,
        );
        let (session, cursor) = open_app_session_with_cursor(id, &self.ctx.storage)
            .map_err(|e| format!("Failed to load session: {e}"))?;
        // Both branches below end with this session in a tab the writer saves.
        self.ctx.storage_writer.adopt_cursor(cursor);
        if self.ctx.workspace_session.is_none() {
            validate_session_focus(&session, None)?;
        }
        let (profile_name, profile) = self.ctx.resolve_prompt_profile(&session)?;
        let focused = &mut self.sessions[self.focused];
        if focused.quiescent() && !focused.app.has_content() && self.ctx.workspace_session.is_none()
        {
            let model = focused.app.state.model.clone();
            focused.close_peer();
            let loaded = match focused.app.apply_loaded_session(session, &model) {
                Ok(loaded) => loaded,
                Err(error) => {
                    focused.install_peer(&self.ctx);
                    return Err(error);
                }
            };
            focused.app.state.system_prompt_profile_name = profile_name;
            focused.app.state.system_prompt_profile = profile;
            focused.app.state.system_prompt_profile_override =
                self.ctx.prompt_profile_override.is_some();
            let old_lease = std::mem::replace(&mut self.sessions[self.focused].lease, lease);
            self.dispatch(self.focused, vec![Action::LoadSession(Box::new(loaded))]);
            drop(old_lease);
            return Ok(());
        }
        let runtime = self.ctx.spawn_runtime(
            SessionTab {
                session,
                lease,
                cursor: None,
            },
            Vec::new(),
        )?;
        let idx = self.push_runtime(runtime);
        self.focused = idx;
        Ok(())
    }

    fn cancel_transfers(&mut self) {
        for runtime in &mut self.sessions {
            if runtime.app.sandbox_live.transfer.is_some() {
                let action = runtime.app.workbench.cancel_transfer();
                if let WorkbenchAction::Transfer(action) = action {
                    runtime.app.handle_transfer_action(action);
                }
            }
        }
    }

    /// Handles one input event plus any leftover produced while coalescing
    /// bursts of scroll/drag events.
    fn handle_input(&mut self, raw: Event) {
        let mut pending = Some(raw);
        while let Some(ev) = pending.take() {
            let runtime = &self.sessions[self.focused];
            let acknowledge = (self.program_status_reporter.is_some()
                && acknowledges_program_status(&ev)
                && matches!(
                    runtime.program_status(),
                    ProgramStatus::Done | ProgramStatus::Error
                ))
            .then_some(runtime.id());
            let (msg, leftover) = self.translate(ev);
            if let Some(msg) = msg {
                let actions = self.sessions[self.focused].app.update(msg);
                self.dispatch(self.focused, actions);
            }
            let runtime = &mut self.sessions[self.focused];
            if acknowledge == Some(runtime.id()) {
                runtime.notifications.outcome = None;
            }
            pending = leftover;
        }
    }

    fn translate(&mut self, raw: Event) -> (Option<Msg>, Option<Event>) {
        let supports_focus_reporting = self
            .notifier
            .as_ref()
            .is_some_and(terminal::TerminalNotifier::supports_focus_reporting);
        if let Some(focused) = terminal_focus_event(&raw) {
            if supports_focus_reporting {
                self.terminal_focused = focused;
            }
            if focused {
                self.wake_appearance();
            }
            return (None, None);
        }
        if supports_focus_reporting && terminal_input_proves_focus(&raw) {
            self.terminal_focused = true;
        }
        match raw {
            Event::ColorSchemeChanged(scheme) => {
                self.sync_auto_theme();
                tracing::debug!(
                    ?scheme,
                    auto_switch = self.auto_theme.is_some(),
                    "terminal appearance report"
                );
                if let Some(auto) = self.auto_theme.as_mut() {
                    let _ = auto.observe(Some(Observation::Explicit(scheme.into())));
                }
                (None, None)
            }
            Event::Key(key) => (Some(Msg::Key(key)), None),
            Event::Paste(text) => (Some(Msg::Paste(text)), None),
            Event::Mouse(mouse) => self.translate_mouse(mouse),
            // Reattaching a multiplexer to another terminal resizes the
            // viewport, and that terminal may not share the old background.
            Event::Resize(..) => {
                for runtime in &mut self.sessions {
                    runtime.app.peer_manager.invalidate_layout();
                }
                self.wake_appearance();
                (None, None)
            }
            _ => (None, None),
        }
    }

    fn wake_appearance(&mut self) {
        if let Some(auto) = self.auto_theme.as_mut() {
            auto.wake();
        }
    }

    fn translate_mouse(&mut self, mouse: CtMouseEvent) -> (Option<Msg>, Option<Event>) {
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let scroll_lines = scroll_lines(self.focused_app().ui_config.mouse_scroll_lines);
                let (msg, leftover) = self.aggregate_scroll(mouse, scroll_lines);
                (Some(msg), leftover)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let (drag, leftover) = self.coalesce_drag(mouse);
                (Some(Msg::Mouse(drag)), leftover)
            }
            _ => (Some(Msg::Mouse(mouse)), None),
        }
    }

    /// Sums queued scroll events into one delta; the first non-scroll event
    /// drained along the way is returned so it isn't lost.
    ///
    /// Aggregation stops when the modifiers change, so releasing Alt partway
    /// through a burst cannot retroactively scale the notches already queued
    /// under it.
    fn aggregate_scroll(&self, first: CtMouseEvent, scroll_lines: u32) -> (Msg, Option<Event>) {
        let step = scroll_step(first.modifiers, scroll_lines);
        let mut delta = scroll_delta(first.kind, step);
        let mut leftover = None;
        while let Ok(next) = self.input.receiver().try_recv() {
            match next {
                Event::Mouse(m)
                    if m.modifiers == first.modifiers
                        && matches!(
                            m.kind,
                            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                        ) =>
                {
                    delta += scroll_delta(m.kind, step);
                }
                other => {
                    leftover = Some(other);
                    break;
                }
            }
        }
        (
            Msg::Scroll {
                column: first.column,
                row: first.row,
                delta,
            },
            leftover,
        )
    }

    /// Keeps only the newest queued drag position; the first non-drag event
    /// drained along the way is returned so it isn't lost.
    fn coalesce_drag(&self, mut latest: CtMouseEvent) -> (CtMouseEvent, Option<Event>) {
        let mut leftover = None;
        while let Ok(next) = self.input.receiver().try_recv() {
            match next {
                Event::Mouse(m) if matches!(m.kind, MouseEventKind::Drag(MouseButton::Left)) => {
                    latest = m;
                }
                other => {
                    leftover = Some(other);
                    break;
                }
            }
        }
        (latest, leftover)
    }

    fn dispatch(&mut self, mut idx: usize, actions: Vec<Action>) {
        if crate::sandbox::transfer::active() {
            return;
        }
        for action in actions {
            if self.relocation.is_some() {
                break;
            }
            if let Action::ClearAndImplement(handoff) = action {
                self.clear_and_implement(idx, *handoff);
                continue;
            }
            if matches!(&action, Action::RequestNewSession) {
                if !self.request_new_session(idx) {
                    break;
                }
                idx = self.focused;
                continue;
            }
            self.handle_action(idx, action);
        }
        let _ = self.sync_peers();
    }

    fn request_new_session(&mut self, idx: usize) -> bool {
        if !self.sessions[idx].work_quiescent() {
            let runtime = match self
                .ctx
                .spawn_fresh_runtime(&self.sessions[idx].app.state.session, None)
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    self.sessions[idx].app.flash(error);
                    return false;
                }
            };
            let child = self.push_runtime(runtime);
            self.focused = child;
            caudra_otel::emit::session_started(
                caudra_otel::emit::START_FRESH,
                Some(&self.sessions[child].id().to_string()),
            );
            return true;
        }
        self.sessions[idx].close_peer();
        let actions = self.sessions[idx].app.reset_session();
        if actions.is_empty() {
            self.sessions[idx].install_peer(&self.ctx);
            return false;
        }
        self.dispatch(idx, actions);
        true
    }

    fn clear_and_implement(&mut self, idx: usize, handoff: PlanHandoff) {
        let target = if self.sessions[idx].work_quiescent() {
            let actions = match self.sessions[idx].app.reset_session_for_plan() {
                Ok(actions) => actions,
                Err(error) => {
                    self.sessions[idx].app.flash(error);
                    return;
                }
            };
            self.dispatch(idx, actions);
            idx
        } else {
            let runtime = match self.ctx.spawn_plan_runtime(&self.sessions[idx].app) {
                Ok(runtime) => runtime,
                Err(error) => {
                    self.sessions[idx].app.flash(error);
                    return;
                }
            };
            self.sessions[idx].app.consume_plan();
            let child = self.push_runtime(runtime);
            self.focused = child;
            caudra_otel::emit::session_started(
                caudra_otel::emit::START_FRESH,
                Some(&self.sessions[child].id().to_string()),
            );
            child
        };
        let app = &mut self.sessions[target].app;
        app.adopt_plan(&handoff);
        let actions = app.finish_plan_handoff(handoff);
        self.dispatch(target, actions);
    }

    fn workspace_group_quiescent(&self, idx: usize) -> bool {
        if let Some(target) = self.sessions[idx].app.state.session.workspace_binding() {
            return matching_remote_workspace_quiescent(
                target,
                self.sessions.iter().map(|runtime| {
                    (
                        runtime.app.state.session.workspace_binding(),
                        runtime.quiescent(),
                    )
                }),
            );
        }
        let Ok(target) = canonical_cwd(Path::new(&self.sessions[idx].app.state.session.cwd)) else {
            return false;
        };
        matching_workspace_quiescent(
            &target,
            self.sessions.iter().map(|runtime| {
                (
                    Path::new(&runtime.app.state.session.cwd),
                    runtime.quiescent(),
                )
            }),
        )
    }

    fn relocation_inventory(&self) -> Result<Vec<SessionLocation>, String> {
        relocation_inventory(
            &self.ctx.storage,
            self.sessions
                .iter()
                .map(|runtime| runtime.app.state.session.as_ref()),
        )
    }

    fn reserve_workspace_restore(&mut self, idx: usize) -> Result<(), String> {
        if !self.workspace_group_quiescent(idx) {
            return Err(crate::app::REVERT_BUSY_MSG.into());
        }
        let target = &self.sessions[idx].app.state.session;
        let mut transitions = Vec::new();
        for runtime in &self.sessions {
            let session = &runtime.app.state.session;
            let same_workspace = match target.workspace_binding() {
                Some(binding) => session.workspace_binding() == Some(binding),
                None => {
                    canonical_cwd(Path::new(&target.cwd))?
                        == canonical_cwd(Path::new(&session.cwd))?
                }
            };
            if same_workspace {
                transitions.push(SessionTransition::reserve(&runtime.handles)?);
            }
        }
        self.sessions[idx].restore_transitions = transitions;
        for runtime in &mut self.sessions {
            runtime.close_peer();
            runtime.install_peer(&self.ctx);
        }
        Ok(())
    }

    fn relocation_available(&self) -> Result<(), String> {
        if self.ctx.storage.is_ephemeral()
            || self
                .sessions
                .iter()
                .any(|runtime| runtime.app.workspace_session.is_some())
        {
            return Err(RELOCATION_LOCAL_ERR.into());
        }
        Ok(())
    }

    fn open_session_relocation(&mut self, idx: usize, bulk: bool, destination: Option<String>) {
        let result = self
            .relocation_available()
            .and_then(|()| self.relocation_inventory());
        match result {
            Ok(locations) => {
                let other_open_count = self.sessions.len().saturating_sub(1);
                self.sessions[idx].app.open_session_relocation(
                    locations,
                    bulk,
                    destination,
                    other_open_count,
                );
            }
            Err(error) => self.sessions[idx].app.flash(error),
        }
    }

    fn prepare_relocation(
        &mut self,
        mut request: SessionRelocation,
        donor: Option<(CaudraId, String)>,
    ) -> Result<PendingRelocation, String> {
        self.relocation_available()?;
        if let Some(error) = cwd_change_blocker(self.sessions.iter().map(|runtime| {
            (
                runtime.quiescent(),
                runtime.app.state.session.meta.pending_revert.is_some(),
            )
        })) {
            return Err(error.into());
        }
        if self
            .sessions
            .iter()
            .any(|runtime| runtime.app.workbench_blocks_workspace_change())
        {
            return Err(RELOCATION_WORKBENCH_ERR.into());
        }
        let destination = canonical_cwd(Path::new(&request.destination))?;
        if destination != Path::new(&request.destination) {
            return Err(RELOCATION_CHANGED_ERR.into());
        }
        fs::read_dir(&destination).map_err(|error| error.to_string())?;
        check_relocation_destination(&self.ctx.storage, &destination)?;
        let inventory = self.relocation_inventory()?;
        for expected in &mut request.sessions {
            if let Some(index) = self.position(expected.id) {
                reconcile_relocation_live_version(
                    expected,
                    &self.sessions[index].app.state.session,
                    &inventory,
                )?;
            }
        }
        validate_relocation_selection(&request, &inventory, donor.as_ref())?;
        let mut leases = Vec::new();
        let mut selected: Vec<_> = request.sessions.iter().map(|entry| entry.id).collect();
        selected.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        for id in selected {
            if self.position(id).is_none() {
                leases.push(Arc::new(
                    SessionLease::acquire(&self.ctx.storage, id)
                        .map_err(|error| error.to_string())?,
                ));
            }
        }
        let mut workflows = Vec::new();
        for runtime in &self.sessions {
            workflows.push(SessionTransition::reserve(&runtime.handles)?);
        }
        let database =
            SessionDatabase::open_state(&self.ctx.storage).map_err(|error| error.to_string())?;
        for expected in &request.sessions {
            refuse_resumable_workflows(&database, expected.id)?;
        }
        for runtime in &mut self.sessions {
            runtime.app.checkpoint_now();
            self.ctx
                .storage_writer
                .save_sync_timeout(
                    Arc::clone(&runtime.app.state.session),
                    AGENT_SHUTDOWN_TIMEOUT,
                )
                .map_err(|error| format!("Failed to save session {}: {error}", runtime.id()))?;
        }
        let current = database
            .local_session_locations()
            .map_err(|error| error.to_string())?;
        for expected in &mut request.sessions {
            if let Some(index) = self.position(expected.id) {
                reconcile_relocation_live_version(
                    expected,
                    &self.sessions[index].app.state.session,
                    &current,
                )?;
            }
        }
        validate_relocation_selection(&request, &current, donor.as_ref())?;
        Ok(PendingRelocation {
            handoff: Handoff::Relocation(SessionRelocationHandoff {
                request,
                donor,
                leases,
            }),
            _workflows: workflows,
        })
    }

    /// Whether the process moved to `cwd`; a refusal is flashed.
    fn change_working_directory(&mut self, idx: usize, cwd: PathBuf) -> bool {
        if let Some(error) = cwd_change_blocker(self.sessions.iter().map(|runtime| {
            (
                runtime.quiescent(),
                runtime.app.state.session.meta.pending_revert.is_some(),
            )
        })) {
            self.sessions[idx].app.flash(error.into());
            return false;
        }

        let mut transitions = Vec::new();
        for runtime in &self.sessions {
            match SessionTransition::reserve(&runtime.handles) {
                Ok(transition) => transitions.push(transition),
                Err(error) => {
                    self.sessions[idx].app.flash(error);
                    return false;
                }
            }
        }
        for runtime in &mut self.sessions {
            runtime.close_peer();
        }
        if let Err(error) = std::env::set_current_dir(&cwd) {
            for runtime in &mut self.sessions {
                runtime.install_peer(&self.ctx);
            }
            self.sessions[idx].app.flash(format!("cd: {error}"));
            return false;
        }
        let permissions = load_permissions(&cwd);
        self.ctx
            .permissions
            .set_project_with_config(&cwd, permissions.clone());
        for runtime in &mut self.sessions {
            runtime.handles.shutdown_workflow();
            runtime
                .app
                .install_working_directory(&cwd, permissions.clone());
            runtime.app.checkpoint_now();
        }
        for index in 0..self.sessions.len() {
            let history = self.sessions[index]
                .app
                .shared_history
                .as_ref()
                .map(|history| history.load().messages.as_ref().clone())
                .unwrap_or_default();
            self.respawn_agent(index, history);
        }
        self.sessions[idx]
            .app
            .open_awaiting_permission_config_trust(false);
        let mut save_error = None;
        for runtime in &self.sessions {
            if runtime.app.has_content()
                && let Err(error) = self
                    .ctx
                    .storage_writer
                    .save_sync(Arc::clone(&runtime.app.state.session))
                && save_error.is_none()
            {
                save_error = Some(error);
            }
        }
        if let Some(error) = save_error {
            self.sessions[idx].app.flash(format!(
                "cd: changed working directory but failed to persist every session: {error}"
            ));
        } else {
            self.sessions[idx]
                .app
                .flash(format!("cd {}", cwd.display()));
        }
        true
    }

    /// Opens a session that works in another checkout of this repository.
    /// Inside Herdr it resumes in that checkout's workspace; otherwise this
    /// process changes directory there first, as `/cd` would, and a session
    /// another Caudra has open is refused before anything moves.
    fn open_session_elsewhere(&mut self, idx: usize, id: CaudraId, cwd: &Path) {
        if let Some(herdr) = self.worktrees.herdr() {
            let outcome = resume_in_herdr(herdr, &self.ctx.storage, id, cwd);
            self.sessions[idx]
                .app
                .flash(outcome.unwrap_or_else(|error| error));
            return;
        }
        let opened = SessionLease::acquire(&self.ctx.storage, id)
            .map_err(|error| format!("Failed to open session: {error}"))
            .and_then(|_| cwd.canonicalize().map_err(|error| format!("cd: {error}")));
        let cwd = match opened {
            Ok(cwd) => cwd,
            Err(error) => {
                self.sessions[idx].app.flash(error);
                return;
            }
        };
        if self.change_working_directory(idx, cwd)
            && let Err(error) = self.focus_session(id)
        {
            self.sessions[idx].app.flash(error);
        }
    }

    fn open_worktrees(&mut self, idx: usize, view: WorktreeView) {
        let overview = self
            .relocation_available()
            .and_then(|()| self.worktree_overview(idx));
        match overview {
            Ok(overview) => self.sessions[idx].app.open_worktrees(overview, view),
            Err(error) => self.sessions[idx].app.flash(error),
        }
    }

    /// The repository the session at `idx` works in, once the sessions left
    /// in removed worktrees have moved back, so every count is current.
    fn worktree_overview(&mut self, idx: usize) -> Result<WorktreeOverview, String> {
        let app = &mut self.sessions[idx].app;
        app.move_back_from_removed_worktrees();
        let session = &app.state.session;
        let cwd = canonical_cwd(Path::new(&session.cwd))?;
        let current = checkout::discover(&cwd).ok_or(WORKTREE_REPOSITORY_ERR)?;
        let checkouts =
            worktrees::checkouts(&self.ctx.storage, &cwd).map_err(|error| error.to_string())?;
        let dirty = Git::new(&current.root)
            .is_dirty()
            .map_err(|error| error.to_string())?;
        Ok(WorktreeOverview {
            session: session.id,
            cwd,
            current: current.root,
            main_root: current.main_root,
            checkouts,
            dirty,
            herdr: self.worktrees.herdr().is_some(),
        })
    }

    /// Inside Herdr the checkout opens in its own workspace; otherwise every
    /// tab moves into it, as `/cd` would take them.
    fn open_worktree(&mut self, idx: usize, root: &Path) {
        if let Some(herdr) = self.worktrees.herdr() {
            let outcome = open_in_herdr(herdr, root);
            self.sessions[idx]
                .app
                .flash(outcome.unwrap_or_else(|error| error));
            return;
        }
        let located =
            canonical_cwd(Path::new(&self.sessions[idx].app.state.session.cwd)).and_then(|cwd| {
                let current = checkout::discover(&cwd).ok_or(WORKTREE_REPOSITORY_ERR)?;
                Ok(counterpart(&cwd, &current.root, root))
            });
        match located {
            Ok(destination) => {
                self.change_working_directory(idx, destination);
            }
            Err(error) => self.sessions[idx].app.flash(error),
        }
    }

    fn inspect_worktree_removal(&mut self, idx: usize, root: PathBuf) {
        match Git::new(&root).is_dirty() {
            Ok(dirty) => self.sessions[idx].app.show_worktree_removal(root, dirty),
            Err(error) => self.sessions[idx].app.flash(error.to_string()),
        }
    }

    /// Checks what `/cd` and relocation check, since the UI restarts around
    /// every worktree change, and reserves each session's workflows until
    /// then. A new worktree's session moves, so it must be free to.
    fn prepare_worktree(&self, request: WorktreeRequest) -> Result<PendingRelocation, String> {
        self.relocation_available()?;
        if let Some(error) = cwd_change_blocker(self.sessions.iter().map(|runtime| {
            (
                runtime.quiescent(),
                runtime.app.state.session.meta.pending_revert.is_some(),
            )
        })) {
            return Err(error.into());
        }
        if self
            .sessions
            .iter()
            .any(|runtime| runtime.app.workbench_blocks_workspace_change())
        {
            return Err(RELOCATION_WORKBENCH_ERR.into());
        }
        if let WorktreeRequest::Create(create) = &request {
            let database = SessionDatabase::open_state(&self.ctx.storage)
                .map_err(|error| error.to_string())?;
            refuse_resumable_workflows(&database, create.session)?;
        }
        let mut workflows = Vec::with_capacity(self.sessions.len());
        for runtime in &self.sessions {
            workflows.push(SessionTransition::reserve(&runtime.handles)?);
        }
        Ok(PendingRelocation {
            handoff: Handoff::Worktree(request),
            _workflows: workflows,
        })
    }

    fn respawn_agent(&mut self, idx: usize, history: Vec<HistoryItem>) {
        let rt = &mut self.sessions[idx];
        rt.close_peer();
        rt.reset_run_notifications();
        let lua_handle = rt.app.lua_event_handle.clone();
        let permissions = Arc::clone(&rt.app.permissions);
        rt.handles.respawn(
            history,
            &self.ctx.model_slot,
            self.ctx.config.clone(),
            self.ctx.ui_config.tool_output_lines,
            &permissions,
            &mut rt.app,
            lua_handle,
            Some(Arc::clone(&rt.lease)),
        );
        rt.handles
            .start_automations(&mut rt.app, &self.ctx.automations, Vec::new());
        rt.install_peer(&self.ctx);
    }

    fn handle_action(&mut self, idx: usize, action: Action) {
        match action {
            Action::ListPeers => self.list_peers(idx),
            Action::PeerMessages(args) => self.peer_messages(idx, &args),
            Action::PeerTopics(args) => self.peer_topics(idx, &args),
            Action::PeerGroups(args) => self.peer_groups(idx, &args),
            Action::PeerSubscribe(change) => self.change_peer_subscriptions(idx, change),
            Action::RefreshPeers => self.refresh_peers(idx),
            Action::ReviewPeerMessage(id) => self.review_peer_message(idx, &id),
            Action::DecidePeerMessage { token, decision } => {
                self.decide_peer_message(idx, token, decision);
            }
            Action::SetPeerInbound(policy) => self.set_peer_inbound(idx, policy),
            Action::SendMessage(input) => {
                let rt = &mut self.sessions[idx];
                rt.reset_run_notifications();
                let mut input = *input;
                prepend_preamble(&mut input.preamble, rt.app.shell.drain_results());
                let run_id = rt.app.run_id;
                rt.handles.queue.push(QueueItem::Message {
                    text: input.message.clone(),
                    image_count: input.images.len(),
                    paste_ranges: Vec::new(),
                    input: Box::new(input),
                    run_id,
                    admission: caudra_agent::PromptAdmission::Queue,
                    displayed: true,
                });
            }
            Action::CancelAgent { run_id } => {
                let rt = &mut self.sessions[idx];
                rt.notifications.reset();
                rt.pause_peer_work();
                let _ = rt.handles.cmd_tx.try_send(AgentCommand::Cancel { run_id });
                rt.app.stop_background_work();
            }
            Action::CancelSubagent { tool_use_id } => {
                if let Some(background) = &self.sessions[idx].handles.background
                    && background.resident_status(&tool_use_id).is_some()
                {
                    if let Err(error) = smol::block_on(background.cancel(&tool_use_id)) {
                        self.sessions[idx].app.flash(error);
                    }
                    return;
                }
                let _ = self.sessions[idx]
                    .handles
                    .cmd_tx
                    .try_send(AgentCommand::CancelSubagent { tool_use_id });
            }
            Action::RequestNewSession | Action::ClearAndImplement(_) => {
                unreachable!("handled by dispatch")
            }
            Action::FocusSession(id) => {
                if let Err(error) = self.focus_session(id) {
                    self.sessions[idx].app.flash(error);
                }
            }
            Action::OpenSessionElsewhere { id, cwd } => self.open_session_elsewhere(idx, id, &cwd),
            Action::DeleteSession(id) => match self.release_for_delete(id) {
                Ok(lease) => self.ctx.storage_writer.delete(id, move |res| {
                    let _lease = lease;
                    if let Err(error) = res {
                        tracing::error!(%error, %id, "failed to delete session");
                    }
                }),
                Err(error) => self.sessions[idx].app.flash(error),
            },
            Action::SetSessionTitle { id, title } => {
                if let Err(error) = self.set_session_title(id, &title) {
                    self.sessions[idx].app.flash(error);
                }
            }
            Action::GenerateSessionTitle(id) => match self.session_opening_prompt(id) {
                Ok(prompt) => {
                    self.spawn_title(id, prompt);
                    self.sessions[idx].app.flash(NAMING_SESSION.into());
                }
                Err(error) => self.sessions[idx].app.flash(error),
            },
            Action::NewSession(lease) => {
                let old_lease = std::mem::replace(&mut self.sessions[idx].lease, lease);
                self.respawn_agent(idx, Vec::new());
                drop(old_lease);
            }
            Action::LoadSession(loaded) => {
                let loaded = *loaded;
                if loaded.model_spec != self.ctx.model_slot.load().model.spec()
                    && self.ctx.model_policy.allows(&loaded.model_spec)
                    && let Ok(mut new_model) = Model::from_spec(&loaded.model_spec)
                    && let Ok(new_provider) = from_model(&mut new_model, self.ctx.timeouts)
                {
                    self.sessions[idx].app.usage_slot.store(None);
                    self.ctx.model_slot.store(Arc::new(ModelSlot {
                        model: new_model,
                        provider: Arc::from(new_provider),
                    }));
                }
                self.respawn_agent(idx, loaded.messages);
            }
            Action::ForkSession(forked) => {
                let ForkedSession {
                    mut session,
                    lease,
                    draft,
                    warning,
                } = *forked;
                if let Some(draft) = draft {
                    install_fork_draft(&mut session, draft);
                }
                let runtime = match self.ctx.spawn_runtime(
                    SessionTab {
                        session,
                        lease,
                        cursor: None,
                    },
                    Vec::new(),
                ) {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        self.sessions[idx].app.flash(error);
                        return;
                    }
                };
                let child = self.push_runtime(runtime);
                let id = self.sessions[child].id();
                if let Some(warning) = warning {
                    self.sessions[child].app.flash(warning);
                }
                self.sessions[child].app.checkpoint_now();
                self.focused = child;
                caudra_otel::emit::session_started(
                    caudra_otel::emit::START_FORK,
                    Some(&id.to_string()),
                );
            }
            Action::RevertSession { source, mode } => {
                if let Err(error) = self.reserve_workspace_restore(idx) {
                    self.sessions[idx].app.flash(error);
                    return;
                }
                let actions = self.sessions[idx].app.revert_at(source, mode);
                self.dispatch(idx, actions);
            }
            Action::RewindSession(entry) => {
                if let Err(error) = self.reserve_workspace_restore(idx) {
                    self.sessions[idx].app.flash(error);
                    return;
                }
                let actions = self.sessions[idx].app.rewind_to(entry);
                self.dispatch(idx, actions);
            }
            Action::UnrevertSession => {
                if let Err(error) = self.reserve_workspace_restore(idx) {
                    self.sessions[idx].app.flash(error);
                    return;
                }
                let actions = self.sessions[idx].app.unrevert();
                self.dispatch(idx, actions);
            }
            Action::ChangeWorkingDirectory(cwd) => {
                self.change_working_directory(idx, cwd);
            }
            Action::OpenSessionRelocation { bulk, destination } => {
                self.open_session_relocation(idx, bulk, destination);
            }
            Action::RelocateSessions { request, donor } => {
                match self.prepare_relocation(request, donor) {
                    Ok(relocation) => {
                        self.focused = idx;
                        self.relocation = Some(relocation);
                    }
                    Err(error) => self.sessions[idx].app.flash(error),
                }
            }
            Action::OpenWorktrees(view) => self.open_worktrees(idx, view),
            Action::OpenWorktree(root) => self.open_worktree(idx, &root),
            Action::InspectWorktreeRemoval(root) => self.inspect_worktree_removal(idx, root),
            Action::RunWorktree(request) => match self.prepare_worktree(request) {
                Ok(pending) => {
                    self.focused = idx;
                    self.relocation = Some(pending);
                }
                Err(error) => self.sessions[idx].app.flash(error),
            },
            Action::RemoteControl(args) => {
                if matches!(
                    WorkspaceControlCommand::parse(&args),
                    Ok(WorkspaceControlCommand::Reconnect)
                ) && let Some(authority) = self.sessions[idx]
                    .app
                    .workspace_session
                    .as_ref()
                    .map(|workspace| workspace.binding().authority().clone())
                {
                    for runtime in &mut self.sessions {
                        if runtime
                            .app
                            .workspace_session
                            .as_ref()
                            .is_some_and(|workspace| workspace.binding().authority() == &authority)
                        {
                            runtime.app.invalidate_permission_authority();
                        }
                    }
                }
                let runtime = &self.sessions[idx];
                spawn_remote_control(
                    runtime.app.workspace_session.clone(),
                    args,
                    runtime.shell_tx.clone(),
                );
            }
            Action::ChangeRemoteWorkingDirectory(path) => {
                let runtime = &mut self.sessions[idx];
                if !runtime.quiescent() {
                    runtime.app.flash(
                        "Cannot change the remote directory while the session has active or queued work"
                            .into(),
                    );
                    return;
                }
                let Some(workspace) = runtime.app.workspace_session.clone() else {
                    runtime
                        .app
                        .flash("cd: remote workspace is unavailable".into());
                    return;
                };
                let Some(binding) = runtime.app.state.session.workspace_binding().cloned() else {
                    runtime
                        .app
                        .flash("cd: session workspace binding is unavailable".into());
                    return;
                };
                spawn_remote_cd(
                    workspace,
                    binding,
                    path,
                    self.ctx.config.features,
                    runtime.shell_tx.clone(),
                );
            }
            Action::ChangeModel(spec) => {
                if let Err(e) = self.change_model(&spec) {
                    self.focused_app().flash(e);
                }
            }
            Action::CompleteProviderSetup(spec) => {
                if let Err(error) = self.change_model_with(&spec, App::select_setup_model) {
                    self.focused_app().flash(error);
                }
            }
            Action::ChangeSystemPromptProfile(name) => {
                self.change_system_prompt_profile(idx, &name);
            }
            Action::RefreshProvider { slug } => self.refresh_provider(slug),
            Action::AuthenticateProvider {
                provider,
                model_spec,
            } => {
                let storage = self.ctx.storage.clone();
                let pause = match self.input.try_pause() {
                    Ok(pause) => pause,
                    Err(error) => {
                        self.sessions[idx].app.flash(error);
                        return;
                    }
                };
                let result = terminal::with_normal_terminal(self.terminal, || match provider {
                    crate::components::SubscriptionProvider::Anthropic => {
                        anthropic_login_command().and_then(run_oauth_login)
                    }
                    crate::components::SubscriptionProvider::OpenAi => {
                        caudra_providers::openai_auth::login(&storage).map_err(Into::into)
                    }
                });
                drop(pause);
                self.terminal_focused = false;
                if let Some(reporter) = &mut self.program_status_reporter {
                    reporter.invalidate();
                }

                match result {
                    Ok(()) => {
                        if let Err(error) =
                            self.change_model_with(&model_spec, App::select_setup_model)
                        {
                            self.sessions[idx].app.flash(error);
                        }
                        self.refresh_models();
                        self.sessions[idx].app.flash(format!(
                            "Authenticated with {} subscription",
                            provider.display_name()
                        ));
                    }
                    Err(error) => self.sessions[idx].app.flash(format!(
                        "{} login failed: {error:#}",
                        provider.display_name()
                    )),
                }
            }
            Action::Bind(purpose, binding) => {
                let result = caudra_providers::model_registry::set_binding_and_persist(
                    purpose,
                    binding.clone(),
                    &self.ctx.storage,
                );
                let message = match result {
                    Ok(()) => format!("{} model: {binding}", purpose.label()),
                    Err(error) => format!("Failed to bind {} model: {error}", purpose.label()),
                };
                self.sessions[idx].app.flash(message);
            }
            Action::Unbind(purpose) => {
                let result = caudra_providers::model_registry::clear_binding_and_persist(
                    purpose,
                    &self.ctx.storage,
                );
                let message = match result {
                    Ok(()) => format!("{} model: {UNBOUND_FLASH}", purpose.label()),
                    Err(error) => format!("Failed to unbind {} model: {error}", purpose.label()),
                };
                self.sessions[idx].app.flash(message);
            }
            Action::Compact => {
                let rt = &mut self.sessions[idx];
                rt.reset_run_notifications();
                let run_id = rt.app.run_id;
                rt.handles.queue.push(QueueItem::Compact { run_id });
            }
            Action::ToggleMcp(server_name, enabled) => {
                self.sessions[idx].handles.send_mcp(McpCommand::Toggle {
                    server: server_name,
                    enabled,
                });
            }
            Action::TrustMcpOnce(server_name) => {
                self.sessions[idx].handles.send_mcp(McpCommand::TrustOnce {
                    server: server_name,
                });
            }
            Action::TrustMcpProject(server_name) => {
                self.sessions[idx]
                    .handles
                    .send_mcp(McpCommand::TrustProject {
                        server: server_name,
                    });
            }
            Action::RejectMcp(server_name) => {
                self.sessions[idx].handles.send_mcp(McpCommand::Reject {
                    server: server_name,
                });
            }
            Action::ShellCommand {
                id,
                command,
                visible,
            } => {
                let rt = &mut self.sessions[idx];
                if let Some(reason) = rt.app.sandbox_network_dispatch_blocker() {
                    rt.app.shell.release_id(&id);
                    rt.app.flash(reason.into());
                    return;
                }
                let (trigger, cancel) = CancelToken::new();
                rt.app.shell.add_trigger(trigger);
                if let Some(workspace) = rt.app.workspace_session.clone() {
                    spawn_remote_shell(
                        RemoteShellTarget { workspace },
                        command,
                        id,
                        visible,
                        rt.shell_tx.clone(),
                        cancel,
                        self.ctx.config.clone(),
                    );
                } else {
                    spawn_shell(
                        command,
                        id,
                        visible,
                        rt.shell_tx.clone(),
                        cancel,
                        self.ctx.config.clone(),
                    );
                }
            }
            Action::OpenUrl(target) => {
                if terminal::local_url_opener_available() {
                    let valid = url::Url::parse(&target).is_ok_and(|url| {
                        matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
                    });
                    if !valid {
                        self.sessions[idx]
                            .app
                            .flash(format!("Invalid web URL: {target}"));
                    } else if let Err(error) = open::that(&target) {
                        self.sessions[idx]
                            .app
                            .flash(format!("Failed to open URL: {error}"));
                    }
                }
            }
            Action::Btw(question) => {
                self.sessions[idx].app.start_btw(question);
            }
            Action::Extract => {
                let chat = self.ctx.model_slot.load_full();
                self.sessions[idx]
                    .app
                    .start_extract(self.ctx.timeouts, &chat);
            }
            Action::RefreshModels => self.refresh_models(),
            Action::RefreshUsage => self.refresh_usage(),
            Action::RefreshStorage => self.refresh_storage(),
            Action::ManualExit => self.sessions[idx].notifications.on_manual_exit(),
        }
    }

    fn change_model(&mut self, spec: &str) -> Result<(), String> {
        self.change_model_with(spec, App::select_model)
    }

    fn change_model_with(
        &mut self,
        spec: &str,
        select: fn(&mut App, &Model),
    ) -> Result<(), String> {
        if !self.ctx.model_policy.allows(spec) {
            return Err(format!("{MODEL_POLICY_ERR}: {spec}"));
        }
        let mut new_model =
            Model::from_spec(spec).map_err(|e| format!("{INVALID_MODEL_ERR}: {e}"))?;
        let new_provider = from_model(&mut new_model, self.ctx.timeouts)
            .map_err(|e| format!("{PROVIDER_INIT_ERR}: {e}"))?;
        let app = self.focused_app();
        select(app, &new_model);
        app.record_recent_model(spec);
        app.usage_slot.store(None);
        self.ctx.model_slot.store(Arc::new(ModelSlot {
            model: new_model,
            provider: Arc::from(new_provider),
        }));
        Ok(())
    }

    fn change_system_prompt_profile(&mut self, idx: usize, name: &str) {
        if self.sessions[idx].app.state.system_prompt_profile_name == name {
            return;
        }
        if self.ctx.prompt_profile_override.is_some() {
            self.sessions[idx].app.flash(
                "System prompt is fixed by --system-prompt-profile for this invocation".into(),
            );
            return;
        }
        if !self.sessions[idx].quiescent() {
            self.sessions[idx].app.flash(
                "Cannot switch system prompt while the session has active or queued work".into(),
            );
            return;
        }
        let profile = match self.ctx.resolve_bound_profile(Some(name)) {
            Ok(profile) => profile,
            Err(error) => {
                self.sessions[idx].app.flash(error.to_string());
                return;
            }
        };
        let history = self.sessions[idx]
            .app
            .shared_history
            .as_ref()
            .map(|history| history.load().messages.as_ref().clone())
            .unwrap_or_default();
        self.sessions[idx].app.state.system_prompt_profile_name = name.to_owned();
        self.sessions[idx].app.state.system_prompt_profile = profile;
        self.sessions[idx].app.invalidate_permission_authority();
        self.sessions[idx].app.checkpoint_now();
        self.sessions[idx]
            .app
            .flash(format!("System prompt: {name}"));
        self.respawn_agent(idx, history);
        self.sessions[idx].app.sync_automation_profile();
    }

    fn refresh_models(&self) {
        let available = Arc::clone(&self.ctx.available_models);
        let warn_tx = self.warn_tx.clone();
        let policy = Arc::clone(&self.ctx.model_policy);
        available.store(None);
        smol::spawn(async move {
            fetch_all_models(
                &policy,
                |batch| merge_batch(&available, batch, &warn_tx),
                None,
            )
            .await;
        })
        .detach();
    }

    fn refresh_usage(&mut self) {
        let provider = Arc::clone(&self.ctx.model_slot.load().provider);
        let slot = Arc::clone(&self.focused_app().usage_slot);
        slot.store(Some(Arc::new(UsageFetchState::Loading)));
        smol::spawn(async move {
            let state = match provider.fetch_usage().await {
                Ok(Some(usage)) => UsageFetchState::Ready(usage),
                Ok(None) => UsageFetchState::Unsupported,
                Err(e) => UsageFetchState::Error(e.user_message()),
            };
            slot.store(Some(Arc::new(state)));
        })
        .detach();
    }

    /// Reading the change record stores walks them on disk, so the whole
    /// measurement goes to a blocking thread and the database is opened
    /// read-only: a diagnostic must never contend with the session writer.
    fn refresh_storage(&mut self) {
        let storage = self.ctx.storage.clone();
        let app = self.focused_app();
        let unavailable = app.record_index_blocker();
        let slot = Arc::clone(&app.storage_slot);
        slot.store(Some(Arc::new(StorageFetchState::Loading)));
        smol::spawn(async move {
            let state = smol::unblock(move || measure_storage(&storage, unavailable)).await;
            slot.store(Some(Arc::new(state)));
        })
        .detach();
    }

    fn refresh_provider(&mut self, slug: String) {
        let mut model = self.ctx.model_slot.load().model.clone();
        if model.provider.to_string() == slug {
            if let Ok(provider) =
                caudra_providers::provider::from_model(&mut model, self.ctx.timeouts)
            {
                self.focused_app().usage_slot.store(None);
                self.ctx.model_slot.store(Arc::new(ModelSlot {
                    model,
                    provider: Arc::from(provider),
                }));
            }
        } else if let Some(builtin) = caudra_config::providers::builtin_provider(&slug)
            && let Err(e) = self.change_model(builtin.default_model)
        {
            self.focused_app().flash(e);
        }
    }

    fn drain_shutdown_envelopes(&mut self) -> bool {
        let mut drained = false;
        for index in 0..self.sessions.len() {
            while let Ok(envelope) = self.sessions[index].handles.agent_rx.try_recv() {
                self.handle_agent(index, Box::new(envelope));
                drained = true;
            }
            while let Ok(event) = self.sessions[index].shell_rx.try_recv() {
                self.sessions[index].app.handle_shell_event(event);
                drained = true;
            }
        }
        drained
    }

    fn shutdown(mut self, status: ProgramStatus) -> Result<ShutdownReport> {
        let started = Instant::now();
        let relocating =
            self.relocation.is_some() || self.sandbox.is_some() || self.sandbox_control.is_some();
        let mut relocation_error = None;
        let mut phase_start = started;
        let mut lap = || {
            let elapsed = phase_start.elapsed().as_millis() as u64;
            phase_start = Instant::now();
            elapsed
        };
        let exit = self.sessions[self.focused].app.exit_request;
        if let Some(ref h) = self.ctx.mcp_handle {
            mcp::kill_process_groups(&h.reader().load().pids);
        }
        // Workflows first: their runs are interrupted and journaled while the
        // store is still open, and their agents are gone before the loops
        // are asked to quiesce.
        for rt in &mut self.sessions {
            rt.close_peer();
            rt.app.prepare_shutdown();
            rt.handles.shutdown_workflow();
            let _ = rt.handles.cmd_tx.try_send(AgentCommand::CancelAll);
        }
        let kill_mcp_ms = lap();
        let deadline = Instant::now() + AGENT_SHUTDOWN_TIMEOUT;
        loop {
            self.drain_shutdown_envelopes();
            if self.sessions.iter().all(SessionRuntime::shutdown_quiescent) {
                break;
            }
            if Instant::now() >= deadline {
                if relocating {
                    relocation_error = Some("agents did not become idle".to_owned());
                }
                for runtime in self
                    .sessions
                    .iter()
                    .filter(|runtime| !runtime.shutdown_quiescent())
                {
                    warn!(
                        session_id = %runtime.id(),
                        status = SessionStatus::of(&runtime.app).as_str(),
                        work = ?runtime.app.session_work(),
                        background_tasks = runtime.handles.active_background_tasks(),
                        shells = runtime.app.shell.active_ids().len(),
                        pending_events = runtime.handles.agent_rx.len() + runtime.shell_rx.len(),
                        timeout = ?AGENT_SHUTDOWN_TIMEOUT,
                        "session did not quiesce, forcing shutdown"
                    );
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.drain_shutdown_envelopes();
        let final_workspace_tabs = self.workspace_tabs_snapshot();

        let mut apps = Vec::with_capacity(self.sessions.len());
        let mut agent_tasks = Vec::with_capacity(self.sessions.len());
        for rt in self.sessions.drain(..) {
            let SessionRuntime {
                mut app,
                lease,
                handles,
                ..
            } = rt;
            app.disconnect_agent_queue();
            apps.push((app, lease));
            agent_tasks.push(handles.into_task());
        }
        let agents_joined = crate::agent::join_all(
            agent_tasks,
            deadline.saturating_duration_since(Instant::now()),
        );
        if relocating && !agents_joined {
            relocation_error.get_or_insert_with(|| "agents did not finish".to_owned());
        }
        let join_agents_ms = lap();

        let mut tabs = Vec::with_capacity(apps.len());
        // Split across the two operations so a slow exit points at one of
        // them instead of at the whole phase.
        let (mut checkpoint_ms, mut session_clone_ms) = (0, 0);
        for (mut app, lease) in apps {
            let mut step = Instant::now();
            let mut step_ms = || {
                let elapsed = step.elapsed().as_millis() as u64;
                step = Instant::now();
                elapsed
            };
            app.checkpoint_now();
            if relocating
                && let Err(error) = self
                    .ctx
                    .storage_writer
                    .save_sync_timeout(Arc::clone(&app.state.session), AGENT_SHUTDOWN_TIMEOUT)
            {
                relocation_error = Some(format!(
                    "failed to save session {}: {error}",
                    app.state.session.id
                ));
            }
            checkpoint_ms += step_ms();
            tabs.push(SessionTab {
                session: Arc::unwrap_or_clone(app.state.session),
                lease,
                cursor: None,
            });
            session_clone_ms += step_ms();
        }
        if !self.ctx.storage.is_ephemeral() {
            self.ctx
                .storage_writer
                .persist_workspace_tabs(final_workspace_tabs.cwd, final_workspace_tabs.tabs);
        }
        let save_sessions_ms = lap();
        if let Some(ref h) = self.ctx.mcp_handle {
            smol::block_on(h.shutdown());
        }
        let mcp_shutdown_ms = lap();
        match Arc::try_unwrap(self.ctx.storage_writer) {
            Ok(writer) if relocating => {
                if let Err(error) = writer.shutdown_checked(AGENT_SHUTDOWN_TIMEOUT) {
                    relocation_error = Some(error.to_string());
                }
            }
            Ok(writer) => writer.shutdown(AGENT_SHUTDOWN_TIMEOUT),
            Err(_) => {
                warn!("storage writer has outstanding references, skipping graceful shutdown");
                if relocating {
                    relocation_error = Some("storage writer still has active owners".to_owned());
                }
            }
        }
        let storage_drain_ms = lap();
        info!(
            kill_mcp_ms,
            join_agents_ms,
            save_sessions_ms,
            checkpoint_ms,
            session_clone_ms,
            mcp_shutdown_ms,
            storage_drain_ms,
            total_ms = started.elapsed().as_millis() as u64,
            "ui shutdown phases"
        );
        if let Some(reporter) = &mut self.program_status_reporter
            && let Err(error) = reporter.observe(if relocation_error.is_some() {
                ProgramStatus::Error
            } else {
                status
            })
        {
            warn!(%error, "failed to report final program status");
        }
        if let Some(error) = relocation_error {
            return Err(eyre!("{RELOCATION_SHUTDOWN_ERR}: {error}"));
        }
        Ok(ShutdownReport {
            exit,
            tabs,
            focused: self.focused,
            run_time: self.started.elapsed(),
            relocation: self.relocation.map(|pending| pending.handoff),
            sandbox: self.sandbox,
            sandbox_control: self.sandbox_control,
        })
    }
}

fn install_fork_draft(session: &mut AppSession, draft: ForkDraft) {
    session.meta.input_draft = Some(draft.text);
    session.meta.input_draft_images = draft
        .images
        .into_iter()
        .map(|image| StoredImage {
            media_type: image.media_type.mime().into(),
            data: image.data.to_string(),
        })
        .collect();
}

/// Under touch a wheel event is one row of finger travel, not one notch of a
/// detented wheel, so any multiplier makes the content outrun the finger.
/// `ui.touch = "off"` is the way back to the configured value.
fn scroll_lines(configured: u32) -> u32 {
    match caudra_workbench::scroll::touch() {
        true => TOUCH_SCROLL_LINES,
        false => configured,
    }
}

fn scroll_delta(kind: MouseEventKind, lines: u32) -> i32 {
    if kind == MouseEventKind::ScrollUp {
        lines as i32
    } else {
        -(lines as i32)
    }
}

/// Alt turns the wheel into the coarse one. Shift is deliberately left alone:
/// terminals widely translate it into `ScrollLeft`/`ScrollRight` themselves,
/// which this app already spends on panning a diagram.
fn scroll_step(modifiers: KeyModifiers, lines: u32) -> u32 {
    match modifiers.contains(KeyModifiers::ALT) {
        true => lines.saturating_mul(FAST_SCROLL_FACTOR),
        false => lines,
    }
}

/// Only the two transitions worth interrupting for: a background session that
/// wants an answer, and one that just finished. Everything else is noise.
fn background_flash(title: &str, previous: SessionStatus, status: SessionStatus) -> Option<String> {
    let mark = match (previous, status) {
        (_, SessionStatus::NeedsInput) => NEEDS_INPUT_MARK,
        (SessionStatus::Working, SessionStatus::Idle) => FINISHED_MARK,
        _ => return None,
    };
    Some(format!("{mark} {title} · {SESSIONS_COMMAND}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::automation_runtime::{COURIER_MESSAGE, LinkedAutomations};
    use crate::app::tests::{plan_app, private_tempdir, test_app};
    use crate::app::{Mode, PlanState};
    use crate::components::docs_modal::fixture as docs_fixture;
    use crate::components::{key, test_model};
    use crate::sandbox::transfer::{TransferCommand, TransferLink, TransferScope};
    use caudra_agent::background::BackgroundTasks;
    use caudra_agent::tools::native::plan::PlanTarget;
    use caudra_agent::{AgentMode, McpSnapshotReader, SubagentInfo, ToolDoneEvent};
    use caudra_automation::event::InputKind;
    use caudra_config::providers::{Protocol, ProviderDef};
    use caudra_config::sandbox::Revision;
    use caudra_config::{FeatureFlags, PermissionsConfig, ToolKey};
    use caudra_providers::provider::BoxFuture;
    use caudra_providers::{
        AgentError, CacheKey, ModelInfo, ProviderEvent, RequestOptions, StreamResponse,
    };
    use caudra_providers::{ImageMediaType, ImageSource, TokenUsage};
    use caudra_storage::sessions::PendingConversationRevert;
    use caudra_workspace::WorkspacePath;
    use crossterm::event::{ColorScheme, KeyCode};
    use std::process;
    use tempfile::TempDir;
    use test_case::test_case;

    const MISSING_PLAN_PROFILE: &str = "missing-plan-handoff-profile";
    const PLAN_TRANSACTION_BLOCKED: &str = "plan handoff did not fail at its admission boundary";
    const PLANNED_PROMPT: &str = "Plan the change.";
    const STATUS_ERROR: &str = "provider stopped";
    const STATUS_RUN_ID: u64 = 7;
    const AUTH_CHILD_TEST: &str = "event_loop::tests::oauth_login_child";
    const AUTH_CHILD_EXIT: &str = "CAUDRA_TEST_AUTH_CHILD_EXIT";
    const AUTH_CHILD_FAILURE: i32 = 17;
    const CUSTOM_PROVIDER: &str = "custom-startup-provider";
    const BUILTIN_PROVIDER: &str = "anthropic";
    const OPENCODE_PROVIDER: &str = "opencode";
    const OPENCODE_GO_PROVIDER: &str = "opencode-go";

    #[test_case(&[], true; "empty_config")]
    #[test_case(&[(CUSTOM_PROVIDER, Some(Protocol::Openai))], false; "custom_openai")]
    #[test_case(&[(CUSTOM_PROVIDER, Some(Protocol::OpenaiResponses))], false; "custom_responses")]
    #[test_case(&[(CUSTOM_PROVIDER, Some(Protocol::Anthropic))], false; "custom_anthropic")]
    #[test_case(&[(CUSTOM_PROVIDER, Some(Protocol::Google))], false; "custom_google")]
    #[test_case(&[(CUSTOM_PROVIDER, None)], true; "custom_without_protocol")]
    #[test_case(&[(BUILTIN_PROVIDER, None)], true; "builtin_override")]
    #[test_case(&[(BUILTIN_PROVIDER, Some(Protocol::Openai))], true; "builtin_with_protocol")]
    #[test_case(&[(OPENCODE_PROVIDER, Some(Protocol::Openai))], true; "opencode_with_protocol")]
    #[test_case(&[(OPENCODE_GO_PROVIDER, Some(Protocol::Openai))], true; "opencode_go_with_protocol")]
    #[test_case(&[(BUILTIN_PROVIDER, None), (CUSTOM_PROVIDER, Some(Protocol::Openai))], false; "mixed_providers")]
    fn startup_login_respects_custom_provider_config(
        definitions: &[(&str, Option<Protocol>)],
        opens_without_model: bool,
    ) {
        let mut providers = ProvidersConfig::default();
        for &(slug, protocol) in definitions {
            providers.upsert(
                slug.into(),
                ProviderDef {
                    protocol,
                    ..Default::default()
                },
            );
        }

        assert_eq!(
            should_open_startup_login(true, &providers),
            opens_without_model
        );
        assert!(!should_open_startup_login(false, &providers));
    }

    #[test]
    fn anthropic_login_uses_current_executable_and_explicit_oauth() {
        let command = anthropic_login_command().unwrap();
        assert_eq!(command.get_program(), env::current_exe().unwrap());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["auth", "login", "anthropic", "--method", "oauth"]
        );
        assert_eq!(command.get_envs().count(), 0);
        assert!(command.get_current_dir().is_none());
    }

    #[test_case(0; "successful_child")]
    #[test_case(AUTH_CHILD_FAILURE; "failed_child")]
    fn oauth_login_waits_for_child_result(exit_code: i32) {
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args(["--exact", AUTH_CHILD_TEST])
            .env(AUTH_CHILD_EXIT, exit_code.to_string());
        let result = run_oauth_login(command);
        if exit_code == 0 {
            result.unwrap();
        } else {
            assert!(result.unwrap_err().to_string().contains(AUTH_EXIT_ERR));
        }
    }

    #[test]
    fn oauth_login_child() {
        if let Ok(exit_code) = env::var(AUTH_CHILD_EXIT) {
            process::exit(exit_code.parse().unwrap());
        }
    }

    #[test]
    fn oauth_login_reports_launch_failure() {
        let temp = TempDir::new().unwrap();
        let command = Command::new(temp.path().join("missing-caudra"));
        let error = run_oauth_login(command).unwrap_err();
        assert_eq!(error.to_string(), AUTH_PROCESS_ERR);
    }

    struct PlanProvider;

    impl Provider for PlanProvider {
        fn stream_message<'a>(
            &'a self,
            _model: &'a Model,
            _messages: &'a [Message],
            _system: &'a str,
            _tools: &'a serde_json::Value,
            _event_tx: &'a flume::Sender<ProviderEvent>,
            _opts: RequestOptions,
            _cache_key: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async { Err(AgentError::Channel) })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn plan_spawn_context(source: &mut App) -> SpawnCtx {
        let path = Path::new(&source.state.session.cwd).join(source.state.plan.path().unwrap());
        source.state.plan = PlanState::Ready(path);
        source.state.session_mut().cwd = std::env::current_dir().unwrap().to_str().unwrap().into();
        SpawnCtx {
            peer_host: None,
            storage: source.storage.clone(),
            background_enabled: false,
            config: AgentConfig {
                features: FeatureFlags::NONE,
                ..Default::default()
            },
            automations: AutomationsConfig::default(),
            ui_config: UiConfig::default(),
            snapshots: SnapshotsConfig::default(),
            change_factory: None,
            allow_workspace_recovery: false,
            input_history_size: RELOCATION_INPUT_HISTORY,
            max_log_files: caudra_storage::log::DEFAULT_MAX_FILES,
            docs: docs_fixture::library,
            permissions: Arc::clone(&source.permissions),
            pattern_suggestion_loader: None,
            permission_authority_factory: None,
            sandbox_connector: None,
            transfer_connector: None,
            sandbox_readiness: None,
            sandbox_name: None,
            network_gate: Arc::default(),
            timeouts: Timeouts::default(),
            custom_commands: Arc::from([]),
            no_commands: true,
            lua_command_reader: LuaCommandReader::empty(),
            keymap_reader: KeymapReader::empty(),
            hint_reader: HintReader::empty(),
            lua_event_handle: EventHandle::disconnected_for_test(),
            mcp_handle: None,
            mcp_config_errors: McpConfigErrors::new(PathBuf::new()),
            model_slot: Arc::new(ArcSwap::from_pointee(ModelSlot {
                model: test_model(),
                provider: Arc::new(PlanProvider),
            })),
            available_models: Arc::new(ArcSwapOption::empty()),
            storage_writer: Arc::new(StorageWriter::new(
                source.storage.clone(),
                flume::unbounded().0,
            )),
            model_policy: Arc::default(),
            prompt_profiles: Arc::default(),
            default_prompt_profile: None,
            prompt_profile_override: None,
            live_sessions: Arc::default(),
            workspace_session: None,
            host_cwd: None,
            local_documents: None,
        }
    }

    fn captured_plan_action(source: &mut App) -> PlanHandoff {
        source.update(Msg::Key(key(KeyCode::Down)));
        let action = source.update(Msg::Key(key(KeyCode::Enter))).pop().unwrap();
        let Action::ClearAndImplement(handoff) = action else {
            panic!("expected captured plan handoff");
        };
        *handoff
    }

    #[test_case(true, false; "reservation")]
    #[test_case(false, false; "spawn_profile_resolution")]
    #[test_case(false, true; "new_session_run_admission")]
    fn failed_plan_runtime_admission_preserves_source(reservation: bool, admission: bool) {
        let mut source = plan_app();
        let mut ctx = plan_spawn_context(&mut source);
        let plan = source.state.plan.clone();
        let id = source.state.session.id;
        let run = source.run_id;
        let _handoff = captured_plan_action(&mut source);
        let temp = private_tempdir();
        let blocked = temp.path().join("not-a-directory");
        fs::write(&blocked, b"blocked").unwrap();
        if reservation {
            ctx.storage = StateDir::from_path(blocked);
        } else if admission {
            ctx.storage_writer = Arc::new(StorageWriter::new(
                StateDir::from_path(blocked),
                flume::unbounded().0,
            ));
        } else {
            ctx.prompt_profile_override = Some(MISSING_PLAN_PROFILE.into());
        }
        let error = ctx
            .spawn_plan_runtime(&source)
            .err()
            .expect(PLAN_TRANSACTION_BLOCKED);
        if reservation {
            assert!(error.starts_with("Failed to reserve new session:"));
        } else if !admission {
            assert!(error.contains(MISSING_PLAN_PROFILE));
        }
        assert_eq!(source.state.session.id, id);
        assert_eq!(source.state.plan, plan);
        assert_eq!(source.state.mode, Mode::Plan);
        assert_eq!(source.run_id, run);
        assert_eq!(source.status, Status::Streaming);
        assert!(source.plan_form.is_visible());
    }

    /// The plan goes with the work: the destination is bound to it, drafting,
    /// and the source keeps none, in memory or on disk.
    #[test_case(false; "idle_same_app")]
    #[test_case(true; "working_new_runtime")]
    fn admitted_plan_handoff_starts_only_the_destination(working: bool) {
        let mut source = plan_app();
        let ctx = plan_spawn_context(&mut source);
        source.status = if working {
            Status::Streaming
        } else {
            Status::Idle
        };
        source
            .state
            .session_mut()
            .replace_messages(crate::history_items(&[Message::user(
                PLANNED_PROMPT.into(),
            )]));
        let source_id = source.state.session.id;
        let source_run = source.run_id;
        let bound = source.state.plan.path().unwrap().to_path_buf();
        let handoff = captured_plan_action(&mut source);
        let expected = handoff.input.message.clone();
        let mut runtime = if working {
            Some(ctx.spawn_plan_runtime(&source).unwrap())
        } else {
            None
        };
        let target = if let Some(runtime) = &mut runtime {
            assert_eq!(source.run_id, source_run);
            assert_eq!(runtime.app.status, Status::Idle);
            assert_eq!(runtime.app.state.plan, PlanState::None);
            source.consume_plan();
            assert_eq!(source.state.plan, PlanState::None);
            &mut runtime.app
        } else {
            let actions = source.reset_session_for_plan().unwrap();
            assert!(matches!(&actions[..], [Action::NewSession(_)]));
            assert_eq!(source.status, Status::Idle);
            assert_eq!(source.run_id, source_run);
            let retired = AppSession::load(source_id, &source.storage).unwrap();
            assert_eq!(retired.meta.plan_target, None);
            &mut source
        };
        let run = target.run_id;
        target.adopt_plan(&handoff);
        let actions = target.finish_plan_handoff(handoff);
        assert_ne!(target.state.session.id, source_id);
        assert_eq!(target.run_id, run + 1);
        assert_eq!(target.status, Status::Streaming);
        assert_eq!(target.state.mode, Mode::Build);
        assert_eq!(target.state.plan, PlanState::Drafting(bound.clone()));
        let plan = Some(PlanTarget::Local(bound));
        assert!(
            matches!(&actions[..], [Action::SendMessage(input)] if input.message == expected && input.mode == AgentMode::Build && input.plan == plan)
        );
        assert!(!target.plan_form.is_visible());
        if let Some(runtime) = runtime {
            runtime.handles.cancel();
        }
    }

    #[test]
    fn settled_transfer_reservations_release_before_queued_compare() {
        const REVISION: &str =
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut app = test_app();
        let tasks = smol::block_on(BackgroundTasks::spawn(
            app.storage.clone(),
            app.state.session.id,
        ))
        .unwrap();
        let mut reservations = vec![SessionTransition {
            _background: Some(tasks.suspend().unwrap()),
            _workflow: None,
        }];
        let scope = TransferScope {
            conversation: app.state.session.id,
            binding: StoredWorkspaceBinding::local_from_cwd("/tmp"),
            name: SandboxName::parse("test").unwrap(),
            instance_revision: Revision::parse(REVISION).unwrap(),
            configuration_revision: Revision::parse(REVISION).unwrap(),
            generation: 1,
        };
        app.sandbox_live.transfer_queued = Some((
            scope.clone(),
            TransferCommand::Open {
                scope: Box::new(scope.clone()),
                link: Box::new(TransferLink {
                    name: scope.name,
                    instance_revision: scope.instance_revision,
                    configuration_revision: scope.configuration_revision,
                    local_root: std::env::temp_dir(),
                    remote_root: WorkspacePath::root(),
                    attached_binding: None,
                    include_ignored: false,
                    skip_dotfiles: false,
                }),
            },
        ));
        let (_sender, receiver) = flume::bounded(1);
        app.sandbox_live.reply = Some(receiver);
        assert!(!SessionTransition::release_settled(
            &mut reservations,
            [&app]
        ));
        assert!(tasks.suspend().is_err());
        app.sandbox_live.reply = None;
        assert!(SessionTransition::release_settled(
            &mut reservations,
            [&app]
        ));
        assert!(app.sandbox_live.transfer_queued.is_some());
        let next = crate::agent::reserve_background_transition(&tasks).unwrap();
        drop(next);
        smol::block_on(tasks.shutdown()).unwrap();
    }

    #[test_case(false; "held")]
    #[test_case(true; "after_runtime_shutdown")]
    fn workspace_reservation_leaves_session_idle(shut_down: bool) {
        let mut app = test_app();
        let tasks = smol::block_on(BackgroundTasks::spawn(
            app.storage.clone(),
            app.state.session.id,
        ))
        .unwrap();
        app.background = Some(tasks.clone());
        let reservation = crate::agent::reserve_background_transition(&tasks).unwrap();
        if shut_down {
            smol::block_on(tasks.shutdown()).unwrap();
        }
        assert!(SessionStatus::of(&app) == SessionStatus::Idle);
        assert!(!app.has_session_work());
        drop(reservation);
        if !shut_down {
            smol::block_on(tasks.shutdown()).unwrap();
        }
    }

    const OBSERVATION: &str = "failed";
    const SHELL_RESULT: &str = "command finished";
    const HERDR_BLOCKER: &str = "Permission requested";
    const WRONG_STEP: &str = "a notch of the wheel carried the wrong distance";
    const REMOTE_ROOT: &str = "remote-root";
    const REMOTE_OTHER: &str = "remote-other";
    const RELOCATION_MODEL: &str = "test-model";
    const RELOCATION_SOURCE: &str = "/relocation-source";
    const RELOCATION_DESTINATION: &str = "/relocation-destination";
    const RELOCATION_DRAFT: &str = "draft before moving";
    const RELOCATION_EXTERNAL_TITLE: &str = "external writer";
    const RELOCATION_VERSION: i64 = 7;
    const RELOCATION_PENDING_ERR: &str = "has a pending restore; resolve it before moving sessions";
    const RELOCATION_INPUT_HISTORY: usize = 100;
    const WRITER_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
    const LAUNCH_AUTOMATION: &str = "nightly";
    const PERMISSION_ID: &str = "permission";
    const PERMISSION_TOOL: &str = "bash";
    const CLAIMED_ONLY_WHEN_DELIVERED: &str =
        "a delivery must leave the outbox exactly when its turn starts";
    const NOTICE_AUTOMATION: &str = "watchdog";
    const NOTICE_TEXT: &str = "still working";
    const NOTICE_FIRE_ID: &str = "notice-firing";

    fn relocation_app(storage: StateDir, cwd: &Path, writer: Arc<StorageWriter>) -> App {
        let session = AppSession::new(RELOCATION_MODEL, cwd.to_str().unwrap());
        App::new(
            &test_model(),
            session,
            storage,
            Arc::new(ArcSwapOption::empty()),
            McpSnapshotReader::empty(),
            McpConfigErrors::new(PathBuf::new()),
            LuaCommandReader::empty(),
            KeymapReader::empty(),
            HintReader::empty(),
            writer,
            UiConfig::default(),
            RELOCATION_INPUT_HISTORY,
            caudra_storage::log::DEFAULT_MAX_FILES,
            docs_fixture::library,
            Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig::default(),
                cwd.to_path_buf(),
                Arc::default(),
            )),
            Arc::from([]),
            EventHandle::disconnected_for_test(),
            Arc::new(ModelPolicy::default()),
            Arc::new(PromptProfileCatalog::default()),
            None,
            FeatureFlags::all(),
        )
    }

    fn relocation_location(cwd: &str) -> SessionLocation {
        SessionLocation {
            id: CaudraId::generate(),
            title: String::new(),
            cwd: cwd.into(),
            updated_at: 0,
            write_version: RELOCATION_VERSION,
        }
    }

    #[test_case(false; "owned_composer_checkpoint")]
    #[test_case(true; "external_write_after_composer_checkpoint")]
    fn relocation_confirmation_reconciles_only_owned_composer_saves(external: bool) {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let source = TempDir::new().unwrap();
        let destination = TempDir::new().unwrap();
        let writer = Arc::new(StorageWriter::new(storage.clone(), flume::unbounded().0));
        let mut app = relocation_app(storage.clone(), source.path(), Arc::clone(&writer));
        let command = format!("/move-session {}", destination.path().display());
        app.update(Msg::Paste(command.clone()));
        app.checkpoint_now();
        writer.save_sync(Arc::clone(&app.state.session)).unwrap();
        assert_eq!(
            load_app_session(app.state.session.id, &storage)
                .unwrap()
                .meta
                .input_draft
                .as_deref(),
            Some(command.as_str())
        );

        let actions = app.update(Msg::Key(key(KeyCode::Enter)));
        let [Action::OpenSessionRelocation { bulk, destination }] = actions.as_slice() else {
            panic!("{RELOCATION_CHANGED_ERR}");
        };
        let inventory = relocation_inventory(&storage, [app.state.session.as_ref()]).unwrap();
        let preview_version = inventory[0].write_version;
        app.open_session_relocation(inventory, *bulk, destination.clone(), 0);
        app.checkpoint_now();
        writer.save_sync(Arc::clone(&app.state.session)).unwrap();
        let owned_version = app
            .state
            .session
            .as_ref()
            .clone()
            .persisted_write_version()
            .unwrap();
        assert!(owned_version > preview_version);
        assert!(
            load_app_session(app.state.session.id, &storage)
                .unwrap()
                .meta
                .input_draft
                .is_none()
        );
        if external {
            let mut other = load_app_session(app.state.session.id, &storage).unwrap();
            other.set_title(RELOCATION_EXTERNAL_TITLE.into());
            other.save(&storage).unwrap();
        }

        let actions = app.update(Msg::Key(key(KeyCode::Enter)));
        let [Action::RelocateSessions { request, donor }] = actions.as_slice() else {
            panic!("{RELOCATION_CHANGED_ERR}");
        };
        assert_eq!(request.sessions[0].write_version, preview_version);
        let mut request = request.clone();
        let current = relocation_inventory(&storage, [app.state.session.as_ref()]).unwrap();
        let result = reconcile_relocation_live_version(
            &mut request.sessions[0],
            &app.state.session,
            &current,
        );
        if external {
            assert_eq!(result, Err(RELOCATION_CHANGED_ERR.into()));
            assert_eq!(request.sessions[0].write_version, preview_version);
            assert_eq!(
                load_app_session(app.state.session.id, &storage)
                    .unwrap()
                    .title,
                RELOCATION_EXTERNAL_TITLE
            );
        } else {
            result.unwrap();
            assert_eq!(request.sessions[0].write_version, owned_version);
            validate_relocation_selection(&request, &current, donor.as_ref()).unwrap();
        }
        drop(app);
        Arc::try_unwrap(writer)
            .ok()
            .unwrap()
            .shutdown_checked(WRITER_DRAIN_TIMEOUT)
            .unwrap();
    }

    #[test_case(false; "unsaved_blank_current")]
    #[test_case(true; "owned_deleted_blank_current")]
    fn relocation_inventory_keeps_blank_live_sessions_until_final_save(deleted: bool) {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let source = TempDir::new().unwrap();
        let writer = Arc::new(StorageWriter::new(storage.clone(), flume::unbounded().0));
        let mut app = relocation_app(storage.clone(), source.path(), Arc::clone(&writer));
        if deleted {
            app.update(Msg::Paste(RELOCATION_DRAFT.into()));
            app.checkpoint_now();
            writer.save_sync(Arc::clone(&app.state.session)).unwrap();
            app.input_box.set_input(String::new());
            app.checkpoint_now();
            writer.delete_sync(app.state.session.id).unwrap();
        }
        assert!(!session_has_content(&app.state.session));
        let database = SessionDatabase::open_state(&storage).unwrap();
        assert!(database.local_session_locations().unwrap().is_empty());
        let inventory = relocation_inventory(&storage, [app.state.session.as_ref()]).unwrap();
        let mut expected = inventory[0].clone();
        assert_eq!(expected.id, app.state.session.id);
        reconcile_relocation_live_version(&mut expected, &app.state.session, &inventory).unwrap();
        app.checkpoint_now();
        writer.save_sync(Arc::clone(&app.state.session)).unwrap();
        let current = database.local_session_locations().unwrap();
        reconcile_relocation_live_version(&mut expected, &app.state.session, &current).unwrap();
        validate_relocation_selection(
            &SessionRelocation {
                sessions: vec![expected],
                source_cwd: Some(app.state.session.cwd.clone()),
                destination: RELOCATION_DESTINATION.into(),
                include_project_usage: true,
                keep_plan: false,
            },
            &current,
            None,
        )
        .unwrap();
        assert!(!session_has_content(
            &load_app_session(app.state.session.id, &storage).unwrap()
        ));
        drop(app);
        Arc::try_unwrap(writer)
            .ok()
            .unwrap()
            .shutdown_checked(WRITER_DRAIN_TIMEOUT)
            .unwrap();
    }

    #[test_case("unchanged", false; "unchanged")]
    #[test_case("empty", true; "empty_selection")]
    #[test_case("duplicate", true; "duplicate_ids")]
    #[test_case("id", true; "replaced_id")]
    #[test_case("cwd", true; "moved_selection")]
    #[test_case("version", true; "changed_write_version")]
    #[test_case("donor_id", true; "deleted_donor")]
    #[test_case("donor_cwd", true; "moved_donor")]
    #[test_case("donor_version", false; "donor_content_change_allowed")]
    #[test_case("member_added", true; "bulk_member_added")]
    #[test_case("member_removed", true; "bulk_member_removed")]
    #[test_case("individual", false; "individual_move_allows_unselected_sibling")]
    #[test_case("metadata", false; "display_metadata_not_identity")]
    fn relocation_selection_checks_exact_identity_and_bulk_membership(change: &str, refused: bool) {
        let selected = relocation_location(RELOCATION_SOURCE);
        let donor = relocation_location(RELOCATION_DESTINATION);
        let mut current = vec![selected.clone(), donor.clone()];
        let mut request = SessionRelocation {
            sessions: vec![selected],
            source_cwd: Some(RELOCATION_SOURCE.into()),
            destination: RELOCATION_DESTINATION.into(),
            include_project_usage: true,
            keep_plan: false,
        };
        match change {
            "empty" => request.sessions.clear(),
            "duplicate" => request.sessions.push(request.sessions[0].clone()),
            "id" => current[0].id = CaudraId::generate(),
            "cwd" => current[0].cwd = RELOCATION_DESTINATION.into(),
            "version" => current[0].write_version += 1,
            "donor_id" => current[1].id = CaudraId::generate(),
            "donor_cwd" => current[1].cwd = RELOCATION_SOURCE.into(),
            "donor_version" => current[1].write_version += 1,
            "member_added" => current.push(relocation_location(RELOCATION_SOURCE)),
            "member_removed" => request
                .sessions
                .push(relocation_location(RELOCATION_SOURCE)),
            "individual" => {
                request.source_cwd = None;
                request.include_project_usage = false;
                current.push(relocation_location(RELOCATION_SOURCE));
            }
            "metadata" => {
                current[0].title = RELOCATION_EXTERNAL_TITLE.into();
                current[0].updated_at += 1;
            }
            "unchanged" => {}
            _ => unreachable!(),
        }
        assert_eq!(
            validate_relocation_selection(&request, &current, Some(&(donor.id, donor.cwd))),
            if refused {
                Err(RELOCATION_CHANGED_ERR.into())
            } else {
                Ok(())
            },
        );
    }

    #[test_case("id"; "live_id_changed")]
    #[test_case("cwd"; "live_cwd_changed")]
    #[test_case("missing"; "stored_row_missing")]
    #[test_case("older"; "live_lineage_older_than_preview")]
    fn relocation_live_reconcile_refuses_identity_and_version_regressions(change: &str) {
        let mut session = AppSession::new(RELOCATION_MODEL, RELOCATION_SOURCE);
        session.set_persisted_write_version(Some(RELOCATION_VERSION));
        let mut expected = relocation_location(RELOCATION_SOURCE);
        expected.id = session.id;
        let mut current = vec![expected.clone()];
        match change {
            "id" => session.id = CaudraId::generate(),
            "cwd" => session.set_cwd(RELOCATION_DESTINATION.into()),
            "missing" => current.clear(),
            "older" => expected.write_version += 1,
            _ => unreachable!(),
        }
        let unchanged = expected.clone();
        assert_eq!(
            reconcile_relocation_live_version(&mut expected, &session, &current),
            Err(RELOCATION_CHANGED_ERR.into())
        );
        assert_eq!(expected, unchanged);
    }

    #[test_case(false; "clean_destination_allowed")]
    #[test_case(true; "destination_donor_pending_revert_refused")]
    fn relocation_startup_rechecks_destination_without_mutation(pending: bool) {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let workspace = TempDir::new().unwrap();
        let cwd = workspace.path().canonicalize().unwrap();
        let mut donor = AppSession::new(RELOCATION_MODEL, cwd.to_str().unwrap());
        if pending {
            donor.meta.pending_revert = Some(PendingConversationRevert {
                original_head: None,
                target_head: None,
                file_status: None,
            });
        }
        donor.save(&storage).unwrap();
        let session_before =
            serde_json::to_value(load_app_session(donor.id, &storage).unwrap()).unwrap();

        let result = check_relocation_destination(&storage, &cwd);

        if pending {
            assert_eq!(
                result,
                Err(format!(
                    "Destination session {} {RELOCATION_PENDING_ERR}",
                    donor.id
                ))
            );
        } else {
            result.unwrap();
        }
        assert_eq!(
            serde_json::to_value(load_app_session(donor.id, &storage).unwrap()).unwrap(),
            session_before
        );
    }

    #[test_case(KeyModifiers::NONE, 3 ; "a plain notch is the configured size")]
    #[test_case(KeyModifiers::ALT, 12 ; "alt multiplies it")]
    #[test_case(KeyModifiers::SHIFT, 3 ; "shift is left to the terminal")]
    fn alt_is_the_only_modifier_the_wheel_reads(modifiers: KeyModifiers, expected: u32) {
        assert_eq!(scroll_step(modifiers, 3), expected, "{WRONG_STEP}");
    }

    #[test]
    fn fork_draft_makes_empty_history_session_restorable() {
        let mut session = AppSession::new("test-model", "/tmp");
        let image = ImageSource::new(ImageMediaType::Png, Arc::from("aW1hZ2U="));

        install_fork_draft(
            &mut session,
            ForkDraft {
                text: "edit me".into(),
                images: vec![image.clone()],
            },
        );

        assert!(session_has_content(&session));
        assert_eq!(session.meta.input_draft.as_deref(), Some("edit me"));
        assert_eq!(session.meta.input_draft_images.len(), 1);
        assert_eq!(session.meta.input_draft_images[0].media_type, "image/png");
        assert_eq!(session.meta.input_draft_images[0].data, image.data.as_ref());
        assert!(session.messages().is_empty());
    }

    fn done_event() -> AgentEvent {
        AgentEvent::Done {
            usage: TokenUsage::default(),
            num_turns: 1,
            reason: DoneReason::EndTurn,
        }
    }

    fn due_completion() -> RunNotificationState {
        let mut state = RunNotificationState::default();
        state.on_done(&done_event());
        state.on_drain();
        state
    }

    #[test_case(DoneReason::EndTurn, ProgramStatus::Done; "completed")]
    #[test_case(DoneReason::Cancelled, ProgramStatus::Idle; "cancelled")]
    #[test_case(DoneReason::MaxTurns, ProgramStatus::Error; "turn_limit")]
    #[test_case(DoneReason::MaxTokens, ProgramStatus::Error; "token_limit")]
    fn program_outcome_waits_for_drain_and_survives_notification_delivery(
        reason: DoneReason,
        expected: ProgramStatus,
    ) {
        let mut state = RunNotificationState::default();
        state.on_done(&AgentEvent::Done {
            usage: TokenUsage::default(),
            num_turns: 1,
            reason,
        });
        assert_eq!(
            state.program_status(ProgramStatus::Idle, state.waiting_for_drain()),
            ProgramStatus::Working
        );
        state.on_drain();
        state.reconcile(None, SessionStatus::Idle, true, true);
        assert_eq!(state.program_status(ProgramStatus::Idle, false), expected);
    }

    #[test]
    fn terminal_error_is_retained_until_new_work() {
        let mut state = RunNotificationState::default();
        state.on_done(&AgentEvent::Error {
            message: STATUS_ERROR.into(),
        });
        assert_eq!(
            state.program_status(ProgramStatus::Idle, state.waiting_for_drain()),
            ProgramStatus::Working
        );
        state.on_drain();
        assert_eq!(
            state.program_status(ProgramStatus::Idle, false),
            ProgramStatus::Error
        );
        state.on_queue_item_consumed();
        assert_eq!(
            state.program_status(ProgramStatus::Idle, false),
            ProgramStatus::Idle
        );
    }

    #[test]
    fn recoverable_tool_error_does_not_finish_the_program() {
        let mut state = RunNotificationState::default();
        state.on_done(&AgentEvent::ToolDone(Box::new(ToolDoneEvent::error(
            PERMISSION_ID.into(),
            STATUS_ERROR,
        ))));
        assert!(!state.waiting_for_drain());
        assert_eq!(state.outcome, None);
        assert_eq!(
            state.program_status(ProgramStatus::Working, false),
            ProgramStatus::Working
        );
    }

    #[test_case(RunNotificationState::on_queue_item_consumed; "queued_followup")]
    #[test_case(RunNotificationState::reset; "new_run_or_cancel")]
    #[test_case(RunNotificationState::on_manual_exit; "manual_exit")]
    fn interrupted_completion_does_not_reappear_after_drain(
        interrupt: fn(&mut RunNotificationState),
    ) {
        let mut state = RunNotificationState::default();
        state.on_done(&done_event());
        interrupt(&mut state);
        state.on_drain();
        assert_eq!(
            state.program_status(ProgramStatus::Idle, false),
            ProgramStatus::Idle
        );
    }

    #[test_case(false; "unfocused")]
    #[test_case(true; "focused")]
    fn program_status_suppresses_legacy_completion_but_preserves_automation_notices(focused: bool) {
        let mut state = due_completion();
        state.on_automation_event(&notice_event(Some(NOTICE_FIRE_ID)));
        assert_eq!(
            state.reconcile_program_status(focused),
            (!focused).then(automation_notice)
        );
        assert_eq!(state.reconcile_program_status(false), None);
        assert_eq!(
            state.program_status(ProgramStatus::Idle, false),
            ProgramStatus::Done
        );
    }

    #[test_case(ProgramStatus::Working, false, ProgramStatus::Working; "app_work")]
    #[test_case(ProgramStatus::Idle, true, ProgramStatus::Working; "background_work")]
    #[test_case(ProgramStatus::Blocked(None), true, ProgramStatus::Blocked(None); "blocked_while_busy")]
    fn live_work_outranks_a_pending_program_outcome(
        status: ProgramStatus,
        busy: bool,
        expected: ProgramStatus,
    ) {
        let state = due_completion();
        assert_eq!(state.program_status(status, busy), expected);
        assert_eq!(
            state.program_status(ProgramStatus::Idle, false),
            ProgramStatus::Done
        );
    }

    #[test_case(ProgramStatus::Idle, ProgramStatus::Done, ProgramStatus::Done; "done_over_idle")]
    #[test_case(ProgramStatus::Done, ProgramStatus::Error, ProgramStatus::Error; "error_over_done")]
    #[test_case(ProgramStatus::Error, ProgramStatus::Working, ProgramStatus::Working; "work_over_error")]
    #[test_case(ProgramStatus::Working, ProgramStatus::Blocked(None), ProgramStatus::Blocked(None); "blocked_over_work")]
    fn pane_program_status_aggregates_all_sessions(
        first: ProgramStatus,
        second: ProgramStatus,
        expected: ProgramStatus,
    ) {
        assert_eq!(aggregate_program_status([first, second]), expected);
        assert_eq!(aggregate_program_status([second, first]), expected);
    }

    #[test_case(ProgramStatus::Done, true, false, ProgramStatus::Done; "exit_on_done")]
    #[test_case(ProgramStatus::Error, true, false, ProgramStatus::Error; "exit_on_error")]
    #[test_case(ProgramStatus::Working, false, false, ProgramStatus::Idle; "manual_exit_working")]
    #[test_case(ProgramStatus::Blocked(None), false, false, ProgramStatus::Idle; "manual_exit_blocked")]
    #[test_case(ProgramStatus::Done, false, false, ProgramStatus::Idle; "manual_exit_done")]
    #[test_case(ProgramStatus::Working, false, true, ProgramStatus::Error; "fatal_ui_error")]
    fn program_status_exit_lifetime(
        status: ProgramStatus,
        exit_on_done: bool,
        failed: bool,
        expected: ProgramStatus,
    ) {
        assert_eq!(exit_program_status(status, exit_on_done, failed), expected);
    }

    #[test_case(Event::FocusGained, false; "focus_is_not_acknowledgment")]
    #[test_case(Event::Resize(80, 24), false; "resize_is_not_acknowledgment")]
    #[test_case(Event::Key(key(KeyCode::Enter)), true; "key_acknowledges")]
    #[test_case(Event::Paste(PLANNED_PROMPT.into()), true; "paste_acknowledges")]
    fn program_outcome_requires_user_interaction(event: Event, expected: bool) {
        assert_eq!(acknowledges_program_status(&event), expected);
    }

    #[test_case(STATUS_RUN_ID, false, true; "current_parent")]
    #[test_case(STATUS_RUN_ID + 1, false, false; "stale_run")]
    #[test_case(STATUS_RUN_ID, true, false; "child_completion")]
    fn completion_filters_current_top_level_run(run_id: u64, child: bool, expected: bool) {
        let envelope = Envelope {
            event: done_event(),
            run_id,
            subagent: child.then(|| SubagentInfo {
                parent_tool_use_id: PERMISSION_ID.into(),
                task_id: PERMISSION_ID.into(),
                name: PERMISSION_TOOL.into(),
                prompt: None,
                model: None,
                thinking: None,
                fast: false,
                answer_tx: None,
                steer_tx: None,
            }),
            task: None,
            workflow: None,
        };
        assert_eq!(is_current_top_level(STATUS_RUN_ID, &envelope), expected);
    }

    #[test]
    fn completion_waits_for_queue_drain() {
        let mut state = RunNotificationState {
            response_candidate: Some("done".into()),
            ..RunNotificationState::default()
        };

        state.on_done(&done_event());
        assert!(state.waiting_for_drain());
        assert_eq!(
            state.reconcile(None, SessionStatus::Idle, true, false),
            None
        );

        state.on_drain();
        assert!(!state.waiting_for_drain());
        assert_eq!(
            state.reconcile(None, SessionStatus::Idle, true, false),
            Some(Notification::TurnComplete {
                response: Some("done".into())
            })
        );
    }

    #[test_case(SessionStatus::Idle, true, false, true ; "fires_when_settled_and_unfocused")]
    #[test_case(SessionStatus::Idle, true, true, false ; "focused_terminal_swallows")]
    #[test_case(SessionStatus::Idle, false, false, false ; "queued_message_swallows")]
    #[test_case(SessionStatus::Working, true, false, false ; "busy_session_swallows")]
    fn due_completion_is_decided_on_first_reconcile(
        status: SessionStatus,
        queue_empty: bool,
        terminal_focused: bool,
        fires: bool,
    ) {
        let mut state = due_completion();

        let first = state.reconcile(None, status, queue_empty, terminal_focused);
        assert_eq!(first.is_some(), fires);
        assert_eq!(
            state.reconcile(None, SessionStatus::Idle, true, false),
            None
        );
    }

    #[test]
    fn prompt_wins_over_completion_and_is_not_repeated_unchanged() {
        let prompt = Notification::QuestionRequested;
        let mut state = due_completion();

        assert_eq!(
            state.reconcile(Some(prompt.clone()), SessionStatus::Idle, true, false),
            Some(prompt.clone())
        );
        assert_eq!(
            state.reconcile(Some(prompt), SessionStatus::NeedsInput, true, false),
            None
        );
        assert_eq!(
            state.reconcile(None, SessionStatus::Idle, true, false),
            None
        );
    }

    fn automation_notice() -> Notification {
        Notification::AutomationNotice {
            automation: NOTICE_AUTOMATION.into(),
            text: NOTICE_TEXT.into(),
        }
    }

    fn notice_event(fire_id: Option<&str>) -> AutomationEvent {
        AutomationEvent::Notice {
            automation: NOTICE_AUTOMATION.into(),
            fire_id: fire_id.map(str::to_owned),
            text: NOTICE_TEXT.into(),
        }
    }

    #[test_case(notice_event(Some(NOTICE_FIRE_ID)), Some(automation_notice()) ; "a_firings_notify")]
    #[test_case(notice_event(None), None ; "an_arming_refusal")]
    #[test_case(AutomationEvent::SaveSession, None ; "another_event")]
    fn only_a_firings_notify_reaches_the_terminal(
        event: AutomationEvent,
        expected: Option<Notification>,
    ) {
        let mut state = RunNotificationState::default();
        state.on_automation_event(&event);

        assert_eq!(
            state.reconcile(None, SessionStatus::Working, true, false),
            expected
        );
    }

    #[test_case(false, true ; "fires_when_unfocused")]
    #[test_case(true, false ; "focused_terminal_swallows")]
    fn automation_notice_is_decided_on_first_reconcile(terminal_focused: bool, fires: bool) {
        let mut state = RunNotificationState::default();
        state.on_automation_event(&notice_event(Some(NOTICE_FIRE_ID)));

        let first = state.reconcile(None, SessionStatus::Working, true, terminal_focused);
        assert_eq!(first, fires.then(automation_notice));
        assert_eq!(
            state.reconcile(None, SessionStatus::Working, true, false),
            None
        );
    }

    #[test_case(Some(Notification::QuestionRequested), Notification::QuestionRequested ; "prompt_outranks_notice")]
    #[test_case(None, automation_notice() ; "notice_outranks_completion")]
    fn automation_notice_ranks_between_prompts_and_completions(
        attention: Option<Notification>,
        expected: Notification,
    ) {
        let mut state = due_completion();
        state.on_automation_event(&notice_event(Some(NOTICE_FIRE_ID)));

        assert_eq!(
            state.reconcile(attention, SessionStatus::Idle, true, false),
            Some(expected)
        );
    }

    #[test]
    fn notification_selection_prefers_priority_then_session_order() {
        let completion = Notification::TurnComplete { response: None };
        let first_prompt = Notification::QuestionRequested;
        let second_prompt = Notification::AuthenticationRequired;

        let selected = select_notification(None, Some(completion));
        let selected = select_notification(selected, Some(first_prompt.clone()));
        let selected = select_notification(selected, Some(second_prompt));

        assert_eq!(selected, Some(first_prompt));
    }

    #[cfg(not(windows))]
    #[test]
    fn focus_events_map_to_terminal_focus_state() {
        assert_eq!(terminal_focus_event(&Event::FocusGained), Some(true));
        assert_eq!(terminal_focus_event(&Event::FocusLost), Some(false));
        assert_eq!(terminal_focus_event(&Event::Resize(80, 24)), None);
    }

    #[test_case(ColorScheme::Dark; "dark")]
    #[test_case(ColorScheme::Light; "light")]
    fn appearance_reports_do_not_prove_terminal_focus(scheme: ColorScheme) {
        let event = Event::ColorSchemeChanged(scheme);
        assert_eq!(terminal_focus_event(&event), None);
        assert!(!terminal_input_proves_focus(&event));
    }

    #[cfg(not(windows))]
    #[test]
    fn interactive_input_proves_terminal_focus() {
        let release = crossterm::event::KeyEvent {
            code: crossterm::event::KeyCode::Enter,
            modifiers: crossterm::event::KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        };

        assert!(terminal_input_proves_focus(&Event::Key(
            crate::components::key(crossterm::event::KeyCode::Enter)
        )));
        assert!(terminal_input_proves_focus(&Event::Paste("text".into())));
        assert!(!terminal_input_proves_focus(&Event::Key(release)));
        assert!(!terminal_input_proves_focus(&Event::Resize(80, 24)));
    }

    #[test]
    fn shell_results_do_not_replace_existing_preamble() {
        let mut preamble = vec![Message::observation(OBSERVATION.into())];

        prepend_preamble(
            &mut preamble,
            vec![Message::observation(SHELL_RESULT.into())],
        );

        let text = preamble.iter().map(Message::user_text).collect::<Vec<_>>();
        assert_eq!(text, [Some(SHELL_RESULT), Some(OBSERVATION)]);
    }

    #[test_case(1, 0 ; "background_subagent")]
    #[test_case(0, 1 ; "shell_command")]
    fn active_workspace_work_is_not_quiescent(background_tasks: usize, shell_commands: usize) {
        assert!(!runtime_state_quiescent(
            SessionStatus::Idle,
            true,
            background_tasks,
            shell_commands,
            false,
        ));
    }

    #[test_case(true, true, false, 0, 0 ; "app_work")]
    #[test_case(false, false, false, 0, 0 ; "queued_prompt")]
    #[test_case(false, true, true, 0, 0 ; "processing_queue")]
    #[test_case(false, true, false, 1, 0 ; "background_subagent")]
    #[test_case(false, true, false, 0, 1 ; "shell_command")]
    fn active_work_reports_working(
        app_working: bool,
        queue_empty: bool,
        queue_processing: bool,
        background_tasks: usize,
        shell_commands: usize,
    ) {
        assert_eq!(
            runtime_observation(
                None,
                app_working,
                queue_empty,
                queue_processing,
                background_tasks,
                shell_commands,
            ),
            HerdrObservation::working()
        );
        let state = due_completion();
        assert_eq!(
            state.program_status(
                ProgramStatus::Idle,
                runtime_busy(
                    app_working,
                    queue_empty,
                    queue_processing,
                    background_tasks,
                    shell_commands
                ),
            ),
            ProgramStatus::Working,
        );
    }

    #[test]
    fn blocker_outranks_active_work() {
        assert_eq!(
            runtime_observation(Some(HERDR_BLOCKER.into()), true, false, true, 1, 1),
            HerdrObservation::blocked(HERDR_BLOCKER)
        );
    }

    #[test]
    fn quiescent_runtime_reports_idle() {
        assert_eq!(
            runtime_observation(None, false, true, false, 0, 0),
            HerdrObservation::idle()
        );
    }

    #[test]
    fn every_session_on_the_same_canonical_workspace_must_be_quiescent() {
        let target = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let canonical = canonical_cwd(target.path()).unwrap();

        assert!(!matching_workspace_quiescent(
            &canonical,
            [(target.path(), true), (target.path(), false)]
        ));
        assert!(matching_workspace_quiescent(
            &canonical,
            [(target.path(), true), (other.path(), false)]
        ));
    }

    #[test]
    fn remote_workspace_quiescence_does_not_probe_local_paths() {
        let target = StoredWorkspaceBinding::local_from_cwd(REMOTE_ROOT);
        let same = StoredWorkspaceBinding::local_from_cwd(REMOTE_ROOT);
        let other = StoredWorkspaceBinding::local_from_cwd(REMOTE_OTHER);

        assert!(!matching_remote_workspace_quiescent(
            &target,
            [(Some(&same), true), (Some(&same), false)]
        ));
        assert!(matching_remote_workspace_quiescent(
            &target,
            [(Some(&same), true), (Some(&other), false)]
        ));
    }

    #[test]
    fn remote_focus_never_resolves_client_cwd() {
        use caudra_workspace::{
            ResourceId, ResourceScope, WorkspaceCursor, WorkspaceHandle, WorkspaceSession,
        };

        let local = StoredWorkspaceBinding::local_from_cwd(REMOTE_ROOT);
        let remote: StoredWorkspaceBinding = serde_json::from_str(
            &serde_json::to_string(&local)
                .unwrap()
                .replace("caudra:local:v1", "https://remote.example"),
        )
        .unwrap();
        let cursor = WorkspaceCursor::new(
            remote.binding(),
            ResourceScope::root(ResourceId::new(REMOTE_ROOT).unwrap()),
            0,
            remote.cwd_handle().clone(),
        );
        let remote = remote.with_cursor(cursor.clone()).unwrap();
        let workspace = WorkspaceSession::new(
            WorkspaceHandle::new(
                remote.binding().authority().clone(),
                Default::default(),
                Default::default(),
            )
            .unwrap(),
            remote.binding().clone(),
            cursor,
        )
        .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("does-not-exist");
        let session = AppSession::new_with_workspace(
            "test/model",
            &missing.to_string_lossy(),
            remote.clone(),
        );
        validate_session_focus(&session, Some(&workspace)).unwrap();
        assert!(!missing.exists());
        assert!(validate_session_focus(&session, None).is_err());
        let local = AppSession::new("test/model", &missing.to_string_lossy());
        assert!(validate_session_focus(&local, Some(&workspace)).is_err());
        let canary_dir = tempfile::tempdir_in(".").unwrap();
        let canary_path = canary_dir.path().join("client-canary");
        let logical_path = format!(
            "{}/client-canary",
            canary_dir.path().file_name().unwrap().to_str().unwrap()
        );
        const CANARY: &[u8] = b"client file must remain untouched";
        std::fs::write(&canary_path, CANARY).unwrap();
        let session = AppSession::new_with_workspace("test/model", &logical_path, remote);
        validate_session_focus(&session, Some(&workspace)).unwrap();
        assert_eq!(std::fs::read(&canary_path).unwrap(), CANARY);
    }

    #[test]
    fn cwd_change_requires_every_tab_idle_and_without_pending_reverts() {
        assert_eq!(
            cwd_change_blocker([(true, false), (false, false)]),
            Some(CWD_BUSY_ERR)
        );
        assert_eq!(
            cwd_change_blocker([(true, false), (true, true)]),
            Some(CWD_REVERT_ERR)
        );
        assert_eq!(cwd_change_blocker([(true, false), (true, false)]), None);
    }

    #[test]
    fn focus_validation_rejects_a_stored_session_from_another_cwd() {
        let current = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let session = AppSession::new("test-model", &other.path().to_string_lossy());

        let error =
            validate_session_cwd(&session, &canonical_cwd(current.path()).unwrap()).unwrap_err();

        assert!(error.contains("Use /cd"), "{error}");
        assert!(error.contains(&session.id.to_string()), "{error}");
    }

    #[test_case(3, 1, &[0, 1, 0] ; "the_focused_tab")]
    #[test_case(2, 5, &[0, 1] ; "a_focus_past_the_last_tab")]
    fn command_line_automations_arm_only_the_tab_that_takes_focus(
        tabs: usize,
        focused: usize,
        expected: &[usize],
    ) {
        let launch = vec![ProfileArming {
            name: LAUNCH_AUTOMATION.into(),
            args: None,
        }];

        let armed: Vec<usize> = launch_armings(tabs, focused, launch)
            .iter()
            .map(Vec::len)
            .collect();

        assert_eq!(armed, expected);
    }

    /// A fresh session whose agent loop waits for its first prompt.
    fn idle_runtime() -> SessionRuntime {
        let mut source = plan_app();
        plan_spawn_context(&mut source)
            .spawn_fresh_runtime(&source.state.session, None)
            .unwrap()
    }

    #[test_case(|_| {}, ProgramStatus::Done; "settled")]
    #[test_case(start_a_run, ProgramStatus::Working; "new_work")]
    #[test_case(open_a_permission_prompt, ProgramStatus::Blocked(Some(ProgramBlockKind::Permission)); "permission")]
    fn runtime_program_status_reconciles_live_app_and_outcome(
        change: fn(&mut SessionRuntime),
        expected: ProgramStatus,
    ) {
        let mut runtime = idle_runtime();
        runtime.notifications = due_completion();
        change(&mut runtime);
        assert_eq!(runtime.program_status(), expected);
        runtime.handles.cancel();
    }

    #[test_case(false, ProgramStatus::Working; "live_agent_must_drain")]
    #[test_case(true, ProgramStatus::Error; "stopped_agent_cannot_drain")]
    fn startup_failure_without_exit_on_done_resolves_program_status(
        agent_stopped: bool,
        expected: ProgramStatus,
    ) {
        let mut runtime = idle_runtime();
        assert!(!runtime.app.exit_on_done);
        runtime.notifications.on_done(&AgentEvent::Error {
            message: STATUS_ERROR.into(),
        });
        assert_eq!(
            runtime.program_status_with_agent_stopped(agent_stopped),
            expected
        );
        runtime.handles.cancel();
    }

    fn start_a_run(runtime: &mut SessionRuntime) {
        runtime.app.status = Status::Streaming;
    }

    fn open_a_permission_prompt(runtime: &mut SessionRuntime) {
        runtime.app.permission_prompt.open(
            PERMISSION_ID.into(),
            ToolKey::native(PERMISSION_TOOL),
            Vec::new(),
            None,
        );
    }

    fn leave_an_agent_event(runtime: &mut SessionRuntime) {
        runtime
            .handles
            .agent_tx
            .send(Envelope {
                task: None,
                event: AgentEvent::TextDelta {
                    text: String::new(),
                },
                subagent: None,
                run_id: runtime.app.run_id,
                workflow: None,
            })
            .unwrap();
    }

    #[test_case(|_| {}, &[] ; "nothing_holds_it")]
    #[test_case(start_a_run, &[SettleBlocker::Busy] ; "a_run")]
    #[test_case(open_a_permission_prompt, &[SettleBlocker::NeedsInput(InputKind::Permission)] ; "a_permission_prompt")]
    #[test_case(leave_an_agent_event, &[SettleBlocker::AgentEvents] ; "an_unhandled_agent_event")]
    fn a_session_settles_once_nothing_holds_it(
        hold: fn(&mut SessionRuntime),
        expected: &[SettleBlocker],
    ) {
        let mut runtime = idle_runtime();

        hold(&mut runtime);

        assert_eq!(runtime.settle_blockers(), expected);
        runtime.handles.cancel();
    }

    #[test_case(|_| {}, true ; "settled")]
    #[test_case(|runtime| runtime.app.automatic_wakes_suppressed = true, true ; "automatic_wakes_suppressed")]
    #[test_case(start_a_run, false ; "a_run_goes_on")]
    #[test_case(leave_an_agent_event, false ; "agent_events_go_first")]
    fn a_settled_session_starts_the_turn_its_next_delivery_asks_for(
        hold: fn(&mut SessionRuntime),
        delivers: bool,
    ) {
        let mut runtime = idle_runtime();
        let linked = LinkedAutomations::courier(&mut runtime.app);
        hold(&mut runtime);

        let actions = runtime.sync_automations();

        let delivered = actions.is_some_and(|actions| {
            matches!(&actions[..], [Action::SendMessage(input)] if input.preamble.iter().any(|message| message.display_text.as_deref() == Some(COURIER_MESSAGE)))
        });
        assert_eq!(delivered, delivers);
        assert_eq!(
            linked.state().outbox.is_empty(),
            delivers,
            "{CLAIMED_ONLY_WHEN_DELIVERED}"
        );
        linked.stop();
        runtime.handles.cancel();
    }
}
