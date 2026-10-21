use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol_schema::v1::{
    AgentResponse, CloseSessionResponse, DeleteSessionResponse, Error as AcpError,
    ForkSessionRequest, ForkSessionResponse, McpServer, NewSessionRequest, RequestId,
    ResumeSessionResponse, SessionId, SessionUpdate,
};
use caudra_agent::headless::{self, InteractiveHandle, InteractiveParams, PreparedInteractive};
use caudra_agent::mcp::McpHandle;
use caudra_agent::prompt::profile::{BUILTIN_PROFILE_NAME, SystemPromptProfile};
use caudra_agent::session_fork::{ForkTarget, fork_stored_session};
use caudra_agent::{AgentMode, History, StoredSession, open_stored_session};
use caudra_otel::emit;
use caudra_providers::model::Model;
use caudra_providers::{HistoryItem, ThinkingConfig};
use caudra_storage::id::{CaudraId, SessionRef};
use caudra_storage::permission_state::PermissionRuleRecord;
use caudra_storage::sessions::{PermissionMode, SessionError, SessionLease};
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_storage::{StateDir, StorageError};
use flume::Sender;

use crate::asks::ClientProfile;
use crate::child_work::Children;
use crate::errors::{
    agent_failure, auth_required, internal, invalid_params, json_str, unknown_session,
};
use crate::mcp_servers::{preflight_mcp, start_mcp};
use crate::outbound::{Outbound, PendingState};
use crate::restore::{
    Restored, load_history_from, replay, restored_cost, restored_mode, restored_plan,
    restored_usage, stored_model,
};
use crate::server::Incoming;
use crate::session::SessionState;
use crate::{AcpParams, AcpRuntime, AcpRuntimeGuard, AcpRuntimeResolver, methods};

const SESSION_IN_USE_ERROR_CODE: i32 = -32001;
pub(crate) const RUNTIME_SHUTDOWN_FAILED: &str = "Session runtime did not shut down; its lease is retained. Restart the ACP server before loading this session again.";
const WORKSPACE_IDENTITY_CHANGED: &str =
    "session workspace identity changed; runtime detached, no local fallback";

/// A runtime that would not stop. It stays alive, lease and all, and only
/// the session it served is refused until the server restarts.
pub(crate) struct FailedRuntime {
    pub(crate) session: SessionId,
    pub(crate) error: String,
    _guard: Box<dyn AcpRuntimeGuard>,
}

impl FailedRuntime {
    pub(crate) fn refusal(&self) -> AcpError {
        internal(&format!("{RUNTIME_SHUTDOWN_FAILED} {}", self.error))
    }
}

fn shut_down(session: SessionId, runtime: &mut AcpRuntime) -> Option<FailedRuntime> {
    let mut guard = runtime.guard.take()?;
    guard.shutdown().err().map(|error| FailedRuntime {
        session,
        error,
        _guard: guard,
    })
}

/// Stops `state` and the runtime it ran on.
pub(crate) async fn close_session(
    state: SessionState,
    outbound: &Outbound,
) -> Option<FailedRuntime> {
    let session = state.id();
    let mut runtime = state.stop(outbound).await;
    shut_down(session, &mut runtime)
}

/// What a start needs from the server, owned so it can run off the main loop.
pub(crate) struct StartContext {
    pub(crate) params: Arc<AcpParams>,
    pub(crate) client: ClientProfile,
    pub(crate) outbound: Outbound,
    pub(crate) storage: StateDir,
    pub(crate) model_specs: Vec<String>,
    pub(crate) default_model: Model,
    pub(crate) events: Sender<Incoming>,
}

/// How a stored session comes back. Only a load shows the client what it
/// already said; a fork is a new session that resumes its copy.
#[derive(Clone, Copy)]
pub(crate) enum Reopen {
    Load,
    Resume,
}

pub(crate) enum StartRequest {
    New(NewSessionRequest),
    Reopen {
        how: Reopen,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
    },
    Fork {
        source: SessionRef,
        request: ForkSessionRequest,
        /// The live mode of a source this server runs; a stored source keeps its own.
        permission_mode: Option<PermissionMode>,
    },
}

pub(crate) struct Started {
    pub(crate) state: SessionState,
    pub(crate) response: AgentResponse,
    pub(crate) initial_cost: Option<f64>,
    /// What the client learns about the session right after the response.
    pub(crate) updates: Vec<SessionUpdate>,
}

pub(crate) struct StartOutcome {
    pub(crate) request_id: RequestId,
    pub(crate) session: SessionId,
    pub(crate) result: Result<Started, AcpError>,
    pub(crate) failure: Option<FailedRuntime>,
}

/// Starts the session off the main loop. Each session resolves its own
/// runtime and tools, so starts run side by side.
pub(crate) fn spawn_start(
    ctx: StartContext,
    request_id: RequestId,
    session: SessionRef,
    request: StartRequest,
) {
    smol::spawn(async move {
        let mut starter = Starter {
            ctx: &ctx,
            failure: None,
        };
        let result = match request {
            StartRequest::New(request) => starter.new_session(&session, request).await,
            StartRequest::Reopen {
                how,
                cwd,
                mcp_servers,
            } => starter.reopen(&session, how, cwd, mcp_servers).await,
            StartRequest::Fork {
                source,
                request,
                permission_mode,
            } => {
                starter
                    .fork(&session, &source, request, permission_mode)
                    .await
            }
        };
        let outcome = StartOutcome {
            request_id,
            session: SessionId::from(session.to_string()),
            result,
            failure: starter.failure,
        };
        let _ = ctx.events.send(Incoming::Started(Box::new(outcome)));
    })
    .detach();
}

pub(crate) enum Closing {
    Close,
    Delete,
}

/// What a close needs from the server, owned so it can run off the main loop.
pub(crate) struct CloseContext {
    pub(crate) outbound: Outbound,
    pub(crate) storage: StateDir,
    pub(crate) events: Sender<Incoming>,
}

pub(crate) struct CloseOutcome {
    pub(crate) request_id: RequestId,
    pub(crate) session: SessionId,
    pub(crate) result: Result<AgentResponse, AcpError>,
    pub(crate) failure: Option<FailedRuntime>,
}

/// Stops `state` if the server runs it, which answers its owed prompt as
/// cancelled first, then deletes the stored session when asked to. The
/// response waits on all of it.
pub(crate) fn spawn_close(
    ctx: CloseContext,
    request_id: RequestId,
    session: SessionRef,
    state: Option<SessionState>,
    closing: Closing,
) {
    smol::spawn(async move {
        let failure = match state {
            Some(state) => close_session(state, &ctx.outbound).await,
            None => None,
        };
        let result = match (&failure, closing) {
            (Some(failure), _) => Err(failure.refusal()),
            (None, Closing::Close) => Ok(AgentResponse::CloseSessionResponse(
                CloseSessionResponse::new(),
            )),
            (None, Closing::Delete) => {
                let (storage, deleted) = (ctx.storage, session.clone());
                smol::unblock(move || delete_stored(&storage, &deleted))
                    .await
                    .map(|()| AgentResponse::DeleteSessionResponse(DeleteSessionResponse::new()))
            }
        };
        let outcome = CloseOutcome {
            request_id,
            session: SessionId::from(session.to_string()),
            result,
            failure,
        };
        let _ = ctx.events.send(Incoming::Closed(Box::new(outcome)));
    })
    .detach();
}

/// Deletes under the session's lease, as the TUI does, so no other process
/// writes the session back meanwhile.
fn delete_stored(storage: &StateDir, session: &SessionRef) -> Result<(), AcpError> {
    let _lease = SessionLease::acquire(storage, session.id()).map_err(session_lease_error)?;
    StoredSession::delete(session.id(), storage).map_err(|error| match error {
        SessionError::Storage(StorageError::NotFound(_)) => {
            unknown_session(&SessionId::from(session.to_string()))
        }
        error => internal(&error),
    })
}

fn open_source(storage: &StateDir, source: &SessionRef) -> Result<StoredSession, AcpError> {
    open_stored_session(source.id(), storage).map_err(|error| {
        AcpError::resource_not_found(Some(source.to_string())).data(json_str(&error))
    })
}

/// Writes the fork of `stored` that `target` will resume.
fn create_fork(
    storage: &StateDir,
    stored: &StoredSession,
    target: ForkTarget<'_>,
) -> Result<(), AcpError> {
    let target = ForkTarget {
        permission_mode: target
            .permission_mode
            .or_else(|| stored.meta.permission_mode.clone()),
        ..target
    };
    fork_stored_session(storage, stored, target)
        .map(drop)
        .map_err(|error| internal(&error))
}

struct Starter<'a> {
    ctx: &'a StartContext,
    failure: Option<FailedRuntime>,
}

struct Launch {
    session: SessionRef,
    lease: Arc<SessionLease>,
    cwd: PathBuf,
    mcp_servers: Vec<McpServer>,
    runtime: AcpRuntime,
    stored_profile: Option<String>,
    model: Model,
    /// The level as asked, which the launch fits to `model`.
    thinking: ThinkingConfig,
    fast: bool,
    expected_write_version: Option<i64>,
    history: Vec<HistoryItem>,
    permissions: (Vec<PermissionRuleRecord>, Option<PermissionMode>),
}

struct SessionStart {
    cwd: PathBuf,
    session: SessionRef,
    lease: Arc<SessionLease>,
    model: Model,
    thinking: ThinkingConfig,
    expected_write_version: Option<i64>,
    history: Vec<HistoryItem>,
    permissions: (Vec<PermissionRuleRecord>, Option<PermissionMode>),
    profile: (String, Option<Arc<SystemPromptProfile>>),
}

/// A stored session read back and its runtime resolved, ready to launch.
struct Restore {
    lease: Arc<SessionLease>,
    model: Model,
    restored: Restored,
    history: History,
    runtime: AcpRuntime,
    cwd: PathBuf,
    mcp_servers: Vec<McpServer>,
}

#[derive(Clone, Copy)]
enum Restoring {
    Reopen(Reopen),
    Fork,
}

impl Starter<'_> {
    async fn new_session(
        &mut self,
        session: &SessionRef,
        request: NewSessionRequest,
    ) -> Result<Started, AcpError> {
        let ctx = self.ctx;
        require_credentials(ctx, &ctx.default_model).await?;
        let lease = acquire_session_lease(&ctx.storage, session.id())?;
        let runtime = resolve_runtime(
            &ctx.params.runtime_resolver,
            request.cwd.clone(),
            None,
            false,
        )
        .await?;
        let mut state = self
            .launch(Launch {
                session: session.clone(),
                lease,
                cwd: request.cwd,
                mcp_servers: request.mcp_servers,
                runtime,
                stored_profile: None,
                model: ctx.default_model.clone(),
                thinking: ctx.params.thinking.clone(),
                fast: false,
                expected_write_version: None,
                history: Vec::new(),
                permissions: (Vec::new(), None),
            })
            .await?;
        emit::session_started(emit::START_FRESH, Some(session.as_str()));
        let response = methods::new_session_response(session.as_str())
            .config_options(state.hand_over_options(&ctx.model_specs));
        Ok(Started {
            state,
            response: AgentResponse::NewSessionResponse(response),
            initial_cost: None,
            updates: Vec::new(),
        })
    }

    async fn reopen(
        &mut self,
        session: &SessionRef,
        how: Reopen,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
    ) -> Result<Started, AcpError> {
        let ctx = self.ctx;
        let lease = acquire_session_lease(&ctx.storage, session.id())?;
        let mut restored = load_history_from(&ctx.storage, session.id())?;
        let model = stored_model(
            &ctx.params.model_policy,
            &restored.model,
            &ctx.default_model,
        );
        require_credentials(ctx, &model).await?;
        let history =
            History::restored(mem::take(&mut restored.history)).map_err(|e| internal(&e))?;
        let runtime = resolve_runtime(
            &ctx.params.runtime_resolver,
            cwd.clone(),
            restored.workspace_binding.clone(),
            true,
        )
        .await?;
        let restore = Restore {
            lease,
            model,
            restored,
            history,
            runtime,
            cwd,
            mcp_servers,
        };
        self.restore(session, restore, Restoring::Reopen(how)).await
    }

    /// Forks into `session` on the request's cwd, then resumes the fork.
    async fn fork(
        &mut self,
        session: &SessionRef,
        source: &SessionRef,
        request: ForkSessionRequest,
        permission_mode: Option<PermissionMode>,
    ) -> Result<Started, AcpError> {
        let ctx = self.ctx;
        let stored = open_source(&ctx.storage, source)?;
        let model = stored_model(&ctx.params.model_policy, &stored.model, &ctx.default_model);
        require_credentials(ctx, &model).await?;
        let lease = acquire_session_lease(&ctx.storage, session.id())?;
        let mut runtime = resolve_runtime(
            &ctx.params.runtime_resolver,
            request.cwd.clone(),
            None,
            false,
        )
        .await?;
        let stored_cwd = runtime.remote_environment.as_ref().map_or_else(
            || request.cwd.to_string_lossy().into_owned(),
            |environment| environment.cwd.clone(),
        );
        let target = ForkTarget {
            id: session,
            cwd: &stored_cwd,
            workspace_binding: runtime.workspace_binding.as_ref(),
            permission_mode,
        };
        let forked = create_fork(&ctx.storage, &stored, target).and_then(|()| {
            let mut restored = load_history_from(&ctx.storage, session.id())?;
            let history =
                History::restored(mem::take(&mut restored.history)).map_err(|e| internal(&e))?;
            Ok((restored, history))
        });
        let (restored, history) = match forked {
            Ok(forked) => forked,
            Err(error) => {
                self.failure = shut_down(SessionId::from(session.to_string()), &mut runtime);
                return Err(error);
            }
        };
        let restore = Restore {
            lease,
            model,
            restored,
            history,
            runtime,
            cwd: request.cwd,
            mcp_servers: request.mcp_servers,
        };
        self.restore(session, restore, Restoring::Fork).await
    }

    async fn restore(
        &mut self,
        session: &SessionRef,
        restore: Restore,
        restoring: Restoring,
    ) -> Result<Started, AcpError> {
        let ctx = self.ctx;
        let Restore {
            lease,
            model,
            mut restored,
            history,
            runtime,
            cwd,
            mcp_servers,
        } = restore;
        let replayed = match restoring {
            Restoring::Reopen(Reopen::Load) => replay(
                &runtime.registry,
                &history,
                restored.cwd.as_deref().unwrap_or(&cwd),
            ),
            Restoring::Reopen(Reopen::Resume) | Restoring::Fork => Vec::new(),
        };
        let initial_cost = restored_cost(&mut restored, &ctx.default_model);
        let usage = restored_usage(&history, &model, initial_cost);
        let mut state = self
            .launch(Launch {
                session: session.clone(),
                lease,
                cwd,
                mcp_servers,
                runtime,
                stored_profile: restored.system_prompt_profile.clone(),
                model,
                thinking: restored
                    .thinking
                    .take()
                    .map_or_else(|| ctx.params.thinking.clone(), ThinkingConfig::from),
                fast: restored.fast,
                expected_write_version: restored.write_version,
                history: history.into_items(),
                permissions: (
                    mem::take(&mut restored.structured_permission_rules),
                    restored.permission_mode.clone(),
                ),
            })
            .await?;
        let sid = state.id();
        for update in replayed {
            ctx.outbound.session_update(&sid, update);
        }
        let (start, response): (_, fn(&mut SessionState, &[String]) -> AgentResponse) =
            match restoring {
                Restoring::Reopen(Reopen::Load) => (emit::START_RESUME, load_response),
                Restoring::Reopen(Reopen::Resume) => (emit::START_RESUME, resume_response),
                Restoring::Fork => (emit::START_FORK, fork_response),
            };
        emit::session_started(start, Some(session.as_str()));
        state.plan = restored_plan(
            restored.plan_target.as_ref(),
            restored.plan_path.as_deref(),
            &state.runtime,
            session,
        );
        state.current_mode = restored_mode(restored.mode, state.plan.as_ref());
        Ok(Started {
            response: response(&mut state, &ctx.model_specs),
            initial_cost,
            updates: restored.info().into_iter().chain([usage]).collect(),
            state,
        })
    }

    async fn launch(&mut self, launch: Launch) -> Result<SessionState, AcpError> {
        let params = &self.ctx.params;
        let mut runtime = launch.runtime;
        let profile = resolve_prompt_profile(params, &runtime, launch.stored_profile.as_deref())?;
        let remote = runtime.remote_environment.is_some();
        preflight_mcp(&launch.cwd, &launch.mcp_servers, remote).await?;
        let cwd = runtime.remote_environment.as_ref().map_or_else(
            || launch.cwd.clone(),
            |environment| environment.cwd.clone().into(),
        );
        let id = SessionId::from(launch.session.to_string());
        let pending = PendingState::default();
        let model = launch.model.clone();
        let interactive = interactive_params(
            self.ctx,
            &runtime,
            &pending,
            SessionStart {
                cwd: cwd.clone(),
                session: launch.session,
                lease: launch.lease,
                model: launch.model,
                thinking: launch.thinking.clone(),
                expected_write_version: launch.expected_write_version,
                history: launch.history,
                permissions: launch.permissions,
                profile,
            },
        );
        let prepared = headless::prepare_interactive(interactive)
            .await
            .map_err(|e| agent_failure(&e.to_string()));
        #[cfg(test)]
        let prepared = prepared.map(|mut prepared| {
            if let Some(provider) = &runtime.provider {
                prepared.set_provider(Arc::clone(provider));
            }
            prepared
        });
        let started = match prepared {
            Ok(prepared) => spawn_agent(prepared, &cwd, &launch.mcp_servers, remote).await,
            Err(error) => Err(error),
        };
        match started {
            Ok((handle, mcp)) => {
                let mut state = SessionState {
                    cwd: handle.remote_cwd().map(PathBuf::from).unwrap_or(cwd),
                    handle,
                    mcp,
                    current_mode: AgentMode::Build,
                    plan: None,
                    model,
                    thinking: launch.thinking,
                    fast: launch.fast,
                    boolean_options: self.ctx.client.boolean_options,
                    updates: self.ctx.client.updates.clone(),
                    child_tier: self.ctx.client.child_tier,
                    children: Children::default(),
                    announced: Vec::new(),
                    commands: Vec::new(),
                    pending,
                    remote,
                    runtime,
                };
                state.fit_reasoning();
                Ok(state)
            }
            Err(error) => {
                self.failure = shut_down(id, &mut runtime);
                Err(error)
            }
        }
    }
}

pub(crate) fn load_response(state: &mut SessionState, model_specs: &[String]) -> AgentResponse {
    AgentResponse::LoadSessionResponse(
        methods::load_session_response()
            .modes(methods::mode_state(state.mode_id()))
            .config_options(state.hand_over_options(model_specs)),
    )
}

pub(crate) fn resume_response(state: &mut SessionState, model_specs: &[String]) -> AgentResponse {
    AgentResponse::ResumeSessionResponse(
        ResumeSessionResponse::new()
            .modes(methods::mode_state(state.mode_id()))
            .config_options(state.hand_over_options(model_specs)),
    )
}

fn fork_response(state: &mut SessionState, model_specs: &[String]) -> AgentResponse {
    AgentResponse::ForkSessionResponse(
        ForkSessionResponse::new(state.id())
            .modes(methods::mode_state(state.mode_id()))
            .config_options(state.hand_over_options(model_specs)),
    )
}

/// Starts the prepared agent and its MCP servers. Every runtime value it
/// needs was copied into `prepared`, so nothing borrowed is held across the awaits.
async fn spawn_agent(
    mut prepared: PreparedInteractive,
    cwd: &Path,
    mcp_servers: &[McpServer],
    remote: bool,
) -> Result<(InteractiveHandle, Option<McpHandle>), AcpError> {
    let mcp = start_mcp(cwd, mcp_servers, remote).await;
    prepared.set_mcp_handle(mcp.clone());
    let handle = headless::spawn_prepared_persistent_interactive(prepared)
        .await
        .map_err(|e| agent_failure(&e.to_string()))?;
    Ok((handle, mcp))
}

fn interactive_params(
    ctx: &StartContext,
    runtime: &AcpRuntime,
    pending: &PendingState,
    start: SessionStart,
) -> InteractiveParams {
    let params = &ctx.params;
    let (excluded_tools, local_tools) = ctx.client.question_tools(&ctx.outbound, pending);
    let (structured_permission_rules, session_permission_mode) = start.permissions;
    let (system_prompt_profile_name, system_prompt_profile) = start.profile;
    InteractiveParams {
        model: start.model,
        config: runtime.config.clone(),
        permissions_config: runtime.permissions_config.clone(),
        decisions_config: runtime.decisions_config.clone(),
        snapshots: runtime.snapshots,
        timeouts: params.timeouts,
        prompt_slots: Arc::clone(&runtime.prompt_slots),
        thinking: start.thinking,
        system_prompt_profile,
        system_prompt_profile_name: Some(system_prompt_profile_name),
        prompt_profiles: Arc::clone(&runtime.prompt_profiles),
        excluded_tools,
        registry: Arc::clone(&runtime.registry),
        mcp_handle: None,
        initial_wd: start.cwd,
        session_id: start.session,
        session_lease: start.lease,
        expected_write_version: start.expected_write_version,
        initial_history: start.history,
        seed_permission_mode: runtime.seed_permission_mode.clone(),
        structured_permission_rules,
        session_permission_mode: params.permission_mode.clone().or(session_permission_mode),
        system_prompt_override: None,
        append_system_prompt: None,
        model_policy: Arc::clone(&params.model_policy),
        plugin_rules: Arc::clone(&runtime.plugin_rules),
        local_tools,
        mcp_elicitors: ctx.client.mcp_elicitors(&ctx.outbound, pending),
        // ACP has no wire shape for workflow runs, so a session under it
        // gets no runtime and the `workflow` tool reports unavailable.
        // Background tasks still run: the pump reports them.
        workflow_mode: None,
        workspace_binding: runtime.workspace_binding.clone(),
        remote_environment: runtime.remote_environment.clone(),
        workspace_session: runtime.workspace_session.clone(),
        remote_project_context: runtime.remote_project_context.clone(),
        // An editor launches ACP from a directory that need not be the project
        // the sandbox holds, so a host overlay here would be a guess.
        host_cwd: None,
        local_documents: runtime.local_documents.clone(),
        automations: None,
        // The client owns the permission prompts, so no delegate answers them.
        autopilot: None,
    }
}

fn resolve_prompt_profile(
    params: &AcpParams,
    runtime: &AcpRuntime,
    stored_name: Option<&str>,
) -> Result<(String, Option<Arc<SystemPromptProfile>>), AcpError> {
    let requested_name = params
        .system_prompt_profile_override
        .as_deref()
        .or(stored_name)
        .or(runtime.config.system_prompt_profile.as_deref());
    let profile = runtime
        .prompt_profiles
        .resolve(requested_name)
        .map_err(|e| internal(&e))?;
    Ok((
        requested_name.unwrap_or(BUILTIN_PROFILE_NAME).to_owned(),
        profile,
    ))
}

/// Refuses a start whose model no account can serve before anything is
/// started for it, so the client is asked to sign in up front rather than
/// failing the first prompt.
async fn require_credentials(ctx: &StartContext, model: &Model) -> Result<(), AcpError> {
    let accounts = Arc::clone(&ctx.params.accounts);
    let model = model.clone();
    smol::unblock(move || accounts.check(&model))
        .await
        .map_err(|text| auth_required(&text))
}

fn acquire_session_lease(storage: &StateDir, id: CaudraId) -> Result<Arc<SessionLease>, AcpError> {
    SessionLease::acquire(storage, id)
        .map(Arc::new)
        .map_err(session_lease_error)
}

fn session_lease_error(error: SessionError) -> AcpError {
    match error {
        SessionError::SessionInUse { .. } => {
            AcpError::new(SESSION_IN_USE_ERROR_CODE, error.to_string())
        }
        _ => internal(&error),
    }
}

async fn resolve_runtime(
    resolver: &AcpRuntimeResolver,
    cwd: PathBuf,
    stored: Option<StoredWorkspaceBinding>,
    restoring: bool,
) -> Result<AcpRuntime, AcpError> {
    let resolver = Arc::clone(resolver);
    let expected = stored.clone();
    let runtime = smol::unblock(move || resolver(cwd, stored))
        .await
        .map_err(|error| invalid_params(&error))?;
    if restoring
        && StoredWorkspaceBinding::validate_resume_identity(
            expected.as_ref(),
            runtime.workspace_binding.as_ref(),
        )
        .is_err()
    {
        return Err(invalid_params(WORKSPACE_IDENTITY_CHANGED));
    }
    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agent_client_protocol_schema::v1::ErrorCode;
    use caudra_providers::Message;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::server::SessionSlot;
    use crate::test_support::{OFFLINE_SPEC, TestServer, history_items};

    const SANDBOX_ANCHOR: &str = "https://sandbox.test";
    const FORK_CWD: &str = "/project";
    const FORK_PROMPT: &str = "rename foo to bar";

    #[test_case(false; "fresh_server_restores_sandbox_provenance")]
    #[test_case(true; "mismatched_runtime_is_refused")]
    fn runtime_resolver_precedes_restore_identity_validation(mismatch: bool) {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let local = StoredWorkspaceBinding::local_from_cwd(".");
        let remote = serde_json::from_str::<StoredWorkspaceBinding>(
            &serde_json::to_string(&local)
                .unwrap()
                .replace(local.trust_anchor().as_str(), SANDBOX_ANCHOR),
        )
        .unwrap()
        .with_sandbox_record(CaudraId::generate())
        .unwrap();
        let mut session =
            caudra_agent::StoredSession::new_with_workspace(OFFLINE_SPEC, ".", remote.clone());
        session.save(&storage).unwrap();
        let restored = load_history_from(&storage, session.id).unwrap();
        let expected = remote.clone();
        let resolver: AcpRuntimeResolver = Arc::new(move |_, stored| {
            assert_eq!(stored.as_ref(), Some(&expected));
            Ok(AcpRuntime {
                workspace_binding: (!mismatch).then(|| expected.clone()),
                ..Default::default()
            })
        });
        let result = smol::block_on(resolve_runtime(
            &resolver,
            temp.path().to_path_buf(),
            restored.workspace_binding,
            true,
        ));
        assert_eq!(result.is_err(), mismatch);
        if let Ok(runtime) = result {
            assert_eq!(runtime.workspace_binding, Some(remote));
        }
    }

    #[test_case(false; "new_local_default")]
    #[test_case(true; "legacy_local_restore")]
    fn injected_local_runtime_remains_local(restoring: bool) {
        let resolver: AcpRuntimeResolver = Arc::new(|_, stored| {
            assert!(stored.is_none());
            Ok(AcpRuntime::default())
        });
        let runtime = smol::block_on(resolve_runtime(
            &resolver,
            PathBuf::from("."),
            None,
            restoring,
        ))
        .unwrap();
        assert!(runtime.workspace_binding.is_none());
        assert!(runtime.remote_environment.is_none());
    }

    #[test]
    fn closing_a_session_waits_for_its_agent_and_releases_its_lease() {
        let mut test = TestServer::with_session();
        let Some(SessionSlot::Active(mut state)) = test.server.sessions.remove(&test.sid) else {
            panic!("the test session is active");
        };
        let lease = Arc::downgrade(&state.handle.session_lease);
        let (finished, observed) = flume::bounded(1);
        state.handle.task = smol::spawn(async move {
            finished.send_async(()).await.unwrap();
        });
        let failure = smol::block_on(close_session(*state, &test.server.outbound));
        assert!(failure.is_none());
        assert_eq!(observed.try_recv(), Ok(()));
        assert!(lease.upgrade().is_none());
    }

    #[test]
    fn session_in_use_has_a_distinct_server_error() {
        let id = CaudraId::generate();

        let error = session_lease_error(SessionError::SessionInUse { id });

        assert_eq!(error.code, ErrorCode::Other(SESSION_IN_USE_ERROR_CODE));
        assert!(error.message.contains(&id.to_string()));
    }

    #[test_case(None, PermissionMode::Yolo ; "stored_source_keeps_its_mode")]
    #[test_case(Some(PermissionMode::Auto), PermissionMode::Auto ; "live_source_passes_its_mode")]
    fn a_fork_copies_history_into_a_new_session_and_leaves_the_source(
        live: Option<PermissionMode>,
        expected: PermissionMode,
    ) {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let mut source = StoredSession::new(OFFLINE_SPEC, FORK_CWD);
        source.meta.permission_mode = Some(PermissionMode::Yolo);
        source.replace_messages(history_items(&[Message::user(FORK_PROMPT.into())]));
        source.save(&storage).unwrap();
        let source_ref = SessionRef::from(source.id);
        let target = SessionRef::generate();

        create_fork(
            &storage,
            &open_source(&storage, &source_ref).unwrap(),
            ForkTarget {
                id: &target,
                cwd: FORK_CWD,
                workspace_binding: None,
                permission_mode: live,
            },
        )
        .unwrap();

        let fork = load_history_from(&storage, target.id()).unwrap();
        let original = load_history_from(&storage, source.id).unwrap();
        assert_ne!(target.id(), source.id);
        assert_eq!(fork.permission_mode, Some(expected));
        assert_eq!(fork.history.len(), original.history.len());
        assert_eq!(
            serde_json::to_value(History::restored(fork.history).unwrap().into_vec()).unwrap(),
            serde_json::to_value(History::restored(original.history).unwrap().into_vec()).unwrap()
        );
        assert_eq!(original.permission_mode, Some(PermissionMode::Yolo));
    }

    #[test]
    fn forking_a_missing_source_is_resource_not_found() {
        let temp = TempDir::new().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));

        let error = open_source(&storage, &SessionRef::generate()).unwrap_err();

        assert_eq!(error.code, ErrorCode::ResourceNotFound);
    }

    struct CountingGuard(Arc<AtomicUsize>);

    impl AcpRuntimeGuard for CountingGuard {
        fn shutdown(&mut self) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn runtime(stops: &Arc<AtomicUsize>) -> AcpRuntime {
        AcpRuntime {
            guard: Some(Box::new(CountingGuard(Arc::clone(stops)))),
            ..Default::default()
        }
    }

    #[test]
    fn closing_a_session_stops_only_its_own_runtime() {
        let stops = Arc::new(AtomicUsize::new(0));
        let mut test = TestServer::with_session();
        let other = test.add_session();
        test.session_mut().runtime = runtime(&stops);
        let Some(SessionSlot::Active(state)) = test.server.sessions.remove(&test.sid) else {
            panic!("the test session is active");
        };
        let Some(SessionSlot::Active(other_state)) = test.server.sessions.get_mut(&other) else {
            panic!("the other session is active");
        };
        other_state.runtime = runtime(&stops);

        let failure = smol::block_on(close_session(*state, &test.server.outbound));

        assert!(failure.is_none());
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert!(test.active(&other).runtime.guard.is_some());
    }
}
