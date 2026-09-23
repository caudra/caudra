//! One workflow runtime per session. Agent loops come and go with every
//! respawn; the runtime outlives them all and only follows a change of
//! session id, so a run keeps going through a model switch or a revert.

use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use async_lock::Mutex as AsyncMutex;
use caudra_agent::agent::task_runner::{
    HostExtras, ModeResolver, ModelResolver, SubagentTaskRunner, WorkflowHostContext,
};
use caudra_agent::context::ContextPublisher;
use caudra_agent::mcp::McpSession;
use caudra_agent::permissions::PermissionManager;
use caudra_agent::prompt::profile::PromptProfileCatalog;
use caudra_agent::tools::{FileReadTracker, PathLocks, ToolAudience, ToolFilter, ToolRegistry};
use caudra_agent::workflow::{RuntimeDeps, WorkflowHandle, WorkflowRuntime};
use caudra_agent::{
    AgentConfig, AgentMode, AgentParams, BaselineGate, CancelMap, Envelope, McpHandle,
    SubagentHistoryStore, ToolOutputLines, agent,
};
use caudra_config::ModelPolicy;
use caudra_lua::EventHandle;
use caudra_providers::{CacheKey, Timeouts};
use caudra_storage::StateDir;
use caudra_storage::id::{CaudraId, SessionRef};
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_workspace::WorkspaceSession;
use futures_lite::future;
use smol::Timer;
use tracing::{info, warn};

use super::ModelSlot;

/// How long a session waits for its runs to interrupt and journal before
/// letting the runtime go. Past this the store may miss the final row, which
/// the next open repairs by marking the run interrupted.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// The mode the agent loop last committed to a turn. Workflow agents read it
/// when they start, so a run that outlives the turn which launched it is
/// capped by what the user allows now.
pub(crate) type SharedMode = Arc<ArcSwap<AgentMode>>;

/// What the workflow host captures once, drawn from the same inputs the
/// session's first agent loop starts with.
pub(crate) struct WorkflowSpawn<'a> {
    pub(crate) state_dir: StateDir,
    pub(crate) session_id: CaudraId,
    pub(crate) model_slot: &'a Arc<ArcSwap<ModelSlot>>,
    pub(crate) effective_model_slot: &'a Arc<ArcSwap<ModelSlot>>,
    pub(crate) config: &'a AgentConfig,
    pub(crate) tool_output_lines: ToolOutputLines,
    pub(crate) permissions: &'a Arc<PermissionManager>,
    pub(crate) mcp_handle: Option<&'a McpHandle>,
    pub(crate) timeouts: Timeouts,
    pub(crate) lua_handle: &'a EventHandle,
    pub(crate) model_policy: &'a Arc<ModelPolicy>,
    pub(crate) subagent_history: &'a SubagentHistoryStore,
    pub(crate) prompt_profiles: &'a Arc<PromptProfileCatalog>,
    pub(crate) task_prompt_profile_name: Arc<str>,
    pub(crate) context_publisher: ContextPublisher,
    pub(crate) answer: AnswerChannel,
    pub(crate) events: flume::Sender<Envelope>,
    /// The agent loop's own, or a workflow agent and the user's run could
    /// both edit one file at once and the later write would drop the other.
    pub(crate) path_locks: Arc<PathLocks>,
    /// The session's, not the runtime's: a workflow agent's write has to be
    /// recoverable through the same revert point as the user's own run.
    pub(crate) baseline: Option<BaselineGate>,
    pub(crate) workspace_session: Option<WorkspaceSession>,
    pub(crate) remote_project_context:
        Option<Arc<caudra_agent::remote_project_context::RemoteProjectContext>>,
    /// The directory Caudra itself runs in. Set only in a sandbox session,
    /// where `{cwd}` names a path inside the VM and this one does not.
    pub(crate) host_cwd: Option<PathBuf>,
    pub(crate) local_documents: Option<Arc<LocalDocumentStore>>,
}

/// The channel a `question` is answered on. Workflow agents ask through the
/// same one as the main loop, so it belongs to the session, and a respawned
/// loop inherits it rather than leaving the runtime's questions unanswerable.
pub(crate) type AnswerChannel = (
    flume::Sender<String>,
    Arc<AsyncMutex<flume::Receiver<String>>>,
);

pub(crate) fn answer_channel() -> AnswerChannel {
    let (tx, rx) = flume::unbounded();
    (tx, Arc::new(AsyncMutex::new(rx)))
}

pub(crate) struct WorkflowSession {
    session_id: CaudraId,
    runtime: WorkflowRuntime,
    handle: WorkflowHandle,
    mode: SharedMode,
    effective_model_slot: Arc<ArcSwap<ModelSlot>>,
    answer: AnswerChannel,
}

impl WorkflowSession {
    /// `None` when the runtime cannot open its store: the session then runs
    /// without workflows rather than not at all.
    pub(crate) fn spawn(spawn: WorkflowSpawn<'_>) -> Option<Self> {
        let cwd = match &spawn.workspace_session {
            Some(workspace) => match smol::block_on(caudra_agent::workspace_logical_cwd(workspace))
            {
                Ok(cwd) => cwd.into(),
                Err(error) => {
                    warn!(%error, "remote workflow cwd unavailable");
                    return None;
                }
            },
            None => env::current_dir().unwrap_or_else(|_| spawn.permissions.project_cwd()),
        };
        let slot = spawn.model_slot.load();
        let tool_filter = ToolFilter::from_config(spawn.config, &slot.model, &[])
            .for_remote_workspace(spawn.workspace_session.is_some());
        let session_ref = SessionRef::from(spawn.session_id);
        let base = AgentParams {
            provider: Arc::clone(&slot.provider),
            model: slot.model.clone(),
            chat_provider: Arc::clone(&slot.provider),
            chat_model: slot.model.clone(),
            config: spawn.config.clone(),
            tool_output_lines: spawn.tool_output_lines,
            permissions: Arc::clone(spawn.permissions),
            session_id: Some(session_ref.clone()),
            cache_key: Some(CacheKey::session(&session_ref)),
            workspace_session: spawn.workspace_session,
            remote_project_context: spawn.remote_project_context.clone(),
            host_cwd: spawn.host_cwd.clone(),
            local_documents: spawn.local_documents,
            task_environment: caudra_agent::template::env_vars()
                .set("{cwd}", cwd.to_string_lossy().into_owned()),
            root_tool_use_id: None,
            mailbox: None,
            context_publisher: Some(spawn.context_publisher),
            timeouts: spawn.timeouts,
            file_tracker: FileReadTracker::fresh(),
            path_locks: spawn.path_locks,
            baseline: spawn.baseline.clone(),
            prompt_slots: Arc::new(spawn.lua_handle.collect_prompt_slots(spawn.config)),
            prompt_profiles: Arc::clone(spawn.prompt_profiles),
            default_task_prompt_profile_name: Arc::clone(&spawn.task_prompt_profile_name),
            active_prompt_profile_name: Some(spawn.task_prompt_profile_name),
            subagent_cancels: Arc::new(CancelMap::new()),
            subagent_history: spawn.subagent_history.clone(),
            registry: Arc::clone(ToolRegistry::global_arc()),
            audience: ToolAudience::MAIN,
            tool_filter,
            model_policy: Arc::clone(spawn.model_policy),
            workflow: None,
        };
        drop(slot);
        let mode: SharedMode = Arc::new(ArcSwap::from_pointee(AgentMode::default()));
        let mode_resolver: ModeResolver = Arc::new({
            let mode = Arc::clone(&mode);
            move || AgentMode::clone(&mode.load())
        });
        let model_resolver: ModelResolver = Arc::new({
            let model_slot = Arc::clone(spawn.model_slot);
            move || {
                let slot = model_slot.load();
                (Arc::clone(&slot.provider), Arc::new(slot.model.clone()))
            }
        });
        // The runtime's own registrations, separate from the per-generation
        // map the agent loop's cancel sweep clears.
        let subagent_cancels = Arc::new(CancelMap::new());
        let mcp = spawn.mcp_handle.map(|handle| {
            McpSession::new(handle.clone(), &[]).with_disabled_tools(&spawn.config.disabled_tools)
        });
        let started = Instant::now();
        let loaded_instructions = spawn.remote_project_context.as_ref().map_or_else(
            || agent::load_instructions(&cwd.to_string_lossy()).loaded,
            |context| agent::load_remote_instructions(context, spawn.host_cwd.as_deref()).loaded,
        );
        let instructions_ms = started.elapsed().as_millis() as u64;
        let runtime_start = Instant::now();
        let host = WorkflowHostContext::from_agent_params(
            &base,
            HostExtras {
                mcp,
                loaded_instructions,
                user_response_rx: Some(Arc::clone(&spawn.answer.1)),
            },
            model_resolver,
            Arc::clone(&mode_resolver),
            Arc::clone(&subagent_cancels),
        );
        let host_ms = runtime_start.elapsed().as_millis() as u64;
        let block_on_start = Instant::now();
        let runtime = smol::block_on(WorkflowRuntime::spawn(RuntimeDeps {
            state_dir: spawn.state_dir,
            session_id: spawn.session_id,
            cwd,
            user_config_dir: None,
            remote_project_context: spawn.remote_project_context,
            runner: Arc::new(SubagentTaskRunner::new(Arc::new(host))),
            events: spawn.events,
            mode: mode_resolver,
            subagent_cancels,
        }))
        .map_err(|error| {
            warn!(%error, session_id = %spawn.session_id, "workflow runtime unavailable for this session")
        })
        .ok()?;
        info!(
            session_id = %spawn.session_id,
            instructions_ms,
            host_ms,
            block_on_ms = block_on_start.elapsed().as_millis() as u64,
            runtime_spawn_ms = runtime_start.elapsed().as_millis() as u64,
            "workflow runtime started"
        );
        Some(Self {
            session_id: spawn.session_id,
            handle: runtime.handle(),
            runtime,
            mode,
            effective_model_slot: Arc::clone(spawn.effective_model_slot),
            answer: spawn.answer,
        })
    }

    pub(crate) fn session_id(&self) -> CaudraId {
        self.session_id
    }

    pub(crate) fn answer_channel(&self) -> AnswerChannel {
        (self.answer.0.clone(), Arc::clone(&self.answer.1))
    }

    pub(crate) fn handle(&self) -> WorkflowHandle {
        self.handle.clone()
    }

    pub(crate) fn mode(&self) -> SharedMode {
        Arc::clone(&self.mode)
    }

    pub(crate) fn effective_model_slot(&self) -> Arc<ArcSwap<ModelSlot>> {
        Arc::clone(&self.effective_model_slot)
    }

    /// Runs whose script is executing right now, which no agent turn owns.
    pub(crate) fn active_runs(&self) -> usize {
        self.handle.active_count()
    }

    /// Interrupts every run and waits for its agents and store to close, so
    /// nothing of this session's runtime survives into the next one.
    pub(crate) fn shutdown(self) {
        info!(session_id = %self.session_id, "workflow runtime shutting down");
        let closed = smol::block_on(future::or(
            async {
                self.runtime.shutdown().await;
                true
            },
            async {
                Timer::after(SHUTDOWN_TIMEOUT).await;
                false
            },
        ));
        if !closed {
            warn!(
                session_id = %self.session_id,
                timeout = ?SHUTDOWN_TIMEOUT,
                "workflow runtime did not close in time, abandoning it"
            );
        }
    }
}
