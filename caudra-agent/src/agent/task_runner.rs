//! The one path every delegated LLM task takes, whether the native `task`
//! tool or the workflow engine asked for it: the process-wide concurrency
//! permit, profile and mode resolution, the optional `output_schema`
//! contract with its nudges, and the cleanup that must run however the
//! subagent stopped.
//!
//! Structured output is a session-local tool. Its handler validates against
//! the caller's schema and captures the value, so invalid input becomes an
//! inline tool error the subagent can fix within the same run instead of a
//! failure the caller has to retry.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Instant;

use arc_swap::ArcSwap;
use async_lock::{Mutex as AsyncMutex, Semaphore};
use caudra_config::{ModelPolicy, ToolOutputLines};
use caudra_providers::model::{Model, ModelPurpose};
use caudra_providers::provider::Provider;
use caudra_providers::{RequestOptions, Timeouts, ToolNameAliases};
use caudra_storage::id::SessionRef;
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::sessions::RuntimeRetry;
use caudra_storage::tool_outputs::ToolOutputStore;
use caudra_workspace::WorkspaceSession;
use jsonschema::Validator;
use serde_json::Value;
use tracing::info;

use crate::agent::LoadedInstructions;
use crate::agent::run::AgentParams;
use crate::agent::subagent::{self, STRUCTURED_OUTPUT_TOOL, Subagent, TaskIdentity};
use crate::background::JobScope;
use crate::cancel::{CancelMap, CancelToken};
use crate::context::ContextPublisher;
use crate::mcp::McpSession;
use crate::permissions::PermissionManager;
use crate::prompt::ResolvedSlots;
use crate::prompt::profile::PromptProfileCatalog;
use crate::subagent_history::{
    SubagentHistoryLease, SubagentHistoryStore, SubagentTaskMode, SubagentTaskSpecCandidate,
};
use crate::template::Vars;
use crate::tools::registry::ToolRegistry;
use crate::tools::{
    Deadline, FileReadTracker, LocalToolFn, LocalTools, PathLocks, ToolAudience, ToolContext,
    ToolEffect, ToolFilter,
};
use crate::types::WorkflowProvenance;
use crate::workflow::WorkspaceRebind;
use crate::workspace_baseline::BaselineGate;
use crate::{AgentConfig, AgentMode, EventSender};

const STRUCTURED_OUTPUT_DESCRIPTION: &str =
    "Report your final result. Call it exactly once when your task is complete.";
const STRUCTURED_OUTPUT_ACK: &str = "Output recorded.";
const STRUCTURED_OUTPUT_PROMPT_SUFFIX: &str =
    "\n\nWhen finished, call the structured_output tool with your final result.";
const MAX_SCHEMA_ERRORS: usize = 3;
const SCHEMA_COMPILE_ERROR: &str = "invalid output_schema";
const SCHEMA_ROOT_ERROR: &str = "output_schema must have type object";
const STRUCTURED_MISSING_ERROR: &str = "subagent finished without calling structured_output";
const STRUCTURED_INVALID_ERROR: &str = "subagent result does not match output_schema";
const SUMMARY_MISSING_ERROR: &str = "subagent finished without providing a summary";
const INVALID_INPUT_PREFIX: &str =
    "Input does not match the required schema. Fix the errors and call structured_output again:\n";
const INTERRUPTED_PREFIX: &str = "sub-agent interrupted (";
const INTERRUPTED_SUFFIX: &str = "). Partial output:\n";
const ERROR_PREFIX: &str = "sub-agent error: ";
const RESERVATION_UNSUPPORTED: &str = "task runner does not support identity reservation; implement reserve_task using the session's shared history and durable identity namespace";

/// Process-wide cap on concurrently running subagents. Sized once from config
/// before any tool runs; `caudra_config::DEFAULT_TASK_MAX_CONCURRENT` until
/// then, so a test or embedder that never configures still gets a bound.
static PERMITS: LazyLock<ArcSwap<Semaphore>> = LazyLock::new(|| {
    ArcSwap::from_pointee(Semaphore::new(caudra_config::DEFAULT_TASK_MAX_CONCURRENT))
});

pub fn set_max_concurrent(limit: usize) {
    PERMITS.store(Arc::new(Semaphore::new(limit.max(1))));
}

/// One delegated task, as either caller describes it.
pub struct TaskRequest {
    /// `None` resumes a continuation with nothing new to say.
    pub prompt: Option<String>,
    /// The subagent's display name.
    pub label: String,
    pub task: TaskIdentity,
    /// `None` is `Plan` for a new task and the stored mode for a
    /// continuation. `Build` is clamped to `Plan` under a read-only parent.
    pub mode: Option<SubagentTaskMode>,
    /// `None` is the parent's default profile for a new task and the stored
    /// profile for a continuation.
    pub profile: Option<String>,
    /// Which model job runs this task. Reachable from a workflow script, which
    /// names a job rather than a model, and never from the `task` tool, whose
    /// API deliberately no longer takes a model.
    pub model_job: Option<ModelPurpose>,
    /// JSON Schema (object) the result must match. When set, `output` is the
    /// validated value the subagent reported through `structured_output`.
    pub output_schema: Option<Value>,
    /// Becomes the subagent's `parent_tool_use_id`, which roots its events
    /// and history in the caller's transcript.
    pub call_id: String,
    /// Stamped on every event the subagent emits.
    pub provenance: Option<WorkflowProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TaskOutcome {
    /// `None` when no subagent was opened, so there is nothing to resume.
    pub task_id: Option<String>,
    /// What the task ran as once the caller's request was defaulted and capped.
    /// `None` when no session opened.
    pub mode: Option<SubagentTaskMode>,
    pub success: bool,
    pub cancelled: bool,
    /// The validated structured value under an `output_schema`, else the
    /// final assistant text as `Value::String`. `Value::Null` on failure.
    pub output: Value,
    pub error: Option<String>,
    /// Input plus output tokens the subagent consumed during this call.
    pub tokens_used: u64,
    pub duration_ms: u64,
}

pub type TaskFuture<'a> = Pin<Box<dyn Future<Output = TaskOutcome> + Send + 'a>>;

pub trait TaskRunner: Send + Sync {
    fn reserve_task(
        &self,
        _task_id: Option<&str>,
        _label: &str,
    ) -> Result<SubagentHistoryLease, String> {
        Err(RESERVATION_UNSUPPORTED.into())
    }

    fn rebind_workspace(
        &self,
        _workspace: &WorkspaceRebind,
    ) -> Result<Arc<dyn TaskRunner>, String> {
        Err("task runner does not support workspace transitions".into())
    }

    fn reserve_task_cancellable(
        &self,
        task_id: Option<&str>,
        label: &str,
        cancel: &CancelToken,
    ) -> Result<SubagentHistoryLease, String> {
        if cancel.is_cancelled() {
            return Err(subagent::CANCELLED.into());
        }
        self.reserve_task(task_id, label)
    }

    fn run(&self, request: TaskRequest, cancel: CancelToken, events: EventSender)
    -> TaskFuture<'_>;
}

/// What the subagent reported through `structured_output`, plus the most
/// recent validation failure. Shared with the local tool's handler, which runs
/// on the subagent's turn while this call awaits it.
#[derive(Default)]
struct Captured {
    value: Option<Value>,
    last_errors: Option<String>,
}

/// Closes the session on every exit, so no early return can leave its cancel
/// registration or history lease behind.
struct OpenSession(Subagent);

impl Drop for OpenSession {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Why a task produced no result. Cancellation is told apart from every
/// other failure because a workflow treats the two differently.
struct Failure {
    message: String,
    cancelled: bool,
}

impl From<subagent::PromptFailure> for Failure {
    fn from(failure: subagent::PromptFailure) -> Self {
        Self {
            cancelled: failure.error == subagent::CANCELLED,
            message: failure_message(failure),
        }
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            message,
            cancelled: false,
        }
    }
}

/// Runs `request` to completion on the calling agent's context. The
/// subagent is rooted at `request.call_id` and its events carry
/// `request.provenance`, whatever the context says.
pub async fn run_task(ctx: &ToolContext, request: TaskRequest) -> TaskOutcome {
    let started = Instant::now();
    match PreparedTask::prepare(ctx, request, None).await {
        Ok(task) => task.run(&ctx.cancel).await,
        Err(message) => finish(started, None, None, 0, Err(message.into())),
    }
}

pub(crate) struct PreparedTask {
    session: OpenSession,
    prompt: Option<String>,
    validating: bool,
    captured: Arc<Mutex<Captured>>,
    reporter: Option<crate::background::TaskReporter>,
    started: Instant,
}

impl PreparedTask {
    pub(crate) async fn prepare(
        ctx: &ToolContext,
        request: TaskRequest,
        reporter: Option<crate::background::TaskReporter>,
    ) -> Result<Self, String> {
        let started = Instant::now();
        let ctx = &ToolContext {
            tool_use_id: Some(request.call_id),
            event_tx: match request.provenance {
                Some(provenance) => ctx.event_tx.clone().with_workflow(provenance),
                None => ctx.event_tx.clone(),
            },
            steering_observations: None,
            steering_order: Vec::new(),
            speculative: None,
            ..ctx.clone()
        };
        // Compile early: a bad schema costs zero tokens.
        let captured = Arc::new(Mutex::new(Captured::default()));
        // A validated report satisfies the output contract even without a prose tail.
        // It does not authorize the agent to suppress cancellation or runtime errors.
        let report_ready = Arc::new(AtomicBool::new(false));
        let validating = request.output_schema.is_some();
        let (mut local_definitions, mut local_tools) = match request.output_schema.as_ref() {
            None => (Vec::new(), LocalTools::default()),
            Some(schema) => match structured_output_tool(schema, &captured, &report_ready) {
                Ok((definition, tools)) => (vec![definition], tools),
                Err(message) => return Err(message),
            },
        };
        if let Some(reporter) = &reporter {
            let (definition, handler) =
                crate::tools::native::report_to_parent::tool(reporter.clone());
            local_definitions.push(definition);
            Arc::make_mut(&mut local_tools)
                .insert(crate::tools::native::report_to_parent::NAME.into(), handler);
        }
        let mode = clamp_mode(&request.task, request.mode, &ctx.mode);
        let mut session = match subagent::open_task(
            ctx,
            subagent::TaskOptions {
                name: request.label,
                task_id: request.task,
                profile: request.profile,
                mode,
                model_job: request.model_job,
                local_definitions,
                local_tools,
            },
        )
        .await
        {
            Ok(session) => OpenSession(session.with_report_ready(report_ready)),
            Err(message) => return Err(message),
        };
        if let Some(reporter) = &reporter {
            session.0.set_terminal_report(Arc::clone(&reporter.blocked));
        }
        Ok(Self {
            session,
            prompt: request.prompt,
            validating,
            captured,
            reporter,
            started,
        })
    }

    pub(crate) fn mode(&self) -> SubagentTaskMode {
        self.session.0.task_mode()
    }

    pub(crate) fn checkpoint(&self) -> Result<(Value, Value), String> {
        self.session.0.checkpoint()
    }

    pub(crate) fn discard(mut self) {
        self.session.0.discard_unstarted();
    }

    pub(crate) async fn run(self, cancel: &CancelToken) -> TaskOutcome {
        self.run_with_permits(cancel, PERMITS.load_full()).await
    }

    pub(crate) async fn run_with_permits(
        mut self,
        cancel: &CancelToken,
        permits: Arc<Semaphore>,
    ) -> TaskOutcome {
        let started = self.started;
        let session = &mut self.session;
        let task_id = session.0.id().to_owned();
        let effective_mode = session.0.task_mode();
        let permit = cancel.race(permits.acquire_arc()).await;
        let mut verdict = if let Ok(_permit) = permit {
            if let Some(reporter) = &self.reporter
                && let Err(error) = reporter.running().await
            {
                session.0.close();
                return finish(
                    started,
                    Some(task_id),
                    Some(effective_mode),
                    0,
                    Err(error.into()),
                );
            }
            converse(
                &mut session.0,
                self.prompt,
                self.validating,
                &self.captured,
                self.reporter.as_ref(),
            )
            .await
        } else {
            Err(cancelled_failure().into())
        };
        let usage = session.0.usage();
        let tokens_used = u64::from(usage.total_input()) + u64::from(usage.output);
        if let Err(error) = session.0.drain_jobs().await {
            verdict = Err(error.into());
        }
        session.0.close();
        finish(
            started,
            Some(task_id),
            Some(effective_mode),
            tokens_used,
            verdict,
        )
    }
}

fn finish(
    started: Instant,
    task_id: Option<String>,
    mode: Option<SubagentTaskMode>,
    tokens_used: u64,
    verdict: Result<Value, Failure>,
) -> TaskOutcome {
    let duration_ms = started.elapsed().as_millis() as u64;
    match verdict {
        Ok(output) => TaskOutcome {
            task_id,
            mode,
            success: true,
            cancelled: false,
            output,
            error: None,
            tokens_used,
            duration_ms,
        },
        Err(failure) => TaskOutcome {
            task_id,
            mode,
            success: false,
            cancelled: failure.cancelled,
            output: Value::Null,
            error: Some(failure.message),
            tokens_used,
            duration_ms,
        },
    }
}

/// A read-only parent cannot hand out a write it does not have, so a build
/// request under it runs as a plan. A continuation keeps its stored mode,
/// and `open_task` refuses one that would build from here.
fn clamp_mode(
    task: &TaskIdentity,
    requested: Option<SubagentTaskMode>,
    ceiling: &AgentMode,
) -> Option<SubagentTaskMode> {
    match (task.is_continuation(), requested, ceiling) {
        (
            false,
            Some(SubagentTaskMode::Build),
            AgentMode::ReadOnly | AgentMode::Plan(_) | AgentMode::RemotePlan(_),
        ) => {
            info!(ceiling = ?ceiling, "build task clamped to plan under a read-only parent");
            Some(SubagentTaskMode::Plan)
        }
        (_, requested, _) => requested,
    }
}

/// Prompts, then nudges a subagent that finished without reporting. A nudge
/// only makes sense while the run is healthy, so any error ends the loop
/// immediately.
async fn converse(
    session: &mut Subagent,
    prompt: Option<String>,
    validating: bool,
    captured: &Mutex<Captured>,
    reporter: Option<&crate::background::TaskReporter>,
) -> Result<Value, Failure> {
    let message = prompt.map(|mut message| {
        if validating {
            message.push_str(STRUCTURED_OUTPUT_PROMPT_SUFFIX);
        }
        message
    });
    let mut reply = session.prompt(message).await?;
    if let Some(reporter) = reporter.filter(|reporter| reporter.blocked.load(Ordering::Acquire)) {
        return Err(reporter.blocker().into());
    }
    // Only healthy runs with an unmet contract may request another response;
    // the shared steering state bounds both these prompts and inner repairs.
    while if validating {
        if !session.report_ready() {
            lock(captured).value = None;
        }
        lock(captured).value.is_none()
    } else {
        reply.text.trim().is_empty()
    } {
        let Some(corrected) = session
            .correct_report(validating)
            .await
            .map_err(|mut failure| {
                if failure.partial.is_none() && !reply.text.trim().is_empty() {
                    failure.partial = Some(reply.text.clone());
                }
                failure
            })?
        else {
            break;
        };
        reply = corrected;
        if let Some(reporter) = reporter.filter(|reporter| reporter.blocked.load(Ordering::Acquire))
        {
            return Err(reporter.blocker().into());
        }
    }

    Ok(report(
        validating,
        std::mem::take(&mut *lock(captured)),
        reply.text,
    )?)
}

fn cancelled_failure() -> subagent::PromptFailure {
    subagent::PromptFailure {
        error: subagent::CANCELLED.to_owned(),
        partial: None,
    }
}

/// A result alongside the error means the run was cut short after streaming
/// some text, and half a transcript beats a bare error.
fn failure_message(failure: subagent::PromptFailure) -> String {
    match failure.partial {
        Some(partial) => format!(
            "{INTERRUPTED_PREFIX}{}{INTERRUPTED_SUFFIX}{partial}",
            failure.error
        ),
        None => format!("{ERROR_PREFIX}{}", failure.error),
    }
}

/// The subagent's verdict, once it has stopped talking. A schema contract is
/// answered by `captured` alone; without one the transcript is the answer.
fn report(validating: bool, captured: Captured, text: String) -> Result<Value, String> {
    match (validating, captured.value) {
        (true, Some(value)) => Ok(value),
        (true, None) => Err(match captured.last_errors {
            Some(errors) => format!("{STRUCTURED_INVALID_ERROR}:\n{errors}"),
            None => STRUCTURED_MISSING_ERROR.to_owned(),
        }),
        (false, _) if text.trim().is_empty() || text == crate::EMPTY_RESPONSE_MARKER => {
            Err(SUMMARY_MISSING_ERROR.to_owned())
        }
        (false, _) => Ok(Value::String(text)),
    }
}

/// Builds the session-local `structured_output` tool from the caller's schema.
/// The handler is the only writer of `captured`, and it runs on the subagent's
/// turn, which is why the state is shared rather than returned.
fn structured_output_tool(
    schema: &Value,
    captured: &Arc<Mutex<Captured>>,
    report_ready: &Arc<AtomicBool>,
) -> Result<(Value, LocalTools), String> {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(SCHEMA_ROOT_ERROR.to_owned());
    }
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| format!("{SCHEMA_COMPILE_ERROR}: {error}"))?;
    let definition = serde_json::json!({
        "name": STRUCTURED_OUTPUT_TOOL,
        "description": STRUCTURED_OUTPUT_DESCRIPTION,
        "input_schema": schema,
    });
    let captured = Arc::clone(captured);
    let report_ready = Arc::clone(report_ready);
    let handler: LocalToolFn =
        crate::tools::audited_local_tool(ToolEffect::ReadOnly, move |input, ctx| {
            let result = record(&validator, &captured, input);
            if result.is_ok() {
                report_ready.store(true, Ordering::Release);
            } else {
                ctx.mark_tool_result_repairable();
            }
            Box::pin(async move { result })
        });
    Ok((
        definition,
        Arc::new([(STRUCTURED_OUTPUT_TOOL.to_owned(), handler)].into()),
    ))
}

fn record(
    validator: &Validator,
    captured: &Mutex<Captured>,
    input: Value,
) -> Result<String, String> {
    let errors: Vec<String> = validator
        .iter_errors(&input)
        .take(MAX_SCHEMA_ERRORS)
        .map(|error| error.to_string())
        .collect();
    if errors.is_empty() {
        lock(captured).value = Some(input);
        return Ok(STRUCTURED_OUTPUT_ACK.to_owned());
    }
    let errors = errors.join("\n");
    lock(captured).last_errors = Some(errors.clone());
    Err(format!("{INVALID_INPUT_PREFIX}{errors}"))
}

/// The captured state is only ever touched between awaits, so a poisoned lock
/// would mean a panic mid-update: recovering the guard keeps a failed
/// subagent from poisoning the whole tool.
fn lock(captured: &Mutex<Captured>) -> MutexGuard<'_, Captured> {
    captured
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Resolves the selected Chat provider and model at launch time, because the
/// TUI and interactive hosts swap them while a session runs.
pub type ModelResolver = Arc<dyn Fn() -> (Arc<dyn Provider>, Arc<Model>) + Send + Sync>;

/// The user's mode as it stands when an agent starts, so a workflow that
/// runs for hours is capped by what the user allows now, not at launch.
pub type ModeResolver = Arc<dyn Fn() -> AgentMode + Send + Sync>;

/// The durable part of a session's [`ToolContext`], captured once so the
/// workflow engine can launch agents long after the tool call that would
/// have carried them is gone.
#[derive(Clone)]
pub struct WorkflowHostContext {
    pub jobs: Option<JobScope>,
    pub model: ModelResolver,
    pub permissions: Arc<PermissionManager>,
    pub path_locks: Arc<PathLocks>,
    /// The session's revert point, so an agent a workflow launches captures it
    /// before its first write just like one the user's run launched.
    pub baseline: Option<BaselineGate>,
    pub subagent_history: SubagentHistoryStore,
    pub registry: Arc<ToolRegistry>,
    pub config: AgentConfig,
    pub mcp: Option<McpSession>,
    pub prompt_slots: Arc<ResolvedSlots>,
    pub prompt_profiles: Arc<PromptProfileCatalog>,
    pub default_task_prompt_profile_name: Arc<str>,
    pub model_policy: Arc<ModelPolicy>,
    pub timeouts: Timeouts,
    pub tool_output_store: Option<Arc<ToolOutputStore>>,
    pub tool_output_lines: ToolOutputLines,
    pub session_id: Option<SessionRef>,
    pub workspace_session: Option<WorkspaceSession>,
    pub remote_project_context: Option<Arc<crate::remote_project_context::RemoteProjectContext>>,
    /// The directory Caudra itself runs in. Set only in a sandbox session,
    /// where `{cwd}` names a path inside the VM and this one does not.
    pub host_cwd: Option<PathBuf>,
    pub local_documents: Option<Arc<LocalDocumentStore>>,
    pub task_environment: Vars,
    pub loaded_instructions: LoadedInstructions,
    /// Read when each task starts; its answer caps that task.
    pub mode: ModeResolver,
    /// The workflow runtime's own registrations, never the agent loop's.
    pub subagent_cancels: Arc<CancelMap<String>>,
    pub context_publisher: Option<ContextPublisher>,
    /// Shared with the UI so a workflow agent's permission prompts resolve.
    pub user_response_rx: Option<Arc<AsyncMutex<flume::Receiver<String>>>>,
    pub audience: ToolAudience,
    pub tool_filter: ToolFilter,
    pub tool_name_aliases: Option<ToolNameAliases>,
}

/// What a session's [`AgentParams`] leaves for the agent loop to set per
/// run, which the workflow host needs captured once instead.
pub struct HostExtras {
    pub mcp: Option<McpSession>,
    pub loaded_instructions: LoadedInstructions,
    pub user_response_rx: Option<Arc<AsyncMutex<flume::Receiver<String>>>>,
}

impl WorkflowHostContext {
    pub fn from_agent_params(
        params: &AgentParams,
        extras: HostExtras,
        model: ModelResolver,
        mode: ModeResolver,
        subagent_cancels: Arc<CancelMap<String>>,
    ) -> Self {
        Self {
            model,
            permissions: Arc::clone(&params.permissions),
            jobs: params
                .jobs
                .clone()
                .or_else(|| params.background.as_ref().map(|tasks| tasks.main_scope())),
            path_locks: Arc::clone(&params.path_locks),
            baseline: params.baseline.clone(),
            subagent_history: params.subagent_history.clone(),
            registry: Arc::clone(&params.registry),
            config: params.config.clone(),
            mcp: extras.mcp,
            prompt_slots: Arc::clone(&params.prompt_slots),
            prompt_profiles: Arc::clone(&params.prompt_profiles),
            default_task_prompt_profile_name: Arc::clone(&params.default_task_prompt_profile_name),
            model_policy: Arc::clone(&params.model_policy),
            timeouts: params.timeouts,
            tool_output_store: params.tool_output_store.clone(),
            tool_output_lines: params.tool_output_lines,
            session_id: params.session_id.clone(),
            workspace_session: params.workspace_session.clone(),
            remote_project_context: params.remote_project_context.clone(),
            host_cwd: params.host_cwd.clone(),
            local_documents: params.local_documents.clone(),
            task_environment: params.task_environment.clone(),
            loaded_instructions: extras.loaded_instructions,
            mode,
            subagent_cancels,
            context_publisher: params.context_publisher.clone(),
            user_response_rx: extras.user_response_rx,
            audience: params.audience,
            tool_filter: params.tool_filter.clone(),
            tool_name_aliases: None,
        }
    }

    pub fn from_tool_context(
        ctx: &ToolContext,
        model: ModelResolver,
        mode: ModeResolver,
        subagent_cancels: Arc<CancelMap<String>>,
    ) -> Self {
        Self {
            model,
            permissions: Arc::clone(&ctx.permissions),
            jobs: ctx.job_scope(),
            path_locks: Arc::clone(&ctx.path_locks),
            baseline: ctx.baseline.clone(),
            subagent_history: ctx.subagent_history.clone(),
            registry: Arc::clone(&ctx.registry),
            config: ctx.config.clone(),
            mcp: ctx.mcp.clone(),
            prompt_slots: Arc::clone(&ctx.prompt_slots),
            prompt_profiles: Arc::clone(&ctx.prompt_profiles),
            default_task_prompt_profile_name: Arc::clone(&ctx.default_task_prompt_profile_name),
            model_policy: Arc::clone(&ctx.model_policy),
            timeouts: ctx.timeouts,
            tool_output_store: ctx.tool_output_store.clone(),
            tool_output_lines: ctx.tool_output_lines,
            session_id: ctx.session_id.clone(),
            workspace_session: ctx.workspace_session.clone(),
            remote_project_context: ctx.remote_project_context.clone(),
            host_cwd: ctx.host_cwd.clone(),
            local_documents: ctx.local_documents.clone(),
            task_environment: ctx.task_environment.clone(),
            loaded_instructions: ctx.loaded_instructions.clone(),
            mode,
            subagent_cancels,
            context_publisher: ctx.context_publisher.clone(),
            user_response_rx: ctx.user_response_rx.clone(),
            audience: ctx.audience,
            tool_filter: ctx.tool_filter.clone(),
            tool_name_aliases: ctx.tool_name_aliases.clone(),
        }
    }

    /// A context for one launched agent: its own cancel scope and event
    /// channel, rooted at `call_id`, with nothing inherited from a tool call.
    pub async fn tool_context(
        &self,
        cancel: CancelToken,
        events: EventSender,
        call_id: &str,
    ) -> Result<ToolContext, String> {
        let mode = (self.mode)();
        let (chat_provider, chat_model) = (self.model)();
        let (provider, model) = if mode.is_planning() {
            crate::agent::resolve_model_for_purpose(
                crate::agent::ModelRoute {
                    provider: &chat_provider,
                    model: &chat_model,
                },
                crate::agent::ModelRoute {
                    provider: &chat_provider,
                    model: &chat_model,
                },
                ModelPurpose::Plan,
                None,
                self.timeouts,
                &self.model_policy,
            )
            .await
            .map_err(|error| error.user_message())?
        } else {
            (Arc::clone(&chat_provider), Model::clone(&chat_model))
        };
        Ok(ToolContext {
            provider,
            model: Arc::new(model),
            chat_provider,
            chat_model,
            event_tx: events,
            mode,
            session_id: self.session_id.clone(),
            workspace_session: self.workspace_session.clone(),
            remote_project_context: self.remote_project_context.clone(),
            host_cwd: self.host_cwd.clone(),
            local_documents: self.local_documents.clone(),
            task_environment: self.task_environment.clone(),
            context_publisher: self.context_publisher.clone(),
            tool_output_store: self.tool_output_store.clone(),
            tool_use_id: Some(call_id.to_owned()),
            root_tool_use_id: Some(call_id.to_owned()),
            local_root_tool_use_id: None,
            user_response_rx: self.user_response_rx.clone(),
            loaded_instructions: self.loaded_instructions.clone(),
            cancel,
            mcp: self.mcp.clone(),
            deferral: None,
            deadline: Deadline::None,
            config: self.config.clone(),
            tool_output_lines: self.tool_output_lines,
            permissions: Arc::clone(&self.permissions),
            timeouts: self.timeouts,
            file_tracker: FileReadTracker::fresh(),
            path_locks: Arc::clone(&self.path_locks),
            baseline: self.baseline.clone(),
            prompt_slots: Arc::clone(&self.prompt_slots),
            prompt_profiles: Arc::clone(&self.prompt_profiles),
            default_task_prompt_profile_name: Arc::clone(&self.default_task_prompt_profile_name),
            opts: RequestOptions::default(),
            subagent_cancels: Arc::clone(&self.subagent_cancels),
            subagent_history: self.subagent_history.clone(),
            registry: Arc::clone(&self.registry),
            audience: self.audience,
            tool_filter: self.tool_filter.clone(),
            local_tools: LocalTools::default(),
            tool_name_aliases: self.tool_name_aliases.clone(),
            live_sink: None,
            json_repair: Arc::default(),
            model_policy: Arc::clone(&self.model_policy),
            workflow: None,
            background: None,
            jobs: self.jobs.clone(),
            steering_observations: None,
            steering_order: Vec::new(),
            speculative: None,
        })
    }
}

/// The production runner: every workflow task goes through [`run_task`] on
/// a context synthesized from the host.
pub struct SubagentTaskRunner {
    host: Arc<WorkflowHostContext>,
}

impl SubagentTaskRunner {
    pub fn new(host: Arc<WorkflowHostContext>) -> Self {
        Self { host }
    }
}

impl TaskRunner for SubagentTaskRunner {
    fn reserve_task_cancellable(
        &self,
        task_id: Option<&str>,
        label: &str,
        cancel: &CancelToken,
    ) -> Result<SubagentHistoryLease, String> {
        if cancel.is_cancelled() {
            return Err(subagent::CANCELLED.into());
        }
        if task_id.is_some() {
            return self.reserve_task(task_id, label);
        }
        let cancelled = || cancel.is_cancelled();
        let retry = RuntimeRetry::new(None, &cancelled);
        subagent::reserve_task_identity_with_retry(
            &self.host.subagent_history,
            label,
            self.host.tool_output_store.as_deref(),
            self.host.session_id.as_ref().map(SessionRef::id),
            self.host.jobs.as_ref(),
            &retry,
        )
    }

    fn reserve_task(
        &self,
        task_id: Option<&str>,
        label: &str,
    ) -> Result<SubagentHistoryLease, String> {
        let history = &self.host.subagent_history;
        match task_id {
            Some(id) if history.snapshot().records().contains_key(id) => history
                .continue_task_with(id, SubagentTaskSpecCandidate::default())
                .map_err(|error| error.to_string()),
            Some(id) => history.reserve_unconfigured(id),
            None => subagent::reserve_task_identity(
                history,
                label,
                self.host.tool_output_store.as_deref(),
                self.host.session_id.as_ref().map(SessionRef::id),
                self.host.jobs.as_ref(),
            ),
        }
    }

    fn rebind_workspace(&self, workspace: &WorkspaceRebind) -> Result<Arc<dyn TaskRunner>, String> {
        if self.host.subagent_cancels.active_count() != 0 {
            return Err("workflow agents must be quiescent before changing workspace".into());
        }
        let mut host = self.host.as_ref().clone();
        let previous = host
            .workspace_session
            .as_ref()
            .ok_or("cannot rebind a local workflow to remote")?;
        if previous.binding().authority() != workspace.workspace.binding().authority()
            || previous.binding().principal() != workspace.workspace.binding().principal()
            || previous.binding().project() != workspace.workspace.binding().project()
        {
            return Err("workflow workspace identity changed".into());
        }
        host.workspace_session = Some(workspace.workspace.clone());
        host.remote_project_context = Some(Arc::clone(&workspace.context));
        host.task_environment = host.task_environment.set("{cwd}", workspace.cwd.clone());
        host.loaded_instructions =
            crate::agent::load_remote_instructions(&workspace.context, host.host_cwd.as_deref())
                .loaded;
        Ok(Arc::new(Self::new(Arc::new(host))))
    }

    fn run(
        &self,
        request: TaskRequest,
        cancel: CancelToken,
        events: EventSender,
    ) -> TaskFuture<'_> {
        Box::pin(async move {
            let started = Instant::now();
            let ctx = match self
                .host
                .tool_context(cancel, events, &request.call_id)
                .await
            {
                Ok(ctx) => ctx,
                Err(message) => return finish(started, None, None, 0, Err(message.into())),
            };
            run_task(&ctx, request).await
        })
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::sync::Mutex;

    use caudra_config::steering::SteeringModelConfig;
    use caudra_providers::{
        AgentError, CacheKey, ContentBlock, Message, ProviderEvent, Role, StopReason,
        StreamResponse, TokenUsage,
    };
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::cancel::CancelTrigger;
    use crate::permissions::PermissionMode;
    use crate::tools::DescriptionContext;
    use crate::tools::TOOL_SEARCH_TOOL_NAME;
    use crate::tools::registry::BoxFuture;
    use crate::tools::registry::{
        ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolInvocation, ToolSource,
    };
    use crate::tools::test_support::{MOCK_TOOL_OUTPUT, NamedMock, stub_ctx_with};
    use crate::workflow::{RuntimeDeps, WorkflowRuntime, WorkspaceRebind};
    use caudra_storage::{StateDir, id::CaudraId};
    use caudra_workflow::{
        LaunchRequest, RunStatus, WorkflowEvent, WorkflowRequest, WorkflowResponse,
    };
    use caudra_workspace::WorkspacePath;

    const CALL_ID: &str = "toolu_01";
    const SEARCH_CALL_ID: &str = "toolu_search";
    const LOADED_CALL_ID: &str = "toolu_loaded";
    const DEFERRED_TOOL: &str = "python_execution";
    const FRESH_ID: &str = "wf-run-1-call-7";
    const LABEL: &str = "find auth";
    const LABEL_ID: &str = "find-auth";
    const SECOND_LABEL_ID: &str = "find-auth-2";
    const PROMPT: &str = "search the codebase";
    const SUMMARY: &str = "found the middleware in src/auth.rs:12";
    const REQUIRED_FIELD: &str = "answer";
    const PARTIAL: &str = "half a transcript";
    const BOOM: &str = "boom";
    const CUSTOM_CORRECTION: &str = "Return the requested report using its schema.";
    const MISSING_REPORT_RULE: &str = "missing_task_report";
    const EMPTY_RESPONSE_RULE: &str = "empty_response";
    const TRUNCATION_RULE: &str = "truncation";
    const DEFAULT_TRUNCATION_ATTEMPTS: u32 = 3;
    const ORDINARY_TOOL_ROUNDS: u32 = 4;
    const SCRIPT_EXHAUSTED: &str = "script exhausted";
    const COMPACTED_SUMMARY: &str = "Earlier investigation was compacted.";
    const COMPACTION_HISTORY_MESSAGES: usize = 32;
    const COMPACTION_HISTORY_REPEATS: usize = 512;
    const COMPACTION_CONTEXT_WINDOW: u32 = 200_000;
    const COMPACTION_INPUT_TOKENS: u32 = 190_000;
    const CURRENT_CHAT_ID: &str = "current-chat";
    const PLAN_PATH: &str = ".caudra/plans/current.md";
    const CURSOR_PROBE: &str = "resume_cursor_probe";
    const CURSOR_WORKFLOW: &str = r#"let meta = #{ name: "echo", description: "cursor test" }; let result = agent("probe the cursor"); complete(result.output);"#;
    const FIRST_TURN: TokenUsage = TokenUsage {
        input: 100,
        output: 20,
        cache_creation: 0,
        cache_read: 0,
    };
    const SECOND_TURN: TokenUsage = TokenUsage {
        input: 50,
        output: 10,
        cache_creation: 0,
        cache_read: 0,
    };

    struct UnsupportedRunner;

    impl TaskRunner for UnsupportedRunner {
        fn run(&self, _: TaskRequest, _: CancelToken, _: EventSender) -> TaskFuture<'_> {
            panic!("unsupported runner must not execute")
        }
    }

    #[test_case(None; "generated")]
    #[test_case(Some(FRESH_ID); "explicit")]
    fn default_runner_requires_session_identity_reservation(task_id: Option<&str>) {
        assert_eq!(
            UnsupportedRunner.reserve_task(task_id, LABEL).unwrap_err(),
            RESERVATION_UNSUPPORTED
        );
    }

    #[test]
    fn cancelled_workflow_reservation_never_calls_the_runner() {
        let (trigger, cancel) = CancelToken::new();
        trigger.cancel();
        assert_eq!(
            UnsupportedRunner
                .reserve_task_cancellable(None, LABEL, &cancel)
                .unwrap_err(),
            subagent::CANCELLED,
        );
    }

    #[test]
    fn workflow_runner_reserves_shared_labels_and_preserves_replay_ids() {
        let ctx = stub_ctx_with(&AgentMode::Build, None, Some(CALL_ID));
        let model: ModelResolver = Arc::new({
            let provider = Arc::clone(&ctx.provider);
            let model = Arc::clone(&ctx.model);
            move || (Arc::clone(&provider), Arc::clone(&model))
        });
        let runner = SubagentTaskRunner::new(Arc::new(WorkflowHostContext::from_tool_context(
            &ctx,
            model,
            Arc::new(|| AgentMode::Build),
            Arc::new(CancelMap::new()),
        )));
        let first = runner
            .reserve_task_cancellable(None, LABEL, &CancelToken::none())
            .unwrap();
        let second = runner.reserve_task(None, "Find/Auth").unwrap();
        assert_eq!(first.task_id(), LABEL_ID);
        assert_eq!(second.task_id(), SECOND_LABEL_ID);
        first.complete(Vec::new());
        let replay = runner.reserve_task(Some(LABEL_ID), PROMPT).unwrap();
        assert_eq!(replay.task_id(), LABEL_ID);
        let legacy = runner.reserve_task(Some(FRESH_ID), LABEL).unwrap();
        assert_eq!(legacy.task_id(), FRESH_ID);
        drop((second, replay, legacy));
        assert_eq!(ctx.subagent_history.active_count(), 0);
    }

    #[derive(Clone, Default)]
    struct CursorProbe {
        cursors: Arc<Mutex<Vec<(String, String)>>>,
        lock_domains: Arc<Mutex<Vec<Arc<PathLocks>>>>,
    }

    impl Tool for CursorProbe {
        fn name(&self) -> &str {
            CURSOR_PROBE
        }
        fn description(&self, _: &DescriptionContext) -> Cow<'_, str> {
            Cow::Borrowed("Probe the bound remote cursor")
        }
        fn schema(&self) -> Value {
            json!({"type":"object","properties":{}})
        }
        fn parse(&self, _: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
            Ok(Box::new(self.clone()))
        }
    }

    impl ToolInvocation for CursorProbe {
        fn start_header(&self) -> HeaderFuture {
            HeaderFuture::Ready(HeaderResult::plain(CURSOR_PROBE.into()))
        }
        fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
            Box::pin(async move {
                let workspace = ctx.workspace_session.as_ref().unwrap();
                let cwd = crate::workspace_logical_cwd(workspace).await.unwrap();
                self.cursors
                    .lock()
                    .unwrap()
                    .push((workspace.cursor().cwd_handle().as_str().into(), cwd.clone()));
                self.lock_domains
                    .lock()
                    .unwrap()
                    .push(Arc::clone(&ctx.path_locks));
                Ok(crate::ToolOutput::Plain(crate::TextOutput {
                    text: cwd,
                    instructions: None,
                    state: None,
                    lua_provenance: None,
                }))
                .into()
            })
        }
    }

    #[test]
    fn subsequent_workflow_agent_executes_with_rebuilt_cursor_and_context() {
        smol::block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let (workspace, service) =
                crate::stored_session::tests::remote_workspace("runner", CURSOR_WORKFLOW);
            let context = crate::remote_project_context::load_remote_project_context(&workspace)
                .await
                .unwrap();
            let provider = Arc::new(ScriptedProvider::new(vec![
                response(
                    vec![ContentBlock::tool_use("probe", CURSOR_PROBE, json!({}))],
                    StopReason::ToolUse,
                    FIRST_TURN,
                ),
                text_response(SUMMARY, SECOND_TURN),
            ]));
            let mut ctx = stub_ctx_with(&AgentMode::Build, None, Some(CALL_ID));
            ctx.session_id = Some(SessionRef::from(CaudraId::generate()));
            let state_dir = StateDir::from_path(temp.path().into());
            let binding =
                caudra_storage::workspace_binding::StoredWorkspaceBinding::new_with_cursor(
                    workspace.binding().clone(),
                    workspace.cursor().clone(),
                    None,
                )
                .unwrap();
            let mut stored = crate::StoredSession::new_with_workspace("test/model", ".", binding);
            stored.id = ctx.session_id.as_ref().unwrap().id();
            stored.save(&state_dir).unwrap();
            ctx.workspace_session = Some(workspace.clone());
            ctx.remote_project_context = Some(Arc::clone(&context));
            ctx.permissions.set_session_mode(Some(PermissionMode::Yolo));
            ctx.registry = Arc::new(ToolRegistry::new());
            let probe = CursorProbe::default();
            ctx.registry
                .register_audited(
                    Arc::new(probe.clone()),
                    ToolSource::Native {
                        owner: CURSOR_PROBE.into(),
                        contract: CURSOR_PROBE.into(),
                        trusted: true,
                    },
                    ToolEffect::ReadOnly,
                )
                .unwrap();
            let model: ModelResolver = Arc::new({
                let model = Arc::clone(&ctx.model);
                move || (provider.clone(), model.clone())
            });
            let mode: ModeResolver = Arc::new(|| AgentMode::Build);
            let cancels = Arc::new(CancelMap::new());
            let host =
                WorkflowHostContext::from_tool_context(&ctx, model, mode.clone(), cancels.clone());
            let (events, received) = flume::unbounded();
            let runtime = WorkflowRuntime::spawn(
                RuntimeDeps {
                    state_dir,
                    session_id: ctx.session_id.as_ref().unwrap().id(),
                    cwd: ".".into(),
                    user_config_dir: Some(temp.path().join("config")),
                    remote_project_context: Some(context),
                    runner: Arc::new(SubagentTaskRunner::new(Arc::new(host))),
                    events,
                    mode,
                    subagent_cancels: cancels,
                },
                None,
            )
            .await
            .unwrap();
            let handle = runtime.handle();
            let resolved = workspace
                .workspace()
                .services()
                .read
                .as_ref()
                .unwrap()
                .resolve_directory(
                    workspace.binding(),
                    workspace.cursor(),
                    &WorkspacePath::new("nested").unwrap(),
                )
                .await
                .unwrap();
            let nested = workspace.with_cursor(resolved).unwrap();
            service
                .revision
                .store(2, std::sync::atomic::Ordering::SeqCst);
            let context = crate::remote_project_context::load_remote_project_context(&nested)
                .await
                .unwrap();
            let transition = handle.suspend().await.unwrap();
            transition
                .rebind(WorkspaceRebind {
                    workspace: nested.clone(),
                    context,
                    cwd: "nested".into(),
                })
                .await
                .unwrap();
            transition.commit().await.unwrap();
            let WorkflowResponse::Catalog(catalog) =
                handle.request(WorkflowRequest::List).await.unwrap()
            else {
                panic!("catalog");
            };
            let entry = catalog
                .entries
                .iter()
                .find(|entry| entry.name == "echo")
                .unwrap();
            handle
                .request(WorkflowRequest::Trust {
                    name: "echo".into(),
                    digest: entry.digest.clone(),
                })
                .await
                .unwrap();
            let WorkflowResponse::Started(run) = handle
                .request(WorkflowRequest::Start(LaunchRequest {
                    name: "echo".into(),
                    args: json!({}),
                    agent_budget: None,
                }))
                .await
                .unwrap()
            else {
                panic!("started");
            };
            loop {
                let envelope = received.recv_async().await.unwrap();
                if let crate::AgentEvent::Workflow(event) = envelope.event
                    && let WorkflowEvent::Snapshot(snapshot) = *event
                    && snapshot.run_id == run.run_id
                    && snapshot.status != RunStatus::Active
                {
                    assert_eq!(
                        snapshot.status,
                        RunStatus::Completed,
                        "{:?}",
                        snapshot.error
                    );
                    break;
                }
            }
            assert_eq!(
                *probe.cursors.lock().unwrap(),
                vec![(
                    nested.cursor().cwd_handle().as_str().into(),
                    "nested".into()
                )]
            );
            let lock_domains = std::mem::take(&mut *probe.lock_domains.lock().unwrap());
            assert_eq!(lock_domains.len(), 1);
            assert!(
                Arc::ptr_eq(&lock_domains[0], &ctx.path_locks),
                "a rebound workflow agent must queue its writes on the session's locks"
            );
            runtime.shutdown().await;
        });
    }
    fn answer_schema() -> Value {
        json!({
            "type": "object",
            "required": [REQUIRED_FIELD],
            "properties": { REQUIRED_FIELD: { "type": "string" } },
        })
    }

    /// Three required strings, so a single empty object over-runs
    /// `MAX_SCHEMA_ERRORS` and the bound is actually exercised.
    fn strict_schema() -> Value {
        json!({
            "type": "object",
            "required": ["a", "b", "c", "d"],
            "properties": {
                "a": { "type": "string" },
                "b": { "type": "string" },
                "c": { "type": "string" },
                "d": { "type": "string" },
            },
        })
    }

    fn tool_for(schema: &Value) -> Result<(Value, LocalTools), String> {
        structured_output_tool(
            schema,
            &Arc::new(Mutex::new(Captured::default())),
            &Arc::new(AtomicBool::new(false)),
        )
    }

    /// `LocalToolEntry` holds boxed closures and cannot be `Debug`, so the
    /// error cases unwrap by hand rather than through `expect_err`.
    fn schema_error(schema: Value) -> String {
        match tool_for(&schema) {
            Err(error) => error,
            Ok(_) => panic!("schema was accepted"),
        }
    }

    #[test]
    fn a_schema_that_is_not_an_object_is_refused_before_any_session() {
        assert_eq!(schema_error(json!({ "type": "array" })), SCHEMA_ROOT_ERROR);
    }

    #[test]
    fn an_uncompilable_schema_is_refused_before_any_session() {
        let error = schema_error(json!({ "type": "object", "properties": 7 }));
        assert!(error.starts_with(SCHEMA_COMPILE_ERROR), "got: {error}");
    }

    #[test]
    fn the_output_tool_carries_the_callers_schema_verbatim() {
        let schema = answer_schema();
        let (definition, tools) = tool_for(&schema).expect("valid schema");
        assert_eq!(definition["name"], json!(STRUCTURED_OUTPUT_TOOL));
        assert_eq!(definition["input_schema"], schema);
        assert!(tools.contains_key(STRUCTURED_OUTPUT_TOOL));
        assert_eq!(
            tools[STRUCTURED_OUTPUT_TOOL].effect,
            ToolEffect::ReadOnly,
            "reporting a result is not an effect"
        );
    }

    #[test]
    fn a_valid_report_is_captured_and_acknowledged() {
        let validator = jsonschema::validator_for(&answer_schema()).unwrap();
        let captured = Mutex::new(Captured::default());
        let value = json!({ REQUIRED_FIELD: SUMMARY });

        assert_eq!(
            record(&validator, &captured, value.clone()),
            Ok(STRUCTURED_OUTPUT_ACK.to_owned())
        );
        assert_eq!(lock(&captured).value.as_ref(), Some(&value));
    }

    #[test]
    fn an_invalid_report_is_an_inline_error_the_subagent_can_fix() {
        let validator = jsonschema::validator_for(&answer_schema()).unwrap();
        let captured = Mutex::new(Captured::default());

        let error = record(&validator, &captured, json!({})).expect_err("missing required field");

        assert!(error.starts_with(INVALID_INPUT_PREFIX), "got: {error}");
        assert!(
            lock(&captured).value.is_none(),
            "an invalid report is not captured"
        );
        assert!(
            lock(&captured).last_errors.is_some(),
            "the failure is remembered for the final verdict"
        );
    }

    #[test]
    fn reported_schema_errors_are_bounded() {
        let validator = jsonschema::validator_for(&strict_schema()).unwrap();
        let captured = Mutex::new(Captured::default());

        record(&validator, &captured, json!({})).expect_err("four fields missing");

        let errors = lock(&captured)
            .last_errors
            .clone()
            .expect("errors recorded");
        assert_eq!(errors.lines().count(), MAX_SCHEMA_ERRORS);
    }

    #[test]
    fn a_later_valid_report_supersedes_an_earlier_failure() {
        let validator = jsonschema::validator_for(&answer_schema()).unwrap();
        let captured = Mutex::new(Captured::default());
        record(&validator, &captured, json!({})).expect_err("first attempt is invalid");

        record(&validator, &captured, json!({ REQUIRED_FIELD: SUMMARY })).expect("second attempt");

        let captured = std::mem::take(&mut *lock(&captured));
        assert!(report(true, captured, String::new()).is_ok());
    }

    #[test]
    fn a_captured_value_is_returned_as_json() {
        let captured = Captured {
            value: Some(json!({ REQUIRED_FIELD: SUMMARY })),
            last_errors: None,
        };
        assert_eq!(
            report(true, captured, String::new()),
            Ok(json!({ REQUIRED_FIELD: SUMMARY }))
        );
    }

    #[test]
    fn a_silent_subagent_under_a_schema_reports_what_it_got_wrong() {
        let errors = "answer is a required property";
        let captured = Captured {
            value: None,
            last_errors: Some(errors.into()),
        };
        assert_eq!(
            report(true, captured, SUMMARY.into()),
            Err(format!("{STRUCTURED_INVALID_ERROR}:\n{errors}"))
        );
    }

    #[test]
    fn a_subagent_that_never_called_the_output_tool_says_so() {
        assert_eq!(
            report(true, Captured::default(), SUMMARY.into()),
            Err(STRUCTURED_MISSING_ERROR.to_owned())
        );
    }

    #[test]
    fn a_summary_is_returned_verbatim_without_a_schema() {
        assert_eq!(
            report(false, Captured::default(), SUMMARY.into()),
            Ok(Value::String(SUMMARY.into()))
        );
    }

    #[test_case(""; "empty")]
    #[test_case(" \n "; "whitespace")]
    #[test_case(crate::EMPTY_RESPONSE_MARKER; "marker")]
    fn a_subagent_that_says_nothing_at_all_is_an_error(text: &str) {
        assert_eq!(
            report(false, Captured::default(), text.into()),
            Err(SUMMARY_MISSING_ERROR.to_owned())
        );
    }

    #[test]
    fn an_interrupted_run_keeps_the_partial_transcript() {
        let message = failure_message(subagent::PromptFailure {
            error: BOOM.into(),
            partial: Some(PARTIAL.into()),
        });
        assert_eq!(
            message,
            format!("{INTERRUPTED_PREFIX}{BOOM}{INTERRUPTED_SUFFIX}{PARTIAL}")
        );
    }

    #[test]
    fn a_run_that_streamed_nothing_reports_the_bare_error() {
        let message = failure_message(subagent::PromptFailure {
            error: BOOM.into(),
            partial: None,
        });
        assert_eq!(message, format!("{ERROR_PREFIX}{BOOM}"));
    }

    /// The whole round trip, because each half passes its own test while the
    /// pair disagrees: `tool_search` puts a deferred definition in the next
    /// request, and dispatch judges the call against a filter built when the
    /// session opened. A child that loads a tool must be able to call it.
    #[test]
    fn a_task_can_call_a_builtin_it_loaded_with_tool_search() {
        smol::block_on(async {
            let provider = ScriptedProvider::new(vec![
                response(
                    vec![ContentBlock::tool_use(
                        SEARCH_CALL_ID,
                        TOOL_SEARCH_TOOL_NAME,
                        json!({ "query": DEFERRED_TOOL }),
                    )],
                    StopReason::ToolUse,
                    FIRST_TURN,
                ),
                response(
                    vec![ContentBlock::tool_use(
                        LOADED_CALL_ID,
                        DEFERRED_TOOL,
                        json!({}),
                    )],
                    StopReason::ToolUse,
                    FIRST_TURN,
                ),
                text_response(SUMMARY, SECOND_TURN),
            ]);
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            ctx.config.defer_builtin_tools = caudra_config::DeferBuiltinTools::Always;
            ctx.registry
                .register(
                    Arc::new(NamedMock::new(DEFERRED_TOOL, ToolAudience::all())),
                    NamedMock::source(),
                )
                .unwrap();

            let outcome = run_task(&ctx, request(TaskIdentity::Derive, None)).await;

            assert!(outcome.success, "{:?}", outcome.error);
            let transcript = format!("{:?}", observed.lock().unwrap().last().unwrap());
            assert!(
                transcript.contains(MOCK_TOOL_OUTPUT),
                "the loaded tool must have run: {transcript}"
            );
        });
    }

    /// A caller that asked for a command runner and was handed a read-only
    /// agent has no other way to find out, so the outcome reports what the
    /// task actually ran as rather than what was requested.
    #[test_case(AgentMode::Build, None, SubagentTaskMode::Build; "an_omitted_mode_is_inherited")]
    #[test_case(AgentMode::ReadOnly, Some(SubagentTaskMode::Build), SubagentTaskMode::Plan; "a_clamped_mode_is_reported")]
    fn an_outcome_reports_the_mode_the_task_ran_as(
        caller: AgentMode,
        requested: Option<SubagentTaskMode>,
        expected: SubagentTaskMode,
    ) {
        smol::block_on(async {
            let provider = ScriptedProvider::new(vec![text_response(SUMMARY, FIRST_TURN)]);
            let ctx = ctx_with(caller, provider);

            let outcome = run_task(&ctx, request(TaskIdentity::Derive, requested)).await;

            assert_eq!(outcome.mode, Some(expected));
        });
    }

    #[test_case(TaskIdentity::Derive, Some(SubagentTaskMode::Build), AgentMode::ReadOnly, Some(SubagentTaskMode::Plan); "build_under_read_only_becomes_plan")]
    #[test_case(TaskIdentity::Derive, Some(SubagentTaskMode::Build), AgentMode::Plan("plan.md".into()), Some(SubagentTaskMode::Plan); "build_under_plan_becomes_plan")]
    #[test_case(TaskIdentity::Derive, Some(SubagentTaskMode::Build), AgentMode::Build, Some(SubagentTaskMode::Build); "build_under_build_stays")]
    #[test_case(TaskIdentity::Derive, None, AgentMode::ReadOnly, None; "an_omitted_mode_is_left_to_the_opener")]
    #[test_case(TaskIdentity::Continue(FRESH_ID.into()), Some(SubagentTaskMode::Build), AgentMode::ReadOnly, Some(SubagentTaskMode::Build); "a_continuation_keeps_its_stored_mode")]
    fn a_build_request_is_capped_by_the_parents_mode(
        task: TaskIdentity,
        requested: Option<SubagentTaskMode>,
        ceiling: AgentMode,
        expected: Option<SubagentTaskMode>,
    ) {
        assert_eq!(clamp_mode(&task, requested, &ceiling), expected);
    }

    #[test]
    fn workflow_context_uses_current_chat_with_one_captured_mode() {
        smol::block_on(async {
            let base = stub_ctx_with(&AgentMode::Build, None, Some(CALL_ID));
            let mut current_chat = Model::clone(&base.model);
            current_chat.id = CURRENT_CHAT_ID.into();
            let selected = Arc::new(ArcSwap::from_pointee((
                Arc::clone(&base.provider),
                Arc::new(current_chat),
            )));
            let mode = Arc::new(ArcSwap::from_pointee(AgentMode::Plan(PLAN_PATH.into())));
            let model_resolver: ModelResolver = Arc::new({
                let selected = Arc::clone(&selected);
                let mode = Arc::clone(&mode);
                move || {
                    mode.store(Arc::new(AgentMode::Build));
                    let selected = selected.load();
                    (Arc::clone(&selected.0), Arc::clone(&selected.1))
                }
            });
            let mode_resolver: ModeResolver = Arc::new({
                let mode = Arc::clone(&mode);
                move || AgentMode::clone(&mode.load())
            });
            let host = WorkflowHostContext::from_tool_context(
                &base,
                model_resolver,
                mode_resolver,
                Arc::new(CancelMap::new()),
            );
            let (event_tx, _event_rx) = flume::unbounded();
            let ctx = host
                .tool_context(CancelToken::none(), EventSender::new(event_tx, 0), CALL_ID)
                .await
                .unwrap();

            assert!(matches!(ctx.mode, AgentMode::Plan(_)));
            assert_eq!(ctx.model.id, CURRENT_CHAT_ID);
            assert_eq!(ctx.chat_model.id, CURRENT_CHAT_ID);
            assert!(Arc::ptr_eq(&ctx.provider, &ctx.chat_provider));
        });
    }

    /// Answers each request from a script; hangs on the last when told to,
    /// firing `cancel` first so the run is cut short mid-stream.
    struct ScriptedProvider {
        responses: Mutex<Vec<StreamResponse>>,
        cancel_when_exhausted: Mutex<Option<CancelTrigger>>,
        requests: Arc<Mutex<Vec<Vec<Message>>>>,
        cancel_partial: Option<&'static str>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<StreamResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                cancel_when_exhausted: Mutex::new(None),
                requests: Arc::default(),
                cancel_partial: None,
            }
        }

        fn cancelling(trigger: CancelTrigger) -> Self {
            Self {
                responses: Mutex::new(Vec::new()),
                cancel_when_exhausted: Mutex::new(Some(trigger)),
                requests: Arc::default(),
                cancel_partial: None,
            }
        }
    }

    impl Provider for ScriptedProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            messages: &'a [Message],
            _: &'a str,
            _: &'a Value,
            events: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(messages.to_vec());
                let next = {
                    let mut responses = self.responses.lock().unwrap();
                    (!responses.is_empty()).then(|| responses.remove(0))
                };
                match next {
                    Some(response) => Ok(response),
                    None => {
                        if let Some(trigger) = self.cancel_when_exhausted.lock().unwrap().take() {
                            if let Some(text) = self.cancel_partial {
                                events
                                    .send(ProviderEvent::TextDelta { text: text.into() })
                                    .unwrap();
                            }
                            trigger.cancel();
                        } else {
                            return Err(AgentError::Config {
                                message: SCRIPT_EXHAUSTED.into(),
                            });
                        }
                        futures_lite::future::pending().await
                    }
                }
            })
        }

        fn list_models(
            &self,
        ) -> BoxFuture<'_, Result<Vec<caudra_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    fn response(content: Vec<ContentBlock>, stop: StopReason, usage: TokenUsage) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content,
                ..Default::default()
            },
            usage,
            stop_reason: Some(stop),
            ..Default::default()
        }
    }

    fn text_response(text: &str, usage: TokenUsage) -> StreamResponse {
        response(
            vec![ContentBlock::Text { text: text.into() }],
            StopReason::EndTurn,
            usage,
        )
    }

    fn truncated_response(text: &str, usage: TokenUsage) -> StreamResponse {
        response(
            vec![ContentBlock::Text { text: text.into() }],
            StopReason::MaxTokens,
            usage,
        )
    }

    fn structured_output_call(value: Value) -> StreamResponse {
        response(
            vec![ContentBlock::tool_use("t1", STRUCTURED_OUTPUT_TOOL, value)],
            StopReason::ToolUse,
            FIRST_TURN,
        )
    }

    fn ctx_with(mode: AgentMode, provider: impl Provider + 'static) -> ToolContext {
        let mut ctx = stub_ctx_with(&mode, None, Some(CALL_ID));
        ctx.provider = Arc::new(provider);
        ctx
    }

    fn request(task: TaskIdentity, mode: Option<SubagentTaskMode>) -> TaskRequest {
        TaskRequest {
            prompt: Some(PROMPT.into()),
            label: LABEL.into(),
            task,
            mode,
            profile: None,
            model_job: None,
            output_schema: None,
            call_id: CALL_ID.into(),
            provenance: None,
        }
    }

    #[track_caller]
    fn assert_retired(ctx: &ToolContext, task_id: &str) {
        assert_eq!(ctx.subagent_cancels.active_count(), 0);
        assert!(!ctx.subagent_history.is_active(task_id));
        assert!(
            ctx.subagent_history
                .snapshot()
                .records()
                .contains_key(task_id),
            "the session's history is recorded on close"
        );
    }

    #[test_case(SUMMARY, StopReason::EndTurn; "prose_tail")]
    #[test_case("", StopReason::EndTurn; "empty_tail")]
    #[test_case(" \n ", StopReason::EndTurn; "whitespace_tail")]
    #[test_case("", StopReason::MaxTokens; "truncated_empty_tail")]
    fn a_structured_result_is_validated_and_returned_as_json(tail: &str, stop: StopReason) {
        smol::block_on(async {
            let expected = json!({ REQUIRED_FIELD: SUMMARY });
            let ctx = ctx_with(
                AgentMode::Build,
                ScriptedProvider::new(vec![
                    structured_output_call(expected.clone()),
                    response(
                        vec![ContentBlock::Text { text: tail.into() }],
                        stop,
                        SECOND_TURN,
                    ),
                ]),
            );

            let outcome = run_task(
                &ctx,
                TaskRequest {
                    output_schema: Some(answer_schema()),
                    ..request(TaskIdentity::Derive, None)
                },
            )
            .await;

            assert_eq!(outcome.error, None);
            assert!(outcome.success && !outcome.cancelled);
            assert_eq!(outcome.output, expected);
            assert_ne!(outcome.task_id.as_deref(), Some(CALL_ID));
            assert_eq!(outcome.task_id.as_deref(), Some(LABEL_ID));
            let expected_tokens = FIRST_TURN.total_input()
                + FIRST_TURN.output
                + SECOND_TURN.total_input()
                + SECOND_TURN.output;
            assert_eq!(outcome.tokens_used, u64::from(expected_tokens));
            assert_retired(&ctx, outcome.task_id.as_deref().unwrap());
        });
    }

    #[test_case(0, 1; "no_allowance")]
    #[test_case(1, 2; "combined_limit")]
    #[test_case(8, 3; "report_limit")]
    fn missing_reports_are_bounded_across_fresh_agents(budget: u32, requests: usize) {
        smol::block_on(async {
            let provider = ScriptedProvider::new(
                (0..requests)
                    .map(|_| text_response(SUMMARY, FIRST_TURN))
                    .collect(),
            );
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            Arc::make_mut(&mut ctx.config.steering).max_recoveries = Some(budget);
            let outcome = run_task(
                &ctx,
                TaskRequest {
                    output_schema: Some(answer_schema()),
                    ..request(TaskIdentity::Derive, None)
                },
            )
            .await;
            let exhausted = AgentError::SteeringExhausted {
                rule: MISSING_REPORT_RULE.into(),
            };
            assert!(!outcome.success);
            assert!(
                outcome
                    .error
                    .as_deref()
                    .unwrap()
                    .contains(&exhausted.to_string())
            );
            assert!(outcome.error.as_deref().unwrap().contains(SUMMARY));
            assert_eq!(observed.lock().unwrap().len(), requests);
            assert_eq!(
                outcome.tokens_used,
                requests as u64 * u64::from(FIRST_TURN.total_input() + FIRST_TURN.output)
            );
            assert_retired(&ctx, outcome.task_id.as_deref().unwrap());
        });
    }

    #[test_case(1, true; "first_response")]
    #[test_case(2, true; "automatic_correction")]
    #[test_case(1, false; "steering_disabled")]
    fn missing_reports_cannot_extend_the_invocation_turn_limit(max_turns: u32, enabled: bool) {
        smol::block_on(async {
            let provider =
                ScriptedProvider::new((0..3).map(|_| text_response(SUMMARY, FIRST_TURN)).collect());
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            ctx.config.max_turns = Some(max_turns);
            Arc::make_mut(&mut ctx.config.steering).enabled = Some(enabled);
            let outcome = run_task(
                &ctx,
                TaskRequest {
                    output_schema: Some(answer_schema()),
                    ..request(TaskIdentity::Derive, None)
                },
            )
            .await;
            assert!(!outcome.success);
            assert_eq!(observed.lock().unwrap().len(), max_turns as usize);
            assert!(
                outcome
                    .error
                    .as_deref()
                    .unwrap()
                    .contains(subagent::TURN_LIMIT)
            );
            assert!(outcome.error.as_deref().unwrap().contains(SUMMARY));
        });
    }

    #[test_case(1; "first_retry")]
    #[test_case(DEFAULT_TRUNCATION_ATTEMPTS; "last_default_retry")]
    fn a_truncated_task_summary_keeps_all_fragments(attempts: u32) {
        smol::block_on(async {
            let mut responses: Vec<_> = (0..attempts)
                .map(|_| truncated_response(PARTIAL, FIRST_TURN))
                .collect();
            responses.push(text_response(SUMMARY, SECOND_TURN));
            let provider = ScriptedProvider::new(responses);
            let observed = Arc::clone(&provider.requests);
            let ctx = ctx_with(AgentMode::Build, provider);
            let outcome = run_task(&ctx, request(TaskIdentity::Derive, None)).await;
            assert_eq!(outcome.error, None);
            assert_eq!(
                outcome.output,
                json!(format!("{}{SUMMARY}", PARTIAL.repeat(attempts as usize)))
            );
            assert_eq!(observed.lock().unwrap().len(), attempts as usize + 1);
            assert_eq!(
                outcome.tokens_used,
                u64::from(
                    attempts * (FIRST_TURN.total_input() + FIRST_TURN.output)
                        + SECOND_TURN.total_input()
                        + SECOND_TURN.output
                )
            );
            assert_retired(&ctx, outcome.task_id.as_deref().unwrap());
        });
    }

    #[test_case(true, false, PARTIAL; "master_disabled_prose")]
    #[test_case(false, false, PARTIAL; "rule_disabled_prose")]
    #[test_case(true, true, PARTIAL; "master_disabled_missing_report")]
    #[test_case(false, true, PARTIAL; "rule_disabled_missing_report")]
    #[test_case(true, true, ""; "master_disabled_empty")]
    #[test_case(false, true, ""; "rule_disabled_empty")]
    fn disabled_truncation_is_not_a_report_or_a_report_retry(
        master: bool,
        validating: bool,
        text: &str,
    ) {
        smol::block_on(async {
            let provider = ScriptedProvider::new(vec![truncated_response(text, FIRST_TURN)]);
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            let steering = Arc::make_mut(&mut ctx.config.steering);
            if master {
                steering.enabled = Some(false);
            } else {
                steering.rules.truncation.enabled = Some(false);
            }
            let outcome = run_task(
                &ctx,
                TaskRequest {
                    output_schema: validating.then(answer_schema),
                    ..request(TaskIdentity::Derive, None)
                },
            )
            .await;
            assert!(!outcome.success && !outcome.cancelled);
            assert_eq!(outcome.output, Value::Null);
            assert_eq!(
                outcome.error,
                Some(failure_message(subagent::PromptFailure {
                    error: subagent::TRUNCATED.into(),
                    partial: (!text.is_empty()).then(|| text.into()),
                }))
            );
            assert_eq!(observed.lock().unwrap().len(), 1);
            assert_eq!(
                outcome.tokens_used,
                u64::from(FIRST_TURN.total_input() + FIRST_TURN.output)
            );
            assert_retired(&ctx, outcome.task_id.as_deref().unwrap());
        });
    }

    #[test_case(1, None, false; "rule_limit")]
    #[test_case(DEFAULT_TRUNCATION_ATTEMPTS, None, true; "default_limit_missing_report")]
    #[test_case(DEFAULT_TRUNCATION_ATTEMPTS, Some(1), true; "combined_limit")]
    #[test_case(DEFAULT_TRUNCATION_ATTEMPTS, Some(0), true; "zero_budget")]
    fn exhausted_truncation_retains_fragments_and_usage(
        attempts: u32,
        budget: Option<u32>,
        validating: bool,
    ) {
        smol::block_on(async {
            let responses = attempts.min(budget.unwrap_or(attempts)) + 1;
            let provider = ScriptedProvider::new(
                (0..responses)
                    .map(|_| truncated_response(PARTIAL, FIRST_TURN))
                    .collect(),
            );
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            let steering = Arc::make_mut(&mut ctx.config.steering);
            steering.rules.truncation.max_attempts = Some(attempts);
            steering.max_recoveries = budget;
            let outcome = run_task(
                &ctx,
                TaskRequest {
                    output_schema: validating.then(answer_schema),
                    ..request(TaskIdentity::Derive, None)
                },
            )
            .await;
            assert!(!outcome.success && !outcome.cancelled);
            assert_eq!(outcome.output, Value::Null);
            assert_eq!(
                outcome.error,
                Some(failure_message(subagent::PromptFailure {
                    error: AgentError::SteeringExhausted {
                        rule: TRUNCATION_RULE.into()
                    }
                    .to_string(),
                    partial: Some(PARTIAL.repeat(responses as usize)),
                }))
            );
            assert_eq!(observed.lock().unwrap().len(), responses as usize);
            assert_eq!(
                outcome.tokens_used,
                u64::from(responses * (FIRST_TURN.total_input() + FIRST_TURN.output))
            );
            assert_retired(&ctx, outcome.task_id.as_deref().unwrap());
        });
    }

    #[test]
    fn ordinary_tool_responses_do_not_spend_task_truncation_attempts() {
        smol::block_on(async {
            let mut responses: Vec<_> = (0..ORDINARY_TOOL_ROUNDS)
                .map(|round| {
                    response(
                        vec![ContentBlock::tool_use(
                            format!("report-{round}"),
                            STRUCTURED_OUTPUT_TOOL,
                            json!({ REQUIRED_FIELD: round.to_string() }),
                        )],
                        StopReason::ToolUse,
                        FIRST_TURN,
                    )
                })
                .collect();
            responses.extend(
                (0..DEFAULT_TRUNCATION_ATTEMPTS).map(|_| truncated_response(PARTIAL, FIRST_TURN)),
            );
            responses.push(text_response(SUMMARY, SECOND_TURN));
            let provider = ScriptedProvider::new(responses);
            let observed = Arc::clone(&provider.requests);
            let ctx = ctx_with(AgentMode::Build, provider);
            let outcome = run_task(
                &ctx,
                TaskRequest {
                    output_schema: Some(answer_schema()),
                    ..request(TaskIdentity::Derive, None)
                },
            )
            .await;
            assert_eq!(outcome.error, None);
            assert_eq!(
                outcome.output,
                json!({ REQUIRED_FIELD: (ORDINARY_TOOL_ROUNDS - 1).to_string() })
            );
            let initial_responses = ORDINARY_TOOL_ROUNDS + DEFAULT_TRUNCATION_ATTEMPTS;
            assert_eq!(
                observed.lock().unwrap().len(),
                initial_responses as usize + 1
            );
            assert_eq!(
                outcome.tokens_used,
                u64::from(
                    initial_responses * (FIRST_TURN.total_input() + FIRST_TURN.output)
                        + SECOND_TURN.total_input()
                        + SECOND_TURN.output
                )
            );
        });
    }

    #[test_case(1, None, TRUNCATION_RULE; "truncation_allowance")]
    #[test_case(DEFAULT_TRUNCATION_ATTEMPTS, Some(2), TRUNCATION_RULE; "combined_allowance")]
    #[test_case(DEFAULT_TRUNCATION_ATTEMPTS, Some(1), MISSING_REPORT_RULE; "truncation_spends_combined_allowance")]
    fn report_corrections_do_not_refill_truncation(
        attempts: u32,
        budget: Option<u32>,
        exhausted_rule: &str,
    ) {
        smol::block_on(async {
            let mut responses = vec![
                truncated_response(PARTIAL, FIRST_TURN),
                text_response(SUMMARY, SECOND_TURN),
            ];
            if exhausted_rule == TRUNCATION_RULE {
                responses.push(truncated_response(PROMPT, FIRST_TURN));
            }
            let requests = responses.len();
            let provider = ScriptedProvider::new(responses);
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            let steering = Arc::make_mut(&mut ctx.config.steering);
            steering.rules.truncation.max_attempts = Some(attempts);
            steering.max_recoveries = budget;
            let outcome = run_task(
                &ctx,
                TaskRequest {
                    output_schema: Some(answer_schema()),
                    ..request(TaskIdentity::Derive, None)
                },
            )
            .await;
            assert!(!outcome.success);
            let error = outcome.error.as_deref().unwrap();
            assert!(
                error.contains(
                    &AgentError::SteeringExhausted {
                        rule: exhausted_rule.into()
                    }
                    .to_string()
                )
            );
            assert!(error.contains(if exhausted_rule == TRUNCATION_RULE {
                PROMPT
            } else {
                PARTIAL
            }));
            assert_eq!(observed.lock().unwrap().len(), requests);
            assert_eq!(
                outcome.tokens_used,
                (requests as u64 - 1) * u64::from(FIRST_TURN.total_input() + FIRST_TURN.output)
                    + u64::from(SECOND_TURN.total_input() + SECOND_TURN.output)
            );
        });
    }

    #[test_case(true, MISSING_REPORT_RULE; "inner_then_outer")]
    #[test_case(false, EMPTY_RESPONSE_RULE; "outer_then_inner")]
    fn inner_and_outer_corrections_share_one_allowance(empty_first: bool, exhausted_rule: &str) {
        smol::block_on(async {
            let texts = if empty_first {
                ["", SUMMARY]
            } else {
                [SUMMARY, ""]
            };
            let provider = ScriptedProvider::new(
                texts
                    .into_iter()
                    .map(|text| text_response(text, FIRST_TURN))
                    .collect(),
            );
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            Arc::make_mut(&mut ctx.config.steering).max_recoveries = Some(1);
            let outcome = run_task(
                &ctx,
                TaskRequest {
                    output_schema: Some(answer_schema()),
                    ..request(TaskIdentity::Derive, None)
                },
            )
            .await;
            let exhausted = AgentError::SteeringExhausted {
                rule: exhausted_rule.into(),
            };
            assert!(!outcome.success);
            assert!(
                outcome
                    .error
                    .as_deref()
                    .unwrap()
                    .contains(&exhausted.to_string())
            );
            assert_eq!(observed.lock().unwrap().len(), texts.len());
        });
    }

    #[test]
    fn a_custom_report_correction_is_host_authored() {
        smol::block_on(async {
            let expected = json!({ REQUIRED_FIELD: SUMMARY });
            let provider = ScriptedProvider::new(vec![
                text_response(SUMMARY, FIRST_TURN),
                structured_output_call(expected.clone()),
                text_response("", SECOND_TURN),
            ]);
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            Arc::make_mut(&mut ctx.config.steering)
                .rules
                .missing_task_report
                .prompt = Some(CUSTOM_CORRECTION.into());
            let outcome = run_task(
                &ctx,
                TaskRequest {
                    output_schema: Some(answer_schema()),
                    ..request(TaskIdentity::Derive, None)
                },
            )
            .await;
            assert_eq!(outcome.error, None);
            assert_eq!(outcome.output, expected);
            let observed = observed.lock().unwrap();
            let corrected = &observed[1];
            let guidance = corrected
                .iter()
                .find(|message| {
                    message.content.iter().any(|block| {
                        matches!(block,
                    ContentBlock::Text { text } if text.contains(CUSTOM_CORRECTION))
                    })
                })
                .unwrap();
            assert!(guidance.is_observation());
            assert_eq!(
                guidance.steering.as_ref().unwrap().rule,
                MISSING_REPORT_RULE
            );
            assert_eq!(
                corrected
                    .iter()
                    .filter(|message| super::super::history::is_user_turn(message))
                    .count(),
                1,
            );
        });
    }

    #[test_case(true; "master_disabled")]
    #[test_case(false; "report_rule_disabled")]
    fn disabled_report_correction_returns_the_unmet_contract(master: bool) {
        smol::block_on(async {
            let provider = ScriptedProvider::new(vec![text_response(SUMMARY, FIRST_TURN)]);
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            if master {
                Arc::make_mut(&mut ctx.config.steering).enabled = Some(false);
            } else {
                Arc::make_mut(&mut ctx.config.steering)
                    .rules
                    .missing_task_report
                    .enabled = Some(false);
            }
            let outcome = run_task(
                &ctx,
                TaskRequest {
                    output_schema: Some(answer_schema()),
                    ..request(TaskIdentity::Derive, None)
                },
            )
            .await;
            assert!(!outcome.success);
            assert_eq!(outcome.error.as_deref(), Some(STRUCTURED_MISSING_ERROR));
            assert_eq!(observed.lock().unwrap().len(), 1);
        });
    }

    #[test_case(""; "empty")]
    #[test_case(crate::EMPTY_RESPONSE_MARKER; "literal_marker")]
    fn an_empty_marker_is_not_a_task_summary(text: &str) {
        smol::block_on(async {
            let provider = ScriptedProvider::new(vec![text_response(text, FIRST_TURN)]);
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            Arc::make_mut(&mut ctx.config.steering).enabled = Some(false);
            let outcome = run_task(&ctx, request(TaskIdentity::Derive, None)).await;
            assert!(!outcome.success);
            assert_eq!(outcome.error.as_deref(), Some(SUMMARY_MISSING_ERROR));
            assert_eq!(observed.lock().unwrap().len(), 1);
        });
    }

    #[test_case(None, false; "terminal_provider_error")]
    #[test_case(Some(1), false; "turn_limit")]
    #[test_case(Some(2), true; "empty_tail_at_turn_limit")]
    fn a_valid_report_does_not_swallow_a_later_failure(max_turns: Option<u32>, empty_tail: bool) {
        smol::block_on(async {
            let mut responses = vec![structured_output_call(json!({ REQUIRED_FIELD: SUMMARY }))];
            if empty_tail {
                responses.push(text_response("", SECOND_TURN));
            }
            let provider = ScriptedProvider::new(responses);
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            ctx.config.max_turns = max_turns;
            let outcome = run_task(
                &ctx,
                TaskRequest {
                    output_schema: Some(answer_schema()),
                    ..request(TaskIdentity::Derive, None)
                },
            )
            .await;
            assert!(!outcome.success);
            assert_eq!(outcome.output, Value::Null);
            if max_turns.is_some() {
                assert!(
                    outcome
                        .error
                        .as_deref()
                        .unwrap()
                        .contains(subagent::TURN_LIMIT)
                );
            }
            assert_eq!(
                observed.lock().unwrap().len(),
                max_turns.unwrap_or(2) as usize
            );
            let mut usage = FIRST_TURN;
            if empty_tail {
                usage += SECOND_TURN;
            }
            assert_eq!(
                outcome.tokens_used,
                u64::from(usage.total_input() + usage.output)
            );
            assert_retired(&ctx, outcome.task_id.as_deref().unwrap());
        });
    }

    #[test]
    fn varying_invalid_reports_consume_the_repair_allowance() {
        smol::block_on(async {
            let provider = ScriptedProvider::new(vec![
                structured_output_call(json!({ REQUIRED_FIELD: PARTIAL })),
                structured_output_call(json!({ REQUIRED_FIELD: PROMPT })),
            ]);
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            Arc::make_mut(&mut ctx.config.steering).max_recoveries = Some(1);
            let outcome = run_task(
                &ctx,
                TaskRequest {
                    output_schema: Some(json!({
                        "type": "object",
                        "required": [REQUIRED_FIELD],
                        "properties": { REQUIRED_FIELD: { "type": "string", "const": SUMMARY } },
                    })),
                    ..request(TaskIdentity::Derive, None)
                },
            )
            .await;
            assert!(!outcome.success);
            assert_eq!(outcome.output, Value::Null);
            assert_eq!(observed.lock().unwrap().len(), 2);
            assert!(!outcome.error.as_deref().unwrap().contains(SCRIPT_EXHAUSTED));
            assert_eq!(
                outcome.tokens_used,
                2 * u64::from(FIRST_TURN.total_input() + FIRST_TURN.output)
            );
        });
    }

    #[test_case(None; "explicit_resume")]
    #[test_case(Some(PROMPT); "external_prompt")]
    fn an_external_invocation_refreshes_the_recovery_allowance(message: Option<&str>) {
        smol::block_on(async {
            let stop = StopReason::MaxTokens;
            let text = PARTIAL;
            let incomplete =
                |usage| response(vec![ContentBlock::Text { text: text.into() }], stop, usage);
            let provider = ScriptedProvider::new(vec![
                incomplete(FIRST_TURN),
                incomplete(FIRST_TURN),
                incomplete(SECOND_TURN),
                text_response(SUMMARY, SECOND_TURN),
            ]);
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            let steering = Arc::make_mut(&mut ctx.config.steering);
            steering.rules.empty_response.max_idle = Some(1);
            steering.rules.truncation.max_attempts = Some(1);
            let mut session = OpenSession(
                subagent::open_task(
                    &ctx,
                    subagent::TaskOptions {
                        name: LABEL.into(),
                        task_id: TaskIdentity::Derive,
                        profile: None,
                        mode: None,
                        model_job: None,
                        local_definitions: Vec::new(),
                        local_tools: LocalTools::default(),
                    },
                )
                .await
                .unwrap(),
            );
            let failure = session.0.prompt(Some(PROMPT.into())).await.err().unwrap();
            assert_eq!(
                failure.error,
                AgentError::SteeringExhausted {
                    rule: if stop == StopReason::MaxTokens {
                        TRUNCATION_RULE
                    } else {
                        EMPTY_RESPONSE_RULE
                    }
                    .into()
                }
                .to_string()
            );
            let reply = session
                .0
                .prompt(message.map(str::to_owned))
                .await
                .map_err(failure_message)
                .unwrap();
            assert_eq!(reply.text, format!("{text}{SUMMARY}"));
            let mut expected_usage = FIRST_TURN;
            expected_usage += FIRST_TURN;
            expected_usage += SECOND_TURN;
            expected_usage += SECOND_TURN;
            assert_eq!(session.0.usage(), expected_usage);
            assert_eq!(reply.input_tokens, expected_usage.total_input());
            assert_eq!(reply.output_tokens, expected_usage.output);
            assert_eq!(observed.lock().unwrap().len(), 4);
        });
    }

    /// A stall is the exception: its episode lives in the history tail, so the
    /// next invocation still gets its request answered but not a fresh budget
    /// to keep asking with.
    #[test_case(None; "explicit_resume")]
    #[test_case(Some(PROMPT); "external_prompt")]
    fn an_external_invocation_does_not_refresh_a_stall(message: Option<&str>) {
        smol::block_on(async {
            let empty = |usage| {
                response(
                    vec![ContentBlock::Text {
                        text: String::new(),
                    }],
                    StopReason::EndTurn,
                    usage,
                )
            };
            let provider = ScriptedProvider::new(vec![
                empty(FIRST_TURN),
                empty(FIRST_TURN),
                empty(SECOND_TURN),
                text_response(SUMMARY, SECOND_TURN),
            ]);
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            Arc::make_mut(&mut ctx.config.steering)
                .rules
                .empty_response
                .max_idle = Some(1);
            let mut session = OpenSession(
                subagent::open_task(
                    &ctx,
                    subagent::TaskOptions {
                        name: LABEL.into(),
                        task_id: TaskIdentity::Derive,
                        profile: None,
                        mode: None,
                        model_job: None,
                        local_definitions: Vec::new(),
                        local_tools: LocalTools::default(),
                    },
                )
                .await
                .unwrap(),
            );
            let exhausted = AgentError::SteeringExhausted {
                rule: EMPTY_RESPONSE_RULE.into(),
            }
            .to_string();
            assert_eq!(
                session
                    .0
                    .prompt(Some(PROMPT.into()))
                    .await
                    .err()
                    .unwrap()
                    .error,
                exhausted
            );
            assert_eq!(
                session
                    .0
                    .prompt(message.map(str::to_owned))
                    .await
                    .err()
                    .unwrap()
                    .error,
                exhausted
            );
            // The follow-up is still sent; only the nudge that would have
            // followed another empty answer is refused.
            assert_eq!(observed.lock().unwrap().len(), 3);
        });
    }

    #[test_case(StopReason::EndTurn, false; "new_answer")]
    #[test_case(StopReason::MaxTokens, false; "truncation_chain")]
    #[test_case(StopReason::EndTurn, true; "usage_on_failure")]
    #[test_case(StopReason::MaxTokens, true; "truncation_budget_and_fragments_on_failure")]
    fn subagent_output_and_usage_survive_compaction(stop: StopReason, fail: bool) {
        smol::block_on(async {
            let initial_usage = TokenUsage {
                input: COMPACTION_INPUT_TOKENS,
                ..FIRST_TURN
            };
            let exhausted_truncation = fail && stop == StopReason::MaxTokens;
            let mut responses = Vec::new();
            if exhausted_truncation {
                responses.push(truncated_response(PROMPT, FIRST_TURN));
            }
            responses.extend([
                response(
                    vec![ContentBlock::Text {
                        text: PARTIAL.into(),
                    }],
                    stop,
                    initial_usage,
                ),
                text_response(COMPACTED_SUMMARY, SECOND_TURN),
                if exhausted_truncation {
                    truncated_response(SUMMARY, SECOND_TURN)
                } else {
                    text_response(if fail { "" } else { SUMMARY }, SECOND_TURN)
                },
            ]);
            let provider = ScriptedProvider::new(responses);
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            if exhausted_truncation {
                Arc::make_mut(&mut ctx.config.steering)
                    .rules
                    .truncation
                    .max_attempts = Some(2);
            } else if fail {
                Arc::make_mut(&mut ctx.config.steering).max_recoveries = Some(0);
            }
            let mut model = Model::clone(&ctx.model);
            model.context_window = COMPACTION_CONTEXT_WINDOW;
            ctx.model = Arc::new(model);
            ctx.subagent_history.reserve(FRESH_ID).unwrap().complete(
                (0..COMPACTION_HISTORY_MESSAGES)
                    .map(|_| Message::user(PROMPT.repeat(COMPACTION_HISTORY_REPEATS)))
                    .collect::<Vec<_>>(),
            );
            let mut session = OpenSession(
                subagent::open_generic(
                    &ctx,
                    subagent::GenericOptions {
                        name: LABEL.into(),
                        task_id: Some(FRESH_ID.into()),
                        model_spec: None,
                        system: PROMPT.into(),
                        tools: json!([]),
                        audience: None,
                        thinking: None,
                        fast: None,
                        mcp: Some(false),
                        local_tools: LocalTools::default(),
                    },
                )
                .await
                .unwrap(),
            );
            let reply = session.0.prompt(None).await;
            if fail {
                let failure = reply.err().unwrap();
                assert_eq!(
                    failure.error,
                    AgentError::SteeringExhausted {
                        rule: if exhausted_truncation {
                            TRUNCATION_RULE
                        } else {
                            EMPTY_RESPONSE_RULE
                        }
                        .into()
                    }
                    .to_string()
                );
                if exhausted_truncation {
                    assert_eq!(failure.partial, Some(format!("{PROMPT}{PARTIAL}{SUMMARY}")));
                }
            } else {
                let expected = if stop == StopReason::MaxTokens {
                    format!("{PARTIAL}{SUMMARY}")
                } else {
                    SUMMARY.into()
                };
                assert_eq!(reply.map_err(failure_message).unwrap().text, expected);
            }
            let mut expected_usage = initial_usage;
            if exhausted_truncation {
                expected_usage += FIRST_TURN;
            }
            expected_usage += SECOND_TURN;
            expected_usage += SECOND_TURN;
            assert_eq!(session.0.usage(), expected_usage);
            let observed = observed.lock().unwrap();
            assert_eq!(observed.len(), if exhausted_truncation { 4 } else { 3 });
            let final_request = observed.last().unwrap();
            assert!(final_request.len() < COMPACTION_HISTORY_MESSAGES);
            assert!(
                final_request
                    .iter()
                    .any(|message| message.is_compaction_summary)
            );
        });
    }

    #[test]
    fn resumed_cancellation_retains_only_the_truncation_chain_across_compaction() {
        smol::block_on(async {
            let (trigger, cancel) = CancelToken::new();
            let initial_usage = TokenUsage {
                input: COMPACTION_INPUT_TOKENS,
                ..FIRST_TURN
            };
            let mut commentary = structured_output_call(json!({ REQUIRED_FIELD: PROMPT }));
            commentary.message.content.insert(
                0,
                ContentBlock::Text {
                    text: PROMPT.into(),
                },
            );
            let provider = ScriptedProvider {
                cancel_when_exhausted: Mutex::new(Some(trigger)),
                cancel_partial: Some(SUMMARY),
                ..ScriptedProvider::new(vec![
                    commentary,
                    truncated_response(PARTIAL, initial_usage),
                    text_response(COMPACTED_SUMMARY, SECOND_TURN),
                ])
            };
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            ctx.cancel = cancel;
            Arc::make_mut(&mut ctx.model).context_window = COMPACTION_CONTEXT_WINDOW;
            let mut history: Vec<_> = (0..COMPACTION_HISTORY_MESSAGES)
                .map(|_| Message::user(PROMPT.repeat(COMPACTION_HISTORY_REPEATS)))
                .collect();
            history.push(text_response(BOOM, FIRST_TURN).message);
            ctx.subagent_history
                .reserve(FRESH_ID)
                .unwrap()
                .complete(history);
            let (definition, tools) = tool_for(&answer_schema()).unwrap();
            let mut session = OpenSession(
                subagent::open_generic(
                    &ctx,
                    subagent::GenericOptions {
                        name: LABEL.into(),
                        task_id: Some(FRESH_ID.into()),
                        model_spec: None,
                        system: PROMPT.into(),
                        tools: json!([definition]),
                        audience: None,
                        thinking: None,
                        fast: None,
                        mcp: Some(false),
                        local_tools: tools,
                    },
                )
                .await
                .unwrap(),
            );
            let failure = session.0.prompt(None).await.err().unwrap();
            assert_eq!(failure.error, subagent::CANCELLED);
            let partial = failure.partial.unwrap();
            assert!(
                partial.starts_with(&format!("{PARTIAL}{SUMMARY}")),
                "{partial}"
            );
            for fragment in [PARTIAL, SUMMARY] {
                assert_eq!(partial.matches(fragment).count(), 1);
            }
            for excluded in [PROMPT, BOOM, COMPACTED_SUMMARY] {
                assert!(!partial.contains(excluded), "{partial}");
            }
            let observed = observed.lock().unwrap();
            assert_eq!(observed.len(), 4);
            let final_request = observed.last().unwrap();
            assert!(final_request.len() < COMPACTION_HISTORY_MESSAGES);
            assert!(
                final_request
                    .iter()
                    .any(|message| message.is_compaction_summary)
            );
        });
    }

    #[test_case(false, false; "empty_summary")]
    #[test_case(true, false; "reasoning_only_summary")]
    #[test_case(false, true; "transport_failure")]
    fn failed_compaction_bills_only_completed_requests(reasoning: bool, transport: bool) {
        smol::block_on(async {
            let initial_usage = TokenUsage {
                input: COMPACTION_INPUT_TOKENS,
                ..FIRST_TURN
            };
            let mut responses = vec![truncated_response(PARTIAL, initial_usage)];
            if !transport {
                let content = if reasoning {
                    vec![ContentBlock::Thinking {
                        thinking: COMPACTED_SUMMARY.into(),
                        signature: None,
                        duration_ms: None,
                        interrupted: false,
                        responses: None,
                    }]
                } else {
                    Vec::new()
                };
                responses.push(response(content, StopReason::EndTurn, SECOND_TURN));
            }
            let provider = ScriptedProvider::new(responses);
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            Arc::make_mut(&mut ctx.model).context_window = COMPACTION_CONTEXT_WINDOW;
            let outcome = run_task(&ctx, request(TaskIdentity::Derive, None)).await;
            assert!(!outcome.success && !outcome.cancelled);
            assert_eq!(outcome.output, Value::Null);
            let error = if transport {
                AgentError::Config {
                    message: SCRIPT_EXHAUSTED.into(),
                }
            } else {
                AgentError::EmptySummary
            };
            assert_eq!(
                outcome.error,
                Some(failure_message(subagent::PromptFailure {
                    error: error.to_string(),
                    partial: Some(PARTIAL.into()),
                }))
            );
            let mut expected_usage = initial_usage;
            if !transport {
                expected_usage += SECOND_TURN;
            }
            assert_eq!(
                outcome.tokens_used,
                u64::from(expected_usage.total_input() + expected_usage.output)
            );
            assert_eq!(observed.lock().unwrap().len(), 2);
            assert_retired(&ctx, outcome.task_id.as_deref().unwrap());
        });
    }

    #[test]
    fn a_child_resolves_its_effective_model_policy_independently() {
        smol::block_on(async {
            let expected = json!({ REQUIRED_FIELD: SUMMARY });
            let provider = ScriptedProvider::new(vec![
                text_response(SUMMARY, FIRST_TURN),
                structured_output_call(expected.clone()),
                text_response("", SECOND_TURN),
            ]);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            let mut child_model = Model::clone(&ctx.model);
            child_model.id = CURRENT_CHAT_ID.into();
            ctx.model = Arc::new(child_model);
            Arc::make_mut(&mut ctx.config.steering).max_recoveries = Some(0);
            Arc::make_mut(&mut ctx.config.steering).models.insert(
                ctx.model.spec(),
                SteeringModelConfig {
                    max_recoveries: Some(1),
                    ..Default::default()
                },
            );
            assert_eq!(
                ctx.config
                    .steering
                    .resolve(&ctx.chat_model.spec())
                    .max_recoveries,
                0
            );
            let outcome = run_task(
                &ctx,
                TaskRequest {
                    output_schema: Some(answer_schema()),
                    ..request(TaskIdentity::Derive, None)
                },
            )
            .await;
            assert_eq!(outcome.error, None);
            assert_eq!(outcome.output, expected);
        });
    }

    #[test_case(None; "explicit_resume")]
    #[test_case(Some(PROMPT); "external_prompt")]
    fn an_external_invocation_refreshes_the_report_allowance(message: Option<&str>) {
        smol::block_on(async {
            let provider =
                ScriptedProvider::new((0..4).map(|_| text_response(SUMMARY, FIRST_TURN)).collect());
            let mut ctx = ctx_with(AgentMode::Build, provider);
            Arc::make_mut(&mut ctx.config.steering).max_recoveries = Some(1);
            ctx.config.max_turns = Some(2);
            let mut session = OpenSession(
                subagent::open_task(
                    &ctx,
                    subagent::TaskOptions {
                        name: LABEL.into(),
                        task_id: TaskIdentity::Derive,
                        profile: None,
                        mode: None,
                        model_job: None,
                        local_definitions: Vec::new(),
                        local_tools: LocalTools::default(),
                    },
                )
                .await
                .unwrap(),
            );
            session
                .0
                .prompt(Some(PROMPT.into()))
                .await
                .map_err(failure_message)
                .unwrap();
            assert!(
                session
                    .0
                    .correct_report(false)
                    .await
                    .map_err(failure_message)
                    .unwrap()
                    .is_some()
            );
            assert!(session.0.correct_report(false).await.is_err());
            session
                .0
                .prompt(message.map(str::to_owned))
                .await
                .map_err(failure_message)
                .unwrap();
            assert!(
                session
                    .0
                    .correct_report(false)
                    .await
                    .map_err(failure_message)
                    .unwrap()
                    .is_some()
            );
            assert!(session.0.correct_report(false).await.is_err());
        });
    }

    #[test_case(true; "closed")]
    #[test_case(false; "cancelled")]
    fn report_correction_never_reopens_a_stopped_subagent(closed: bool) {
        smol::block_on(async {
            let provider = ScriptedProvider::new(Vec::new());
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            let (trigger, cancel) = CancelToken::new();
            ctx.cancel = cancel;
            let mut session = OpenSession(
                subagent::open_task(
                    &ctx,
                    subagent::TaskOptions {
                        name: LABEL.into(),
                        task_id: TaskIdentity::Derive,
                        profile: None,
                        mode: None,
                        model_job: None,
                        local_definitions: Vec::new(),
                        local_tools: LocalTools::default(),
                    },
                )
                .await
                .unwrap(),
            );
            if closed {
                session.0.close();
            } else {
                trigger.cancel();
            }
            let error = session.0.correct_report(false).await.err().unwrap();
            assert_eq!(
                error.error,
                if closed {
                    subagent::SESSION_CLOSED
                } else {
                    subagent::CANCELLED
                }
            );
            assert!(observed.lock().unwrap().is_empty());
        });
    }

    #[test_case(false; "valid_report")]
    #[test_case(true; "truncated_response")]
    fn cancellation_keeps_its_streamed_tail_after_a_completed_response(truncated: bool) {
        smol::block_on(async {
            let (trigger, cancel) = CancelToken::new();
            let provider = ScriptedProvider {
                cancel_when_exhausted: Mutex::new(Some(trigger)),
                cancel_partial: Some(PARTIAL),
                ..ScriptedProvider::new(vec![if truncated {
                    truncated_response(SUMMARY, FIRST_TURN)
                } else {
                    structured_output_call(json!({ REQUIRED_FIELD: SUMMARY }))
                }])
            };
            let observed = Arc::clone(&provider.requests);
            let mut ctx = ctx_with(AgentMode::Build, provider);
            ctx.cancel = cancel;
            let outcome = run_task(
                &ctx,
                TaskRequest {
                    output_schema: Some(answer_schema()),
                    ..request(TaskIdentity::Derive, None)
                },
            )
            .await;
            assert!(outcome.cancelled && !outcome.success);
            assert_eq!(outcome.output, Value::Null);
            assert!(outcome.error.as_deref().unwrap().contains(PARTIAL));
            if truncated {
                assert!(outcome.error.as_deref().unwrap().contains(SUMMARY));
            }
            assert_eq!(observed.lock().unwrap().len(), 2);
            assert_eq!(
                outcome.tokens_used,
                u64::from(FIRST_TURN.total_input() + FIRST_TURN.output)
            );
            assert_retired(&ctx, outcome.task_id.as_deref().unwrap());
        });
    }

    #[test]
    fn a_cancelled_run_is_reported_as_such_and_fully_retired() {
        smol::block_on(async {
            let (trigger, cancel) = CancelToken::new();
            let mut ctx = ctx_with(AgentMode::Build, ScriptedProvider::cancelling(trigger));
            ctx.cancel = cancel;

            let outcome = run_task(&ctx, request(TaskIdentity::Derive, None)).await;

            assert!(outcome.cancelled && !outcome.success);
            assert_eq!(
                outcome.error.as_deref(),
                Some(format!("{ERROR_PREFIX}{}", subagent::CANCELLED).as_str())
            );
            assert_eq!(outcome.output, Value::Null);
            assert_retired(&ctx, outcome.task_id.as_deref().unwrap());
        });
    }

    #[test]
    fn a_build_request_under_a_plan_parent_runs_as_a_plan_task() {
        smol::block_on(async {
            let ctx = ctx_with(
                AgentMode::ReadOnly,
                ScriptedProvider::new(vec![text_response(SUMMARY, FIRST_TURN)]),
            );

            let outcome = run_task(
                &ctx,
                request(TaskIdentity::Derive, Some(SubagentTaskMode::Build)),
            )
            .await;

            assert_eq!(outcome.error, None);
            assert_eq!(outcome.output, Value::String(SUMMARY.into()));
            let snapshot = ctx.subagent_history.snapshot();
            let stored = snapshot.records()[outcome.task_id.as_deref().unwrap()]
                .spec()
                .expect("task spec");
            assert_eq!(stored.mode, SubagentTaskMode::Plan);
        });
    }

    #[test]
    fn a_fresh_identity_names_the_task_exactly() {
        smol::block_on(async {
            let ctx = ctx_with(
                AgentMode::Build,
                ScriptedProvider::new(vec![text_response(SUMMARY, FIRST_TURN)]),
            );

            let outcome = run_task(&ctx, request(TaskIdentity::Fresh(FRESH_ID.into()), None)).await;

            assert_eq!(outcome.task_id.as_deref(), Some(FRESH_ID));
            assert_retired(&ctx, FRESH_ID);
        });
    }

    #[test]
    fn a_bad_schema_costs_no_session() {
        smol::block_on(async {
            let ctx = ctx_with(AgentMode::Build, ScriptedProvider::new(Vec::new()));

            let outcome = run_task(
                &ctx,
                TaskRequest {
                    output_schema: Some(json!({ "type": "array" })),
                    ..request(TaskIdentity::Derive, None)
                },
            )
            .await;

            assert_eq!(outcome.task_id, None);
            assert_eq!(outcome.error.as_deref(), Some(SCHEMA_ROOT_ERROR));
            assert!(ctx.subagent_history.snapshot().records().is_empty());
        });
    }
}
