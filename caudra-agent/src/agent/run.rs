use std::borrow::Cow;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::{debug, error, info, warn};

use caudra_providers::model_registry::CompactionTarget;
use caudra_providers::provider::Provider;
use caudra_providers::{
    ContentBlock, EMPTY_RESPONSE_MARKER, Message, Model, RequestOptions, Role, StopReason,
    StreamResponse, TokenUsage, estimate_tokens_cached,
};

use super::compaction;
use super::goal::{
    Evaluator, GOAL_BLOCK_CAP, GoalApply, GoalHandle, GoalStatus, ResolvedEvaluator,
    continuation_message, is_unrecoverable, resolve_evaluator,
};
use super::history::{History, repair_tool_pairs, sanitize_cancelled_history};
use super::instructions::LoadedInstructions;
use super::provider_projection;
use super::streaming::{StreamError, stream_with_retry};
use super::title;
use super::tool_dispatch::{self, RecentCalls};
use crate::cancel::{CancelMap, CancelToken};
use crate::mcp::McpSession;
use crate::permissions::PermissionManager;
use crate::tools::{Deadline, FileReadTracker, LocalTools, PathLocks, ToolAudience, ToolContext};
use crate::{
    AgentConfig, AgentError, AgentEvent, AgentInput, AgentMode, DoneReason, EventSender,
    ExtractedCommand, InterruptSource, QueueConsumedItem, SessionMailbox, SubagentHistoryStore,
    TurnCompleteEvent,
};
use caudra_config::{ModelPolicy, ToolOutputLines};
use caudra_storage::id::SessionRef;
use caudra_storage::usage_ledger::LedgerPurpose;

const MAX_REAUTH_ATTEMPTS: u32 = 2;
const AUTH_RELOAD_POLL_MIN_MS: u64 = 250;
const AUTH_RELOAD_POLL_MAX_MS: u64 = 1_000;
const NUDGE_PROMPT: &str = "You just executed tool calls but returned an empty response. Please process the tool results above and continue with the task.";
/// A model that stalls once often stalls again on the retry, so it gets
/// plenty of chances before the turn ends empty handed.
const MAX_NUDGES: u32 = 20;
/// Counted over non-padding messages.
const RECENT_TOOL_WINDOW: usize = 5;
/// Without this note a cancelled reply replays in history as a finished
/// turn, and a model resuming its own cut-off text can wedge the session
/// (seen with llama.cpp stuck on an unterminated tool call).
const CANCELLED_TEXT_NOTE: &str = "[Response cut off by user cancel]";

pub fn resolve_compaction_model(
    provider: &Arc<dyn Provider>,
    model: &Model,
    timeouts: caudra_providers::Timeouts,
    model_policy: &ModelPolicy,
) -> Result<(Arc<dyn Provider>, Model), AgentError> {
    let CompactionTarget::Model(spec) = caudra_providers::model_registry::compaction_target()
    else {
        return Ok((Arc::clone(provider), model.clone()));
    };
    if !model_policy.allows(&spec) {
        return Err(AgentError::Config {
            message: format!("compaction model '{spec}' is not allowed by provider model policy"),
        });
    }
    let mut compact_model = Model::from_spec(&spec).map_err(|error| AgentError::Config {
        message: format!("cannot resolve compaction model '{spec}': {error}"),
    })?;
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
    pub timeouts: caudra_providers::Timeouts,
    pub file_tracker: Arc<FileReadTracker>,
    pub path_locks: Arc<PathLocks>,
    pub prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    pub prompt_profiles: Arc<crate::prompt::profile::PromptProfileCatalog>,
    pub system_prompt_profile_name: Arc<str>,
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
    pub event_tx: EventSender,
    pub tools: Value,
}

pub struct Agent<'h> {
    provider: Arc<dyn Provider>,
    model: Arc<Model>,
    history: &'h mut History,
    system: String,
    event_tx: EventSender,
    tools: Value,
    mode: AgentMode,
    user_response_rx: Option<Arc<async_lock::Mutex<flume::Receiver<String>>>>,
    interrupt_source: Option<Arc<dyn InterruptSource>>,
    cancel: CancelToken,
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
    root_tool_use_id: Option<String>,
    mailbox: Option<SessionMailbox>,
    timeouts: caudra_providers::Timeouts,
    file_tracker: Arc<FileReadTracker>,
    path_locks: Arc<PathLocks>,
    prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    prompt_profiles: Arc<crate::prompt::profile::PromptProfileCatalog>,
    system_prompt_profile_name: Arc<str>,
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
        Self {
            provider: params.provider,
            model: Arc::new(model),
            config: params.config,
            tool_output_lines: params.tool_output_lines,
            permissions: params.permissions,
            timeouts: params.timeouts,
            history: run.history,
            system: run.system,
            event_tx: run.event_tx,
            tools: run.tools,
            mode: AgentMode::default(),
            user_response_rx: None,
            interrupt_source: None,
            cancel: CancelToken::none(),
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
            root_tool_use_id: params.root_tool_use_id,
            mailbox: params.mailbox,
            file_tracker: params.file_tracker,
            path_locks: params.path_locks,
            prompt_slots: params.prompt_slots,
            prompt_profiles: params.prompt_profiles,
            system_prompt_profile_name: params.system_prompt_profile_name,
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

    async fn run_inputs(
        &mut self,
        inputs: Vec<AgentInput>,
        queued: bool,
    ) -> Result<DoneReason, AgentError> {
        self.goal_blocks = 0;
        self.rollback_len = self.history.len();
        let message = self.push_user_inputs(inputs, queued);

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
                sanitize_cancelled_history(self.history, self.rollback_len);
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
                    model: resolved.model.id.clone(),
                    provider: resolved.model.provider.to_string(),
                });
            }
        })
        .detach();
    }

    fn push_user_inputs(&mut self, mut inputs: Vec<AgentInput>, queued: bool) -> String {
        let Some(latest) = inputs.last() else {
            return String::new();
        };
        self.mode = latest.mode.clone();
        self.workflow = latest.workflow;
        self.opts = RequestOptions {
            thinking: latest.thinking.clone(),
            fast: latest.fast,
        };

        let mut preamble = Vec::new();
        for input in &mut inputs {
            preamble.append(&mut input.preamble);
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

    fn push_input_context(&mut self, preamble: Vec<Message>) {
        for message in preamble {
            self.history.push(message);
        }
        if let Some(mailbox) = &self.mailbox {
            for message in mailbox.drain() {
                self.history.push(message);
            }
        }
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

    /// `self.tools` holds base tools only; the MCP part is recomputed here
    /// every turn so `tool_search` loads and late-connecting servers take
    /// effect on the next request.
    fn request_tools(&self) -> Cow<'_, Value> {
        match &self.mcp {
            Some(mcp) => {
                let mut tools = self.tools.clone();
                mcp.extend_tools(&mut tools);
                Cow::Owned(tools)
            }
            None => Cow::Borrowed(&self.tools),
        }
    }

    async fn turn(&mut self) -> Result<TurnOutcome, AgentError> {
        if self.cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let tools = self.request_tools();
        let provider_history = provider_projection::project_for_target(
            self.history.as_slice(),
            tools.as_ref(),
            &self.model,
            self.provider.reasoning_transport(&self.model),
        );
        let provider_history = repair_tool_pairs(provider_history);
        let mut response = match stream_with_retry(
            &*self.provider,
            &self.model,
            provider_history.as_ref(),
            &self.system,
            tools.as_ref(),
            &self.event_tx,
            &self.cancel,
            self.opts.clone(),
            self.session_id.as_ref(),
        )
        .await
        {
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
                error!(error = %e, model = %self.model.id, self.num_turns, "stream_message failed");
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
        );
        self.context_size = usage.total_input();

        if has_tools {
            let history_len_before = self.history.len();
            self.process_tool_calls(response).await?;
            self.context_size +=
                estimate_message_tokens(&self.history.as_slice()[history_len_before..]);
        } else {
            let has_reasoning = response.message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
                )
            });
            if response.message.first_text_content().is_some() {
                self.history.push(response.message);
            } else if has_reasoning {
                response.message.content.push(ContentBlock::Text {
                    text: EMPTY_RESPONSE_MARKER.into(),
                });
                self.history.push(response.message);
                if stop_reason != Some(StopReason::MaxTokens) && self.recover_stalled_turn(false)? {
                    return Ok(TurnOutcome::Continue);
                }
            } else if self.recover_stalled_turn(true)? {
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

        if self.handle_queued_command().await? || self.try_auto_compact().await? {
            return Ok(TurnOutcome::Continue);
        }

        if has_tools {
            Ok(TurnOutcome::Continue)
        } else {
            self.goal_completion(stop_reason.into()).await
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
        let target = caudra_providers::model_registry::goal_evaluator_target();
        let cached_provider = self
            .goal_evaluator
            .as_ref()
            .filter(|resolved| {
                matches!(
                    &target,
                    caudra_providers::model_registry::GoalEvaluatorTarget::Model(_)
                ) && resolved.target == target
            })
            .map(|resolved| Arc::clone(&resolved.provider));
        let evaluator = match resolve_evaluator(
            &self.provider,
            &self.model,
            target.clone(),
            self.timeouts,
            &self.model_policy,
            &self.cancel,
            cached_provider,
        )
        .await
        {
            Ok(resolved) => {
                self.goal_evaluator = matches!(
                    resolved.target,
                    caudra_providers::model_registry::GoalEvaluatorTarget::Model(_)
                )
                .then_some(resolved.clone());
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
                    model: target.to_string(),
                })?;
                if cancelled {
                    return Err(AgentError::Cancelled);
                }
                return Ok(TurnOutcome::Done(done_reason));
            }
        };
        let result = Evaluator {
            provider: &*evaluator.provider,
            model: &evaluator.model,
            history: self.history.as_slice(),
            condition: &goal.condition,
            evaluation,
            cancel: &self.cancel,
            session_id: self.session_id.as_ref(),
        }
        .run()
        .await;
        let result = match result {
            Ok(result) => result,
            Err(failure) => {
                self.total_usage += failure.usage;
                self.goal
                    .record_usage_for(goal.generation, failure.usage, failure.cost);
                let cancelled = matches!(failure.error, AgentError::Cancelled);
                let applied = self.goal.is_generation_active(goal.generation);
                self.event_tx.send(AgentEvent::GoalEvaluationFailed {
                    evaluation,
                    message: failure.error.to_string(),
                    applied,
                    usage: failure.usage,
                    cost: failure.cost,
                    model: failure.model,
                })?;
                if cancelled {
                    return Err(AgentError::Cancelled);
                }
                return Ok(TurnOutcome::Done(done_reason));
            }
        };

        self.total_usage += result.usage;
        self.goal
            .record_usage_for(goal.generation, result.usage, result.cost);
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
                if self.goal_blocks >= GOAL_BLOCK_CAP {
                    self.event_tx.send(AgentEvent::GoalLoopCap {
                        evaluations: evaluation,
                    })?;
                    return Ok(TurnOutcome::Done(done_reason));
                }
                self.goal_blocks += 1;
                self.history.push(Message::synthetic(continuation_message(
                    &goal.condition,
                    &reason,
                )));
                Ok(TurnOutcome::Continue)
            }
        }
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
            error!(error = %err, model = %self.model.id, self.num_turns, "stream_message failed");
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
        let nudges = self.history.recent_nudges();
        let nudge = nudges < MAX_NUDGES && self.history.has_recent_tool_results(RECENT_TOOL_WINDOW);
        if pad_empty_response {
            self.history.push(Message::empty_marker());
        }
        if !nudge {
            return Ok(false);
        }

        warn!(
            nudges = nudges + 1,
            "empty response after tool calls, nudging model to continue"
        );
        self.event_tx.send(AgentEvent::Nudge)?;
        self.history.push(Message::synthetic(NUDGE_PROMPT.into()));
        Ok(true)
    }

    async fn process_tool_calls(&mut self, response: StreamResponse) -> Result<(), AgentError> {
        let ctx = ToolContext {
            tool_name_aliases: response.tool_name_aliases.clone(),
            ..self.tool_context()
        };
        tool_dispatch::process_tool_calls(
            response,
            &mut self.recent_calls,
            self.mcp.as_ref(),
            self.history,
            &self.event_tx,
            &ctx,
        )
        .await
    }

    fn tool_context(&self) -> ToolContext {
        ToolContext {
            provider: Arc::clone(&self.provider),
            model: Arc::clone(&self.model),
            event_tx: self.event_tx.clone(),
            mode: self.mode.clone(),
            session_id: self.session_id.clone(),
            tool_output_store: crate::tool_output::default_store(),
            tool_use_id: None,
            root_tool_use_id: self.root_tool_use_id.clone(),
            user_response_rx: self.user_response_rx.clone(),
            loaded_instructions: self.loaded_instructions.clone(),
            cancel: self.cancel.clone(),
            mcp: self.mcp.clone(),
            deadline: Deadline::None,
            config: self.config.clone(),
            tool_output_lines: self.tool_output_lines,
            permissions: Arc::clone(&self.permissions),
            timeouts: self.timeouts,
            file_tracker: Arc::clone(&self.file_tracker),
            path_locks: Arc::clone(&self.path_locks),
            prompt_slots: Arc::clone(&self.prompt_slots),
            prompt_profiles: Arc::clone(&self.prompt_profiles),
            system_prompt_profile_name: Arc::clone(&self.system_prompt_profile_name),
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
        self.event_tx.send(AgentEvent::AutoCompacting)?;
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
            &self.config,
        )
        .await?;
        let cost = compact_model.billed_cost(&usage, false);
        self.total_usage += usage;
        self.goal.record_usage(usage, cost);
        self.rollback_len = self.history.len();
        self.event_tx.send(AgentEvent::CompactionDone)?;
        self.history
            .push(Message::synthetic(compaction::continue_message(
                &self.config,
            )));
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
                self.push_user_inputs(vec![input], true);
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
                );
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

/// Counts message content only. The system prompt and the tool schemas, a five
/// figure baseline on a full tool set, stay invisible here, so never let this
/// replace a context size the provider measured.
///
/// Counts rather than estimates from byte length. The ratio a byte heuristic
/// assumes holds for prose and breaks on everything a tool returns: dense JSON
/// costs about twice what its length suggests, so a heuristic hid the growth on
/// exactly the turns that overflow.
pub fn estimate_message_tokens(messages: &[Message]) -> u32 {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .map(block_tokens)
        .sum()
}

fn block_tokens(block: &ContentBlock) -> u32 {
    match block {
        ContentBlock::Text { text } => estimate_tokens_cached(text),
        ContentBlock::ToolResult { content, .. } => estimate_tokens_cached(content),
        ContentBlock::ToolUse { input, .. } => estimate_tokens_cached(&input.to_string()),
        ContentBlock::Thinking { thinking, .. } => estimate_tokens_cached(thinking),
        // Vision input is not free, and treating it as free let a screenshot
        // enter the window costing nothing against the compaction trigger.
        ContentBlock::Image { source } => crate::tools::image_bytes::token_estimate(source),
        ContentBlock::RedactedThinking { data } => estimate_tokens_cached(data),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
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
    use crate::mcp::tool_names;
    use crate::permissions::PermissionManager;
    use crate::{Envelope, QueueItemId};

    const AUTH_ERROR_STATUS: u16 = 401;
    const AUTH_ERROR_MESSAGE: &str = "expired";
    const EXPECTED_AUTH_ERROR: &str = "expected terminal authentication error";
    const ADJUSTED_CONTEXT_WINDOW: u32 = 1;
    const PARTIAL_RESPONSE: &str = "partial";
    const TITLE_PROMPT: &str = "add refresh token support";
    const MODEL_TITLE: &str = "Refresh token support";
    const TITLE_MUST_SURVIVE: &str = "a title is asked for at the start of a turn and answers after it, so the turn ending must not cancel it";
    /// Generous: the mock answers in microseconds, so this only bounds a
    /// regression that would otherwise hang instead of failing.
    const TITLE_EVENT_TIMEOUT: Duration = Duration::from_secs(5);

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
        responses: Mutex<Vec<StreamResponse>>,
        captured_tools: Arc<Mutex<Vec<Value>>>,
    }

    impl MockProvider {
        fn new(responses: Vec<StreamResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                captured_tools: Arc::default(),
            }
        }
    }

    impl Provider for MockProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            tools: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                self.captured_tools.lock().unwrap().push(tools.clone());
                let mut responses = self.responses.lock().unwrap();
                assert!(!responses.is_empty(), "MockProvider: no more responses");
                Ok(responses.remove(0))
            })
        }

        fn list_models(
            &self,
        ) -> BoxFuture<'_, Result<Vec<caudra_providers::ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
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
                        message: "stub".into(),
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
                timeouts: caudra_providers::Timeouts::default(),
                file_tracker: FileReadTracker::fresh(),
                path_locks: PathLocks::fresh(),
                prompt_slots: Arc::new(crate::prompt::ResolvedSlots::default()),
                prompt_profiles: Arc::new(crate::prompt::profile::PromptProfileCatalog::default()),
                system_prompt_profile_name: Arc::from(crate::prompt::profile::BUILTIN_PROFILE_NAME),
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
                event_tx: EventSender::new(raw_tx, 0),
                tools: serde_json::json!([]),
            },
        );
        (agent, event_rx)
    }

    fn default_input() -> AgentInput {
        AgentInput {
            message: "hello".into(),
            mode: AgentMode::Build,
            images: Vec::new(),
            preamble: Vec::new(),
            thinking: Default::default(),
            fast: false,
            workflow: false,
            prompt: None,
        }
    }

    fn auth_error() -> AgentError {
        AgentError::Api {
            status: AUTH_ERROR_STATUS,
            message: AUTH_ERROR_MESSAGE.into(),
        }
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
    fn goal_loop_cap_pauses_after_eight_continuations() {
        smol::block_on(async {
            let mut responses = Vec::new();
            for _ in 0..=GOAL_BLOCK_CAP {
                responses.push(text_response(StopReason::EndTurn));
                responses.push(goal_response(false, false, "more work remains"));
            }
            let goal = GoalHandle::default();
            goal.set("never met").unwrap();
            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(MockProvider::new(responses), &mut history);
            let mut agent = agent.with_goal(goal.clone());

            agent.run(default_input()).await.unwrap();
            drop(agent);

            assert_eq!(goal.snapshot().unwrap().evaluations, GOAL_BLOCK_CAP + 1);
            assert!(event_rx.try_iter().any(|envelope| matches!(
                envelope.event,
                AgentEvent::GoalLoopCap { evaluations } if evaluations == GOAL_BLOCK_CAP + 1
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
                    crate::mcp::TOOL_SEARCH_TOOL_NAME,
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
            assert!(first.contains(&crate::mcp::TOOL_SEARCH_TOOL_NAME));
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
                    AgentEvent::AutoCompacting
                )),
                expected,
            );
        });
    }

    #[test]
    fn do_compact_appends_post_instructions_to_continue_message() {
        smol::block_on(async {
            const POST: &str = "Re-read plan.md";
            let mut history = History::new(vec![Message::user("go".into())]);
            let (mut agent, _event_rx) = make_agent(
                MockProvider::new(vec![text_response(StopReason::EndTurn)]),
                &mut history,
            );
            agent.config.post_compaction_instructions = Some(POST.into());
            agent.do_compact().await.unwrap();
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

            let mut history = History::new(Vec::new());
            let (agent, event_rx) = make_agent(StubStreamProvider::default(), &mut history);
            let mut agent = agent.with_cancel(cancel);

            assert_eq!(
                agent.run(default_input()).await.unwrap(),
                DoneReason::Cancelled
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
        1, 0
        ; "no_nudge_without_recent_tools"
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
