use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use caudra_config::ModelPolicy;
use caudra_providers::model_registry::Binding;
use caudra_providers::provider::{Provider, from_model_async};
use caudra_providers::{
    AgentError, Billing, ContentBlock, Message, Model, ModelPurpose, RequestOptions, Timeouts,
    TokenUsage,
};
use caudra_storage::sessions::{StoredActiveGoal, StoredGoalVerdict};
use serde::{Deserialize, Serialize};
use serde_json::json;
use strum::Display;

use super::run::estimate_message_tokens;
use super::streaming::stream_silent_with_retry;
use crate::cancel::CancelToken;
use crate::{AgentEvent, EventSender};

pub const MAX_GOAL_CHARS: usize = 4_000;
pub const DEFAULT_GOAL_CONTINUATION_LIMIT: u32 = 16;
pub const MAX_GOAL_CONTINUATION_LIMIT: u32 = 100;
const EVALUATOR_INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(30);
const EVALUATOR_OUTPUT_TOKENS: u32 = 4_096;
const MAX_EVALUATOR_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_VALIDATION_ATTEMPTS: u32 = 3;
const INITIAL_TRANSCRIPT_PERCENT: u32 = 50;
const RETRY_TRANSCRIPT_PERCENT: u32 = 25;
const TOOL_CALL_LINE: &str = "[tool call]";
const TOOL_RESULT_LINE: &str = "[tool result]";
const TOOL_ERROR_LINE: &str = "[tool error]";
const UNNAMED_TOOL: &str = "tool";
const NO_TEXT_ERROR: &str = "evaluator returned no text";
const OVERSIZE_ERROR: &str = "evaluator response was too large";
const TOOL_CALL_ERROR: &str = "evaluator attempted to call a tool instead of answering";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GoalError {
    #[error("goal condition is empty")]
    Empty,
    #[error("goal condition exceeds {MAX_GOAL_CHARS} characters")]
    TooLong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum GoalVerdict {
    NotMet,
    Met,
    Impossible,
}

#[derive(Debug, Clone)]
pub struct GoalSnapshot {
    pub condition: Arc<str>,
    pub evaluations: u32,
    pub last_verdict: Option<GoalVerdict>,
    pub last_reason: Option<Arc<str>>,
    pub started_at: Instant,
    /// What the goal had already spent on the clock before this process saw
    /// it. Zero for a goal set here, and the stored total for a resumed one.
    pub elapsed_before: Duration,
    pub usage: TokenUsage,
    pub cost: Option<f64>,
    pub subscription_cost: Option<f64>,
    pub(crate) generation: u64,
}

impl GoalSnapshot {
    pub fn elapsed(&self) -> Duration {
        self.elapsed_before + self.started_at.elapsed()
    }
}

impl From<GoalVerdict> for StoredGoalVerdict {
    fn from(verdict: GoalVerdict) -> Self {
        match verdict {
            GoalVerdict::Met => Self::Met,
            GoalVerdict::NotMet => Self::NotMet,
            GoalVerdict::Impossible => Self::Impossible,
        }
    }
}

impl From<StoredGoalVerdict> for GoalVerdict {
    fn from(verdict: StoredGoalVerdict) -> Self {
        match verdict {
            StoredGoalVerdict::Met => Self::Met,
            StoredGoalVerdict::NotMet => Self::NotMet,
            StoredGoalVerdict::Impossible => Self::Impossible,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GoalResult {
    pub condition: Arc<str>,
    pub verdict: GoalVerdict,
    pub reason: Arc<str>,
    pub evaluations: u32,
    pub duration: Duration,
    pub usage: TokenUsage,
    pub cost: Option<f64>,
    pub subscription_cost: Option<f64>,
}

#[derive(Debug, Clone)]
pub enum GoalStatus {
    Active(GoalSnapshot),
    Finished(GoalResult),
}

struct GoalState {
    generation: u64,
    active: Option<GoalSnapshot>,
    finished: Option<GoalResult>,
    continuation_limit: u32,
}

impl Default for GoalState {
    fn default() -> Self {
        Self {
            generation: 0,
            active: None,
            finished: None,
            continuation_limit: DEFAULT_GOAL_CONTINUATION_LIMIT,
        }
    }
}

#[derive(Clone, Default)]
pub struct GoalHandle {
    inner: Arc<Mutex<GoalState>>,
}

pub(crate) enum GoalApply {
    Stale,
    Continue { evaluation: u32 },
    Terminal,
}

impl GoalHandle {
    /// Reopens a goal a previous run left active, with the spend, evaluation
    /// count and clock it had then. A condition that no longer validates is
    /// dropped rather than resumed: the panel would have nothing to show and
    /// the evaluator nothing to check.
    pub fn restored(stored: Option<&StoredActiveGoal>) -> Self {
        let handle = Self::default();
        if let Some(stored) = stored
            && let Ok(condition) = Goal::validate(&stored.condition)
        {
            let mut state = handle.lock();
            state.generation = state.generation.wrapping_add(1);
            state.active = Some(GoalSnapshot {
                condition,
                evaluations: stored.evaluations,
                last_verdict: stored.last_verdict.map(Into::into),
                last_reason: stored.last_reason.as_deref().map(Arc::from),
                started_at: Instant::now(),
                elapsed_before: Duration::from_millis(stored.elapsed_ms),
                usage: stored.usage.into(),
                cost: stored.usage.cost,
                subscription_cost: stored.usage.subscription_cost,
                generation: state.generation,
            });
        }
        handle
    }

    pub fn set(&self, condition: &str) -> Result<GoalSnapshot, GoalError> {
        let condition = Goal::validate(condition)?;
        Ok(self.set_validated(condition))
    }

    fn set_validated(&self, condition: Arc<str>) -> GoalSnapshot {
        let mut state = self.lock();
        state.generation = state.generation.wrapping_add(1);
        let snapshot = GoalSnapshot {
            condition,
            evaluations: 0,
            last_verdict: None,
            last_reason: None,
            started_at: Instant::now(),
            elapsed_before: Duration::ZERO,
            usage: TokenUsage::default(),
            cost: None,
            subscription_cost: None,
            generation: state.generation,
        };
        state.finished = None;
        state.active = Some(snapshot.clone());
        snapshot
    }

    pub fn clear(&self) -> Option<GoalSnapshot> {
        let mut state = self.lock();
        let active = state.active.take();
        if active.is_some() {
            state.generation = state.generation.wrapping_add(1);
        }
        active
    }

    pub fn reset(&self) {
        let mut state = self.lock();
        state.generation = state.generation.wrapping_add(1);
        state.active = None;
        state.finished = None;
    }

    pub fn restore_finished(&self, result: GoalResult) {
        let mut state = self.lock();
        if state.active.is_none() {
            state.finished = Some(result);
        }
    }

    pub fn snapshot(&self) -> Option<GoalSnapshot> {
        self.lock().active.clone()
    }

    pub fn status(&self) -> Option<GoalStatus> {
        let state = self.lock();
        state
            .active
            .clone()
            .map(GoalStatus::Active)
            .or_else(|| state.finished.clone().map(GoalStatus::Finished))
    }

    pub fn active_condition(&self) -> Option<String> {
        self.lock()
            .active
            .as_ref()
            .map(|goal| goal.condition.to_string())
    }

    pub fn continuation_limit(&self) -> u32 {
        self.lock().continuation_limit
    }

    pub fn set_continuation_limit(&self, limit: u32) {
        self.lock().continuation_limit = limit.min(MAX_GOAL_CONTINUATION_LIMIT);
    }

    pub fn record_external_usage(&self, usage: TokenUsage, cost: Option<f64>, billing: Billing) {
        self.record_usage(usage, cost, billing);
    }

    pub(crate) fn record_usage(&self, usage: TokenUsage, cost: Option<f64>, billing: Billing) {
        if let Some(active) = self.lock().active.as_mut() {
            active.usage += usage;
            add_spend(active, cost, billing);
        }
    }

    pub(crate) fn record_usage_for(
        &self,
        generation: u64,
        usage: TokenUsage,
        cost: Option<f64>,
        billing: Billing,
    ) {
        let mut state = self.lock();
        if let Some(active) = state.active.as_mut()
            && active.generation == generation
        {
            active.usage += usage;
            add_spend(active, cost, billing);
        }
    }

    pub(crate) fn is_generation_active(&self, generation: u64) -> bool {
        self.lock()
            .active
            .as_ref()
            .is_some_and(|goal| goal.generation == generation)
    }

    pub(crate) fn apply_evaluation(
        &self,
        generation: u64,
        verdict: GoalVerdict,
        reason: Arc<str>,
    ) -> GoalApply {
        let mut state = self.lock();
        let Some(active) = state
            .active
            .as_mut()
            .filter(|goal| goal.generation == generation)
        else {
            return GoalApply::Stale;
        };
        active.evaluations = active.evaluations.saturating_add(1);
        active.last_verdict = Some(verdict);
        active.last_reason = Some(Arc::clone(&reason));
        let evaluation = active.evaluations;
        if verdict == GoalVerdict::NotMet {
            return GoalApply::Continue { evaluation };
        }

        let active = state.active.take().expect("active goal checked above");
        state.finished = Some(GoalResult {
            condition: active.condition,
            verdict,
            reason,
            evaluations: active.evaluations,
            duration: active.started_at.elapsed(),
            usage: active.usage,
            cost: active.cost,
            subscription_cost: active.subscription_cost,
        });
        GoalApply::Terminal
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GoalState> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }
}

struct Goal;

impl Goal {
    fn validate(condition: &str) -> Result<Arc<str>, GoalError> {
        let condition = condition.trim();
        if condition.is_empty() {
            return Err(GoalError::Empty);
        }
        if condition.chars().count() > MAX_GOAL_CHARS {
            return Err(GoalError::TooLong);
        }
        Ok(Arc::from(condition))
    }
}

pub(crate) struct EvaluationResult {
    pub verdict: GoalVerdict,
    pub reason: String,
    pub usage: TokenUsage,
    pub cost: Option<f64>,
    pub billing: Billing,
    pub model: String,
}

pub(crate) struct EvaluationError {
    pub error: AgentError,
    pub usage: TokenUsage,
    pub cost: Option<f64>,
    pub billing: Billing,
    pub model: String,
}

#[derive(Clone)]
pub(crate) struct ResolvedEvaluator {
    /// The Goal binding this was resolved from, or `None` when nothing is bound
    /// and the default rule chose the model.
    pub binding: Option<Binding>,
    pub provider: Arc<dyn Provider>,
    pub model: Model,
}

impl ResolvedEvaluator {
    /// Only an unbound evaluator retreats to the chat model. A model the user
    /// named is reported instead, since silently substituting it would hide the
    /// choice they made.
    pub(crate) fn fallback_to_current(
        &self,
        error: &AgentError,
        current_provider: &Arc<dyn Provider>,
        current_model: &Model,
    ) -> Option<Self> {
        if self.binding.is_some()
            || !error.is_model_unavailable()
            || (self.model.provider == current_model.provider && self.model.id == current_model.id)
        {
            return None;
        }
        Some(Self {
            binding: None,
            provider: Arc::clone(current_provider),
            model: with_evaluator_limits(current_model.clone()),
        })
    }
}

pub(crate) async fn resolve_evaluator(
    current_provider: &Arc<dyn Provider>,
    current_model: &Model,
    binding: Option<Binding>,
    timeouts: Timeouts,
    model_policy: &ModelPolicy,
    cancel: &CancelToken,
    cached_provider: Option<Arc<dyn Provider>>,
) -> Result<ResolvedEvaluator, AgentError> {
    let current_provider = Arc::clone(current_provider);
    let current_model = current_model.clone();
    let current_slug = Arc::clone(&current_model.provider);
    let model_policy = model_policy.clone();
    let bound = binding.clone();
    let (model, provider) = initialization_with_limits(cancel, async move {
        let mut model =
            smol::unblock(move || evaluator_model(bound.as_ref(), &current_model, &model_policy))
                .await?;
        let provider: Arc<dyn Provider> = if model.provider == current_slug {
            current_provider.adjust_model(&mut model);
            current_provider
        } else if let Some(provider) = cached_provider {
            provider.adjust_model(&mut model);
            provider
        } else {
            Arc::from(from_model_async(&mut model, timeouts).await?)
        };
        Ok((model, provider))
    })
    .await?;
    let model = with_evaluator_limits(model);
    Ok(ResolvedEvaluator {
        binding,
        provider,
        model,
    })
}

fn with_evaluator_limits(mut model: Model) -> Model {
    model.max_output_tokens = Some(
        model
            .max_output_tokens
            .unwrap_or(EVALUATOR_OUTPUT_TOKENS)
            .min(EVALUATOR_OUTPUT_TOKENS),
    );
    model
}

async fn initialization_with_limits<T>(
    cancel: &CancelToken,
    future: impl Future<Output = Result<T, AgentError>>,
) -> Result<T, AgentError> {
    let timed = futures_lite::future::race(future, async {
        smol::Timer::after(EVALUATOR_INITIALIZATION_TIMEOUT).await;
        Err(AgentError::Timeout {
            secs: EVALUATOR_INITIALIZATION_TIMEOUT.as_secs(),
        })
    });
    cancel
        .race(timed)
        .await
        .map_err(|_| AgentError::Cancelled)?
}

fn evaluator_model(
    binding: Option<&Binding>,
    current_model: &Model,
    model_policy: &ModelPolicy,
) -> Result<Model, AgentError> {
    Model::resolve_binding(ModelPurpose::Goal, binding, current_model, model_policy).map_err(
        |error| AgentError::Config {
            message: format!("cannot resolve the goal evaluator model: {error}"),
        },
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluationWire {
    ok: bool,
    reason: String,
    #[serde(default)]
    impossible: bool,
}

pub(crate) struct Evaluator<'a> {
    pub provider: &'a dyn Provider,
    pub model: &'a Model,
    pub history: &'a [Message],
    pub condition: &'a str,
    pub evaluation: u32,
    pub event_tx: &'a EventSender,
    pub cancel: &'a CancelToken,
}

impl Evaluator<'_> {
    pub async fn run(&self) -> Result<EvaluationResult, EvaluationError> {
        let model = self.model;

        let mut percent = INITIAL_TRANSCRIPT_PERCENT;
        let mut validation_attempt = 0;
        let mut previous_error = None;
        let mut previous_output = None;
        let mut usage = TokenUsage::default();
        let mut cost = None;

        loop {
            let mut messages = transcript_within_budget(self.history, model, percent);
            messages.push(Message::synthetic(evaluator_prompt(
                self.condition,
                self.evaluation,
                previous_error.as_deref(),
                previous_output.as_deref(),
            )));

            let response = match evaluator_request(
                self.provider,
                model,
                &messages,
                crate::prompt::GOAL_EVALUATOR,
                self.event_tx,
                self.cancel,
            )
            .await
            {
                Ok(response) => response,
                Err(error)
                    if error.is_context_overflow() && percent == INITIAL_TRANSCRIPT_PERCENT =>
                {
                    percent = RETRY_TRANSCRIPT_PERCENT;
                    continue;
                }
                Err(error) => {
                    return Err(EvaluationError {
                        error,
                        usage,
                        cost,
                        billing: model.billing,
                        model: model.spec(),
                    });
                }
            };
            usage += response.usage;
            add_cost(&mut cost, model.billed_cost(&response.usage, false));

            let (error, output) = match response_text(&response.message) {
                Ok(output) => match parse_evaluation(&output) {
                    Ok((verdict, reason)) => {
                        return Ok(EvaluationResult {
                            verdict,
                            reason,
                            usage,
                            cost,
                            billing: model.billing,
                            model: model.spec(),
                        });
                    }
                    Err(error) => (error, Some(output)),
                },
                Err(error) => (error, None),
            };
            validation_attempt += 1;
            if validation_attempt >= MAX_VALIDATION_ATTEMPTS {
                return Err(EvaluationError {
                    error: AgentError::Tool {
                        tool: "goal_evaluator".into(),
                        message: format!(
                            "invalid evaluator response after {MAX_VALIDATION_ATTEMPTS} attempts: {error}"
                        ),
                    },
                    usage,
                    cost,
                    billing: model.billing,
                    model: model.spec(),
                });
            }
            previous_error = Some(error);
            previous_output = output.as_deref().map(truncate_output);
        }
    }
}

async fn evaluator_request(
    provider: &dyn Provider,
    model: &Model,
    messages: &[Message],
    system: &str,
    event_tx: &EventSender,
    cancel: &CancelToken,
) -> Result<caudra_providers::StreamResponse, AgentError> {
    let tools = json!([]);
    let request = stream_silent_with_retry(
        provider,
        model,
        messages,
        system,
        &tools,
        Some(event_tx),
        cancel,
        RequestOptions::default(),
        None,
    );
    let result = request.await.map_err(Into::into);
    event_tx.send(AgentEvent::PromptProgress {
        processed: 0,
        total: 0,
        cache: 0,
    })?;
    result
}

/// Re-containers tool traffic as plain text. Tool output is the evidence the
/// evaluator judges, so nothing is dropped; the point is that a transcript of
/// `tool_use` blocks demonstrates tool calling to the one model being asked not
/// to call tools, and weak models follow the demonstration over the
/// instruction. It also stops the request contradicting itself, since the
/// evaluator declares no tools while replaying a conversation full of them.
///
/// Message count is preserved, which the omission notice in
/// [`transcript_within_budget`] counts on, and no tool pair survives for
/// [`truncate_oldest_round`] to orphan.
fn flatten_tool_blocks(messages: &mut [Message]) {
    let names: HashMap<String, String> = messages
        .iter()
        .flat_map(|message| message.tool_uses())
        .map(|(id, name, _)| (id.to_owned(), name.to_owned()))
        .collect();

    for message in messages {
        for block in &mut message.content {
            let text = match block {
                ContentBlock::ToolUse { name, input, .. } => {
                    format!("{TOOL_CALL_LINE} {name} {input}")
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } => {
                    let line = if *is_error {
                        TOOL_ERROR_LINE
                    } else {
                        TOOL_RESULT_LINE
                    };
                    // Unknown once the owning call is truncated away.
                    let name = names.get(tool_use_id).map_or(UNNAMED_TOOL, String::as_str);
                    format!("{line} {name}\n{content}")
                }
                _ => continue,
            };
            *block = ContentBlock::Text { text };
        }
    }
}

fn transcript_within_budget(history: &[Message], model: &Model, percent: u32) -> Vec<Message> {
    let mut messages = history.to_vec();
    // Before budgeting, so the estimate measures what is actually sent.
    flatten_tool_blocks(&mut messages);
    let budget = model.context_window.saturating_mul(percent) / 100;
    // Rescans the whole transcript per iteration. The token counts themselves
    // are cached, so an iteration hashes the transcript rather than tokenizing
    // it, but the walk is still proportional to its size. A running total is
    // not worth the state here, and this runs once per goal evaluation rather
    // than per turn.
    while estimate_message_tokens(&messages) > budget && truncate_oldest_round(&mut messages) {}
    let omitted = history.len().saturating_sub(messages.len());
    if omitted > 0 {
        messages.insert(
            0,
            Message::synthetic(format!(
                "[Earlier conversation truncated to fit the goal evaluator context window: {omitted} message(s) omitted. If required evidence may be in the omitted prefix, return not met with reason \"insufficient evidence in transcript\".]"
            )),
        );
    }
    messages
}

fn truncate_oldest_round(messages: &mut Vec<Message>) -> bool {
    if messages.len() <= 1 {
        return false;
    }
    let next_user = messages
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, message)| matches!(message.role, caudra_providers::Role::User))
        .map(|(index, _)| index)
        .unwrap_or(1);
    messages.drain(..next_user);
    while messages.len() > 1
        && matches!(
            messages.first().map(|message| &message.role),
            Some(caudra_providers::Role::Assistant)
        )
    {
        messages.remove(0);
    }
    true
}

fn evaluator_prompt(
    condition: &str,
    evaluation: u32,
    previous_error: Option<&str>,
    previous_output: Option<&str>,
) -> String {
    let condition = serde_json::to_string(condition).unwrap_or_else(|_| "\"\"".into());
    let mut prompt = format!("Evaluation number: {evaluation}\nCondition: {condition}");
    if let Some(error) = previous_error {
        prompt.push_str(&format!(
            "\n\nYour previous response was invalid ({error})."
        ));
        if let Some(output) = previous_output {
            prompt.push_str(&format!(" Previous response:\n{output}"));
        }
        prompt.push_str("\n\nReturn only the required JSON object.");
    }
    prompt
}

/// A stray tool call beside a real answer is ignored rather than fatal:
/// [`parse_evaluation`] is the arbiter of the text, and a bad answer reports the
/// more actionable JSON error. Only a reply that is *nothing but* a tool call
/// fails, and it names the tool so the retry has something concrete to correct.
fn response_text(message: &Message) -> Result<String, String> {
    let mut output = String::new();
    for block in &message.content {
        if let ContentBlock::Text { text } = block {
            output.push_str(text);
        }
    }
    if output.trim().is_empty() {
        return match message.tool_uses().next() {
            Some((_, name, _)) => Err(format!("{TOOL_CALL_ERROR} ({name})")),
            None => Err(NO_TEXT_ERROR.into()),
        };
    }
    if output.len() > MAX_EVALUATOR_OUTPUT_BYTES {
        return Err(OVERSIZE_ERROR.into());
    }
    Ok(output)
}

fn parse_evaluation(output: &str) -> Result<(GoalVerdict, String), String> {
    let output = output.trim();
    let candidate = strip_json_fence(output).unwrap_or(output);
    if !candidate.starts_with('{') || !candidate.ends_with('}') {
        return Err("response must contain exactly one JSON object".into());
    }

    let value =
        serde_json::from_str::<EvaluationWire>(candidate).map_err(|error| error.to_string())?;
    let reason = value.reason.trim();
    if reason.is_empty() {
        return Err("reason is empty".into());
    }
    if value.ok && value.impossible {
        return Err("ok and impossible cannot both be true".into());
    }
    let verdict = if value.impossible {
        GoalVerdict::Impossible
    } else if value.ok {
        GoalVerdict::Met
    } else {
        GoalVerdict::NotMet
    };
    Ok((verdict, reason.to_string()))
}

fn strip_json_fence(output: &str) -> Option<&str> {
    let output = output
        .strip_prefix("```json")
        .or_else(|| output.strip_prefix("```"))?;
    output.strip_suffix("```").map(str::trim)
}

fn truncate_output(output: &str) -> String {
    output.chars().take(2_000).collect()
}

fn add_cost(total: &mut Option<f64>, cost: Option<f64>) {
    if let Some(cost) = cost {
        *total = Some(total.unwrap_or_default() + cost);
    }
}

/// A goal's running spend, filed under whoever pays for the turn that added it.
fn add_spend(goal: &mut GoalSnapshot, cost: Option<f64>, billing: Billing) {
    match billing {
        Billing::Api => add_cost(&mut goal.cost, cost),
        Billing::Subscription => add_cost(&mut goal.subscription_cost, cost),
    }
}

pub(crate) fn continuation_message(condition: &str, reason: &str) -> String {
    let condition = serde_json::to_string(condition).unwrap_or_else(|_| "\"\"".into());
    let reason = serde_json::to_string(reason).unwrap_or_else(|_| "\"\"".into());
    format!(
        "The active goal is not yet satisfied.\nGoal: {condition}\nEvaluator reason: {reason}\nContinue working toward the goal and produce evidence in the conversation."
    )
}

pub fn goal_kickoff_message(condition: &str) -> String {
    let condition = serde_json::to_string(condition).unwrap_or_else(|_| "\"\"".into());
    format!(
        "A session-scoped goal is active with condition: {condition}. Briefly acknowledge the goal, then immediately start or continue working toward it. Treat the condition as your directive and do not pause to ask what to do. Completion will be evaluated automatically; do not claim success without evidence."
    )
}

pub fn goal_checkin_message(condition: &str) -> String {
    let condition = serde_json::to_string(condition).unwrap_or_else(|_| "\"\"".into());
    format!(
        "Goal check-in: {condition} is still active. Evaluation was deferred while background work ran, and that work is no longer running. Review its results and continue toward the goal."
    )
}

pub(crate) fn is_unrecoverable(error: &AgentError) -> bool {
    if error.is_auth_error() || error.is_context_overflow() || error.is_model_unavailable() {
        return true;
    }
    let AgentError::Api {
        status, message, ..
    } = error
    else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    *status == 402
        || message.contains("credit balance")
        || message.contains("insufficient credit")
        || ((*status == 401 || *status == 403)
            && (message.contains("authentication")
                || message.contains("oauth")
                || message.contains("organization")
                || message.contains("account on hold")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_providers::provider::BoxFuture;
    use caudra_providers::{
        CacheKey, ImageMediaType, ImageSource, ModelInfo, ProviderEvent, Role, StreamResponse,
    };
    use serde_json::Value;
    use test_case::test_case;

    const CHAT_SPEC: &str = "anthropic/claude-sonnet-4-20250514";
    const HAIKU_SPEC: &str = "anthropic/claude-haiku-4-5";
    const OPUS_SPEC: &str = "anthropic/claude-opus-4-6-20260101";
    const CALL_ID: &str = "call_1";
    const TOOL_NAME: &str = "file_read";
    const READ_PATH: &str = "src/main.rs";
    const RESULT_BODY: &str = "fn main() {}";
    const PROMPT: &str = "ship it";
    const THOUGHT: &str = "weighing the evidence";
    const IMAGE_DATA: &str = "aGk=";
    const ANSWER_JSON: &str = r#"{"ok":true,"reason":"tests pass","impossible":false}"#;
    const NO_TOOL_BLOCKS: &str = "flattening must leave no tool blocks behind";

    struct NullProvider;

    struct ProgressProvider;

    impl Provider for NullProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async { unimplemented!() })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    impl Provider for ProgressProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            event_tx: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                event_tx.send(ProviderEvent::PromptProgress {
                    processed: 100,
                    total: 1_000,
                    cache: 50,
                })?;
                Ok(StreamResponse::default())
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    #[test]
    fn validates_condition_by_characters() {
        assert_eq!(Goal::validate("  ship it  ").unwrap().as_ref(), "ship it");
        assert_eq!(Goal::validate("").unwrap_err(), GoalError::Empty);
        assert_eq!(
            Goal::validate(&"é".repeat(MAX_GOAL_CHARS + 1)).unwrap_err(),
            GoalError::TooLong
        );
    }

    #[test]
    fn continuation_limit_is_session_scoped_and_bounded() {
        let handle = GoalHandle::default();
        assert_eq!(handle.continuation_limit(), DEFAULT_GOAL_CONTINUATION_LIMIT);

        handle.set_continuation_limit(24);
        handle.set("first").unwrap();
        handle.clear();
        handle.set("second").unwrap();
        assert_eq!(handle.continuation_limit(), 24);

        handle.set_continuation_limit(MAX_GOAL_CONTINUATION_LIMIT + 1);
        assert_eq!(handle.continuation_limit(), MAX_GOAL_CONTINUATION_LIMIT);
    }

    #[test]
    fn evaluator_request_forwards_prefill_progress_and_clears_it() {
        smol::block_on(async {
            let provider = ProgressProvider;
            let model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
            let (raw_tx, event_rx) = flume::unbounded();
            let event_tx = EventSender::new(raw_tx, 0);

            evaluator_request(
                &provider,
                &model,
                &[],
                "system",
                &event_tx,
                &CancelToken::none(),
            )
            .await
            .unwrap();
            drop(event_tx);

            let events: Vec<_> = event_rx.drain().map(|envelope| envelope.event).collect();
            assert!(matches!(
                events.as_slice(),
                [
                    AgentEvent::PromptProgress {
                        processed: 100,
                        total: 1_000,
                        cache: 50,
                    },
                    AgentEvent::PromptProgress {
                        processed: 0,
                        total: 0,
                        cache: 0,
                    },
                ]
            ));
        });
    }

    #[test]
    fn stale_evaluation_cannot_clear_replacement() {
        let handle = GoalHandle::default();
        let old = handle.set("old").unwrap();
        handle.set("new").unwrap();
        assert!(matches!(
            handle.apply_evaluation(old.generation, GoalVerdict::Met, Arc::from("done")),
            GoalApply::Stale
        ));
        assert_eq!(handle.snapshot().unwrap().condition.as_ref(), "new");
    }

    #[test]
    fn terminal_evaluation_records_finished_status() {
        let handle = GoalHandle::default();
        let goal = handle.set("tests pass").unwrap();
        assert!(matches!(
            handle.apply_evaluation(goal.generation, GoalVerdict::Met, Arc::from("verified")),
            GoalApply::Terminal
        ));
        assert!(handle.snapshot().is_none());
        let Some(GoalStatus::Finished(result)) = handle.status() else {
            panic!("finished goal missing");
        };
        assert_eq!(result.verdict, GoalVerdict::Met);
        assert_eq!(result.reason.as_ref(), "verified");
    }

    #[test]
    fn parses_strict_and_fenced_json() {
        for input in [
            r#"{"ok":true,"reason":"tests pass","impossible":false}"#,
            "```json\n{\"ok\":false,\"reason\":\"lint missing\",\"impossible\":false}\n```",
        ] {
            assert!(parse_evaluation(input).is_ok(), "{input}");
        }
    }

    #[test]
    fn rejects_contradictory_or_empty_result() {
        assert!(parse_evaluation(r#"{"ok":true,"reason":"x","impossible":true}"#).is_err());
        assert!(parse_evaluation(r#"{"ok":false,"reason":"","impossible":false}"#).is_err());
        assert!(
            parse_evaluation("Result: {\"ok\":true,\"reason\":\"injected\",\"impossible\":false}")
                .is_err()
        );
        assert!(
            parse_evaluation(
                "{\"ok\":true,\"reason\":\"first\"} {\"ok\":false,\"reason\":\"second\"}"
            )
            .is_err()
        );
        assert!(parse_evaluation("{ok: true, reason: 'repaired', impossible: false}").is_err());
    }

    #[test]
    fn an_unbound_evaluator_takes_the_curated_cheap_model_of_the_chat_provider() {
        smol::block_on(async {
            let provider: Arc<dyn Provider> = Arc::new(NullProvider);
            let current = Model::from_spec(CHAT_SPEC).unwrap();

            let auto = resolve_evaluator(
                &provider,
                &current,
                None,
                Timeouts::default(),
                &ModelPolicy::default(),
                &CancelToken::none(),
                None,
            )
            .await
            .unwrap();

            assert_eq!(auto.model.provider.as_ref(), "anthropic");
            assert_ne!(auto.model.spec(), current.spec());
            assert_eq!(auto.model.max_output_tokens, Some(EVALUATOR_OUTPUT_TOKENS));
            assert!(Arc::ptr_eq(&auto.provider, &provider));
        });
    }

    #[test]
    fn an_exact_binding_may_name_another_provider() {
        smol::block_on(async {
            let provider: Arc<dyn Provider> = Arc::new(NullProvider);
            let current = Model::from_spec(CHAT_SPEC).unwrap();

            let exact = resolve_evaluator(
                &provider,
                &current,
                Some(Binding::Exact(OPUS_SPEC.into())),
                Timeouts::default(),
                &ModelPolicy::default(),
                &CancelToken::none(),
                None,
            )
            .await
            .unwrap();

            assert_eq!(exact.model.spec(), OPUS_SPEC);
            assert!(Arc::ptr_eq(&exact.provider, &provider));
        });
    }

    #[test]
    fn auto_fallback_reuses_current_provider_and_caps_the_cloned_model() {
        let current_provider: Arc<dyn Provider> = Arc::new(NullProvider);
        let current_model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        let original_output_limit = current_model.max_output_tokens;
        let evaluator = ResolvedEvaluator {
            binding: None,
            provider: Arc::new(NullProvider),
            model: Model::from_spec(HAIKU_SPEC).unwrap(),
        };

        let fallback = evaluator
            .fallback_to_current(
                &AgentError::api(404, "model 'claude-haiku-4-5' not found"),
                &current_provider,
                &current_model,
            )
            .unwrap();

        assert!(Arc::ptr_eq(&fallback.provider, &current_provider));
        assert_eq!(fallback.model.spec(), current_model.spec());
        assert_eq!(
            fallback.model.max_output_tokens,
            Some(EVALUATOR_OUTPUT_TOKENS)
        );
        assert_eq!(current_model.max_output_tokens, original_output_limit);
    }

    #[test]
    fn only_an_unbound_evaluator_falls_back_from_an_unavailable_model() {
        let current_provider: Arc<dyn Provider> = Arc::new(NullProvider);
        let current_model = Model::from_spec(CHAT_SPEC).unwrap();
        let unavailable = AgentError::api(404, "unknown model claude-haiku-4-5");

        for binding in [
            Binding::Same(ModelPurpose::Fast),
            Binding::Exact(HAIKU_SPEC.into()),
        ] {
            let evaluator = ResolvedEvaluator {
                binding: Some(binding),
                provider: Arc::new(NullProvider),
                model: Model::from_spec(HAIKU_SPEC).unwrap(),
            };
            assert!(
                evaluator
                    .fallback_to_current(&unavailable, &current_provider, &current_model)
                    .is_none()
            );
        }
    }

    #[test]
    fn auto_does_not_fallback_for_an_unrelated_error_or_the_current_model() {
        let current_provider: Arc<dyn Provider> = Arc::new(NullProvider);
        let current_model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        let evaluator = ResolvedEvaluator {
            binding: None,
            provider: Arc::new(NullProvider),
            model: current_model.clone(),
        };

        assert!(
            evaluator
                .fallback_to_current(
                    &AgentError::api(404, "model 'claude-sonnet-4-20250514' not found"),
                    &current_provider,
                    &current_model,
                )
                .is_none()
        );

        let other_model = ResolvedEvaluator {
            model: Model::from_spec(HAIKU_SPEC).unwrap(),
            ..evaluator
        };
        assert!(
            other_model
                .fallback_to_current(
                    &AgentError::api(404, "route not found"),
                    &current_provider,
                    &current_model,
                )
                .is_none()
        );
    }

    #[test]
    fn explicit_goal_evaluator_never_silently_falls_back() {
        smol::block_on(async {
            let provider: Arc<dyn Provider> = Arc::new(NullProvider);
            let current = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
            let policy = ModelPolicy::new(&[], &["anthropic/claude-opus*".into()]).unwrap();

            let disallowed = resolve_evaluator(
                &provider,
                &current,
                Some(Binding::Exact("anthropic/claude-opus-4-6".into())),
                Timeouts::default(),
                &policy,
                &CancelToken::none(),
                None,
            )
            .await
            .err()
            .expect("disallowed exact model should fail");
            assert!(disallowed.to_string().contains("not allowed"));
        });
    }

    #[test]
    fn evaluator_initialization_honors_cancellation() {
        smol::block_on(async {
            let (trigger, cancel) = CancelToken::new();
            let initialization = initialization_with_limits(
                &cancel,
                futures_lite::future::pending::<Result<(), AgentError>>(),
            );
            let cancel_now = async move {
                trigger.cancel();
            };

            let (result, ()) = futures_lite::future::zip(initialization, cancel_now).await;

            assert!(matches!(result, Err(AgentError::Cancelled)));
        });
    }

    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            ..Default::default()
        }
    }

    fn tool_result(is_error: bool) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: CALL_ID.into(),
                content: RESULT_BODY.into(),
                is_error,
                output_ref: None,
            }],
            ..Default::default()
        }
    }

    fn tool_call() -> Message {
        assistant(vec![ContentBlock::tool_use(
            CALL_ID,
            TOOL_NAME,
            json!({ "path": READ_PATH }),
        )])
    }

    fn block_text(message: &Message) -> String {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn has_tool_blocks(messages: &[Message]) -> bool {
        messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. }
                )
            })
        })
    }

    #[test]
    fn a_flattened_transcript_keeps_its_evidence_and_loses_its_tool_blocks() {
        let mut messages = vec![
            Message::user(PROMPT.into()),
            tool_call(),
            tool_result(false),
        ];

        flatten_tool_blocks(&mut messages);

        assert!(!has_tool_blocks(&messages), "{NO_TOOL_BLOCKS}");
        assert_eq!(messages.len(), 3);
        let call = block_text(&messages[1]);
        assert!(
            call.starts_with(&format!("{TOOL_CALL_LINE} {TOOL_NAME}")),
            "{call}"
        );
        assert!(call.contains(READ_PATH), "{call}");
        assert!(block_text(&messages[2]).contains(RESULT_BODY));
    }

    #[test_case(false, TOOL_RESULT_LINE ; "a_result_is_flattened_as_a_result")]
    #[test_case(true, TOOL_ERROR_LINE ; "an_error_result_stays_marked_as_an_error")]
    fn a_flattened_result_names_the_tool_that_produced_it(is_error: bool, line: &str) {
        let mut messages = vec![tool_call(), tool_result(is_error)];

        flatten_tool_blocks(&mut messages);

        let text = block_text(&messages[1]);
        assert!(text.starts_with(&format!("{line} {TOOL_NAME}")), "{text}");
    }

    /// The owning call is gone once truncation drains the front of the
    /// transcript, leaving the result with nothing to name it.
    #[test]
    fn an_orphaned_result_flattens_under_a_placeholder_name() {
        let mut messages = vec![tool_result(false)];

        flatten_tool_blocks(&mut messages);

        let text = block_text(&messages[0]);
        assert!(
            text.starts_with(&format!("{TOOL_RESULT_LINE} {UNNAMED_TOOL}")),
            "{text}"
        );
    }

    #[test]
    fn thinking_and_images_survive_flattening() {
        let mut messages = vec![assistant(vec![
            ContentBlock::thinking(THOUGHT.into(), None),
            ContentBlock::Image {
                source: ImageSource::new(ImageMediaType::Png, IMAGE_DATA.into()),
            },
            ContentBlock::tool_use(CALL_ID, TOOL_NAME, json!({})),
        ])];

        flatten_tool_blocks(&mut messages);

        assert!(matches!(
            messages[0].content.as_slice(),
            [
                ContentBlock::Thinking { .. },
                ContentBlock::Image { .. },
                ContentBlock::Text { .. }
            ]
        ));
    }

    #[test]
    fn the_budgeted_transcript_is_flattened_and_needs_no_placeholder_result() {
        let history = vec![Message::user(PROMPT.into()), tool_call()];

        let messages = transcript_within_budget(
            &history,
            &Model::from_spec(CHAT_SPEC).unwrap(),
            INITIAL_TRANSCRIPT_PERCENT,
        );

        assert_eq!(messages.len(), history.len());
        assert!(!has_tool_blocks(&messages), "{NO_TOOL_BLOCKS}");
        assert!(
            !messages
                .iter()
                .any(|message| block_text(message).contains(crate::UNAVAILABLE_RESULT)),
            "a dangling call has nothing left to close"
        );
    }

    #[test]
    fn a_stray_tool_call_does_not_discard_a_valid_answer() {
        let message = assistant(vec![
            ContentBlock::Text {
                text: ANSWER_JSON.into(),
            },
            ContentBlock::tool_use(CALL_ID, TOOL_NAME, json!({})),
        ]);

        let output = response_text(&message).unwrap();

        assert_eq!(parse_evaluation(&output).unwrap().0, GoalVerdict::Met);
    }

    #[test]
    fn a_tool_call_with_no_text_names_the_tool_it_tried() {
        let message = tool_call();

        let error = response_text(&message).unwrap_err();

        assert!(error.starts_with(TOOL_CALL_ERROR), "{error}");
        assert!(error.contains(TOOL_NAME), "{error}");
    }

    #[test]
    fn an_answerless_response_still_reports_no_text() {
        let message = assistant(vec![ContentBlock::Text { text: "  ".into() }]);

        assert_eq!(response_text(&message).unwrap_err(), NO_TEXT_ERROR);
    }
}
