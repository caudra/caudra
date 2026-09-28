//! The session's workflow runtime: one task that owns the store, answers
//! control requests, and launches one driver per execution attempt. Runs
//! outlive agent turns, so nothing here is tied to the agent loop's
//! cancellation: the runtime has its own root token, and every run's root is
//! a child of it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use arc_swap::ArcSwap;
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::paths::config_dir;
use caudra_storage::workflow::{
    MAX_HISTORY_RUNS, WorkflowCallKind, WorkflowCallRow, WorkflowCallState, WorkflowRunPatch,
    WorkflowRunRow, WorkflowRunStatus, WorkflowUpdate,
};
use caudra_workflow::{
    CallKey, CallKind, CallState, DEFAULT_AGENT_BUDGET, Journal, JournalEntry, LaunchRequest,
    MAX_ACTIVE_RUNS, MAX_AGENT_BUDGET, RunCall, RunCallBody, RunDetail, RunHistoryEntry,
    RunSnapshot, RunStatus, RunUsage, SmokeResult, WORKFLOW_ABI_VERSION, WORKFLOW_LANGUAGE_VERSION,
    WorkflowError, WorkflowEvent, WorkflowOutcome, WorkflowRequest, WorkflowResponse,
    WorkflowState, call_body, call_preview, hash_request, validate,
};
use flume::Receiver;
use serde_json::Value;
use tracing::{info, warn};

use super::catalog::Catalog;
use super::handle::{Reply, RuntimeRequest, WorkflowHandle, WorkspaceRebind};
use super::run::{ActiveRun, RunEnv, RunSpec, launch};
use super::state::{
    Published, publish, restore_timeline, run_event, run_status, snapshot_from_row,
    stored_source_kind,
};
use super::store::WorkflowStore;
use crate::AgentMode;
use crate::agent::task_runner::{ModeResolver, TaskRunner};
use crate::background::BackgroundTasks;
use crate::background_reminder::RuntimeHealth;
use crate::cancel::{CancelMap, CancelToken, CancelTrigger};
use crate::decisions::Decisions;
use crate::types::{AgentEvent, Envelope, EventSender, WORKFLOW_EVENT_RUN_ID, WorkflowProvenance};

const OBJECTIVE_ARG: &str = "objective";
const QUERY_ARG: &str = "query";
const DISPLAY_NAME_SEPARATOR: &str = "-";
const FIRST_DUPLICATE_SUFFIX: u32 = 2;
const MODE_BUILD: &str = "build";
const MODE_READ_ONLY: &str = "read-only";
const MODE_PLAN: &str = "plan";
const RUN_MOVED_DURING_RESUME: &str = "workflow run changed while it was being resumed";
const RUN_ALREADY_DRIVEN: &str = "workflow run already has a driver";
const DRIVER_MISSING: &str = "active workflow run has no driver";
const PHASE_LIST_SEPARATOR: &str = ", ";
const DEFAULT_HISTORY_RUNS: usize = 20;
const CALL_LABEL_FIELD: &str = "label";
const CALL_PROMPT_FIELD: &str = "prompt";
const CALL_NAME_FIELD: &str = "name";
const INVALID_BACKGROUND_BINDING: &str =
    "workflow background binding must be unique and session-owned";
const STALE_ADMISSION: &str = "workflow admission belongs to an obsolete session generation";

pub struct RuntimeDeps {
    pub state_dir: StateDir,
    pub session_id: CaudraId,
    pub cwd: PathBuf,
    /// The user's config directory, whose `workflows/` scope the catalog
    /// scans. `None` uses the real one; tests point at a tempdir.
    pub user_config_dir: Option<PathBuf>,
    pub remote_project_context: Option<Arc<crate::remote_project_context::RemoteProjectContext>>,
    pub runner: Arc<dyn TaskRunner>,
    pub events: flume::Sender<Envelope>,
    /// Read when each agent starts, so the user's current mode caps it.
    pub mode: ModeResolver,
    /// The map the runner's host context registers workflow agents in. It is
    /// shared by every run of the session, so per-run cancellation goes
    /// through the run's own root token; `cancel_all` fires only at shutdown.
    pub subagent_cancels: Arc<CancelMap<String>>,
}

pub struct WorkflowRuntime {
    handle: WorkflowHandle,
    task: smol::Task<()>,
    root: CancelTrigger,
    subagent_cancels: Arc<CancelMap<String>>,
    #[cfg(test)]
    store: WorkflowStore,
}

impl WorkflowRuntime {
    /// Opens the store, marks every run the previous process left active as
    /// interrupted, publishes the session's history, and starts serving.
    pub async fn spawn(
        deps: RuntimeDeps,
        decisions: Option<Decisions>,
    ) -> Result<Self, WorkflowError> {
        let store = WorkflowStore::spawn(deps.state_dir.clone(), deps.session_id)?;
        let interrupted = store.interrupt_active().await?;
        if interrupted > 0 {
            info!(interrupted, "workflow runs lost with the previous process");
        }
        for (run_id, revision) in store.pending_outbox().await? {
            store.ack_outbox(run_id, revision).await?;
        }
        let rows = store.load_runs().await?;
        let mut runs = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut snapshot = snapshot_from_row(row);
            restore_timeline(&mut snapshot, &store.load_events(row.run_id.clone()).await?);
            runs.push(snapshot);
        }
        let published: Published = Arc::new(ArcSwap::from_pointee(WorkflowState { runs }));
        let (requests, inbox) = flume::unbounded();
        let handle = WorkflowHandle::new(requests, Arc::clone(&published));
        #[cfg(test)]
        let test_store = store.clone();
        let (root_trigger, root) = CancelToken::new();
        let subagent_cancels = Arc::clone(&deps.subagent_cancels);
        let user_config_dir = deps.user_config_dir.or_else(|| config_dir().ok());
        Catalog::ensure_user_scope(user_config_dir.as_deref());
        let manager = Manager {
            state_dir: deps.state_dir,
            session_id: deps.session_id,
            cwd: deps.cwd,
            user_config_dir,
            remote_project_context: deps.remote_project_context,
            env: RunEnv {
                store,
                decisions,
                runner: deps.runner,
                events: deps.events,
                mode: deps.mode,
                published,
                health: Arc::clone(&handle.health),
            },
            root,
            active: HashMap::new(),
            suspended: None,
            pending_workspace: None,
            background: Arc::clone(&handle.background),
            health: Arc::clone(&handle.health),
        };
        Ok(Self {
            handle,
            task: smol::spawn(manager.serve(inbox)),
            root: root_trigger,
            subagent_cancels,
            #[cfg(test)]
            store: test_store,
        })
    }

    pub fn handle(&self) -> WorkflowHandle {
        self.handle.clone()
    }

    /// Interrupts every active run, waits for its agents to stop, closes the
    /// store, and joins the runtime task.
    pub async fn shutdown(self) {
        let _ = self.handle.request(WorkflowRequest::Shutdown).await;
        self.root.cancel();
        self.subagent_cancels.cancel_all();
        self.task.await;
    }
}

struct Manager {
    state_dir: StateDir,
    session_id: CaudraId,
    cwd: PathBuf,
    user_config_dir: Option<PathBuf>,
    remote_project_context: Option<Arc<crate::remote_project_context::RemoteProjectContext>>,
    env: RunEnv,
    root: CancelToken,
    active: HashMap<String, ActiveRun>,
    suspended: Option<Arc<()>>,
    pending_workspace: Option<(Arc<dyn TaskRunner>, WorkspaceRebind)>,
    background: Arc<OnceLock<BackgroundTasks>>,
    health: Arc<ArcSwap<RuntimeHealth>>,
}

impl Manager {
    /// Serves until `Shutdown` arrives or every handle is gone; either way
    /// the runs are interrupted and the store closed before the task ends.
    async fn serve(mut self, inbox: Receiver<(RuntimeRequest, Reply)>) {
        while let Ok((request, reply)) = inbox.recv_async().await {
            if matches!(
                request,
                RuntimeRequest::Workflow(WorkflowRequest::Shutdown, _)
            ) {
                self.shutdown().await;
                let _ = reply.send(Ok(WorkflowResponse::Ack));
                return;
            }
            let response = match request {
                #[cfg(test)]
                RuntimeRequest::Park(entered, release) => {
                    let _ = entered.send(());
                    let _ = release.recv_async().await;
                    Ok(WorkflowResponse::Ack)
                }
                RuntimeRequest::ReceivedCompletion(origin) => self
                    .env
                    .store
                    .received_completion(origin.run_id, origin.revision)
                    .await
                    .map(WorkflowResponse::Acked),
                RuntimeRequest::BindBackground(background) => {
                    if background.session_id() != self.session_id {
                        Err(internal(INVALID_BACKGROUND_BINDING))
                    } else {
                        self.background
                            .set(background)
                            .map(|_| WorkflowResponse::Ack)
                            .map_err(|_| internal(INVALID_BACKGROUND_BINDING))
                    }
                }
                RuntimeRequest::Workflow(request, generation) => {
                    self.handle(request, generation).await
                }
                RuntimeRequest::Suspend(token) => {
                    if self.suspended.is_some()
                        || self
                            .env
                            .published
                            .load()
                            .runs
                            .iter()
                            .any(|run| run.status == RunStatus::Active)
                    {
                        Err(internal(
                            "workflow runs must be quiescent before changing workspace",
                        ))
                    } else {
                        for (_, run) in std::mem::take(&mut self.active) {
                            run.task.await;
                        }
                        self.suspended = Some(token);
                        self.health.store(Arc::new(RuntimeHealth::Stopping));
                        Ok(WorkflowResponse::Ack)
                    }
                }
                RuntimeRequest::Release(token) => {
                    if self
                        .suspended
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &token))
                    {
                        self.pending_workspace = None;
                        self.suspended = None;
                        self.health.store(Arc::new(RuntimeHealth::Current));
                    }
                    Ok(WorkflowResponse::Ack)
                }
                RuntimeRequest::Commit(token) => {
                    if self
                        .suspended
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &token))
                        && let Some((runner, workspace)) = self.pending_workspace.take()
                    {
                        self.env.runner = runner;
                        self.cwd = workspace.cwd.into();
                        self.remote_project_context = Some(workspace.context);
                        Ok(WorkflowResponse::Ack)
                    } else {
                        Err(internal("workspace transition was not prepared"))
                    }
                }
                RuntimeRequest::Rebind(token, workspace) => {
                    if self
                        .suspended
                        .as_ref()
                        .is_none_or(|current| !Arc::ptr_eq(current, &token))
                    {
                        Err(internal("workspace transition is not suspended"))
                    } else {
                        match self.env.runner.rebind_workspace(&workspace) {
                            Ok(runner) => {
                                self.pending_workspace = Some((runner, workspace));
                                Ok(WorkflowResponse::Ack)
                            }
                            Err(error) => Err(internal(error)),
                        }
                    }
                }
            };
            let _ = reply.send(response);
        }
        self.shutdown().await;
    }

    async fn handle(
        &mut self,
        request: WorkflowRequest,
        generation: Option<u64>,
    ) -> Result<WorkflowResponse, WorkflowError> {
        if self.suspended.is_some()
            && matches!(
                request,
                WorkflowRequest::Start(_) | WorkflowRequest::Resume { .. }
            )
        {
            return Err(internal("workspace transition in progress"));
        }
        let _admission = if matches!(
            request,
            WorkflowRequest::Start(_) | WorkflowRequest::Resume { .. }
        ) {
            match self.background.get() {
                Some(background) => {
                    let guard = background.workflow_admission().await.map_err(internal)?;
                    if generation != Some(background.generation()) {
                        return Err(internal(STALE_ADMISSION));
                    }
                    Some(guard)
                }
                None => None,
            }
        } else {
            None
        };
        match request {
            WorkflowRequest::List => Ok(WorkflowResponse::Catalog(self.scan().await.to_catalog())),
            WorkflowRequest::Validate { name } => self.validate(name).await,
            WorkflowRequest::Start(launch) => self.start(launch).await,
            WorkflowRequest::Status { run_id } => self.status(run_id.as_deref()),
            WorkflowRequest::Inspect { run_id } => self.inspect(&run_id).await,
            WorkflowRequest::CallBodies { run_id, call_key } => {
                self.call_bodies(&run_id, call_key).await
            }
            WorkflowRequest::History { limit } => self.history(limit).await,
            WorkflowRequest::Pause { run_id } => self.interrupt(&run_id, RunStatus::Paused).await,
            WorkflowRequest::Stop { run_id } => self.interrupt(&run_id, RunStatus::Cancelled).await,
            WorkflowRequest::Resume {
                run_id,
                agent_budget,
            } => self.resume(run_id, agent_budget).await,
            WorkflowRequest::Trust { name, digest } => {
                self.scan().await.trust(&self.state_dir, &name, &digest)?;
                Ok(WorkflowResponse::Trusted { name })
            }
            WorkflowRequest::AckCompletion { run_id, revision } => self.ack(run_id, revision).await,
            WorkflowRequest::Shutdown => Ok(WorkflowResponse::Ack),
        }
    }

    async fn scan(&self) -> Catalog {
        let state_dir = self.state_dir.clone();
        if let Some(context) = &self.remote_project_context {
            return Catalog::scan_remote(&state_dir, context, self.user_config_dir.as_deref());
        }
        let cwd = self.cwd.clone();
        let user_config_dir = self.user_config_dir.clone();
        smol::unblock(move || Catalog::scan_with(&state_dir, &cwd, user_config_dir.as_deref()))
            .await
    }

    async fn validate(&self, name: String) -> Result<WorkflowResponse, WorkflowError> {
        let resolved = self.scan().await.resolve(&name)?;
        let report = smol::unblock(move || validate(&resolved.source)).await;
        let (ok, report) = match report {
            Ok(report) => (true, smoke_summary(&report.smoke)),
            Err(error) => (false, error.to_string()),
        };
        Ok(WorkflowResponse::Validation { name, ok, report })
    }

    async fn start(&mut self, launch: LaunchRequest) -> Result<WorkflowResponse, WorkflowError> {
        let resolved = self.scan().await.resolve(&launch.name)?;
        let source_label = resolved.source_label();
        if !resolved.trusted {
            return Err(WorkflowError::TrustRequired {
                name: launch.name,
                digest: resolved.digest,
                path: source_label,
            });
        }
        let agent_budget = launch.agent_budget.unwrap_or(DEFAULT_AGENT_BUDGET);
        check_budget(agent_budget)?;
        self.check_capacity()?;
        let run_id = CaudraId::generate().to_string();
        let launch_mode = mode_label(&(self.env.mode)());
        let row = WorkflowRunRow {
            run_id: run_id.clone(),
            session_id: self.session_id,
            display_name: self.unique_display_name(&resolved.meta.name),
            workflow_name: resolved.meta.name,
            source_kind: stored_source_kind(resolved.source_kind),
            source_path: Some(source_label),
            source_digest: resolved.digest,
            language_version: WORKFLOW_LANGUAGE_VERSION,
            abi_version: WORKFLOW_ABI_VERSION,
            source: resolved.source,
            args: launch.args.to_string(),
            objective: objective_of(&launch.args),
            launch_mode: launch_mode.to_owned(),
            status: WorkflowRunStatus::Active,
            pause_kind: None,
            pause_message: None,
            revision: 0,
            execution_epoch: 0,
            phase: None,
            agent_budget: u64::from(agent_budget),
            agents_admitted: 0,
            usage: json_text(&RunUsage::default()),
            roster: json_text(&Vec::<Value>::new()),
            result: None,
            error: None,
            outbox_pending: false,
            created_at: 0,
            updated_at: 0,
            bytes: 0,
        };
        self.env.store.insert_run(row).await?;
        let row = self.load_row(&run_id).await?;
        let snapshot = snapshot_from_row(&row);
        publish(&self.env.published, &snapshot);
        info!(run_id, workflow = %snapshot.workflow_name, agent_budget, launch_mode, "workflow run started");
        self.launch(
            snapshot.clone(),
            RunSpec {
                source: row.source,
                args: launch.args,
                journal: Journal::new(),
            },
        )?;
        Ok(WorkflowResponse::Started(Box::new(snapshot)))
    }

    async fn resume(
        &mut self,
        run_id: String,
        agent_budget: Option<u32>,
    ) -> Result<WorkflowResponse, WorkflowError> {
        let row = self.load_row(&run_id).await?;
        if row.session_id != self.session_id {
            return Err(WorkflowError::UnknownRun { run_id });
        }
        let status = run_status(row.status);
        let admitted = u32::try_from(row.agents_admitted).unwrap_or(u32::MAX);
        let budget_raised = agent_budget.is_some_and(|budget| budget > admitted);
        if !(status.is_resumable() || (status == RunStatus::BudgetLimited && budget_raised)) {
            return Err(WorkflowError::InvalidTransition { run_id, status });
        }
        if let Some(budget) = agent_budget {
            check_budget(budget)?;
        }
        self.check_capacity()?;
        let patch = WorkflowRunPatch {
            status: Some(WorkflowRunStatus::Active),
            execution_epoch: Some(row.execution_epoch + 1),
            agent_budget: agent_budget.map(u64::from),
            pause_kind: Some(None),
            pause_message: Some(None),
            result: Some(None),
            error: Some(None),
            outbox_pending: Some(false),
            ..WorkflowRunPatch::default()
        };
        let update = self
            .env
            .store
            .update_run(run_id.clone(), row.revision, row.execution_epoch, patch)
            .await?;
        if update == WorkflowUpdate::Stale {
            return Err(WorkflowError::Internal(RUN_MOVED_DURING_RESUME.to_owned()));
        }
        let row = self.load_row(&run_id).await?;
        let mut snapshot = snapshot_from_row(&row);
        restore_timeline(
            &mut snapshot,
            &self.env.store.load_events(run_id.clone()).await?,
        );
        publish(&self.env.published, &snapshot);
        let journal = self.journal(&run_id).await?;
        let args = serde_json::from_str(&row.args)
            .map_err(|error| WorkflowError::Internal(format!("stored workflow args: {error}")))?;
        info!(
            run_id,
            epoch = snapshot.execution_epoch,
            journaled = journal.len(),
            "workflow run resumed"
        );
        self.launch(
            snapshot.clone(),
            RunSpec {
                source: row.source,
                args,
                journal,
            },
        )?;
        Ok(WorkflowResponse::Run(Box::new(snapshot)))
    }

    /// Commits `status` under the next epoch, cancels the attempt, and waits
    /// for everything it started to stop before answering.
    async fn interrupt(
        &mut self,
        run_id: &str,
        status: RunStatus,
    ) -> Result<WorkflowResponse, WorkflowError> {
        let current = self.find(run_id)?;
        if status == RunStatus::Cancelled
            && matches!(current.status, RunStatus::Paused | RunStatus::BudgetLimited)
        {
            if let Some(ended) = self.active.remove(run_id) {
                ended.task.await;
            }
            let update = self
                .env
                .store
                .update_run(
                    run_id.to_owned(),
                    current.revision,
                    current.execution_epoch,
                    WorkflowRunPatch {
                        status: Some(WorkflowRunStatus::Cancelled),
                        execution_epoch: Some(current.execution_epoch + 1),
                        outbox_pending: Some(true),
                        ..WorkflowRunPatch::default()
                    },
                )
                .await?;
            let mut snapshot = snapshot_from_row(&self.load_row(run_id).await?);
            restore_timeline(
                &mut snapshot,
                &self.env.store.load_events(run_id.to_owned()).await?,
            );
            publish(&self.env.published, &snapshot);
            if update == WorkflowUpdate::Stale {
                return Err(WorkflowError::InvalidTransition {
                    run_id: run_id.to_owned(),
                    status: snapshot.status,
                });
            }
            EventSender::new(self.env.events.clone(), WORKFLOW_EVENT_RUN_ID)
                .with_workflow(WorkflowProvenance {
                    run_id: run_id.to_owned(),
                    epoch: snapshot.execution_epoch,
                    call_key: 0,
                    phase: snapshot.phase.clone(),
                })
                .try_send(AgentEvent::Workflow(Box::new(WorkflowEvent::Snapshot(
                    Box::new(snapshot.clone()),
                ))));
            return Ok(WorkflowResponse::Run(Box::new(snapshot)));
        }
        if current.status != RunStatus::Active {
            return Err(WorkflowError::InvalidTransition {
                run_id: run_id.to_owned(),
                status: current.status,
            });
        }
        let active = self
            .active
            .remove(run_id)
            .ok_or_else(|| WorkflowError::Internal(DRIVER_MISSING.to_owned()))?;
        let snapshot = match active.interrupt(status).await {
            Some(snapshot) => snapshot,
            None => snapshot_from_row(&self.load_row(run_id).await?),
        };
        publish(&self.env.published, &snapshot);
        if snapshot.status != status {
            return Err(WorkflowError::InvalidTransition {
                run_id: run_id.to_owned(),
                status: snapshot.status,
            });
        }
        info!(run_id, %status, epoch = snapshot.execution_epoch, "workflow run interrupted");
        Ok(WorkflowResponse::Run(Box::new(snapshot)))
    }

    fn status(&self, run_id: Option<&str>) -> Result<WorkflowResponse, WorkflowError> {
        match run_id {
            Some(run_id) => Ok(WorkflowResponse::Run(Box::new(self.find(run_id)?))),
            None => Ok(WorkflowResponse::Runs(
                self.env.published.load().runs.clone(),
            )),
        }
    }

    /// A run from any session with its journal and timeline. A live run is
    /// read from the published state so its in-flight roster shows.
    async fn inspect(&self, run_id: &str) -> Result<WorkflowResponse, WorkflowError> {
        let events = self.env.store.load_events(run_id.to_owned()).await?;
        let run = match self.find(run_id) {
            Ok(run) => run,
            Err(_) => {
                let mut run = snapshot_from_row(&self.load_row(run_id).await?);
                restore_timeline(&mut run, &events);
                run
            }
        };
        let calls: Vec<RunCall> = self
            .env
            .store
            .load_calls(run_id.to_owned())
            .await?
            .iter()
            .map(run_call)
            .collect();
        let journal_trimmed = calls.is_empty() && run.usage.agents_admitted > 0;
        Ok(WorkflowResponse::Detail(Box::new(RunDetail {
            run,
            calls,
            events: events.iter().map(run_event).collect(),
            journal_trimmed,
        })))
    }

    /// The untruncated text of one call, or of every call the run journaled.
    /// A key the run never recorded answers with nothing rather than an error,
    /// because a trimmed journal is a normal state and not a failure.
    async fn call_bodies(
        &self,
        run_id: &str,
        call_key: Option<u64>,
    ) -> Result<WorkflowResponse, WorkflowError> {
        let rows = match call_key {
            Some(key) => self
                .env
                .store
                .load_call(run_id.to_owned(), key)
                .await?
                .into_iter()
                .collect(),
            None => self.env.store.load_calls(run_id.to_owned()).await?,
        };
        Ok(WorkflowResponse::CallBodies(
            rows.iter().map(run_call_body).collect(),
        ))
    }

    async fn history(&self, limit: Option<usize>) -> Result<WorkflowResponse, WorkflowError> {
        let limit = limit.unwrap_or(DEFAULT_HISTORY_RUNS).min(MAX_HISTORY_RUNS);
        let rows = self.env.store.load_history(limit).await?;
        Ok(WorkflowResponse::History(
            rows.into_iter()
                .map(|row| RunHistoryEntry {
                    run: snapshot_from_row(&row.run),
                    session_id: row.run.session_id.to_string(),
                    session_title: row.session_title,
                })
                .collect(),
        ))
    }

    async fn ack(&self, run_id: String, revision: u64) -> Result<WorkflowResponse, WorkflowError> {
        let acked = self.env.store.ack_outbox(run_id.clone(), revision).await?;
        if acked {
            self.env.published.rcu(|state| {
                let mut runs = state.runs.clone();
                if let Some(run) = runs
                    .iter_mut()
                    .find(|run| run.run_id == run_id && run.revision == revision)
                {
                    run.outbox_pending = false;
                }
                WorkflowState { runs }
            });
        }
        Ok(WorkflowResponse::Acked(acked))
    }

    /// Interrupts what is still running and joins the drivers of runs that
    /// already ended, so nothing writes to the store after it closes.
    async fn shutdown(&mut self) {
        self.health.store(Arc::new(RuntimeHealth::Stopping));
        let run_ids: Vec<String> = self.active.keys().cloned().collect();
        for run_id in run_ids {
            let running = self
                .find(&run_id)
                .is_ok_and(|run| run.status == RunStatus::Active);
            if !running {
                if let Some(ended) = self.active.remove(&run_id) {
                    ended.task.await;
                }
            } else if let Err(error) = self.interrupt(&run_id, RunStatus::Interrupted).await {
                warn!(run_id, %error, "workflow run could not be interrupted at shutdown");
            }
        }
        self.env.store.clone().shutdown().await;
        self.health.store(Arc::new(RuntimeHealth::Closed));
    }

    fn launch(&mut self, snapshot: RunSnapshot, spec: RunSpec) -> Result<(), WorkflowError> {
        self.active.retain(|_, run| !run.task.is_finished());
        if self.active.contains_key(&snapshot.run_id) {
            return Err(WorkflowError::Internal(RUN_ALREADY_DRIVEN.to_owned()));
        }
        let run_id = snapshot.run_id.clone();
        let active = launch(self.env.clone(), snapshot, spec, &self.root);
        self.active.insert(run_id, active);
        Ok(())
    }

    async fn journal(&self, run_id: &str) -> Result<Journal, WorkflowError> {
        let calls = self.env.store.load_calls(run_id.to_owned()).await?;
        let mut journal = Journal::new();
        for call in calls {
            let (WorkflowCallState::Completed, Some(result)) = (call.state, call.result) else {
                continue;
            };
            let kind = call_kind(call.kind);
            // Decision inputs are omitted from the journal; their original hash still fences replay.
            let request_hash = if kind == CallKind::Decision {
                serde_json::from_value(Value::String(call.request_hash)).map_err(internal)?
            } else {
                let request: Value = serde_json::from_str(&call.request).map_err(internal)?;
                hash_request(kind, &request)
            };
            let result: Value = serde_json::from_str(&result).map_err(internal)?;
            journal
                .insert(
                    CallKey(call.call_key),
                    JournalEntry::new(kind, request_hash, result),
                )
                .map_err(internal)?;
        }
        Ok(journal)
    }

    async fn load_row(&self, run_id: &str) -> Result<WorkflowRunRow, WorkflowError> {
        self.env
            .store
            .load_run(run_id.to_owned())
            .await?
            .ok_or_else(|| WorkflowError::UnknownRun {
                run_id: run_id.to_owned(),
            })
    }

    fn find(&self, run_id: &str) -> Result<RunSnapshot, WorkflowError> {
        self.env
            .published
            .load()
            .runs
            .iter()
            .find(|run| run.run_id == run_id)
            .cloned()
            .ok_or_else(|| WorkflowError::UnknownRun {
                run_id: run_id.to_owned(),
            })
    }

    fn check_capacity(&self) -> Result<(), WorkflowError> {
        let active = self
            .env
            .published
            .load()
            .runs
            .iter()
            .filter(|run| run.status == RunStatus::Active)
            .count();
        if active >= MAX_ACTIVE_RUNS {
            return Err(WorkflowError::TooManyRuns {
                max: MAX_ACTIVE_RUNS,
            });
        }
        Ok(())
    }

    /// `name`, or `name-2`, `name-3`, ... once the session already has a run
    /// by that name.
    fn unique_display_name(&self, name: &str) -> String {
        let state = self.env.published.load();
        let taken = |candidate: &str| state.runs.iter().any(|run| run.display_name == candidate);
        if !taken(name) {
            return name.to_owned();
        }
        (FIRST_DUPLICATE_SUFFIX..)
            .map(|suffix| format!("{name}{DISPLAY_NAME_SEPARATOR}{suffix}"))
            .find(|candidate| !taken(candidate))
            .unwrap_or_else(|| name.to_owned())
    }
}

/// A journal row as the inspector shows it. The label is what the script
/// gave the agent, else the start of its prompt, else the scratch file name.
/// A string result is quoted bare, so a scratch call previews its path.
fn run_call(call: &WorkflowCallRow) -> RunCall {
    let request: Value = serde_json::from_str(&call.request).unwrap_or(Value::Null);
    let result_preview = call.result.as_deref().map(|text| {
        let value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        call_preview(value.as_str().unwrap_or(text))
    });
    let label = [CALL_LABEL_FIELD, CALL_NAME_FIELD, CALL_PROMPT_FIELD]
        .iter()
        .find_map(|field| request.get(field).and_then(Value::as_str))
        .map(|text| call_preview(text.lines().next().unwrap_or_default()));
    let prompt = request
        .get(CALL_PROMPT_FIELD)
        .and_then(Value::as_str)
        .map(call_preview);
    RunCall {
        call_key: call.call_key,
        kind: call_kind(call.kind),
        state: match call.state {
            WorkflowCallState::Started => CallState::Started,
            WorkflowCallState::Completed => CallState::Completed,
            WorkflowCallState::Failed => CallState::Failed,
        },
        label,
        prompt,
        task_id: call.task_id.clone(),
        tokens_used: call.tokens_used,
        duration_ms: call.duration_ms,
        started_at: call.started_at,
        finished_at: call.finished_at,
        result_preview,
        error: call.error.clone(),
    }
}

/// A journal row as a reader opens it: the request and result as stored, cut
/// only where a body has to end. A string result is unquoted, so a scratch
/// call reads as its path rather than as JSON.
fn run_call_body(call: &WorkflowCallRow) -> RunCallBody {
    RunCallBody {
        call_key: call.call_key,
        request: call_body(&pretty_json(&call.request)),
        result: call
            .result
            .as_deref()
            .map(|text| call_body(&pretty_json(text))),
        error: call.error.as_deref().map(call_body),
    }
}

/// Stored JSON re-rendered for reading. A bare string unwraps to itself and
/// anything unparseable is passed through untouched.
fn pretty_json(text: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return text.to_owned();
    };
    match value.as_str() {
        Some(text) => text.to_owned(),
        None => serde_json::to_string_pretty(&value).unwrap_or_else(|_| text.to_owned()),
    }
}

fn call_kind(kind: WorkflowCallKind) -> CallKind {
    match kind {
        WorkflowCallKind::Agent => CallKind::Agent,
        WorkflowCallKind::Parallel => CallKind::Parallel,
        WorkflowCallKind::ScratchFile => CallKind::ScratchFile,
        WorkflowCallKind::Decision => CallKind::Decision,
    }
}

fn check_budget(budget: u32) -> Result<(), WorkflowError> {
    if budget > MAX_AGENT_BUDGET {
        return Err(WorkflowError::Budget {
            requested: budget,
            max: MAX_AGENT_BUDGET,
        });
    }
    Ok(())
}

fn objective_of(args: &Value) -> Option<String> {
    args.get(OBJECTIVE_ARG)
        .or_else(|| args.get(QUERY_ARG))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn mode_label(mode: &AgentMode) -> &'static str {
    match mode {
        AgentMode::Build => MODE_BUILD,
        AgentMode::ReadOnly => MODE_READ_ONLY,
        AgentMode::Plan(_) | AgentMode::RemotePlan(_) => MODE_PLAN,
    }
}

fn smoke_summary(smoke: &SmokeResult) -> String {
    let outcome = match &smoke.outcome {
        WorkflowOutcome::Completed(_) => "completed".to_owned(),
        WorkflowOutcome::Paused { kind, .. } => format!("paused ({kind})"),
        WorkflowOutcome::Failed(error) => format!("failed: {error}"),
        WorkflowOutcome::Cancelled => "cancelled".to_owned(),
        WorkflowOutcome::BudgetLimited => "budget limited".to_owned(),
    };
    format!(
        "smoke run {outcome}; {} host calls; phases: {}",
        smoke.host_calls,
        smoke.phases_seen.join(PHASE_LIST_SEPARATOR)
    )
}

fn json_text<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .map(|value| value.to_string())
        .unwrap_or_else(|_| Value::Null.to_string())
}

fn internal(error: impl std::fmt::Display) -> WorkflowError {
    WorkflowError::Internal(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use caudra_config::decisions::DecisionsConfig;
    use caudra_decision::{DecisionEngine, DecisionError, DecisionRequest, DecisionResponse};
    use caudra_providers::{Message, WorkflowEventOrigin, expand_message};
    use caudra_storage::sessions::{SessionDatabase, SessionRelocation};
    use caudra_storage::workflow::{WorkflowCallState, WorkflowEventKind, WorkflowSourceKind};
    use caudra_workflow::RosterState;
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::StoredSession;
    use crate::agent::subagent::TaskIdentity;
    use crate::agent::task_runner::{TaskFuture, TaskOutcome, TaskRequest};
    use crate::subagent_history::{SubagentHistoryLease, SubagentHistoryStore};
    use crate::workflow::state::stored_status;

    const MODEL: &str = "test/model";
    const DUPLICATE_LABELS: &str = "duplicate-labels";
    const DUPLICATE_LABELS_BODY: &str = r#"
let first = agent("one", #{ label: "worker" });
let second = agent("two", #{ label: "worker" });
complete([first.output.echo, second.output.echo]);
"#;
    const DESCRIPTION: &str = "A test workflow";
    const USER_WORKFLOWS: &str = "workflows";
    const PROJECT_WORKFLOWS: &str = ".caudra/workflows";
    const SCRIPT_EXTENSION: &str = "rhai";
    const BLOCK_PREFIX: &str = "block-";
    const FAIL_PREFIX: &str = "fail-";
    const FAILURE: &str = "boom";
    const CANCELLED: &str = "cancelled";
    const TOKENS_PER_AGENT: u64 = 10;
    const PHASE: &str = "Work";
    const LOG_LINE: &str = "starting";
    const SCRATCH_FILE: &str = "notes.md";
    const SCRATCH_CONTENT: &str = "draft";
    const PAUSE_KIND: &str = "verification";
    const PAUSE_MESSAGE: &str = "check the first result";
    const UNKNOWN_RUN: &str = "no-such-run";
    const RELOCATED_PROJECT: &str = "relocated";
    /// The engine numbers a run's calls from one.
    const FIRST_KEY: u64 = 1;
    const SECOND_KEY: u64 = 2;
    const THIRD_KEY: u64 = 3;
    const UNKNOWN_KEY: u64 = 99;
    /// What `ECHO_BODY` asks its first agent for.
    const FIRST_PROMPT: &str = "hello";
    const FIRST_AGENT_LABEL: &str = "worker-1";
    const FIRST_PARALLEL_LABEL: &str = "worker-a";
    const SECOND_PARALLEL_LABEL: &str = "worker-b";
    const PROMPT_IS_CARRIED: &str = "a call row must say what its agent was asked";
    const ONE_BODY_IS_ONE_CALL: &str = "a keyed body request must answer for that call alone";
    const BODY_CARRIES_THE_REQUEST: &str = "a body must carry the request the journal stored";
    const ALL_BODIES_ARE_THE_JOURNAL: &str = "an unkeyed body request must answer for every call";
    const SCRATCH_BODY_IS_ITS_PATH: &str = "a scratch body must read as its path, not as JSON";
    const UNKNOWN_KEY_IS_EMPTY: &str = "a key the run never recorded must answer with nothing";

    const ECHO: &str = "echo";
    const ECHO_BODY: &str = r#"
phase("Work");
log("starting");
let first = agent("hello", #{ label: "worker-1" });
let path = write_scratch_file("notes.md", "draft");
complete(#{ echoed: first.output.echo, path: path });
"#;
    const SLOW: &str = "slow";
    const SLOW_BODY: &str = r#"
let first = agent("one", #{ label: "worker-1" });
let second = agent("two", #{ label: "block-2" });
complete([first.output.echo, second.output.echo]);
"#;
    const PAIR: &str = "pair";
    const PAIR_BODY: &str = r#"
let first = agent("one", #{ label: "worker-1" });
let second = agent("two", #{ label: "worker-2" });
complete([first.output.echo, second.output.echo]);
"#;
    const FAILING: &str = "failing";
    const FAILING_BODY: &str = r#"
let first = agent("one", #{ label: "worker-1" });
let second = agent("two", #{ label: "fail-2" });
complete([first.output.echo, second.output.echo]);
"#;
    const FANOUT: &str = "fanout";
    const FANOUT_BODY: &str = r#"
let results = parallel([
    #{ prompt: "a", label: "worker-a" },
    #{ prompt: "b", label: "worker-b" },
    #{ prompt: "c", label: "worker-c" },
]);
let echoes = [];
for result in results { echoes.push(result.output.echo); }
complete(echoes);
"#;
    const PAUSING: &str = "pausing";
    const PAUSING_BODY: &str = r#"
let first = agent("one", #{ label: "worker-1" });
pause("verification", "check the first result");
"#;
    const FRAGILE: &str = "fragile";
    const FRAGILE_BODY: &str = r#"
let note = "unset";
try { agent("x", #{ label: "fail-1" }); } catch (error) { note = "caught"; }
complete(note);
"#;
    const BLOCKING: &str = "blocking";
    const BLOCKING_BODY: &str = r#"
let first = agent("one", #{ label: "block-1" });
complete(first.output.echo);
"#;

    const EVENTS_CLOSED: &str = "the runtime dropped its event channel mid-run";
    const RUNNER_STOPPED: &str = "the runtime stopped before its agents started";
    const JOURNAL_REPLAYS: &str = "a journaled call must be replayed, not re-run";
    const ALL_OR_NOTHING: &str = "a batch the budget cannot hold must admit nothing";
    const INTERRUPT_LEAVES_NOTHING_RUNNING: &str =
        "an interrupted attempt must leave no agent running in the roster";
    const RESUME_BUMPS_EPOCH: &str = "every attempt must run under a fresh epoch";
    const ACK_IS_EXACT: &str = "an ack must clear only the revision it delivered";
    const NAMES_ARE_UNIQUE: &str = "two runs of one workflow must not share a display name";
    const OLD_HANDLE_IS_DEAD: &str = "a handle must report unavailable after shutdown";
    const TIMELINE_IS_KEPT: &str = "phases and log lines must be stored with the run";
    const SCRATCH_PREVIEW_IS_ITS_PATH: &str = "a scratch call previews the bare path it wrote";
    const HISTORY_IS_FOREIGN: &str = "history must list only other sessions' runs";
    const DECIDING: &str = "deciding";
    const DECISION_MODEL: &str = "workflow/model";
    const DECISION_SECRET: &str = "workflow-test-secret";
    const DECISION_ENDPOINT: &str = "http://127.0.0.1:8000/v1/systemone";
    const DECISION_TIMEOUT_MS: u64 = 60_000;
    const CONTROL_TEST_TIMEOUT: Duration = Duration::from_secs(5);
    const CONTROL_TIMEOUT_ERROR: &str = "workflow control waited for the decision deadline";
    const CAUGHT: &str = "caught";
    const DECIDING_BODY: &str = r#"
let result = decide(#{ command: "ls", api_key: "workflow-test-secret" }, #{ ready: #{ type: "noul", instructions: "Is it ready?" } }, #{ model: "workflow/model" });
agent("one", #{ label: "block-1" });
complete(result);
"#;
    const DECISION_FAILURE_BODY: &str = r#"
let result = "not caught";
try {
    decide(#{ command: "ls" }, #{ ready: #{ type: "noul", instructions: "Is it ready?" } });
} catch (error) { result = "caught"; }
complete(result);
"#;
    const DECISION_DEADLINE_BODY: &str = r#"
let result = "not caught";
try {
    decide(#{ command: "ls" }, #{ ready: #{ type: "noul", instructions: "Is it ready?" } }, #{ timeout_ms: 10 });
} catch (error) { result = "caught"; }
complete(result);
"#;

    struct FakeDecisionEngine {
        requests: Mutex<Vec<DecisionRequest>>,
        started: flume::Sender<()>,
        dropped: flume::Sender<()>,
        error: Option<DecisionError>,
        block: bool,
    }

    struct DecisionDrop(flume::Sender<()>);

    impl Drop for DecisionDrop {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    #[async_trait]
    impl DecisionEngine for FakeDecisionEngine {
        async fn decide(
            &self,
            request: &DecisionRequest,
            deadline: Instant,
        ) -> Result<DecisionResponse, DecisionError> {
            let _drop = DecisionDrop(self.dropped.clone());
            assert!(deadline <= Instant::now() + Duration::from_millis(DECISION_TIMEOUT_MS));
            self.requests.lock().unwrap().push(request.clone());
            let _ = self.started.send(());
            if self.block {
                futures_lite::future::pending::<()>().await;
            }
            if let Some(error) = &self.error {
                return Err(error.clone());
            }
            Ok(serde_json::from_value(json!({
                "answers": { "ready": { "type": "noul", "noul": 0.9, "confidence": 0.9 } },
                "usage": { "input_tokens": 1, "output_tokens": 1 },
            }))
            .unwrap())
        }
    }

    fn decision_service(
        fixture: &Fixture,
        error: Option<DecisionError>,
        block: bool,
    ) -> (
        Decisions,
        Arc<FakeDecisionEngine>,
        flume::Receiver<()>,
        flume::Receiver<()>,
    ) {
        let (started, entered) = flume::unbounded();
        let (dropped, ended) = flume::unbounded();
        let engine = Arc::new(FakeDecisionEngine {
            requests: Mutex::new(Vec::new()),
            started,
            dropped,
            error,
            block,
        });
        let config = DecisionsConfig {
            endpoint: Some(DECISION_ENDPOINT.parse().unwrap()),
            model: MODEL.into(),
            timeout_ms: DECISION_TIMEOUT_MS,
            ..DecisionsConfig::default()
        };
        let decisions =
            Decisions::with_engine(config, &fixture.state_dir, Arc::clone(&engine)).unwrap();
        (decisions, engine, entered, ended)
    }

    #[test_case(false; "resume")]
    #[test_case(true; "restart_without_service")]
    fn workflow_decision_is_committed_before_reply_and_replayed(restart: bool) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(DECIDING, DECIDING_BODY);
            let (decisions, engine, _, _) = decision_service(&fixture, None, false);
            let mut runtime = fixture.spawn_with_decisions(Some(decisions)).await;
            let handle = runtime.handle();
            let started = start(&handle, DECIDING, None).await;
            fixture.started().await;
            let call = runtime
                .store
                .load_call(started.run_id.clone(), FIRST_KEY)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(call.kind, WorkflowCallKind::Decision);
            assert_eq!(call.state, WorkflowCallState::Completed);
            assert!(!call.request.contains(DECISION_SECRET));
            let result: Value = serde_json::from_str(call.result.as_deref().unwrap()).unwrap();
            assert_eq!(result["model"], DECISION_MODEL);
            assert_eq!(result["answers"]["ready"]["noul"], 0.9);
            assert_eq!(engine.requests.lock().unwrap()[0].model, DECISION_MODEL);
            assert!(
                !engine.requests.lock().unwrap()[0]
                    .state
                    .to_string()
                    .contains(DECISION_SECRET)
            );
            let paused = run(
                &handle,
                WorkflowRequest::Pause {
                    run_id: started.run_id.clone(),
                },
            )
            .await;
            assert_eq!(paused.status, RunStatus::Paused);
            if restart {
                runtime.shutdown().await;
                runtime = fixture.spawn().await;
            }
            let handle = runtime.handle();
            resume(&handle, &started.run_id, None).await.unwrap();
            fixture.started().await;
            fixture.release.send(()).unwrap();
            let completed = fixture
                .wait_for(&started.run_id, RunStatus::Completed)
                .await;
            assert_eq!(completed.result, Some(result));
            assert_eq!(
                engine.requests.lock().unwrap().len(),
                1,
                "{JOURNAL_REPLAYS}"
            );
            runtime.shutdown().await;
        });
    }

    #[test_case(None; "missing_service")]
    #[test_case(Some(DecisionError::Unreachable); "unreachable")]
    #[test_case(Some(DecisionError::Timeout); "timeout")]
    #[test_case(Some(DecisionError::Invalid(FAILURE)); "invalid")]
    fn workflow_decision_failure_is_catchable(error: Option<DecisionError>) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(DECIDING, DECISION_FAILURE_BODY);
            let has_service = error.is_some();
            let decisions = error.map(|error| decision_service(&fixture, Some(error), false).0);
            let runtime = fixture.spawn_with_decisions(decisions).await;
            let started = start(&runtime.handle(), DECIDING, Some(0)).await;
            let completed = fixture
                .wait_for(&started.run_id, RunStatus::Completed)
                .await;
            assert_eq!(completed.result, Some(json!(CAUGHT)));
            assert_eq!(completed.usage.agents_admitted, 0);
            let call = runtime
                .store
                .load_call(started.run_id, FIRST_KEY)
                .await
                .unwrap();
            if has_service {
                let call = call.unwrap();
                assert_eq!(call.state, WorkflowCallState::Failed);
                assert!(call.result.is_none());
                assert!(!call.error.unwrap().contains(FAILURE));
            } else {
                assert!(call.is_none());
            }
            runtime.shutdown().await;
        });
    }

    #[test_case(RunStatus::Paused; "pause")]
    #[test_case(RunStatus::Cancelled; "stop")]
    #[test_case(RunStatus::Interrupted; "shutdown")]
    fn workflow_decision_cancellation_drops_pending_engine(status: RunStatus) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(DECIDING, DECISION_FAILURE_BODY);
            let (decisions, _, entered, ended) = decision_service(&fixture, None, true);
            let runtime = fixture.spawn_with_decisions(Some(decisions)).await;
            let handle = runtime.handle();
            let started = start(&handle, DECIDING, None).await;
            entered.recv_async().await.unwrap();
            let interrupt = async {
                match status {
                    RunStatus::Paused => {
                        run(
                            &handle,
                            WorkflowRequest::Pause {
                                run_id: started.run_id.clone(),
                            },
                        )
                        .await;
                        runtime.shutdown().await;
                    }
                    RunStatus::Cancelled => {
                        run(
                            &handle,
                            WorkflowRequest::Stop {
                                run_id: started.run_id.clone(),
                            },
                        )
                        .await;
                        runtime.shutdown().await;
                    }
                    _ => runtime.shutdown().await,
                }
                ended.recv_async().await.unwrap();
            };
            futures_lite::future::race(interrupt, async {
                smol::Timer::after(CONTROL_TEST_TIMEOUT).await;
                panic!("{CONTROL_TIMEOUT_ERROR}");
            })
            .await;
            let runtime = fixture.spawn().await;
            let store = &runtime.store;
            let row = store
                .load_run(started.run_id.clone())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.status, stored_status(status));
            let call = store
                .load_call(started.run_id, FIRST_KEY)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(call.state, WorkflowCallState::Failed);
            assert!(call.result.is_none());
            runtime.shutdown().await;
        });
    }

    #[test_case(true; "pending_engine")]
    fn workflow_decision_enforces_script_deadline(block: bool) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(DECIDING, DECISION_DEADLINE_BODY);
            let (decisions, _, _, _) = decision_service(&fixture, None, block);
            let runtime = fixture.spawn_with_decisions(Some(decisions)).await;
            let started = start(&runtime.handle(), DECIDING, None).await;
            let completed = futures_lite::future::race(
                fixture.wait_for(&started.run_id, RunStatus::Completed),
                async {
                    smol::Timer::after(CONTROL_TEST_TIMEOUT).await;
                    panic!("{CONTROL_TIMEOUT_ERROR}");
                },
            )
            .await;
            assert_eq!(completed.result, Some(json!(CAUGHT)));
            let call = runtime
                .store
                .load_call(started.run_id, FIRST_KEY)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(call.state, WorkflowCallState::Failed);
            runtime.shutdown().await;
        });
    }

    #[test_case(true; "stop_then_shutdown")]
    #[test_case(false; "shutdown_without_stop")]
    fn failed_terminal_commit_marks_cached_active_run_unavailable_with_manager_alive(stop: bool) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(BLOCKING, BLOCKING_BODY);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();
            let started = start(&handle, BLOCKING, None).await;
            fixture.started().await;
            assert_eq!(handle.reminder_snapshot().health, RuntimeHealth::Current);
            let (failed_tx, failed_rx) = flume::bounded(1);
            runtime.store.fail_next_terminal_update(failed_tx);
            fixture.release.send(()).unwrap();
            failed_rx.recv_async().await.unwrap();
            if stop {
                assert!(matches!(
                    handle
                        .request(WorkflowRequest::Stop {
                            run_id: started.run_id.clone()
                        })
                        .await,
                    Err(WorkflowError::InvalidTransition {
                        status: RunStatus::Active,
                        ..
                    })
                ));
                assert_eq!(
                    handle.reminder_snapshot().health,
                    RuntimeHealth::Unavailable
                );
            }
            let cached = run(
                &handle,
                WorkflowRequest::Status {
                    run_id: Some(started.run_id.clone()),
                },
            )
            .await;
            assert_eq!(cached.status, RunStatus::Active);
            assert!(handle.request(WorkflowRequest::List).await.is_ok());
            let persisted = runtime
                .store
                .load_run(started.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(persisted.status, WorkflowRunStatus::Active);
            runtime.shutdown().await;
            assert_eq!(handle.reminder_snapshot().health, RuntimeHealth::Closed);
        });
    }

    /// Answers each agent by its label: `block-*` parks until released or
    /// cancelled, `fail-*` fails without opening a session, anything else
    /// succeeds echoing its prompt. Every start is announced so a test can
    /// wait for an agent to be in flight without sleeping.
    struct FakeRunner {
        history: SubagentHistoryStore,
        started: flume::Sender<String>,
        release: flume::Receiver<()>,
        calls: Mutex<Vec<(String, Option<WorkflowProvenance>)>>,
    }

    impl FakeRunner {
        fn labels(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(label, _)| label.clone())
                .collect()
        }

        fn provenance(&self) -> Vec<WorkflowProvenance> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(_, provenance)| provenance.clone())
                .collect()
        }
    }

    impl TaskRunner for FakeRunner {
        fn reserve_task(
            &self,
            task_id: Option<&str>,
            label: &str,
        ) -> Result<SubagentHistoryLease, String> {
            match task_id {
                Some(id) if self.history.snapshot().records().contains_key(id) => self
                    .history
                    .continue_task(id)
                    .map_err(|error| error.to_string()),
                Some(id) => self.history.reserve_unconfigured(id),
                None => self.history.reserve_generated(label, |_| Ok(false)),
            }
        }

        fn run(
            &self,
            request: TaskRequest,
            cancel: CancelToken,
            events: EventSender,
        ) -> TaskFuture<'_> {
            Box::pin(async move {
                let outcome = async {
                    let label = request.label.clone();
                    self.calls
                        .lock()
                        .unwrap()
                        .push((label.clone(), events.workflow().cloned()));
                    let _ = self.started.send(label.clone());
                    let task_id = Some(request.task.requested().unwrap().to_owned());
                    if label.starts_with(BLOCK_PREFIX)
                        && cancel.race(self.release.recv_async()).await.is_err()
                    {
                        return TaskOutcome {
                            task_id,
                            mode: None,
                            success: false,
                            cancelled: true,
                            output: Value::Null,
                            error: Some(CANCELLED.to_owned()),
                            tokens_used: 0,
                            duration_ms: 0,
                        };
                    }
                    if label.starts_with(FAIL_PREFIX) {
                        return TaskOutcome {
                            task_id: None,
                            mode: None,
                            success: false,
                            cancelled: false,
                            output: Value::Null,
                            error: Some(FAILURE.to_owned()),
                            tokens_used: 0,
                            duration_ms: 0,
                        };
                    }
                    TaskOutcome {
                        task_id,
                        mode: None,
                        success: true,
                        cancelled: false,
                        output: json!({ "echo": request.prompt }),
                        error: None,
                        tokens_used: TOKENS_PER_AGENT,
                        duration_ms: 1,
                    }
                }
                .await;
                if let TaskIdentity::Reserved(lease) = request.task {
                    lease.complete(Vec::new());
                }
                outcome
            })
        }
    }

    struct BlockedReservationRunner {
        inner: Arc<FakeRunner>,
        entered: flume::Sender<()>,
        label: &'static str,
    }

    impl TaskRunner for BlockedReservationRunner {
        fn reserve_task(
            &self,
            task_id: Option<&str>,
            label: &str,
        ) -> Result<SubagentHistoryLease, String> {
            self.inner.reserve_task(task_id, label)
        }

        fn reserve_task_cancellable(
            &self,
            task_id: Option<&str>,
            label: &str,
            cancel: &CancelToken,
        ) -> Result<SubagentHistoryLease, String> {
            let lease = self.reserve_task(task_id, label)?;
            if label == self.label {
                self.entered.send(()).unwrap();
                smol::block_on(cancel.cancelled());
            }
            Ok(lease)
        }

        fn run(
            &self,
            request: TaskRequest,
            cancel: CancelToken,
            events: EventSender,
        ) -> TaskFuture<'_> {
            self.inner.run(request, cancel, events)
        }
    }

    struct Fixture {
        _temp: TempDir,
        state_dir: StateDir,
        session_id: CaudraId,
        project: PathBuf,
        config: PathBuf,
        runner: Arc<FakeRunner>,
        started: flume::Receiver<String>,
        release: flume::Sender<()>,
        events: flume::Receiver<Envelope>,
        events_tx: flume::Sender<Envelope>,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let project = temp.path().join("project");
            let config = temp.path().join("config");
            fs::create_dir(&project).unwrap();
            fs::create_dir(&config).unwrap();
            let state_dir = StateDir::from_path(temp.path().join("state"));
            let mut session = StoredSession::new(MODEL, project.to_string_lossy().as_ref());
            session.save(&state_dir).unwrap();
            let (started_tx, started) = flume::unbounded();
            let (release, release_rx) = flume::unbounded();
            let (events_tx, events) = flume::unbounded();
            Self {
                state_dir,
                session_id: session.id,
                project: project.canonicalize().unwrap(),
                config,
                runner: Arc::new(FakeRunner {
                    history: SubagentHistoryStore::default(),
                    started: started_tx,
                    release: release_rx,
                    calls: Mutex::new(Vec::new()),
                }),
                started,
                release,
                events,
                events_tx,
                _temp: temp,
            }
        }

        fn write(&self, dir: &Path, name: &str, body: &str) -> String {
            fs::create_dir_all(dir).unwrap();
            let source = format!(
                "let meta = #{{ name: \"{name}\", description: \"{DESCRIPTION}\" }};\n{body}"
            );
            fs::write(dir.join(format!("{name}.{SCRIPT_EXTENSION}")), &source).unwrap();
            source
        }

        fn user_workflow(&self, name: &str, body: &str) -> &Self {
            self.write(&self.config.join(USER_WORKFLOWS), name, body);
            self
        }

        fn project_workflow(&self, name: &str, body: &str) -> String {
            self.write(&self.project.join(PROJECT_WORKFLOWS), name, body)
        }

        async fn spawn(&self) -> WorkflowRuntime {
            self.spawn_as(self.session_id).await
        }

        fn save_receipt(&self, run: &RunSnapshot, compact: bool) {
            let mut database = SessionDatabase::open(&self.state_dir).unwrap();
            let mut session: StoredSession = database.load(self.session_id).unwrap();
            let message = Message::workflow_observation(
                FAILURE.into(),
                WorkflowEventOrigin {
                    run_id: run.run_id.clone(),
                    revision: run.revision,
                },
            );
            for item in expand_message(&message, session.messages().last().map(|item| item.id)) {
                session.push_message(item);
            }
            database.save(&session, None).unwrap();
            if compact {
                session.replace_messages(Vec::new());
                database.save(&session, None).unwrap();
            }
        }

        fn relocate(&mut self) {
            let destination = self._temp.path().join(RELOCATED_PROJECT);
            fs::create_dir(&destination).unwrap();
            let mut database = SessionDatabase::open(&self.state_dir).unwrap();
            let request = SessionRelocation {
                sessions: database.local_session_locations().unwrap(),
                source_cwd: Some(self.project.to_string_lossy().into_owned()),
                destination: destination.to_string_lossy().into_owned(),
                include_project_usage: true,
                keep_plan: false,
            };
            assert_eq!(
                database.relocate_sessions(&request).unwrap().sessions_moved,
                1
            );
            self.project = destination;
        }

        /// A second session in the same state directory, as another
        /// Caudra process on the same machine would have.
        fn new_session(&self) -> CaudraId {
            let mut session = StoredSession::new(MODEL, self.project.to_string_lossy().as_ref());
            session.save(&self.state_dir).unwrap();
            session.id
        }

        async fn spawn_as(&self, session_id: CaudraId) -> WorkflowRuntime {
            self.spawn_for(session_id, None).await
        }

        async fn spawn_with_decisions(&self, decisions: Option<Decisions>) -> WorkflowRuntime {
            self.spawn_for(self.session_id, decisions).await
        }

        async fn spawn_for(
            &self,
            session_id: CaudraId,
            decisions: Option<Decisions>,
        ) -> WorkflowRuntime {
            WorkflowRuntime::spawn(
                RuntimeDeps {
                    state_dir: self.state_dir.clone(),
                    session_id,
                    cwd: self.project.clone(),
                    user_config_dir: Some(self.config.clone()),
                    remote_project_context: None,
                    runner: Arc::clone(&self.runner) as Arc<dyn TaskRunner>,
                    events: self.events_tx.clone(),
                    mode: Arc::new(|| AgentMode::Build),
                    subagent_cancels: Arc::new(CancelMap::new()),
                },
                decisions,
            )
            .await
            .unwrap()
        }

        async fn started(&self) -> String {
            self.started.recv_async().await.expect(RUNNER_STOPPED)
        }

        /// The next published snapshot of `run_id` in `status`.
        async fn wait_for(&self, run_id: &str, status: RunStatus) -> RunSnapshot {
            loop {
                let envelope = self.events.recv_async().await.expect(EVENTS_CLOSED);
                let AgentEvent::Workflow(event) = &envelope.event else {
                    continue;
                };
                assert_eq!(envelope.run_id, WORKFLOW_EVENT_RUN_ID);
                if let WorkflowEvent::Snapshot(snapshot) = event.as_ref()
                    && snapshot.run_id == run_id
                    && snapshot.status == status
                {
                    return *snapshot.clone();
                }
            }
        }
    }

    async fn start(handle: &WorkflowHandle, name: &str, agent_budget: Option<u32>) -> RunSnapshot {
        match handle
            .request(WorkflowRequest::Start(LaunchRequest {
                name: name.into(),
                args: json!({}),
                agent_budget,
            }))
            .await
        {
            Ok(WorkflowResponse::Started(run)) => *run,
            other => panic!("expected a started run, got {other:?}"),
        }
    }

    async fn run(handle: &WorkflowHandle, request: WorkflowRequest) -> RunSnapshot {
        match handle.request(request).await {
            Ok(WorkflowResponse::Run(run)) => *run,
            other => panic!("expected a run, got {other:?}"),
        }
    }

    async fn resume(
        handle: &WorkflowHandle,
        run_id: &str,
        agent_budget: Option<u32>,
    ) -> Result<WorkflowResponse, WorkflowError> {
        handle
            .request(WorkflowRequest::Resume {
                run_id: run_id.into(),
                agent_budget,
            })
            .await
    }

    fn roster_states(run: &RunSnapshot) -> Vec<(u64, RosterState)> {
        run.roster
            .iter()
            .map(|entry| (entry.call_key, entry.state))
            .collect()
    }

    fn log_messages(run: &RunSnapshot) -> Vec<&str> {
        run.logs.iter().map(|line| line.message.as_str()).collect()
    }

    fn phase_titles(run: &RunSnapshot) -> Vec<&str> {
        run.phase_history
            .iter()
            .map(|record| record.title.as_str())
            .collect()
    }

    async fn run_history(handle: &WorkflowHandle) -> Vec<RunHistoryEntry> {
        match handle
            .request(WorkflowRequest::History { limit: None })
            .await
        {
            Ok(WorkflowResponse::History(entries)) => entries,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn repeated_workflow_labels_receive_distinct_journaled_ids() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(DUPLICATE_LABELS, DUPLICATE_LABELS_BODY);
            let runtime = fixture.spawn().await;
            let started = start(&runtime.handle(), DUPLICATE_LABELS, None).await;
            let done = fixture
                .wait_for(&started.run_id, RunStatus::Completed)
                .await;
            let ids: Vec<_> = done
                .roster
                .iter()
                .map(|entry| entry.task_id.as_deref().unwrap())
                .collect();
            assert_eq!(ids, ["worker", "worker-2"]);
            let store =
                WorkflowStore::spawn(fixture.state_dir.clone(), fixture.session_id).unwrap();
            let calls = store.load_calls(started.run_id.clone()).await.unwrap();
            assert_eq!(
                calls
                    .iter()
                    .map(|call| call.task_id.as_deref().unwrap())
                    .collect::<Vec<_>>(),
                ids
            );
        });
    }

    #[test]
    fn a_run_completes_and_journals_every_call() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(ECHO, ECHO_BODY);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();

            let started = start(&handle, ECHO, None).await;
            assert_eq!(started.status, RunStatus::Active);
            assert_eq!(started.workflow_name, ECHO);
            assert_eq!(started.agent_budget, DEFAULT_AGENT_BUDGET);
            assert_eq!(handle.active_count(), 1);
            let done = fixture
                .wait_for(&started.run_id, RunStatus::Completed)
                .await;

            let result = done.result.clone().unwrap();
            assert_eq!(result["echoed"], "hello");
            let scratch = Path::new(result["path"].as_str().unwrap());
            assert_eq!(scratch.file_name().unwrap(), SCRATCH_FILE);
            assert_eq!(fs::read_to_string(scratch).unwrap(), SCRATCH_CONTENT);
            assert_eq!(done.phase.as_deref(), Some(PHASE));
            assert_eq!(log_messages(&done), [LOG_LINE]);
            assert_eq!(phase_titles(&done), [PHASE], "{TIMELINE_IS_KEPT}");
            assert_eq!(done.usage.agents_admitted, 1);
            assert_eq!(done.usage.tokens_used, TOKENS_PER_AGENT);
            assert_eq!(roster_states(&done), [(FIRST_KEY, RosterState::Completed)]);
            assert!(done.outbox_pending);
            assert_eq!(handle.active_count(), 0);
            let provenance = fixture.runner.provenance();
            assert_eq!(provenance.len(), 1);
            assert_eq!(provenance[0].run_id, started.run_id);
            assert_eq!(provenance[0].epoch, 0);
            assert_eq!(provenance[0].call_key, FIRST_KEY);
            assert_eq!(provenance[0].phase.as_deref(), Some(PHASE));

            let store =
                WorkflowStore::spawn(fixture.state_dir.clone(), fixture.session_id).unwrap();
            let calls = store.load_calls(started.run_id.clone()).await.unwrap();
            let summary: Vec<(u64, WorkflowCallKind, WorkflowCallState)> = calls
                .iter()
                .map(|call| (call.call_key, call.kind, call.state))
                .collect();
            assert_eq!(
                summary,
                [
                    (
                        FIRST_KEY,
                        WorkflowCallKind::Agent,
                        WorkflowCallState::Completed
                    ),
                    (
                        SECOND_KEY,
                        WorkflowCallKind::ScratchFile,
                        WorkflowCallState::Completed
                    ),
                ]
            );
            assert_eq!(calls[0].task_id, done.roster[0].task_id);
            let task_id = calls[0].task_id.as_deref().unwrap();
            assert_eq!(task_id, "worker-1");
            let result: Value = serde_json::from_str(calls[0].result.as_deref().unwrap()).unwrap();
            assert_eq!(result["agent_id"], task_id);
            let events = store.load_events(started.run_id.clone()).await.unwrap();
            assert_eq!(
                events
                    .iter()
                    .map(|event| (event.kind, event.text.as_str()))
                    .collect::<Vec<_>>(),
                [
                    (WorkflowEventKind::Phase, PHASE),
                    (WorkflowEventKind::Log, LOG_LINE)
                ],
                "{TIMELINE_IS_KEPT}"
            );
            store.shutdown().await;
            runtime.shutdown().await;
        });
    }

    /// The timeline is what a restarted process shows for a settled run, so
    /// it must come back from the store rather than from memory.
    #[test]
    fn a_restarted_runtime_restores_the_timeline() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(ECHO, ECHO_BODY);
            let runtime = fixture.spawn().await;
            let started = start(&runtime.handle(), ECHO, None).await;
            fixture
                .wait_for(&started.run_id, RunStatus::Completed)
                .await;
            runtime.shutdown().await;

            let runtime = fixture.spawn().await;
            let restored = run(
                &runtime.handle(),
                WorkflowRequest::Status {
                    run_id: Some(started.run_id.clone()),
                },
            )
            .await;

            assert_eq!(log_messages(&restored), [LOG_LINE], "{TIMELINE_IS_KEPT}");
            assert_eq!(phase_titles(&restored), [PHASE], "{TIMELINE_IS_KEPT}");
            runtime.shutdown().await;
        });
    }

    #[test]
    fn inspect_answers_with_the_journal_and_timeline() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(ECHO, ECHO_BODY);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();
            let started = start(&handle, ECHO, None).await;
            fixture
                .wait_for(&started.run_id, RunStatus::Completed)
                .await;

            let detail = match handle
                .request(WorkflowRequest::Inspect {
                    run_id: started.run_id.clone(),
                })
                .await
            {
                Ok(WorkflowResponse::Detail(detail)) => detail,
                other => panic!("{other:?}"),
            };
            let unknown = handle
                .request(WorkflowRequest::Inspect {
                    run_id: UNKNOWN_RUN.into(),
                })
                .await;

            assert_eq!(detail.run.run_id, started.run_id);
            assert_eq!(
                detail
                    .calls
                    .iter()
                    .map(|call| (call.call_key, call.kind, call.state))
                    .collect::<Vec<_>>(),
                [
                    (FIRST_KEY, CallKind::Agent, CallState::Completed),
                    (SECOND_KEY, CallKind::ScratchFile, CallState::Completed),
                ]
            );
            assert_eq!(detail.calls[0].label.as_deref(), Some("worker-1"));
            assert!(detail.calls[0].result_preview.is_some());
            assert_eq!(detail.calls[1].label.as_deref(), Some(SCRATCH_FILE));
            assert_eq!(
                detail.calls[1].result_preview.as_deref(),
                detail.run.scratch_path(),
                "{SCRATCH_PREVIEW_IS_ITS_PATH}"
            );
            assert_eq!(detail.events.len(), 2, "{TIMELINE_IS_KEPT}");
            assert!(!detail.journal_trimmed);
            assert!(matches!(unknown, Err(WorkflowError::UnknownRun { .. })));
            assert_eq!(
                detail.calls[0].prompt.as_deref(),
                Some(FIRST_PROMPT),
                "{PROMPT_IS_CARRIED}"
            );
            runtime.shutdown().await;
        });
    }

    #[test]
    fn call_bodies_answer_for_one_call_or_for_all_of_them() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(ECHO, ECHO_BODY);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();
            let started = start(&handle, ECHO, None).await;
            fixture
                .wait_for(&started.run_id, RunStatus::Completed)
                .await;

            let one = call_bodies(&handle, &started.run_id, Some(FIRST_KEY)).await;
            let all = call_bodies(&handle, &started.run_id, None).await;
            let missing = call_bodies(&handle, &started.run_id, Some(UNKNOWN_KEY)).await;

            assert_eq!(
                one.iter().map(|body| body.call_key).collect::<Vec<_>>(),
                [FIRST_KEY],
                "{ONE_BODY_IS_ONE_CALL}"
            );
            assert!(
                one[0].request.contains(FIRST_PROMPT),
                "{BODY_CARRIES_THE_REQUEST}"
            );
            assert_eq!(
                all.iter().map(|body| body.call_key).collect::<Vec<_>>(),
                [FIRST_KEY, SECOND_KEY],
                "{ALL_BODIES_ARE_THE_JOURNAL}"
            );
            assert!(
                all[1]
                    .result
                    .as_deref()
                    .is_some_and(|path| path.ends_with(SCRATCH_FILE)),
                "{SCRATCH_BODY_IS_ITS_PATH}"
            );
            assert!(missing.is_empty(), "{UNKNOWN_KEY_IS_EMPTY}");
            runtime.shutdown().await;
        });
    }

    async fn call_bodies(
        handle: &WorkflowHandle,
        run_id: &str,
        call_key: Option<u64>,
    ) -> Vec<RunCallBody> {
        match handle
            .request(WorkflowRequest::CallBodies {
                run_id: run_id.to_owned(),
                call_key,
            })
            .await
        {
            Ok(WorkflowResponse::CallBodies(bodies)) => bodies,
            other => panic!("{other:?}"),
        }
    }

    /// History is for looking back at other sessions: this session's runs
    /// are already in the published state.
    #[test]
    fn history_lists_other_sessions_runs_only() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(ECHO, ECHO_BODY);
            let runtime = fixture.spawn().await;
            let started = start(&runtime.handle(), ECHO, None).await;
            fixture
                .wait_for(&started.run_id, RunStatus::Completed)
                .await;
            let own = run_history(&runtime.handle()).await;
            runtime.shutdown().await;

            let runtime = fixture.spawn_as(fixture.new_session()).await;
            let foreign = run_history(&runtime.handle()).await;

            assert!(own.is_empty(), "{HISTORY_IS_FOREIGN}");
            assert_eq!(foreign.len(), 1, "{HISTORY_IS_FOREIGN}");
            assert_eq!(foreign[0].run.run_id, started.run_id);
            assert_eq!(foreign[0].session_id, fixture.session_id.to_string());
            runtime.shutdown().await;
        });
    }

    #[test_case(RunStatus::Paused, false; "pause_sequential")]
    #[test_case(RunStatus::Cancelled, false; "stop_sequential")]
    #[test_case(RunStatus::Paused, true; "pause_parallel")]
    #[test_case(RunStatus::Cancelled, true; "stop_parallel")]
    fn interrupt_joins_blocked_reservation_without_starting_agent(
        interrupted: RunStatus,
        parallel: bool,
    ) {
        smol::block_on(async {
            let fixture = Fixture::new();
            let (name, body, first_label, blocked_label) = if parallel {
                (
                    FANOUT,
                    FANOUT_BODY,
                    FIRST_PARALLEL_LABEL,
                    SECOND_PARALLEL_LABEL,
                )
            } else {
                (PAIR, PAIR_BODY, FIRST_AGENT_LABEL, FIRST_AGENT_LABEL)
            };
            fixture.user_workflow(name, body);
            let (entered, waiting) = flume::bounded(1);
            let runtime = WorkflowRuntime::spawn(
                RuntimeDeps {
                    state_dir: fixture.state_dir.clone(),
                    session_id: fixture.session_id,
                    cwd: fixture.project.clone(),
                    user_config_dir: Some(fixture.config.clone()),
                    remote_project_context: None,
                    runner: Arc::new(BlockedReservationRunner {
                        inner: Arc::clone(&fixture.runner),
                        entered,
                        label: blocked_label,
                    }),
                    events: fixture.events_tx.clone(),
                    mode: Arc::new(|| AgentMode::Build),
                    subagent_cancels: Arc::new(CancelMap::new()),
                },
                None,
            )
            .await
            .unwrap();
            let handle = runtime.handle();
            let started = start(&handle, name, None).await;
            waiting.recv_async().await.unwrap();
            let request = match interrupted {
                RunStatus::Paused => WorkflowRequest::Pause {
                    run_id: started.run_id.clone(),
                },
                _ => WorkflowRequest::Stop {
                    run_id: started.run_id.clone(),
                },
            };
            let stopped = run(&handle, request).await;
            assert_eq!(stopped.status, interrupted);
            assert_eq!(stopped.execution_epoch, 1);
            assert!(stopped.roster.is_empty());
            assert!(fixture.runner.labels().is_empty());
            assert!(fixture.runner.history.snapshot().records().is_empty());
            for label in [first_label, blocked_label] {
                let lease = fixture.runner.reserve_task(None, label).unwrap();
                assert_eq!(lease.task_id(), label);
                drop(lease);
            }
            runtime.shutdown().await;
        });
    }

    #[test_case(RunStatus::Paused; "pause")]
    #[test_case(RunStatus::Cancelled; "stop")]
    fn an_interrupted_attempt_resumes_from_its_journal(interrupted: RunStatus) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(SLOW, SLOW_BODY);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();
            let started = start(&handle, SLOW, None).await;
            let run_id = started.run_id.clone();
            assert_eq!(fixture.started().await, "worker-1");
            assert_eq!(fixture.started().await, "block-2");

            let request = match interrupted {
                RunStatus::Paused => WorkflowRequest::Pause {
                    run_id: run_id.clone(),
                },
                _ => WorkflowRequest::Stop {
                    run_id: run_id.clone(),
                },
            };
            let stopped = run(&handle, request).await;

            assert_eq!(stopped.status, interrupted);
            assert_eq!(stopped.execution_epoch, 1, "{RESUME_BUMPS_EPOCH}");
            assert_eq!(
                roster_states(&stopped),
                [
                    (FIRST_KEY, RosterState::Completed),
                    (SECOND_KEY, RosterState::Cancelled)
                ],
                "{INTERRUPT_LEAVES_NOTHING_RUNNING}"
            );
            assert_eq!(fixture.runner.labels(), ["worker-1", "block-2"]);

            let resumed = match resume(&handle, &run_id, None).await {
                Ok(WorkflowResponse::Run(run)) => *run,
                other => panic!("expected the resumed run, got {other:?}"),
            };
            assert_eq!(resumed.status, RunStatus::Active);
            assert_eq!(resumed.execution_epoch, 2, "{RESUME_BUMPS_EPOCH}");
            assert_eq!(fixture.started().await, "block-2");
            fixture.release.send(()).unwrap();
            let done = fixture.wait_for(&run_id, RunStatus::Completed).await;

            assert_eq!(
                done.roster
                    .iter()
                    .map(|entry| &entry.task_id)
                    .collect::<Vec<_>>(),
                stopped
                    .roster
                    .iter()
                    .map(|entry| &entry.task_id)
                    .collect::<Vec<_>>()
            );

            assert_eq!(done.result, Some(json!(["one", "two"])));
            assert_eq!(
                fixture.runner.labels(),
                ["worker-1", "block-2", "block-2"],
                "{JOURNAL_REPLAYS}"
            );
            assert_eq!(
                roster_states(&done),
                [
                    (FIRST_KEY, RosterState::Completed),
                    (SECOND_KEY, RosterState::Completed)
                ]
            );
            runtime.shutdown().await;
        });
    }

    #[test_case(false; "active")]
    #[test_case(true; "paused")]
    fn foreign_runs_are_inspectable_but_cannot_be_controlled(paused: bool) {
        smol::block_on(async {
            let owner = Fixture::new();
            owner.user_workflow(BLOCKING, BLOCKING_BODY);
            let runtime = owner.spawn().await;
            let owned = runtime.handle();
            let started = start(&owned, BLOCKING, None).await;
            owner.started().await;
            if paused {
                owned
                    .request(WorkflowRequest::Pause {
                        run_id: started.run_id.clone(),
                    })
                    .await
                    .unwrap();
            }
            let mut requester = Fixture::new();
            requester.state_dir = owner.state_dir.clone();
            requester.session_id = requester.new_session();
            let foreign_runtime = requester.spawn().await;
            let foreign = foreign_runtime.handle();
            let database = SessionDatabase::open(&owner.state_dir).unwrap();
            let before = database
                .load_workflow_run(&started.run_id)
                .unwrap()
                .unwrap();
            let calls = owner.runner.labels();
            for request in [
                WorkflowRequest::Resume {
                    run_id: started.run_id.clone(),
                    agent_budget: None,
                },
                WorkflowRequest::Pause {
                    run_id: started.run_id.clone(),
                },
                WorkflowRequest::Stop {
                    run_id: started.run_id.clone(),
                },
            ] {
                assert_eq!(
                    foreign.request(request).await,
                    Err(WorkflowError::UnknownRun {
                        run_id: started.run_id.clone()
                    })
                );
                assert_eq!(
                    database
                        .load_workflow_run(&started.run_id)
                        .unwrap()
                        .unwrap(),
                    before
                );
                assert!(requester.runner.labels().is_empty());
                assert_eq!(owner.runner.labels(), calls);
            }
            assert!(matches!(
                foreign
                    .request(WorkflowRequest::Inspect {
                        run_id: started.run_id.clone()
                    })
                    .await
                    .unwrap(),
                WorkflowResponse::Detail(_)
            ));
            assert!(matches!(
                foreign
                    .request(WorkflowRequest::CallBodies {
                        run_id: started.run_id.clone(),
                        call_key: None
                    })
                    .await
                    .unwrap(),
                WorkflowResponse::CallBodies(_)
            ));
            assert_eq!(run_history(&foreign).await[0].run.run_id, started.run_id);
            assert_eq!(
                foreign
                    .request(WorkflowRequest::AckCompletion {
                        run_id: started.run_id.clone(),
                        revision: before.revision
                    })
                    .await,
                Ok(WorkflowResponse::Acked(false))
            );
            assert_eq!(
                database
                    .load_workflow_run(&started.run_id)
                    .unwrap()
                    .unwrap(),
                before
            );
            assert!(foreign.state().runs.is_empty());
            foreign_runtime.shutdown().await;
            runtime.shutdown().await;
        });
    }

    #[test]
    fn a_script_pause_is_reported_and_resumable() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(PAUSING, PAUSING_BODY);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();
            let started = start(&handle, PAUSING, None).await;

            let paused = fixture.wait_for(&started.run_id, RunStatus::Paused).await;

            assert_eq!(paused.pause_kind.as_deref(), Some(PAUSE_KIND));
            assert_eq!(paused.pause_message.as_deref(), Some(PAUSE_MESSAGE));
            assert_eq!(paused.execution_epoch, 0);
            let resumed = resume(&handle, &started.run_id, None).await.unwrap();
            assert!(matches!(resumed, WorkflowResponse::Run(_)));
            let paused_again = fixture.wait_for(&started.run_id, RunStatus::Paused).await;
            assert_eq!(paused_again.execution_epoch, 1, "{RESUME_BUMPS_EPOCH}");
            assert_eq!(fixture.runner.labels(), ["worker-1"], "{JOURNAL_REPLAYS}");
            runtime.shutdown().await;
        });
    }

    #[test_case(PAUSING, PAUSING_BODY, RunStatus::Paused, false; "paused")]
    #[test_case(PAIR, PAIR_BODY, RunStatus::BudgetLimited, false; "budget_limited")]
    #[test_case(PAUSING, PAUSING_BODY, RunStatus::Paused, true; "restarted_paused")]
    #[test_case(PAIR, PAIR_BODY, RunStatus::BudgetLimited, true; "restarted_budget_limited")]
    fn stop_settled_run_without_executing_or_raising_budget(
        name: &str,
        body: &str,
        status: RunStatus,
        restart: bool,
    ) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            fixture.user_workflow(name, body);
            let runtime = fixture.spawn().await;
            let started = start(&runtime.handle(), name, Some(1)).await;
            let settled = fixture.wait_for(&started.run_id, status).await;
            let runtime = if restart {
                runtime.shutdown().await;
                fixture.spawn().await
            } else {
                runtime
            };
            let handle = runtime.handle();
            assert_eq!(
                handle
                    .request(WorkflowRequest::Pause {
                        run_id: started.run_id.clone()
                    })
                    .await,
                Err(WorkflowError::InvalidTransition {
                    run_id: started.run_id.clone(),
                    status
                })
            );
            let stopped = run(
                &handle,
                WorkflowRequest::Stop {
                    run_id: started.run_id.clone(),
                },
            )
            .await;
            assert_eq!(stopped.status, RunStatus::Cancelled);
            assert_eq!(stopped.revision, settled.revision + 1);
            assert_eq!(stopped.execution_epoch, settled.execution_epoch + 1);
            assert_eq!(stopped.agent_budget, settled.agent_budget);
            assert_eq!(stopped.usage, settled.usage);
            assert_eq!(stopped.roster, settled.roster);
            assert!(stopped.outbox_pending);
            assert_eq!(
                fixture
                    .wait_for(&started.run_id, RunStatus::Cancelled)
                    .await,
                stopped
            );
            assert_eq!(fixture.runner.labels(), ["worker-1"]);
            runtime.shutdown().await;
            fixture.relocate();
            let runtime = fixture.spawn().await;
            assert_eq!(
                resume(&runtime.handle(), &started.run_id, None).await,
                Err(WorkflowError::InvalidTransition {
                    run_id: started.run_id,
                    status: RunStatus::Interrupted
                })
            );
            runtime.shutdown().await;
            assert_eq!(fixture.runner.labels(), ["worker-1"]);
        });
    }

    #[test_case(false; "revision_changed")]
    #[test_case(true; "epoch_changed")]
    fn stop_settled_run_rejects_stale_snapshot(advance_epoch: bool) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(PAUSING, PAUSING_BODY);
            let runtime = fixture.spawn().await;
            let started = start(&runtime.handle(), PAUSING, None).await;
            let paused = fixture.wait_for(&started.run_id, RunStatus::Paused).await;
            let database = SessionDatabase::open(&fixture.state_dir).unwrap();
            assert_eq!(
                database
                    .update_workflow_run(
                        &started.run_id,
                        paused.revision,
                        paused.execution_epoch,
                        &WorkflowRunPatch {
                            execution_epoch: advance_epoch.then_some(paused.execution_epoch + 1),
                            ..WorkflowRunPatch::default()
                        },
                    )
                    .unwrap(),
                WorkflowUpdate::Applied {
                    revision: paused.revision + 1
                }
            );
            let before = database.load_workflow_run(&started.run_id).unwrap();
            assert_eq!(
                runtime
                    .handle()
                    .request(WorkflowRequest::Stop {
                        run_id: started.run_id.clone()
                    })
                    .await,
                Err(WorkflowError::InvalidTransition {
                    run_id: started.run_id.clone(),
                    status: RunStatus::Paused
                })
            );
            assert_eq!(database.load_workflow_run(&started.run_id).unwrap(), before);
            runtime.shutdown().await;
            assert_eq!(fixture.runner.labels(), ["worker-1"]);
        });
    }

    #[test_case(false, false; "failed_same_workspace")]
    #[test_case(true, false; "cancelled_same_workspace")]
    #[test_case(false, true; "failed_relocated")]
    #[test_case(true, true; "cancelled_relocated")]
    fn fresh_runtime_resumes_terminal_runs_only_in_original_workspace(
        cancelled: bool,
        moved: bool,
    ) {
        smol::block_on(async {
            let mut fixture = Fixture::new();
            let (name, body) = if cancelled {
                (SLOW, SLOW_BODY)
            } else {
                (FAILING, FAILING_BODY)
            };
            fixture.user_workflow(name, body);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();
            let started = start(&handle, name, None).await;
            let settled = if cancelled {
                fixture.started().await;
                fixture.started().await;
                run(
                    &handle,
                    WorkflowRequest::Stop {
                        run_id: started.run_id.clone(),
                    },
                )
                .await
            } else {
                fixture.wait_for(&started.run_id, RunStatus::Failed).await
            };
            runtime.shutdown().await;
            let labels = fixture.runner.labels();
            if moved {
                fixture.relocate();
            }
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();
            if moved {
                for budget in [None, Some(MAX_AGENT_BUDGET)] {
                    assert_eq!(
                        resume(&handle, &started.run_id, budget).await,
                        Err(WorkflowError::InvalidTransition {
                            run_id: started.run_id.clone(),
                            status: RunStatus::Interrupted
                        })
                    );
                }
                runtime.shutdown().await;
                assert_eq!(fixture.runner.labels(), labels);
            } else {
                fixture.release.send(()).unwrap();
                let resumed = resume(&handle, &started.run_id, None).await.unwrap();
                let WorkflowResponse::Run(resumed) = resumed else {
                    panic!("{resumed:?}")
                };
                assert_eq!(resumed.status, RunStatus::Active);
                assert_eq!(resumed.execution_epoch, settled.execution_epoch + 1);
                fixture
                    .wait_for(
                        &started.run_id,
                        if cancelled {
                            RunStatus::Completed
                        } else {
                            RunStatus::Failed
                        },
                    )
                    .await;
                runtime.shutdown().await;
                assert_eq!(
                    fixture.runner.labels(),
                    [labels[0].clone(), labels[1].clone(), labels[1].clone()]
                );
            }
        });
    }

    #[test]
    fn a_budget_limited_run_needs_a_higher_budget_to_resume() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(PAIR, PAIR_BODY);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();
            let started = start(&handle, PAIR, Some(1)).await;
            let run_id = started.run_id.clone();

            let limited = fixture.wait_for(&run_id, RunStatus::BudgetLimited).await;

            assert_eq!(limited.usage.agents_admitted, 1);
            for budget in [None, Some(1)] {
                assert_eq!(
                    resume(&handle, &run_id, budget).await,
                    Err(WorkflowError::InvalidTransition {
                        run_id: run_id.clone(),
                        status: RunStatus::BudgetLimited,
                    })
                );
            }
            assert_eq!(
                resume(&handle, &run_id, Some(MAX_AGENT_BUDGET + 1)).await,
                Err(WorkflowError::Budget {
                    requested: MAX_AGENT_BUDGET + 1,
                    max: MAX_AGENT_BUDGET,
                })
            );
            resume(&handle, &run_id, Some(2)).await.unwrap();
            let done = fixture.wait_for(&run_id, RunStatus::Completed).await;
            assert_eq!(done.agent_budget, 2);
            assert_eq!(done.result, Some(json!(["one", "two"])));
            assert_eq!(
                fixture.runner.labels(),
                ["worker-1", "worker-2"],
                "{JOURNAL_REPLAYS}"
            );
            runtime.shutdown().await;
        });
    }

    #[test_case(3, RunStatus::Completed, 3; "held_by_the_budget")]
    #[test_case(2, RunStatus::BudgetLimited, 0; "over_the_budget")]
    fn a_parallel_batch_is_admitted_all_or_nothing(
        budget: u32,
        expected: RunStatus,
        admitted: u32,
    ) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(FANOUT, FANOUT_BODY);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();
            let started = start(&handle, FANOUT, Some(budget)).await;

            let ended = fixture.wait_for(&started.run_id, expected).await;

            assert_eq!(ended.usage.agents_admitted, admitted, "{ALL_OR_NOTHING}");
            assert_eq!(ended.roster.len(), admitted as usize, "{ALL_OR_NOTHING}");
            assert_eq!(
                fixture.runner.labels().len(),
                admitted as usize,
                "{ALL_OR_NOTHING}"
            );
            if expected == RunStatus::Completed {
                assert_eq!(ended.result, Some(json!(["a", "b", "c"])));
                assert_eq!(
                    roster_states(&ended),
                    [
                        (FIRST_KEY, RosterState::Completed),
                        (SECOND_KEY, RosterState::Completed),
                        (THIRD_KEY, RosterState::Completed)
                    ]
                );
            }
            runtime.shutdown().await;
        });
    }

    #[test]
    fn a_project_workflow_starts_only_once_trusted() {
        smol::block_on(async {
            let fixture = Fixture::new();
            let source = fixture.project_workflow(ECHO, ECHO_BODY);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();

            let refused = start_result(&handle, ECHO).await.unwrap_err();
            let WorkflowError::TrustRequired { name, digest, path } = refused else {
                panic!("expected trust to be required, got {refused:?}");
            };
            assert_eq!(name, ECHO);
            assert_eq!(fs::read_to_string(&path).unwrap(), source);
            let listed = handle.request(WorkflowRequest::List).await.unwrap();
            let WorkflowResponse::Catalog(catalog) = listed else {
                panic!("expected the catalog, got {listed:?}");
            };
            let entry = catalog
                .entries
                .iter()
                .find(|entry| entry.name == ECHO)
                .unwrap();
            assert!(!entry.trusted);
            assert_eq!(
                handle
                    .request(WorkflowRequest::Trust {
                        name: ECHO.into(),
                        digest,
                    })
                    .await,
                Ok(WorkflowResponse::Trusted { name: ECHO.into() })
            );

            let started = start(&handle, ECHO, None).await;

            assert_eq!(started.source_kind, caudra_workflow::SourceKind::Project);
            fixture
                .wait_for(&started.run_id, RunStatus::Completed)
                .await;
            runtime.shutdown().await;
        });
    }

    async fn start_result(
        handle: &WorkflowHandle,
        name: &str,
    ) -> Result<WorkflowResponse, WorkflowError> {
        handle
            .request(WorkflowRequest::Start(LaunchRequest {
                name: name.into(),
                args: json!({}),
                agent_budget: None,
            }))
            .await
    }

    #[test]
    fn active_runs_are_capped_and_shutdown_interrupts_them() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(BLOCKING, BLOCKING_BODY);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();
            let mut run_ids = Vec::new();
            for _ in 0..MAX_ACTIVE_RUNS {
                run_ids.push(start(&handle, BLOCKING, None).await.run_id);
                fixture.started().await;
            }

            assert_eq!(
                start_result(&handle, BLOCKING).await,
                Err(WorkflowError::TooManyRuns {
                    max: MAX_ACTIVE_RUNS
                })
            );
            assert_eq!(handle.active_count(), MAX_ACTIVE_RUNS);
            runtime.shutdown().await;

            assert_eq!(
                handle.request(WorkflowRequest::List).await,
                Err(WorkflowError::Unavailable),
                "{OLD_HANDLE_IS_DEAD}"
            );
            let reopened = fixture.spawn().await;
            let state = reopened.handle().state();
            assert_eq!(state.runs.len(), MAX_ACTIVE_RUNS);
            for run in &state.runs {
                assert!(run_ids.contains(&run.run_id));
                assert_eq!(run.status, RunStatus::Interrupted);
                assert_eq!(run.execution_epoch, 1, "{RESUME_BUMPS_EPOCH}");
                assert_eq!(
                    roster_states(run),
                    [(FIRST_KEY, RosterState::Cancelled)],
                    "{INTERRUPT_LEAVES_NOTHING_RUNNING}"
                );
            }
            reopened.shutdown().await;
        });
    }

    #[test]
    fn runs_left_active_by_a_dead_process_are_interrupted_on_spawn() {
        smol::block_on(async {
            let fixture = Fixture::new();
            let store =
                WorkflowStore::spawn(fixture.state_dir.clone(), fixture.session_id).unwrap();
            store
                .insert_run(WorkflowRunRow {
                    run_id: UNKNOWN_RUN.into(),
                    session_id: fixture.session_id,
                    display_name: ECHO.into(),
                    workflow_name: ECHO.into(),
                    source_kind: WorkflowSourceKind::User,
                    source_path: None,
                    source_digest: String::new(),
                    language_version: WORKFLOW_LANGUAGE_VERSION,
                    abi_version: WORKFLOW_ABI_VERSION,
                    source: String::new(),
                    args: json!({}).to_string(),
                    objective: None,
                    launch_mode: MODE_BUILD.into(),
                    status: WorkflowRunStatus::Active,
                    pause_kind: None,
                    pause_message: None,
                    revision: 0,
                    execution_epoch: 0,
                    phase: None,
                    agent_budget: u64::from(DEFAULT_AGENT_BUDGET),
                    agents_admitted: 0,
                    usage: json_text(&RunUsage::default()),
                    roster: json_text(&Vec::<Value>::new()),
                    result: None,
                    error: None,
                    outbox_pending: false,
                    created_at: 0,
                    updated_at: 0,
                    bytes: 0,
                })
                .await
                .unwrap();
            store.shutdown().await;

            let runtime = fixture.spawn().await;

            let recovered = run(
                &runtime.handle(),
                WorkflowRequest::Status {
                    run_id: Some(UNKNOWN_RUN.into()),
                },
            )
            .await;
            assert_eq!(recovered.status, RunStatus::Interrupted);
            assert_eq!(runtime.handle().active_count(), 0);
            runtime.shutdown().await;
        });
    }

    #[test]
    fn a_completion_is_acknowledged_exactly_once() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(ECHO, ECHO_BODY);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();
            let started = start(&handle, ECHO, None).await;
            let done = fixture
                .wait_for(&started.run_id, RunStatus::Completed)
                .await;
            assert_eq!(handle.pending_completions(), 1);

            let ack = |revision| {
                handle.request(WorkflowRequest::AckCompletion {
                    run_id: started.run_id.clone(),
                    revision,
                })
            };
            assert_eq!(
                ack(done.revision - 1).await,
                Ok(WorkflowResponse::Acked(false)),
                "{ACK_IS_EXACT}"
            );
            assert_eq!(handle.pending_completions(), 1, "{ACK_IS_EXACT}");
            assert_eq!(ack(done.revision).await, Ok(WorkflowResponse::Acked(false)));
            fixture.save_receipt(&done, false);
            assert_eq!(ack(done.revision).await, Ok(WorkflowResponse::Acked(true)));
            assert_eq!(handle.pending_completions(), 0);
            assert_eq!(handle.state().runs[0].logs, done.logs);
            assert_eq!(handle.state().runs[0].revision, done.revision);
            assert_eq!(
                ack(done.revision).await,
                Ok(WorkflowResponse::Acked(false)),
                "{ACK_IS_EXACT}"
            );
            runtime.shutdown().await;
        });
    }

    #[test]
    fn display_names_are_unique_within_the_session() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(ECHO, ECHO_BODY);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();

            let first = start(&handle, ECHO, None).await;
            let second = start(&handle, ECHO, None).await;

            assert_eq!(first.display_name, ECHO);
            assert_eq!(
                second.display_name,
                format!("{ECHO}{DISPLAY_NAME_SEPARATOR}{FIRST_DUPLICATE_SUFFIX}"),
                "{NAMES_ARE_UNIQUE}"
            );
            let listed = handle
                .request(WorkflowRequest::Status { run_id: None })
                .await
                .unwrap();
            let WorkflowResponse::Runs(runs) = listed else {
                panic!("expected every run, got {listed:?}");
            };
            let ids: Vec<&str> = runs.iter().map(|run| run.run_id.as_str()).collect();
            assert_eq!(ids, [second.run_id.as_str(), first.run_id.as_str()]);
            for run in [&first, &second] {
                fixture.wait_for(&run.run_id, RunStatus::Completed).await;
            }
            runtime.shutdown().await;
        });
    }

    #[test_case(false; "saved_history")]
    #[test_case(true; "compacted_history")]
    fn recovery_reconciles_saved_completion_without_reinjection(compact: bool) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(ECHO, ECHO_BODY);
            let runtime = fixture.spawn().await;
            let started = start(&runtime.handle(), ECHO, None).await;
            let done = fixture
                .wait_for(&started.run_id, RunStatus::Completed)
                .await;
            let origin = WorkflowEventOrigin {
                run_id: done.run_id.clone(),
                revision: done.revision,
            };
            assert!(
                !runtime
                    .handle()
                    .received_completion(origin.clone())
                    .await
                    .unwrap()
            );
            fixture.save_receipt(&done, compact);
            assert!(
                runtime
                    .handle()
                    .received_completion(origin.clone())
                    .await
                    .unwrap()
            );
            assert_eq!(runtime.handle().pending_completions(), 1);
            runtime.shutdown().await;
            let recovered = fixture.spawn().await;
            assert_eq!(recovered.handle().pending_completions(), 0);
            let restored = recovered.handle().state().runs[0].clone();
            assert_eq!(restored.revision, done.revision);
            assert_eq!(restored.execution_epoch, done.execution_epoch);
            assert!(
                recovered
                    .handle()
                    .received_completion(origin)
                    .await
                    .unwrap()
            );
            recovered.shutdown().await;
        });
    }

    #[test_case(false; "start")]
    #[test_case(true; "resume")]
    fn bound_admission_obeys_stop_rearm_and_workspace_reservation(resume: bool) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(BLOCKING, BLOCKING_BODY);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();
            let background = BackgroundTasks::spawn(fixture.state_dir.clone(), fixture.session_id)
                .await
                .unwrap();
            handle.bind_background(background.clone()).await.unwrap();
            let request = if resume {
                let started = start(&handle, BLOCKING, None).await;
                handle
                    .request(WorkflowRequest::Pause {
                        run_id: started.run_id.clone(),
                    })
                    .await
                    .unwrap();
                WorkflowRequest::Resume {
                    run_id: started.run_id,
                    agent_budget: None,
                }
            } else {
                WorkflowRequest::Start(LaunchRequest {
                    name: BLOCKING.into(),
                    args: json!({}),
                    agent_budget: None,
                })
            };
            let guard = background.workflow_admission().await.unwrap();
            let mut launch = Box::pin(handle.request(request.clone()));
            assert!(futures_lite::future::poll_once(&mut launch).await.is_none());
            let mut stop = Box::pin(background.stop());
            assert!(futures_lite::future::poll_once(&mut stop).await.is_none());
            background.rearm();
            drop(guard);
            assert!(launch.await.is_err());
            stop.await.unwrap();
            assert!(handle.request(request.clone()).await.is_err());
            let transition = background.suspend().unwrap();
            background.rearm();
            transition.drain().await.unwrap();
            assert!(handle.request(request.clone()).await.is_err());
            drop(transition);
            assert!(handle.request(request.clone()).await.is_err());
            background.rearm();
            let transition = background.suspend().unwrap();
            assert!(handle.request(request.clone()).await.is_err());
            drop(transition);
            assert!(handle.request(request).await.is_ok());
            runtime.shutdown().await;
            background.shutdown().await.unwrap();
        });
    }

    #[test]
    fn cancelled_launch_caller_does_not_abandon_manager_owned_admission() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(BLOCKING, BLOCKING_BODY);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();
            let background = BackgroundTasks::spawn(fixture.state_dir.clone(), fixture.session_id)
                .await
                .unwrap();
            handle.bind_background(background.clone()).await.unwrap();
            let guard = background.workflow_admission().await.unwrap();
            let mut launch = Box::pin(handle.request(WorkflowRequest::Start(LaunchRequest {
                name: BLOCKING.into(),
                args: json!({}),
                agent_budget: None,
            })));
            assert!(futures_lite::future::poll_once(&mut launch).await.is_none());
            drop(launch);
            drop(guard);
            handle
                .request(WorkflowRequest::Status { run_id: None })
                .await
                .unwrap();
            assert_eq!(handle.active_count(), 1);
            background.stop().await.unwrap();
            let snapshot = handle.state();
            assert_eq!(snapshot.runs.len(), 1);
            for run in &snapshot.runs {
                handle
                    .request(WorkflowRequest::Stop {
                        run_id: run.run_id.clone(),
                    })
                    .await
                    .unwrap();
            }
            assert_eq!(handle.active_count(), 0);
            runtime.shutdown().await;
            background.shutdown().await.unwrap();
        });
    }

    #[test]
    fn waiting_workflow_admission_cannot_cross_a_rearmed_generation() {
        smol::block_on(async {
            let fixture = Fixture::new();
            let background = BackgroundTasks::spawn(fixture.state_dir.clone(), fixture.session_id)
                .await
                .unwrap();
            let guard = background.workflow_admission().await.unwrap();
            let mut pending = Box::pin(background.workflow_admission());
            assert!(
                futures_lite::future::poll_once(&mut pending)
                    .await
                    .is_none()
            );
            background.suppress_wakes();
            background.rearm();
            drop(guard);
            assert!(pending.await.is_err());
            assert!(background.workflow_admission().await.is_ok());
            background.shutdown().await.unwrap();
        });
    }

    #[test_case(false; "start")]
    #[test_case(true; "resume")]
    fn queued_workflow_requests_keep_their_submission_generation(resume: bool) {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(BLOCKING, BLOCKING_BODY);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();
            let cloned_before_binding = handle.clone();
            let background = BackgroundTasks::spawn(fixture.state_dir.clone(), fixture.session_id)
                .await
                .unwrap();
            handle.bind_background(background.clone()).await.unwrap();
            let request = if resume {
                let started = start(&handle, BLOCKING, None).await;
                handle
                    .request(WorkflowRequest::Pause {
                        run_id: started.run_id.clone(),
                    })
                    .await
                    .unwrap();
                WorkflowRequest::Resume {
                    run_id: started.run_id,
                    agent_budget: None,
                }
            } else {
                WorkflowRequest::Start(LaunchRequest {
                    name: BLOCKING.into(),
                    args: json!({}),
                    agent_budget: None,
                })
            };
            let database = SessionDatabase::open(&fixture.state_dir).unwrap();
            let before = database.load_workflow_runs(fixture.session_id).unwrap();
            let calls = fixture.runner.labels();
            let (entered_tx, entered) = flume::bounded(1);
            let (release, release_rx) = flume::bounded(1);
            let mut parked = Box::pin(handle.park(entered_tx, release_rx));
            assert!(futures_lite::future::poll_once(&mut parked).await.is_none());
            entered.recv_async().await.unwrap();
            let mut queued = Box::pin(cloned_before_binding.request(request.clone()));
            assert!(futures_lite::future::poll_once(&mut queued).await.is_none());
            background.stop().await.unwrap();
            background.rearm();
            release.send(()).unwrap();
            parked.await.unwrap();
            assert_eq!(queued.await, Err(internal(STALE_ADMISSION)));
            assert_eq!(
                database.load_workflow_runs(fixture.session_id).unwrap(),
                before
            );
            assert_eq!(fixture.runner.labels(), calls);
            assert!(cloned_before_binding.request(request).await.is_ok());
            runtime.shutdown().await;
            background.shutdown().await.unwrap();
        });
    }

    #[test]
    fn transitions_are_checked_against_the_current_status() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture
                .user_workflow(ECHO, ECHO_BODY)
                .user_workflow(BLOCKING, BLOCKING_BODY);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();
            let done = start(&handle, ECHO, None).await;
            fixture.wait_for(&done.run_id, RunStatus::Completed).await;
            let blocked = start(&handle, BLOCKING, None).await;
            fixture.started().await;

            assert_eq!(
                handle
                    .request(WorkflowRequest::Pause {
                        run_id: done.run_id.clone()
                    })
                    .await,
                Err(WorkflowError::InvalidTransition {
                    run_id: done.run_id.clone(),
                    status: RunStatus::Completed,
                })
            );
            assert_eq!(
                resume(&handle, &blocked.run_id, None).await,
                Err(WorkflowError::InvalidTransition {
                    run_id: blocked.run_id.clone(),
                    status: RunStatus::Active,
                })
            );
            assert_eq!(
                handle
                    .request(WorkflowRequest::Stop {
                        run_id: UNKNOWN_RUN.into()
                    })
                    .await,
                Err(WorkflowError::UnknownRun {
                    run_id: UNKNOWN_RUN.into()
                })
            );
            runtime.shutdown().await;
        });
    }

    #[test]
    fn a_failed_agent_is_a_catchable_script_error() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(FRAGILE, FRAGILE_BODY);
            let runtime = fixture.spawn().await;
            let handle = runtime.handle();
            let started = start(&handle, FRAGILE, None).await;

            let done = fixture
                .wait_for(&started.run_id, RunStatus::Completed)
                .await;

            assert_eq!(done.result, Some(json!("caught")));
            assert_eq!(roster_states(&done), [(FIRST_KEY, RosterState::Failed)]);
            assert_eq!(done.logs.len(), 1);
            assert!(
                done.logs[0].message.contains(FAILURE),
                "{}",
                done.logs[0].message
            );
            runtime.shutdown().await;
        });
    }

    #[test]
    fn validation_smoke_runs_the_script() {
        smol::block_on(async {
            let fixture = Fixture::new();
            fixture.user_workflow(ECHO, ECHO_BODY);
            let runtime = fixture.spawn().await;

            let response = runtime
                .handle()
                .request(WorkflowRequest::Validate { name: ECHO.into() })
                .await
                .unwrap();

            let WorkflowResponse::Validation { name, ok, report } = response else {
                panic!("expected a validation, got {response:?}");
            };
            assert_eq!(name, ECHO);
            assert!(ok, "{report}");
            assert!(report.contains(PHASE), "{report}");
            assert_eq!(fixture.runner.labels().len(), 0);
            runtime.shutdown().await;
        });
    }
}
