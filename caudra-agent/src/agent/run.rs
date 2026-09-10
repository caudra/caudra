use std::borrow::Cow;
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::{Instrument, debug, error, info, info_span, warn};

use caudra_providers::model_registry::Binding;
use caudra_providers::provider::Provider;
use caudra_providers::{
    Billing, ContentBlock, EMPTY_RESPONSE_MARKER, Message, Model, ModelPurpose, RequestOptions,
    Role, StopReason, StreamResponse, TokenUsage, estimate_tokens_cached,
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
use super::streaming::{StreamError, stream_with_retry};
use super::title;
use super::tool_dispatch::{self, RecentCalls};
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
use crate::{
    AgentConfig, AgentError, AgentEvent, AgentInput, AgentMode, DoneReason, EventSender,
    ExtractedCommand, InterruptSource, Mention, QueueConsumedItem, SessionMailbox,
    SubagentHistoryStore, TurnCompleteEvent,
};
use caudra_config::{ModelPolicy, ToolOutputLines};
use caudra_storage::id::SessionRef;
use caudra_storage::usage_ledger::LedgerPurpose;

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
const NUDGE_PROMPT: &str = "You just executed tool calls but returned an empty response. Please process the tool results above and continue with the task. Always end your turn with a text response.";
/// Nothing is pending, so this asks for the closing response the system
/// prompt requires rather than for tool results to be processed.
const IDLE_NUDGE_PROMPT: &str = "You ended your turn without a response. Continue the task, and always end your turn with a text response summarizing what you did.";
/// A model that stalls once often stalls again on the retry, so it gets
/// plenty of chances before the turn ends empty handed.
const MAX_NUDGES: u32 = 20;
/// With no tool results to resume from there is nothing to salvage, so a
/// model that answers nothing twice is finished rather than wedged.
const MAX_IDLE_NUDGES: u32 = 2;
/// Counted over non-padding messages.
const RECENT_TOOL_WINDOW: usize = 5;
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

#[derive(Clone)]
pub struct AgentParams {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
    pub config: AgentConfig,
    pub tool_output_lines: ToolOutputLines,
    pub permissions: Arc<PermissionManager>,
    pub session_id: Option<SessionRef>,
    pub root_tool_use_id: Option<String>,
    pub mailbox: Option<SessionMailbox>,
    pub context_publisher: Option<ContextPublisher>,
    pub timeouts: caudra_providers::Timeouts,
    pub file_tracker: Arc<FileReadTracker>,
    pub path_locks: Arc<PathLocks>,
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
    context_size: u32,
    num_turns: u32,
    recent_calls: RecentCalls,
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
    /// Numbers each turn so every log line inside one can be correlated.
    turn_id: u64,
    root_tool_use_id: Option<String>,
    mailbox: Option<SessionMailbox>,
    context_publisher: Option<ContextPublisher>,
    timeouts: caudra_providers::Timeouts,
    file_tracker: Arc<FileReadTracker>,
    path_locks: Arc<PathLocks>,
    prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    prompt_profiles: Arc<crate::prompt::profile::PromptProfileCatalog>,
    default_task_prompt_profile_name: Arc<str>,
    active_prompt_profile_name: Option<Arc<str>>,
    subagent_cancels: Arc<crate::cancel::CancelMap<String>>,
    subagent_history: SubagentHistoryStore,
    registry: Arc<crate::tools::ToolRegistry>,
    audience: ToolAudience,
    tool_filter: crate::tools::ToolFilter,
    workflow: bool,
    local_tools: LocalTools,
    model_policy: Arc<ModelPolicy>,
    goal: GoalHandle,
    goal_evaluator: Option<ResolvedEvaluator>,
    goal_blocks: u32,
    wait_for_background: bool,
}

impl<'h> Agent<'h> {
    pub fn new(params: AgentParams, run: AgentRunParams<'h>) -> Self {
        let mut model = params.model;
        params.provider.adjust_model(&mut model);
        // Seeded before the history moves in: a restored session keeps the
        // tools it already searched for rather than hunting them again.
        let deferral = DeferralSession::new(
            run.deferred,
            crate::tools::deferral::loaded_tool_names(run.history.as_slice()),
        );
        Self {
            provider: params.provider,
            model: Arc::new(model),
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
            context_size: 0,
            num_turns: 0,
            recent_calls: RecentCalls::new(),
            auto_compact: compaction::auto_compact_enabled(),
            loaded_instructions: LoadedInstructions::new(),
            rollback_len: 0,
            mcp: None,
            reauth_attempts: 0,
            opts: RequestOptions::default(),
            session_id: params.session_id,
            turn_id: 0,
            root_tool_use_id: params.root_tool_use_id,
            mailbox: params.mailbox,
            context_publisher: params.context_publisher,
            file_tracker: params.file_tracker,
            path_locks: params.path_locks,
            prompt_slots: params.prompt_slots,
            prompt_profiles: params.prompt_profiles,
            default_task_prompt_profile_name: params.default_task_prompt_profile_name,
            active_prompt_profile_name: params.active_prompt_profile_name,
            subagent_cancels: params.subagent_cancels,
            subagent_history: params.subagent_history,
            registry: params.registry,
            audience: params.audience,
            tool_filter: params.tool_filter,
            workflow: false,
            local_tools: LocalTools::default(),
            model_policy: params.model_policy,
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
        self.workflow = latest.workflow;
        self.opts = RequestOptions {
            thinking: latest.thinking.clone(),
            fast: latest.fast,
        };

        let mut preamble = standing;
        for input in &mut inputs {
            preamble.append(&mut input.preamble);
            preamble.append(&mut self.mention_preamble(&input.mentions).await);
        }
        self.push_input_context(preamble);

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
            },
        )
        .await
    }

    fn push_input_context(&mut self, preamble: Vec<Message>) {
        for message in preamble {
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
        loop {
            if let Some(max) = self.config.max_turns
                && self.num_turns >= max
            {
                if let Some(goal) = self.goal.snapshot() {
                    self.event_tx.send(AgentEvent::GoalTurnLimit {
                        evaluations: goal.evaluations,
                    })?;
                }
                return Ok(DoneReason::MaxTurns);
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
        let stream_result = {
            let (tools, mcp) = self.request_tools();
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
            )
            .await
        };
        let mut response = match stream_result {
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
                    content.push(ContentBlock::Text {
                        text: format!("{streamed}\n\n{CANCELLED_TEXT_NOTE}"),
                    });
                }
                if !content.is_empty() {
                    self.history.push(Message {
                        role: Role::Assistant,
                        content,
                        reasoning_source: Some(caudra_providers::ReasoningSource::new(
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

        self.emit_turn_complete(&response)?;
        let usage = response.usage;
        self.total_usage += usage;
        self.goal.record_usage(
            usage,
            self.model
                .billed_cost(&usage, self.opts.clamped(&self.model).fast),
            self.model.billing,
        );
        self.context_size = usage.total_input();

        if has_tools {
            let history_len_before = self.history.len();
            self.process_tool_calls(response).await?;
            self.context_size = self.context_size.saturating_add(estimate_message_tokens(
                &self.history.as_slice()[history_len_before..],
            ));
        } else {
            let has_reasoning = response.message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
                )
            });
            if response.message.first_text_content().is_some() {
                self.push_assistant_message(response.message);
            } else if has_reasoning {
                response.message.content.push(ContentBlock::Text {
                    text: EMPTY_RESPONSE_MARKER.into(),
                });
                self.push_assistant_message(response.message);
                if stop_reason != Some(StopReason::MaxTokens) && self.recover_stalled_turn(false)? {
                    self.publish_prepared_context();
                    return Ok(TurnOutcome::Continue);
                }
            } else if self.recover_stalled_turn(true)? {
                self.publish_prepared_context();
                return Ok(TurnOutcome::Continue);
            }

            if stop_reason == Some(StopReason::MaxTokens)
                && self.num_turns <= self.config.max_continuation_turns
            {
                warn!(
                    self.num_turns,
                    "response truncated (max_tokens), re-prompting"
                );
                return Ok(TurnOutcome::Continue);
            }
        }

        if self.handle_queued_command().await? {
            self.publish_prepared_context();
            return Ok(TurnOutcome::Continue);
        }
        if self.try_auto_compact().await? {
            return Ok(TurnOutcome::Continue);
        }

        if has_tools {
            Ok(TurnOutcome::Continue)
        } else {
            let outcome = self.goal_completion(stop_reason.into()).await?;
            self.publish_prepared_context();
            Ok(outcome)
        }
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
                self.push_input_context(Vec::new());
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
        let Some(rx) = &self.user_response_rx else {
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
                    self.provider.adjust_model(Arc::make_mut(&mut self.model));
                    self.event_tx.send(AgentEvent::AuthRestored)?;
                    return Ok(TurnOutcome::Continue);
                }
                Wake::Response(Err(_)) | Wake::Cancelled => return Err(AgentError::Cancelled),
                Wake::Poll => match self.provider.reload_auth_if_changed().await {
                    Ok(true) => {
                        self.provider.adjust_model(Arc::make_mut(&mut self.model));
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
                context_size: Some(response.usage.context_tokens()),
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

    /// Returns true when the model was nudged to try again. A wholly empty
    /// response needs assistant padding; retained reasoning already occupies
    /// that turn and must not be followed by another assistant message.
    fn recover_stalled_turn(&mut self, pad_empty_response: bool) -> Result<bool, AgentError> {
        let after_tools = self.history.has_recent_tool_results(RECENT_TOOL_WINDOW);
        let nudges = self.history.recent_nudges();
        if pad_empty_response {
            self.push_assistant_message(Message::empty_marker());
        }
        let nudge_limit = if after_tools {
            MAX_NUDGES
        } else {
            MAX_IDLE_NUDGES
        };
        if nudges >= nudge_limit {
            return Ok(false);
        }

        warn!(
            nudges = nudges + 1,
            after_tools, "turn ended without a response, nudging model to continue"
        );
        self.event_tx.send(AgentEvent::Nudge)?;
        self.push_injected(Message::synthetic(
            if after_tools {
                NUDGE_PROMPT
            } else {
                IDLE_NUDGE_PROMPT
            }
            .into(),
        ));
        Ok(true)
    }

    async fn process_tool_calls(&mut self, response: StreamResponse) -> Result<(), AgentError> {
        let tool_uses = response
            .message
            .tool_uses()
            .map(|(id, name, input)| (id.to_owned(), name.to_owned(), input.clone()))
            .collect();
        let ctx = ToolContext {
            tool_name_aliases: response.tool_name_aliases.clone(),
            ..self.tool_context()
        };
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
        if result.is_ok() {
            self.publish_prepared_context();
        }
        result
    }

    fn tool_context(&self) -> ToolContext {
        ToolContext {
            provider: Arc::clone(&self.provider),
            model: Arc::clone(&self.model),
            event_tx: self.event_tx.clone(),
            mode: self.mode.clone(),
            session_id: self.session_id.clone(),
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
            prompt_slots: Arc::clone(&self.prompt_slots),
            prompt_profiles: Arc::clone(&self.prompt_profiles),
            default_task_prompt_profile_name: Arc::clone(&self.default_task_prompt_profile_name),
            opts: self.opts.clone(),
            subagent_cancels: Arc::clone(&self.subagent_cancels),
            subagent_history: self.subagent_history.clone(),
            registry: Arc::clone(&self.registry),
            workflow: self.workflow,
            audience: self.audience,
            tool_filter: self.tool_filter.clone(),
            local_tools: Arc::clone(&self.local_tools),
            tool_name_aliases: None,
            live_sink: None,
            model_policy: Arc::clone(&self.model_policy),
        }
    }

    async fn try_auto_compact(&mut self) -> Result<bool, AgentError> {
        if !self.auto_compact
            || !compaction::is_overflow(
                &TokenUsage {
                    input: self.context_size,
                    ..Default::default()
                },
                &self.model,
                self.config.compaction_buffer,
            )
        {
            return Ok(false);
        }
        info!(context_size = self.context_size, "auto-compacting");
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
        let usage = compaction::compact_history(
            &*compact_provider,
            &compact_model,
            self.history,
            &self.event_tx,
            &self.cancel,
            &self.retry_now,
            &self.config,
        )
        .await?;
        let cost = compact_model.billed_cost(&usage, false);
        self.total_usage += usage;
        self.goal.record_usage(usage, cost, compact_model.billing);
        self.rollback_len = self.history.len();
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
            AgentMode::Plan(_) => Some(Self::Plan),
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
/// both modes so that toggling does not re-cache the conversation. Compaction
/// dropping the announcement is self-healing, since the next turn then finds no
/// match and announces again.
fn mode_switch_notice(history: &[Message], next: &AgentMode) -> Option<Message> {
    let announced = AnnouncedMode::of(next)?;
    if announced == last_announced_mode(history) {
        return None;
    }
    let text = match next.plan_path() {
        Some(plan_path) => Vars::new()
            .set("{plan_path}", plan_path.display().to_string())
            .apply(crate::prompt::PLAN_PROMPT)
            .into_owned(),
        None => crate::prompt::BUILD_PROMPT.to_owned(),
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
                add_estimated_tokens(&mut tokens, signature);
            }
            if let Some(responses) = responses {
                add_estimated_tokens(&mut tokens, RESPONSES_REASONING_FRAMING);
                add_estimated_tokens(&mut tokens, &responses.item_id);
                if let Some(encrypted_content) = &responses.encrypted_content {
                    add_estimated_tokens(&mut tokens, RESPONSES_ENCRYPTED_CONTENT_FRAMING);
                    add_estimated_tokens(&mut tokens, encrypted_content);
                }
            }
            tokens
        }
        ContentBlock::RedactedThinking { data } => {
            framed_tokens(REDACTED_THINKING_BLOCK_FRAMING, [data.as_str()])
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
                add_estimated_tokens(&mut tokens, thought_signature);
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

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use caudra_providers::provider::{BoxFuture, Provider};
    use caudra_providers::{
        ContentBlock, Message, Model, ProviderEvent, RequestOptions, Role, StopReason,
        StreamResponse, TokenUsage,
    };
    use serde_json::Value;
    use test_case::test_case;

    use super::*;
    use crate::context::{ContextKey, ContextStore};
    use crate::mcp::tool_names;
    use crate::permissions::PermissionManager;
    use crate::{Envelope, QueueItemId};

    const AUTH_ERROR_STATUS: u16 = 401;
    const AUTH_ERROR_MESSAGE: &str = "expired";
    const EXPECTED_AUTH_ERROR: &str = "expected terminal authentication error";
    const ADJUSTED_CONTEXT_WINDOW: u32 = 1;
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
    const INSTRUCTIONS_CHANGED: &str =
        "<system-reminder>\n# Instructions changed\n\n+ be brief\n</system-reminder>";
    const MENTION_BODY: &str = "<file path=\"a.rs\">fn main() {}</file>";
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
        let agent = Agent::new(
            AgentParams {
                provider: Arc::new(provider),
                model: default_model(),
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
                root_tool_use_id: None,
                mailbox: None,
                context_publisher: None,
                timeouts: caudra_providers::Timeouts::default(),
                file_tracker: FileReadTracker::fresh(),
                path_locks: PathLocks::fresh(),
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
            workflow: false,
            prompt: None,
            resume: false,
        }
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
                agent.process_tool_calls(tool_use_response(
                    BLOCKING_TOOL_NAME,
                    serde_json::json!({}),
                )),
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

    /// The case an in-memory previous mode cannot catch: `Agent` is rebuilt per
    /// run, so only the transcript still knows the session was planning.
    #[test]
    fn a_restored_plan_transcript_switched_to_build_announces_build() {
        let history = [
            plan_announcement(),
            Message::user("draft the plan".into()),
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

            assert_eq!(history.as_slice()[0].user_text(), Some(ENVIRONMENT));
            assert!(
                history.as_slice()[1]
                    .user_text()
                    .is_some_and(|text| text.contains(crate::prompt::PLAN_MODE_MARKER))
            );
            assert_eq!(history.as_slice()[2].user_text(), Some("hello"));
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

    /// Instruction drift patches the system prompt, so it has to land before
    /// anything the model might act on under the stale text.
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
            assert_eq!(texts[0], ENVIRONMENT);
            assert_eq!(texts[1], INSTRUCTIONS_CHANGED);
            assert!(texts[2].contains(crate::prompt::PLAN_MODE_MARKER));
            assert_eq!(texts[3], "hello");
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
    fn run_announces_plan_mode_ahead_of_the_user_message() {
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

            assert!(
                history.as_slice()[0]
                    .user_text()
                    .is_some_and(|text| text.contains(crate::prompt::PLAN_MODE_MARKER))
            );
            assert_eq!(history.as_slice()[1].user_text(), Some("hello"));
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
    #[test_case(&[StopReason::MaxTokens; 4], None, 4, DoneReason::MaxTokens ; "max_tokens_gives_up_after_limit")]
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

    #[test_case(true,  170_000, true  ; "enabled_and_over_threshold")]
    #[test_case(true,  150_000, false ; "enabled_but_below_threshold")]
    #[test_case(false, 170_000, false ; "disabled_even_over_threshold")]
    fn try_auto_compact_behavior(enabled: bool, context_size: u32, expected: bool) {
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
            agent.context_size = context_size;
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
            let _ = agent.run(default_input()).await;
            drop(agent);
            let events = drain_events(&event_rx);

            let nudges = events
                .iter()
                .filter(|e| matches!(e.event, AgentEvent::Nudge))
                .count();
            assert_eq!(nudges, expected_nudges);

            let done = events
                .iter()
                .find_map(|e| match &e.event {
                    AgentEvent::Done { num_turns, .. } => Some(*num_turns),
                    _ => None,
                })
                .expect("expected Done event");
            assert_eq!(done, expected_turns);

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
