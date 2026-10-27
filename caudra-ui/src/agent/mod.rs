mod agent_loop;
mod automation;
mod cancel_map;
mod command_router;
mod settings;
pub(crate) mod shared_queue;
mod workflow;

use std::collections::HashSet;
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use caudra_agent::agent::task_runner::WorkflowHostContext;
use caudra_agent::automation::workflows::Workflows;
use caudra_agent::background::{BackgroundTasks, BackgroundTransition};
use caudra_agent::context::{ContextKey, ContextStore, RequestPrefix};
use caudra_agent::permissions::PermissionManager;
use caudra_agent::prompt::profile::PromptProfileCatalog;
use caudra_agent::prompt::profile::{BUILTIN_PROFILE_NAME, SystemPromptProfile};
use caudra_agent::remote_project_context::{RemoteProjectContext, load_remote_project_context};
use caudra_agent::restart::{self, DanglingCall, RestoredWork};
use caudra_agent::tools::{PathLocks, ToolContext};
use caudra_agent::types::TodoItem;
use caudra_agent::workflow::{WorkflowHandle, WorkspaceRebind, prepare_workspace_transition};
use caudra_agent::{
    AgentConfig, AgentMode, CancelMap, CancelToken, ConversationControl, Envelope, EventSender,
    HistorySnapshot, McpCommand, McpConfigErrors, McpHandle, McpSnapshotReader, SessionMailbox,
    SharedHistory, SubagentHistoryStore, ToolOutputLines,
};
use caudra_automation::event::SessionView;
use caudra_automation::request::ProfileArming;
use caudra_automation::snapshot::AutomationEvent;
use caudra_config::{AutomationsConfig, CacheWarmingMode, FeatureFlags, ModelPolicy};
use caudra_lua::EventHandle;
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::id::SessionRef;
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::sessions::SessionLease;
use caudra_storage::tool_outputs::ToolOutputStore;
use caudra_workspace::WorkspaceSession;
use futures_lite::future;
use jiff::Timestamp;

use self::cancel_map::new_run_cancel_map;
use caudra_providers::provider::Provider;
use caudra_providers::{AgentError, HistoryItem, Message, Model, RequestOptions, project_messages};
use serde_json::Value;
use tracing::{info, warn};

use crate::app::App;
use crate::app::automation::AutomationLink;
use crate::app::background_delivery::DeliveryFence;
use crate::app::file_revert::RecorderSlot;

use self::agent_loop::AgentLoop;
pub(crate) use self::agent_loop::ToolsPreviewSource;
#[cfg(test)]
pub(crate) use self::agent_loop::tests::tools_preview_source;
use self::automation::{AutomationSession, AutomationSpawn};
use self::command_router::spawn_command_router;
use self::settings::LiveSettings;
pub(crate) use self::settings::Settings;
pub(crate) use self::shared_queue::{QueueSender, QueuedMessage};
pub(crate) use self::workflow::SharedMode;
use self::workflow::{WorkflowSession, WorkflowSpawn, answer_channel};

const TRANSITION_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
const BACKGROUND_TRANSITION_BUSY: &str =
    "Wait for background tasks to settle before changing the workspace";

pub(crate) fn reserve_background_transition(
    tasks: &BackgroundTasks,
) -> Result<BackgroundTransition, String> {
    let transition = tasks.suspend()?;
    if tasks.active_count() != 0 {
        return Err(BACKGROUND_TRANSITION_BUSY.into());
    }
    smol::block_on(future::or(transition.drain(), async {
        smol::Timer::after(TRANSITION_DRAIN_TIMEOUT).await;
        Err(BACKGROUND_TRANSITION_BUSY.into())
    }))?;
    Ok(transition)
}

pub(crate) async fn task_context(
    host: &WorkflowHostContext,
    workflow: Option<&WorkflowHandle>,
    cancel: CancelToken,
    events: EventSender,
    call_id: &str,
) -> Result<ToolContext, String> {
    let mut host = host.clone();
    if let Some(workspace) = &host.workspace_session {
        let (context, cwd) = refresh_remote_context(
            workspace,
            host.remote_project_context.as_deref(),
            &host.permissions,
            workflow,
            &host.subagent_cancels,
            host.config.features,
        )
        .await
        .map_err(|error| error.user_message())?;
        host.loaded_instructions =
            caudra_agent::agent::load_remote_instructions(&context, host.host_cwd.as_deref())
                .loaded;
        host.remote_project_context = Some(context);
        host.task_environment = host.task_environment.set("{cwd}", cwd);
    }
    host.tool_context(cancel, events, call_id).await
}

async fn refresh_remote_context(
    workspace: &WorkspaceSession,
    previous: Option<&RemoteProjectContext>,
    permissions: &PermissionManager,
    workflow: Option<&WorkflowHandle>,
    subagent_cancels: &CancelMap<String>,
    features: FeatureFlags,
) -> Result<(Arc<RemoteProjectContext>, String), AgentError> {
    let result = async {
        let context = load_remote_project_context(workspace, features)
            .await
            .map_err(|error| AgentError::Tool {
                tool: "remote_project_context".into(),
                message: format!("Remote project context unavailable: {error}"),
            })?;
        let cwd = caudra_agent::workspace_logical_cwd(workspace)
            .await
            .map_err(|message| AgentError::Tool {
                tool: "remote_project_context".into(),
                message,
            })?;
        let changed =
            previous.is_none_or(|old| old.manifest_revision() != context.manifest_revision());
        let transition = if changed {
            prepare_workspace_transition(
                workflow,
                subagent_cancels.active_count(),
                WorkspaceRebind {
                    workspace: workspace.clone(),
                    context: Arc::clone(&context),
                    cwd: cwd.clone(),
                },
            )
            .await
            .map_err(|message| AgentError::Tool {
                tool: "remote_project_context".into(),
                message,
            })?
        } else {
            None
        };
        permissions
            .replace_remote_permission_asset(context.permissions())
            .map_err(|error| AgentError::Tool {
                tool: "remote_permissions".into(),
                message: format!("Remote permission policy unavailable: {error}"),
            })?;
        if let Some(transition) = transition {
            transition
                .commit()
                .await
                .map_err(|error| AgentError::Tool {
                    tool: "workflow".into(),
                    message: error.to_string(),
                })?;
        }
        Ok((context, cwd))
    }
    .await;
    result.inspect_err(|_| permissions.invalidate_remote_permission_asset())
}

pub(crate) struct ModelSlot {
    pub(crate) model: Model,
    pub(crate) provider: Arc<dyn Provider>,
    /// The model the user chose, while their subscriptions ran out and the
    /// conversation moved to `model` on another provider. A pick replaces the
    /// slot, which drops it.
    pub(crate) home: Option<Model>,
}

/// The prefix of the last live request and the route that sent it, captured
/// where the run builds it and published as one unit so a model switch cannot
/// pair a new provider with stale tools or system text.
pub(crate) struct BtwPrompt {
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) model: Model,
    pub(crate) system: String,
    pub(crate) tools: Value,
    pub(crate) opts: RequestOptions,
    /// The profile `system` was built from. An edit on disk moves past it, and
    /// `system` keeps it until the prompt is rebuilt.
    pub(crate) profile: Option<Arc<SystemPromptProfile>>,
}

impl BtwPrompt {
    /// Whether `current`, the profile as the session holds it now, builds
    /// another prompt than the one `system` was built from.
    pub(crate) fn outdated_by(&self, current: Option<&SystemPromptProfile>) -> bool {
        match (self.profile.as_deref(), current) {
            (Some(bound), Some(current)) => !bound.same_prompt(current),
            (bound, current) => bound.is_some() != current.is_some(),
        }
    }
}

impl From<&RequestPrefix> for BtwPrompt {
    fn from(prefix: &RequestPrefix) -> Self {
        Self {
            provider: Arc::clone(&prefix.provider),
            model: prefix.model.clone(),
            system: prefix.system.clone(),
            tools: prefix.tools.clone(),
            opts: prefix.opts.clone(),
            profile: None,
        }
    }
}

pub(crate) type SharedBtwPrompt = Arc<ArcSwap<BtwPrompt>>;

pub(crate) enum AgentCommand {
    Cancel {
        run_id: u64,
    },
    CancelAll,
    CancelSubagent {
        tool_use_id: String,
    },
    /// Stop waiting out a retry backoff and try again now.
    RetryNow,
    /// The next request sends different thinking or speed settings, so the
    /// cache the last one wrote will not be read again.
    PrefixChanged,
    /// The config's `cache_warming` as it now reads, applied at once so
    /// turning it off also ends warming between runs.
    CacheWarming(CacheWarmingMode),
}

/// Input channels (`cmd_tx`, `answer_tx`, `queue`) are per-agent, so an old
/// loop can never steal new input. The output channel (`agent_tx`/`agent_rx`)
/// is per-tab: `respawn` reuses it, so anyone still holding a sender (a Lua
/// restore reply, a click, an old agent winding down) can always deliver.
/// Stale events are filtered by `run_id`, not by killing the channel.
pub(crate) struct AgentHandles {
    pub(crate) cmd_tx: flume::Sender<AgentCommand>,
    pub(crate) agent_rx: flume::Receiver<Envelope>,
    pub(crate) agent_tx: flume::Sender<Envelope>,
    pub(crate) answer_tx: flume::Sender<String>,
    pub(crate) history: SharedHistory,
    pub(crate) btw_prompt: SharedBtwPrompt,
    pub(crate) context_store: ContextStore,
    tools_preview_source: Arc<ToolsPreviewSource>,
    /// Resolved for the active lane without replacing the selected Chat slot.
    pub(crate) effective_model_slot: Arc<ArcSwap<ModelSlot>>,
    /// The main run's picks and backoff wake, which the app holds a clone of.
    pub(crate) control: ConversationControl,
    pub(crate) execution_mode: SharedMode,
    pub(crate) mcp_handle: Option<McpHandle>,
    pub(crate) mcp_config_errors: McpConfigErrors,
    pub(crate) queue: QueueSender,
    pub(crate) goal: caudra_agent::GoalHandle,
    subagent_cancels: Arc<CancelMap<String>>,
    pub(crate) timeouts: caudra_providers::Timeouts,
    model_policy: Arc<ModelPolicy>,
    prompt_profiles: Arc<PromptProfileCatalog>,
    mailbox: Option<SessionMailbox>,
    /// Session-lifetime: `respawn` carries it over untouched and only a change
    /// of session id replaces it.
    workflow: Option<WorkflowSession>,
    /// Session-lifetime like `workflow`, but started once the app exists, by
    /// [`Self::start_automations`]: its launch facts come from the app.
    automation: Option<AutomationSession>,
    pub(crate) background: Option<BackgroundTasks>,
    /// What the previous process left cut off when this loop opened a
    /// session, for the app to take once it has drawn the transcript.
    pub(crate) interrupted: Option<(RestoredWork, Vec<DanglingCall>)>,
    pub(crate) task_host: Option<Arc<WorkflowHostContext>>,
    /// The session's continual learner, while one runs.
    pub(crate) learner: Arc<arc_swap::ArcSwapOption<caudra_agent::learning::LearnerHandle>>,
    delivery_fence: Arc<DeliveryFence>,
    background_enabled: bool,
    session_id: Option<CaudraId>,
    subagent_history: SubagentHistoryStore,
    pub(crate) background_wake_rx: flume::Receiver<()>,
    _background_notifier: Option<smol::Task<()>>,
    /// Tab-lifetime, like the output channel: the loop being replaced and the
    /// workflow agents that outlive it may still be writing, so every
    /// generation queues on the same per-file locks.
    path_locks: Arc<PathLocks>,
    workspace_session: Option<WorkspaceSession>,
    remote_project_context: Option<Arc<caudra_agent::remote_project_context::RemoteProjectContext>>,
    /// The directory Caudra itself runs in. Set only in a sandbox session,
    /// where `{cwd}` names a path inside the VM and this one does not.
    host_cwd: Option<PathBuf>,
    local_documents: Option<Arc<LocalDocumentStore>>,
    settings: LiveSettings,
    task: smol::Task<()>,
}

impl AgentHandles {
    /// MCP is shared across sessions and agent respawns; the event loop starts it
    /// once and shuts it down at exit. Only the agent loop task lives here.
    /// The workflow runtime needs `state_dir` alongside a session id; without
    /// both the session runs with no workflow support.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        model_slot: &Arc<ArcSwap<ModelSlot>>,
        initial_history: Vec<HistoryItem>,
        archived_history: Vec<HistoryItem>,
        todos: Option<Vec<TodoItem>>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        permissions: &Arc<PermissionManager>,
        session_id: Option<SessionRef>,
        session_lease: Option<Arc<SessionLease>>,
        timeouts: caudra_providers::Timeouts,
        lua_handle: EventHandle,
        mcp_handle: Option<McpHandle>,
        mcp_config_errors: McpConfigErrors,
        model_policy: Arc<ModelPolicy>,
        goal: caudra_agent::GoalHandle,
        subagent_history: SubagentHistoryStore,
        system_prompt_profile: Option<Arc<SystemPromptProfile>>,
        prompt_profiles: Arc<PromptProfileCatalog>,
        state_dir: Option<StateDir>,
        change_recorder: RecorderSlot,
        workspace_session: Option<WorkspaceSession>,
        remote_project_context: Option<
            Arc<caudra_agent::remote_project_context::RemoteProjectContext>,
        >,
        host_cwd: Option<PathBuf>,
        local_documents: Option<Arc<LocalDocumentStore>>,
        background_enabled: bool,
    ) -> Self {
        let background = background_enabled
            .then(|| state_dir.clone())
            .flatten()
            .zip(session_id.as_ref())
            .and_then(|(storage, session)| {
                smol::block_on(BackgroundTasks::spawn(storage, session.id()))
                    .map_err(|error| warn!(%error, "background tasks unavailable"))
                    .ok()
            });
        let mut initial_history = initial_history;
        let interrupted = take_interrupted(background.as_ref(), &mut initial_history);
        let mut handles = spawn_agent_internal(
            flume::unbounded(),
            model_slot,
            Arc::new(ArcSwap::new(model_slot.load_full())),
            Arc::new(ArcSwap::from_pointee(AgentMode::default())),
            initial_history,
            archived_history,
            todos,
            config,
            tool_output_lines,
            state_dir.clone().map(ToolOutputStore::new).map(Arc::new),
            permissions,
            mcp_handle,
            mcp_config_errors,
            session_id,
            session_lease,
            timeouts,
            lua_handle,
            model_policy,
            goal,
            subagent_history,
            system_prompt_profile,
            Arc::clone(&prompt_profiles),
            WorkflowSlot::Fresh(state_dir),
            background,
            background_enabled,
            Arc::default(),
            PathLocks::fresh(),
            change_recorder,
            workspace_session,
            remote_project_context,
            host_cwd,
            local_documents,
        );
        handles.interrupted = Some(interrupted);
        handles
    }

    pub(crate) fn workflow_handle(&self) -> Option<WorkflowHandle> {
        self.workflow.as_ref().map(WorkflowSession::handle)
    }

    /// Settings the session's next turn starts with, and the next loop a
    /// respawn starts.
    pub(crate) fn publish_settings(&mut self, settings: Settings) {
        self.prompt_profiles = Arc::clone(&settings.prompt_profiles);
        if self.mcp_handle.is_none() {
            self.mcp_handle.clone_from(&settings.mcp);
        }
        let _ = self
            .cmd_tx
            .try_send(AgentCommand::CacheWarming(settings.config.cache_warming));
        self.settings.publish(settings);
    }

    /// Interrupts every run of this session and waits for the runtimes to
    /// close. Idempotent, so exit can call it ahead of the agent join.
    /// Automations stop first, so no firing starts during the teardown and no
    /// run it interrupts reaches them.
    pub(crate) fn shutdown_workflow(&mut self) {
        self.shutdown_automations();
        if let Some(background) = self.background.take()
            && let Err(error) = smol::block_on(background.shutdown())
        {
            warn!(%error, "background task shutdown failed");
        }
        if let Some(workflow) = self.workflow.take() {
            workflow.shutdown();
        }
    }

    /// The app keeps its handle, whose mirror holds the final counters for
    /// the save that follows.
    fn shutdown_automations(&mut self) {
        if let Some(automation) = self.automation.take() {
            automation.shutdown();
        }
    }

    /// Starts this session's automation runtime unless one already serves
    /// it, and points the app at it. Only the session focused at launch
    /// takes the CLI's `cli` entries.
    pub(crate) fn start_automations(
        &mut self,
        app: &mut App,
        config: &AutomationsConfig,
        cli: Vec<ProfileArming>,
    ) {
        if self.automation.is_some() {
            return;
        }
        let facts = app.automation_facts(false, None, Timestamp::now().as_second());
        self.automation =
            AutomationSession::spawn(self.automation_spawn(app, config, cli, facts.clone()));
        app.automation = self
            .automation
            .as_ref()
            .map_or_else(AutomationLink::default, |automation| {
                AutomationLink::new(automation.handle(), facts)
            });
    }

    /// What this session's automation runtime starts from. It reaches the
    /// session's workflow runtime only while one runs, which `respawn` keeps
    /// true by restarting the automations whenever that runtime changes.
    fn automation_spawn<'a>(
        &'a self,
        app: &'a App,
        config: &AutomationsConfig,
        cli: Vec<ProfileArming>,
        facts: SessionView,
    ) -> AutomationSpawn<'a> {
        AutomationSpawn {
            state_dir: app.storage.clone(),
            session_id: app.state.session.id,
            workspace_session: self.workspace_session.as_ref(),
            sandbox: app
                .state
                .session
                .workspace_binding()
                .is_some_and(|binding| binding.sandbox_record().is_some()),
            project_cwd: app.permissions.project_cwd(),
            features: app.features,
            config: config.clone(),
            controls: app.state.session.meta.automations.clone(),
            profile: app.state.system_prompt_profile.as_deref(),
            cli,
            facts,
            workflows: self
                .workflow_handle()
                .map(|handle| Arc::new(handle) as Arc<dyn Workflows>),
        }
    }

    pub(crate) fn automation_events(&self) -> Option<&flume::Receiver<AutomationEvent>> {
        self.automation.as_ref().map(AutomationSession::events)
    }

    pub(crate) fn mailbox_wake_pending(&self) -> bool {
        self.mailbox
            .as_ref()
            .is_some_and(SessionMailbox::wake_pending)
    }

    pub(crate) fn mcp_reader(&self) -> McpSnapshotReader {
        self.mcp_handle
            .as_ref()
            .map(McpHandle::reader)
            .unwrap_or_else(McpSnapshotReader::empty)
    }

    pub(crate) fn apply_to_app(&self, app: &mut App) {
        let sandbox = app
            .state
            .session
            .workspace_binding()
            .and_then(|binding| binding.sandbox_record());
        let gate = app.sandbox_live.network_gate.clone();
        let fence = Arc::clone(&self.delivery_fence);
        self.queue.set_dispatch_guard(Arc::new(move || {
            fence.dispatch_allowed()
                && sandbox.is_none_or(|id| gate.lock().is_ok_and(|gate| gate.blocker(id).is_none()))
        }));
        app.answer_tx = Some(self.answer_tx.clone());
        app.cmd_tx = Some(self.cmd_tx.clone());
        app.shared_history = Some(Arc::clone(&self.history));
        app.forget_merged_history();
        app.btw_prompt = Some(Arc::clone(&self.btw_prompt));
        app.subagent_history = Some(self.subagent_history.clone());
        app.learner = Some(Arc::clone(&self.learner));
        app.context_store = Some(self.context_store.clone());
        app.tools_preview_source = Some(Arc::clone(&self.tools_preview_source));
        if app
            .execution_mode
            .as_ref()
            .is_none_or(|mode| !Arc::ptr_eq(mode, &self.execution_mode))
        {
            self.execution_mode
                .store(Arc::new(app.execution_agent_mode()));
        }
        app.execution_mode = Some(Arc::clone(&self.execution_mode));
        self.queue
            .set_execution_mode(AgentMode::clone(&self.execution_mode.load()));
        app.effective_model_slot = Some(Arc::clone(&self.effective_model_slot));
        app.main_control = Some(self.control.clone());
        app.queue.set_shared(self.queue.clone());
        if self.goal.status().is_none() {
            self.goal.adopt(&app.state.goal);
        }
        app.state.goal = self.goal.clone();
        app.workflow.set_handle(self.workflow_handle());
        app.background = self.background.clone();
        app.task_host = self.task_host.clone();
        app.background_delivery.invalidate();
        app.background_delivery.fence = Arc::clone(&self.delivery_fence);
        let restore_tx =
            caudra_agent::EventSender::new(self.agent_tx.clone(), crate::app::RESTORE_RUN_ID);
        app.restore_event_tx = Some(restore_tx.clone());
        for chat in &mut app.chats {
            chat.set_restore_channel(Some(restore_tx.clone()));
        }
    }

    pub(crate) fn cancel(mut self) {
        self.shutdown_automations();
        let _ = self.cmd_tx.try_send(AgentCommand::CancelAll);
    }

    pub(crate) fn rebind_workspace(
        &mut self,
        workspace: WorkspaceSession,
        context: Arc<caudra_agent::remote_project_context::RemoteProjectContext>,
    ) {
        self.workspace_session = Some(workspace);
        self.remote_project_context = Some(context);
        self.shutdown_workflow();
    }

    pub(crate) fn send_mcp(&self, cmd: McpCommand) {
        if let Some(ref h) = self.mcp_handle {
            h.send(cmd);
        }
    }

    pub(crate) fn claim_mailbox_wake(&self) -> Vec<Message> {
        self.mailbox
            .as_ref()
            .map(SessionMailbox::claim_wake)
            .unwrap_or_default()
    }

    /// Background subagents plus the workflow runs in progress: both keep
    /// working after the turn that started them, and both must be over before
    /// the session counts as quiescent.
    pub(crate) fn active_background_tasks(&self) -> usize {
        self.subagent_cancels.active_count()
            + self.active_workflow_runs()
            + self
                .background
                .as_ref()
                .map_or(0, BackgroundTasks::active_count)
    }

    pub(crate) fn active_workflow_runs(&self) -> usize {
        self.workflow
            .as_ref()
            .map_or(0, WorkflowSession::active_runs)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn respawn(
        &mut self,
        history: Vec<HistoryItem>,
        model_slot: &Arc<ArcSwap<ModelSlot>>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        permissions: &Arc<PermissionManager>,
        app: &mut App,
        lua_handle: EventHandle,
        session_lease: Option<Arc<SessionLease>>,
    ) {
        // The output channel survives the respawn, so this bump is the only
        // thing that makes the old loop's in-flight envelopes stale. It lives
        // here so no caller can respawn without it.
        app.run_id += 1;
        let slot = model_slot.load();
        if let Err(e) = smol::block_on(slot.provider.reload_auth()) {
            warn!(error = %e, "failed to reload auth, continuing with existing credentials");
        }
        let same_session = self.session_id == Some(app.state.session.id);
        let subagent_history = if same_session {
            self.subagent_history.clone()
        } else {
            stored_subagent_history(&app.state.session)
        };
        // Automations stop before anything below is torn down. They hold the
        // workflow runtime they started with, so they follow it as well as the
        // session, and `start_automations` gives the next ones the new handle.
        let workflow_kept = self
            .workflow
            .as_ref()
            .is_some_and(|current| current.session_id() == app.state.session.id);
        let automation = match self.automation.take() {
            Some(current)
                if current.session_id() == app.state.session.id
                    && (workflow_kept || self.workflow.is_none()) =>
            {
                Some(current)
            }
            stale => {
                if let Some(stale) = stale {
                    retire_automations(stale, app);
                }
                None
            }
        };
        let background = if same_session && self.background.is_some() {
            self.background.clone()
        } else {
            if let Some(background) = self.background.take()
                && let Err(error) = smol::block_on(background.shutdown())
            {
                warn!(%error, "background task shutdown failed");
            }
            if !self.background_enabled {
                None
            } else {
                smol::block_on(BackgroundTasks::spawn(
                    app.storage.clone(),
                    app.state.session.id,
                ))
                .map_err(|error| app.flash(error))
                .ok()
            }
        };
        // The runtime follows the session, not the loop: a respawn under the
        // same id keeps every run going, and a loaded or reset session gets
        // its own only once the previous one has stopped and drained.
        let workflow = match self.workflow.take() {
            Some(current) if current.session_id() == app.state.session.id => {
                WorkflowSlot::Reuse(current)
            }
            stale => {
                if let Some(stale) = stale {
                    stale.shutdown();
                }
                WorkflowSlot::Fresh(Some(app.storage.clone()))
            }
        };
        let mut history = history;
        let interrupted =
            (!same_session).then(|| take_interrupted(background.as_ref(), &mut history));
        let archived = crate::archived_session_history(&app.state.session);
        let todos = crate::session_todos(&app.state.session, &archived, &history);
        let mut new = spawn_agent_internal(
            (self.agent_tx.clone(), self.agent_rx.clone()),
            model_slot,
            if same_session {
                Arc::clone(&self.effective_model_slot)
            } else {
                Arc::new(ArcSwap::new(model_slot.load_full()))
            },
            if same_session {
                Arc::clone(&self.execution_mode)
            } else {
                Arc::new(ArcSwap::from_pointee(app.execution_agent_mode()))
            },
            history,
            archived,
            todos,
            config,
            tool_output_lines,
            Some(Arc::new(ToolOutputStore::new(app.storage.clone()))),
            permissions,
            self.mcp_handle.clone(),
            self.mcp_config_errors.clone(),
            Some(SessionRef::from(app.state.session.id)),
            session_lease,
            self.timeouts,
            lua_handle,
            Arc::clone(&self.model_policy),
            app.state.goal.clone(),
            subagent_history,
            app.state.system_prompt_profile.clone(),
            Arc::clone(&self.prompt_profiles),
            workflow,
            background,
            self.background_enabled,
            if same_session {
                Arc::clone(&self.delivery_fence)
            } else {
                Arc::default()
            },
            Arc::clone(&self.path_locks),
            Arc::clone(&app.change_recorder),
            self.workspace_session.clone(),
            self.remote_project_context.clone(),
            self.host_cwd.clone(),
            self.local_documents.clone(),
        );
        new.interrupted = interrupted;
        let old = mem::replace(self, new);
        self.automation = match automation {
            Some(started_without) if !workflow_kept && self.workflow.is_some() => {
                retire_automations(started_without, app);
                None
            }
            kept => kept,
        };
        // Repoint the app at the new queue before dropping `old`, otherwise the app keeps
        // the last old `QueueSender` alive and the old loop parks in `recv_notify` forever.
        if same_session {
            app.execution_mode = Some(Arc::clone(&self.execution_mode));
        }
        self.apply_to_app(app);
        app.refresh_workflow_cards();
        app.flush_restored_queue();
        if same_session {
            let _ = old.cmd_tx.try_send(AgentCommand::Cancel {
                run_id: app.run_id - 1,
            });
        } else {
            old.cancel();
        }
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Hand back the agent task, dropping every channel so the loop can
    /// wind down. The caller sends `CancelAll` first and then awaits all
    /// tabs at once via [`join_all`] instead of paying a serial timeout
    /// per tab.
    pub(crate) fn into_task(self) -> smol::Task<()> {
        self.task
    }
}

/// Wait for every agent task under one shared timeout, not one per task.
pub(crate) fn join_all(tasks: Vec<smol::Task<()>>, timeout: Duration) -> bool {
    info!(
        count = tasks.len(),
        "waiting for agents to finish (timeout {timeout:?})"
    );
    smol::block_on(async {
        let finished = futures_lite::future::or(
            async {
                for task in tasks {
                    task.await;
                }
                true
            },
            async {
                smol::Timer::after(timeout).await;
                false
            },
        )
        .await;
        if !finished {
            warn!("agents did not finish within {timeout:?}, forcing shutdown");
        }
        finished
    })
}

/// Stops a runtime `respawn` will not carry over, so the app cannot reach it
/// if the next one fails to start.
fn retire_automations(automation: AutomationSession, app: &mut App) {
    automation.shutdown();
    app.automation = AutomationLink::default();
}

/// Where a new agent generation gets its workflow runtime from.
enum WorkflowSlot {
    /// Keep the session's runtime; the new loop only gets a fresh handle.
    Reuse(WorkflowSession),
    /// Open one for this session, when there is a state directory to keep
    /// its runs in.
    Fresh(Option<StateDir>),
}

#[allow(clippy::too_many_arguments)]
fn spawn_agent_internal(
    (agent_tx, agent_rx): (flume::Sender<Envelope>, flume::Receiver<Envelope>),
    model_slot: &Arc<ArcSwap<ModelSlot>>,
    effective_model_slot: Arc<ArcSwap<ModelSlot>>,
    execution_mode: SharedMode,
    initial_history: Vec<HistoryItem>,
    archived_history: Vec<HistoryItem>,
    todos: Option<Vec<TodoItem>>,
    config: AgentConfig,
    tool_output_lines: ToolOutputLines,
    tool_output_store: Option<Arc<ToolOutputStore>>,
    permissions: &Arc<PermissionManager>,
    mcp_handle: Option<McpHandle>,
    mcp_config_errors: McpConfigErrors,
    session_id: Option<SessionRef>,
    session_lease: Option<Arc<SessionLease>>,
    timeouts: caudra_providers::Timeouts,
    lua_handle: EventHandle,
    model_policy: Arc<ModelPolicy>,
    goal: caudra_agent::GoalHandle,
    subagent_history: SubagentHistoryStore,
    system_prompt_profile: Option<Arc<SystemPromptProfile>>,
    prompt_profiles: Arc<PromptProfileCatalog>,
    workflow: WorkflowSlot,
    background: Option<BackgroundTasks>,
    background_enabled: bool,
    delivery_fence: Arc<DeliveryFence>,
    path_locks: Arc<PathLocks>,
    change_recorder: RecorderSlot,
    workspace_session: Option<WorkspaceSession>,
    remote_project_context: Option<Arc<caudra_agent::remote_project_context::RemoteProjectContext>>,
    host_cwd: Option<PathBuf>,
    local_documents: Option<Arc<LocalDocumentStore>>,
) -> AgentHandles {
    let (background_wake_tx, background_wake_rx) = flume::bounded(1);
    let background_notifier = background.clone().map(|background| {
        smol::spawn(async move {
            loop {
                let changed = background.listen();
                if background_wake_tx.send_async(()).await.is_err() {
                    break;
                }
                changed.await;
            }
        })
    });
    let (cmd_tx, cmd_rx) = flume::unbounded::<AgentCommand>();
    let (answer_tx, answer_rx) = match &workflow {
        WorkflowSlot::Reuse(current) => current.answer_channel(),
        WorkflowSlot::Fresh(_) => answer_channel(),
    };
    let (queue_tx, queue_rx) = shared_queue::queue();
    let queue_rx = Arc::new(queue_rx);
    // Keep the incoming items visible if restore validation fails. A valid
    // AgentLoop synchronously replaces this with its sanitized snapshot.
    let shared_history: SharedHistory = Arc::new(ArcSwap::from_pointee(HistorySnapshot::new(
        initial_history.clone(),
    )));
    let initial_model = effective_model_slot.load();
    let btw_prompt: SharedBtwPrompt = Arc::new(ArcSwap::from_pointee(BtwPrompt {
        provider: Arc::clone(&initial_model.provider),
        model: initial_model.model.clone(),
        system: String::new(),
        tools: Value::Null,
        opts: RequestOptions::default(),
        profile: None,
    }));
    let context_store = ContextStore::new();
    let context_publisher = context_store.publisher(ContextKey::Main);
    let (init_trigger, init_cancel) = CancelToken::new();
    let cancel_map = Arc::new(new_run_cancel_map(0, init_trigger));
    let subagent_cancels: Arc<CancelMap<String>> = Arc::new(CancelMap::new());
    let control = ConversationControl::default();
    let mailbox = session_id
        .as_ref()
        .map(|session_id| SessionMailbox::register(session_id.id()));
    let prompt_profiles = prompt_profiles.keeping(system_prompt_profile.as_ref());
    let task_prompt_profile_name: Arc<str> = Arc::from(
        system_prompt_profile
            .as_ref()
            .map_or(BUILTIN_PROFILE_NAME, |profile| profile.name()),
    );
    // Before the loop, so its `workflow` tool reaches the runtime from the
    // first turn.
    let workflow = match workflow {
        WorkflowSlot::Reuse(current) => Some(current),
        WorkflowSlot::Fresh(state_dir) => {
            state_dir
                .zip(session_id.as_ref())
                .and_then(|(state_dir, session_id)| {
                    WorkflowSession::spawn(WorkflowSpawn {
                        background: background.clone(),
                        state_dir,
                        session_id: session_id.id(),
                        effective_model_slot: &effective_model_slot,
                        execution_mode: &execution_mode,
                        config: &config,
                        tool_output_lines,
                        permissions,
                        mcp_handle: mcp_handle.as_ref(),
                        timeouts,
                        lua_handle: &lua_handle,
                        model_policy: &model_policy,
                        subagent_history: &subagent_history,
                        prompt_profiles: &prompt_profiles,
                        task_prompt_profile_name: Arc::clone(&task_prompt_profile_name),
                        context_publisher: context_publisher.clone(),
                        answer: (answer_tx.clone(), Arc::clone(&answer_rx)),
                        events: agent_tx.clone(),
                        path_locks: Arc::clone(&path_locks),
                        changes: change_recorder.load_full().as_deref().cloned(),
                        workspace_session: workspace_session.clone(),
                        remote_project_context: remote_project_context.clone(),
                        host_cwd: host_cwd.clone(),
                        local_documents: local_documents.clone(),
                    })
                })
        }
    };
    let settings = LiveSettings::default();
    let agent_loop = AgentLoop::new(
        Arc::clone(model_slot),
        Arc::clone(&effective_model_slot),
        config,
        settings.clone(),
        tool_output_lines,
        tool_output_store,
        initial_history,
        archived_history,
        todos,
        Arc::clone(&shared_history),
        Arc::clone(&btw_prompt),
        context_publisher,
        mcp_handle.clone(),
        Arc::clone(permissions),
        agent_tx.clone(),
        answer_rx,
        queue_rx,
        Arc::clone(&cancel_map),
        control.clone(),
        init_cancel,
        session_id.clone(),
        mailbox.clone(),
        timeouts,
        lua_handle,
        Arc::clone(&subagent_cancels),
        subagent_history.clone(),
        Arc::clone(&model_policy),
        goal.clone(),
        system_prompt_profile,
        Arc::clone(&prompt_profiles),
        workflow.as_ref().map(WorkflowSession::handle),
        background.clone(),
        Arc::clone(&delivery_fence),
        Arc::clone(&execution_mode),
        Arc::clone(&path_locks),
        change_recorder,
        workspace_session.clone(),
        remote_project_context.clone(),
        host_cwd.clone(),
        local_documents.clone(),
    );

    spawn_command_router(
        cmd_rx,
        cancel_map,
        Arc::clone(&subagent_cancels),
        control.retry_now.clone(),
        agent_loop.cache_warmer(),
    );
    let tools_preview_source = Arc::new(agent_loop.tools_preview_source());
    let learner = agent_loop.learner_handle();
    let task_host = agent_loop
        .task_host()
        .map(Arc::new)
        .map_err(|error| warn!(%error, "task continuation host unavailable"))
        .ok();
    let task = smol::spawn(async move {
        let _session_lease = session_lease;
        agent_loop.run().await;
    });

    AgentHandles {
        cmd_tx,
        agent_rx,
        agent_tx,
        answer_tx,
        history: shared_history,
        btw_prompt,
        context_store,
        tools_preview_source,
        effective_model_slot,
        control,
        execution_mode,
        mcp_handle,
        mcp_config_errors,
        queue: queue_tx,
        goal,
        subagent_cancels,
        timeouts,
        model_policy,
        prompt_profiles,
        mailbox,
        workflow,
        automation: None,
        background,
        interrupted: None,
        background_enabled,
        task_host,
        learner,
        delivery_fence,
        session_id: session_id.map(|session| session.id()),
        subagent_history,
        background_wake_rx,
        _background_notifier: background_notifier,
        path_locks,
        workspace_session,
        remote_project_context,
        host_cwd,
        local_documents,
        settings,
        task,
    }
}

/// What the previous process left cut off for a session this loop opens,
/// with the open calls ending `history` answered for what became of them.
fn take_interrupted(
    background: Option<&BackgroundTasks>,
    history: &mut Vec<HistoryItem>,
) -> (RestoredWork, Vec<DanglingCall>) {
    let work = background
        .map(BackgroundTasks::take_restored)
        .unwrap_or_default();
    let dangling = restart::close_interrupted_items(history, &work);
    (work, dangling)
}

pub(crate) fn stored_subagent_history(session: &crate::AppSession) -> SubagentHistoryStore {
    let active_history = crate::active_session_history(session).unwrap_or_else(|error| {
        warn!(%error, "failed to resolve active history for subagent restoration");
        Vec::new()
    });
    let mut versions =
        caudra_agent::active_task_history_versions_with_outputs(&active_history, |call_id| {
            session.tool_outputs().get(call_id).map(AsRef::as_ref)
        });
    let mut reachable = crate::app::reachable_subagent_ids(
        &active_history,
        session.subagent_messages(),
        session.tool_outputs(),
        session.subagents(),
    );
    reachable.extend(
        versions
            .iter()
            .filter(|(_, version_id)| session.subagent_messages().contains_key(*version_id))
            .map(|(task_id, _)| task_id.clone()),
    );
    let legacy_fallback = reachable.is_empty()
        || active_history.iter().any(|item| {
            matches!(
                &item.kind,
                caudra_providers::HistoryItemKind::AssistantText {
                    retained_subagent_ids,
                    is_compaction_summary: true,
                    ..
                } if retained_subagent_ids.is_empty()
            )
        });
    let mut active_calls = caudra_agent::history_tool_call_ids(&active_history);
    for task_id in &reachable {
        if let Some(history) = session.subagent_messages().get(task_id) {
            active_calls.extend(caudra_agent::history_tool_call_ids(history));
        }
    }
    reachable.extend(
        session
            .subagent_task_specs()
            .iter()
            .filter(|(task_id, spec)| {
                if !spec.is_generic() {
                    return false;
                }
                let descriptor = session
                    .subagents()
                    .iter()
                    .find(|subagent| subagent.tool_use_id == **task_id);
                descriptor.is_none()
                    || active_calls.contains(*task_id)
                    || descriptor
                        .and_then(|subagent| subagent.root_tool_use_id.as_ref())
                        .is_some_and(|root| active_calls.contains(root))
            })
            .map(|(task_id, _)| task_id.clone()),
    );
    if legacy_fallback {
        reachable.extend(
            session
                .subagent_messages()
                .keys()
                .filter(|task_id| !session.subagent_task_specs().contains_key(*task_id))
                .filter(|task_id| {
                    !session.subagents().iter().any(|subagent| {
                        subagent.tool_use_id.as_str() != task_id.as_str()
                            && subagent.parent_tool_use_id.as_ref() == Some(*task_id)
                    })
                })
                .cloned(),
        );
    }
    for subagent in session.subagents() {
        if reachable.contains(&subagent.tool_use_id)
            && !versions.contains_key(&subagent.tool_use_id)
            && let Some(version_id) = &subagent.parent_tool_use_id
        {
            versions.insert(subagent.tool_use_id.clone(), version_id.clone());
        }
    }
    let version_ids: HashSet<&str> = versions
        .iter()
        .filter_map(|(task_id, version_id)| {
            (task_id != version_id && session.subagent_messages().contains_key(version_id))
                .then_some(version_id.as_str())
        })
        .collect();
    let histories = reachable
        .iter()
        .filter(|task_id| !version_ids.contains(task_id.as_str()))
        .filter_map(|task_id| {
            let version_id = versions.get(task_id).unwrap_or(task_id);
            let items = session.subagent_messages().get(version_id)?;
            match project_messages(items) {
                Ok(messages) => Some((task_id.clone(), Arc::new(messages))),
                Err(error) => {
                    warn!(%error, %task_id, "failed to restore subagent history");
                    None
                }
            }
        })
        .collect();
    let specs = session
        .subagent_task_specs()
        .iter()
        .filter(|(task_id, spec)| reachable.contains(*task_id) && !spec.is_version())
        .map(|(task_id, spec)| (task_id.clone(), spec.clone()))
        .collect();
    SubagentHistoryStore::seeded_with_versions(histories, specs, versions)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::Instant;

    use caudra_agent::automation::manager::RuntimeDeps;
    use caudra_agent::automation::store::AutomationStore;
    use caudra_agent::automation::testing::AutomationFixture;
    use caudra_agent::automation::workflows::NO_WORKFLOWS;
    use caudra_agent::prompt::profile::{PROFILE_DIR, PROFILE_EXTENSION};
    use caudra_agent::prompt::{
        CONFIGURATION_CHANGED_MARKER, CONFIGURATION_RESTORED_PROMPT, ENVIRONMENT_MARKER,
        INSTRUCTIONS_CHANGED_MARKER, INSTRUCTIONS_RESTORED_PROMPT, MEMORY_UPDATED_MARKER,
    };
    use caudra_agent::tools::TODOWRITE_TOOL_NAME;
    use caudra_agent::{AgentEvent, AgentInput, PromptAdmission};
    use caudra_automation::host::{Failure, FailureKind};
    use caudra_automation::request::{AutomationRequest, AutomationResponse};
    use caudra_automation::snapshot::{ActionRow, ActionStatus, AutomationState};
    use caudra_config::{Feature, FeatureFlags, PermissionsConfig};
    use caudra_providers::provider::BoxFuture;
    use caudra_providers::{
        AgentError, CacheKey, ContentBlock, ModelInfo, ProviderEvent, RequestOptions, Role,
        StopReason, StreamResponse,
    };
    use caudra_storage::sessions::PermissionMode;
    use caudra_workflow::{LaunchRequest, WorkflowRequest, WorkflowResponse};
    use caudra_workspace::PlanRef;
    use tempfile::TempDir;
    use test_case::test_case;

    use crate::app::{Mode, PlanState};

    use super::shared_queue::QueueItem;

    use super::*;

    const LONG_TIMEOUT: Duration = Duration::from_secs(60);
    const SHORT_TIMEOUT: Duration = Duration::from_millis(50);
    const PROBE_TEXT: &str = "probe-through-old-sender";
    const RESTORED_TEXT: &str = "restored-queued-message";
    const RESUMED_HISTORY_TEXT: &str = "resumed-conversation";
    const PLAN_PATH: &str = ".caudra/plans/runtime-mode.md";
    const PLAN_REF: &str = "plan-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const COMMITTED_MODEL: &str = "committed-model";
    const SELECTED_MODEL: &str = "pending-model";
    const ROUTE_REQUEST_MISSING: &str = "runtime did not request the expected model route";
    const REVIEW_WORKFLOW: &str = "review-changes";
    const WORKFLOW_BUDGET: u32 = 1;
    const STARTER: &str = "starter";
    const STARTER_FIELDS: &str = r#"triggers: [#{ kind: "armed" }], workflows: ["review-changes"]"#;
    const START_BODY: &str =
        r#"start_workflow("review-changes", #{ scope: "main" }, #{ agent_budget: 1 });"#;
    const WATCHER: &str = "watcher";
    const WATCHER_FIELDS: &str = r#"triggers: [#{ kind: "workflow_finished", workflows: ["review-changes"], statuses: ["interrupted"] }]"#;
    const WATCHER_BODY: &str = r#"notify("interrupted");"#;
    const FIRING_LIMIT: usize = 16;
    const AUTOMATIONS_ON: &str = "the app enables automations";
    const AUTOMATIONS_STARTED: &str = "the automation runtime started";
    const AUTOMATION_EVENTS_STOPPED: &str = "the automation runtime stopped publishing";
    const ONE_ACTION: &str = "the firing made one call";
    const RUN_REACHED: &str = "the start reached the session's workflow runtime";
    const NO_FIRING_FOR_TEARDOWN: &str =
        "a run interrupted by the shutdown must not reach automations that are going away";
    const ANSWER_TEXT: &str = "answered";
    const NO_REQUEST: &str = "the turn never reached the provider";
    /// The run a loop admits until a respawn moves it on.
    const FIRST_RUN: u64 = 0;
    const PROFILE_NAME: &str = "review";
    const LOADED_PROFILE: &str = "Review carefully.";
    const EDITED_PROFILE: &str = "Review every line.";
    /// Frontmatter that is never closed.
    const BROKEN_PROFILE: &str = "---\nlayout: custom\nReview every line.";
    const STALE_PROFILE: &str = "a respawned agent must build from the profile as last reloaded";
    const PROFILE_DROPPED: &str = "a session must keep the profile it loaded once the file is lost";
    const FROZEN_PROMPT: &str = "an edited profile must leave the cached prompt as it was";
    const EDIT_UNNOTED: &str = "the bound prompt must say which profile built it";
    const STALE_COMPACTION_CONTEXT: &str = "stale-compaction-context";
    const COMPACTION_FINISHED: &str = "manual compaction must finish before the next turn";

    struct StubProvider(Option<flume::Sender<String>>);

    impl Provider for StubProvider {
        fn stream_message<'a>(
            &'a self,
            model: &'a Model,
            _messages: &'a [Message],
            _system: &'a str,
            _tools: &'a serde_json::Value,
            _event_tx: &'a flume::Sender<ProviderEvent>,
            _opts: RequestOptions,
            _cache_key: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                if let Some(requests) = &self.0 {
                    requests
                        .send(model.id.clone())
                        .map_err(|_| AgentError::Channel)?;
                }
                future::pending().await
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn stub_spawn() -> (
        AgentHandles,
        Arc<ArcSwap<ModelSlot>>,
        Arc<PermissionManager>,
    ) {
        stub_spawn_with(Vec::new())
    }

    fn stub_spawn_with(
        initial_history: Vec<Message>,
    ) -> (
        AgentHandles,
        Arc<ArcSwap<ModelSlot>>,
        Arc<PermissionManager>,
    ) {
        stub_spawn_with_session(initial_history, None, None, None)
    }

    fn stub_spawn_with_session(
        initial_history: Vec<Message>,
        session_id: Option<SessionRef>,
        session_lease: Option<Arc<SessionLease>>,
        state_dir: Option<StateDir>,
    ) -> (
        AgentHandles,
        Arc<ArcSwap<ModelSlot>>,
        Arc<PermissionManager>,
    ) {
        spawn_with_provider(
            Arc::new(StubProvider(None)),
            initial_history,
            session_id,
            session_lease,
            state_dir,
        )
    }

    fn spawn_with_provider(
        provider: Arc<dyn Provider>,
        initial_history: Vec<Message>,
        session_id: Option<SessionRef>,
        session_lease: Option<Arc<SessionLease>>,
        state_dir: Option<StateDir>,
    ) -> (
        AgentHandles,
        Arc<ArcSwap<ModelSlot>>,
        Arc<PermissionManager>,
    ) {
        let model_slot = Arc::new(ArcSwap::from_pointee(ModelSlot {
            model: crate::components::test_model(),
            provider,
            home: None,
        }));
        let permissions = Arc::new(PermissionManager::new_nonpersistent(
            PermissionsConfig::default(),
            PathBuf::from("/tmp"),
            Arc::default(),
        ));
        let handles = AgentHandles::spawn(
            &model_slot,
            crate::history_items(&initial_history),
            Vec::new(),
            None,
            AgentConfig {
                features: if state_dir.is_some() {
                    FeatureFlags::NONE.with(Feature::Workflows)
                } else {
                    FeatureFlags::NONE
                },
                summarize_memory: false,
                ..Default::default()
            },
            ToolOutputLines::default(),
            &permissions,
            session_id,
            session_lease,
            caudra_providers::Timeouts::default(),
            EventHandle::disconnected_for_test(),
            None,
            McpConfigErrors::new(PathBuf::new()),
            Arc::new(ModelPolicy::default()),
            caudra_agent::GoalHandle::default(),
            SubagentHistoryStore::default(),
            None,
            Arc::new(PromptProfileCatalog::default()),
            state_dir,
            RecorderSlot::default(),
            None,
            None,
            None,
            None,
            false,
        );
        (handles, model_slot, permissions)
    }

    /// The cached prefix of one request, and the conversation that follows it.
    struct Request {
        tools: String,
        system: String,
        conversation: String,
    }

    /// Answers every request at once and reports what it was sent.
    struct PrefixRecorder(flume::Sender<Request>);

    impl Provider for PrefixRecorder {
        fn stream_message<'a>(
            &'a self,
            _model: &'a Model,
            messages: &'a [Message],
            system: &'a str,
            tools: &'a serde_json::Value,
            _event_tx: &'a flume::Sender<ProviderEvent>,
            _opts: RequestOptions,
            _cache_key: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                let conversation = messages
                    .iter()
                    .flat_map(|message| &message.content)
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                self.0
                    .send(Request {
                        tools: tools.to_string(),
                        system: system.to_owned(),
                        conversation,
                    })
                    .map_err(|_| AgentError::Channel)?;
                Ok(StreamResponse {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::Text {
                            text: ANSWER_TEXT.into(),
                        }],
                        ..Default::default()
                    },
                    stop_reason: Some(StopReason::EndTurn),
                    ..Default::default()
                })
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    /// Queues a prompt for the loop started at `run_id`, which a respawn moves
    /// on, and returns the request it sent.
    fn ask(handles: &AgentHandles, received: &flume::Receiver<Request>, run_id: u64) -> Request {
        handles.queue.push(QueueItem::Message {
            text: PROBE_TEXT.into(),
            image_count: 0,
            paste_ranges: Vec::new(),
            input: Box::new(AgentInput {
                message: PROBE_TEXT.into(),
                mode: AgentMode::Build,
                plan: None,
                images: Vec::new(),
                mentions: Vec::new(),
                commits: Vec::new(),
                preamble: Vec::new(),
                thinking: Default::default(),
                fast: false,
                prompt: None,
                resume: false,
            }),
            run_id,
            admission: PromptAdmission::Queue,
            displayed: true,
        });
        received.recv_timeout(LONG_TIMEOUT).expect(NO_REQUEST)
    }

    fn tool_names(tools: &str) -> Vec<String> {
        let tools: Vec<Value> = serde_json::from_str(tools).unwrap();
        tools
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
            .collect()
    }

    fn finished_events(handles: &AgentHandles) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        loop {
            let event = handles
                .agent_rx
                .recv_timeout(LONG_TIMEOUT)
                .expect(COMPACTION_FINISHED)
                .event;
            assert!(!matches!(&event, AgentEvent::Error { .. }), "{event:?}");
            let done = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if done {
                return events;
            }
        }
    }

    #[test_case(false; "summarized_reminders")]
    #[test_case(true; "retained_reminders")]
    fn manual_compaction_refreshes_standing_context_without_an_extra_turn(retained: bool) {
        crate::components::register_native_tools();
        let stale: Vec<_> = [
            ENVIRONMENT_MARKER,
            INSTRUCTIONS_CHANGED_MARKER,
            MEMORY_UPDATED_MARKER,
            CONFIGURATION_CHANGED_MARKER,
        ]
        .into_iter()
        .map(|marker| Message::observation(format!("{marker}\n{STALE_COMPACTION_CONTEXT}")))
        .collect();
        let mut initial = vec![Message::user(RESUMED_HISTORY_TEXT.into())];
        if retained {
            initial.push(Message::user(PROBE_TEXT.into()));
        }
        initial.extend(stale);
        let (requests, received) = flume::unbounded();
        let (mut handles, _, _) = spawn_with_provider(
            Arc::new(PrefixRecorder(requests)),
            initial,
            None,
            None,
            None,
        );
        handles.publish_settings(Settings {
            config: AgentConfig {
                disabled_tools: [TODOWRITE_TOOL_NAME.to_owned()].into_iter().collect(),
                ..turns_only()
            },
            prompt_profiles: Arc::default(),
            mcp: None,
        });
        handles.queue.push(QueueItem::Compact { run_id: FIRST_RUN });
        let events = finished_events(&handles);
        received.recv_timeout(LONG_TIMEOUT).expect(NO_REQUEST);
        assert!(received.is_empty());
        let injected: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Injected { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            injected
                .iter()
                .all(|text| !text.contains(STALE_COMPACTION_CONTEXT))
        );
        for notice in [
            INSTRUCTIONS_RESTORED_PROMPT,
            caudra_agent::prompt::MEMORY_UNAVAILABLE_PROMPT,
            CONFIGURATION_RESTORED_PROMPT,
        ] {
            assert!(injected.contains(&notice));
        }
        let environment = injected
            .iter()
            .find(|text| text.contains(ENVIRONMENT_MARKER))
            .unwrap();
        assert!(environment.contains(&crate::components::test_model().spec()));
        assert!(environment.contains(caudra_agent::template::env_vars().apply("{date}").as_ref()));
        let before = handles.btw_prompt.load_full();
        assert!(!tool_names(&before.tools.to_string()).contains(&TODOWRITE_TOOL_NAME.to_owned()));
        assert!(handles.context_store.latest(&ContextKey::Main).is_some());

        let next = ask(&handles, &received, FIRST_RUN);
        let next_events = finished_events(&handles);
        assert_eq!(next.system, before.system);
        assert_eq!(next.tools, before.tools.to_string());
        assert!(next_events.iter().all(|event| !matches!(event,
            AgentEvent::Injected { text, .. }
                if [ENVIRONMENT_MARKER, INSTRUCTIONS_CHANGED_MARKER, MEMORY_UPDATED_MARKER, CONFIGURATION_CHANGED_MARKER]
                    .iter().any(|marker| text.contains(marker))
        )));
        assert!(received.is_empty());
        smol::block_on(handles.into_task().cancel());
    }

    #[test]
    fn a_configuration_change_keeps_the_cached_prefix_until_a_rebuild() {
        crate::components::register_native_tools();
        let (requests, received) = flume::unbounded();
        let (mut handles, model_slot, _permissions) = spawn_with_provider(
            Arc::new(PrefixRecorder(requests)),
            Vec::new(),
            None,
            None,
            None,
        );
        let first = ask(&handles, &received, FIRST_RUN);
        assert!(tool_names(&first.tools).contains(&TODOWRITE_TOOL_NAME.to_owned()));

        handles.publish_settings(Settings {
            config: AgentConfig {
                disabled_tools: [TODOWRITE_TOOL_NAME.to_owned()].into_iter().collect(),
                summarize_memory: false,
                ..Default::default()
            },
            prompt_profiles: Arc::default(),
            mcp: None,
        });
        let kept = ask(&handles, &received, FIRST_RUN);

        assert_eq!(kept.tools, first.tools);
        assert_eq!(kept.system, first.system);
        assert!(kept.conversation.contains(CONFIGURATION_CHANGED_MARKER));

        let mut switched = crate::components::test_model();
        switched.id = SELECTED_MODEL.into();
        let provider = Arc::clone(&model_slot.load().provider);
        model_slot.store(Arc::new(ModelSlot {
            model: switched,
            provider,
            home: None,
        }));
        let rebuilt = ask(&handles, &received, FIRST_RUN);

        assert!(!tool_names(&rebuilt.tools).contains(&TODOWRITE_TOOL_NAME.to_owned()));
        assert!(
            rebuilt
                .conversation
                .contains(CONFIGURATION_RESTORED_PROMPT.trim())
        );
        smol::block_on(handles.into_task().cancel());
    }

    /// Saves `saved` as the `review` profile, or deletes it when `None`, and
    /// reads the profiles again.
    fn reread_profiles(dir: &TempDir, saved: Option<&str>) -> PromptProfileCatalog {
        let profiles = dir.path().join(PROFILE_DIR);
        let file = profiles.join(format!("{PROFILE_NAME}.{PROFILE_EXTENSION}"));
        match saved {
            Some(text) => {
                fs::create_dir_all(&profiles).unwrap();
                fs::write(&file, text).unwrap();
            }
            None => fs::remove_file(&file).unwrap(),
        }
        PromptProfileCatalog::discover_with(Some(dir.path()), false)
    }

    /// What the event loop does once the profiles are read again: every tab
    /// takes them, and every agent gets them with the settings.
    fn reload_profiles(handles: &mut AgentHandles, app: &mut App, catalog: PromptProfileCatalog) {
        let catalog = Arc::new(catalog);
        app.adopt_prompt_profiles(&catalog);
        handles.publish_settings(Settings {
            config: turns_only(),
            prompt_profiles: catalog,
            mcp: None,
        });
    }

    /// A tab whose agent runs the `review` profile as first saved, and records
    /// each request it sends.
    fn running_loaded_profile(
        dir: &TempDir,
    ) -> (
        AgentHandles,
        Arc<ArcSwap<ModelSlot>>,
        Arc<PermissionManager>,
        App,
        flume::Receiver<Request>,
    ) {
        crate::components::register_native_tools();
        let (requests, received) = flume::unbounded();
        let (mut handles, model_slot, permissions) = spawn_with_provider(
            Arc::new(PrefixRecorder(requests)),
            Vec::new(),
            None,
            None,
            None,
        );
        let mut app = crate::app::tests::test_app();
        let loaded = reread_profiles(dir, Some(LOADED_PROFILE));
        app.state.system_prompt_profile = loaded.get(PROFILE_NAME);
        reload_profiles(&mut handles, &mut app, loaded);
        respawn(&mut handles, &model_slot, &permissions, &mut app);
        (handles, model_slot, permissions, app, received)
    }

    #[test]
    fn manual_compaction_rebuilds_the_profile_for_the_effective_conversation_model() {
        let dir = TempDir::new().unwrap();
        let (mut handles, model_slot, _, mut app, received) = running_loaded_profile(&dir);
        let first = ask(&handles, &received, app.run_id);
        finished_events(&handles);
        assert!(first.system.contains(LOADED_PROFILE));
        reload_profiles(
            &mut handles,
            &mut app,
            reread_profiles(&dir, Some(EDITED_PROFILE)),
        );
        let selected = model_slot.load_full();
        let mut summary_model = selected.model.clone();
        summary_model.id = SELECTED_MODEL.into();
        model_slot.store(Arc::new(ModelSlot {
            model: summary_model,
            provider: Arc::clone(&selected.provider),
            home: None,
        }));
        handles
            .queue
            .push(QueueItem::Compact { run_id: app.run_id });
        let events = finished_events(&handles);
        received.recv_timeout(LONG_TIMEOUT).expect(NO_REQUEST);
        let prompt = handles.btw_prompt.load_full();
        assert!(prompt.system.contains(EDITED_PROFILE));
        assert!(!prompt.system.contains(LOADED_PROFILE));
        assert!(!prompt.outdated_by(app.state.system_prompt_profile.as_deref()));
        assert_eq!(prompt.model.spec(), selected.model.spec());
        assert!(events.iter().all(|event| !matches!(event,
            AgentEvent::Injected { text, .. }
                if text.contains(ENVIRONMENT_MARKER) && text.contains(SELECTED_MODEL)
        )));
        model_slot.store(selected);
        let next = ask(&handles, &received, app.run_id);
        finished_events(&handles);
        assert_eq!(next.system, prompt.system);
        assert_eq!(next.tools, prompt.tools.to_string());
        smol::block_on(handles.into_task().cancel());
    }

    /// `/new` and `/cd` respawn the agent from the profile the tab holds, and a
    /// respawn drops the settings the old agent had yet to adopt.
    #[test]
    fn a_respawned_agent_builds_from_the_profile_as_last_reloaded() {
        let dir = TempDir::new().unwrap();
        let (mut handles, model_slot, permissions, mut app, received) =
            running_loaded_profile(&dir);
        let loaded = ask(&handles, &received, app.run_id);
        assert!(loaded.system.contains(LOADED_PROFILE));

        reload_profiles(
            &mut handles,
            &mut app,
            reread_profiles(&dir, Some(EDITED_PROFILE)),
        );
        respawn(&mut handles, &model_slot, &permissions, &mut app);

        let respawned = ask(&handles, &received, app.run_id);
        assert!(respawned.system.contains(EDITED_PROFILE), "{STALE_PROFILE}");
        assert!(
            !respawned.system.contains(LOADED_PROFILE),
            "{STALE_PROFILE}"
        );
        smol::block_on(handles.into_task().cancel());
    }

    /// Each turn checks that the session's profile resolves, so profiles read
    /// again without it must stop neither the session nor the agent `/new`
    /// respawns.
    #[test_case(None ; "deleted")]
    #[test_case(Some(BROKEN_PROFILE) ; "broken")]
    fn a_session_runs_on_the_profile_it_loaded_once_the_file_is_lost(saved: Option<&str>) {
        let dir = TempDir::new().unwrap();
        let (mut handles, model_slot, permissions, mut app, received) =
            running_loaded_profile(&dir);
        ask(&handles, &received, app.run_id);

        reload_profiles(&mut handles, &mut app, reread_profiles(&dir, saved));
        let adopted = ask(&handles, &received, app.run_id);
        respawn(&mut handles, &model_slot, &permissions, &mut app);
        let respawned = ask(&handles, &received, app.run_id);

        assert!(adopted.system.contains(LOADED_PROFILE), "{PROFILE_DROPPED}");
        assert!(
            respawned.system.contains(LOADED_PROFILE),
            "{PROFILE_DROPPED}"
        );
        smol::block_on(handles.into_task().cancel());
    }

    /// `/system-prompt` notes an edit by weighing the profile the bound prompt
    /// was built from against the tab's, so the prompt keeps naming the old
    /// profile for as long as it carries the old text.
    #[test]
    fn a_bound_prompt_names_its_profile_until_a_rebuild() {
        let dir = TempDir::new().unwrap();
        let (mut handles, model_slot, _permissions, mut app, received) =
            running_loaded_profile(&dir);
        ask(&handles, &received, app.run_id);
        reload_profiles(
            &mut handles,
            &mut app,
            reread_profiles(&dir, Some(EDITED_PROFILE)),
        );
        let outdated = || {
            handles
                .btw_prompt
                .load()
                .outdated_by(app.state.system_prompt_profile.as_deref())
        };

        let kept = ask(&handles, &received, app.run_id);
        assert!(kept.system.contains(LOADED_PROFILE), "{FROZEN_PROMPT}");
        assert!(kept.conversation.contains(CONFIGURATION_CHANGED_MARKER));
        assert!(outdated(), "{EDIT_UNNOTED}");

        let mut switched = crate::components::test_model();
        switched.id = SELECTED_MODEL.into();
        let provider = Arc::clone(&model_slot.load().provider);
        model_slot.store(Arc::new(ModelSlot {
            model: switched,
            provider,
            home: None,
        }));
        let rebuilt = ask(&handles, &received, app.run_id);

        assert!(rebuilt.system.contains(EDITED_PROFILE));
        assert!(!outdated(), "{EDIT_UNNOTED}");
        smol::block_on(handles.into_task().cancel());
    }

    #[test]
    fn agent_task_holds_the_session_lease_until_termination() {
        let temp = tempfile::TempDir::new().unwrap();
        let state_dir = caudra_storage::StateDir::from_path(temp.path().to_path_buf());
        let id = caudra_storage::id::CaudraId::generate();
        let lease = Arc::new(SessionLease::acquire(&state_dir, id).unwrap());
        let (handles, _, _) = stub_spawn_with_session(
            Vec::new(),
            Some(SessionRef::from(id)),
            Some(Arc::clone(&lease)),
            None,
        );
        drop(lease);

        assert!(matches!(
            SessionLease::acquire(&state_dir, id),
            Err(caudra_storage::sessions::SessionError::SessionInUse { .. })
        ));

        smol::block_on(handles.into_task().cancel());
        assert!(SessionLease::acquire(&state_dir, id).is_ok());
    }

    /// Settings under which an agent sends its provider nothing but its turns,
    /// where a session's opening prompt would also ask for a title.
    fn turns_only() -> AgentConfig {
        AgentConfig {
            features: FeatureFlags::NONE,
            summarize_memory: false,
            compaction_requirements: false,
            generate_titles: false,
            ..Default::default()
        }
    }

    fn respawn(
        handles: &mut AgentHandles,
        model_slot: &Arc<ArcSwap<ModelSlot>>,
        permissions: &Arc<PermissionManager>,
        app: &mut App,
    ) {
        handles.respawn(
            Vec::new(),
            model_slot,
            turns_only(),
            ToolOutputLines::default(),
            permissions,
            app,
            EventHandle::disconnected_for_test(),
            None,
        );
    }

    /// Senders captured before any respawn (Lua restore replies, clicks) must
    /// still reach the live receiver, and restored queue items must land in
    /// the freshly wired queue, not the one that just died.
    #[test]
    fn respawn_twice_keeps_channel_and_delivers_restored_queue() {
        let (mut handles, model_slot, permissions) = stub_spawn();
        let pre_gen1_sender =
            caudra_agent::EventSender::new(handles.agent_tx.clone(), crate::app::RESTORE_RUN_ID);

        let mut app = crate::app::tests::test_app();
        let run_id_before = app.run_id;
        respawn(&mut handles, &model_slot, &permissions, &mut app);
        assert_eq!(app.run_id, run_id_before + 1);

        app.state.session_mut().meta.queued_messages =
            vec![caudra_storage::sessions::StoredQueuedPrompt {
                text: RESTORED_TEXT.into(),
                mode: None,
                images: Vec::new(),
                paste_ranges: Vec::new(),
            }];
        respawn(&mut handles, &model_slot, &permissions, &mut app);
        assert_eq!(
            app.run_id,
            run_id_before + 2,
            "each respawn must bump run_id exactly once"
        );

        pre_gen1_sender
            .send(AgentEvent::TextDelta {
                text: PROBE_TEXT.into(),
            })
            .expect("pre-generation-1 sender must still deliver after two respawns");

        // The respawned loop claims the restored item as soon as the flush
        // wakes it, so reading the queue here races that claim. Count what
        // the loop reports instead: `claim_idle` removes an item before
        // `QueueItemConsumed` announces it, so the queue has settled by the
        // time the event lands and a second copy would still be sitting in
        // it.
        let mut probe_seen = false;
        let mut consumed = Vec::new();
        while !(probe_seen && !consumed.is_empty()) {
            let envelope = handles
                .agent_rx
                .recv_timeout(LONG_TIMEOUT)
                .expect("probe or restored queue item never reached the tab channel");
            match envelope.event {
                AgentEvent::TextDelta { ref text } if text == PROBE_TEXT => probe_seen = true,
                AgentEvent::QueueItemConsumed { text, .. } => {
                    assert_eq!(envelope.run_id, app.run_id);
                    consumed.push(text);
                }
                _ => {}
            }
        }
        assert_eq!(
            consumed,
            [RESTORED_TEXT],
            "the restored item is delivered exactly once"
        );
        assert!(
            app.queue.text_messages().is_empty(),
            "no duplicate of the restored item is left queued"
        );
    }

    /// The loop being replaced may still be finishing a write when the new one
    /// starts its own, so both have to queue on the same locks.
    #[test]
    fn respawn_keeps_the_lock_domain() {
        let (mut handles, model_slot, permissions) = stub_spawn();
        let before = Arc::clone(&handles.path_locks);
        let mut app = crate::app::tests::test_app();
        respawn(&mut handles, &model_slot, &permissions, &mut app);
        assert!(Arc::ptr_eq(&handles.path_locks, &before));
    }

    #[test_case(AgentMode::Build; "build")]
    #[test_case(AgentMode::ReadOnly; "read_only")]
    #[test_case(AgentMode::Plan(PLAN_PATH.into()); "plan")]
    fn task_host_without_workflows_tracks_live_session_and_respawn(mode: AgentMode) {
        let (mut handles, model_slot, permissions) = stub_spawn();
        let mut app = crate::app::tests::test_app();
        handles.apply_to_app(&mut app);
        let host = Arc::clone(handles.task_host.as_ref().unwrap());
        assert!(handles.workflow_handle().is_none());
        assert!(Arc::ptr_eq(app.task_host.as_ref().unwrap(), &host));
        assert!(Arc::ptr_eq(&host.permissions, &permissions));
        assert!(Arc::ptr_eq(&host.path_locks, &handles.path_locks));
        assert!(Arc::ptr_eq(
            &host.subagent_cancels,
            &handles.subagent_cancels
        ));

        handles.execution_mode.store(Arc::new(mode.clone()));
        let selected = model_slot.load_full();
        let mut model = selected.model.clone();
        model.id = COMMITTED_MODEL.into();
        handles.effective_model_slot.store(Arc::new(ModelSlot {
            model,
            provider: Arc::clone(&selected.provider),
            home: None,
        }));
        assert_eq!((host.mode)(), mode);
        assert_eq!((host.model)().1.id, COMMITTED_MODEL);

        respawn(&mut handles, &model_slot, &permissions, &mut app);
        let replacement = handles.task_host.as_ref().unwrap();
        assert!(!Arc::ptr_eq(replacement, &host));
        assert!(Arc::ptr_eq(app.task_host.as_ref().unwrap(), replacement));
        assert!(Arc::ptr_eq(&replacement.path_locks, &host.path_locks));
        assert!(Arc::ptr_eq(
            &replacement.subagent_cancels,
            &handles.subagent_cancels
        ));
        assert!(!Arc::ptr_eq(
            &replacement.subagent_cancels,
            &host.subagent_cancels
        ));
        assert_eq!(
            replacement.session_id.as_ref().unwrap().id(),
            app.state.session.id
        );
    }

    #[test_case(false; "build_with_pending_plan")]
    #[test_case(true; "plan_with_pending_build")]
    fn first_binding_seeds_execution_mode_and_rebinding_preserves_live_mode(planning: bool) {
        let (handles, _, _) = stub_spawn();
        let mut app = crate::app::tests::test_app();
        app.state.mode = if planning { Mode::Build } else { Mode::Plan };
        app.state.applied_mode = if planning { Mode::Plan } else { Mode::Build };
        app.state.plan = PlanState::Drafting(PLAN_PATH.into());
        let restored_mode = if planning {
            AgentMode::Plan(PLAN_PATH.into())
        } else {
            AgentMode::Build
        };

        handles.apply_to_app(&mut app);
        assert_eq!(**handles.execution_mode.load(), restored_mode);
        let admitted_mode = if planning {
            AgentMode::Build
        } else {
            AgentMode::Plan(PLAN_PATH.into())
        };
        handles
            .execution_mode
            .store(Arc::new(admitted_mode.clone()));
        handles.apply_to_app(&mut app);
        assert_eq!(app.execution_agent_mode(), admitted_mode);
        assert!(Arc::ptr_eq(
            app.execution_mode.as_ref().unwrap(),
            &handles.execution_mode
        ));
    }

    #[test_case(AgentMode::Build; "build")]
    #[test_case(AgentMode::ReadOnly; "read_only")]
    #[test_case(AgentMode::Plan(PLAN_PATH.into()); "local_plan")]
    #[test_case(AgentMode::RemotePlan(PlanRef::new(PLAN_REF).unwrap()); "remote_plan")]
    fn respawn_without_workflows_keeps_execution_mode_and_route(mode: AgentMode) {
        let mut app = crate::app::tests::test_app();
        let (mut handles, model_slot, permissions) = stub_spawn_with_session(
            Vec::new(),
            Some(SessionRef::from(app.state.session.id)),
            None,
            None,
        );
        handles.apply_to_app(&mut app);
        handles.execution_mode.store(Arc::new(mode.clone()));
        let execution_mode = Arc::clone(&handles.execution_mode);
        let route = Arc::clone(&handles.effective_model_slot);
        let committed = route.load_full();
        let mut selected_model = committed.model.clone();
        selected_model.id = SELECTED_MODEL.into();
        model_slot.store(Arc::new(ModelSlot {
            model: selected_model,
            provider: Arc::clone(&committed.provider),
            home: None,
        }));
        app.state.mode = if mode.is_planning() {
            Mode::Build
        } else {
            Mode::Plan
        };
        app.state.applied_mode = app.state.mode;
        app.state.plan = PlanState::Drafting(PLAN_PATH.into());
        app.execution_mode = None;

        respawn(&mut handles, &model_slot, &permissions, &mut app);

        assert!(handles.workflow_handle().is_none());
        assert!(Arc::ptr_eq(&handles.execution_mode, &execution_mode));
        assert!(Arc::ptr_eq(&handles.effective_model_slot, &route));
        assert!(Arc::ptr_eq(
            &handles.effective_model_slot.load_full(),
            &committed
        ));
        assert_eq!(app.execution_agent_mode(), mode);
    }

    #[test_case(false; "build_session")]
    #[test_case(true; "plan_session")]
    fn new_session_replaces_execution_mode_and_route(planning: bool) {
        let (mut handles, model_slot, permissions) = stub_spawn();
        let old_mode = Arc::clone(&handles.execution_mode);
        let old_route = Arc::clone(&handles.effective_model_slot);
        let mut app = crate::app::tests::test_app();
        app.state.mode = if planning { Mode::Plan } else { Mode::Build };
        app.state.applied_mode = app.state.mode;
        app.state.plan = PlanState::Drafting(PLAN_PATH.into());
        let expected = app.execution_agent_mode();

        respawn(&mut handles, &model_slot, &permissions, &mut app);

        assert!(!Arc::ptr_eq(&handles.execution_mode, &old_mode));
        assert!(!Arc::ptr_eq(&handles.effective_model_slot, &old_route));
        assert_eq!(app.execution_agent_mode(), expected);
    }

    #[test_case(AgentMode::Build, true, false; "build_automatic_keeps_committed_route")]
    #[test_case(AgentMode::Plan(PLAN_PATH.into()), true, false; "plan_automatic_keeps_committed_route")]
    #[test_case(AgentMode::Build, false, false; "explicit_input_uses_selected_route")]
    #[test_case(AgentMode::Build, true, true; "workflow_keeps_committed_route")]
    fn respawned_runtime_selects_route_at_admission(
        mode: AgentMode,
        automatic: bool,
        workflow: bool,
    ) {
        let mut app = crate::app::tests::test_app();
        if workflow {
            Arc::make_mut(&mut app.state.session)
                .save(&app.storage)
                .unwrap();
        }
        let (mut handles, model_slot, permissions) = stub_spawn_with_session(
            Vec::new(),
            Some(SessionRef::from(app.state.session.id)),
            None,
            workflow.then(|| app.storage.clone()),
        );
        handles.apply_to_app(&mut app);
        handles.execution_mode.store(Arc::new(mode.clone()));
        let (requests, received) = flume::unbounded();
        let provider: Arc<dyn Provider> = Arc::new(StubProvider(Some(requests)));
        let mut committed_model = crate::components::test_model();
        committed_model.id = COMMITTED_MODEL.into();
        handles.effective_model_slot.store(Arc::new(ModelSlot {
            model: committed_model.clone(),
            provider: Arc::clone(&provider),
            home: None,
        }));
        let mut selected_model = committed_model;
        selected_model.id = SELECTED_MODEL.into();
        model_slot.store(Arc::new(ModelSlot {
            model: selected_model,
            provider,
            home: None,
        }));
        respawn(&mut handles, &model_slot, &permissions, &mut app);
        if workflow {
            permissions.set_session_mode(Some(PermissionMode::Yolo));
            let response = smol::block_on(handles.workflow_handle().unwrap().request(
                WorkflowRequest::Start(LaunchRequest {
                    name: REVIEW_WORKFLOW.into(),
                    args: serde_json::json!({"scope": PROBE_TEXT}),
                    agent_budget: Some(WORKFLOW_BUDGET),
                }),
            ))
            .unwrap();
            assert!(matches!(response, WorkflowResponse::Started(_)));
        } else {
            let text = if automatic {
                String::new()
            } else {
                PROBE_TEXT.into()
            };
            handles.queue.push(QueueItem::Message {
                text: text.clone(),
                image_count: 0,
                paste_ranges: Vec::new(),
                input: Box::new(AgentInput {
                    message: text,
                    mode: app.execution_agent_mode(),
                    plan: None,
                    images: Vec::new(),
                    mentions: Vec::new(),
                    commits: Vec::new(),
                    preamble: vec![Message::observation(PROBE_TEXT.into())],
                    thinking: Default::default(),
                    fast: false,
                    prompt: None,
                    resume: false,
                }),
                run_id: app.run_id,
                admission: PromptAdmission::Queue,
                displayed: true,
            });
        }

        assert_eq!(
            received
                .recv_timeout(LONG_TIMEOUT)
                .expect(ROUTE_REQUEST_MISSING),
            if automatic {
                COMMITTED_MODEL
            } else {
                SELECTED_MODEL
            }
        );
        assert_eq!(app.execution_agent_mode(), mode);
        handles.shutdown_workflow();
        smol::block_on(handles.into_task().cancel());
    }

    /// If the seeded empty snapshot ever outlived `spawn`, the next checkpoint
    /// would adopt it and wipe a resumed conversation from disk.
    #[test]
    fn spawn_publishes_the_resumed_history_before_the_handles_escape() {
        let (handles, _model_slot, _permissions) =
            stub_spawn_with(vec![Message::user(RESUMED_HISTORY_TEXT.into())]);
        let snapshot = handles.history.load();
        assert_eq!(
            snapshot.messages.len(),
            1,
            "the seeded empty snapshot must be replaced synchronously"
        );
        assert!(matches!(
            &snapshot.messages[0].kind,
            caudra_providers::HistoryItemKind::User { text, .. }
                if text == RESUMED_HISTORY_TEXT
        ));
    }

    #[test]
    fn respawn_publishes_the_new_history_into_the_app_mirror() {
        let (mut handles, model_slot, permissions) = stub_spawn();
        let mut app = crate::app::tests::test_app();
        handles.respawn(
            crate::history_items(&[Message::user(RESUMED_HISTORY_TEXT.into())]),
            &model_slot,
            AgentConfig {
                summarize_memory: false,
                ..AgentConfig::default()
            },
            ToolOutputLines::default(),
            &permissions,
            &mut app,
            EventHandle::disconnected_for_test(),
            None,
        );

        let mirror = app
            .shared_history
            .as_ref()
            .expect("respawn wires the live mirror into the app");
        let snapshot = mirror.load();
        assert_eq!(
            snapshot.messages.len(),
            1,
            "a checkpoint right after respawn must not see the seeded empty snapshot"
        );
        assert!(matches!(
            &snapshot.messages[0].kind,
            caudra_providers::HistoryItemKind::User { text, .. }
                if text == RESUMED_HISTORY_TEXT
        ));
    }

    /// An app with automations and workflows on, whose session has its record,
    /// and its handles, with a workflow runtime when `workflows`.
    fn automated_session(workflows: bool) -> (App, AgentHandles, AutomationFixture) {
        let mut app = crate::app::tests::test_app();
        app.features = FeatureFlags::all();
        Arc::make_mut(&mut app.state.session)
            .save(&app.storage)
            .unwrap();
        let (handles, _, permissions) = stub_spawn_with_session(
            Vec::new(),
            Some(SessionRef::from(app.state.session.id)),
            None,
            workflows.then(|| app.storage.clone()),
        );
        permissions.set_session_mode(Some(PermissionMode::Yolo));
        (app, handles, AutomationFixture::default())
    }

    /// The runtime `start_automations` would start, arming `script` from the
    /// command line, with the fixture's user scripts in place of the user's.
    fn start_automations_with(
        handles: &mut AgentHandles,
        app: &App,
        fixture: &AutomationFixture,
        script: &str,
    ) {
        let cli = vec![ProfileArming {
            name: script.to_owned(),
            args: None,
        }];
        let facts = app.automation_facts(false, None, 0);
        let deps = handles
            .automation_spawn(app, &AutomationsConfig::default(), cli, facts)
            .deps()
            .expect(AUTOMATIONS_ON);
        handles.automation = AutomationSession::start(RuntimeDeps {
            user_config_dir: fixture.deps(app.state.session.id, &[]).user_config_dir,
            ..deps
        });
        assert!(handles.automation.is_some(), "{AUTOMATIONS_STARTED}");
    }

    /// Reads the session's own event feed, which a second receiver would split.
    fn automations_until(
        handles: &AgentHandles,
        ready: impl Fn(&AutomationState) -> bool,
    ) -> Arc<AutomationState> {
        let automation = handles.automation.as_ref().expect(AUTOMATIONS_STARTED);
        loop {
            let state = automation.handle().state();
            if ready(&state) {
                return state;
            }
            automation
                .events()
                .recv_timeout(LONG_TIMEOUT)
                .expect(AUTOMATION_EVENTS_STOPPED);
        }
    }

    fn only_action(handles: &AgentHandles, script: &str) -> ActionRow {
        let state = automations_until(handles, |state| {
            state
                .recent
                .iter()
                .any(|firing| firing.automation == script && !firing.status.is_pending())
        });
        let fire_id = state.recent[0].fire_id.clone();
        let automation = handles.automation.as_ref().expect(AUTOMATIONS_STARTED);
        let Ok(AutomationResponse::Firing(detail)) = smol::block_on(
            automation
                .handle()
                .request(AutomationRequest::Firing { fire_id }),
        ) else {
            panic!("{ONE_ACTION}");
        };
        detail.actions.into_iter().next().expect(ONE_ACTION)
    }

    #[test]
    fn started_automations_start_runs_on_the_sessions_workflow_runtime() {
        let (app, mut handles, fixture) = automated_session(true);
        fixture.script(&fixture.user_scripts(), STARTER, STARTER_FIELDS, START_BODY);

        start_automations_with(&mut handles, &app, &fixture, STARTER);
        let action = only_action(&handles, STARTER);

        assert_eq!(action.status, ActionStatus::Done, "{:?}", action.error);
        let run_id = action.target.expect(RUN_REACHED);
        assert!(
            handles
                .workflow_handle()
                .expect(RUN_REACHED)
                .state()
                .runs
                .iter()
                .any(|run| run.run_id == run_id),
            "{RUN_REACHED}"
        );
        handles.shutdown_workflow();
        smol::block_on(handles.into_task().cancel());
    }

    #[test]
    fn without_a_workflow_runtime_a_start_is_unavailable() {
        let (app, mut handles, fixture) = automated_session(false);
        fixture.script(&fixture.user_scripts(), STARTER, STARTER_FIELDS, START_BODY);

        start_automations_with(&mut handles, &app, &fixture, STARTER);
        let action = only_action(&handles, STARTER);

        assert_eq!(
            (action.status, action.error),
            (
                ActionStatus::Failed,
                Some(Failure::new(FailureKind::Unavailable, NO_WORKFLOWS).to_string())
            )
        );
        handles.shutdown_workflow();
        smol::block_on(handles.into_task().cancel());
    }

    /// The runs a shutdown interrupts settle as it closes the workflow
    /// runtime, so a watcher that still ran would record a firing for each.
    #[test]
    fn automations_stop_before_the_shutdown_interrupts_their_runs() {
        let (app, mut handles, fixture) = automated_session(true);
        let (requests, received) = flume::unbounded();
        handles.effective_model_slot.store(Arc::new(ModelSlot {
            model: crate::components::test_model(),
            provider: Arc::new(StubProvider(Some(requests))),
            home: None,
        }));
        fixture.script(
            &fixture.user_scripts(),
            WATCHER,
            WATCHER_FIELDS,
            WATCHER_BODY,
        );
        start_automations_with(&mut handles, &app, &fixture, WATCHER);
        automations_until(&handles, |state| {
            state
                .find(WATCHER)
                .is_some_and(|watcher| watcher.armed.is_some())
        });
        let response = smol::block_on(handles.workflow_handle().unwrap().request(
            WorkflowRequest::Start(LaunchRequest {
                name: REVIEW_WORKFLOW.into(),
                args: serde_json::json!({"scope": PROBE_TEXT}),
                agent_budget: Some(WORKFLOW_BUDGET),
            }),
        ))
        .unwrap();
        assert!(matches!(response, WorkflowResponse::Started(_)));
        received
            .recv_timeout(LONG_TIMEOUT)
            .expect(ROUTE_REQUEST_MISSING);

        handles.shutdown_workflow();

        let store = AutomationStore::spawn(app.storage.clone(), app.state.session.id).unwrap();
        let firings = smol::block_on(store.load_firings(Some(WATCHER.into()), FIRING_LIMIT));
        smol::block_on(store.shutdown());
        assert_eq!(firings.unwrap(), [], "{NO_FIRING_FOR_TEARDOWN}");
        smol::block_on(handles.into_task().cancel());
    }

    #[test]
    fn join_all_returns_when_all_tasks_complete() {
        assert!(join_all(Vec::new(), LONG_TIMEOUT));
        assert!(join_all(
            (0..3).map(|_| smol::spawn(async {})).collect(),
            LONG_TIMEOUT,
        ));
    }

    #[test]
    fn join_all_stuck_task_returns_after_shared_timeout() {
        let start = Instant::now();
        assert!(!join_all(
            vec![
                smol::spawn(async {}),
                smol::spawn(futures_lite::future::pending::<()>()),
            ],
            SHORT_TIMEOUT,
        ));
        assert!(start.elapsed() >= SHORT_TIMEOUT);
    }
}
