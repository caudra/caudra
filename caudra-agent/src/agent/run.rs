use std::borrow::Cow;
use std::env;
use std::slice;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crate::tools::json_repair::RepairState;
use serde_json::Value;
use tracing::{Instrument, debug, error, info, info_span, warn};

use caudra_providers::model_registry::{self, Binding};
use caudra_providers::provider::{self, Provider};
use caudra_providers::{
    Billing, ContentBlock, EMPTY_RESPONSE_MARKER, Message, Model, ModelError, ModelPurpose,
    ReasoningSource, RequestOptions, Role, StopReason, StreamResponse, Timeouts, TokenUsage,
    estimate_tokens_cached,
};

use super::compaction;
use super::goal::{
    Evaluator, GoalApply, GoalHandle, GoalStatus, ResolvedEvaluator, continuation_message,
    is_unrecoverable, resolve_evaluator,
};
use super::history::{
    CANCEL_MARKER, History, repair_tool_pairs, sanitize_cancelled_history, sanitize_failed_history,
};
use super::instructions::LoadedInstructions;
use super::mention_preamble;
use super::provider_projection;
use super::speculative::SpeculativeRuns;
use super::steering::{self, Recovery, RecoveryAction, SharedSteering, Steering};
use super::streaming::{StreamError, stream_with_retry};
use super::title;
use super::tool_dispatch::{self, RecentCalls, ResponseObservations, ToolObservation};
use crate::cancel::{CancelMap, CancelToken};
use crate::context::{
    BuiltinToolsInput, ContextCapture, ContextInventory, ContextPublisher, ContextReadiness,
    ContextSnapshot,
};
use crate::mcp::{McpRequestSnapshot, McpSession};
use crate::nudge::Nudge;
use crate::permissions::PermissionManager;
use crate::template::Vars;
use crate::tools::{BuiltinDeferral, DeferralSession, DeferredTool};
use crate::tools::{Deadline, FileReadTracker, LocalTools, PathLocks, ToolAudience, ToolContext};
use crate::workflow::WorkflowHandle;
use crate::workspace_baseline::BaselineGate;
use crate::{
    AgentConfig, AgentError, AgentEvent, AgentInput, AgentMode, DoneReason, EventSender,
    ExtractedCommand, InterruptSource, Mention, QueueConsumedItem, SessionMailbox,
    SubagentHistoryStore, TurnCompleteEvent,
};
use caudra_config::{ModelPolicy, ToolOutputLines};
use caudra_storage::id::SessionRef;
use caudra_storage::local_documents::LocalDocumentStore;
use caudra_storage::usage_ledger::LedgerPurpose;
use caudra_workspace::WorkspaceSession;

const MAX_REAUTH_ATTEMPTS: u32 = 2;
const AUTH_RELOAD_POLL_MIN_MS: u64 = 250;
const AUTH_RELOAD_POLL_MAX_MS: u64 = 1_000;
const USER_MESSAGE_FRAMING: &str = r#"{"role":"user","content":[]}"#;
const ASSISTANT_MESSAGE_FRAMING: &str = r#"{"role":"assistant","content":[]}"#;
const TEXT_BLOCK_FRAMING: &str = r#"{"type":"text","text":""}"#;
const THINKING_BLOCK_FRAMING: &str = r#"{"type":"thinking","thinking":""}"#;
const THINKING_SIGNATURE_FRAMING: &str = r#"{"signature":""}"#;
const RESPONSES_REASONING_FRAMING: &str = r#"{"item_id":""}"#;
const RESPONSES_ENCRYPTED_CONTENT_FRAMING: &str = r#"{"encrypted_content":""}"#;
const REDACTED_THINKING_BLOCK_FRAMING: &str = r#"{"type":"redacted_thinking","data":""}"#;
const TOOL_USE_BLOCK_FRAMING: &str = r#"{"type":"tool_use","id":"","name":"","input":null}"#;
const TOOL_USE_SIGNATURE_FRAMING: &str = r#"{"thought_signature":""}"#;
const TOOL_RESULT_BLOCK_FRAMING: &str = r#"{"type":"tool_result","tool_use_id":"","content":""}"#;
const TOOL_RESULT_ERROR_FRAMING: &str = r#"{"is_error":true}"#;
const IMAGE_BLOCK_FRAMING: &str =
    r#"{"type":"image","source":{"type":"base64","media_type":"","data":""}}"#;
/// Base64 characters an opaque blob spends per token of what it encodes.
///
/// Signatures and encrypted reasoning travel as ciphertext but are never billed
/// as such: the provider decrypts the item and charges the reasoning inside it.
/// A token is roughly four characters of that plaintext, which is four bytes of
/// ciphertext, which base64 inflates by a third. Counting the armour with a
/// tokenizer instead charges one token per two or three characters, so a
/// reasoning-heavy transcript reads about twice its true size and the
/// conversation appears to overflow a window it fits in.
const OPAQUE_BLOB_CHARS_PER_TOKEN: usize = 5;
/// Without this note a cancelled reply replays in history as a finished
/// turn, and a model resuming its own cut-off text can wedge the session
/// (seen with llama.cpp stuck on an unterminated tool call).
const CANCELLED_TEXT_NOTE: &str = "[Response cut off by user cancel]";
/// Only reached when a resume has no seam to land on, so it says what the
/// assistant tail above it cannot: keep going.
const RESUME_PROMPT: &str =
    "Continue the task from where you left off, and end your turn with a text response.";
/// Reported in place of a spec when nothing is bound to the goal evaluator.
const UNBOUND_EVALUATOR: &str = "default";
const LOCAL_PLAN_WRITE_TOOLS: &str = "`file_write`, `file_edit`, or `file_apply_patch`";
const REMOTE_PLAN_WRITE_TOOLS: &str = "`local_document_write` or `local_document_apply_patch`";

/// Resolves an explicit or global binding against the selected Chat model.
/// With no binding, the caller's effective model is the automatic fallback.
pub fn resolve_purpose_model(
    purpose: ModelPurpose,
    binding_override: Option<&Binding>,
    default_model: &Model,
    chat_model: &Model,
    model_policy: &ModelPolicy,
) -> Result<Model, ModelError> {
    let binding = model_registry::binding(purpose);
    resolve_captured_purpose_model(
        purpose,
        binding_override,
        binding.as_ref(),
        default_model,
        chat_model,
        model_policy,
    )
}

fn resolve_captured_purpose_model(
    purpose: ModelPurpose,
    binding_override: Option<&Binding>,
    purpose_binding: Option<&Binding>,
    default_model: &Model,
    chat_model: &Model,
    model_policy: &ModelPolicy,
) -> Result<Model, ModelError> {
    let binding = binding_override.or(purpose_binding);
    match binding {
        Some(binding) => Model::resolve_binding(purpose, Some(binding), chat_model, model_policy),
        None => Model::resolve_binding(purpose, None, default_model, model_policy),
    }
}

pub struct ModelRoute<'a> {
    pub provider: &'a Arc<dyn Provider>,
    pub model: &'a Model,
}

/// Resolves a purpose and pairs it with a provider ready to serve the model.
/// An existing effective or Chat provider is reused when its model wins.
pub async fn resolve_model_for_purpose(
    default: ModelRoute<'_>,
    chat: ModelRoute<'_>,
    purpose: ModelPurpose,
    binding_override: Option<&Binding>,
    timeouts: Timeouts,
    model_policy: &ModelPolicy,
) -> Result<(Arc<dyn Provider>, Model), AgentError> {
    let default_anchor = default.model.clone();
    let chat_anchor = chat.model.clone();
    let policy = model_policy.clone();
    let binding = binding_override.cloned();
    let mut model = smol::unblock(move || {
        resolve_purpose_model(
            purpose,
            binding.as_ref(),
            &default_anchor,
            &chat_anchor,
            &policy,
        )
    })
    .await
    .map_err(|error| AgentError::Config {
        message: format!("cannot resolve the {purpose} model: {error}"),
    })?;

    if model.provider == default.model.provider && model.id == default.model.id {
        default.provider.adjust_model(&mut model);
        return Ok((Arc::clone(default.provider), model));
    }
    if model.provider == chat.model.provider && model.id == chat.model.id {
        chat.provider.adjust_model(&mut model);
        return Ok((Arc::clone(chat.provider), model));
    }

    let provider = provider::from_model_async(&mut model, timeouts).await?;
    Ok((Arc::from(provider), model))
}

pub fn resolve_compaction_model(
    provider: &Arc<dyn Provider>,
    model: &Model,
    timeouts: caudra_providers::Timeouts,
    model_policy: &ModelPolicy,
) -> Result<(Arc<dyn Provider>, Model), AgentError> {
    let mut compact_model =
        Model::resolve(ModelPurpose::Compact, model, model_policy).map_err(|error| {
            AgentError::Config {
                message: format!("cannot resolve the compaction model: {error}"),
            }
        })?;
    if compact_model.spec() == model.spec() {
        return Ok((Arc::clone(provider), model.clone()));
    }
    let compact_provider = caudra_providers::provider::from_model(&mut compact_model, timeouts)?;
    Ok((Arc::from(compact_provider), compact_model))
}

enum TurnOutcome {
    Continue,
    Done(DoneReason),
}

/// The last request the provider billed, and where the transcript stood when it
/// was sent.
///
/// Auto-compaction and the status bar both need one number for how full the
/// window is, and only the provider can supply it. The estimate is kept for what
/// it is good at — how much the transcript has *grown* since — which needs no
/// tokenizer agreement, only that the same counter answers at both ends.
#[derive(Debug, Clone, Copy)]
struct MeasuredContext {
    reported: u32,
    history_len: usize,
}

impl MeasuredContext {
    /// `None` once the transcript is shorter than it was when the count was
    /// taken: compaction and rollback replace what was billed, so the anchor no
    /// longer describes anything and would otherwise hold a stale high count.
    fn extended_by(self, history: &[Message]) -> Option<u32> {
        let appended = history.get(self.history_len..)?;
        Some(
            self.reported
                .saturating_add(estimate_message_tokens(appended)),
        )
    }
}

#[derive(Clone)]
pub struct AgentParams {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
    pub chat_provider: Arc<dyn Provider>,
    pub chat_model: Model,
    pub config: AgentConfig,
    pub tool_output_lines: ToolOutputLines,
    pub permissions: Arc<PermissionManager>,
    pub session_id: Option<SessionRef>,
    pub workspace_session: Option<WorkspaceSession>,
    pub remote_project_context: Option<Arc<crate::remote_project_context::RemoteProjectContext>>,
    pub local_documents: Option<Arc<LocalDocumentStore>>,
    pub task_environment: Vars,
    pub root_tool_use_id: Option<String>,
    pub mailbox: Option<SessionMailbox>,
    pub context_publisher: Option<ContextPublisher>,
    pub timeouts: caudra_providers::Timeouts,
    pub file_tracker: Arc<FileReadTracker>,
    pub path_locks: Arc<PathLocks>,
    /// The run's revert point, captured on its first mutating tool call.
    pub baseline: Option<BaselineGate>,
    pub prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    pub prompt_profiles: Arc<crate::prompt::profile::PromptProfileCatalog>,
    pub default_task_prompt_profile_name: Arc<str>,
    pub active_prompt_profile_name: Option<Arc<str>>,
    pub subagent_cancels: Arc<CancelMap<String>>,
    pub subagent_history: SubagentHistoryStore,
    pub registry: Arc<crate::tools::ToolRegistry>,
    pub audience: ToolAudience,
    pub tool_filter: crate::tools::ToolFilter,
    pub model_policy: Arc<ModelPolicy>,
    /// The session's workflow runtime, reachable through the `workflow` tool.
    pub workflow: Option<WorkflowHandle>,
}

pub struct AgentRunParams<'h> {
    pub history: &'h mut History,
    pub system: String,
    /// `None` for a subagent, whose task prompt carries its own environment
    /// section and which is too short-lived for the announcement to pay.
    pub environment: Option<String>,
    /// A diff against the instruction files `system` quotes, when they have
    /// since changed on disk. `None` when they match, and for a subagent, which
    /// reads them fresh at spawn.
    pub instructions: Option<String>,
    pub event_tx: EventSender,
    pub tools: Value,
    /// Definitions the request holds back until `tool_search` loads them.
    /// Built by the same caller as `tools`, from the same vars and filter.
    pub deferred: Vec<DeferredTool>,
}

pub struct Agent<'h> {
    provider: Arc<dyn Provider>,
    model: Arc<Model>,
    chat_provider: Arc<dyn Provider>,
    chat_model: Arc<Model>,
    history: &'h mut History,
    system: String,
    environment: Option<String>,
    instructions: Option<String>,
    event_tx: EventSender,
    tools: Value,
    deferral: DeferralSession,
    mode: AgentMode,
    user_response_rx: Option<Arc<async_lock::Mutex<flume::Receiver<String>>>>,
    interrupt_source: Option<Arc<dyn InterruptSource>>,
    cancel: CancelToken,
    retry_now: Nudge,
    total_usage: TokenUsage,
    measured: Option<MeasuredContext>,
    num_turns: u32,
    recent_calls: RecentCalls,
    /// What the last response called its tools, kept because the next one
    /// names them the same way. Only the response carries the map, and a
    /// `batch` child started mid-stream needs it a response early.
    tool_name_aliases: Option<caudra_providers::ToolNameAliases>,
    /// The children of this turn's `batch` calls, started as their arguments
    /// arrived rather than after the message closed.
    speculative: Option<Arc<SpeculativeRuns>>,
    steering: SharedSteering,
    shared_steering: bool,
    report_ready: Option<Arc<AtomicBool>>,
    response_text: Option<String>,
    continuing_response: bool,
    auto_compact: bool,
    loaded_instructions: LoadedInstructions,
    rollback_len: usize,
    mcp: Option<McpSession>,
    config: AgentConfig,
    tool_output_lines: ToolOutputLines,
    reauth_attempts: u32,
    permissions: Arc<PermissionManager>,
    opts: RequestOptions,
    session_id: Option<SessionRef>,
    workspace_session: Option<WorkspaceSession>,
    remote_project_context: Option<Arc<crate::remote_project_context::RemoteProjectContext>>,
    local_documents: Option<Arc<LocalDocumentStore>>,
    task_environment: Vars,
    /// Numbers each turn so every log line inside one can be correlated.
    turn_id: u64,
    root_tool_use_id: Option<String>,
    mailbox: Option<SessionMailbox>,
    context_publisher: Option<ContextPublisher>,
    timeouts: caudra_providers::Timeouts,
    file_tracker: Arc<FileReadTracker>,
    path_locks: Arc<PathLocks>,
    baseline: Option<BaselineGate>,
    prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    prompt_profiles: Arc<crate::prompt::profile::PromptProfileCatalog>,
    default_task_prompt_profile_name: Arc<str>,
    active_prompt_profile_name: Option<Arc<str>>,
    subagent_cancels: Arc<crate::cancel::CancelMap<String>>,
    subagent_history: SubagentHistoryStore,
    registry: Arc<crate::tools::ToolRegistry>,
    audience: ToolAudience,
    tool_filter: crate::tools::ToolFilter,
    local_tools: LocalTools,
    model_policy: Arc<ModelPolicy>,
    workflow: Option<WorkflowHandle>,
    goal: GoalHandle,
    goal_evaluator: Option<ResolvedEvaluator>,
    goal_blocks: u32,
    wait_for_background: bool,
}

impl<'h> Agent<'h> {
    pub fn new(params: AgentParams, run: AgentRunParams<'h>) -> Self {
        let shared_route = Arc::ptr_eq(&params.provider, &params.chat_provider)
            && params.model.provider == params.chat_model.provider
            && params.model.id == params.chat_model.id;
        let mut model = params.model;
        params.provider.adjust_model(&mut model);
        let chat_model = if shared_route {
            model.clone()
        } else {
            let mut chat_model = params.chat_model;
            params.chat_provider.adjust_model(&mut chat_model);
            chat_model
        };
        // Seeded before the history moves in: a restored session keeps the
        // tools it already searched for rather than hunting them again.
        let deferral = DeferralSession::new(
            run.deferred,
            crate::tools::deferral::loaded_tool_names(run.history.as_slice()),
        );
        let model = Arc::new(model);
        let chat_model = if shared_route {
            Arc::clone(&model)
        } else {
            Arc::new(chat_model)
        };
        let steering = Steering::new(params.config.steering.resolve(&model.spec()));
        let recent_calls = RecentCalls::with_threshold(steering.repeat_threshold());
        Self {
            provider: params.provider,
            model,
            chat_provider: params.chat_provider,
            chat_model,
            config: params.config,
            tool_output_lines: params.tool_output_lines,
            permissions: params.permissions,
            timeouts: params.timeouts,
            history: run.history,
            system: run.system,
            environment: run.environment,
            instructions: run.instructions,
            event_tx: run.event_tx,
            tools: run.tools,
            deferral,
            mode: AgentMode::default(),
            user_response_rx: None,
            interrupt_source: None,
            cancel: CancelToken::none(),
            retry_now: Nudge::default(),
            total_usage: TokenUsage::default(),
            measured: None,
            num_turns: 0,
            recent_calls,
            tool_name_aliases: None,
            speculative: None,
            steering: Arc::new(Mutex::new(steering)),
            shared_steering: false,
            report_ready: None,
            response_text: None,
            continuing_response: false,
            auto_compact: compaction::auto_compact_enabled(),
            loaded_instructions: LoadedInstructions::new(),
            rollback_len: 0,
            mcp: None,
            reauth_attempts: 0,
            opts: RequestOptions::default(),
            session_id: params.session_id,
            workspace_session: params.workspace_session,
            remote_project_context: params.remote_project_context,
            local_documents: params.local_documents,
            task_environment: params.task_environment,
            turn_id: 0,
            root_tool_use_id: params.root_tool_use_id,
            mailbox: params.mailbox,
            context_publisher: params.context_publisher,
            file_tracker: params.file_tracker,
            path_locks: params.path_locks,
            baseline: params.baseline,
            prompt_slots: params.prompt_slots,
            prompt_profiles: params.prompt_profiles,
            default_task_prompt_profile_name: params.default_task_prompt_profile_name,
            active_prompt_profile_name: params.active_prompt_profile_name,
            subagent_cancels: params.subagent_cancels,
            subagent_history: params.subagent_history,
            registry: params.registry,
            audience: params.audience,
            tool_filter: params.tool_filter,
            local_tools: LocalTools::default(),
            model_policy: params.model_policy,
            workflow: params.workflow,
            goal: GoalHandle::default(),
            goal_evaluator: None,
            goal_blocks: 0,
            wait_for_background: false,
        }
    }

    pub fn with_mcp(mut self, mcp: Option<McpSession>) -> Self {
        self.mcp = mcp;
        self
    }

    pub(crate) fn with_steering(mut self, steering: SharedSteering) -> Self {
        self.recent_calls =
            RecentCalls::with_threshold(steering::lock(&steering).repeat_threshold());
        self.steering = steering;
        self.shared_steering = true;
        self
    }

    pub(crate) fn with_report_ready(mut self, ready: Arc<AtomicBool>) -> Self {
        self.report_ready = Some(ready);
        self
    }

    pub fn response_text(&self) -> Option<&str> {
        self.response_text.as_deref()
    }

    fn record_response_text(&mut self, text: Option<String>) {
        // Only truncation joins requests into one answer. This also preserves an
        // interrupted continuation across compaction without replaying commentary.
        if self.continuing_response {
            if let Some(text) = text {
                self.response_text.get_or_insert_default().push_str(&text);
            }
        } else {
            self.response_text = text;
        }
    }

    pub(crate) fn usage(&self) -> TokenUsage {
        self.total_usage
    }

    fn readjust_model(&mut self) {
        let chat_follows_model = Arc::ptr_eq(&self.model, &self.chat_model);
        self.provider.adjust_model(Arc::make_mut(&mut self.model));
        if chat_follows_model {
            self.chat_model = Arc::clone(&self.model);
        }
        let mut steering = steering::lock(&self.steering);
        if steering.bind_model(&self.model, &self.config.steering) {
            self.recent_calls = RecentCalls::with_threshold(steering.repeat_threshold());
        }
    }

    pub fn with_user_response_rx(
        mut self,
        rx: Arc<async_lock::Mutex<flume::Receiver<String>>>,
    ) -> Self {
        self.user_response_rx = Some(rx);
        self
    }

    pub fn with_interrupt_source(mut self, source: Arc<dyn InterruptSource>) -> Self {
        self.interrupt_source = Some(source);
        self
    }

    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Lets a waiting retry be cut short from outside the loop.
    pub fn with_retry_now(mut self, retry_now: Nudge) -> Self {
        self.retry_now = retry_now;
        self
    }

    pub fn with_local_tools(mut self, local_tools: LocalTools) -> Self {
        self.local_tools = local_tools;
        self
    }

    pub fn with_loaded_instructions(mut self, loaded: LoadedInstructions) -> Self {
        self.loaded_instructions = loaded;
        self
    }

    pub fn with_goal(mut self, goal: GoalHandle) -> Self {
        self.goal = goal;
        self
    }

    pub fn with_background_wait(mut self) -> Self {
        self.wait_for_background = true;
        self
    }

    /// Cancellation is an ending, not a failure: it comes back as
    /// `Ok(DoneReason::Cancelled)` so callers only report real errors.
    pub async fn run(&mut self, input: AgentInput) -> Result<DoneReason, AgentError> {
        self.run_inputs(vec![input], false).await
    }

    pub async fn run_batch(
        &mut self,
        first: AgentInput,
        rest: Vec<AgentInput>,
    ) -> Result<DoneReason, AgentError> {
        let mut inputs = Vec::with_capacity(rest.len() + 1);
        inputs.push(first);
        inputs.extend(rest);
        self.run_inputs(inputs, true).await
    }

    pub async fn run_initial_batch(
        &mut self,
        first: AgentInput,
        rest: Vec<AgentInput>,
    ) -> Result<DoneReason, AgentError> {
        let mut inputs = Vec::with_capacity(rest.len() + 1);
        inputs.push(first);
        inputs.extend(rest);
        self.run_inputs(inputs, false).await
    }

    /// One span per turn. Everything the turn awaits inherits `session_id`,
    /// `turn_id`, and `model`, which is what makes a log line from deep inside
    /// tool dispatch attributable.
    async fn run_inputs(
        &mut self,
        inputs: Vec<AgentInput>,
        queued: bool,
    ) -> Result<DoneReason, AgentError> {
        self.turn_id += 1;
        let span = info_span!(
            "turn",
            session_id = self.session_id.as_ref().map(SessionRef::as_str),
            turn_id = self.turn_id,
            model = %self.model.id,
        );
        self.run_turn(inputs, queued).instrument(span).await
    }

    async fn run_turn(
        &mut self,
        inputs: Vec<AgentInput>,
        queued: bool,
    ) -> Result<DoneReason, AgentError> {
        self.goal_blocks = 0;
        self.response_text = None;
        self.continuing_response = false;
        if !self.shared_steering {
            let state = Steering::new(self.config.steering.resolve(&self.model.spec()));
            self.recent_calls = RecentCalls::with_threshold(state.repeat_threshold());
            self.steering = Arc::new(Mutex::new(state));
        }
        {
            let mut steering = steering::lock(&self.steering);
            if steering.bind_model(&self.model, &self.config.steering) {
                self.recent_calls = RecentCalls::with_threshold(steering.repeat_threshold());
            }
        }
        if inputs.last().is_some_and(|input| input.resume) {
            self.prepare_resume();
        }
        self.rollback_len = self.history.len();
        let message = self.push_user_inputs(inputs, queued).await;

        info!(
            model = %self.model.id,
            mode = ?self.mode,
            message_len = message.len(),
            "agent run started"
        );
        // Subagents are prompted by the machine inside the parent's window;
        // counting them would inflate prompts and busy time.
        let top_level = self.audience.contains(ToolAudience::MAIN);
        if top_level {
            // Tabs and frontends share one process, so attribution follows
            // whichever session actually runs a turn, not whoever last
            // called `set_session_id`.
            if let Some(session) = &self.session_id {
                caudra_otel::set_session_id(session.as_str());
            }
            caudra_otel::emit::user_prompt(&message);
        }

        if self.should_generate_title(&message) {
            self.spawn_title(message);
        }

        // Every frontend enters here, so busy time is measured here; a turn
        // that failed was still busy.
        let busy_since = Instant::now();
        let result = self.run_loop().await;
        if let Some(runs) = self.speculative.clone() {
            runs.abandon_unfinished();
            self.drain_repair_usage(&runs.repair_state());
        }
        if top_level {
            caudra_otel::emit::active_time(busy_since.elapsed());
        }
        let reason = match result {
            Ok(reason) => reason,
            Err(AgentError::Cancelled) => {
                if sanitize_cancelled_history(self.history, self.rollback_len) {
                    let _ = self.event_tx.send(AgentEvent::Injected {
                        text: CANCEL_MARKER.into(),
                    });
                }
                self.publish_prepared_context();
                DoneReason::Cancelled
            }
            Err(e) => {
                if is_unrecoverable(&e)
                    && let Some(goal) = self.goal.clear()
                {
                    self.event_tx.send(AgentEvent::GoalClearedAfterError {
                        condition: goal.condition.to_string(),
                        message: e.to_string(),
                    })?;
                }
                // The turn dies here, but whatever it already wrote stays. Closing it on a
                // marker is what stops a reloaded session from ending mid-task on a tool
                // result with nothing to say why.
                if let Some(text) =
                    sanitize_failed_history(self.history, self.rollback_len, &e.user_message())
                {
                    let _ = self.event_tx.send(AgentEvent::Injected { text });
                }
                self.publish_prepared_context();
                return Err(e);
            }
        };
        self.emit_done(reason)?;

        Ok(reason)
    }

    /// Only the prompt that opens a session can name it: `rollback_len` is the
    /// history length before this run, so zero means nothing came before. A
    /// resumed session already carries a title and never asks for another.
    fn should_generate_title(&self, prompt: &str) -> bool {
        self.config.generate_titles
            && self.audience.contains(ToolAudience::MAIN)
            && self.root_tool_use_id.is_none()
            && self.session_id.is_some()
            && self.rollback_len == 0
            && !prompt.trim().is_empty()
    }

    /// Detached: nobody waits for a title, and a turn must not pay for one.
    /// The event can therefore land after the run that triggered it ended.
    ///
    /// Which is why the title gets its own token rather than the run's: the
    /// run's trigger fires when it is dropped at the end of the turn, so
    /// sharing it cancels every title that outlives the prompt it names.
    /// [`TITLE_TIMEOUT`](title) is what bounds the request instead.
    fn spawn_title(&self, prompt: String) {
        let provider = Arc::clone(&self.provider);
        let model = Arc::clone(&self.model);
        let timeouts = self.timeouts;
        let model_policy = Arc::clone(&self.model_policy);
        let cancel = CancelToken::none();
        let session_id = self.session_id.clone();
        let event_tx = self.event_tx.clone();
        smol::spawn(async move {
            if let Ok((resolved, outcome)) = title::for_prompt(
                &provider,
                &model,
                timeouts,
                &model_policy,
                &prompt,
                &cancel,
                session_id.as_ref(),
            )
            .await
            {
                event_tx.try_send(AgentEvent::SessionTitle {
                    title: outcome.title,
                    usage: outcome.usage,
                    cost: resolved.model.billed_cost(&outcome.usage, false),
                    billing: resolved.model.billing,
                    model: resolved.model.id.clone(),
                    provider: resolved.model.provider.to_string(),
                });
            }
        })
        .detach();
    }

    /// A resume adds no turn of its own, so the request still has to end
    /// somewhere the model can answer from. Dropping the cancel marker puts it
    /// back on the tool result the loop stopped at; when the reply itself was
    /// cut there is no such seam, and an assistant tail would go out as a
    /// prefill, which providers reject once reasoning is on.
    fn prepare_resume(&mut self) {
        self.history.drop_run_marker();
        if self
            .history
            .as_slice()
            .last()
            .is_none_or(|message| matches!(message.role, Role::Assistant))
        {
            self.push_injected(Message::synthetic(RESUME_PROMPT.into()));
        }
    }

    /// Two kinds of harness-authored message bracket the turn, and which side
    /// they land on follows from whether anything else still holds a copy.
    ///
    /// What arrived since the last turn leads the message: a shell result, a
    /// finished background task, a settled workflow, an MCP prompt's canned
    /// exchange. It happened before the user typed, its source dropped it the
    /// moment it was claimed, and so a rewind that cut it would destroy it.
    ///
    /// What the turn needs to know trails the message: the environment, an
    /// instruction diff, the mode, the bodies behind the mentioned paths. Every
    /// one is re-derived whenever the transcript lacks it, so a rewind may take
    /// it freely, and trailing puts it closest to the point the model generates
    /// from, which is what makes the most recent block of a kind the one in
    /// force.
    async fn push_user_inputs(&mut self, mut inputs: Vec<AgentInput>, queued: bool) -> String {
        let Some(latest) = inputs.last() else {
            return String::new();
        };
        let mut standing = Vec::from_iter(standing_notice(
            self.history.as_slice(),
            crate::prompt::ENVIRONMENT_MARKER,
            self.environment.as_deref(),
        ));
        standing.extend(standing_notice(
            self.history.as_slice(),
            crate::prompt::INSTRUCTIONS_CHANGED_MARKER,
            self.instructions.as_deref(),
        ));
        standing.extend(mode_switch_notice(self.history.as_slice(), &latest.mode));
        self.mode = latest.mode.clone();
        self.opts = RequestOptions {
            thinking: latest.thinking.clone(),
            fast: latest.fast,
        };

        let mut arrivals = Vec::new();
        for input in &mut inputs {
            arrivals.append(&mut input.preamble);
            standing.append(&mut self.mention_preamble(&input.mentions).await);
        }
        self.push_arrivals(arrivals);

        let mut telemetry = Vec::with_capacity(inputs.len());
        for input in inputs {
            if input.message.trim().is_empty() && input.images.is_empty() {
                continue;
            }
            telemetry.push(input.message.clone());
            let message = if queued {
                let display = input.message;
                let wrapped = queued_message(&display);
                Message::user_display_with_images(wrapped, display, input.images)
            } else {
                Message::user_with_images(input.message, input.images)
            };
            self.history.push(message);
        }
        for message in standing {
            self.push_injected(message);
        }
        telemetry.join("\n\n")
    }

    /// Resolves the caller's mentions into hidden context. A slice gets a
    /// throwaway file tracker because Workcell records every successful read
    /// against the tracker it is given, and seeing 20 lines must not clear a
    /// later edit's staleness check on the whole file.
    async fn mention_preamble(&self, mentions: &[Mention]) -> Vec<Message> {
        if mentions.is_empty() {
            return Vec::new();
        }
        let slice = ToolContext {
            file_tracker: FileReadTracker::fresh(),
            ..self.tool_context()
        };
        mention_preamble::build(
            mentions,
            mention_preamble::Resolution {
                root: &self.permissions.project_cwd(),
                registry: &self.registry,
                whole_file: &self.tool_context(),
                slice: &slice,
                vision: self.model.supports_vision(),
                remote_context: self.remote_project_context.as_ref(),
            },
        )
        .await
    }

    /// The mailbox drains here because a notice waiting in it is an arrival by
    /// the same definition as the rest: it landed before the turn and the
    /// mailbox no longer holds it.
    ///
    /// A restored transcript therefore draws these above the message, while a
    /// live one draws them below it, because the row is emitted when the run
    /// claims the notice rather than when the notice arrived. The restored order
    /// is the true one. Closing the gap means drawing the row on arrival, not
    /// reordering what the model was sent.
    fn push_arrivals(&mut self, arrivals: Vec<Message>) {
        for message in arrivals {
            self.push_injected(message);
        }
        if let Some(mailbox) = &self.mailbox {
            for message in mailbox.drain() {
                self.push_injected(message);
            }
        }
    }

    /// Every harness-authored user message goes through here, so the transcript
    /// can show what was injected rather than only that something was. Mention
    /// preambles are pushed silently: the user already sees the path they typed,
    /// and the file body would swamp the rows that carry new information.
    fn push_injected(&mut self, message: Message) {
        if !message.is_mention()
            && let Some(ContentBlock::Text { text }) = message.content.first()
        {
            let _ = self
                .event_tx
                .send(AgentEvent::Injected { text: text.clone() });
        }
        self.history.push(message);
    }

    async fn run_loop(&mut self) -> Result<DoneReason, AgentError> {
        let mut initial = true;
        loop {
            if self.cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            if steering::lock(&self.steering).turn_limit_reached(self.config.max_turns) {
                if let Some(goal) = self.goal.snapshot() {
                    self.event_tx.send(AgentEvent::GoalTurnLimit {
                        evaluations: goal.evaluations,
                    })?;
                }
                return Ok(DoneReason::MaxTurns);
            }
            if initial {
                self.inject_advisory();
                initial = false;
            }
            match self.turn().await? {
                TurnOutcome::Continue => {}
                TurnOutcome::Done(reason) => return Ok(reason),
            }
        }
    }

    /// `self.tools` holds the declared base only; deferred built-ins and MCP
    /// are recomputed here every turn so `tool_search` loads and
    /// late-connecting servers take effect on the next request.
    ///
    /// Both extensions append, which keeps the base a prefix of the result
    /// and gives token accounting a stable boundary to attribute against.
    fn request_tools(&self) -> (Cow<'_, Value>, Option<McpRequestSnapshot>) {
        if self.mcp.is_none() && self.deferral.is_empty() {
            return (Cow::Borrowed(&self.tools), None);
        }
        let mut tools = self.tools.clone();
        let mut sections: Vec<String> = self
            .deferral
            .request_snapshot()
            .extend_declared(&mut tools)
            .into_iter()
            .collect();
        let snapshot = self.mcp.as_ref().map(|mcp| {
            let snapshot = mcp.request_snapshot();
            sections.extend(snapshot.extend_declared(&mut tools));
            snapshot
        });
        crate::tools::deferral::push_catalog(&mut tools, &sections);
        (Cow::Owned(tools), snapshot)
    }

    fn projected_history<'a>(&'a self, tools: &Value) -> Cow<'a, [Message]> {
        repair_tool_pairs(provider_projection::project_for_target(
            self.history.as_slice(),
            tools,
            &self.model,
            self.provider.reasoning_transport(&self.model),
        ))
    }

    fn publish_context(
        &self,
        readiness: ContextReadiness,
        full_tools: &Value,
        projected_messages: &[Message],
        mcp: Option<&McpRequestSnapshot>,
    ) {
        let Some(publisher) = &self.context_publisher else {
            return;
        };
        let options = self.opts.clamped(&self.model);
        let task_profiles = self.prompt_profiles.bind_for_tasks(
            &self.model,
            &self.chat_model,
            &options.thinking,
            &self.model_policy,
            self.timeouts,
        );
        let cwd = env::current_dir().unwrap_or_else(|_| self.permissions.project_cwd());
        let inventory = ContextInventory::collect(
            &cwd,
            &self.registry,
            &self.prompt_profiles,
            &task_profiles,
            self.active_prompt_profile_name.as_deref(),
            Some(&BuiltinToolsInput {
                registry: &self.registry,
                filter: &self.tool_filter,
                config: &self.config,
                model: &self.model,
                deferral: BuiltinDeferral::resolve(&self.config, &self.model),
                deferred: self.deferral.definitions(),
            }),
            mcp,
        );
        publisher.publish(ContextSnapshot::capture(ContextCapture {
            readiness,
            model: &self.model,
            auto_compact: self.auto_compact,
            compaction_buffer: self.config.compaction_buffer,
            system: &self.system,
            base_tools: &self.tools,
            full_tools,
            projected_messages,
            measured: self.context_size(),
            inventory,
        }));
    }

    fn publish_prepared_context(&self) {
        if self.context_publisher.is_none() {
            return;
        }
        let (tools, mcp) = self.request_tools();
        let provider_history = self.projected_history(tools.as_ref());
        self.publish_context(
            ContextReadiness::PreparedNextRequest,
            tools.as_ref(),
            provider_history.as_ref(),
            mcp.as_ref(),
        );
    }

    fn push_assistant_message(&mut self, message: Message) {
        self.history.push(message);
        self.publish_prepared_context();
    }

    async fn turn(&mut self) -> Result<TurnOutcome, AgentError> {
        if self.cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        // Whatever the previous turn started and nobody took is reported
        // before this request, not after the one that abandoned it: the model
        // must learn what already ran before it decides what to run next.
        self.report_speculative();
        let sent_at_history_len = self.history.len();
        self.tool_name_aliases = None;
        let repair_state = Arc::new(RepairState::default());
        self.speculative = self.config.eager_tool_dispatch.then(|| {
            let ctx = ToolContext {
                json_repair: Arc::clone(&repair_state),
                steering_observations: Some(ResponseObservations::new(
                    steering::lock(&self.steering).observation_window(),
                )),
                ..self.tool_context()
            };
            Arc::new(
                SpeculativeRuns::new(&ctx, self.mcp.clone()).with_recent(self.recent_calls.clone()),
            )
        });
        let stream_result = {
            let (tools, mcp) = self.request_tools();
            repair_state.register_definitions(tools.as_ref());
            let provider_history = self.projected_history(tools.as_ref());
            self.publish_context(
                ContextReadiness::CapturedCurrentRequest,
                tools.as_ref(),
                provider_history.as_ref(),
                mcp.as_ref(),
            );
            stream_with_retry(
                &*self.provider,
                &self.model,
                provider_history.as_ref(),
                &self.system,
                tools.as_ref(),
                &self.event_tx,
                &self.cancel,
                &self.retry_now,
                self.opts.clone(),
                self.session_id.as_ref(),
                self.speculative.as_ref(),
            )
            .await
        };
        self.drain_repair_usage(&repair_state);
        let mut interrupted = None;
        let mut response = match stream_result {
            Err(StreamError::Partial { response, error }) => {
                interrupted = Some(error);
                *response
            }
            Ok(r) => {
                self.reauth_attempts = 0;
                r
            }
            Err(StreamError::Cancelled {
                streamed,
                reasoning,
            }) => {
                let streamed = streamed.trim_end();
                let mut content: Vec<_> =
                    reasoning
                        .into_iter()
                        .filter(|run| !run.text.is_empty())
                        .map(|run| ContentBlock::Thinking {
                            thinking: run.text,
                            signature: None,
                            duration_ms: Some(
                                run.duration.as_millis().min(u128::from(u64::MAX)) as u64
                            ),
                            interrupted: true,
                            responses: None,
                        })
                        .collect();
                if !streamed.is_empty() {
                    let text = format!("{streamed}\n\n{CANCELLED_TEXT_NOTE}");
                    self.record_response_text(Some(text.clone()));
                    content.push(ContentBlock::Text { text });
                }
                if !content.is_empty() {
                    self.history.push(Message {
                        role: Role::Assistant,
                        content,
                        reasoning_source: Some(ReasoningSource::new(
                            &self.model,
                            self.provider.reasoning_transport(&self.model),
                        )),
                        ..Default::default()
                    });
                }
                return Err(AgentError::Cancelled);
            }
            Err(StreamError::Auth { error, forwarded }) => {
                return self.wait_for_reauth(error, forwarded).await;
            }
            Err(StreamError::Other(e)) => {
                error!(error = %e, model = %self.model.id, turns = self.num_turns, "stream_message failed");
                return Err(e);
            }
        };
        self.num_turns += 1;

        let has_tools = response.message.has_tool_calls();
        let stop_reason = response.stop_reason;
        info!(
            input_tokens = response.usage.input,
            output_tokens = response.usage.output,
            cache_creation = response.usage.cache_creation,
            cache_read = response.usage.cache_read,
            has_tools,
            self.num_turns,
            model = %self.model.id,
            stop_reason = stop_reason.map_or("none", Into::into),
            "API response received"
        );

        if interrupted.is_none() {
            self.measured = Some(MeasuredContext {
                reported: response.usage.total_input(),
                history_len: sent_at_history_len,
            });
        }
        let usage = response.usage;
        self.total_usage += usage;
        self.emit_turn_complete(&response)?;
        self.goal.record_usage(
            usage,
            self.model
                .billed_cost(&usage, self.opts.clamped(&self.model).fast),
            self.model.billing,
        );

        // Settle the response before deciding whether another request is needed.
        // Tool feedback is already sufficient for repair; charging it must not
        // add a second prompt or replay successful siblings.
        let protocol = !has_tools && stop_reason == Some(StopReason::ToolUse);
        // A synthetic resume preserves the transcript's padding tail, but starts
        // a new episode. Only empties from this invocation can spend its allowance;
        // automatic corrections and queued input retain the same response count.
        let nudges = u64::from(self.history.recent_nudges())
            .min(steering::lock(&self.steering).responses()) as u32;
        let recent_tool_window = steering::lock(&self.steering)
            .policy()
            .rules
            .empty_response
            .recent_tool_window;
        let after_tools = self.history.has_recent_tool_results(recent_tool_window);
        response.message.reasoning_source = Some(ReasoningSource::new(
            &self.model,
            self.provider.reasoning_transport(&self.model),
        ));
        let empty = !has_tools && steering::visible_text(&response.message).is_none();
        let (observations, all_repairable) = if has_tools {
            self.response_text = None;
            self.process_tool_calls(response, repair_state).await?
        } else {
            self.record_response_text(steering::visible_text(&response.message));
            if empty {
                response
                    .message
                    .content
                    .retain(|block| !matches!(block, ContentBlock::Text { .. }));
                response.message.content.push(ContentBlock::Text {
                    text: EMPTY_RESPONSE_MARKER.into(),
                });
            }
            self.push_assistant_message(response.message);
            (Vec::new(), false)
        };
        self.continuing_response = false;
        if let Some(error) = &interrupted {
            self.push_injected(Message::observation(format!("The provider stream stopped after tool admission ({}). Admitted calls were settled and their actual outcomes are recorded above. Calls not admitted were not executed. Do not replay successful calls; consider possible effects of failed calls before continuing.", error.kind())));
        }
        steering::lock(&self.steering).observe(observations, protocol);
        if self.cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        // User control and context maintenance precede steering. Neither can
        // replenish the invocation allowance, and the turn limit wins before
        // a recovery is charged or another request is promised.
        let queued = self.handle_queued_command().await?;
        if self.cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let turn_limit_reached =
            steering::lock(&self.steering).turn_limit_reached(self.config.max_turns);
        if turn_limit_reached {
            if !has_tools
                && !empty
                && !protocol
                && !queued
                && stop_reason != Some(StopReason::MaxTokens)
            {
                return self.goal_completion(stop_reason.into()).await;
            }
            return Ok(TurnOutcome::Continue);
        }
        let compacted = self.try_auto_compact().await?;
        if self.cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        if queued {
            self.publish_prepared_context();
            return Ok(TurnOutcome::Continue);
        }
        // A captured structured report satisfies missing prose only. This is
        // deliberately not an error handler: cancellation, failed dispatch,
        // protocol mismatches and hard limits keep their own outcomes.
        let usable_report = empty
            && !protocol
            && self
                .report_ready
                .as_ref()
                .is_some_and(|ready| ready.load(Ordering::Acquire));
        let protocol_enabled = {
            let state = steering::lock(&self.steering);
            state.policy().enabled && state.policy().rules.protocol_mismatch.enabled
        };
        let recovery = if interrupted.is_some() {
            Some(Recovery::Truncated)
        } else if all_repairable {
            Some(Recovery::ToolRepair)
        } else if protocol && protocol_enabled {
            Some(Recovery::Protocol)
        } else if usable_report {
            None
        } else if !has_tools && stop_reason == Some(StopReason::MaxTokens) {
            Some(Recovery::Truncated)
        } else if empty {
            Some(Recovery::Empty {
                after_tools,
                nudges,
            })
        } else {
            None
        };
        if let Some(recovery) = recovery {
            let is_empty = matches!(recovery, Recovery::Empty { .. });
            let is_truncated = matches!(recovery, Recovery::Truncated);
            let action = steering::lock(&self.steering).recover(recovery)?;
            if matches!(action, RecoveryAction::Disabled)
                && let Some(error) = interrupted.take()
            {
                return Err(error);
            }
            if let RecoveryAction::Continue(message) = action {
                self.continuing_response = is_truncated && interrupted.is_none();
                if let Some(message) = message {
                    if is_empty {
                        self.event_tx.send(AgentEvent::Nudge)?;
                    }
                    self.push_injected(*message);
                }
                self.publish_prepared_context();
                return Ok(TurnOutcome::Continue);
            }
        }
        if usable_report {
            return Ok(TurnOutcome::Done(DoneReason::EndTurn));
        }
        // Context maintenance alone cannot reopen a disabled truncation recovery.
        // Explicit user input and goal evaluation retain their own continuation policy.
        let outcome = if has_tools || (compacted && stop_reason != Some(StopReason::MaxTokens)) {
            TurnOutcome::Continue
        } else {
            self.goal_completion(stop_reason.into()).await?
        };
        // Hints can decorate an independently scheduled request, never create
        // one by reopening a normal final answer.
        if matches!(outcome, TurnOutcome::Continue) && !compacted {
            self.inject_advisory();
        }
        self.publish_prepared_context();
        Ok(outcome)
    }

    async fn goal_completion(
        &mut self,
        done_reason: DoneReason,
    ) -> Result<TurnOutcome, AgentError> {
        let Some(goal) = self.goal.snapshot() else {
            return Ok(TurnOutcome::Done(done_reason));
        };
        let active_background_tasks = self.subagent_cancels.active_count();
        if active_background_tasks > 0 {
            self.event_tx.send(AgentEvent::GoalDeferred {
                active_background_tasks,
            })?;
            if self.wait_for_background {
                futures_lite::future::race(
                    async {
                        self.subagent_cancels.wait_for_idle().await;
                        Ok(())
                    },
                    async {
                        self.cancel.cancelled().await;
                        Err(AgentError::Cancelled)
                    },
                )
                .await?;
                self.push_arrivals(Vec::new());
                return Ok(TurnOutcome::Continue);
            }
            return Ok(TurnOutcome::Done(done_reason));
        }

        let evaluation = goal.evaluations.saturating_add(1);
        self.event_tx
            .send(AgentEvent::GoalEvaluating { evaluation })?;
        // Only an exactly bound evaluator is worth caching a provider for: any
        // other binding can resolve elsewhere as the conversation model changes.
        let binding = caudra_providers::model_registry::binding(ModelPurpose::Goal);
        let exactly_bound = matches!(binding, Some(Binding::Exact(_)));
        let binding_label = binding
            .as_ref()
            .map_or_else(|| UNBOUND_EVALUATOR.to_string(), Binding::to_string);
        let cached_provider = self
            .goal_evaluator
            .as_ref()
            .filter(|resolved| exactly_bound && resolved.binding == binding)
            .map(|resolved| Arc::clone(&resolved.provider));
        let mut evaluator = match resolve_evaluator(
            &self.provider,
            &self.model,
            binding,
            self.timeouts,
            &self.model_policy,
            &self.cancel,
            cached_provider,
        )
        .await
        {
            Ok(resolved) => {
                self.goal_evaluator = exactly_bound.then_some(resolved.clone());
                resolved
            }
            Err(error) => {
                let cancelled = matches!(error, AgentError::Cancelled);
                self.event_tx.send(AgentEvent::GoalEvaluationFailed {
                    evaluation,
                    message: error.to_string(),
                    applied: self.goal.is_generation_active(goal.generation),
                    usage: TokenUsage::default(),
                    cost: None,
                    billing: Billing::default(),
                    model: binding_label,
                })?;
                if cancelled {
                    return Err(AgentError::Cancelled);
                }
                return Ok(TurnOutcome::Done(done_reason));
            }
        };
        let mut result = Evaluator {
            provider: &*evaluator.provider,
            model: &evaluator.model,
            history: self.history.as_slice(),
            condition: &goal.condition,
            evaluation,
            event_tx: &self.event_tx,
            cancel: &self.cancel,
            session_id: self.session_id.as_ref(),
        }
        .run()
        .await;
        let fallback = match &result {
            Err(failure) if self.goal.is_generation_active(goal.generation) => {
                evaluator.fallback_to_current(&failure.error, &self.provider, &self.model)
            }
            _ => None,
        };
        result = match (fallback, result) {
            (Some(fallback), Err(failure)) => {
                let fallback_model = fallback.model.spec();
                warn!(
                    model = %failure.model,
                    fallback_model,
                    status = failure.error.status(),
                    error_kind = failure.error.kind(),
                    "goal evaluator model unavailable, falling back to chat model"
                );
                if failure.usage != TokenUsage::default() || failure.cost.is_some() {
                    self.record_goal_evaluation_failure(
                        goal.generation,
                        evaluation,
                        failure,
                        false,
                    )?;
                }
                evaluator = fallback;
                Evaluator {
                    provider: &*evaluator.provider,
                    model: &evaluator.model,
                    history: self.history.as_slice(),
                    condition: &goal.condition,
                    evaluation,
                    event_tx: &self.event_tx,
                    cancel: &self.cancel,
                    session_id: self.session_id.as_ref(),
                }
                .run()
                .await
            }
            (_, result) => result,
        };
        let result = match result {
            Ok(result) => result,
            Err(failure) => {
                let cancelled = matches!(failure.error, AgentError::Cancelled);
                let applied = self.goal.is_generation_active(goal.generation);
                self.record_goal_evaluation_failure(goal.generation, evaluation, failure, applied)?;
                if cancelled {
                    return Err(AgentError::Cancelled);
                }
                return Ok(TurnOutcome::Done(done_reason));
            }
        };

        self.total_usage += result.usage;
        self.goal
            .record_usage_for(goal.generation, result.usage, result.cost, result.billing);
        let reason: Arc<str> = Arc::from(result.reason.as_str());
        let apply =
            self.goal
                .apply_evaluation(goal.generation, result.verdict, Arc::clone(&reason));
        self.event_tx.send(AgentEvent::GoalEvaluation {
            verdict: result.verdict,
            reason: result.reason.clone(),
            evaluation,
            applied: !matches!(apply, GoalApply::Stale),
            usage: result.usage,
            cost: result.cost,
            billing: result.billing,
            model: result.model,
        })?;

        match apply {
            GoalApply::Stale => Ok(TurnOutcome::Done(done_reason)),
            GoalApply::Terminal => {
                if let Some(GoalStatus::Finished(result)) = self.goal.status() {
                    self.event_tx.send(AgentEvent::GoalFinished { result })?;
                }
                Ok(TurnOutcome::Done(done_reason))
            }
            GoalApply::Continue { evaluation } => {
                let continuation_limit = self.goal.continuation_limit();
                if self.goal_blocks >= continuation_limit {
                    self.event_tx.send(AgentEvent::GoalLoopCap {
                        evaluations: evaluation,
                        continuations: self.goal_blocks,
                        limit: continuation_limit,
                    })?;
                    return Ok(TurnOutcome::Done(done_reason));
                }
                self.goal_blocks += 1;
                self.push_injected(Message::synthetic(continuation_message(
                    &goal.condition,
                    &reason,
                )));
                Ok(TurnOutcome::Continue)
            }
        }
    }

    fn record_goal_evaluation_failure(
        &mut self,
        generation: u64,
        evaluation: u32,
        failure: super::goal::EvaluationError,
        applied: bool,
    ) -> Result<(), AgentError> {
        self.total_usage += failure.usage;
        self.goal
            .record_usage_for(generation, failure.usage, failure.cost, failure.billing);
        self.event_tx.send(AgentEvent::GoalEvaluationFailed {
            evaluation,
            message: failure.error.to_string(),
            applied,
            usage: failure.usage,
            cost: failure.cost,
            billing: failure.billing,
            model: failure.model,
        })
    }

    async fn wait_for_reauth(
        &mut self,
        err: AgentError,
        reset_stream: bool,
    ) -> Result<TurnOutcome, AgentError> {
        if reset_stream {
            self.event_tx.send(AgentEvent::StreamReset)?;
        }
        if self.reauth_attempts >= MAX_REAUTH_ATTEMPTS {
            error!(error = %err, attempts = self.reauth_attempts, "max re-auth attempts reached");
            return Err(err);
        }
        let Some(rx) = self.user_response_rx.as_ref().map(Arc::clone) else {
            error!(error = %err, model = %self.model.id, turns = self.num_turns, "stream_message failed");
            return Err(err);
        };
        self.reauth_attempts += 1;
        warn!(error = %err, attempt = self.reauth_attempts, "auth error, waiting for re-authentication");
        self.event_tx.send(AgentEvent::AuthRequired)?;
        let rx = rx.lock().await;
        enum Wake {
            Response(Result<String, flume::RecvError>),
            Poll,
            Cancelled,
        }
        loop {
            let poll_delay = Duration::from_millis(fastrand::u64(
                AUTH_RELOAD_POLL_MIN_MS..=AUTH_RELOAD_POLL_MAX_MS,
            ));
            let wake = futures_lite::future::race(
                async { Wake::Response(rx.recv_async().await) },
                futures_lite::future::race(
                    async {
                        self.cancel.cancelled().await;
                        Wake::Cancelled
                    },
                    async {
                        smol::Timer::after(poll_delay).await;
                        Wake::Poll
                    },
                ),
            )
            .await;
            match wake {
                Wake::Response(Ok(_)) => {
                    self.provider.refresh_auth().await?;
                    self.readjust_model();
                    self.event_tx.send(AgentEvent::AuthRestored)?;
                    return Ok(TurnOutcome::Continue);
                }
                Wake::Response(Err(_)) | Wake::Cancelled => return Err(AgentError::Cancelled),
                Wake::Poll => match self.provider.reload_auth_if_changed().await {
                    Ok(true) => {
                        self.readjust_model();
                        self.event_tx.send(AgentEvent::AuthRestored)?;
                        return Ok(TurnOutcome::Continue);
                    }
                    Ok(false) => {}
                    Err(error) => {
                        debug!(%error, "failed to poll for replacement credentials");
                    }
                },
            }
        }
    }

    fn emit_turn_complete(&self, response: &StreamResponse) -> Result<(), AgentError> {
        self.event_tx
            .send(AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
                message: response.message.clone(),
                usage: response.usage,
                model: self.model.id.clone(),
                provider: self.model.provider.to_string(),
                purpose: LedgerPurpose::Chat,
                cost: self
                    .model
                    .billed_cost(&response.usage, self.opts.clamped(&self.model).fast),
                billing: self.model.billing,
                // The reply reaches history after this fires but is replayed by
                // the next request all the same, so count it here too. Leaving
                // it out is what the session would persist, and the bar would
                // drop by a turn's worth of tokens on reload.
                context_size: self.context_size().map(|size| {
                    size.saturating_add(estimate_message_tokens(slice::from_ref(&response.message)))
                }),
                context_window: self.model.context_window,
            })))
    }

    fn emit_done(&self, reason: DoneReason) -> Result<(), AgentError> {
        info!(
            self.num_turns,
            total_input = self.total_usage.input,
            total_output = self.total_usage.output,
            %reason,
            "agent run completed"
        );
        self.event_tx.send(AgentEvent::Done {
            usage: self.total_usage,
            num_turns: self.num_turns,
            reason,
        })
    }

    fn inject_advisory(&mut self) {
        if self
            .history
            .as_slice()
            .iter()
            .rev()
            .take_while(|message| !matches!(message.role, Role::Assistant))
            .any(|message| message.steering.is_some())
        {
            return;
        }
        let has_tools = self
            .request_tools()
            .0
            .as_array()
            .is_some_and(|tools| !tools.is_empty());
        let message = steering::lock(&self.steering).advisory(
            self.history.as_slice(),
            &self.model,
            has_tools,
        );
        if let Some(message) = message {
            self.push_injected(message);
        }
    }

    async fn process_tool_calls(
        &mut self,
        response: StreamResponse,
        repair_state: Arc<RepairState>,
    ) -> Result<(Vec<ToolObservation>, bool), AgentError> {
        let tool_uses = response
            .message
            .tool_uses()
            .map(|(id, name, input)| (id.to_owned(), name.to_owned(), input.clone()))
            .collect();
        let observations = self
            .speculative
            .as_ref()
            .and_then(|runs| runs.observations())
            .unwrap_or_else(|| {
                ResponseObservations::new(steering::lock(&self.steering).observation_window())
            });
        self.tool_name_aliases = response.tool_name_aliases.clone();
        let ctx = ToolContext {
            steering_observations: Some(observations.clone()),
            json_repair: repair_state,
            tool_name_aliases: response.tool_name_aliases.clone(),
            speculative: self.speculative.clone(),
            ..self.tool_context()
        };
        for (id, invalid) in response.invalid_tool_inputs {
            ctx.json_repair.register_invalid(&id, invalid);
        }
        self.push_assistant_message(response.message);
        let result = tool_dispatch::process_tool_calls(
            tool_uses,
            &mut self.recent_calls,
            self.mcp.as_ref(),
            self.history,
            &self.event_tx,
            &ctx,
        )
        .await;
        self.drain_repair_usage(&ctx.json_repair);
        if result.is_ok() {
            self.publish_prepared_context();
        }
        result?;
        Ok(observations.take())
    }

    fn drain_repair_usage(&mut self, state: &RepairState) {
        for repair in state.take_usage() {
            self.total_usage += repair.usage;
            self.goal
                .record_usage(repair.usage, repair.cost, repair.billing);
            self.event_tx.try_send(AgentEvent::ModelUsage {
                usage: repair.usage,
                cost: repair.cost,
                billing: repair.billing,
                provider: repair.provider,
                model: repair.model,
                purpose: repair.purpose,
            });
        }
    }

    /// Hands the model whatever the last response started early and never
    /// asked for, so it does not do that work a second time.
    fn report_speculative(&mut self) {
        let Some(runs) = self.speculative.take() else {
            return;
        };
        if let Some(message) = runs.drain_report() {
            self.history.push(message);
        }
        self.drain_repair_usage(&runs.repair_state());
    }

    fn tool_context(&self) -> ToolContext {
        ToolContext {
            provider: Arc::clone(&self.provider),
            model: Arc::clone(&self.model),
            chat_provider: Arc::clone(&self.chat_provider),
            chat_model: Arc::clone(&self.chat_model),
            event_tx: self.event_tx.clone(),
            mode: self.mode.clone(),
            session_id: self.session_id.clone(),
            workspace_session: self.workspace_session.clone(),
            remote_project_context: self.remote_project_context.clone(),
            local_documents: self.local_documents.clone(),
            task_environment: self.task_environment.clone(),
            context_publisher: self.context_publisher.clone(),
            tool_output_store: crate::tool_output::default_store(),
            tool_use_id: None,
            root_tool_use_id: self.root_tool_use_id.clone(),
            user_response_rx: self.user_response_rx.clone(),
            loaded_instructions: self.loaded_instructions.clone(),
            cancel: self.cancel.clone(),
            mcp: self.mcp.clone(),
            deferral: Some(self.deferral.clone()),
            deadline: Deadline::None,
            config: self.config.clone(),
            tool_output_lines: self.tool_output_lines,
            permissions: Arc::clone(&self.permissions),
            timeouts: self.timeouts,
            file_tracker: Arc::clone(&self.file_tracker),
            path_locks: Arc::clone(&self.path_locks),
            baseline: self.baseline.clone(),
            prompt_slots: Arc::clone(&self.prompt_slots),
            prompt_profiles: Arc::clone(&self.prompt_profiles),
            default_task_prompt_profile_name: Arc::clone(&self.default_task_prompt_profile_name),
            opts: self.opts.clone(),
            subagent_cancels: Arc::clone(&self.subagent_cancels),
            subagent_history: self.subagent_history.clone(),
            registry: Arc::clone(&self.registry),
            audience: self.audience,
            tool_filter: self.tool_filter.clone(),
            local_tools: Arc::clone(&self.local_tools),
            tool_name_aliases: self.tool_name_aliases.clone(),
            steering_observations: None,
            steering_order: Vec::new(),
            json_repair: Arc::new(RepairState::default()),
            live_sink: None,
            model_policy: Arc::clone(&self.model_policy),
            workflow: self.workflow.clone(),
            speculative: None,
        }
    }

    /// How full the window is: the provider's own count for the last request it
    /// billed, extended by everything appended since. `None` before the first
    /// response, when nothing has been billed to anchor on.
    fn context_size(&self) -> Option<u32> {
        self.measured
            .and_then(|measured| measured.extended_by(self.history.as_slice()))
    }

    async fn try_auto_compact(&mut self) -> Result<bool, AgentError> {
        let Some(context_size) = self.context_size().filter(|_| self.auto_compact) else {
            return Ok(false);
        };
        if !compaction::is_overflow(
            &TokenUsage {
                input: context_size,
                ..Default::default()
            },
            &self.model,
            self.config.compaction_buffer,
        ) {
            return Ok(false);
        }
        info!(context_size, "auto-compacting");
        self.event_tx.send(AgentEvent::Compacting)?;
        self.do_compact().await?;
        Ok(true)
    }

    async fn do_compact(&mut self) -> Result<(), AgentError> {
        let (compact_provider, compact_model) = resolve_compaction_model(
            &self.provider,
            &self.model,
            self.timeouts,
            &self.model_policy,
        )?;
        let compacted = compaction::compact_history(
            &*compact_provider,
            &compact_model,
            self.history,
            &self.event_tx,
            &self.cancel,
            &self.retry_now,
            &self.config,
        )
        .await?;
        let usage = compacted.usage;
        let cost = compact_model.billed_cost(&usage, false);
        self.total_usage += usage;
        self.goal.record_usage(usage, cost, compact_model.billing);
        compacted.result?;
        self.rollback_len = self.history.len();
        steering::lock(&self.steering).reset_patterns();
        self.recent_calls =
            RecentCalls::with_threshold(steering::lock(&self.steering).repeat_threshold());
        self.event_tx.send(AgentEvent::CompactionDone)?;
        self.push_injected(Message::synthetic(compaction::continue_message(
            &self.config,
        )));
        self.publish_prepared_context();
        Ok(())
    }

    async fn handle_queued_command(&mut self) -> Result<bool, AgentError> {
        let Some(ref source) = self.interrupt_source else {
            return Ok(false);
        };
        let Some(cmd) = source.poll() else {
            return Ok(false);
        };
        match cmd {
            ExtractedCommand::Interrupt(input, run_id, id) => {
                self.event_tx.send(AgentEvent::QueueItemConsumed {
                    id,
                    text: input.message.clone(),
                    image_count: input.images.len(),
                })?;
                self.push_user_inputs(vec![input], true).await;
                let _ = run_id;
            }
            ExtractedCommand::InterruptBatch(inputs) => {
                self.event_tx.send(AgentEvent::QueueBatchConsumed {
                    items: inputs
                        .iter()
                        .map(|queued| QueueConsumedItem {
                            id: queued.id,
                            text: queued.input.message.clone(),
                            image_count: queued.input.images.len(),
                        })
                        .collect(),
                })?;
                self.push_user_inputs(
                    inputs.into_iter().map(|queued| queued.input).collect(),
                    true,
                )
                .await;
            }
            ExtractedCommand::Compact(_) => {
                self.do_compact().await?;
            }
        }
        Ok(true)
    }
}

fn queued_message(display: &str) -> String {
    format!(
        "<user-interrupt>\nThe user sent a new message while you were working. Address it and continue.\n\n{display}\n</user-interrupt>"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnnouncedMode {
    Build,
    Plan,
}

impl AnnouncedMode {
    /// `ReadOnly` is a subagent contract rather than a mode the user toggles,
    /// and its system prompt restates it every turn, so it announces nothing.
    fn of(mode: &AgentMode) -> Option<Self> {
        match mode {
            AgentMode::Plan(_) | AgentMode::RemotePlan(_) => Some(Self::Plan),
            AgentMode::Build => Some(Self::Build),
            AgentMode::ReadOnly => None,
        }
    }
}

/// The text of the most recent standing reminder carrying `marker`.
fn last_announced<'a>(history: &'a [Message], marker: &str) -> Option<&'a str> {
    history
        .iter()
        .rev()
        .filter(|message| message.is_observation())
        .find_map(|message| message.user_text().filter(|text| text.contains(marker)))
}

/// Restates `text` when the transcript does not already carry this exact block.
/// Idempotent by construction, which is what covers a first turn, a change in
/// the underlying value, and a compaction that dropped the last announcement.
fn standing_notice(history: &[Message], marker: &str, text: Option<&str>) -> Option<Message> {
    let text = text?;
    if last_announced(history, marker) == Some(text) {
        return None;
    }
    Some(Message::observation(text.to_owned()))
}

/// The mode the transcript last told the model it was in.
///
/// Derived from history rather than from a field because [`Agent`] is rebuilt
/// for every run while history outlives it, so an in-memory previous mode is
/// always the default and never a transition. Reading the transcript also
/// survives a restart, which is the case that matters most: a session resumed
/// days later still knows it was planning.
fn last_announced_mode(history: &[Message]) -> AnnouncedMode {
    history
        .iter()
        .rev()
        .filter(|message| message.is_observation())
        .find_map(|message| {
            let text = message.user_text()?;
            if text.contains(crate::prompt::BUILD_MODE_MARKER) {
                Some(AnnouncedMode::Build)
            } else if text.contains(crate::prompt::PLAN_MODE_MARKER) {
                Some(AnnouncedMode::Plan)
            } else {
                None
            }
        })
        .unwrap_or(AnnouncedMode::Build)
}

/// Announces the active mode to the model, or `None` when the transcript
/// already says what the incoming input says.
///
/// Carries the mode's full instructions rather than a bare notice, because this
/// is the only place they appear: the system prompt is deliberately identical in
/// both modes so that toggling does not re-cache the conversation. An
/// announcement trails the turn it governs, so the cut that preserves that turn
/// preserves it too; losing one anyway is self-healing, since the next turn then
/// finds no match and announces again.
fn mode_switch_notice(history: &[Message], next: &AgentMode) -> Option<Message> {
    let announced = AnnouncedMode::of(next)?;
    if announced == last_announced_mode(history) {
        return None;
    }
    let text = match next {
        AgentMode::Plan(plan_path) => Vars::new()
            .set("{plan_path}", plan_path.display().to_string())
            .set("{plan_write_tools}", LOCAL_PLAN_WRITE_TOOLS)
            .apply(crate::prompt::PLAN_PROMPT)
            .into_owned(),
        AgentMode::RemotePlan(reference) => Vars::new()
            .set(
                "{plan_path}",
                format!("opaque plan reference {}", reference.as_str()),
            )
            .set("{plan_write_tools}", REMOTE_PLAN_WRITE_TOOLS)
            .apply(crate::prompt::PLAN_PROMPT)
            .into_owned(),
        AgentMode::Build | AgentMode::ReadOnly => crate::prompt::BUILD_PROMPT.to_owned(),
    };
    Some(Message::observation(text))
}

/// Counts provider-visible message content and replay framing. The system
/// prompt and tool schemas stay invisible here, so never let this replace a
/// context size the provider measured.
///
/// Counts rather than estimates from byte length. The ratio a byte heuristic
/// assumes holds for prose and breaks on everything a tool returns: dense JSON
/// costs about twice what its length suggests, so a heuristic hid the growth on
/// exactly the turns that overflow.
pub fn estimate_message_tokens(messages: &[Message]) -> u32 {
    messages.iter().fold(0, |total, message| {
        let framing = match message.role {
            Role::User => USER_MESSAGE_FRAMING,
            Role::Assistant => ASSISTANT_MESSAGE_FRAMING,
        };
        message.content.iter().fold(
            total.saturating_add(estimate_tokens_cached(framing)),
            |total, block| total.saturating_add(message_block_tokens(block)),
        )
    })
}

fn message_block_tokens(block: &ContentBlock) -> u32 {
    match block {
        ContentBlock::Text { text } => framed_tokens(TEXT_BLOCK_FRAMING, [text.as_str()]),
        ContentBlock::Thinking {
            thinking,
            signature,
            responses,
            ..
        } => {
            let mut tokens = framed_tokens(THINKING_BLOCK_FRAMING, [thinking.as_str()]);
            if let Some(signature) = signature {
                add_estimated_tokens(&mut tokens, THINKING_SIGNATURE_FRAMING);
                add_opaque_blob_tokens(&mut tokens, signature);
            }
            if let Some(responses) = responses {
                add_estimated_tokens(&mut tokens, RESPONSES_REASONING_FRAMING);
                add_estimated_tokens(&mut tokens, &responses.item_id);
                if let Some(encrypted_content) = &responses.encrypted_content {
                    add_estimated_tokens(&mut tokens, RESPONSES_ENCRYPTED_CONTENT_FRAMING);
                    add_opaque_blob_tokens(&mut tokens, encrypted_content);
                }
            }
            tokens
        }
        ContentBlock::RedactedThinking { data } => {
            let mut tokens = estimate_tokens_cached(REDACTED_THINKING_BLOCK_FRAMING);
            add_opaque_blob_tokens(&mut tokens, data);
            tokens
        }
        ContentBlock::ToolUse {
            id,
            name,
            input,
            thought_signature,
        } => {
            let mut tokens = framed_tokens(TOOL_USE_BLOCK_FRAMING, [id.as_str(), name.as_str()]);
            add_estimated_tokens(&mut tokens, &input.to_string());
            if let Some(thought_signature) = thought_signature {
                add_estimated_tokens(&mut tokens, TOOL_USE_SIGNATURE_FRAMING);
                add_opaque_blob_tokens(&mut tokens, thought_signature);
            }
            tokens
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => {
            let mut tokens = framed_tokens(
                TOOL_RESULT_BLOCK_FRAMING,
                [tool_use_id.as_str(), content.as_str()],
            );
            if *is_error {
                add_estimated_tokens(&mut tokens, TOOL_RESULT_ERROR_FRAMING);
            }
            tokens
        }
        ContentBlock::Image { source } => {
            let mut tokens = framed_tokens(IMAGE_BLOCK_FRAMING, [source.media_type.mime()]);
            tokens = tokens.saturating_add(crate::tools::image_bytes::token_estimate(source));
            tokens
        }
    }
}

/// Empty values price fixed wire structure separately from payloads. Keeping
/// the parts additive avoids serializing large tool results and lets callers
/// move loaded bodies between exclusive accounting categories.
fn framed_tokens<const N: usize>(framing: &str, fields: [&str; N]) -> u32 {
    fields
        .into_iter()
        .fold(estimate_tokens_cached(framing), |total, field| {
            total.saturating_add(estimate_tokens_cached(field))
        })
}

fn add_estimated_tokens(total: &mut u32, text: &str) {
    *total = total.saturating_add(estimate_tokens_cached(text));
}

/// Charges base64 armour for what it encodes rather than for how it is spelled.
/// See [`OPAQUE_BLOB_CHARS_PER_TOKEN`].
fn add_opaque_blob_tokens(total: &mut u32, blob: &str) {
    let tokens = blob.len() / OPAQUE_BLOB_CHARS_PER_TOKEN;
    *total = total.saturating_add(u32::try_from(tokens).unwrap_or(u32::MAX));
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use caudra_config::steering::SteeringConfig;
    use caudra_providers::provider::{BoxFuture, Provider};
    use caudra_providers::{
        ContentBlock, InvalidToolInput, Message, Model, ProviderEvent, RequestOptions, Role,
        SteeringKind, StopReason, StreamResponse, TokenUsage, invalid_tool_input,
    };
    use caudra_workspace::PlanRef;
    use serde_json::Value;
    use test_case::test_case;

    use super::*;
    use crate::cancel::CancelTrigger;
    use crate::context::{ContextKey, ContextStore};
    use crate::mcp::tool_names;
    use crate::permissions::PermissionManager;
    use crate::{Envelope, QueueItemId};

    const AUTH_ERROR_STATUS: u16 = 401;
    const MAX_NUDGES: u32 = 20;
    const MAX_IDLE_NUDGES: u32 = 2;
    const STEERING_EMPTY: &str = "empty_response";
    const STEERING_PROTOCOL: &str = "protocol_mismatch";
    const STEERING_TOOL_REPAIR: &str = "tool_repair";
    const STEERING_TRUNCATION: &str = "truncation";
    const TRUNCATION_ATTEMPTS: u32 = 3;
    const RECOVERY_BUDGET: u32 = 32;
    const TOOL_ROUNDS: u32 = 5;
    const OUTPUT_TOKENS: u32 = 7;
    const TEST_TOOL: &str = "test_tool";
    const TEST_TOOL_RESULT: &str = "completed";
    const VISIBLE_RESPONSE: &str = "response";
    const STEERING_CUSTOM: &str = "Custom runtime guidance.";
    const INVALID_TOOL: &str = "invalid_tool";
    const REPAIR_RAW: &str = "{\"a\" 1}";
    const REPAIR_ACCEPTED: &str = "{\"a\":1}";
    const REPAIR_REJECTED: &str = "{\"a\":2}";
    const REPAIR_USAGE: TokenUsage = TokenUsage {
        input: 19,
        output: 7,
        cache_creation: 3,
        cache_read: 5,
    };
    const MAIN_REQUEST_USAGE: TokenUsage = TokenUsage {
        input: 73,
        output: 11,
        cache_creation: 0,
        cache_read: 0,
    };
    const LARGE_CONTEXT: u32 = 170_000;
    const AUTH_ERROR_MESSAGE: &str = "expired";
    const EXPECTED_AUTH_ERROR: &str = "expected terminal authentication error";
    /// Distinctive so `adjust_model` is provably what set it, but wide enough
    /// that a two-message transcript does not trip auto-compaction on its way
    /// through the run these tests are actually about.
    const ADJUSTED_CONTEXT_WINDOW: u32 = 111_111;
    const PUBLISHED_CONTEXT_WINDOW: u32 = 123_456;
    const CAPTURED_CONTEXT_MISSING: &str = "current request must publish before provider dispatch";
    const PREPARED_CONTEXT_MISSING: &str = "completed turn must publish its next request";
    const BLOCKING_TOOL_NAME: &str = "blocking_context_tool";
    const BLOCKING_TOOL_RESULT: &str = "The blocking tool completed and returned this deliberately long result so the actual tool-result snapshot is distinguishable from the temporary unavailable-result projection used while execution is pending.";
    const BLOCKING_TOOL_CONTEXT_MISSING: &str =
        "assistant tool-use history must publish before tool execution completes";
    const COMPLETED_TOOL_CONTEXT_MISSING: &str =
        "actual tool-result history must publish after insertion";
    const GOAL_CONTEXT_MISSING: &str =
        "assistant history must publish before goal evaluation completes";
    const PARTIAL_RESPONSE: &str = "partial";
    const GO: &str = "go";
    const RESUME_TOOL_ID: &str = "t1";
    const RESUME_DROPPED_ONLY_MARKER: &str = "a resume drops the cancel marker and nothing else";
    const TITLE_PROMPT: &str = "add refresh token support";
    const MODEL_TITLE: &str = "Refresh token support";
    const TITLE_MUST_SURVIVE: &str = "a title is asked for at the start of a turn and answers after it, so the turn ending must not cancel it";
    /// Generous: the mock answers in microseconds, so this only bounds a
    /// regression that would otherwise hang instead of failing.
    const TITLE_EVENT_TIMEOUT: Duration = Duration::from_secs(5);
    const TEST_PLAN_PATH: &str = ".caudra/plans/123.md";
    const EXPECTED_PLAN_NOTICE: &str = "entering plan mode must be announced";
    const EXPECTED_BUILD_NOTICE: &str = "leaving plan mode must be announced";
    const ENVIRONMENT: &str =
        "<system-reminder>\n# Environment\n\n- Date: 2026-09-09\n</system-reminder>";
    const ENVIRONMENT_NEXT_DAY: &str =
        "<system-reminder>\n# Environment\n\n- Date: 2026-09-10\n</system-reminder>";
    const EXPECTED_ENVIRONMENT_NOTICE: &str = "a changed environment must be announced";
    const EXPECTED_USER_TURN: &str = "the fixture opens on the message the user typed";
    const EXPECTED_REQUEST: &str = "a run must send at least one message";
    const NO_PREFILL: &str = "an arrival must never leave the request ending on the assistant";
    const SHELL_RESULT: &str = "I ran: $ ls\n\nOutput:\na.rs";
    const PROMPT_SEED: &str = "Here is how I review code.";
    const INSTRUCTIONS_CHANGED: &str =
        "<system-reminder>\n# Instructions changed\n\n+ be brief\n</system-reminder>";
    const MENTION_BODY: &str = "<file path=\"a.rs\">fn main() {}</file>";
    const OVERRIDE_MODEL_SPEC: &str = "openai/gpt-5.4";
    const PLAN_MODEL_SPEC: &str = "openai/gpt-5.4";
    const PROFILE_MODEL_SPEC: &str = "anthropic/claude-opus-4-6";
    const MODEL_UNAVAILABLE: &str = r#"{"error":{"code":"model_not_found","message":"model 'claude-haiku-4-5' not found","param":"model","type":"invalid_request_error"}}"#;

    struct MockInterruptSource {
        commands: Mutex<VecDeque<ExtractedCommand>>,
    }

    impl MockInterruptSource {
        fn new(commands: Vec<ExtractedCommand>) -> Arc<Self> {
            Arc::new(Self {
                commands: Mutex::new(commands.into()),
            })
        }
    }

    impl InterruptSource for MockInterruptSource {
        fn poll(&self) -> Option<ExtractedCommand> {
            self.commands.lock().unwrap().pop_front()
        }
    }

    struct MockProvider {
        responses: Mutex<Vec<Result<StreamResponse, AgentError>>>,
        captured_tools: Arc<Mutex<Vec<Value>>>,
        captured_models: Arc<Mutex<Vec<String>>>,
    }

    impl MockProvider {
        fn new(responses: Vec<StreamResponse>) -> Self {
            Self::with_results(responses.into_iter().map(Ok).collect())
        }

        fn with_results(responses: Vec<Result<StreamResponse, AgentError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                captured_tools: Arc::default(),
                captured_models: Arc::default(),
            }
        }
    }

    impl Provider for MockProvider {
        fn stream_message<'a>(
            &'a self,
            model: &'a Model,
            _: &'a [Message],
            _: &'a str,
            tools: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                self.captured_tools.lock().unwrap().push(tools.clone());
                self.captured_models.lock().unwrap().push(model.spec());
                let mut responses = self.responses.lock().unwrap();
                assert!(!responses.is_empty(), "MockProvider: no more responses");
                responses.remove(0)
            })
        }

        fn list_models(
            &self,
        ) -> BoxFuture<'_, Result<Vec<caudra_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    /// Keeps the last request's messages, which is the only way to tell what a
    /// resume actually put on the wire rather than what it left in history.
    struct RequestCapturingProvider {
        captured: Arc<Mutex<Vec<Message>>>,
    }

    impl Provider for RequestCapturingProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            messages: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                let mut captured = self.captured.lock().unwrap();
                captured.clear();
                captured.extend_from_slice(messages);
                Ok(text_response(StopReason::EndTurn))
            })
        }

        fn list_models(
            &self,
        ) -> BoxFuture<'_, Result<Vec<caudra_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    struct ContextObservingProvider {
        store: ContextStore,
        captured: Arc<Mutex<Option<Arc<ContextSnapshot>>>>,
    }

    impl Provider for ContextObservingProvider {
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
            Box::pin(async {
                *self.captured.lock().unwrap() = self.store.latest(&ContextKey::Main);
                Ok(text_response(StopReason::EndTurn))
            })
        }

        fn list_models(
            &self,
        ) -> BoxFuture<'_, Result<Vec<caudra_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }

        fn adjust_model(&self, model: &mut Model) {
            model.context_window = PUBLISHED_CONTEXT_WINDOW;
        }
    }

    struct ControlledEvaluatorProvider {
        calls: AtomicUsize,
        evaluator_started: flume::Sender<()>,
        evaluator_response: flume::Receiver<StreamResponse>,
    }

    #[derive(Default)]
    struct ReauthState {
        stream_calls: AtomicUsize,
        reloads: AtomicUsize,
        refreshes: AtomicUsize,
        adjustments: AtomicUsize,
    }

    struct ReauthProvider {
        state: Arc<ReauthState>,
    }

    impl Provider for ReauthProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            event_tx: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                if self.state.stream_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    event_tx
                        .send(ProviderEvent::TextDelta {
                            text: PARTIAL_RESPONSE.into(),
                        })
                        .unwrap();
                    return Err(auth_error());
                }
                Ok(text_response(StopReason::EndTurn))
            })
        }

        fn list_models(
            &self,
        ) -> BoxFuture<'_, Result<Vec<caudra_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }

        fn refresh_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
            Box::pin(async {
                self.state.refreshes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }

        fn reload_auth_if_changed(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
            Box::pin(async {
                self.state.reloads.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            })
        }

        fn adjust_model(&self, model: &mut Model) {
            self.state.adjustments.fetch_add(1, Ordering::SeqCst);
            model.context_window = ADJUSTED_CONTEXT_WINDOW;
        }
    }

    impl Provider for ControlledEvaluatorProvider {
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
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Ok(text_response(StopReason::EndTurn));
                }
                self.evaluator_started.send(()).unwrap();
                self.evaluator_response
                    .recv_async()
                    .await
                    .map_err(|_| AgentError::Cancelled)
            })
        }

        fn list_models(
            &self,
        ) -> BoxFuture<'_, Result<Vec<caudra_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    /// Streams `delta` (if any), fires `cancel_after_delta` (if any),
    /// then fails with `fail_status` or hangs until cancelled.
    #[derive(Default)]
    struct StubStreamProvider {
        delta: Option<&'static str>,
        delta_is_thinking: bool,
        cancel_after_delta: Mutex<Option<crate::cancel::CancelTrigger>>,
        fail_status: Option<u16>,
        fail_body: Option<&'static str>,
        fail_retry_after: Option<Duration>,
    }

    impl Provider for StubStreamProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            ptx: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                if let Some(text) = self.delta {
                    let event = if self.delta_is_thinking {
                        ProviderEvent::ThinkingDelta { text: text.into() }
                    } else {
                        ProviderEvent::TextDelta { text: text.into() }
                    };
                    ptx.send(event).unwrap();
                }
                if let Some(trigger) = self.cancel_after_delta.lock().unwrap().take() {
                    trigger.cancel();
                }
                match self.fail_status {
                    Some(status) => Err(AgentError::Api {
                        status,
                        message: self.fail_body.unwrap_or("stub").into(),
                        retry_after: self.fail_retry_after,
                    }),
                    None => futures_lite::future::pending().await,
                }
            })
        }

        fn list_models(
            &self,
        ) -> BoxFuture<'_, Result<Vec<caudra_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    fn default_model() -> Model {
        Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
    }

    #[derive(Clone, Copy)]
    enum TestBinding {
        None,
        Exact(&'static str),
        Chat,
    }

    impl TestBinding {
        fn binding(self) -> Option<Binding> {
            match self {
                Self::None => None,
                Self::Exact(spec) => Some(Binding::Exact(spec.into())),
                Self::Chat => Some(Binding::Same(ModelPurpose::Chat)),
            }
        }
    }

    #[test_case(TestBinding::None, TestBinding::None, PLAN_MODEL_SPEC ; "unbound_inherits_effective_plan")]
    #[test_case(TestBinding::Exact(PROFILE_MODEL_SPEC), TestBinding::None, PROFILE_MODEL_SPEC ; "global_exact_override")]
    #[test_case(TestBinding::Chat, TestBinding::None, "anthropic/claude-sonnet-4-20250514" ; "global_same_chat_override")]
    #[test_case(TestBinding::Exact(PLAN_MODEL_SPEC), TestBinding::Exact(PROFILE_MODEL_SPEC), PROFILE_MODEL_SPEC ; "profile_exact_override")]
    #[test_case(TestBinding::Exact(PLAN_MODEL_SPEC), TestBinding::Chat, "anthropic/claude-sonnet-4-20250514" ; "profile_same_chat_override")]
    fn subagent_binding_uses_effective_default_and_selected_chat_anchor(
        global: TestBinding,
        profile: TestBinding,
        expected: &str,
    ) {
        let chat = default_model();
        let plan = Model::from_spec(PLAN_MODEL_SPEC).unwrap();
        let global = global.binding();
        let profile = profile.binding();

        let resolved = resolve_captured_purpose_model(
            ModelPurpose::Subagent,
            profile.as_ref(),
            global.as_ref(),
            &plan,
            &chat,
            &ModelPolicy::default(),
        )
        .unwrap();

        assert_eq!(resolved.spec(), expected);
    }

    #[test_case(ModelPurpose::Plan ; "plan")]
    #[test_case(ModelPurpose::Subagent ; "subagent")]
    fn explicit_purpose_override_does_not_consult_the_global_binding(purpose: ModelPurpose) {
        let model = resolve_purpose_model(
            purpose,
            Some(&Binding::Exact(OVERRIDE_MODEL_SPEC.into())),
            &default_model(),
            &default_model(),
            &ModelPolicy::default(),
        )
        .unwrap();

        assert_eq!(model.spec(), OVERRIDE_MODEL_SPEC);
    }

    #[test_case(ModelPurpose::Plan ; "plan")]
    #[test_case(ModelPurpose::Subagent ; "subagent")]
    fn unchanged_purpose_model_reuses_the_running_provider(purpose: ModelPurpose) {
        smol::block_on(async {
            let current_provider: Arc<dyn Provider> = Arc::new(MockProvider::new(Vec::new()));
            let current_model = default_model();
            let binding = Binding::Exact(current_model.spec());

            let (resolved_provider, resolved_model) = resolve_model_for_purpose(
                ModelRoute {
                    provider: &current_provider,
                    model: &current_model,
                },
                ModelRoute {
                    provider: &current_provider,
                    model: &current_model,
                },
                purpose,
                Some(&binding),
                Timeouts::default(),
                &ModelPolicy::default(),
            )
            .await
            .unwrap();

            assert!(Arc::ptr_eq(&resolved_provider, &current_provider));
            assert_eq!(resolved_model.spec(), current_model.spec());
        });
    }

    fn text_response(stop_reason: StopReason) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "response".into(),
                }],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(stop_reason),
            ..Default::default()
        }
    }

    fn empty_response() -> StreamResponse {
        assistant_response(vec![])
    }

    fn thinking_response() -> StreamResponse {
        assistant_response(vec![ContentBlock::thinking("stalled".into(), None)])
    }

    fn assistant_response(content: Vec<ContentBlock>) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content,
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::EndTurn),
            ..Default::default()
        }
    }

    fn goal_response(ok: bool, impossible: bool, reason: &str) -> StreamResponse {
        assistant_response(vec![ContentBlock::Text {
            text: serde_json::json!({
                "ok": ok,
                "reason": reason,
                "impossible": impossible,
            })
            .to_string(),
        }])
    }

    fn invalid_goal_response() -> StreamResponse {
        let mut response = assistant_response(vec![ContentBlock::Text {
            text: "not json".into(),
        }]);
        response.usage.output = 7;
        response
    }

    fn tool_call_goal_response() -> StreamResponse {
        let mut response = assistant_response(vec![ContentBlock::tool_use(
            "toolu_goal",
            "shell",
            serde_json::json!({"command": "true"}),
        )]);
        response.usage.output = 7;
        response
    }

    fn goal_response_with_output_usage(reason: &str, output: u32) -> StreamResponse {
        let mut response = goal_response(true, false, reason);
        response.usage.output = output;
        response
    }

    fn make_agent(
        provider: impl Provider + 'static,
        history: &mut History,
    ) -> (Agent<'_>, flume::Receiver<Envelope>) {
        let (raw_tx, event_rx) = flume::unbounded();
        let provider: Arc<dyn Provider> = Arc::new(provider);
        let model = default_model();
        let agent = Agent::new(
            AgentParams {
                provider: Arc::clone(&provider),
                model: model.clone(),
                chat_provider: provider,
                chat_model: model,
                config: AgentConfig::default(),
                tool_output_lines: ToolOutputLines::default(),
                permissions: Arc::new(PermissionManager::new_nonpersistent(
                    caudra_config::PermissionsConfig {
                        default: caudra_config::DefaultEffect::Allow,
                        rules: vec![],
                        ..Default::default()
                    },
                    std::path::PathBuf::from("/tmp"),
                    Arc::default(),
                )),
                session_id: None,
                workspace_session: None,
                remote_project_context: None,
                local_documents: None,
                task_environment: crate::template::env_vars(),
                root_tool_use_id: None,
                mailbox: None,
                context_publisher: None,
                timeouts: caudra_providers::Timeouts::default(),
                file_tracker: FileReadTracker::fresh(),
                path_locks: PathLocks::fresh(),
                baseline: None,
                prompt_slots: Arc::new(crate::prompt::ResolvedSlots::default()),
                prompt_profiles: Arc::new(crate::prompt::profile::PromptProfileCatalog::default()),
                default_task_prompt_profile_name: Arc::from(
                    crate::prompt::profile::BUILTIN_PROFILE_NAME,
                ),
                active_prompt_profile_name: Some(Arc::from(
                    crate::prompt::profile::BUILTIN_PROFILE_NAME,
                )),
                subagent_cancels: Arc::new(crate::cancel::CancelMap::new()),
                subagent_history: SubagentHistoryStore::default(),
                registry: Arc::new(crate::tools::ToolRegistry::new()),
                audience: ToolAudience::MAIN,
                tool_filter: crate::tools::ToolFilter::All,
                model_policy: Arc::new(ModelPolicy::default()),
                workflow: None,
            },
            AgentRunParams {
                history,
                system: "system".into(),
                environment: None,
                instructions: None,
                event_tx: EventSender::new(raw_tx, 0),
                tools: serde_json::json!([]),
                deferred: Vec::new(),
            },
        );
        (agent, event_rx)
    }

    fn default_input() -> AgentInput {
        AgentInput {
            message: "hello".into(),
            mode: AgentMode::Build,
            images: Vec::new(),
            mentions: Vec::new(),
            preamble: Vec::new(),
            thinking: Default::default(),
            fast: false,
            prompt: None,
            resume: false,
        }
    }

    fn repair_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {"a": {"type": "integer"}},
            "required": ["a"],
            "additionalProperties": false,
        })
    }

    fn repair_reply(reply: &str) -> StreamResponse {
        let mut response = assistant_response(vec![ContentBlock::Text { text: reply.into() }]);
        response.usage = REPAIR_USAGE;
        response
    }

    fn repair_input() -> InvalidToolInput {
        InvalidToolInput {
            raw: REPAIR_RAW.into(),
            complete: true,
            clipped: false,
        }
    }

    #[test_case(REPAIR_ACCEPTED, true, false; "accepted_non_eager")]
    #[test_case(REPAIR_REJECTED, false, false; "rejected_non_eager")]
    #[test_case(REPAIR_ACCEPTED, true, true; "accepted_eager")]
    #[test_case(REPAIR_REJECTED, false, true; "rejected_eager")]
    fn request_local_schema_and_repair_usage_are_shared(reply: &str, accepted: bool, eager: bool) {
        smol::block_on(async {
            let mut response = tool_use_response(TEST_TOOL, invalid_tool_input(REPAIR_RAW));
            response.usage = MAIN_REQUEST_USAGE;
            response
                .invalid_tool_inputs
                .insert(RESUME_TOOL_ID.into(), repair_input());
            let provider = MockProvider::new(vec![response, repair_reply(reply)]);
            let captured_tools = provider.captured_tools.clone();
            let mut history = History::new(vec![Message::user(GO.into())]);
            let (mut agent, event_rx) = make_agent(provider, &mut history);
            agent.config.eager_tool_dispatch = eager;
            agent.config.tool_json_repair = true;
            agent.tools = serde_json::json!([{
                "name": TEST_TOOL,
                "input_schema": repair_schema(),
            }]);
            let executions = Arc::new(AtomicUsize::new(0));
            let calls = executions.clone();
            agent.local_tools = Arc::new(HashMap::from([(
                TEST_TOOL.into(),
                crate::tools::local_tool(move |input, ctx| {
                    let calls = calls.clone();
                    Box::pin(async move {
                        assert_eq!(ctx.json_repair.schema(TEST_TOOL), Some(repair_schema()));
                        assert_eq!(
                            input,
                            serde_json::from_str::<Value>(REPAIR_ACCEPTED).unwrap()
                        );
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(TEST_TOOL_RESULT.into())
                    })
                }),
            )]));
            agent.goal.set(GO).unwrap();

            assert!(matches!(agent.turn().await.unwrap(), TurnOutcome::Continue));
            agent.report_speculative();
            assert_eq!(executions.load(Ordering::SeqCst), usize::from(accepted));
            let mut expected_usage = MAIN_REQUEST_USAGE;
            expected_usage += REPAIR_USAGE;
            assert_eq!(agent.usage(), expected_usage);
            assert_eq!(agent.goal.snapshot().unwrap().usage, expected_usage);
            assert_eq!(
                agent.measured.unwrap().reported,
                MAIN_REQUEST_USAGE.total_input()
            );
            assert_eq!(agent.num_turns, 1);
            assert_eq!(agent.history.len(), 3);
            let requests = captured_tools.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0], agent.tools);
            assert_eq!(requests[1], serde_json::json!([]));
            let events: Vec<_> = event_rx.try_iter().map(|envelope| envelope.event).collect();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::TurnComplete(_)))
                    .count(),
                1
            );
            let accounting: Vec<_> = events
                .iter()
                .filter(|event| matches!(event, AgentEvent::ModelUsage { .. }))
                .collect();
            assert_eq!(accounting.len(), 1);
            let AgentEvent::ModelUsage {
                usage,
                purpose,
                provider,
                model,
                cost,
                billing,
            } = accounting[0]
            else {
                unreachable!()
            };
            assert_eq!(*usage, REPAIR_USAGE);
            assert_eq!(*purpose, LedgerPurpose::ToolJsonRepair);
            assert_eq!(*provider, agent.model.provider.to_string());
            assert_eq!(*model, agent.model.id);
            assert_eq!(*cost, agent.model.billed_cost(&REPAIR_USAGE, false));
            assert_eq!(*billing, agent.model.billing);
        });
    }

    #[test_case(REPAIR_ACCEPTED, true; "accepted")]
    #[test_case(REPAIR_REJECTED, false; "rejected")]
    fn repair_accounting_does_not_change_context_or_history(reply: &str, accepted: bool) {
        smol::block_on(async {
            let mut history = History::new(vec![Message::user(GO.into())]);
            let (mut agent, event_rx) =
                make_agent(MockProvider::new(vec![repair_reply(reply)]), &mut history);
            agent.measured = Some(MeasuredContext {
                reported: MAIN_REQUEST_USAGE.total_input(),
                history_len: agent.history.len(),
            });
            agent.goal.set(GO).unwrap();
            let context = agent.context_size();
            let history = serde_json::to_value(agent.history.as_slice()).unwrap();
            let ctx = agent.tool_context();
            ctx.json_repair
                .register_invalid(RESUME_TOOL_ID, repair_input());
            assert_eq!(
                ctx.json_repair
                    .repair(RESUME_TOOL_ID, TEST_TOOL, &repair_schema(), &ctx)
                    .await
                    .is_ok(),
                accepted
            );
            agent.drain_repair_usage(&ctx.json_repair);
            agent.drain_repair_usage(&ctx.json_repair);
            assert_eq!(agent.usage(), REPAIR_USAGE);
            assert_eq!(agent.goal.snapshot().unwrap().usage, REPAIR_USAGE);
            assert_eq!(agent.context_size(), context);
            assert_eq!(
                serde_json::to_value(agent.history.as_slice()).unwrap(),
                history
            );
            assert_eq!(agent.num_turns, 0);
            let events: Vec<_> = event_rx.try_iter().collect();
            assert!(matches!(
                &events[..],
                [Envelope {
                    event: AgentEvent::ModelUsage { .. },
                    ..
                }]
            ));
        });
    }

    #[test_case(true; "cancelled")]
    #[test_case(false; "event_channel_closed")]
    fn finished_repair_usage_survives_dispatch_termination(cancelled: bool) {
        smol::block_on(async {
            let mut history = History::new(vec![Message::user(GO.into())]);
            let (mut agent, event_rx) = make_agent(
                MockProvider::new(vec![repair_reply(REPAIR_ACCEPTED)]),
                &mut history,
            );
            let ctx = agent.tool_context();
            ctx.json_repair
                .register_invalid(RESUME_TOOL_ID, repair_input());
            ctx.json_repair
                .repair(RESUME_TOOL_ID, TEST_TOOL, &repair_schema(), &ctx)
                .await
                .unwrap();
            let (trigger, token) = CancelToken::new();
            agent.cancel = token;
            if cancelled {
                trigger.cancel();
            } else {
                drop(event_rx);
            }
            let result = agent
                .process_tool_calls(empty_response(), ctx.json_repair.clone())
                .await;
            assert_eq!(result.is_ok(), cancelled);
            agent.drain_repair_usage(&ctx.json_repair);
            assert_eq!(agent.usage(), REPAIR_USAGE);
            assert!(ctx.json_repair.take_usage().is_empty());
        });
    }

    fn resume_input() -> AgentInput {
        AgentInput {
            message: String::new(),
            resume: true,
            ..default_input()
        }
    }

    fn cancelled_history(tail: Message) -> History {
        let mut history = History::new(vec![Message::user(GO.into()), tail]);
        sanitize_cancelled_history(&mut history, 0);
        history
    }

    #[test]
    fn turn_publishes_current_request_before_dispatch_and_prepared_request_after_history() {
        smol::block_on(async {
            let store = ContextStore::new();
            let captured = Arc::new(Mutex::new(None));
            let provider = ContextObservingProvider {
                store: store.clone(),
                captured: Arc::clone(&captured),
            };
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(provider, &mut history);
            agent.context_publisher = Some(store.publisher(ContextKey::Main));
            assert!(agent.tool_context().context_publisher.is_some());

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::EndTurn
            );

            let captured = captured
                .lock()
                .unwrap()
                .clone()
                .expect(CAPTURED_CONTEXT_MISSING);
            let prepared = store
                .latest(&ContextKey::Main)
                .expect(PREPARED_CONTEXT_MISSING);
            assert_eq!(captured.readiness, ContextReadiness::CapturedCurrentRequest);
            assert_eq!(prepared.readiness, ContextReadiness::PreparedNextRequest);
            assert_eq!(captured.window.tokens, PUBLISHED_CONTEXT_WINDOW);
            assert_eq!(prepared.window.tokens, PUBLISHED_CONTEXT_WINDOW);
            assert!(prepared.usage.messages > captured.usage.messages);
        });
    }

    #[test]
    fn blocking_tool_publishes_assistant_and_result_history_boundaries() {
        smol::block_on(async {
            let store = ContextStore::new();
            let (started_tx, started_rx) = flume::bounded(1);
            let (release_tx, release_rx) = flume::bounded(1);
            let local_tools = HashMap::from([(
                BLOCKING_TOOL_NAME.to_owned(),
                crate::tools::local_tool(move |_, _| {
                    let started_tx = started_tx.clone();
                    let release_rx = release_rx.clone();
                    Box::pin(async move {
                        started_tx
                            .send_async(())
                            .await
                            .map_err(|error| error.to_string())?;
                        release_rx
                            .recv_async()
                            .await
                            .map_err(|error| error.to_string())?;
                        Ok(BLOCKING_TOOL_RESULT.into())
                    })
                }),
            )]);
            let mut history = History::new(vec![Message::user("hello".into())]);
            let (agent, _event_rx) = make_agent(MockProvider::new(Vec::new()), &mut history);
            let mut agent = agent.with_local_tools(Arc::new(local_tools));
            agent.context_publisher = Some(store.publisher(ContextKey::Main));

            let inspect_pending_context = async {
                started_rx.recv_async().await.unwrap();
                let snapshot = store
                    .latest(&ContextKey::Main)
                    .expect(BLOCKING_TOOL_CONTEXT_MISSING);
                assert_eq!(snapshot.readiness, ContextReadiness::PreparedNextRequest);
                release_tx.send_async(()).await.unwrap();
                snapshot
            };
            let (result, pending) = futures_lite::future::zip(
                agent.process_tool_calls(
                    tool_use_response(BLOCKING_TOOL_NAME, serde_json::json!({})),
                    Arc::new(RepairState::default()),
                ),
                inspect_pending_context,
            )
            .await;
            result.unwrap();

            let completed = store
                .latest(&ContextKey::Main)
                .expect(COMPLETED_TOOL_CONTEXT_MISSING);
            assert_eq!(completed.readiness, ContextReadiness::PreparedNextRequest);
            assert!(completed.usage.messages > pending.usage.messages);
            assert_eq!(
                completed.usage.messages,
                estimate_message_tokens(agent.history.as_slice())
            );
        });
    }

    #[test]
    fn blocking_goal_evaluator_observes_published_main_history_only() {
        smol::block_on(async {
            let store = ContextStore::new();
            let (started_tx, started_rx) = flume::bounded(1);
            let (response_tx, response_rx) = flume::bounded(1);
            let provider = ControlledEvaluatorProvider {
                calls: AtomicUsize::new(0),
                evaluator_started: started_tx,
                evaluator_response: response_rx,
            };
            let goal = GoalHandle::default();
            goal.set("tests pass").unwrap();
            let expected_messages = estimate_message_tokens(&[
                Message::user("hello".into()),
                text_response(StopReason::EndTurn).message,
            ]);
            let mut history = History::new(Vec::new());
            let (agent, _event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_goal(goal);
            agent.context_publisher = Some(store.publisher(ContextKey::Main));

            let inspect_context = async {
                started_rx.recv_async().await.unwrap();
                let snapshot = store.latest(&ContextKey::Main).expect(GOAL_CONTEXT_MISSING);
                assert_eq!(snapshot.readiness, ContextReadiness::PreparedNextRequest);
                assert_eq!(snapshot.usage.messages, expected_messages);
                response_tx
                    .send_async(goal_response(true, false, "verified"))
                    .await
                    .unwrap();
            };
            let (result, ()) =
                futures_lite::future::zip(agent.run(default_input()), inspect_context).await;

            assert_eq!(result.unwrap(), DoneReason::EndTurn);
        });
    }

    fn auth_error() -> AgentError {
        AgentError::api(AUTH_ERROR_STATUS, AUTH_ERROR_MESSAGE)
    }

    #[test]
    fn automatic_reauth_reload_resumes_without_manual_response() {
        smol::block_on(async {
            let state = Arc::new(ReauthState::default());
            let provider = ReauthProvider {
                state: Arc::clone(&state),
            };
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(provider, &mut history);
            let (_answer_tx, answer_rx) = flume::unbounded();
            let mut agent =
                agent.with_user_response_rx(Arc::new(async_lock::Mutex::new(answer_rx)));

            let outcome = agent.wait_for_reauth(auth_error(), true).await.unwrap();

            assert!(matches!(outcome, TurnOutcome::Continue));
            assert_eq!(state.reloads.load(Ordering::SeqCst), 1);
            assert_eq!(state.refreshes.load(Ordering::SeqCst), 0);
            assert_eq!(state.adjustments.load(Ordering::SeqCst), 2);
            assert_eq!(agent.model.context_window, ADJUSTED_CONTEXT_WINDOW);
            drop(agent);
            let events: Vec<_> = event_rx.try_iter().map(|event| event.event).collect();
            assert!(matches!(
                events.as_slice(),
                [
                    AgentEvent::StreamReset,
                    AgentEvent::AuthRequired,
                    AgentEvent::AuthRestored
                ]
            ));
        });
    }

    #[test]
    fn manual_reauth_response_refreshes_auth() {
        smol::block_on(async {
            let state = Arc::new(ReauthState::default());
            let provider = ReauthProvider {
                state: Arc::clone(&state),
            };
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(provider, &mut history);
            let (answer_tx, answer_rx) = flume::unbounded();
            answer_tx.send(String::new()).unwrap();
            let mut agent =
                agent.with_user_response_rx(Arc::new(async_lock::Mutex::new(answer_rx)));

            let outcome = agent.wait_for_reauth(auth_error(), false).await.unwrap();

            assert!(matches!(outcome, TurnOutcome::Continue));
            assert_eq!(state.reloads.load(Ordering::SeqCst), 0);
            assert_eq!(state.refreshes.load(Ordering::SeqCst), 1);
            assert_eq!(state.adjustments.load(Ordering::SeqCst), 2);
            assert_eq!(agent.model.context_window, ADJUSTED_CONTEXT_WINDOW);
            drop(agent);
            let events: Vec<_> = event_rx.try_iter().map(|event| event.event).collect();
            assert!(matches!(
                events.as_slice(),
                [AgentEvent::AuthRequired, AgentEvent::AuthRestored]
            ));
        });
    }

    #[test]
    fn partial_auth_failure_resets_stream_before_automatic_resume() {
        smol::block_on(async {
            let state = Arc::new(ReauthState::default());
            let provider = ReauthProvider {
                state: Arc::clone(&state),
            };
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(provider, &mut history);
            let (_answer_tx, answer_rx) = flume::unbounded();
            let mut agent =
                agent.with_user_response_rx(Arc::new(async_lock::Mutex::new(answer_rx)));

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::EndTurn
            );
            drop(agent);

            let events: Vec<_> = event_rx.try_iter().map(|event| event.event).collect();
            assert!(matches!(
                events.as_slice(),
                [
                    AgentEvent::TextDelta { text },
                    AgentEvent::StreamReset,
                    AgentEvent::AuthRequired,
                    AgentEvent::AuthRestored,
                    AgentEvent::TurnComplete(_),
                    AgentEvent::Done { .. }
                ] if text == PARTIAL_RESPONSE
            ));
            assert_eq!(state.stream_calls.load(Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn terminal_partial_auth_failure_still_resets_stream() {
        smol::block_on(async {
            let state = Arc::new(ReauthState::default());
            let provider = ReauthProvider { state };
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(provider, &mut history);
            agent.reauth_attempts = MAX_REAUTH_ATTEMPTS;

            let Err(error) = agent.wait_for_reauth(auth_error(), true).await else {
                panic!("{EXPECTED_AUTH_ERROR}");
            };
            assert!(matches!(
                error,
                AgentError::Api {
                    status: AUTH_ERROR_STATUS,
                    ..
                }
            ));
            drop(agent);

            let events = event_rx
                .try_iter()
                .map(|event| event.event)
                .collect::<Vec<_>>();
            assert!(matches!(events.as_slice(), [AgentEvent::StreamReset]));
        });
    }

    #[test]
    fn goal_met_gates_done_with_private_tool_free_evaluation() {
        smol::block_on(async {
            let provider = MockProvider::new(vec![
                text_response(StopReason::EndTurn),
                goal_response(true, false, "verified"),
            ]);
            let captured_tools = Arc::clone(&provider.captured_tools);
            let goal = GoalHandle::default();
            goal.set("tests pass").unwrap();
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(provider, &mut history);
            agent.tools = serde_json::json!([{"name": "bash"}]);
            let mut agent = agent.with_goal(goal.clone());

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::EndTurn
            );
            drop(agent);

            let events: Vec<_> = event_rx.try_iter().map(|envelope| envelope.event).collect();
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, AgentEvent::GoalEvaluating { evaluation: 1 }))
            );
            assert!(events.iter().any(|event| matches!(
                event,
                AgentEvent::GoalFinished { result } if result.verdict == super::super::goal::GoalVerdict::Met
            )));
            assert!(matches!(goal.status(), Some(GoalStatus::Finished(_))));
            assert_eq!(
                captured_tools.lock().unwrap().as_slice(),
                [serde_json::json!([{"name": "bash"}]), serde_json::json!([]),]
            );
            assert_eq!(history.len(), 2, "evaluator transcript must remain private");
            assert!(history.as_slice().iter().all(|message| {
                !message
                    .user_text()
                    .is_some_and(|text| text.contains("verified"))
            }));
        });
    }

    #[test]
    fn unmet_goal_continues_without_persisting_evaluator_output() {
        smol::block_on(async {
            let provider = MockProvider::new(vec![
                text_response(StopReason::EndTurn),
                goal_response(false, false, "lint has not run"),
                text_response(StopReason::EndTurn),
                goal_response(true, false, "lint passed"),
            ]);
            let goal = GoalHandle::default();
            goal.set("lint passes").unwrap();
            let mut history = History::new(Vec::new());
            let (agent, _event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_goal(goal.clone());

            agent.run(default_input()).await.unwrap();
            drop(agent);

            let Some(GoalStatus::Finished(result)) = goal.status() else {
                panic!("goal did not finish");
            };
            assert_eq!(result.evaluations, 2);
            let history_text: Vec<_> = history
                .as_slice()
                .iter()
                .filter_map(Message::first_text_content)
                .collect();
            assert!(
                history_text
                    .iter()
                    .any(|text| text.contains("lint has not run"))
            );
            assert!(history.as_slice().iter().all(|message| {
                message
                    .first_text_content()
                    .is_none_or(|text| !text.contains("\"ok\""))
            }));
        });
    }

    #[test]
    fn replacing_goal_discards_in_flight_verdict() {
        smol::block_on(async {
            let (started_tx, started_rx) = flume::bounded(1);
            let (response_tx, response_rx) = flume::bounded(1);
            let provider = ControlledEvaluatorProvider {
                calls: AtomicUsize::new(0),
                evaluator_started: started_tx,
                evaluator_response: response_rx,
            };
            let goal = GoalHandle::default();
            goal.set("old goal").unwrap();
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_goal(goal.clone());

            let control = async {
                started_rx.recv_async().await.unwrap();
                goal.set("replacement").unwrap();
                response_tx
                    .send_async(goal_response(true, false, "old goal met"))
                    .await
                    .unwrap();
            };
            let (result, ()) = futures_lite::future::zip(agent.run(default_input()), control).await;
            assert_eq!(result.unwrap(), DoneReason::EndTurn);
            drop(agent);

            let replacement = goal.snapshot().unwrap();
            assert_eq!(replacement.condition.as_ref(), "replacement");
            assert_eq!(replacement.evaluations, 0);
            assert!(event_rx.try_iter().any(|envelope| matches!(
                envelope.event,
                AgentEvent::GoalEvaluation { applied: false, .. }
            )));
        });
    }

    #[test]
    fn cancellation_during_goal_evaluation_stays_cancelled() {
        smol::block_on(async {
            let (started_tx, started_rx) = flume::bounded(1);
            let (_keep_response_open, response_rx) = flume::bounded(1);
            let provider = ControlledEvaluatorProvider {
                calls: AtomicUsize::new(0),
                evaluator_started: started_tx,
                evaluator_response: response_rx,
            };
            let goal = GoalHandle::default();
            goal.set("keep active").unwrap();
            let (trigger, cancel) = CancelToken::new();
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_goal(goal.clone()).with_cancel(cancel);

            let cancel_when_started = async {
                started_rx.recv_async().await.unwrap();
                trigger.cancel();
            };
            let (result, ()) =
                futures_lite::future::zip(agent.run(default_input()), cancel_when_started).await;
            assert_eq!(result.unwrap(), DoneReason::Cancelled);
            drop(agent);

            assert!(goal.snapshot().is_some());
            assert!(event_rx.try_iter().any(|envelope| matches!(
                envelope.event,
                AgentEvent::Done {
                    reason: DoneReason::Cancelled,
                    ..
                }
            )));
        });
    }

    #[test]
    fn invalid_evaluator_attempts_are_billed() {
        smol::block_on(async {
            let provider = MockProvider::new(vec![
                text_response(StopReason::EndTurn),
                invalid_goal_response(),
                invalid_goal_response(),
                invalid_goal_response(),
            ]);
            let goal = GoalHandle::default();
            goal.set("strict result").unwrap();
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_goal(goal.clone());

            agent.run(default_input()).await.unwrap();
            drop(agent);

            assert_eq!(goal.snapshot().unwrap().usage.output, 21);
            assert!(event_rx.try_iter().any(|envelope| matches!(
                envelope.event,
                AgentEvent::GoalEvaluationFailed {
                    usage: TokenUsage { output: 21, .. },
                    ..
                }
            )));
        });
    }

    #[test]
    fn evaluator_tool_call_is_reprompted_without_interrupting_the_goal() {
        smol::block_on(async {
            let valid_output = 11;
            let provider = MockProvider::new(vec![
                text_response(StopReason::EndTurn),
                tool_call_goal_response(),
                goal_response_with_output_usage("verified after retry", valid_output),
            ]);
            let captured_models = Arc::clone(&provider.captured_models);
            let current_model = default_model();
            let weak_model = Model::resolve(
                caudra_providers::ModelPurpose::Fast,
                &current_model,
                &ModelPolicy::default(),
            )
            .unwrap();
            let goal = GoalHandle::default();
            goal.set("tests pass").unwrap();
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_goal(goal.clone());

            agent.run(default_input()).await.unwrap();
            drop(agent);

            assert_eq!(
                captured_models.lock().unwrap().as_slice(),
                [current_model.spec(), weak_model.spec(), weak_model.spec()]
            );
            assert!(!event_rx.try_iter().any(|envelope| matches!(
                envelope.event,
                AgentEvent::GoalEvaluationFailed { applied: true, .. }
            )));
            let Some(GoalStatus::Finished(result)) = goal.status() else {
                panic!("goal did not finish after evaluator retry");
            };
            assert_eq!(result.reason.as_ref(), "verified after retry");
            assert_eq!(result.usage.output, 7 + valid_output);
            assert_eq!(result.evaluations, 1);
        });
    }

    #[test]
    fn auto_goal_evaluator_falls_back_when_fast_model_is_unavailable() {
        smol::block_on(async {
            let provider = MockProvider::with_results(vec![
                Ok(text_response(StopReason::EndTurn)),
                Err(AgentError::api(404, MODEL_UNAVAILABLE)),
                Ok(goal_response(true, false, "verified by chat")),
            ]);
            let captured_models = Arc::clone(&provider.captured_models);
            let current_model = default_model();
            let weak_model = Model::resolve(
                caudra_providers::ModelPurpose::Fast,
                &current_model,
                &ModelPolicy::default(),
            )
            .unwrap();
            let goal = GoalHandle::default();
            goal.set("tests pass").unwrap();
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_goal(goal.clone());

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::EndTurn
            );
            drop(agent);

            assert_eq!(
                captured_models.lock().unwrap().as_slice(),
                [
                    current_model.spec(),
                    weak_model.spec(),
                    current_model.spec()
                ]
            );
            let events: Vec<_> = event_rx.try_iter().map(|envelope| envelope.event).collect();
            assert!(events.iter().any(|event| matches!(
                event,
                AgentEvent::GoalEvaluation {
                    evaluation: 1,
                    applied: true,
                    model,
                    ..
                } if model == &current_model.spec()
            )));
            assert!(!events.iter().any(|event| matches!(
                event,
                AgentEvent::GoalEvaluationFailed { applied: true, .. }
            )));
            let Some(GoalStatus::Finished(result)) = goal.status() else {
                panic!("goal did not finish after fallback");
            };
            assert_eq!(result.evaluations, 1);
            assert_eq!(result.reason.as_ref(), "verified by chat");
        });
    }

    #[test]
    fn auto_fallback_attributes_each_attempt_to_its_model() {
        smol::block_on(async {
            let fallback_output = 11;
            let provider = MockProvider::with_results(vec![
                Ok(text_response(StopReason::EndTurn)),
                Ok(invalid_goal_response()),
                Err(AgentError::api(404, MODEL_UNAVAILABLE)),
                Ok(goal_response_with_output_usage(
                    "verified by chat",
                    fallback_output,
                )),
            ]);
            let captured_models = Arc::clone(&provider.captured_models);
            let current_model = default_model();
            let weak_model = Model::resolve(
                caudra_providers::ModelPurpose::Fast,
                &current_model,
                &ModelPolicy::default(),
            )
            .unwrap();
            let goal = GoalHandle::default();
            goal.set("tests pass").unwrap();
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_goal(goal.clone());

            agent.run(default_input()).await.unwrap();
            drop(agent);

            assert_eq!(
                captured_models.lock().unwrap().as_slice(),
                [
                    current_model.spec(),
                    weak_model.spec(),
                    weak_model.spec(),
                    current_model.spec(),
                ]
            );
            let events: Vec<_> = event_rx.try_iter().map(|envelope| envelope.event).collect();
            assert!(events.iter().any(|event| matches!(
                event,
                AgentEvent::GoalEvaluationFailed {
                    applied: false,
                    usage: TokenUsage { output: 7, .. },
                    model,
                    ..
                } if model == &weak_model.spec()
            )));
            assert!(events.iter().any(|event| matches!(
                event,
                AgentEvent::GoalEvaluation {
                    applied: true,
                    usage: TokenUsage { output, .. },
                    model,
                    ..
                } if *output == fallback_output && model == &current_model.spec()
            )));
            assert!(events.iter().any(|event| matches!(
                event,
                AgentEvent::Done {
                    usage: TokenUsage { output: 18, .. },
                    ..
                }
            )));
            let Some(GoalStatus::Finished(result)) = goal.status() else {
                panic!("goal did not finish after fallback");
            };
            assert_eq!(result.usage.output, 18);
            assert_eq!(result.evaluations, 1);
        });
    }

    #[test]
    fn goal_loop_cap_uses_the_session_limit() {
        smol::block_on(async {
            let continuation_limit = 2;
            let mut responses = Vec::new();
            for _ in 0..=continuation_limit {
                responses.push(text_response(StopReason::EndTurn));
                responses.push(goal_response(false, false, "more work remains"));
            }
            let goal = GoalHandle::default();
            goal.set_continuation_limit(continuation_limit);
            goal.set("never met").unwrap();
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(MockProvider::new(responses), &mut history);
            let mut agent = agent.with_goal(goal.clone());

            agent.run(default_input()).await.unwrap();
            drop(agent);

            assert_eq!(goal.snapshot().unwrap().evaluations, continuation_limit + 1);
            assert!(event_rx.try_iter().any(|envelope| matches!(
                envelope.event,
                AgentEvent::GoalLoopCap {
                    evaluations,
                    continuations,
                    limit,
                } if evaluations == continuation_limit + 1
                    && continuations == continuation_limit
                    && limit == continuation_limit
            )));
        });
    }

    #[test]
    fn turn_limit_reports_unresolved_goal() {
        smol::block_on(async {
            let provider = MockProvider::new(vec![
                text_response(StopReason::EndTurn),
                goal_response(false, false, "more work remains"),
            ]);
            let goal = GoalHandle::default();
            goal.set("needs another turn").unwrap();
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(provider, &mut history);
            agent.config.max_turns = Some(1);
            let mut agent = agent.with_goal(goal.clone());

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::MaxTurns
            );
            drop(agent);

            assert!(goal.snapshot().is_some());
            assert!(event_rx.try_iter().any(|envelope| matches!(
                envelope.event,
                AgentEvent::GoalTurnLimit { evaluations: 1 }
            )));
        });
    }

    #[test]
    fn run_ingests_preamble_then_mailbox_then_user_message() {
        smol::block_on(async {
            let id = caudra_storage::id::CaudraId::generate();
            let mailbox = SessionMailbox::register(id);
            SessionMailbox::notify(id, "mailbox".into(), false).unwrap();
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            agent.mailbox = Some(mailbox);
            let mut input = default_input();
            input.preamble = vec![Message::observation("preamble".into())];

            agent.run(input).await.unwrap();
            drop(agent);

            assert_eq!(history.as_slice()[0].user_text(), Some("preamble"));
            assert_eq!(history.as_slice()[1].user_text(), Some("mailbox"));
            assert_eq!(history.as_slice()[2].user_text(), Some("hello"));
        });
    }

    fn plan_mode() -> AgentMode {
        AgentMode::Plan(std::path::PathBuf::from(TEST_PLAN_PATH))
    }

    fn plan_announcement() -> Message {
        mode_switch_notice(&[], &plan_mode()).expect(EXPECTED_PLAN_NOTICE)
    }

    #[test_case(AgentMode::Build, Some(AnnouncedMode::Build) ; "build_announces_build")]
    #[test_case(AgentMode::ReadOnly, None ; "read_only_announces_nothing")]
    fn announced_mode_of_agent_mode(mode: AgentMode, expected: Option<AnnouncedMode>) {
        assert_eq!(AnnouncedMode::of(&mode), expected);
    }

    #[test]
    fn plan_mode_announces_plan() {
        assert_eq!(AnnouncedMode::of(&plan_mode()), Some(AnnouncedMode::Plan));
    }

    #[test]
    fn a_fresh_build_session_announces_nothing() {
        assert!(mode_switch_notice(&[], &AgentMode::Build).is_none());
    }

    #[test]
    fn entering_plan_announces_plan() {
        let notice = plan_announcement();
        assert!(notice.is_observation());
        assert!(
            notice
                .user_text()
                .is_some_and(|text| text.contains(crate::prompt::PLAN_MODE_MARKER))
        );
    }

    #[test_case(false; "embedded_plan_uses_only_file_tools")]
    #[test_case(true; "remote_plan_uses_only_local_document_tools")]
    fn plan_notice_names_only_the_active_workspace_plan_tools(remote: bool) {
        let mode = if remote {
            AgentMode::RemotePlan(PlanRef::new("plan-test").expect("valid plan reference"))
        } else {
            plan_mode()
        };
        let notice = mode_switch_notice(&[], &mode).expect(EXPECTED_PLAN_NOTICE);
        let text = notice.user_text().expect(EXPECTED_PLAN_NOTICE);
        for tool in ["local_document_write", "local_document_apply_patch"] {
            assert_eq!(text.contains(tool), remote, "{tool}: {text}");
        }
        for tool in ["`file_write`", "`file_edit`", "`file_apply_patch`"] {
            assert_eq!(text.contains(tool), !remote, "{tool}: {text}");
        }
        if !remote {
            assert!(!text.contains("local_"));
        }
        assert!(!text.contains("{plan_write_tools}"));
    }

    #[test]
    fn leaving_plan_announces_build() {
        let history = [plan_announcement()];
        let notice = mode_switch_notice(&history, &AgentMode::Build).expect(EXPECTED_BUILD_NOTICE);
        assert!(
            notice
                .user_text()
                .is_some_and(|text| text.contains(crate::prompt::BUILD_MODE_MARKER))
        );
    }

    #[test]
    fn repeated_build_turns_announce_once() {
        let history = [
            plan_announcement(),
            mode_switch_notice(&[plan_announcement()], &AgentMode::Build)
                .expect(EXPECTED_BUILD_NOTICE),
        ];
        assert!(mode_switch_notice(&history, &AgentMode::Build).is_none());
    }

    #[test]
    fn repeated_plan_turns_announce_once() {
        let history = [plan_announcement()];
        assert!(mode_switch_notice(&history, &plan_mode()).is_none());
    }

    /// A revert cuts the turn and the announcements that trail it, so the next
    /// turn starts from a transcript that says nothing about either. Both have
    /// to speak up again rather than leave the model on a block that was
    /// rewound away.
    #[test]
    fn a_turn_reverted_away_takes_its_announcements_with_it() {
        let turn = [
            Message::user("draft the plan".into()),
            standing_notice(&[], crate::prompt::ENVIRONMENT_MARKER, Some(ENVIRONMENT))
                .expect(EXPECTED_ENVIRONMENT_NOTICE),
            plan_announcement(),
        ];
        let position = turn
            .iter()
            .position(|message| !message.is_observation())
            .expect(EXPECTED_USER_TURN);
        let reverted = &turn[..position];

        assert!(
            standing_notice(
                reverted,
                crate::prompt::ENVIRONMENT_MARKER,
                Some(ENVIRONMENT)
            )
            .is_some()
        );
        assert!(mode_switch_notice(reverted, &plan_mode()).is_some());
    }

    /// The case an in-memory previous mode cannot catch: `Agent` is rebuilt per
    /// run, so only the transcript still knows the session was planning.
    #[test]
    fn a_restored_plan_transcript_switched_to_build_announces_build() {
        let history = [
            Message::user("draft the plan".into()),
            plan_announcement(),
            Message::observation("a mention preamble".into()),
        ];
        let notice = mode_switch_notice(&history, &AgentMode::Build).expect(EXPECTED_BUILD_NOTICE);
        assert!(
            notice
                .user_text()
                .is_some_and(|text| text.contains(crate::prompt::BUILD_MODE_MARKER))
        );
    }

    /// Only Caudra's own announcements count, or quoting this conversation back
    /// at the model would rewrite its mode.
    #[test]
    fn a_marker_quoted_by_the_user_is_not_an_announcement() {
        let history = [Message::user(crate::prompt::PLAN_MODE_MARKER.into())];
        assert!(mode_switch_notice(&history, &AgentMode::Build).is_none());
    }

    fn environment_announcement(environment: &str) -> Message {
        standing_notice(&[], crate::prompt::ENVIRONMENT_MARKER, Some(environment))
            .expect(EXPECTED_ENVIRONMENT_NOTICE)
    }

    #[test]
    fn a_fresh_session_announces_its_environment() {
        let notice = environment_announcement(ENVIRONMENT);
        assert!(notice.is_observation());
        assert_eq!(notice.user_text(), Some(ENVIRONMENT));
    }

    #[test]
    fn an_unchanged_environment_announces_once() {
        let history = [environment_announcement(ENVIRONMENT)];
        assert!(
            standing_notice(
                &history,
                crate::prompt::ENVIRONMENT_MARKER,
                Some(ENVIRONMENT)
            )
            .is_none()
        );
    }

    /// A date rollover or a model switch is the whole reason this is announced
    /// rather than carried by the system prompt.
    #[test]
    fn a_changed_environment_is_re_announced() {
        let history = [environment_announcement(ENVIRONMENT)];
        let notice = standing_notice(
            &history,
            crate::prompt::ENVIRONMENT_MARKER,
            Some(ENVIRONMENT_NEXT_DAY),
        )
        .expect(EXPECTED_ENVIRONMENT_NOTICE);
        assert_eq!(notice.user_text(), Some(ENVIRONMENT_NEXT_DAY));
    }

    /// The compaction contract: dropping the announcement costs one re-emit
    /// rather than leaving the model without an environment.
    #[test]
    fn a_compacted_history_re_announces_the_environment() {
        let history = [Message::user("what survived compaction".into())];
        assert_eq!(
            standing_notice(
                &history,
                crate::prompt::ENVIRONMENT_MARKER,
                Some(ENVIRONMENT)
            )
            .expect(EXPECTED_ENVIRONMENT_NOTICE)
            .user_text(),
            Some(ENVIRONMENT)
        );
    }

    #[test]
    fn a_subagent_announces_no_environment() {
        assert!(standing_notice(&[], crate::prompt::ENVIRONMENT_MARKER, None).is_none());
    }

    /// Announcements are only worth persisting if they come back as
    /// observations: `last_announced` ignores anything else, so a lossy round
    /// trip would silently re-announce on every session load.
    #[test]
    fn announcements_survive_a_storage_round_trip() {
        let mut items: Vec<caudra_providers::HistoryItem> = Vec::new();
        for message in [environment_announcement(ENVIRONMENT), plan_announcement()] {
            let parent = items.last().map(|item| item.id);
            items.extend(caudra_providers::expand_message(&message, parent));
        }
        let restored = caudra_providers::project_messages(&items).unwrap();

        assert!(restored.iter().all(Message::is_observation));
        assert!(
            standing_notice(
                &restored,
                crate::prompt::ENVIRONMENT_MARKER,
                Some(ENVIRONMENT)
            )
            .is_none()
        );
        assert!(mode_switch_notice(&restored, &plan_mode()).is_none());
    }

    #[test]
    fn run_announces_the_environment_ahead_of_the_mode() {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            agent.environment = Some(ENVIRONMENT.to_owned());
            let mut input = default_input();
            input.mode = plan_mode();

            agent.run(input).await.unwrap();
            drop(agent);

            assert_eq!(history.as_slice()[0].user_text(), Some("hello"));
            assert_eq!(history.as_slice()[1].user_text(), Some(ENVIRONMENT));
            assert!(
                history.as_slice()[2]
                    .user_text()
                    .is_some_and(|text| text.contains(crate::prompt::PLAN_MODE_MARKER))
            );
        });
    }

    /// A reminder the user cannot see is a reminder they cannot audit, and the
    /// mention body is the one injection they already have on screen: the path
    /// they typed sits directly above it.
    #[test]
    fn run_reports_injected_messages_but_not_mention_bodies() {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            agent.environment = Some(ENVIRONMENT.to_owned());
            let mut input = default_input();
            input.mode = plan_mode();
            input.preamble = vec![Message::mention(MENTION_BODY.into())];

            agent.run(input).await.unwrap();
            drop(agent);

            let injected: Vec<String> = drain_events(&event_rx)
                .into_iter()
                .filter_map(|envelope| match envelope.event {
                    AgentEvent::Injected { text } => Some(text),
                    _ => None,
                })
                .collect();

            assert_eq!(injected[0], ENVIRONMENT);
            assert!(injected[1].contains(crate::prompt::PLAN_MODE_MARKER));
            assert_eq!(injected.len(), 2);
            assert!(
                history
                    .as_slice()
                    .iter()
                    .any(|message| message.first_text_content() == Some(MENTION_BODY)),
                "the mention body must still reach the model"
            );
        });
    }

    /// Instruction drift patches the system prompt, so it has to land ahead of
    /// the mode rules the model reads under the stale text.
    #[test]
    fn run_announces_changed_instructions_between_the_environment_and_the_mode() {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            agent.environment = Some(ENVIRONMENT.to_owned());
            agent.instructions = Some(INSTRUCTIONS_CHANGED.to_owned());
            let mut input = default_input();
            input.mode = plan_mode();

            agent.run(input).await.unwrap();
            drop(agent);

            let texts: Vec<_> = history
                .as_slice()
                .iter()
                .filter_map(Message::user_text)
                .collect();
            assert_eq!(texts[0], "hello");
            assert_eq!(texts[1], ENVIRONMENT);
            assert_eq!(texts[2], INSTRUCTIONS_CHANGED);
            assert!(texts[3].contains(crate::prompt::PLAN_MODE_MARKER));
        });
    }

    #[test]
    fn an_unchanged_instruction_notice_announces_once() {
        let history = [Message::observation(INSTRUCTIONS_CHANGED.to_owned())];
        assert!(
            standing_notice(
                &history,
                crate::prompt::INSTRUCTIONS_CHANGED_MARKER,
                Some(INSTRUCTIONS_CHANGED)
            )
            .is_none()
        );
    }

    #[test]
    fn run_announces_plan_mode_below_the_user_message() {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            let mut input = default_input();
            input.mode = plan_mode();

            agent.run(input).await.unwrap();
            drop(agent);

            assert_eq!(history.as_slice()[0].user_text(), Some("hello"));
            assert!(
                history.as_slice()[1]
                    .user_text()
                    .is_some_and(|text| text.contains(crate::prompt::PLAN_MODE_MARKER))
            );
        });
    }

    /// The whole split in one run. The shell result arrived before the prompt
    /// and nothing else still holds it, so it keeps its place above. The
    /// environment is re-sent whenever the transcript lacks it, so it lands
    /// below, where a rewind of the prompt takes it along.
    #[test]
    fn an_arrival_leads_the_turn_whose_reminders_trail_it() {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            agent.environment = Some(ENVIRONMENT.to_owned());
            let mut input = default_input();
            input.preamble = vec![Message::user(SHELL_RESULT.into())];

            agent.run(input).await.unwrap();
            drop(agent);

            let texts: Vec<_> = history
                .as_slice()
                .iter()
                .filter(|message| matches!(message.role, Role::User))
                .filter_map(Message::user_text)
                .collect();
            assert_eq!(texts, [SHELL_RESULT, "hello", ENVIRONMENT]);
        });
    }

    /// An MCP prompt seeds a canned exchange and may close it on an assistant
    /// message. Trailing the turn, that message would be the request's last,
    /// which is a prefill the provider rejects once reasoning is on.
    #[test]
    fn an_assistant_arrival_never_leaves_the_request_on_a_prefill() {
        smol::block_on(async {
            let captured: Arc<Mutex<Vec<Message>>> = Arc::default();
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(
                RequestCapturingProvider {
                    captured: Arc::clone(&captured),
                },
                &mut history,
            );
            let mut input = default_input();
            input.preamble = vec![Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: PROMPT_SEED.into(),
                }],
                ..Default::default()
            }];

            agent.run(input).await.unwrap();
            drop(agent);

            let request = captured.lock().unwrap().clone();
            assert_eq!(
                request.first().and_then(Message::first_text_content),
                Some(PROMPT_SEED)
            );
            assert!(
                matches!(request.last().expect(EXPECTED_REQUEST).role, Role::User),
                "{NO_PREFILL}"
            );
        });
    }

    #[test]
    fn queued_input_drains_preamble_and_mailbox() {
        smol::block_on(async {
            let id = caudra_storage::id::CaudraId::generate();
            let mailbox = SessionMailbox::register(id);
            SessionMailbox::notify(id, "mailbox".into(), false).unwrap();
            let mut input = default_input();
            input.preamble = vec![Message::observation("preamble".into())];
            let source = MockInterruptSource::new(vec![ExtractedCommand::Interrupt(
                input,
                0,
                QueueItemId::new(),
            )]);
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(MockProvider::new(Vec::new()), &mut history);
            agent.mailbox = Some(mailbox);
            let mut agent = agent.with_interrupt_source(source);

            assert!(agent.handle_queued_command().await.unwrap());
            drop(agent);

            let text = history
                .as_slice()
                .iter()
                .map(Message::user_text)
                .collect::<Vec<_>>();
            assert_eq!(text, [Some("preamble"), Some("mailbox"), Some("hello")]);
            assert!(history.as_slice()[0].is_observation());
            assert!(history.as_slice()[1].is_observation());
        });
    }

    #[test]
    fn queued_batch_emits_one_event_and_preserves_separate_history_messages() {
        smol::block_on(async {
            let first_id = QueueItemId::new();
            let second_id = QueueItemId::new();
            let mut first = default_input();
            first.message = "first".into();
            first.images.push(caudra_providers::ImageSource::new(
                caudra_providers::ImageMediaType::Png,
                Arc::from("aGVsbG8="),
            ));
            let mut second = default_input();
            second.message = "second".into();
            let source = MockInterruptSource::new(vec![ExtractedCommand::InterruptBatch(vec![
                crate::QueuedInterrupt {
                    id: first_id,
                    input: first,
                    run_id: 0,
                },
                crate::QueuedInterrupt {
                    id: second_id,
                    input: second,
                    run_id: 0,
                },
            ])]);
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(MockProvider::new(Vec::new()), &mut history);
            let mut agent = agent.with_interrupt_source(source);

            assert!(agent.handle_queued_command().await.unwrap());
            drop(agent);

            let events = drain_events(&event_rx);
            let items = events
                .iter()
                .find_map(|envelope| match &envelope.event {
                    AgentEvent::QueueBatchConsumed { items } => Some(items),
                    _ => None,
                })
                .expect("batch consumption event missing");
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].id, first_id);
            assert_eq!(items[1].id, second_id);
            assert_eq!(history.len(), 2);
            assert_eq!(history.as_slice()[0].user_text(), Some("first"));
            assert_eq!(history.as_slice()[1].user_text(), Some("second"));
            assert!(matches!(
                history.as_slice()[0].content.first(),
                Some(ContentBlock::Image { .. })
            ));
        });
    }

    #[test]
    fn initial_batch_preserves_plain_user_messages() {
        smol::block_on(async {
            let mut first = default_input();
            first.message = "guide".into();
            let mut replacement = default_input();
            replacement.message = "replace".into();
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );

            agent
                .run_initial_batch(first, vec![replacement])
                .await
                .unwrap();
            drop(agent);

            assert_eq!(history.as_slice()[0].user_text(), Some("guide"));
            assert_eq!(history.as_slice()[1].user_text(), Some("replace"));
            assert!(!has_interrupt_in_history(history.as_slice()));
        });
    }

    #[test]
    fn wake_only_run_does_not_insert_an_empty_user_turn() {
        smol::block_on(async {
            let id = caudra_storage::id::CaudraId::generate();
            let mailbox = SessionMailbox::register(id);
            SessionMailbox::notify(id, "failed".into(), true).unwrap();
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            agent.mailbox = Some(mailbox);
            let mut input = default_input();
            input.message.clear();

            agent.run(input).await.unwrap();
            drop(agent);

            assert_eq!(history.as_slice().len(), 2);
            assert!(history.as_slice()[0].is_observation());
            assert!(matches!(history.as_slice()[1].role, Role::Assistant));
        });
    }

    /// The point of a resume: nothing the caller did not write reaches the
    /// request, which lands back on the tool result the cancelled loop
    /// stopped at.
    #[test]
    fn a_resume_after_a_tool_cancel_sends_no_new_turn() {
        smol::block_on(async {
            let captured: Arc<Mutex<Vec<Message>>> = Arc::default();
            let mut history = cancelled_history(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    RESUME_TOOL_ID,
                    "read",
                    serde_json::json!({}),
                )],
                ..Default::default()
            });
            let before = history.len();
            let (mut agent, _event_rx) = make_agent(
                RequestCapturingProvider {
                    captured: Arc::clone(&captured),
                },
                &mut history,
            );

            agent.run(resume_input()).await.unwrap();
            drop(agent);

            let request = captured.lock().unwrap().clone();
            assert_eq!(request.len(), before - 1, "{RESUME_DROPPED_ONLY_MARKER}");
            let last = request.last().unwrap();
            assert!(matches!(last.role, Role::User));
            assert!(
                last.content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
            );
        });
    }

    /// An assistant tail has no seam to resume from, and a prefill breaks once
    /// reasoning is on, so this is the one case that says it in words.
    #[test]
    fn a_resume_after_a_cut_reply_injects_one_continuation() {
        smol::block_on(async {
            let mut history = cancelled_history(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: PARTIAL_RESPONSE.into(),
                }],
                ..Default::default()
            });
            let (mut agent, event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );

            agent.run(resume_input()).await.unwrap();
            drop(agent);

            let injected: Vec<String> = drain_events(&event_rx)
                .into_iter()
                .filter_map(|envelope| match envelope.event {
                    AgentEvent::Injected { text, .. } => Some(text),
                    _ => None,
                })
                .collect();
            assert_eq!(injected, [RESUME_PROMPT]);
            assert!(matches!(
                history.as_slice()[2].content.first(),
                Some(ContentBlock::Text { text }) if text == RESUME_PROMPT
            ));
        });
    }

    fn drain_events(rx: &flume::Receiver<Envelope>) -> Vec<Envelope> {
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        events
    }

    async fn run_agent(provider: MockProvider, max_turns: Option<u32>) -> (u32, DoneReason) {
        let mut history = History::new(Vec::new());
        let (mut agent, event_rx) = make_agent(provider, &mut history);
        agent.config.max_turns = max_turns;
        let _ = agent.run(default_input()).await;
        drain_events(&event_rx)
            .into_iter()
            .find_map(|e| match e.event {
                AgentEvent::Done {
                    num_turns, reason, ..
                } => Some((num_turns, reason)),
                _ => None,
            })
            .expect("expected Done event")
    }

    fn has_event(events: &[Envelope], predicate: impl Fn(&AgentEvent) -> bool) -> bool {
        events.iter().any(|e| predicate(&e.event))
    }

    fn has_interrupt_in_history(history: &[Message]) -> bool {
        history.iter().any(|m| {
            m.content.iter().any(
                |b| matches!(b, ContentBlock::Text { text } if text.contains("<user-interrupt>")),
            )
        })
    }

    fn tool_call_response(tool_name: &str, tool_id: &str) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    tool_id,
                    tool_name,
                    serde_json::json!({"pattern": "*.nonexistent_test_xyz", "path": "/tmp"}),
                )],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::ToolUse),
            ..Default::default()
        }
    }

    fn tool_use_response(tool_name: &str, input: Value) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("t1", tool_name, input)],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::ToolUse),
            ..Default::default()
        }
    }

    #[test]
    fn mcp_definitions_refresh_per_request() {
        smol::block_on(async {
            let provider = MockProvider::new(vec![
                tool_use_response(
                    crate::tools::TOOL_SEARCH_TOOL_NAME,
                    serde_json::json!({"query": "fetch issue"}),
                ),
                text_response(StopReason::EndTurn),
            ]);
            let captured = Arc::clone(&provider.captured_tools);
            let mut history = History::new(Vec::new());
            let (agent, _event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_mcp(Some(crate::mcp::stub_session(&[(
                "srv.fetch_issue",
                "Fetch a GitHub issue",
            )])));
            agent.run(default_input()).await.unwrap();

            let captured = captured.lock().unwrap();
            assert_eq!(captured.len(), 2);
            let first = tool_names(&captured[0]);
            assert!(first.contains(&crate::tools::TOOL_SEARCH_TOOL_NAME));
            assert!(!first.contains(&"srv__fetch_issue"));
            assert!(tool_names(&captured[1]).contains(&"srv__fetch_issue"));
        });
    }

    fn small_context_model(context_window: u32, max_output_tokens: u32) -> Model {
        let mut model = default_model();
        model.context_window = context_window;
        model.max_output_tokens = Some(max_output_tokens);
        model
    }

    #[track_caller]
    fn assert_ends_with_cancel_marker(history: &History) {
        let last = history.as_slice().last().unwrap();
        assert!(matches!(last.role, Role::User));
        assert!(
            matches!(&last.content[0], ContentBlock::Text { text } if text == "[Cancelled by user]")
        );
    }

    /// A truncated answer buys another turn, but only until one of the two
    /// budgets runs out: the continuation limit or the caller's `max_turns`.
    #[test_case(&[StopReason::EndTurn], None, 1, DoneReason::EndTurn ; "end_turn_completes")]
    #[test_case(&[StopReason::MaxTokens, StopReason::EndTurn], None, 2, DoneReason::EndTurn ; "max_tokens_continues")]
    #[test_case(&[StopReason::MaxTokens, StopReason::MaxTokens, StopReason::MaxTokens, StopReason::EndTurn], None, 4, DoneReason::EndTurn ; "last_retry_can_complete")]
    #[test_case(&[StopReason::MaxTokens, StopReason::EndTurn], Some(1), 1, DoneReason::MaxTurns ; "turn_budget_exhausted")]
    fn turn_counting(
        stops: &[StopReason],
        max_turns: Option<u32>,
        expected_turns: u32,
        expected_reason: DoneReason,
    ) {
        smol::block_on(async {
            let responses: Vec<_> = stops.iter().map(|s| text_response(*s)).collect();
            let provider = MockProvider::new(responses);
            let (turns, reason) = run_agent(provider, max_turns).await;
            assert_eq!(turns, expected_turns);
            assert_eq!(reason, expected_reason);
        });
    }

    #[test_case(true; "steering_enabled")]
    #[test_case(false; "steering_disabled")]
    fn rebuilt_agents_share_hard_limits(enabled: bool) {
        smol::block_on(async {
            let mut history = History::default();
            let (mut first, _events) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            Arc::make_mut(&mut first.config.steering).enabled = Some(enabled);
            first.run(default_input()).await.unwrap();
            let steering = Arc::clone(&first.steering);
            drop(first);
            let (agent, _events) = make_agent(
                MockProvider::new(vec![text_response(StopReason::MaxTokens)]),
                &mut history,
            );
            let mut agent = agent.with_steering(steering);
            agent.config.max_turns = Some(2);
            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::MaxTurns
            );
            assert_eq!(agent.num_turns, 1);
            assert_eq!(steering::lock(&agent.steering).responses(), 2);
        });
    }

    #[test_case(None, None, true, false; "default_last_retry_succeeds_after_tools")]
    #[test_case(None, None, false, false; "default_rule_exhaustion_after_tools")]
    #[test_case(Some(1), None, false, false; "combined_exhaustion")]
    #[test_case(Some(0), None, false, true; "zero_budget_empty_max_tokens")]
    #[test_case(None, Some(1), false, false; "custom_rule_limit")]
    fn truncation_retries_count_corrections_not_tool_rounds(
        budget: Option<u32>,
        limit: Option<u32>,
        succeeds: bool,
        empty: bool,
    ) {
        smol::block_on(async {
            let attempts = budget
                .unwrap_or(RECOVERY_BUDGET)
                .min(limit.unwrap_or(TRUNCATION_ATTEMPTS));
            let mut responses: Vec<_> = (0..TOOL_ROUNDS)
                .map(|round| tool_use_response(TEST_TOOL, serde_json::json!({"round": round})))
                .collect();
            for index in 0..=attempts {
                let mut response = if empty {
                    empty_response()
                } else {
                    text_response(StopReason::MaxTokens)
                };
                response.stop_reason = Some(if succeeds && index == attempts {
                    StopReason::EndTurn
                } else {
                    StopReason::MaxTokens
                });
                responses.push(response);
            }
            for response in &mut responses {
                response.usage.output = OUTPUT_TOKENS;
            }
            let calls = Arc::new(AtomicUsize::new(0));
            let executed = Arc::clone(&calls);
            let local_tools = Arc::new(HashMap::from([(
                TEST_TOOL.to_owned(),
                crate::tools::local_tool(move |_, _| {
                    executed.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(TEST_TOOL_RESULT.into()) })
                }),
            )]));
            let mut history = History::default();
            let (agent, events) = make_agent(MockProvider::new(responses), &mut history);
            let mut agent = agent.with_local_tools(local_tools);
            let config = Arc::make_mut(&mut agent.config.steering);
            config.max_recoveries = budget;
            config.rules.truncation.max_attempts = limit;
            let result = agent.run(default_input()).await;
            if succeeds {
                assert_eq!(result.unwrap(), DoneReason::EndTurn);
            } else {
                assert!(
                    matches!(result, Err(AgentError::SteeringExhausted { rule }) if rule == STEERING_TRUNCATION)
                );
            }
            assert_eq!(calls.load(Ordering::SeqCst), TOOL_ROUNDS as usize);
            assert_eq!(agent.num_turns, TOOL_ROUNDS + attempts + 1);
            assert_eq!(agent.usage().output, agent.num_turns * OUTPUT_TOKENS);
            assert_eq!(
                agent.response_text(),
                (!empty)
                    .then(|| VISIBLE_RESPONSE.repeat((attempts + 1) as usize))
                    .as_deref()
            );
            let origins: Vec<_> = agent
                .history
                .as_slice()
                .iter()
                .filter_map(|message| message.steering.as_ref())
                .collect();
            assert_eq!(origins.len(), attempts as usize);
            assert!(
                origins
                    .iter()
                    .all(|origin| origin.rule == STEERING_TRUNCATION
                        && origin.kind == SteeringKind::Recovery)
            );
            assert!(
                !events
                    .try_iter()
                    .any(|event| matches!(event.event, AgentEvent::Nudge))
            );
        });
    }

    #[test_case(true, false, false; "master_disabled_text")]
    #[test_case(false, false, false; "rule_disabled_text")]
    #[test_case(true, true, false; "master_disabled_empty")]
    #[test_case(false, true, false; "rule_disabled_empty")]
    #[test_case(true, false, true; "master_disabled_text_compacts")]
    #[test_case(false, false, true; "rule_disabled_text_compacts")]
    #[test_case(true, true, true; "master_disabled_empty_compacts")]
    #[test_case(false, true, true; "rule_disabled_empty_compacts")]
    fn disabled_truncation_preserves_max_tokens(master: bool, empty: bool, compact: bool) {
        smol::block_on(async {
            let mut response = if empty {
                empty_response()
            } else {
                text_response(StopReason::MaxTokens)
            };
            response.stop_reason = Some(StopReason::MaxTokens);
            response.usage.output = OUTPUT_TOKENS;
            let mut responses = Vec::new();
            if compact {
                response.usage.input = LARGE_CONTEXT;
            }
            responses.push(response);
            if compact {
                responses.push(text_response(StopReason::EndTurn));
            }
            let provider = MockProvider::new(responses);
            let requests = Arc::clone(&provider.captured_models);
            let mut history = History::new(vec![Message::user(GO.into()); 10]);
            let (mut agent, events) = make_agent(provider, &mut history);
            agent.auto_compact = compact;
            agent.model = Arc::new(small_context_model(200_000, 8_192));
            let config = Arc::make_mut(&mut agent.config.steering);
            config.max_recoveries = Some(0);
            if master {
                config.enabled = Some(false);
            } else {
                config.rules.truncation.enabled = Some(false);
            }
            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::MaxTokens
            );
            assert_eq!(agent.num_turns, 1);
            assert_eq!(requests.lock().unwrap().len(), 1 + usize::from(compact));
            assert_eq!(agent.usage().output, OUTPUT_TOKENS);
            assert_eq!(agent.response_text(), (!empty).then_some(VISIBLE_RESPONSE));
            assert!(
                !agent
                    .history
                    .as_slice()
                    .iter()
                    .any(|message| message.steering.is_some())
            );
            let events = drain_events(&events);
            assert_eq!(
                has_event(&events, |event| matches!(event, AgentEvent::CompactionDone)),
                compact
            );
            assert!(!has_event(&events, |event| matches!(
                event,
                AgentEvent::Nudge
            )));
        });
    }

    #[test_case(false; "master_disabled")]
    #[test_case(true; "rule_disabled")]
    fn disabled_truncation_leaves_goal_continuation_independent(rule_only: bool) {
        smol::block_on(async {
            let goal = GoalHandle::default();
            goal.set(GO).unwrap();
            let mut history = History::default();
            let (agent, _events) = make_agent(
                MockProvider::new(vec![
                    text_response(StopReason::MaxTokens),
                    goal_response(false, false, GO),
                    text_response(StopReason::EndTurn),
                    goal_response(true, false, GO),
                ]),
                &mut history,
            );
            let mut agent = agent.with_goal(goal);
            let config = Arc::make_mut(&mut agent.config.steering);
            if rule_only {
                config.rules.truncation.enabled = Some(false);
            } else {
                config.enabled = Some(false);
            }
            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::EndTurn
            );
            assert_eq!(agent.num_turns, 2);
            assert!(
                !agent
                    .history
                    .as_slice()
                    .iter()
                    .any(|message| message.steering.is_some())
            );
        });
    }

    #[test_case(true; "tool_commentary")]
    #[test_case(false; "empty_padding")]
    fn truncation_output_excludes_earlier_commentary_and_padding(tool: bool) {
        smol::block_on(async {
            let mut earlier = if tool {
                tool_call_response("glob", RESUME_TOOL_ID)
            } else {
                empty_response()
            };
            if tool {
                earlier.message.content.insert(
                    0,
                    ContentBlock::Text {
                        text: PARTIAL_RESPONSE.into(),
                    },
                );
            }
            let mut history = History::default();
            let (mut agent, _events) = make_agent(
                MockProvider::new(vec![
                    earlier,
                    text_response(StopReason::MaxTokens),
                    text_response(StopReason::EndTurn),
                ]),
                &mut history,
            );
            agent.run(default_input()).await.unwrap();
            assert_eq!(
                agent.response_text(),
                Some(format!("{VISIBLE_RESPONSE}{VISIBLE_RESPONSE}").as_str())
            );
        });
    }

    #[test_case(Some(true),  true,  true  ; "after_tool_use_turn")]
    #[test_case(Some(false), true,  true  ; "after_text_only_turn")]
    #[test_case(None,        false, false ; "channel_empty")]
    fn interrupt_handling(queued: Option<bool>, expect_consumed: bool, expect_injected: bool) {
        smol::block_on(async {
            let source = if queued.is_some() {
                Some(MockInterruptSource::new(vec![ExtractedCommand::Interrupt(
                    default_input(),
                    0,
                    QueueItemId::new(),
                )]))
            } else {
                None
            };

            let tool_use = queued.unwrap_or(true);
            let responses = if tool_use {
                vec![
                    tool_call_response("glob", "t1"),
                    text_response(StopReason::EndTurn),
                ]
            } else {
                vec![
                    text_response(StopReason::EndTurn),
                    text_response(StopReason::EndTurn),
                ]
            };

            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(MockProvider::new(responses), &mut history);
            if let Some(s) = source {
                agent = agent.with_interrupt_source(s);
            }
            let _ = agent.run(default_input()).await;
            let events = drain_events(&event_rx);

            assert_eq!(
                has_event(&events, |e| matches!(
                    e,
                    AgentEvent::QueueItemConsumed { .. }
                )),
                expect_consumed,
            );
            assert_eq!(
                has_interrupt_in_history(history.as_slice()),
                expect_injected
            );
        });
    }

    #[test_case(
        (0..10).map(|i| Message::user(format!("msg {i}"))).collect(),
        vec![ExtractedCommand::Compact(0)],
        vec![tool_call_response("glob", "t1"), text_response(StopReason::EndTurn), text_response(StopReason::EndTurn)]
        ; "compaction_via_interrupt_source"
    )]
    fn compaction_through_interrupt(
        prior: Vec<Message>,
        commands: Vec<ExtractedCommand>,
        responses: Vec<StreamResponse>,
    ) {
        smol::block_on(async {
            let source = MockInterruptSource::new(commands);

            let mut history = History::new(prior);
            let (agent, _event_rx) = make_agent(MockProvider::new(responses), &mut history);
            let result = agent
                .with_interrupt_source(source)
                .run(default_input())
                .await;

            assert!(result.is_ok());
        });
    }

    #[test_case(|_| {}, true ; "first_prompt_of_a_stored_session")]
    #[test_case(|a| a.session_id = None, false ; "no_session_to_name")]
    #[test_case(|a| a.rollback_len = 1, false ; "history_already_had_turns")]
    #[test_case(|a| a.config.generate_titles = false, false ; "disabled_in_config")]
    #[test_case(|a| a.root_tool_use_id = Some("call-1".into()), false ; "subagent_run")]
    #[test_case(|a| a.audience = ToolAudience::GENERAL_SUB, false ; "not_the_main_audience")]
    fn should_generate_title_only_for_a_session_opening_prompt(
        adjust: fn(&mut Agent<'_>),
        expected: bool,
    ) {
        const PROMPT: &str = "add refresh token support";
        let mut history = History::default();
        let (mut agent, _event_rx) = make_agent(MockProvider::new(vec![]), &mut history);
        agent.session_id = Some(SessionRef::from_id(caudra_storage::id::CaudraId::generate()));
        adjust(&mut agent);

        assert_eq!(agent.should_generate_title(PROMPT), expected);
    }

    /// Yields once before answering. A provider that answers on its first poll
    /// races an already-cancelled token as a coin flip; yielding lets the
    /// cancelled side win deterministically, so the test can tell a shared
    /// token from an independent one.
    struct YieldingProvider(Mutex<Option<StreamResponse>>);

    impl Provider for YieldingProvider {
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
            Box::pin(async {
                futures_lite::future::yield_now().await;
                self.0.lock().unwrap().take().ok_or(AgentError::Channel)
            })
        }

        fn list_models(
            &self,
        ) -> BoxFuture<'_, Result<Vec<caudra_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    fn title_response() -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: MODEL_TITLE.into(),
                }],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::EndTurn),
            ..Default::default()
        }
    }

    /// A run's `CancelTrigger` fires when the turn drops it, and a title is
    /// asked for at the start of a turn but answers long after it. Sharing that
    /// token meant every title came back `cancelled` instead.
    #[test]
    fn a_title_outlives_the_run_that_asked_for_it() {
        smol::block_on(async {
            let mut history = History::default();
            let (mut agent, event_rx) = make_agent(
                YieldingProvider(Mutex::new(Some(title_response()))),
                &mut history,
            );
            agent.session_id = Some(SessionRef::from_id(caudra_storage::id::CaudraId::generate()));
            let (trigger, cancel) = CancelToken::new();
            agent.cancel = cancel;
            // The turn is over before the detached title ever reaches the wire.
            trigger.cancel();

            agent.spawn_title(TITLE_PROMPT.into());

            let envelope = futures_lite::future::race(event_rx.recv_async(), async {
                smol::Timer::after(TITLE_EVENT_TIMEOUT).await;
                Err(flume::RecvError::Disconnected)
            })
            .await
            .expect(TITLE_MUST_SURVIVE);

            assert!(
                matches!(envelope.event, AgentEvent::SessionTitle { title: Some(named), .. }
                    if named == MODEL_TITLE),
                "{TITLE_MUST_SURVIVE}"
            );
        });
    }

    #[test]
    fn a_blank_prompt_never_names_a_session() {
        let mut history = History::default();
        let (mut agent, _event_rx) = make_agent(MockProvider::new(vec![]), &mut history);
        agent.session_id = Some(SessionRef::from_id(caudra_storage::id::CaudraId::generate()));

        assert!(!agent.should_generate_title("   \n\t"));
    }

    #[test_case(true,  Some(170_000), true  ; "enabled_and_over_threshold")]
    #[test_case(true,  Some(150_000), false ; "enabled_but_below_threshold")]
    #[test_case(false, Some(170_000), false ; "disabled_even_over_threshold")]
    #[test_case(true,  None,          false ; "nothing_billed_yet_is_not_an_overflow")]
    fn try_auto_compact_behavior(enabled: bool, reported: Option<u32>, expected: bool) {
        smol::block_on(async {
            let responses = if expected {
                vec![text_response(StopReason::EndTurn)]
            } else {
                vec![]
            };
            let mut history = History::new(vec![Message::user("go".into())]);
            let (mut agent, event_rx) = make_agent(MockProvider::new(responses), &mut history);
            agent.model = Arc::new(small_context_model(200_000, 8_192));
            agent.auto_compact = enabled;
            agent.measured = reported.map(|reported| MeasuredContext {
                reported,
                history_len: agent.history.len(),
            });
            let result = agent.try_auto_compact().await.unwrap();

            assert_eq!(result, expected);
            drop(agent);
            assert_eq!(
                has_event(&drain_events(&event_rx), |e| matches!(
                    e,
                    AgentEvent::Compacting
                )),
                expected,
            );
        });
    }

    const ANCHOR_IGNORED: &str =
        "the provider's count must anchor the size, not an estimate of the whole transcript";
    const GROWTH_UNCOUNTED: &str = "what was appended after the count must be added to it";
    const STALE_ANCHOR_KEPT: &str =
        "an anchor older than the transcript describes messages that no longer exist";

    /// The estimate is only ever asked how much was appended, so a transcript it
    /// counts wildly wrong in absolute terms still reports the provider's number.
    #[test]
    fn a_measured_context_reports_what_the_provider_billed() {
        const REPORTED: u32 = 300_000;
        let history = [Message::user("go".into()), Message::user("again".into())];
        let measured = MeasuredContext {
            reported: REPORTED,
            history_len: history.len(),
        };

        assert_eq!(
            measured.extended_by(&history),
            Some(REPORTED),
            "{ANCHOR_IGNORED}"
        );
    }

    #[test]
    fn a_measured_context_adds_only_what_arrived_after_it() {
        const REPORTED: u32 = 300_000;
        let history = [Message::user("go".into()), Message::user("again".into())];
        let measured = MeasuredContext {
            reported: REPORTED,
            history_len: 1,
        };

        let size = measured.extended_by(&history).expect(GROWTH_UNCOUNTED);
        assert_eq!(
            size,
            REPORTED + estimate_message_tokens(&history[1..]),
            "{GROWTH_UNCOUNTED}"
        );
        assert!(size > REPORTED, "{GROWTH_UNCOUNTED}");
    }

    /// Compaction replaces what was billed. Carrying the old count forward would
    /// hold the pre-compaction size and compact again immediately.
    #[test]
    fn a_measured_context_expires_when_the_transcript_shrinks_under_it() {
        let measured = MeasuredContext {
            reported: 300_000,
            history_len: 4,
        };

        assert_eq!(
            measured.extended_by(&[Message::user("summary".into())]),
            None,
            "{STALE_ANCHOR_KEPT}"
        );
    }

    const CIPHERTEXT_AS_PROSE: &str =
        "an encrypted reasoning item must be charged for what it encodes, not for its armour";
    const ARMOUR_IS_CHEAPER_THAN_PROSE: &str = "base64 tokenizes worse than anything it stands in for, so counting it as text \
         must cost strictly more than the rate that replaced it";

    /// Reasoning items are decrypted before they are billed, so the provider
    /// charges the reasoning inside rather than the base64 it travelled as. A
    /// tokenizer run over the armour roughly doubles the bill, which is what
    /// made a transcript read past its window while it still fitted.
    #[test]
    fn encrypted_reasoning_is_charged_at_the_rate_of_what_it_encodes() {
        const CIPHERTEXT_LEN: usize = 40_000;
        let ciphertext = "aGVsbG8".repeat(CIPHERTEXT_LEN / "aGVsbG8".len());
        let block = ContentBlock::Thinking {
            thinking: String::new(),
            signature: None,
            responses: Some(caudra_providers::ResponsesReasoning {
                item_id: "rs_1".into(),
                encrypted_content: Some(ciphertext.clone()),
            }),
            interrupted: false,
            duration_ms: None,
        };

        let charged = message_block_tokens(&block);
        let framing = estimate_tokens_cached(THINKING_BLOCK_FRAMING)
            + estimate_tokens_cached(RESPONSES_REASONING_FRAMING)
            + estimate_tokens_cached(RESPONSES_ENCRYPTED_CONTENT_FRAMING)
            + estimate_tokens_cached("rs_1");

        assert_eq!(
            charged - framing,
            u32::try_from(ciphertext.len() / OPAQUE_BLOB_CHARS_PER_TOKEN).unwrap(),
            "{CIPHERTEXT_AS_PROSE}"
        );
        assert!(
            estimate_tokens_cached(&ciphertext) > charged - framing,
            "{ARMOUR_IS_CHEAPER_THAN_PROSE}"
        );
    }

    #[test]
    fn do_compact_appends_post_instructions_to_continue_message() {
        smol::block_on(async {
            const POST: &str = "Re-read plan.md";
            let context_store = ContextStore::new();
            let mut history = History::new(vec![Message::user("go".into())]);
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            agent.context_publisher = Some(context_store.publisher(ContextKey::Main));
            agent.config.post_compaction_instructions = Some(POST.into());
            agent.do_compact().await.unwrap();
            assert_eq!(
                context_store
                    .latest(&ContextKey::Main)
                    .expect(PREPARED_CONTEXT_MISSING)
                    .readiness,
                ContextReadiness::PreparedNextRequest
            );
            drop(agent);

            let last = history.as_slice().last().unwrap();
            assert!(matches!(
                &last.content[0],
                ContentBlock::Text { text } if text.ends_with(POST) && text != POST
            ));
        });
    }

    #[test]
    fn cancel_token_aborts_during_api_call() {
        smol::block_on(async {
            let (trigger, cancel) = CancelToken::new();
            trigger.cancel();

            let context_store = ContextStore::new();
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(StubStreamProvider::default(), &mut history);
            let mut agent = agent.with_cancel(cancel);
            agent.context_publisher = Some(context_store.publisher(ContextKey::Main));

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::Cancelled
            );
            assert_eq!(
                context_store
                    .latest(&ContextKey::Main)
                    .expect(PREPARED_CONTEXT_MISSING)
                    .readiness,
                ContextReadiness::PreparedNextRequest
            );
            drop(agent);
            assert_ends_with_cancel_marker(&history);
            assert!(has_event(&drain_events(&event_rx), |e| matches!(
                e,
                AgentEvent::Done {
                    reason: DoneReason::Cancelled,
                    ..
                }
            )));
        });
    }

    #[test]
    fn cancel_mid_stream_keeps_partial_text_in_history() {
        const PARTIAL: &str = "partial answer";
        smol::block_on(async {
            let (trigger, cancel) = CancelToken::new();
            let provider = StubStreamProvider {
                delta: Some(PARTIAL),
                cancel_after_delta: Mutex::new(Some(trigger)),
                ..Default::default()
            };
            let mut history = History::new(Vec::new());
            let (agent, _event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_cancel(cancel);

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::Cancelled
            );
            drop(agent);
            assert_ends_with_cancel_marker(&history);
            let messages = history.as_slice();
            let partial = &messages[messages.len() - 2];
            assert!(matches!(partial.role, Role::Assistant));
            let expected = format!("{PARTIAL}\n\n{CANCELLED_TEXT_NOTE}");
            assert!(
                matches!(&partial.content[0], ContentBlock::Text { text } if *text == expected),
                "kept text must carry the truncation note so the model never resumes it"
            );
        });
    }

    #[test]
    fn cancel_mid_stream_keeps_partial_reasoning_as_interrupted() {
        const PARTIAL: &str = "partial reasoning";
        smol::block_on(async {
            let (trigger, cancel) = CancelToken::new();
            let provider = StubStreamProvider {
                delta: Some(PARTIAL),
                delta_is_thinking: true,
                cancel_after_delta: Mutex::new(Some(trigger)),
                ..Default::default()
            };
            let mut history = History::new(Vec::new());
            let (agent, _event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_cancel(cancel);

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::Cancelled
            );
            drop(agent);
            assert_ends_with_cancel_marker(&history);
            let partial = &history.as_slice()[history.len() - 2];
            assert!(matches!(
                &partial.content[0],
                ContentBlock::Thinking {
                    thinking,
                    interrupted: true,
                    duration_ms: Some(_),
                    ..
                } if thinking == PARTIAL
            ));
            assert!(partial.reasoning_source.is_some());
        });
    }

    #[test]
    fn reasoning_only_max_tokens_response_is_retained_before_continuation() {
        smol::block_on(async {
            let reasoning = StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::thinking("summary".into(), None)],
                    ..Default::default()
                },
                stop_reason: Some(StopReason::MaxTokens),
                ..Default::default()
            };
            let mut history = History::new(Vec::new());
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![reasoning, text_response(StopReason::EndTurn)]),
                &mut history,
            );

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::EndTurn
            );
            drop(agent);

            assert!(history.as_slice().iter().any(|message| {
                message.content.iter().any(
                    |block| matches!(block, ContentBlock::Thinking { thinking, .. } if thinking == "summary"),
                )
            }));
        });
    }

    /// The `Retry` event already made the view drop the failed attempt's
    /// text, so history must not resurrect it (see `StreamError`).
    #[test]
    fn cancel_during_retry_backoff_discards_failed_attempt_text() {
        const PARTIAL: &str = "doomed attempt";
        smol::block_on(async {
            let (trigger, cancel) = CancelToken::new();
            let provider = StubStreamProvider {
                delta: Some(PARTIAL),
                fail_status: Some(529),
                ..Default::default()
            };
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_cancel(cancel);

            let mut trigger = Some(trigger);
            let pump = smol::spawn(async move {
                while let Ok(envelope) = event_rx.recv_async().await {
                    if matches!(envelope.event, AgentEvent::Retry { .. })
                        && let Some(t) = trigger.take()
                    {
                        t.cancel();
                    }
                }
            });

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::Cancelled
            );
            drop(agent);
            pump.await;

            assert_ends_with_cancel_marker(&history);
            assert!(
                history
                    .as_slice()
                    .iter()
                    .all(|m| !m.content.iter().any(
                        |b| matches!(b, ContentBlock::Text { text } if text.contains(PARTIAL))
                    )),
                "failed attempt's text must not reach history"
            );
        });
    }

    /// The provider's own `Retry-After` sets the wait, and its explanation of
    /// why survives all the way to the event the status bar renders.
    #[test]
    fn retry_event_carries_the_provider_hint_and_reason() {
        const RETRY_AFTER: Duration = Duration::from_secs(7);
        const BODY: &str =
            r#"{"error":{"type":"rate_limit_error","message":"input tokens per minute exceeded"}}"#;
        const EXPECTED: &str = "Rate limited: rate_limit_error: input tokens per minute exceeded";
        smol::block_on(async {
            let (trigger, cancel) = CancelToken::new();
            let provider = StubStreamProvider {
                fail_status: Some(429),
                fail_body: Some(BODY),
                fail_retry_after: Some(RETRY_AFTER),
                ..Default::default()
            };
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_cancel(cancel);

            let observed = Arc::new(Mutex::new(None));
            let pump = smol::spawn({
                let observed = Arc::clone(&observed);
                let mut trigger = Some(trigger);
                async move {
                    while let Ok(envelope) = event_rx.recv_async().await {
                        if let AgentEvent::Retry {
                            attempt,
                            message,
                            delay_ms,
                        } = envelope.event
                            && let Some(t) = trigger.take()
                        {
                            *observed.lock().unwrap() = Some((attempt, message, delay_ms));
                            t.cancel();
                        }
                    }
                }
            });

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::Cancelled
            );
            drop(agent);
            pump.await;

            let (attempt, message, delay_ms) = observed.lock().unwrap().take().unwrap();
            assert_eq!(attempt, 1);
            assert_eq!(delay_ms, RETRY_AFTER.as_millis() as u64);
            assert_eq!(message, EXPECTED);
        });
    }

    /// A nudge cuts the backoff short. Without it the second attempt waits out
    /// the provider's full window, so reaching attempt two at all is the proof.
    #[test]
    fn a_nudge_retries_without_waiting_out_the_backoff() {
        const LONG_WAIT: Duration = Duration::from_secs(60);
        const GENEROUS_BOUND: Duration = Duration::from_secs(10);
        const FIRST: u32 = 1;
        const SECOND: u32 = 2;
        smol::block_on(async {
            let (trigger, cancel) = CancelToken::new();
            let retry_now = Nudge::default();
            let provider = StubStreamProvider {
                fail_status: Some(429),
                fail_retry_after: Some(LONG_WAIT),
                ..Default::default()
            };
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(provider, &mut history);
            let mut agent = agent.with_cancel(cancel).with_retry_now(retry_now.clone());

            let attempts = Arc::new(Mutex::new(Vec::new()));
            let pump = smol::spawn({
                let attempts = Arc::clone(&attempts);
                let mut trigger = Some(trigger);
                async move {
                    while let Ok(envelope) = event_rx.recv_async().await {
                        let AgentEvent::Retry { attempt, .. } = envelope.event else {
                            continue;
                        };
                        attempts.lock().unwrap().push(attempt);
                        match attempt {
                            FIRST => retry_now.notify(),
                            _ => drop(trigger.take()),
                        }
                    }
                }
            });

            let started = Instant::now();
            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::Cancelled
            );
            drop(agent);
            pump.await;

            assert_eq!(*attempts.lock().unwrap(), vec![FIRST, SECOND]);
            assert!(
                started.elapsed() < GENEROUS_BOUND,
                "the nudge must not wait out the provider's window"
            );
        });
    }

    #[test_case(
        vec![tool_call_response("nonexistent_tool_xyz", "t1"), text_response(StopReason::EndTurn)],
        "t1"
        ; "parse_error"
    )]
    #[test_case(
        vec![tool_call_response("glob", "t1"), tool_call_response("glob", "t2"), tool_call_response("glob", "t3"), text_response(StopReason::EndTurn)],
        "t3"
        ; "doom_loop"
    )]
    fn error_emits_tool_done_event(responses: Vec<StreamResponse>, expected_error_id: &str) {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(MockProvider::new(responses), &mut history);
            let _ = agent.run(default_input()).await;
            drop(agent);
            let events = drain_events(&event_rx);

            assert!(has_event(&events, |e| matches!(
                e,
                AgentEvent::ToolDone(done) if done.is_error && done.id == expected_error_id
            )));
        });
    }

    #[test_case(
        vec![
            tool_call_response("glob", "t1"),
            empty_response(),
            text_response(StopReason::EndTurn),
        ],
        3, 1
        ; "nudge_on_empty_after_tools"
    )]
    #[test_case(
        [tool_call_response("glob", "t1"), thinking_response()]
            .into_iter()
            .chain((0..MAX_NUDGES).map(|_| empty_response()))
            .collect(),
        MAX_NUDGES + 2, MAX_NUDGES as usize
        ; "gives_up_after_max_nudges"
    )]
    #[test_case(
        vec![
            tool_call_response("glob", "t1"),
            text_response(StopReason::EndTurn),
        ],
        2, 0
        ; "no_nudge_when_text_after_tools"
    )]
    #[test_case(
        vec![
            empty_response(),
            text_response(StopReason::EndTurn),
        ],
        2, 1
        ; "nudge_without_recent_tools"
    )]
    #[test_case(
        vec![
            thinking_response(),
            text_response(StopReason::EndTurn),
        ],
        2, 1
        ; "nudge_on_thinking_only_without_tools"
    )]
    #[test_case(
        (0..=MAX_IDLE_NUDGES).map(|_| empty_response()).collect(),
        MAX_IDLE_NUDGES + 1, MAX_IDLE_NUDGES as usize
        ; "gives_up_after_max_idle_nudges"
    )]
    fn nudge_behavior(responses: Vec<StreamResponse>, expected_turns: u32, expected_nudges: usize) {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(MockProvider::new(responses), &mut history);
            let result = agent.run(default_input()).await;
            assert_eq!(agent.num_turns, expected_turns);
            if expected_nudges == MAX_NUDGES as usize || expected_nudges == MAX_IDLE_NUDGES as usize
            {
                assert!(matches!(result, Err(AgentError::SteeringExhausted { .. })));
            } else {
                assert!(result.is_ok());
            }
            drop(agent);
            let events = drain_events(&event_rx);

            let nudges = events
                .iter()
                .filter(|e| matches!(e.event, AgentEvent::Nudge))
                .count();
            assert_eq!(nudges, expected_nudges);

            assert!(
                history
                    .as_slice()
                    .iter()
                    .all(|m| m.content.iter().any(|b| !b.is_thinking())),
                "history holds a message no provider will accept: {:?}",
                history.as_slice()
            );
        });
    }

    /// Pins the regression where a stale nudge counter made a follow-up
    /// "continue" end instantly: the budget lives in the history tail, and
    /// the new user message breaks the streak.
    #[test]
    fn nudge_budget_resets_on_new_run() {
        smol::block_on(async {
            let responses = [tool_call_response("glob", "t1")]
                .into_iter()
                .chain((0..=MAX_NUDGES).map(|_| empty_response()))
                .chain([empty_response(), text_response(StopReason::EndTurn)])
                .collect();
            let mut history = History::new(Vec::new());
            let (mut agent, event_rx) = make_agent(MockProvider::new(responses), &mut history);
            let _ = agent.run(default_input()).await;
            let _ = agent.run(default_input()).await;
            drop(agent);
            let events = drain_events(&event_rx);

            let nudges = events
                .iter()
                .filter(|e| matches!(e.event, AgentEvent::Nudge))
                .count();
            assert_eq!(nudges, MAX_NUDGES as usize + 1);
        });
    }

    #[test_case(empty_response(); "fully_empty")]
    #[test_case(thinking_response(); "reasoning_only")]
    #[test_case(assistant_response(vec![ContentBlock::Text { text: " \n\t ".into() }]); "whitespace")]
    fn empty_variants_have_the_same_episode_budget(response: StreamResponse) {
        smol::block_on(async {
            let mut history = History::default();
            let responses = (0..3)
                .map(|_| assistant_response(response.message.content.clone()))
                .collect();
            let (mut agent, events) = make_agent(MockProvider::new(responses), &mut history);
            assert!(
                matches!(agent.run(default_input()).await, Err(AgentError::SteeringExhausted { rule }) if rule == STEERING_EMPTY)
            );
            assert_eq!(agent.num_turns, 3);
            assert_eq!(agent.response_text(), None);
            assert_eq!(
                events
                    .try_iter()
                    .filter(|event| matches!(event.event, AgentEvent::Nudge))
                    .count(),
                2
            );
        });
    }

    #[test_case("A normal final answer."; "prose")]
    #[test_case(r#"{"name":"shell","arguments":{"command":"pwd"}}"#; "tool_json")]
    #[test_case("```json\n{\"tool\":\"shell\"}\n```"; "fenced_example")]
    fn default_steering_never_reopens_final_text(text: &str) {
        smol::block_on(async {
            let mut history = History::default();
            let (mut agent, _events) = make_agent(
                MockProvider::new(vec![assistant_response(vec![ContentBlock::Text {
                    text: text.into(),
                }])]),
                &mut history,
            );
            Arc::make_mut(&mut agent.config.steering)
                .rules
                .no_tool_use
                .after_responses = Some(1);
            agent.tools = serde_json::json!([{"name":"shell"}]);
            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::EndTurn
            );
            assert_eq!(agent.num_turns, 1);
            assert_eq!(agent.response_text(), Some(text));
            assert!(
                !agent
                    .history
                    .as_slice()
                    .iter()
                    .any(|message| message.steering.is_some())
            );
        });
    }

    #[test_case(false; "no_visible_tools")]
    #[test_case(true; "visible_tools")]
    fn no_tool_hint_crosses_user_turns_without_reopening_them(has_tools: bool) {
        smol::block_on(async {
            let mut history = History::new(
                (0..3)
                    .flat_map(|_| {
                        [
                            Message::user(GO.into()),
                            text_response(StopReason::EndTurn).message,
                        ]
                    })
                    .collect(),
            );
            for run in 0..2 {
                let (mut agent, _events) = make_agent(
                    MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                    &mut history,
                );
                if has_tools {
                    agent.tools = serde_json::json!([{"name":"shell"}]);
                }
                assert_eq!(
                    agent.run(default_input()).await.unwrap(),
                    DoneReason::EndTurn
                );
                assert_eq!(agent.num_turns, 1);
                assert_eq!(
                    agent
                        .history
                        .as_slice()
                        .iter()
                        .filter(|message| message.steering.is_some())
                        .count(),
                    usize::from(has_tools),
                    "{run}"
                );
            }
        });
    }

    #[test_case(false; "no_captured_report")]
    #[test_case(true; "captured_report")]
    fn ready_report_does_not_swallow_protocol_failure(ready: bool) {
        smol::block_on(async {
            let mut history = History::default();
            let (agent, _events) = make_agent(
                MockProvider::new(vec![text_response(StopReason::ToolUse)]),
                &mut history,
            );
            let mut agent = agent.with_report_ready(Arc::new(AtomicBool::new(ready)));
            Arc::make_mut(&mut agent.config.steering).max_recoveries = Some(0);
            assert!(
                matches!(agent.run(default_input()).await, Err(AgentError::SteeringExhausted { rule }) if rule == STEERING_PROTOCOL)
            );
        });
    }

    #[test_case(false; "text_protocol_mismatch")]
    #[test_case(true; "empty_protocol_mismatch")]
    fn explicit_tool_stop_has_two_corrections(empty: bool) {
        smol::block_on(async {
            let mut response = if empty {
                empty_response()
            } else {
                text_response(StopReason::ToolUse)
            };
            response.stop_reason = Some(StopReason::ToolUse);
            let mut history = History::default();
            let responses = (0..3)
                .map(|_| StreamResponse {
                    message: response.message.clone(),
                    stop_reason: response.stop_reason,
                    ..Default::default()
                })
                .collect();
            let (mut agent, _events) = make_agent(MockProvider::new(responses), &mut history);
            assert!(
                matches!(agent.run(default_input()).await, Err(AgentError::SteeringExhausted { rule }) if rule == STEERING_PROTOCOL)
            );
            assert_eq!(agent.num_turns, 3);
            assert_eq!(
                agent
                    .history
                    .as_slice()
                    .iter()
                    .filter(|message| message.steering.is_some())
                    .count(),
                2
            );
        });
    }

    #[test_case(false; "invalid_arguments")]
    #[test_case(true; "repeat_refusals")]
    fn tool_feedback_charges_one_transition_without_supplemental_prompt(repeated: bool) {
        smol::block_on(async {
            let mut history = History::default();
            let responses = (0..3)
                .map(|index| {
                    if repeated {
                        return tool_use_response(INVALID_TOOL, serde_json::json!({}));
                    }
                    let raw = format!("{{{index}");
                    let mut response = tool_use_response(INVALID_TOOL, invalid_tool_input(&raw));
                    let id = response.message.tool_uses().next().unwrap().0.to_owned();
                    response.invalid_tool_inputs.insert(
                        id,
                        InvalidToolInput {
                            raw,
                            complete: true,
                            clipped: false,
                        },
                    );
                    response
                })
                .collect();
            let (mut agent, _events) = make_agent(MockProvider::new(responses), &mut history);
            Arc::make_mut(&mut agent.config.steering).max_recoveries =
                Some(if repeated { 0 } else { 2 });
            assert!(
                matches!(agent.run(default_input()).await, Err(AgentError::SteeringExhausted { rule }) if rule == STEERING_TOOL_REPAIR)
            );
            assert_eq!(agent.num_turns, 3);
            assert!(
                !agent
                    .history
                    .as_slice()
                    .iter()
                    .any(|message| message.steering.is_some())
            );
            assert_eq!(
                agent
                    .history
                    .as_slice()
                    .iter()
                    .flat_map(|message| &message.content)
                    .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
                    .count(),
                3
            );
            assert!(agent.tool_context().steering_observations.is_none());
        });
    }

    #[test_case(None, StopReason::EndTurn, true; "valid_report_missing_prose")]
    #[test_case(None, StopReason::MaxTokens, true; "valid_report_truncated_empty_tail")]
    #[test_case(Some(1), StopReason::EndTurn, false; "turn_limit_wins")]
    #[test_case(Some(1), StopReason::MaxTokens, false; "turn_limit_preempts_truncation_and_report")]
    fn ready_report_only_relaxes_the_prose_contract(
        max_turns: Option<u32>,
        stop: StopReason,
        usable: bool,
    ) {
        smol::block_on(async {
            let mut response = empty_response();
            response.stop_reason = Some(stop);
            let mut history = History::default();
            let (agent, _events) = make_agent(MockProvider::new(vec![response]), &mut history);
            let mut agent = agent.with_report_ready(Arc::new(AtomicBool::new(true)));
            Arc::make_mut(&mut agent.config.steering).max_recoveries = Some(0);
            agent.config.max_turns = max_turns;
            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                if usable {
                    DoneReason::EndTurn
                } else {
                    DoneReason::MaxTurns
                }
            );
            assert_eq!(agent.response_text(), None);
        });
    }

    #[test_case(empty_response(); "empty")]
    #[test_case(text_response(StopReason::MaxTokens); "truncated")]
    fn queued_input_preempts_exhausted_recovery(response: StreamResponse) {
        smol::block_on(async {
            let mut history = History::default();
            let (agent, _events) = make_agent(
                MockProvider::new(vec![response, text_response(StopReason::EndTurn)]),
                &mut history,
            );
            let source = MockInterruptSource::new(vec![ExtractedCommand::Interrupt(
                default_input(),
                0,
                QueueItemId::new(),
            )]);
            let mut agent = agent.with_interrupt_source(source);
            Arc::make_mut(&mut agent.config.steering).max_recoveries = Some(0);
            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::EndTurn
            );
            assert_eq!(agent.num_turns, 2);
            assert!(
                !agent
                    .history
                    .as_slice()
                    .iter()
                    .any(|message| message.steering.is_some())
            );
        });
    }

    struct CancelAtBoundary(Mutex<Option<CancelTrigger>>);

    impl InterruptSource for CancelAtBoundary {
        fn poll(&self) -> Option<ExtractedCommand> {
            if let Some(trigger) = self.0.lock().unwrap().take() {
                trigger.cancel();
            }
            None
        }
    }

    #[test_case(false, StopReason::EndTurn; "empty_contract")]
    #[test_case(true, StopReason::EndTurn; "ready_report")]
    #[test_case(false, StopReason::MaxTokens; "truncation")]
    #[test_case(true, StopReason::MaxTokens; "truncated_ready_report")]
    fn cancellation_at_response_boundary_preempts_contracts(ready: bool, stop: StopReason) {
        smol::block_on(async {
            let (trigger, cancel) = CancelToken::new();
            let mut history = History::default();
            let mut response = empty_response();
            response.stop_reason = Some(stop);
            let (agent, _events) = make_agent(MockProvider::new(vec![response]), &mut history);
            let mut agent = agent
                .with_cancel(cancel)
                .with_report_ready(Arc::new(AtomicBool::new(ready)))
                .with_interrupt_source(Arc::new(CancelAtBoundary(Mutex::new(Some(trigger)))));
            Arc::make_mut(&mut agent.config.steering).max_recoveries = Some(0);
            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::Cancelled
            );
            assert!(
                !agent
                    .history
                    .as_slice()
                    .iter()
                    .any(|message| message.steering.is_some())
            );
        });
    }

    #[test_case(false; "correction_run")]
    #[test_case(true; "external_invocation")]
    fn rebuilt_agents_only_refill_on_external_invocations(external: bool) {
        smol::block_on(async {
            let policy = SteeringConfig {
                max_recoveries: Some(2),
                ..Default::default()
            }
            .resolve(&default_model().spec());
            let shared = Arc::new(Mutex::new(Steering::new(policy)));
            let mut history = History::default();
            let (agent, _events) = make_agent(
                MockProvider::new(vec![empty_response(), text_response(StopReason::EndTurn)]),
                &mut history,
            );
            agent
                .with_steering(Arc::clone(&shared))
                .run(default_input())
                .await
                .unwrap();
            let correction = steering::lock(&shared)
                .report_correction(false)
                .unwrap()
                .unwrap();
            let responses = if external {
                vec![empty_response(), text_response(StopReason::EndTurn)]
            } else {
                vec![empty_response()]
            };
            let (agent, _events) = make_agent(MockProvider::new(responses), &mut history);
            let mut agent = if external {
                agent
            } else {
                agent.with_steering(shared)
            };
            let input = if external {
                default_input()
            } else {
                AgentInput {
                    message: String::new(),
                    preamble: vec![correction],
                    ..default_input()
                }
            };
            let result = agent.run(input).await;
            if external {
                assert_eq!(result.unwrap(), DoneReason::EndTurn);
            } else {
                assert!(
                    matches!(result, Err(AgentError::SteeringExhausted { rule }) if rule == STEERING_EMPTY)
                );
            }
        });
    }

    #[test_case(false; "retains_latest_text")]
    #[test_case(true; "empty_latest_clears_text")]
    fn response_text_survives_compaction_but_not_an_empty_latest_response(empty_latest: bool) {
        smol::block_on(async {
            let mut history = History::new(vec![Message::user(GO.into()); 10]);
            let mut responses = vec![
                text_response(StopReason::EndTurn),
                text_response(StopReason::EndTurn),
            ];
            if empty_latest {
                responses.push(empty_response());
            }
            let (mut agent, _events) = make_agent(MockProvider::new(responses), &mut history);
            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::EndTurn
            );
            assert_eq!(agent.response_text(), Some(VISIBLE_RESPONSE));
            agent.do_compact().await.unwrap();
            assert_eq!(agent.response_text(), Some(VISIBLE_RESPONSE));
            if empty_latest {
                Arc::make_mut(&mut agent.config.steering)
                    .rules
                    .empty_response
                    .enabled = Some(false);
                agent.run(default_input()).await.unwrap();
                assert_eq!(agent.response_text(), None);
            }
        });
    }

    #[test_case(0, false, false; "zero_combined_budget")]
    #[test_case(1, false, false; "combined_exhaustion")]
    #[test_case(32, false, false; "truncation_exhaustion")]
    #[test_case(32, true, false; "last_retry_succeeds")]
    #[test_case(1, false, true; "spent_combined_allowance_survives_compaction")]
    #[test_case(32, false, true; "spent_truncation_allowance_survives_compaction")]
    #[test_case(32, true, true; "fragments_survive_mid_chain_compaction")]
    fn truncation_compaction_retains_fragments_and_budgets(
        budget: u32,
        succeeds: bool,
        after_first: bool,
    ) {
        smol::block_on(async {
            let attempts = budget.min(TRUNCATION_ATTEMPTS);
            let mut responses = Vec::new();
            for attempt in 0..=attempts {
                let mut response = text_response(if succeeds && attempt == attempts {
                    StopReason::EndTurn
                } else {
                    StopReason::MaxTokens
                });
                response.usage.output = OUTPUT_TOKENS;
                let compact = attempt == u32::from(after_first);
                if compact {
                    response.usage.input = LARGE_CONTEXT;
                }
                responses.push(response);
                if compact {
                    responses.push(text_response(StopReason::EndTurn));
                }
            }
            let mut history = History::new(vec![Message::user(GO.into()); 10]);
            let (mut agent, events) = make_agent(MockProvider::new(responses), &mut history);
            agent.auto_compact = true;
            agent.model = Arc::new(small_context_model(200_000, 8_192));
            agent.tools = serde_json::json!([{"name": TEST_TOOL}]);
            let config = Arc::make_mut(&mut agent.config.steering);
            config.max_recoveries = Some(budget);
            config.rules.truncation.prompt = Some(STEERING_CUSTOM.into());
            config.rules.no_tool_use.after_responses = Some(1);
            let result = agent.run(default_input()).await;
            if succeeds {
                assert_eq!(result.unwrap(), DoneReason::EndTurn);
            } else {
                assert!(
                    matches!(result, Err(AgentError::SteeringExhausted { rule }) if rule == STEERING_TRUNCATION)
                );
            }
            assert_eq!(agent.num_turns, attempts + 1);
            assert_eq!(agent.usage().output, (attempts + 1) * OUTPUT_TOKENS);
            assert_eq!(
                agent.response_text(),
                Some(VISIBLE_RESPONSE.repeat((attempts + 1) as usize).as_str())
            );
            let messages: Vec<_> = agent
                .history
                .as_slice()
                .iter()
                .filter(|message| message.steering.is_some())
                .collect();
            assert_eq!(messages.len(), attempts as usize);
            for message in messages {
                let origin = message.steering.as_ref().unwrap();
                assert_eq!(origin.rule, STEERING_TRUNCATION);
                assert_eq!(origin.kind, SteeringKind::Recovery);
                assert!(
                    message
                        .first_text_content()
                        .unwrap()
                        .ends_with(STEERING_CUSTOM)
                );
            }
            assert!(has_event(&drain_events(&events), |event| matches!(
                event,
                AgentEvent::CompactionDone
            )));
        });
    }

    #[test_case(false, false; "automatic_report_correction")]
    #[test_case(true, false; "external_prompt")]
    #[test_case(true, true; "external_resume")]
    fn truncation_allowance_survives_report_corrections_but_not_external_invocations(
        external: bool,
        resume: bool,
    ) {
        smol::block_on(async {
            let mut history = History::default();
            let mut responses: Vec<_> = (0..TRUNCATION_ATTEMPTS)
                .map(|_| text_response(StopReason::MaxTokens))
                .collect();
            responses.push(text_response(StopReason::EndTurn));
            let (mut first, _events) = make_agent(MockProvider::new(responses), &mut history);
            assert_eq!(
                first.run(default_input()).await.unwrap(),
                DoneReason::EndTurn
            );
            let shared = Arc::clone(&first.steering);
            drop(first);
            let correction = steering::lock(&shared)
                .report_correction(false)
                .unwrap()
                .unwrap();
            let mut responses = vec![text_response(StopReason::MaxTokens)];
            if external {
                responses.push(text_response(StopReason::EndTurn));
            }
            let (agent, _events) = make_agent(MockProvider::new(responses), &mut history);
            let mut agent = if external {
                agent
            } else {
                agent.with_steering(shared)
            };
            let input = if resume {
                resume_input()
            } else if external {
                default_input()
            } else {
                AgentInput {
                    message: String::new(),
                    preamble: vec![correction],
                    ..default_input()
                }
            };
            let result = agent.run(input).await;
            if external {
                assert_eq!(result.unwrap(), DoneReason::EndTurn);
                assert_eq!(agent.num_turns, 2);
            } else {
                assert!(
                    matches!(result, Err(AgentError::SteeringExhausted { rule }) if rule == STEERING_TRUNCATION)
                );
                assert_eq!(agent.num_turns, 1);
                assert_eq!(agent.response_text(), Some(VISIBLE_RESPONSE));
            }
        });
    }

    #[test_case(false; "new_prompt")]
    #[test_case(true; "resume")]
    fn reused_agent_refills_truncation_on_external_run(resume: bool) {
        smol::block_on(async {
            let mut responses: Vec<_> = (0..=TRUNCATION_ATTEMPTS)
                .map(|_| text_response(StopReason::MaxTokens))
                .collect();
            responses.extend([
                text_response(StopReason::MaxTokens),
                text_response(StopReason::EndTurn),
            ]);
            let mut history = History::default();
            let (mut agent, _events) = make_agent(MockProvider::new(responses), &mut history);
            assert!(
                matches!(agent.run(default_input()).await, Err(AgentError::SteeringExhausted { rule }) if rule == STEERING_TRUNCATION)
            );
            assert_eq!(
                agent
                    .run(if resume {
                        resume_input()
                    } else {
                        default_input()
                    })
                    .await
                    .unwrap(),
                DoneReason::EndTurn
            );
            assert_eq!(
                agent.response_text(),
                Some(VISIBLE_RESPONSE.repeat(2).as_str())
            );
            assert_eq!(steering::lock(&agent.steering).responses(), 2);
        });
    }

    #[test_case(0; "already_exhausted")]
    #[test_case(1; "one_recovery")]
    fn empty_recovery_compacts_before_charging_without_refill(budget: u32) {
        smol::block_on(async {
            let mut response = empty_response();
            response.usage.input = LARGE_CONTEXT;
            let mut responses = vec![response, text_response(StopReason::EndTurn)];
            if budget > 0 {
                responses.push(empty_response());
            }
            let mut history = History::new(vec![Message::user(GO.into()); 10]);
            let (mut agent, events) = make_agent(MockProvider::new(responses), &mut history);
            agent.auto_compact = true;
            agent.model = Arc::new(small_context_model(200_000, 8_192));
            Arc::make_mut(&mut agent.config.steering).max_recoveries = Some(budget);
            Arc::make_mut(&mut agent.config.steering)
                .rules
                .empty_response
                .prompt = Some(STEERING_CUSTOM.into());
            assert!(
                matches!(agent.run(default_input()).await, Err(AgentError::SteeringExhausted { rule }) if rule == STEERING_EMPTY)
            );
            assert_eq!(agent.num_turns, budget + 1);
            let events = drain_events(&events);
            assert!(has_event(&events, |event| matches!(
                event,
                AgentEvent::CompactionDone
            )));
            assert_eq!(events.iter().filter(|event| matches!(&event.event, AgentEvent::Injected { text } if text == STEERING_CUSTOM)).count(), budget as usize);
        });
    }

    /// Wiring this to `None` to make the struct literal compile would
    /// silently reintroduce the bug the field exists to fix.
    #[test]
    fn tool_context_carries_the_session() {
        let mut history = History::new(Vec::new());
        let (mut agent, _event_rx) = make_agent(MockProvider::new(Vec::new()), &mut history);
        assert_eq!(agent.tool_context().session_id, None);

        let session: SessionRef = "CNK1hV6GWoysH3KQMm5wu".parse().expect("valid session id");
        agent.session_id = Some(session.clone());
        assert_eq!(agent.tool_context().session_id, Some(session));
    }
}
