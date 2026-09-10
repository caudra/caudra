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
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Instant;

use arc_swap::ArcSwap;
use async_lock::{Mutex as AsyncMutex, Semaphore};
use caudra_config::{ModelPolicy, ToolOutputLines};
use caudra_providers::model::Model;
use caudra_providers::provider::Provider;
use caudra_providers::{RequestOptions, Timeouts, ToolNameAliases};
use caudra_storage::id::SessionRef;
use caudra_storage::tool_outputs::ToolOutputStore;
use jsonschema::Validator;
use serde_json::Value;
use tracing::info;

use crate::agent::LoadedInstructions;
use crate::agent::run::AgentParams;
use crate::agent::subagent::{self, STRUCTURED_OUTPUT_TOOL, Subagent, TaskIdentity};
use crate::cancel::{CancelMap, CancelToken};
use crate::context::ContextPublisher;
use crate::mcp::McpSession;
use crate::permissions::PermissionManager;
use crate::prompt::ResolvedSlots;
use crate::prompt::profile::PromptProfileCatalog;
use crate::subagent_history::{SubagentHistoryStore, SubagentTaskMode};
use crate::tools::registry::ToolRegistry;
use crate::tools::{
    Deadline, FileReadTracker, LocalToolFn, LocalTools, PathLocks, ToolAudience, ToolContext,
    ToolEffect, ToolFilter,
};
use crate::types::WorkflowProvenance;
use crate::{AgentConfig, AgentMode, EventSender};

const STRUCTURED_OUTPUT_DESCRIPTION: &str =
    "Report your final result. Call it exactly once when your task is complete.";
const STRUCTURED_OUTPUT_ACK: &str = "Output recorded.";
const STRUCTURED_OUTPUT_PROMPT_SUFFIX: &str =
    "\n\nWhen finished, call the structured_output tool with your final result.";
const MAX_NUDGES: usize = 2;
const MAX_SCHEMA_ERRORS: usize = 3;
const SCHEMA_COMPILE_ERROR: &str = "invalid output_schema";
const SCHEMA_ROOT_ERROR: &str = "output_schema must have type object";
const STRUCTURED_MISSING_ERROR: &str = "subagent finished without calling structured_output";
const STRUCTURED_INVALID_ERROR: &str = "subagent result does not match output_schema";
const SUMMARY_MISSING_ERROR: &str = "subagent finished without providing a summary";
const NUDGE_MISSING: &str = "You did not call the structured_output tool. Call it now with your final result matching its input schema.";
const NUDGE_SUMMARY: &str = "You finished your work but did not provide a summary. Reply with a concise summary of what you did and found.";
const INVALID_INPUT_PREFIX: &str =
    "Input does not match the required schema. Fix the errors and call structured_output again:\n";
const INTERRUPTED_PREFIX: &str = "sub-agent interrupted (";
const INTERRUPTED_SUFFIX: &str = "). Partial output:\n";
const ERROR_PREFIX: &str = "sub-agent error: ";

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
    /// JSON Schema (object) the result must match. When set, `output` is the
    /// validated value the subagent reported through `structured_output`.
    pub output_schema: Option<Value>,
    /// Becomes the subagent's `parent_tool_use_id`, which roots its events
    /// and history in the caller's transcript.
    pub call_id: String,
    /// Stamped on every event the subagent emits.
    pub provenance: Option<WorkflowProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskOutcome {
    /// `None` when no subagent was opened, so there is nothing to resume.
    pub task_id: Option<String>,
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
    let ctx = &ToolContext {
        tool_use_id: Some(request.call_id),
        event_tx: match request.provenance {
            Some(provenance) => ctx.event_tx.clone().with_workflow(provenance),
            None => ctx.event_tx.clone(),
        },
        ..ctx.clone()
    };
    // Compile early: a bad schema costs zero tokens.
    let captured = Arc::new(Mutex::new(Captured::default()));
    let validating = request.output_schema.is_some();
    let (local_definitions, local_tools) = match request.output_schema.as_ref() {
        None => (Vec::new(), LocalTools::default()),
        Some(schema) => match structured_output_tool(schema, &captured) {
            Ok((definition, tools)) => (vec![definition], tools),
            Err(message) => return finish(started, None, 0, Err(message.into())),
        },
    };
    let Ok(_permit) = ctx.cancel.race(PERMITS.load().acquire_arc()).await else {
        return finish(started, None, 0, Err(cancelled_failure().into()));
    };
    let mode = clamp_mode(&request.task, request.mode, &ctx.mode);
    let mut session = match subagent::open_task(
        ctx,
        subagent::TaskOptions {
            name: request.label,
            task_id: request.task,
            profile: request.profile,
            mode,
            local_definitions,
            local_tools,
        },
    )
    .await
    {
        Ok(session) => OpenSession(session),
        Err(message) => return finish(started, None, 0, Err(message.into())),
    };
    let task_id = session.0.id().to_owned();
    let verdict = converse(&mut session.0, request.prompt, validating, &captured).await;
    let usage = session.0.usage();
    let tokens_used = u64::from(usage.total_input()) + u64::from(usage.output);
    drop(session);
    finish(started, Some(task_id), tokens_used, verdict)
}

fn finish(
    started: Instant,
    task_id: Option<String>,
    tokens_used: u64,
    verdict: Result<Value, Failure>,
) -> TaskOutcome {
    let duration_ms = started.elapsed().as_millis() as u64;
    match verdict {
        Ok(output) => TaskOutcome {
            task_id,
            success: true,
            cancelled: false,
            output,
            error: None,
            tokens_used,
            duration_ms,
        },
        Err(failure) => TaskOutcome {
            task_id,
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
        (false, Some(SubagentTaskMode::Build), AgentMode::ReadOnly | AgentMode::Plan(_)) => {
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
) -> Result<Value, Failure> {
    let message = prompt.map(|mut message| {
        if validating {
            message.push_str(STRUCTURED_OUTPUT_PROMPT_SUFFIX);
        }
        message
    });
    let mut result = session.prompt(message).await;
    for _ in 0..MAX_NUDGES {
        let Ok(reply) = &result else { break };
        let Some(nudge) = nudge_for(validating, lock(captured).value.is_some(), &reply.text) else {
            break;
        };
        result = session.prompt(Some(nudge.to_owned())).await;
    }

    let text = result?.text;
    Ok(report(
        validating,
        std::mem::take(&mut *lock(captured)),
        text,
    )?)
}

/// What to say to a subagent that finished without reporting, or `None` when
/// it already has. Splitting the run's only decision out of the loop is what
/// makes the nudge policy testable without an agent behind it.
fn nudge_for(validating: bool, reported: bool, text: &str) -> Option<&'static str> {
    match validating {
        true if !reported => Some(NUDGE_MISSING),
        false if text.is_empty() => Some(NUDGE_SUMMARY),
        _ => None,
    }
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
        (false, _) if text.is_empty() => Err(SUMMARY_MISSING_ERROR.to_owned()),
        (false, _) => Ok(Value::String(text)),
    }
}

/// Builds the session-local `structured_output` tool from the caller's schema.
/// The handler is the only writer of `captured`, and it runs on the subagent's
/// turn, which is why the state is shared rather than returned.
fn structured_output_tool(
    schema: &Value,
    captured: &Arc<Mutex<Captured>>,
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
    let handler: LocalToolFn =
        crate::tools::audited_local_tool(ToolEffect::ReadOnly, move |input, _ctx| {
            let result = record(&validator, &captured, input);
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

/// Resolves the provider and model at launch time, because the TUI swaps
/// models while a session runs.
pub type ModelResolver = Arc<dyn Fn() -> (Arc<dyn Provider>, Arc<Model>) + Send + Sync>;

/// The user's mode as it stands when an agent starts, so a workflow that
/// runs for hours is capped by what the user allows now, not at launch.
pub type ModeResolver = Arc<dyn Fn() -> AgentMode + Send + Sync>;

/// The durable part of a session's [`ToolContext`], captured once so the
/// workflow engine can launch agents long after the tool call that would
/// have carried them is gone.
pub struct WorkflowHostContext {
    pub model: ModelResolver,
    pub permissions: Arc<PermissionManager>,
    pub path_locks: Arc<PathLocks>,
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
            path_locks: Arc::clone(&params.path_locks),
            subagent_history: params.subagent_history.clone(),
            registry: Arc::clone(&params.registry),
            config: params.config.clone(),
            mcp: extras.mcp,
            prompt_slots: Arc::clone(&params.prompt_slots),
            prompt_profiles: Arc::clone(&params.prompt_profiles),
            default_task_prompt_profile_name: Arc::clone(&params.default_task_prompt_profile_name),
            model_policy: Arc::clone(&params.model_policy),
            timeouts: params.timeouts,
            tool_output_store: crate::tool_output::default_store(),
            tool_output_lines: params.tool_output_lines,
            session_id: params.session_id.clone(),
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
            path_locks: Arc::clone(&ctx.path_locks),
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
    pub fn tool_context(
        &self,
        cancel: CancelToken,
        events: EventSender,
        call_id: &str,
    ) -> ToolContext {
        let (provider, model) = (self.model)();
        ToolContext {
            provider,
            model,
            event_tx: events,
            mode: (self.mode)(),
            session_id: self.session_id.clone(),
            context_publisher: self.context_publisher.clone(),
            tool_output_store: self.tool_output_store.clone(),
            tool_use_id: Some(call_id.to_owned()),
            root_tool_use_id: Some(call_id.to_owned()),
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
            model_policy: Arc::clone(&self.model_policy),
            workflow: None,
        }
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
    fn run(
        &self,
        request: TaskRequest,
        cancel: CancelToken,
        events: EventSender,
    ) -> TaskFuture<'_> {
        Box::pin(async move {
            let ctx = self.host.tool_context(cancel, events, &request.call_id);
            run_task(&ctx, request).await
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use caudra_providers::{
        AgentError, ContentBlock, Message, ProviderEvent, Role, StopReason, StreamResponse,
        TokenUsage,
    };
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::cancel::CancelTrigger;
    use crate::tools::registry::BoxFuture;
    use crate::tools::test_support::stub_ctx_with;

    const CALL_ID: &str = "toolu_01";
    const FRESH_ID: &str = "wf-run-1-call-7";
    const LABEL: &str = "find auth";
    const PROMPT: &str = "search the codebase";
    const SUMMARY: &str = "found the middleware in src/auth.rs:12";
    const REQUIRED_FIELD: &str = "answer";
    const PARTIAL: &str = "half a transcript";
    const BOOM: &str = "boom";
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
        structured_output_tool(schema, &Arc::new(Mutex::new(Captured::default())))
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

    #[test_case(true, false, "", Some(NUDGE_MISSING); "schema_contract_unmet")]
    #[test_case(true, true, "", None; "schema_contract_met")]
    #[test_case(false, false, "", Some(NUDGE_SUMMARY); "silent_without_a_schema")]
    #[test_case(false, false, SUMMARY, None; "summarised_without_a_schema")]
    #[test_case(true, true, SUMMARY, None; "reported_and_summarised")]
    fn a_subagent_is_nudged_only_when_it_owes_a_report(
        validating: bool,
        reported: bool,
        text: &str,
        expected: Option<&str>,
    ) {
        assert_eq!(nudge_for(validating, reported, text), expected);
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

    #[test]
    fn a_subagent_that_says_nothing_at_all_is_an_error() {
        assert_eq!(
            report(false, Captured::default(), String::new()),
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

    /// Answers each request from a script; hangs on the last when told to,
    /// firing `cancel` first so the run is cut short mid-stream.
    struct ScriptedProvider {
        responses: Mutex<Vec<StreamResponse>>,
        cancel_when_exhausted: Mutex<Option<CancelTrigger>>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<StreamResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                cancel_when_exhausted: Mutex::new(None),
            }
        }

        fn cancelling(trigger: CancelTrigger) -> Self {
            Self {
                responses: Mutex::new(Vec::new()),
                cancel_when_exhausted: Mutex::new(Some(trigger)),
            }
        }
    }

    impl Provider for ScriptedProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                let next = {
                    let mut responses = self.responses.lock().unwrap();
                    (!responses.is_empty()).then(|| responses.remove(0))
                };
                match next {
                    Some(response) => Ok(response),
                    None => {
                        if let Some(trigger) = self.cancel_when_exhausted.lock().unwrap().take() {
                            trigger.cancel();
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

    #[test]
    fn a_structured_result_is_validated_and_returned_as_json() {
        smol::block_on(async {
            let expected = json!({ REQUIRED_FIELD: SUMMARY });
            let ctx = ctx_with(
                AgentMode::Build,
                ScriptedProvider::new(vec![
                    structured_output_call(expected.clone()),
                    text_response(SUMMARY, SECOND_TURN),
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
            assert_eq!(outcome.task_id.as_deref(), Some(CALL_ID));
            let expected_tokens = FIRST_TURN.total_input()
                + FIRST_TURN.output
                + SECOND_TURN.total_input()
                + SECOND_TURN.output;
            assert_eq!(outcome.tokens_used, u64::from(expected_tokens));
            assert_retired(&ctx, CALL_ID);
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
            assert_retired(&ctx, CALL_ID);
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
            let stored = snapshot.records()[CALL_ID].spec().expect("task spec");
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
