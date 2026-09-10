//! The session's workflow runtime: one task that owns the store, answers
//! control requests, and launches one driver per execution attempt. Runs
//! outlive agent turns, so nothing here is tied to the agent loop's
//! cancellation: the runtime has its own root token, and every run's root is
//! a child of it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::paths::config_dir;
use caudra_storage::workflow::{
    WorkflowCallKind, WorkflowCallState, WorkflowRunPatch, WorkflowRunRow, WorkflowRunStatus,
    WorkflowUpdate,
};
use caudra_workflow::{
    CallKey, CallKind, DEFAULT_AGENT_BUDGET, Journal, JournalEntry, LaunchRequest, MAX_ACTIVE_RUNS,
    MAX_AGENT_BUDGET, RunSnapshot, RunStatus, RunUsage, SmokeResult, WORKFLOW_ABI_VERSION,
    WORKFLOW_LANGUAGE_VERSION, WorkflowError, WorkflowOutcome, WorkflowRequest, WorkflowResponse,
    WorkflowState, hash_request, validate,
};
use flume::Receiver;
use serde_json::Value;
use tracing::{info, warn};

use super::catalog::Catalog;
use super::handle::{Reply, WorkflowHandle};
use super::run::{ActiveRun, Interrupt, RunEnv, RunSpec, launch};
use super::state::{Published, publish, run_status, snapshot_from_row, stored_source_kind};
use super::store::WorkflowStore;
use crate::AgentMode;
use crate::agent::task_runner::{ModeResolver, TaskRunner};
use crate::cancel::{CancelMap, CancelToken, CancelTrigger};
use crate::types::Envelope;

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

pub struct RuntimeDeps {
    pub state_dir: StateDir,
    pub session_id: CaudraId,
    pub cwd: PathBuf,
    /// The user's config directory, whose `workflows/` scope the catalog
    /// scans. `None` uses the real one; tests point at a tempdir.
    pub user_config_dir: Option<PathBuf>,
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
}

impl WorkflowRuntime {
    /// Opens the store, marks every run the previous process left active as
    /// interrupted, publishes the session's history, and starts serving.
    pub async fn spawn(deps: RuntimeDeps) -> Result<Self, WorkflowError> {
        let store = WorkflowStore::spawn(deps.state_dir.clone(), deps.session_id)?;
        let interrupted = store.interrupt_active().await?;
        if interrupted > 0 {
            info!(interrupted, "workflow runs lost with the previous process");
        }
        let rows = store.load_runs().await?;
        let published: Published = Arc::new(ArcSwap::from_pointee(WorkflowState {
            runs: rows.iter().map(snapshot_from_row).collect(),
        }));
        let (requests, inbox) = flume::unbounded();
        let handle = WorkflowHandle::new(requests, Arc::clone(&published));
        let (root_trigger, root) = CancelToken::new();
        let subagent_cancels = Arc::clone(&deps.subagent_cancels);
        let user_config_dir = deps.user_config_dir.or_else(|| config_dir().ok());
        Catalog::ensure_user_scope(user_config_dir.as_deref());
        let manager = Manager {
            state_dir: deps.state_dir,
            session_id: deps.session_id,
            cwd: deps.cwd,
            user_config_dir,
            env: RunEnv {
                store,
                runner: deps.runner,
                events: deps.events,
                mode: deps.mode,
                published,
            },
            root,
            active: HashMap::new(),
        };
        Ok(Self {
            handle,
            task: smol::spawn(manager.serve(inbox)),
            root: root_trigger,
            subagent_cancels,
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
    env: RunEnv,
    root: CancelToken,
    active: HashMap<String, ActiveRun>,
}

impl Manager {
    /// Serves until `Shutdown` arrives or every handle is gone; either way
    /// the runs are interrupted and the store closed before the task ends.
    async fn serve(mut self, inbox: Receiver<(WorkflowRequest, Reply)>) {
        while let Ok((request, reply)) = inbox.recv_async().await {
            if matches!(request, WorkflowRequest::Shutdown) {
                self.shutdown().await;
                let _ = reply.send(Ok(WorkflowResponse::Ack));
                return;
            }
            let response = self.handle(request).await;
            let _ = reply.send(response);
        }
        self.shutdown().await;
    }

    async fn handle(
        &mut self,
        request: WorkflowRequest,
    ) -> Result<WorkflowResponse, WorkflowError> {
        match request {
            WorkflowRequest::List => Ok(WorkflowResponse::Catalog(self.scan().await.to_catalog())),
            WorkflowRequest::Validate { name } => self.validate(name).await,
            WorkflowRequest::Start(launch) => self.start(launch).await,
            WorkflowRequest::Status { run_id } => self.status(run_id.as_deref()),
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
        if !resolved.trusted {
            return Err(WorkflowError::TrustRequired {
                name: launch.name,
                digest: resolved.digest,
                path: resolved.path.unwrap_or_default(),
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
            source_path: resolved
                .path
                .map(|path| path.to_string_lossy().into_owned()),
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
        let snapshot = snapshot_from_row(&row);
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
        let (reply, answer) = flume::bounded(1);
        let mut snapshot = None;
        if active
            .control
            .send_async(Interrupt { status, reply })
            .await
            .is_ok()
        {
            snapshot = answer.recv_async().await.ok();
        }
        active.task.await;
        let snapshot = match snapshot {
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

    async fn ack(&self, run_id: String, revision: u64) -> Result<WorkflowResponse, WorkflowError> {
        let acked = self.env.store.ack_outbox(run_id.clone(), revision).await?;
        if acked {
            let snapshot = snapshot_from_row(&self.load_row(&run_id).await?);
            publish(&self.env.published, &snapshot);
        }
        Ok(WorkflowResponse::Acked(acked))
    }

    /// Interrupts what is still running and joins the drivers of runs that
    /// already ended, so nothing writes to the store after it closes.
    async fn shutdown(&mut self) {
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
            let kind = match call.kind {
                WorkflowCallKind::Agent => CallKind::Agent,
                WorkflowCallKind::Parallel => CallKind::Parallel,
                WorkflowCallKind::ScratchFile => CallKind::ScratchFile,
            };
            let request: Value = serde_json::from_str(&call.request).map_err(internal)?;
            let result: Value = serde_json::from_str(&result).map_err(internal)?;
            journal
                .insert(
                    CallKey(call.call_key),
                    JournalEntry::new(kind, hash_request(kind, &request), result),
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
        AgentMode::Plan(_) => MODE_PLAN,
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

    use caudra_storage::workflow::{WorkflowCallState, WorkflowSourceKind};
    use caudra_workflow::{RosterState, WorkflowEvent};
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::StoredSession;
    use crate::agent::task_runner::{TaskFuture, TaskOutcome, TaskRequest};
    use crate::types::{AgentEvent, EventSender, WORKFLOW_EVENT_RUN_ID, WorkflowProvenance};

    const MODEL: &str = "test/model";
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
    /// The engine numbers a run's calls from one.
    const FIRST_KEY: u64 = 1;
    const SECOND_KEY: u64 = 2;
    const THIRD_KEY: u64 = 3;

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

    /// Answers each agent by its label: `block-*` parks until released or
    /// cancelled, `fail-*` fails without opening a session, anything else
    /// succeeds echoing its prompt. Every start is announced so a test can
    /// wait for an agent to be in flight without sleeping.
    struct FakeRunner {
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
        fn run(
            &self,
            request: TaskRequest,
            cancel: CancelToken,
            events: EventSender,
        ) -> TaskFuture<'_> {
            Box::pin(async move {
                let label = request.label.clone();
                self.calls
                    .lock()
                    .unwrap()
                    .push((label.clone(), events.workflow().cloned()));
                let _ = self.started.send(label.clone());
                let task_id = Some(format!("task-{label}"));
                if label.starts_with(BLOCK_PREFIX)
                    && cancel.race(self.release.recv_async()).await.is_err()
                {
                    return TaskOutcome {
                        task_id,
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
                    success: true,
                    cancelled: false,
                    output: json!({ "echo": request.prompt }),
                    error: None,
                    tokens_used: TOKENS_PER_AGENT,
                    duration_ms: 1,
                }
            })
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
            WorkflowRuntime::spawn(RuntimeDeps {
                state_dir: self.state_dir.clone(),
                session_id: self.session_id,
                cwd: self.project.clone(),
                user_config_dir: Some(self.config.clone()),
                runner: Arc::clone(&self.runner) as Arc<dyn TaskRunner>,
                events: self.events_tx.clone(),
                mode: Arc::new(|| AgentMode::Build),
                subagent_cancels: Arc::new(CancelMap::new()),
            })
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
            assert_eq!(done.logs, [LOG_LINE]);
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
            assert_eq!(calls[0].task_id.as_deref(), Some("task-worker-1"));
            store.shutdown().await;
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
            assert_eq!(ack(done.revision).await, Ok(WorkflowResponse::Acked(true)));
            assert_eq!(handle.pending_completions(), 0);
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
            assert!(done.logs[0].contains(FAILURE), "{}", done.logs[0]);
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
