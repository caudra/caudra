//! Multi-session supervisor: every session owns an `App` + `AgentHandles` and
//! keeps draining agent events while backgrounded; only the focused session
//! renders and receives input. `SpawnCtx` carries the shared resources needed
//! to spawn session runtimes at any point.
//!
//! Terminal input arrives on a channel (see [`InputReader`]), so the loop
//! waits on every event source at once and wakes the moment a plugin action,
//! agent event, or keypress arrives instead of sleeping in `event::poll`.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::{ArcSwap, ArcSwapOption};
use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};

use crate::sandbox::{
    LiveOperation, NETWORK_RECOVERY, NetworkGate, SandboxAttachment, SandboxConnector,
    SandboxControl, SandboxReadiness,
};
use caudra_agent::command::CustomCommand;
use caudra_agent::permissions::PermissionManager;
use caudra_agent::prompt::profile::{
    BUILTIN_PROFILE_NAME, PromptProfileCatalog, SystemPromptProfile,
};
use caudra_agent::snapshots::{SESSION_SNAPSHOTS_DIR, SnapshotLimits, SnapshotStore};
use caudra_agent::workflow::WorkflowTransition;
use caudra_agent::workspace_baseline::WorkspaceBaseline;
use caudra_agent::{
    AgentConfig, AgentEvent, CancelToken, Envelope, McpCommand, McpConfigErrors, McpHandle, mcp,
};
use caudra_config::sandbox::SandboxName;
use caudra_config::{ModelPolicy, SnapshotsConfig, UiConfig, load_permissions};
use caudra_lua::{
    EventHandle, HintReader, KeymapReader, LuaCommandReader, ModelRequest, SessionRequest,
    TaskRequest, UiAction, UiReply,
};
use caudra_providers::Timeouts;
use caudra_providers::provider::{Provider, fetch_all_models, from_model};
use caudra_providers::{HistoryItem, Message, Model};
use caudra_storage::StateDir;
use caudra_storage::StorageError;
use caudra_storage::id::{CaudraId, CaudraIdParseError, SessionRef};
use caudra_storage::remote_operation_journal::RemoteOperationJournal;
use caudra_storage::sessions::{
    SessionDatabase, SessionError, SessionLease, SessionLocation, SessionRelocation, StoredImage,
    TitleSource, normalize_title,
};
use caudra_storage::state::WorkspaceTabs;
use caudra_storage::workflow::WorkflowRunStatus;
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_workspace::WorkspaceControlCommand;
#[cfg(not(windows))]
use crossterm::event::KeyEventKind;
use crossterm::event::{
    Event, KeyModifiers, MouseButton, MouseEvent as CtMouseEvent, MouseEventKind,
};
use serde_json::json;
use tracing::{info, warn};

use crate::agent::{AgentCommand, AgentHandles, ModelSlot, shared_queue::QueueItem};
use crate::app::permission_editor::{ConversationPermissions, attach_session_permissions};
use crate::app::shell::{
    RemoteShellTarget, ShellEvent, spawn_remote_cd, spawn_remote_control, spawn_remote_shell,
    spawn_shell,
};
use crate::app::tasks::{TaskStatus, diff_task_states};
use crate::app::{
    App, Msg, Notification, QueuedMessage, SubmitOutcome, session_has_content, turn_response,
};
use crate::appearance::{self, AutoSwitch};
use crate::color_compat;
use crate::components::input::Submission;
use crate::components::session_picker::{SessionActivity, SessionRow};
use crate::components::storage_modal::{StorageFetchState, StorageReport};
use crate::components::usage_modal::UsageFetchState;
use crate::components::{Action, ExitRequest, ForkDraft, ForkedSession, Status};
use crate::herdr::{HerdrObservation, HerdrReporterHandle, aggregate_observations};
use crate::input::InputReader;
use crate::repaint::{Dirty, FrameLimiter, IDLE_POLL};
use crate::theme;
use crate::{
    AppSession, PatternSuggestionLoader, PermissionAuthorityFactory, SessionRelocationHandoff,
    SessionTab,
};
use crate::{load_app_session, open_app_session_with_cursor};

use crate::storage_writer::StorageWriter;
use crate::terminal;

/// Max events handled per frame so a flood cannot starve rendering.
const DRAIN_BUDGET: usize = 256;
/// How much further one notch of an Alt-held wheel carries.
const FAST_SCROLL_FACTOR: u32 = 4;
/// One row of finger travel moves the content one row.
const TOUCH_SCROLL_LINES: u32 = 1;
const AGENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
/// How long the final snapshot of one session may wait for the artifact lock.
/// Budgeted per session so a workspace of tabs cannot multiply into a stall,
/// and matched to the agent timeout so no one phase of exit dominates.
const SHUTDOWN_SNAPSHOT_BUDGET: Duration = Duration::from_secs(3);
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
    pub relocation: Option<SessionRelocationHandoff>,
    pub sandbox: Option<SandboxAttachment>,
    pub sandbox_control: Option<SandboxControl>,
}

pub struct EventLoopParams {
    pub model: Model,
    pub needs_login: bool,
    pub commands: Vec<CustomCommand>,
    pub no_commands: bool,
    pub sessions: Vec<SessionTab>,
    pub focused: usize,
    pub startup_warnings: Vec<String>,
    pub storage: StateDir,
    pub config: AgentConfig,
    pub ui_config: UiConfig,
    pub snapshots: SnapshotsConfig,
    pub allow_workspace_recovery: bool,
    pub input_history_size: usize,
    pub max_log_files: u32,
    pub permissions: Arc<PermissionManager>,
    pub pattern_suggestion_loader: Option<PatternSuggestionLoader>,
    pub permission_authority_factory: Option<PermissionAuthorityFactory>,
    pub sandbox_connector: Option<SandboxConnector>,
    pub transfer_connector: Option<crate::sandbox::transfer::TransferConnector>,
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
    last_attention: Option<Notification>,
}

impl RunNotificationState {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn on_queue_item_consumed(&mut self) {
        self.response_candidate = None;
        self.pending_completion = None;
    }

    fn on_turn_complete(&mut self, message: &Message) {
        self.response_candidate = turn_response(message);
    }

    fn on_done(&mut self, event: &AgentEvent) {
        let notification = match event {
            AgentEvent::Done { .. } => Notification::TurnComplete {
                response: self.response_candidate.take(),
            },
            AgentEvent::Error { .. } => {
                self.response_candidate = None;
                Notification::error_completion()
            }
            _ => return,
        };
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
        (!terminal_focused).then(|| prompt.or(completion)).flatten()
    }
}

impl SessionStatus {
    fn of(app: &App) -> Self {
        if app.awaiting_input() {
            Self::NeedsInput
        } else if app.status == Status::Streaming {
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
        self.handles.queue.is_empty()
            && !self.handles.queue.is_processing()
            && self.handles.active_background_tasks() == 0
            && self.app.shell.active_ids().is_empty()
            && !self.app.holds_recovery_text()
            && !self
                .app
                .state
                .session
                .meta
                .pending_revert
                .as_ref()
                .is_some_and(|pending| pending.restore_operation.is_some())
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
}

fn runtime_observation(
    blocker: Option<&'static str>,
    app_working: bool,
    queue_empty: bool,
    queue_processing: bool,
    background_tasks: usize,
    shell_commands: usize,
) -> HerdrObservation {
    if let Some(message) = blocker {
        HerdrObservation::blocked(message)
    } else if app_working
        || !queue_empty
        || queue_processing
        || background_tasks > 0
        || shell_commands > 0
    {
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
    restore_operation_pending: bool,
) -> bool {
    status == SessionStatus::Idle
        && queue_empty
        && background_tasks == 0
        && shell_commands == 0
        && !holds_recovery_text
        && !restore_operation_pending
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
        if ids.contains(&facts.id) {
            if facts.pending_revert {
                return Err(format!(
                    "Destination session {} has a pending restore; resolve it before moving sessions",
                    facts.id
                ));
            }
            check_relocation_journals(storage, facts.id)?;
        }
    }
    Ok(())
}

fn check_relocation_journals(storage: &StateDir, id: CaudraId) -> Result<(), String> {
    let root = storage
        .path()
        .join(SESSION_SNAPSHOTS_DIR)
        .join(id.to_string());
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "Cannot inspect snapshots for session {id}: {error}"
            ));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
            && SnapshotStore::new(entry.path())
                .journal_state()
                .map_err(|error| format!("Cannot inspect snapshots for session {id}: {error}"))?
                .is_some()
        {
            return Err(format!(
                "Session {id} has an unfinished workspace restore; resolve it before moving"
            ));
        }
    }
    Ok(())
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
    let stats = match SessionDatabase::open_read_only(storage).and_then(|db| db.stats()) {
        Ok(stats) => stats,
        Err(error) => return StorageFetchState::Error(error.to_string()),
    };
    let stores = SnapshotStore::store_entries(&storage.path().join(SESSION_SNAPSHOTS_DIR));
    StorageFetchState::Ready(Box::new(StorageReport {
        stats,
        stores,
        unavailable,
    }))
}

fn prepare_session_for_runtime(
    storage: &StateDir,
    storage_writer: &StorageWriter,
    mut session: AppSession,
    limits: SnapshotLimits,
    workspace: Option<&caudra_workspace::WorkspaceSession>,
    allow_workspace_recovery: bool,
) -> Result<(AppSession, Arc<caudra_agent::snapshots::SnapshotStore>), String> {
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
        let session_id = session.id;
        return Ok((
            session,
            Arc::new(SnapshotStore::new(
                storage
                    .path()
                    .join("remote-snapshot-metadata")
                    .join(session_id.to_string()),
            )),
        ));
    }
    let process_cwd = canonical_cwd(
        &std::env::current_dir()
            .map_err(|error| format!("failed to read current directory: {error}"))?,
    )?;
    validate_session_cwd(&session, &process_cwd)?;
    if !allow_workspace_recovery {
        if session.meta.pending_revert.is_some() {
            return Err(format!(
                "Session {} has a pending restore; resolve it before moving sessions",
                session.id
            ));
        }
        check_relocation_journals(storage, session.id)?;
    }
    let snapshot_store = App::snapshot_store_for(storage, session.id, &process_cwd, limits)
        .map_err(|error| format!("Failed to initialize workspace snapshots: {error}"))?;
    if allow_workspace_recovery {
        crate::app::recover_pending_workspace_restore(
            &mut session,
            &snapshot_store,
            storage_writer,
        )?;
    }
    Ok((session, snapshot_store))
}

pub(crate) fn reconcile_remote_session_restore(
    session: &mut AppSession,
    storage_writer: &StorageWriter,
    baseline: &WorkspaceBaseline,
    status: Option<caudra_workspace::SnapshotRestoreStatus>,
) -> Result<(), String> {
    let Some(mut pending) = session.meta.pending_revert.clone() else {
        return Ok(());
    };
    let Some(operation) = pending.restore_operation.clone() else {
        return Ok(());
    };
    let Some(status) = status else {
        pending.restore_operation = None;
        let head = crate::session_history_head(session);
        session.set_conversation_state(head, Some(pending));
        storage_writer
            .save_sync(Arc::new(session.clone()))
            .map_err(|error| format!("Failed to clear an undispatched remote restore: {error}"))?;
        return Ok(());
    };
    pending.file_status = serde_json::to_value(&status).ok();
    let definitive = matches!(
        status.state,
        caudra_workspace::SnapshotRestoreState::Completed
            | caudra_workspace::SnapshotRestoreState::Acknowledged
    ) && !status.reconciliation_required;
    if !definitive {
        let head = crate::session_history_head(session);
        session.set_conversation_state(head, Some(pending));
        storage_writer
            .save_sync(Arc::new(session.clone()))
            .map_err(|error| format!("Failed to save remote restore recovery state: {error}"))?;
        return Ok(());
    }

    pending.workspace_head = Some(operation.target_workspace_head.clone());
    if let Some(operation) = pending.restore_operation.as_mut() {
        operation.phase = caudra_storage::sessions::PendingRestorePhase::FilesApplied;
    }
    let transition_head = match operation.kind {
        caudra_storage::sessions::PendingRestoreKind::Revert => operation
            .conversation_target
            .as_ref()
            .map(|head| head.head)
            .unwrap_or_else(|| crate::session_history_head(session)),
        caudra_storage::sessions::PendingRestoreKind::Unrevert => pending.original_head,
    };
    session.set_conversation_state(transition_head, Some(pending.clone()));
    storage_writer
        .save_sync(Arc::new(session.clone()))
        .map_err(|error| format!("Failed to commit recovered remote restore: {error}"))?;
    if operation.kind == caudra_storage::sessions::PendingRestoreKind::Unrevert
        || status.state == caudra_workspace::SnapshotRestoreState::Acknowledged
    {
        if status.state == caudra_workspace::SnapshotRestoreState::Acknowledged {
            baseline
                .mark_remote_restore_acknowledged(&status.restore_id)
                .map_err(|error| {
                    format!("Failed to record remote restore acknowledgement: {error}")
                })?;
        } else {
            smol::block_on(baseline.acknowledge_remote_restore(&status.restore_id)).map_err(
                |error| format!("Failed to acknowledge recovered remote restore: {error}"),
            )?;
        }
    }
    match operation.kind {
        caudra_storage::sessions::PendingRestoreKind::Revert => {
            pending.restore_operation = None;
            session.set_conversation_state(transition_head, Some(pending));
        }
        caudra_storage::sessions::PendingRestoreKind::Unrevert => {
            session.set_conversation_state(transition_head, None);
        }
    }
    storage_writer
        .save_sync(Arc::new(session.clone()))
        .map_err(|error| format!("Failed to finalize recovered remote restore: {error}"))?;
    Ok(())
}

fn prepare_workspace_for_runtime(
    storage: &StateDir,
    storage_writer: &StorageWriter,
    cwd: &Path,
    active: &HashSet<CaudraId>,
    limits: SnapshotLimits,
    allow_workspace_recovery: bool,
) -> Result<(), String> {
    if allow_workspace_recovery {
        recover_stored_sessions_in_cwd(storage, storage_writer, cwd, active, limits)
    } else {
        check_relocation_destination(storage, cwd)
    }
}

fn recover_stored_sessions_in_cwd(
    storage: &StateDir,
    storage_writer: &StorageWriter,
    cwd: &Path,
    active: &std::collections::HashSet<CaudraId>,
    limits: SnapshotLimits,
) -> Result<(), String> {
    let cwd_text = cwd.to_string_lossy();
    let facts = SessionDatabase::open_state(storage)
        .and_then(|database| database.session_facts(Some(&cwd_text)))
        .map_err(|error| format!("Failed to scan sessions for workspace recovery: {error}"))?;
    for facts in facts {
        if active.contains(&facts.id) {
            continue;
        }
        let snapshot_store = App::snapshot_store_for(storage, facts.id, cwd, limits)
            .map_err(|error| format!("Failed to initialize workspace snapshots: {error}"))?;
        // Loading a session is proportional to its whole transcript, and a
        // workspace accumulates hundreds. `recover_pending_workspace_restore`
        // reads the history only once one of these two says there is something
        // to recover: `pending_revert` comes out of the facts query, and a
        // journal without it is the crash between writing one and saving the
        // other. Both are cheap, and on a healthy workspace both are false for
        // every session, so nothing is deserialized at all.
        if !facts.pending_revert
            && snapshot_store
                .journal_state()
                .map_err(|error| format!("Failed to inspect workspace restore journal: {error}"))?
                .is_none()
        {
            continue;
        }
        let _lease = match SessionLease::acquire(storage, facts.id) {
            Ok(lease) => lease,
            Err(SessionError::SessionInUse { .. }) => continue,
            Err(error) => {
                return Err(format!(
                    "Failed to reserve session {} for workspace recovery: {error}",
                    facts.id
                ));
            }
        };
        let mut session = load_app_session(facts.id, storage).map_err(|error| {
            format!(
                "Failed to load session {} for workspace recovery: {error}",
                facts.id
            )
        })?;
        validate_session_cwd(&session, cwd)?;
        crate::app::recover_pending_workspace_restore(
            &mut session,
            &snapshot_store,
            storage_writer,
        )?;
    }
    Ok(())
}

/// Everything needed to bring up a new session runtime after startup.
struct SpawnCtx {
    storage: StateDir,
    config: AgentConfig,
    ui_config: UiConfig,
    snapshots: SnapshotsConfig,
    allow_workspace_recovery: bool,
    input_history_size: usize,
    max_log_files: u32,
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
    fn resolve_prompt_profile(
        &self,
        session: &AppSession,
    ) -> (String, Option<Arc<SystemPromptProfile>>, Option<String>) {
        let requested_name = self
            .prompt_profile_override
            .as_deref()
            .or(session.meta.system_prompt_profile.as_deref())
            .or_else(|| {
                self.default_prompt_profile
                    .as_deref()
                    .map(SystemPromptProfile::name)
            });
        match self.prompt_profiles.resolve(requested_name) {
            Ok(profile) => (
                requested_name.unwrap_or(BUILTIN_PROFILE_NAME).to_owned(),
                profile,
                None,
            ),
            Err(error) => (
                BUILTIN_PROFILE_NAME.to_owned(),
                None,
                Some(format!(
                    "Could not use system prompt profile {:?}: {error}. Using built-in prompt.",
                    requested_name.unwrap_or(BUILTIN_PROFILE_NAME)
                )),
            ),
        }
    }

    fn spawn_runtime(&self, tab: SessionTab) -> Result<SessionRuntime, String> {
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
                &self.storage,
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
                    caudra_agent::remote_project_context::load_remote_project_context(workspace),
                )
                .map_err(|error| error.to_string())
            })
            .transpose()?;
        let (mut session, snapshot_store) = prepare_session_for_runtime(
            &self.storage,
            &self.storage_writer,
            session,
            self.snapshots.into(),
            workspace_session.as_ref(),
            self.allow_workspace_recovery,
        )?;
        let workspace_baseline = if let Some(workspace) = workspace_session.clone() {
            let stored =
                caudra_storage::workspace_binding::StoredWorkspaceBinding::new_with_cursor(
                    workspace.binding().clone(),
                    workspace.cursor().clone(),
                    session
                        .workspace_binding()
                        .and_then(|binding| binding.cursor_label().map(str::to_owned)),
                )
                .map_err(|error| {
                    format!("Remote workspace snapshot identity is invalid: {error}")
                })?;
            if let Some(binding) = session.workspace_binding()
                && !binding.exact_scope_eq(&stored)
            {
                return Err("Remote session belongs to a different workspace cursor".into());
            }
            let baseline = WorkspaceBaseline::new_workspace_session(
                self.storage.clone(),
                session.id,
                workspace,
                stored,
                self.snapshots.enabled,
            );
            let recovered =
                smol::block_on(baseline.reconcile_remote_restore()).map_err(|error| {
                    format!("Remote workspace snapshot recovery is unavailable: {error}")
                })?;
            reconcile_remote_session_restore(
                &mut session,
                &self.storage_writer,
                &baseline,
                recovered,
            )?;
            baseline
        } else {
            WorkspaceBaseline::new(
                Arc::clone(&snapshot_store),
                PathBuf::from(&session.cwd),
                self.snapshots.enabled,
            )
        };
        workspace_baseline.set_current_head(crate::session_history_head(&session));
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
        let archived_history_ms = lap();
        let restore_session = !initial_history.is_empty() || session_has_content(&session);
        let (system_prompt_profile_name, system_prompt_profile, profile_warning) =
            self.resolve_prompt_profile(&session);
        let permissions = Arc::new(self.permissions.fork());
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
        permissions.set_session_yolo(session.meta.yolo);
        let goal = caudra_agent::GoalHandle::restored(session.meta.active_goal.as_deref());
        if let Some(limit) = session.meta.goal_continuation_limit {
            goal.set_continuation_limit(limit);
        }
        let permissions_ms = lap();
        let subagent_history = crate::agent::stored_subagent_history(&session);
        let subagent_history_ms = lap();
        let handles = AgentHandles::spawn(
            &self.model_slot,
            initial_history,
            archived_history,
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
            Arc::clone(&workspace_baseline),
            workspace_session.clone(),
            remote_project_context.clone(),
            self.host_cwd.clone(),
            self.local_documents.clone(),
        );
        let agent_spawn_ms = lap();
        let mut app = App::new(
            &self.model_slot.load().model,
            session,
            self.storage.clone(),
            snapshot_store,
            workspace_baseline,
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
        app.live_sessions = Arc::clone(&self.live_sessions);
        app.state.system_prompt_profile_name = system_prompt_profile_name;
        app.state.system_prompt_profile = system_prompt_profile;
        app.state.system_prompt_profile_override = self.prompt_profile_override.is_some();
        if let Some(warning) = profile_warning {
            app.state.warnings.push(warning);
        }
        handles.apply_to_app(&mut app);
        if restore_session {
            app.restore_resumed_session();
        }
        // After the transcript exists: a card can only be brought up to date
        // once the restore that draws it has run.
        app.refresh_workflow_cards();
        app.set_pattern_suggestion_loader(self.pattern_suggestion_loader.clone());
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
            app,
            lease,
            handles,
            shell_tx,
            shell_rx,
            last_status: SessionStatus::Idle,
            last_tasks: Vec::new(),
            notifications: RunNotificationState::default(),
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
    ctx: SpawnCtx,
    input: InputReader,
    auto_theme: Option<AutoSwitch>,
    warn_rx: flume::Receiver<String>,
    warn_tx: flume::Sender<String>,
    title_rx: flume::Receiver<GeneratedTitle>,
    title_tx: flume::Sender<GeneratedTitle>,
    ui_action_rx: flume::Receiver<UiAction>,
    herdr_reporter: Option<HerdrReporterHandle>,
    _model_fetch_task: smol::Task<()>,
    relocation: Option<PendingRelocation>,
    sandbox: Option<SandboxAttachment>,
    sandbox_control: Option<SandboxControl>,
    sandbox_workflows: Vec<WorkflowTransition>,
}

struct PendingRelocation {
    handoff: SessionRelocationHandoff,
    _workflows: Vec<WorkflowTransition>,
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

impl<'t> EventLoop<'t> {
    pub(crate) fn new(
        terminal: &'t mut ratatui::DefaultTerminal,
        params: EventLoopParams,
    ) -> Result<Self> {
        let EventLoopParams {
            mut model,
            needs_login,
            commands,
            no_commands,
            sessions,
            focused,
            mut startup_warnings,
            storage,
            config,
            ui_config,
            snapshots,
            allow_workspace_recovery,
            input_history_size,
            max_log_files,
            permissions,
            pattern_suggestion_loader,
            permission_authority_factory,
            sandbox_connector,
            transfer_connector,
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
            crate::update::spawn_check(ui_config.update_check);
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

        let notifier = terminal::TerminalNotifier::new(ui_config.notifications);
        let mut ctx = SpawnCtx {
            storage,
            config,
            ui_config,
            snapshots,
            allow_workspace_recovery,
            input_history_size,
            max_log_files,
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

        let active = sessions.iter().map(|tab| tab.session.id).collect();
        prepare_workspace_for_runtime(
            &ctx.storage,
            &ctx.storage_writer,
            &cwd,
            &active,
            ctx.snapshots.into(),
            ctx.allow_workspace_recovery,
        )
        .map_err(|error| eyre!(error))?;
        let recover_sessions_ms = lap();

        let mut runtimes: Vec<SessionRuntime> = sessions
            .into_iter()
            .map(|tab| ctx.spawn_runtime(tab))
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
        app.exit_on_done = exit_on_done;
        if needs_login {
            app.login_picker.open(app.storage.clone());
        }
        app.open_awaiting_mcp_trust(needs_login);
        app.open_awaiting_permission_config_trust(needs_login);
        if !ctx.mcp_config_errors.is_empty() {
            let msg = format!("MCP config error: {}", ctx.mcp_config_errors);
            app.flash(msg);
        }
        for w in startup_warnings {
            app.flash(w);
        }

        Ok(Self {
            terminal,
            sessions: runtimes,
            focused,
            started: Instant::now(),
            last_focused: None,
            last_workspace_tabs: None,
            terminal_focused: false,
            notifier,
            ctx,
            input: InputReader::spawn(),
            auto_theme,
            warn_rx: bg.warn_rx,
            warn_tx: bg.warn_tx,
            title_rx,
            title_tx,
            ui_action_rx,
            herdr_reporter,
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
        let report = self.shutdown()?;
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
            if !rt.handles.agent_rx.is_disconnected() {
                sel = sel.recv(&rt.handles.agent_rx, move |res| {
                    res.ok().map(|env| Wake::Agent(i, Box::new(env)))
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
        let _transition = match self.sessions[index].handles.workflow_handle() {
            Some(workflow) => match smol::block_on(workflow.suspend()) {
                Ok(transition) => Some(transition),
                Err(error) => {
                    self.sessions[index].app.flash(error.to_string());
                    return;
                }
            },
            None => None,
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

    fn sandbox_gate(&mut self, transition: bool) -> Result<Vec<WorkflowTransition>, String> {
        if self
            .sessions
            .iter()
            .any(|runtime| !runtime.work_quiescent())
        {
            return Err(
                "Wait for agents, queues, background tasks and shells before a sandbox action"
                    .into(),
            );
        }
        for runtime in &self.sessions {
            if let Some(reason) = runtime.app.sandbox_action_blocker(transition) {
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
            check_relocation_journals(&self.ctx.storage, runtime.id())?;
        }
        let mut workflows = Vec::new();
        for runtime in &self.sessions {
            if let Some(workflow) = runtime.handles.workflow_handle() {
                workflows
                    .push(smol::block_on(workflow.suspend()).map_err(|error| error.to_string())?);
            }
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
        let mut dirty = Dirty::NO;
        for index in 0..self.sessions.len() {
            if let Some(command) = self.sessions[index].app.sandbox_live.transfer_queued.take() {
                let admitted = if matches!(
                    command,
                    crate::sandbox::transfer::TransferCommand::Open { .. }
                ) {
                    self.sandbox_gate(false).map(|_| ())
                } else {
                    Ok(())
                };
                match admitted {
                    Ok(()) => self.sessions[index].app.start_transfer(command),
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
                    Ok(_workflows) => self.sessions[index].app.start_sandbox_live(*request),
                    Err(error) => self.sessions[index].app.sandbox_failed(error),
                }
                dirty = Dirty::YES;
            }
            if let Some(attachment) = self.sessions[index].app.sandbox_live.attachment.take() {
                match self.sandbox_gate(true) {
                    Ok(workflows) => {
                        self.sandbox_workflows = workflows;
                        self.sandbox = Some(attachment);
                    }
                    Err(error) => self.sessions[index].app.sandbox_failed(error),
                }
                dirty = Dirty::YES;
            }
        }
        dirty
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
        self.auto_theme = theme::pair_for(&current).map(|pair| AutoSwitch::adopt(pair, &current));
    }

    /// Re-ask the terminal for its background so a session left open across a
    /// light/dark switch follows it.
    ///
    /// The probe reads the tty directly, so it parks the input reader first
    /// and waits for a lull: bytes arriving mid-probe would be consumed
    /// instead of delivered as keystrokes.
    fn poll_appearance(&mut self) -> Dirty {
        let Some(auto) = self.auto_theme.as_mut() else {
            return Dirty::NO;
        };
        if !auto.due(Instant::now()) {
            return Dirty::NO;
        }
        if !self.input.receiver().is_empty() {
            auto.defer();
            return Dirty::NO;
        }
        let observed = {
            let _pause = self.input.pause();
            appearance::detect()
        };
        auto.observe(observed)
    }

    fn handle_agent(&mut self, idx: usize, envelope: Box<caudra_agent::Envelope>) {
        let rt = &mut self.sessions[idx];
        let current = is_current_top_level(rt.app.run_id, &envelope);
        match &envelope.event {
            AgentEvent::QueueDrained => {
                if current {
                    rt.notifications.on_drain();
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
        dirty |= self.start_mailbox_runs();
        dirty |= self.start_goal_checkins();
        self.emit_status_changes();
        self.publish_live_sessions();
        dirty |= self.emit_task_changes();
        self.emit_notifications();
        if let Some(reporter) = &self.herdr_reporter {
            reporter.observe(aggregate_observations(
                self.sessions.iter().map(SessionRuntime::herdr_observation),
            ));
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
            let status = SessionStatus::of(&rt.app);
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
        let mut changed = false;
        for rt in &mut self.sessions {
            let session_id = rt.app.state.session.id;
            diff_task_states(&mut rt.last_tasks, rt.app.task_states(), |task| {
                changed = true;
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
        if !changed {
            return Dirty::NO;
        }
        self.focused_app().refresh_task_picker()
    }

    fn emit_notifications(&mut self) {
        let Some(notifier) = &self.notifier else {
            return;
        };
        let mut selected = None;
        for rt in &mut self.sessions {
            let candidate = rt.notifications.reconcile(
                rt.app.attention(),
                rt.last_status,
                rt.handles.queue.is_empty(),
                self.terminal_focused,
            );
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
                activity: Some(SessionStatus::of(&rt.app).activity()),
                focused: i == self.focused,
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
        // The picker only ever lists the focused session, so a session switch
        // closes it rather than leaving ids from elsewhere on screen.
        self.focused_app().task_picker.close();
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
        let ready: Vec<_> = self
            .sessions
            .iter_mut()
            .enumerate()
            .filter_map(|(index, runtime)| {
                if !runtime.quiescent() || runtime.app.sandbox_network_dispatch_blocker().is_some()
                {
                    return None;
                }
                let mut preamble = runtime.handles.claim_mailbox_wake();
                preamble.extend(runtime.app.claim_workflow_completions());
                (!preamble.is_empty()).then_some((index, preamble))
            })
            .collect();

        let dirty = Dirty::from(!ready.is_empty());
        for (index, preamble) in ready {
            let actions = self.sessions[index].app.start_mailbox_run(preamble);
            self.dispatch(index, actions);
        }
        dirty
    }

    fn start_goal_checkins(&mut self) -> Dirty {
        let ready: Vec<_> = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, runtime)| {
                (runtime.quiescent()
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
                let runtime = match self.ctx.spawn_runtime(SessionTab {
                    session,
                    lease,
                    cursor: None,
                }) {
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
        let rt = self.sessions.remove(idx);
        if idx < self.focused {
            self.focused -= 1;
        }
        rt
    }

    fn push_runtime(&mut self, rt: SessionRuntime) -> usize {
        self.sessions.push(rt);
        self.sessions.len() - 1
    }

    /// Focus a live session, or bring a stored one up: in place when the
    /// focused session is a blank idle one (nothing worth keeping), otherwise
    /// as a new runtime so the session you came from stays live.
    fn focus_session(&mut self, id: CaudraId) -> Result<(), String> {
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
        let (profile_name, profile, profile_warning) = self.ctx.resolve_prompt_profile(&session);
        let focused = &mut self.sessions[self.focused];
        if focused.quiescent() && !focused.app.has_content() && self.ctx.workspace_session.is_none()
        {
            let model = focused.app.state.model.clone();
            let loaded = focused.app.apply_loaded_session(session, &model)?;
            focused.app.state.system_prompt_profile_name = profile_name;
            focused.app.state.system_prompt_profile = profile;
            focused.app.state.system_prompt_profile_override =
                self.ctx.prompt_profile_override.is_some();
            if let Some(warning) = profile_warning {
                focused.app.flash(warning);
            }
            let old_lease = std::mem::replace(&mut self.sessions[self.focused].lease, lease);
            self.dispatch(self.focused, vec![Action::LoadSession(Box::new(loaded))]);
            drop(old_lease);
            return Ok(());
        }
        let runtime = self.ctx.spawn_runtime(SessionTab {
            session,
            lease,
            cursor: None,
        })?;
        let idx = self.push_runtime(runtime);
        self.focused = idx;
        Ok(())
    }

    /// Handles one input event plus any leftover produced while coalescing
    /// bursts of scroll/drag events.
    fn handle_input(&mut self, raw: Event) {
        let mut pending = Some(raw);
        while let Some(ev) = pending.take() {
            let (msg, leftover) = self.translate(ev);
            if let Some(msg) = msg {
                let actions = self.sessions[self.focused].app.update(msg);
                self.dispatch(self.focused, actions);
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
            Event::Key(key) => (Some(Msg::Key(key)), None),
            Event::Paste(text) => (Some(Msg::Paste(text)), None),
            Event::Mouse(mouse) => self.translate_mouse(mouse),
            // Reattaching a multiplexer to another terminal resizes the
            // viewport, and that terminal may not share the old background.
            Event::Resize(..) => {
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
        for action in actions {
            if self.relocation.is_some() {
                break;
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
    }

    fn request_new_session(&mut self, idx: usize) -> bool {
        if !self.sessions[idx].work_quiescent() {
            let session = {
                let current = &self.sessions[idx].app.state.session;
                current.workspace_binding().map_or_else(
                    || AppSession::new(&current.model, &current.cwd),
                    |binding| {
                        AppSession::new_with_workspace(
                            &current.model,
                            &current.cwd,
                            binding.clone(),
                        )
                    },
                )
            };
            let lease = match SessionLease::acquire(&self.ctx.storage, session.id) {
                Ok(lease) => Arc::new(lease),
                Err(error) => {
                    self.sessions[idx]
                        .app
                        .flash(format!("Failed to reserve new session: {error}"));
                    return false;
                }
            };
            let runtime = match self.ctx.spawn_runtime(SessionTab {
                session,
                lease,
                cursor: None,
            }) {
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
        let actions = self.sessions[idx].app.reset_session();
        if actions.is_empty() {
            return false;
        }
        self.dispatch(idx, actions);
        true
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

    fn relocation_available(&self) -> Result<(), String> {
        if self.ctx.storage.is_ephemeral()
            || self
                .sessions
                .iter()
                .any(|runtime| runtime.app.workspace_baseline.is_remote())
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
            check_relocation_journals(&self.ctx.storage, id)?;
        }
        let mut workflows = Vec::new();
        for runtime in &self.sessions {
            if let Some(workflow) = runtime.handles.workflow_handle() {
                workflows
                    .push(smol::block_on(workflow.suspend()).map_err(|error| error.to_string())?);
            }
        }
        let database =
            SessionDatabase::open_state(&self.ctx.storage).map_err(|error| error.to_string())?;
        for expected in &request.sessions {
            if database
                .load_workflow_runs(expected.id)
                .map_err(|error| error.to_string())?
                .iter()
                .any(|run| {
                    matches!(
                        run.status,
                        WorkflowRunStatus::Active
                            | WorkflowRunStatus::Paused
                            | WorkflowRunStatus::BudgetLimited
                    )
                })
            {
                return Err(format!("{RELOCATION_WORKFLOW_ERR}: {}", expected.id));
            }
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
            handoff: SessionRelocationHandoff {
                request,
                donor,
                leases,
            },
            _workflows: workflows,
        })
    }

    fn change_working_directory(&mut self, idx: usize, cwd: PathBuf) {
        if let Some(error) = cwd_change_blocker(self.sessions.iter().map(|runtime| {
            (
                runtime.quiescent(),
                runtime.app.state.session.meta.pending_revert.is_some(),
            )
        })) {
            self.sessions[idx].app.flash(error.into());
            return;
        }

        let mut transitions = Vec::new();
        for runtime in &self.sessions {
            if let Some(workflow) = runtime.handles.workflow_handle() {
                match smol::block_on(workflow.suspend()) {
                    Ok(transition) => transitions.push(transition),
                    Err(error) => {
                        self.sessions[idx].app.flash(error.to_string());
                        return;
                    }
                }
            }
        }
        let mut stores = Vec::with_capacity(self.sessions.len());
        for runtime_index in 0..self.sessions.len() {
            let session_id = self.sessions[runtime_index].id();
            let store = match App::snapshot_store_for(
                &self.ctx.storage,
                session_id,
                &cwd,
                self.ctx.snapshots.into(),
            ) {
                Ok(store) => store,
                Err(error) => {
                    self.sessions[idx].app.flash(format!("cd: {error}"));
                    return;
                }
            };
            match store.journal_state() {
                Ok(None) => stores.push(store),
                Ok(Some(_)) => {
                    self.sessions[idx].app.flash(format!(
                        "cd: session {} has an unfinished workspace restore in {}",
                        session_id,
                        cwd.display()
                    ));
                    return;
                }
                Err(error) => {
                    self.sessions[idx]
                        .app
                        .flash(format!("cd: failed to inspect workspace restore: {error}"));
                    return;
                }
            }
        }

        if let Err(error) = std::env::set_current_dir(&cwd) {
            self.sessions[idx].app.flash(format!("cd: {error}"));
            return;
        }
        let permissions = load_permissions(&cwd);
        self.ctx
            .permissions
            .set_project_with_config(&cwd, permissions.clone());
        for (runtime, store) in self.sessions.iter_mut().zip(stores) {
            runtime.handles.shutdown_workflow();
            runtime
                .app
                .install_working_directory(&cwd, store, permissions.clone());
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
    }

    fn respawn_agent(&mut self, idx: usize, history: Vec<HistoryItem>) {
        let rt = &mut self.sessions[idx];
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
    }

    fn handle_action(&mut self, idx: usize, action: Action) {
        match action {
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
                let _ = rt.handles.cmd_tx.try_send(AgentCommand::Cancel { run_id });
            }
            Action::CancelSubagent { tool_use_id } => {
                let _ = self.sessions[idx]
                    .handles
                    .cmd_tx
                    .try_send(AgentCommand::CancelSubagent { tool_use_id });
            }
            Action::RequestNewSession => unreachable!("handled by dispatch"),
            Action::FocusSession(id) => {
                if let Err(error) = self.focus_session(id) {
                    self.sessions[idx].app.flash(error);
                }
            }
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
                } = *forked;
                if let Some(draft) = draft {
                    install_fork_draft(&mut session, draft);
                }
                let runtime = match self.ctx.spawn_runtime(SessionTab {
                    session,
                    lease,
                    cursor: None,
                }) {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        self.sessions[idx].app.flash(error);
                        return;
                    }
                };
                let child = self.push_runtime(runtime);
                let id = self.sessions[child].id();
                self.sessions[child].app.checkpoint_now();
                self.focused = child;
                caudra_otel::emit::session_started(
                    caudra_otel::emit::START_FORK,
                    Some(&id.to_string()),
                );
            }
            Action::RevertSession { source, mode } => {
                if !self.workspace_group_quiescent(idx) {
                    self.sessions[idx]
                        .app
                        .flash(crate::app::REVERT_BUSY_MSG.into());
                    return;
                }
                let actions = self.sessions[idx].app.revert_at(source, mode);
                self.dispatch(idx, actions);
            }
            Action::RewindSession(entry) => {
                if !self.workspace_group_quiescent(idx) {
                    self.sessions[idx]
                        .app
                        .flash(crate::app::REVERT_BUSY_MSG.into());
                    return;
                }
                let actions = self.sessions[idx].app.rewind_to(entry);
                self.dispatch(idx, actions);
            }
            Action::UnrevertSession => {
                if !self.workspace_group_quiescent(idx) {
                    self.sessions[idx]
                        .app
                        .flash(crate::app::REVERT_BUSY_MSG.into());
                    return;
                }
                let actions = self.sessions[idx].app.unrevert();
                self.dispatch(idx, actions);
            }
            Action::ChangeWorkingDirectory(cwd) => self.change_working_directory(idx, cwd),
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
                spawn_remote_cd(workspace, binding, path, runtime.shell_tx.clone());
            }
            Action::ChangeModel(spec) => {
                if let Err(e) = self.change_model(&spec) {
                    self.focused_app().flash(e);
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
                        caudra_providers::anthropic_auth::login_browser_callback(&storage)
                    }
                    crate::components::SubscriptionProvider::OpenAi => {
                        caudra_providers::openai_auth::login(&storage)
                    }
                });
                drop(pause);
                self.terminal_focused = false;

                match result {
                    Ok(()) => {
                        self.refresh_provider(provider.slug().into());
                        if let Err(error) = self.change_model(&model_spec) {
                            self.sessions[idx].app.flash(error);
                        }
                        self.refresh_models();
                        self.sessions[idx].app.flash(format!(
                            "Authenticated with {} subscription",
                            provider.display_name()
                        ));
                    }
                    Err(error) => self.sessions[idx]
                        .app
                        .flash(format!("{} login failed: {error}", provider.display_name())),
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
                    rt.app
                        .workspace_baseline
                        .set_current_head(rt.app.history_head());
                    spawn_remote_shell(
                        RemoteShellTarget {
                            workspace,
                            baseline: Arc::clone(&rt.app.workspace_baseline),
                        },
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
            Action::Suspend => {
                let _pause = self.input.pause();
                terminal::suspend(self.terminal);
                self.terminal_focused = false;
            }
            Action::RefreshModels => self.refresh_models(),
            Action::RefreshUsage => self.refresh_usage(),
            Action::RefreshStorage => self.refresh_storage(),
            Action::ManualExit => self.sessions[idx].notifications.on_manual_exit(),
        }
    }

    fn change_model(&mut self, spec: &str) -> Result<(), String> {
        if !self.ctx.model_policy.allows(spec) {
            return Err(format!("{MODEL_POLICY_ERR}: {spec}"));
        }
        let mut new_model =
            Model::from_spec(spec).map_err(|e| format!("{INVALID_MODEL_ERR}: {e}"))?;
        let new_provider = from_model(&mut new_model, self.ctx.timeouts)
            .map_err(|e| format!("{PROVIDER_INIT_ERR}: {e}"))?;
        let app = self.focused_app();
        app.select_model(&new_model);
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
        let profile = match self.ctx.prompt_profiles.resolve(Some(name)) {
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
        self.sessions[idx].app.checkpoint_now();
        self.sessions[idx]
            .app
            .flash(format!("System prompt: {name}"));
        self.respawn_agent(idx, history);
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

    /// Sizing the snapshot stores walks every object on disk, so the whole
    /// measurement goes to a blocking thread and the database is opened
    /// read-only: a diagnostic must never contend with the session writer.
    fn refresh_storage(&mut self) {
        let storage = self.ctx.storage.clone();
        let app = self.focused_app();
        let unavailable = app.file_revert_blocker();
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

    fn shutdown_quiescent(&self) -> bool {
        self.sessions.iter().all(|runtime| {
            SessionStatus::of(&runtime.app) == SessionStatus::Idle
                && runtime.handles.active_background_tasks() == 0
                && runtime.app.shell.active_ids().is_empty()
                && runtime.handles.agent_rx.is_empty()
                && runtime.shell_rx.is_empty()
        })
    }

    fn shutdown(mut self) -> Result<ShutdownReport> {
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
            rt.app.prepare_shutdown();
            rt.handles.shutdown_workflow();
            let _ = rt.handles.cmd_tx.try_send(AgentCommand::CancelAll);
        }
        let kill_mcp_ms = lap();
        let deadline = Instant::now() + AGENT_SHUTDOWN_TIMEOUT;
        loop {
            self.drain_shutdown_envelopes();
            if self.shutdown_quiescent() {
                break;
            }
            if Instant::now() >= deadline {
                if relocating {
                    relocation_error = Some("agents did not become idle".to_owned());
                }
                warn!("agents did not quiesce within {AGENT_SHUTDOWN_TIMEOUT:?}, forcing shutdown");
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
            relocation_error = Some("agents did not finish".to_owned());
        }
        let join_agents_ms = lap();

        let mut tabs = Vec::with_capacity(apps.len());
        // Split across the three operations so a slow exit points at one of
        // them instead of at the whole phase.
        let (mut snapshot_ms, mut checkpoint_ms, mut session_clone_ms) = (0, 0, 0);
        for (mut app, lease) in apps {
            let mut step = Instant::now();
            let mut step_ms = || {
                let elapsed = step.elapsed().as_millis() as u64;
                step = Instant::now();
                elapsed
            };
            // Only a session that captured a baseline has a bracket to close.
            // Exiting one that never wrote must not walk the tree on the way
            // out for a revert point nothing can reach.
            match app
                .has_revert_point()
                .then(|| app.snapshot_history_head_within(SHUTDOWN_SNAPSHOT_BUDGET))
            {
                None | Some(Ok(true)) => {}
                Some(Ok(false)) => warn!(
                    session_id = %app.state.session.id,
                    budget = ?SHUTDOWN_SNAPSHOT_BUDGET,
                    "artifact lock busy, skipping final workspace snapshot"
                ),
                Some(Err(error)) => {
                    warn!(session_id = %app.state.session.id, %error, "final workspace snapshot failed")
                }
            }
            snapshot_ms += step_ms();
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
            snapshot_ms,
            checkpoint_ms,
            session_clone_ms,
            mcp_shutdown_ms,
            storage_drain_ms,
            total_ms = started.elapsed().as_millis() as u64,
            "ui shutdown phases"
        );
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
    use crate::components::{key, test_model};
    use caudra_agent::snapshots::{
        ConflictPolicy, JournalState, RestoreTarget, SnapshotKey, workspace_key,
    };
    use caudra_agent::{DoneReason, McpSnapshotReader};
    use caudra_config::PermissionsConfig;
    use caudra_providers::{ImageMediaType, ImageSource, TokenUsage};
    use caudra_storage::sessions::{
        PendingConversationRevert, PendingRestoreKind, PendingRestoreOperation, PendingRestorePhase,
    };
    use crossterm::event::KeyCode;
    use tempfile::TempDir;
    use test_case::test_case;

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
    const RELOCATION_JOURNAL: &str = "restore-journal.json";
    const RELOCATION_FILE: &str = "tracked.txt";
    const RELOCATION_BEFORE: &str = "before restore";
    const RELOCATION_AFTER: &str = "after restore";
    const RELOCATION_INSPECT_ERR: &str = "Cannot inspect snapshots for session";
    const RELOCATION_RESTORE_ERR: &str =
        "has an unfinished workspace restore; resolve it before moving";
    const RELOCATION_PENDING_ERR: &str = "has a pending restore; resolve it before moving sessions";
    const RELOCATION_INPUT_HISTORY: usize = 100;
    const WRITER_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

    fn relocation_app(storage: StateDir, cwd: &Path, writer: Arc<StorageWriter>) -> App {
        let session = AppSession::new(RELOCATION_MODEL, cwd.to_str().unwrap());
        let store =
            App::snapshot_store_for(&storage, session.id, cwd, SnapshotLimits::default()).unwrap();
        let baseline = WorkspaceBaseline::new(Arc::clone(&store), cwd.to_path_buf(), true);
        App::new(
            &test_model(),
            session,
            storage,
            store,
            baseline,
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

    #[test_case("missing"; "missing_source_snapshot_state_allowed")]
    #[test_case("clean"; "clean_destination_allowed")]
    #[test_case("pending"; "destination_donor_pending_revert_refused")]
    #[test_case("corrupt"; "corrupt_destination_journal_refused")]
    #[test_case("unfinished"; "unfinished_destination_journal_refused")]
    fn relocation_startup_rechecks_destination_without_recovery_or_mutation(state: &str) {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let workspace = TempDir::new().unwrap();
        let cwd = workspace.path().canonicalize().unwrap();
        let mut donor = AppSession::new(RELOCATION_MODEL, cwd.to_str().unwrap());
        let file = cwd.join(RELOCATION_FILE);
        fs::write(&file, RELOCATION_BEFORE).unwrap();
        donor.save(&storage).unwrap();
        check_relocation_destination(&storage, &cwd).unwrap();
        if state == "pending" {
            donor.meta.pending_revert = Some(PendingConversationRevert {
                original_head: None,
                target_head: None,
                original_workspace_head: None,
                workspace_head: None,
                file_status: None,
                restore_operation: None,
            });
        }
        donor.save(&storage).unwrap();
        let root = storage
            .path()
            .join(SESSION_SNAPSHOTS_DIR)
            .join(donor.id.to_string());
        let journal = root
            .join(workspace_key(&cwd).unwrap())
            .join(RELOCATION_JOURNAL);
        if state != "missing" {
            let store =
                App::snapshot_store_for(&storage, donor.id, &cwd, SnapshotLimits::default())
                    .unwrap();
            store.snapshot_session_start(&cwd).unwrap();
            if state == "corrupt" {
                corrupt_restore_journal(&storage, &donor, &cwd);
            } else if state == "unfinished" {
                let source = CaudraId::generate();
                let target = CaudraId::generate();
                store.snapshot(&cwd, source).unwrap();
                fs::write(&file, RELOCATION_AFTER).unwrap();
                store.snapshot(&cwd, target).unwrap();
                fs::write(&file, RELOCATION_BEFORE).unwrap();
                store
                    .restore_transaction_with_policy(
                        &cwd,
                        &[source],
                        &[target],
                        ConflictPolicy::Abort,
                        CaudraId::generate(),
                    )
                    .unwrap();
                assert!(store.journal_state().unwrap().is_some());
            }
        }
        let journal_before = fs::read(&journal).ok();
        let file_before = fs::read(&file).unwrap();
        let session_before =
            serde_json::to_value(load_app_session(donor.id, &storage).unwrap()).unwrap();
        let version_before = SessionDatabase::open_state(&storage)
            .unwrap()
            .local_session_locations()
            .unwrap();
        let writer = StorageWriter::new(storage.clone(), flume::unbounded().0);
        let result = prepare_workspace_for_runtime(
            &storage,
            &writer,
            &cwd,
            &HashSet::new(),
            SnapshotLimits::default(),
            false,
        );
        match state {
            "pending" => assert_eq!(
                result,
                Err(format!(
                    "Destination session {} {RELOCATION_PENDING_ERR}",
                    donor.id
                ))
            ),
            "corrupt" => assert!(result.unwrap_err().starts_with(RELOCATION_INSPECT_ERR)),
            "unfinished" => assert_eq!(
                result,
                Err(format!("Session {} {RELOCATION_RESTORE_ERR}", donor.id))
            ),
            _ => result.unwrap(),
        }
        if state == "missing" {
            check_relocation_journals(&storage, donor.id).unwrap();
            assert!(!root.exists());
        }
        assert_eq!(fs::read(&journal).ok(), journal_before);
        assert_eq!(fs::read(&file).unwrap(), file_before);
        assert_eq!(
            serde_json::to_value(load_app_session(donor.id, &storage).unwrap()).unwrap(),
            session_before
        );
        assert_eq!(
            SessionDatabase::open_state(&storage)
                .unwrap()
                .local_session_locations()
                .unwrap(),
            version_before
        );
        writer.shutdown(WRITER_DRAIN_TIMEOUT);
    }

    #[test_case(false, true, false; "relocation_refuses_pending_restore")]
    #[test_case(false, false, false; "relocation_refuses_destination_journal")]
    #[test_case(false, false, true; "relocation_refuses_source_journal")]
    #[test_case(true, true, false; "normal_prepare_recovers_files_and_history")]
    #[test_case(true, false, false; "normal_prepare_recovers_journal_without_pending_restore")]
    fn runtime_preparation_gates_restore_recovery(
        allow_workspace_recovery: bool,
        pending_restore: bool,
        source_journal: bool,
    ) {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let cwd = canonical_cwd(&std::env::current_dir().unwrap()).unwrap();
        let workspace = TempDir::new_in(&cwd).unwrap();
        let source = canonical_cwd(workspace.path()).unwrap();
        let root = if source_journal { &source } else { &cwd };
        let file = source.join(RELOCATION_FILE);
        fs::write(&file, RELOCATION_BEFORE).unwrap();
        let mut session = AppSession::new(RELOCATION_MODEL, cwd.to_str().unwrap());
        let items = crate::history_items(&[
            Message::user(RELOCATION_BEFORE.into()),
            Message::user(RELOCATION_AFTER.into()),
        ]);
        let target_head = items[0].id;
        let source_head = items[1].id;
        session.replace_messages(items);
        session.save(&storage).unwrap();
        check_relocation_destination(&storage, &cwd).unwrap();
        let operation_id = pending_restore.then(CaudraId::generate);
        if let Some(id) = operation_id {
            session.set_conversation_state(
                Some(source_head),
                Some(PendingConversationRevert {
                    original_head: Some(source_head),
                    target_head: Some(target_head),
                    original_workspace_head: Some(Some(source_head).into()),
                    workspace_head: Some(Some(source_head).into()),
                    file_status: None,
                    restore_operation: Some(PendingRestoreOperation {
                        id,
                        kind: PendingRestoreKind::Revert,
                        phase: PendingRestorePhase::Intent,
                        target_workspace_head: Some(target_head).into(),
                        conversation_target: Some(Some(target_head).into()),
                        overwrite: true,
                    }),
                }),
            );
            session.save(&storage).unwrap();
        }
        let store_dir = storage
            .path()
            .join(SESSION_SNAPSHOTS_DIR)
            .join(session.id.to_string())
            .join(workspace_key(root).unwrap());
        fs::create_dir_all(&store_dir).unwrap();
        let journal = store_dir.join(RELOCATION_JOURNAL);
        fs::write(
            &journal,
            serde_json::to_vec(&json!({
                "state": JournalState::Prepare,
                "root": root,
                "before": {},
                "target": {},
                "paths": [file.strip_prefix(root).unwrap()],
                "outcomes": [],
                "destination": RestoreTarget::Snapshot(SnapshotKey::Checkpoint(target_head)),
                "policy": ConflictPolicy::Overwrite,
                "operation_id": operation_id,
            }))
            .unwrap(),
        )
        .unwrap();
        let journal_before = fs::read(&journal).unwrap();
        let session_before =
            serde_json::to_value(load_app_session(session.id, &storage).unwrap()).unwrap();
        let writer = StorageWriter::new(storage.clone(), flume::unbounded().0);
        let result = prepare_session_for_runtime(
            &storage,
            &writer,
            session.clone(),
            SnapshotLimits::default(),
            None,
            allow_workspace_recovery,
        );
        if allow_workspace_recovery {
            let (recovered, store) = result.unwrap();
            assert!(!file.exists());
            assert_eq!(store.journal_state().unwrap(), None);
            let expected_head = if pending_restore {
                target_head
            } else {
                source_head
            };
            assert_eq!(crate::session_history_head(&recovered), Some(expected_head));
            let stored = load_app_session(session.id, &storage).unwrap();
            assert_eq!(crate::session_history_head(&stored), Some(expected_head));
            assert!(
                stored
                    .meta
                    .pending_revert
                    .as_ref()
                    .is_none_or(|pending| { pending.restore_operation.is_none() })
            );
        } else {
            let error = result.err().unwrap();
            let expected = if pending_restore {
                RELOCATION_PENDING_ERR
            } else {
                RELOCATION_RESTORE_ERR
            };
            assert_eq!(error, format!("Session {} {expected}", session.id));
            assert_eq!(fs::read_to_string(&file).unwrap(), RELOCATION_BEFORE);
            assert_eq!(fs::read(&journal).unwrap(), journal_before);
            assert_eq!(
                serde_json::to_value(load_app_session(session.id, &storage).unwrap()).unwrap(),
                session_before
            );
        }
        writer.shutdown(WRITER_DRAIN_TIMEOUT);
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
    }

    #[test]
    fn blocker_outranks_active_work() {
        assert_eq!(
            runtime_observation(Some(HERDR_BLOCKER), true, false, true, 1, 1),
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

    fn corrupt_restore_journal(storage: &StateDir, session: &AppSession, cwd: &Path) {
        let store =
            App::snapshot_store_for(storage, session.id, cwd, SnapshotLimits::default()).unwrap();
        store.snapshot_session_start(cwd).unwrap();
        let store_dir = storage
            .path()
            .join(caudra_agent::snapshots::SESSION_SNAPSHOTS_DIR)
            .join(session.id.to_string())
            .join(caudra_agent::snapshots::workspace_key(cwd).unwrap());
        std::fs::write(store_dir.join("restore-journal.json"), "not json").unwrap();
    }

    #[test]
    fn runtime_preparation_returns_recovery_error_without_flushing_stored_queue() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let cwd = canonical_cwd(&std::env::current_dir().unwrap()).unwrap();
        let mut session = AppSession::new("test-model", &cwd.to_string_lossy());
        session.meta.queued_messages = vec![caudra_storage::sessions::StoredQueuedPrompt {
            text: "still queued".into(),
            images: Vec::new(),
            paste_ranges: Vec::new(),
        }];
        session.save(&storage).unwrap();
        corrupt_restore_journal(&storage, &session, &cwd);
        let writer = StorageWriter::new(storage.clone(), flume::unbounded().0);

        let error = match prepare_session_for_runtime(
            &storage,
            &writer,
            session.clone(),
            SnapshotLimits::default(),
            None,
            true,
        ) {
            Ok(_) => panic!("corrupt recovery journal unexpectedly allowed runtime preparation"),
            Err(error) => error,
        };

        assert!(
            error.contains("inspect workspace restore journal"),
            "{error}"
        );
        assert_eq!(
            load_app_session(session.id, &storage)
                .unwrap()
                .meta
                .queued_messages,
            [caudra_storage::sessions::StoredQueuedPrompt {
                text: "still queued".into(),
                images: Vec::new(),
                paste_ranges: Vec::new(),
            }]
        );
        writer.shutdown(Duration::from_secs(30));
    }

    #[test]
    fn startup_scans_unopened_sessions_for_restore_journals() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let cwd = TempDir::new().unwrap();
        let cwd = canonical_cwd(cwd.path()).unwrap();
        let mut resumed = AppSession::new("test-model", &cwd.to_string_lossy());
        crate::push_history_message(&mut resumed, Message::user("resumed".into()));
        resumed.save(&storage).unwrap();
        let mut unopened = AppSession::new("test-model", &cwd.to_string_lossy());
        crate::push_history_message(&mut unopened, Message::user("unopened".into()));
        unopened.save(&storage).unwrap();
        corrupt_restore_journal(&storage, &unopened, &cwd);
        let writer = StorageWriter::new(storage.clone(), flume::unbounded().0);

        let error = recover_stored_sessions_in_cwd(
            &storage,
            &writer,
            &cwd,
            &std::collections::HashSet::new(),
            SnapshotLimits::default(),
        )
        .unwrap_err();

        assert!(
            error.contains("inspect workspace restore journal"),
            "{error}"
        );
        writer.shutdown(Duration::from_secs(30));
    }

    #[test]
    fn startup_recovers_an_interrupted_restore_for_an_unopened_session() {
        use caudra_storage::sessions::{
            PendingConversationRevert, PendingRestoreKind, PendingRestoreOperation,
            PendingRestorePhase,
        };

        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().to_path_buf());
        let workspace = TempDir::new().unwrap();
        let cwd = canonical_cwd(workspace.path()).unwrap();
        let file = cwd.join("tracked.txt");
        let mut resumed = AppSession::new("test-model", &cwd.to_string_lossy());
        crate::push_history_message(&mut resumed, Message::user("resumed".into()));
        resumed.save(&storage).unwrap();

        let mut unopened = AppSession::new("test-model", &cwd.to_string_lossy());
        let items = crate::history_items(&[
            Message::user("target".into()),
            Message::user("source".into()),
        ]);
        let target_head = items[0].id;
        let source_head = items[1].id;
        unopened.replace_messages(items);
        let store = App::snapshot_store_for(&storage, unopened.id, &cwd, SnapshotLimits::default())
            .unwrap();
        std::fs::write(&file, "root").unwrap();
        store.snapshot_session_start(&cwd).unwrap();
        std::fs::write(&file, "target").unwrap();
        store.snapshot(&cwd, target_head).unwrap();
        std::fs::write(&file, "source").unwrap();
        store.snapshot(&cwd, source_head).unwrap();
        let operation_id = CaudraId::generate();
        unopened.set_conversation_state(
            Some(source_head),
            Some(PendingConversationRevert {
                original_head: Some(source_head),
                target_head: Some(target_head),
                original_workspace_head: Some(Some(source_head).into()),
                workspace_head: Some(Some(source_head).into()),
                file_status: None,
                restore_operation: Some(PendingRestoreOperation {
                    id: operation_id,
                    kind: PendingRestoreKind::Revert,
                    phase: PendingRestorePhase::Intent,
                    target_workspace_head: Some(target_head).into(),
                    conversation_target: Some(Some(target_head).into()),
                    overwrite: false,
                }),
            }),
        );
        unopened.save(&storage).unwrap();
        store
            .restore_transaction_with_policy(
                &cwd,
                &[source_head],
                &[target_head],
                caudra_agent::snapshots::ConflictPolicy::Abort,
                operation_id,
            )
            .unwrap();
        let writer = StorageWriter::new(storage.clone(), flume::unbounded().0);

        prepare_workspace_for_runtime(
            &storage,
            &writer,
            &cwd,
            &std::collections::HashSet::new(),
            SnapshotLimits::default(),
            true,
        )
        .unwrap();

        let recovered = load_app_session(unopened.id, &storage).unwrap();
        assert_eq!(crate::session_history_head(&recovered), Some(target_head));
        assert!(
            recovered
                .meta
                .pending_revert
                .as_ref()
                .is_some_and(|pending| pending.restore_operation.is_none())
        );
        assert_eq!(std::fs::read_to_string(file).unwrap(), "target");
        assert_eq!(store.journal_state().unwrap(), None);
        writer.shutdown(Duration::from_secs(30));
    }
}
