//! The delegate's model call. Its model is resolved like the goal evaluator's:
//! a bound model is never replaced, and only an unbound purpose falls back to
//! the chat model when the one it resolved to is unavailable. Every call is
//! reported as autopilot spend and counted against the engagement's budget.

use std::mem;
use std::sync::{Arc, Mutex, MutexGuard};

use caudra_config::{AutopilotConfig, ModelPolicy};
use caudra_providers::model_registry::{self, Binding};
use caudra_providers::provider::Provider;
use caudra_providers::{
    AgentError, Billing, ContentBlock, Message, Model, ModelPurpose, Timeouts, TokenUsage,
};
use caudra_storage::usage_ledger::LedgerPurpose;
use tracing::warn;

use super::context::{AutopilotSnapshot, DecisionRequest, logged_item, user_message};
use super::wire::{
    ActionName, AfterInvalid, AutopilotDecision, DecisionKind, DelegateOutput, StoodDown,
    after_invalid, parse_output,
};
use super::{AutopilotHandle, DecisionOutcome};
use crate::agent::judging::{INITIAL_TRANSCRIPT_PERCENT, RETRY_TRANSCRIPT_PERCENT};
use crate::agent::{JudgeRole, ResolvedEvaluator, evaluator_request, resolve_evaluator};
use crate::cancel::CancelToken;
use crate::prompt::AUTOPILOT_PROMPT;
use crate::types::AutopilotEvent;
use crate::{AgentEvent, EventSender};

const DELEGATE_ROLE: JudgeRole = JudgeRole {
    purpose: ModelPurpose::Autopilot,
    label: "autopilot delegate",
};
const NO_SNAPSHOT_REASON: &str = "autopilot has not seen the conversation yet";
const CORRECTION_SEPARATOR: &str = "\n\n";

/// Where a decision was asked for, as its row records it.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionOrigin {
    /// What was decided, such as the tool a prompt names.
    pub subject: String,
    /// The subagent it was made for, `None` for the main agent.
    pub agent: Option<String>,
    pub tool_use_id: Option<String>,
    /// History items the session held when it was asked for.
    pub history_len: usize,
    /// What the delegate was shown, without the schema.
    pub item: Option<String>,
    /// The engagement it was asked for in.
    pub engagement: Option<u32>,
}

impl DecisionOrigin {
    /// A decision about `item` in `handle`'s engagement, for the main agent
    /// and no tool call until the caller names them.
    pub fn new(handle: &AutopilotHandle, subject: String, item: &str) -> Self {
        Self {
            subject,
            agent: None,
            tool_use_id: None,
            history_len: handle.snapshot().map_or(0, |snapshot| snapshot.history_len),
            item: Some(logged_item(item)),
            engagement: handle.engagement(),
        }
    }
}

/// One decision's place in the engagement, from its number to its last row.
/// Dropped after its announcement and before its row is final, it stands
/// down or settles as cancelled, so every announced decision ends in exactly
/// one row, and none is left open.
pub(crate) struct DecisionSlot {
    handle: AutopilotHandle,
    seq: u32,
    kind: DecisionKind,
    origin: DecisionOrigin,
    event_tx: EventSender,
    stage: Mutex<Stage>,
}

/// Where a decision's row stands.
enum Stage {
    Unannounced,
    /// Announced, with no answer yet.
    Deciding,
    /// Recorded, with what follows to settle. `escalated` when it was left
    /// to the human.
    Open {
        escalated: bool,
    },
    Closed,
}

impl DecisionSlot {
    /// Takes the engagement's next decision number.
    pub(crate) fn new(
        handle: AutopilotHandle,
        kind: DecisionKind,
        origin: DecisionOrigin,
        event_tx: &EventSender,
    ) -> Self {
        Self {
            seq: handle.next_seq(),
            handle,
            kind,
            origin,
            event_tx: event_tx.clone(),
            stage: Mutex::new(Stage::Unannounced),
        }
    }

    pub(crate) fn handle(&self) -> &AutopilotHandle {
        &self.handle
    }

    pub(crate) fn seq(&self) -> u32 {
        self.seq
    }

    pub(crate) fn announce(&self) {
        *self.stage() = Stage::Deciding;
        self.event_tx.try_send(AutopilotEvent::Deciding {
            seq: self.seq,
            kind: self.kind,
            subject: self.origin.subject.clone(),
        });
    }

    /// Asks the delegate, as [`AutopilotHandle::decide`] does. Boxed, because
    /// every caller sits deep in a dispatch or a permission check.
    pub(crate) async fn decide(
        &self,
        request: &DecisionRequest<'_>,
        cancel: &CancelToken,
    ) -> Option<DelegateOutcome> {
        Box::pin(self.handle.decide(request, &self.event_tx, cancel)).await
    }

    /// Records the delegate's answer and, where it is known, what became of
    /// it. Nothing follows that this slot settles.
    pub(crate) fn record(&self, outcome: &DelegateOutcome, settled: Option<DecisionOutcome>) {
        *self.stage() = Stage::Closed;
        self.send_decision(outcome.decision(
            self.seq,
            self.handle.now_ms(),
            self.origin.clone(),
            settled,
        ));
    }

    /// Records the delegate's answer while the request is still open, for the
    /// human's answer or whatever else ends it to settle.
    pub(crate) fn record_open(&self, outcome: &DelegateOutcome) {
        let decision = outcome.decision(self.seq, self.handle.now_ms(), self.origin.clone(), None);
        *self.stage() = Stage::Open {
            escalated: decision.action == ActionName::Escalate,
        };
        self.send_decision(decision);
    }

    /// The human answered with `answer`: a decision still deciding was taken
    /// over, an escalation was answered, and an answer autopilot gave went
    /// unused.
    pub(crate) fn human_answered(&self, answer: &str) {
        let answer = logged_item(answer);
        let outcome = match &*self.stage() {
            Stage::Deciding => DecisionOutcome::TakenOver,
            Stage::Open { escalated: true } => DecisionOutcome::Answered { answer },
            Stage::Open { escalated: false } => DecisionOutcome::HumanFirst { answer },
            Stage::Unannounced | Stage::Closed => return,
        };
        self.end(outcome);
    }

    /// Ends the decision with `outcome`: one still deciding stands down, and
    /// an open one settles. A closed one is left as it is.
    pub(crate) fn end(&self, outcome: DecisionOutcome) {
        let stage = mem::replace(&mut *self.stage(), Stage::Closed);
        let event = match stage {
            Stage::Deciding => AutopilotEvent::StoodDown(Box::new(self.stood_down(outcome))),
            Stage::Open { .. } => self.handle.settle(self.seq, outcome),
            Stage::Unannounced | Stage::Closed => return,
        };
        self.event_tx.try_send(event);
    }

    fn send_decision(&self, decision: AutopilotDecision) {
        self.event_tx.try_send(self.handle.record(decision));
    }

    fn stood_down(&self, outcome: DecisionOutcome) -> StoodDown {
        let origin = self.origin.clone();
        StoodDown {
            seq: self.seq,
            at_ms: self.handle.now_ms(),
            kind: self.kind,
            subject: origin.subject,
            item: origin.item,
            agent: origin.agent,
            tool_use_id: origin.tool_use_id,
            history_len: origin.history_len,
            engagement: origin.engagement,
            outcome,
        }
    }

    fn stage(&self) -> MutexGuard<'_, Stage> {
        self.stage.lock().unwrap_or_else(|error| error.into_inner())
    }
}

impl Drop for DecisionSlot {
    fn drop(&mut self) {
        self.end(DecisionOutcome::Cancelled);
    }
}

/// What one decision came to and what it cost.
#[derive(Debug, Clone, PartialEq)]
pub struct DelegateOutcome {
    pub kind: DecisionKind,
    /// The delegate's valid answer, or why the decision is left to the human.
    pub answer: Result<DelegateOutput, String>,
    pub usage: TokenUsage,
    pub cost: Option<f64>,
    pub billing: Billing,
    /// The model last asked, `None` when none could be resolved.
    pub model: Option<String>,
}

impl DelegateOutcome {
    fn escalated(kind: DecisionKind, reason: String) -> Self {
        Self {
            kind,
            answer: Err(reason),
            usage: TokenUsage::default(),
            cost: None,
            billing: Billing::default(),
            model: None,
        }
    }

    /// The decision's row, with what became of it where that is known. An
    /// answer that never came is recorded as [`ActionName::Escalate`], with
    /// the failure as its reason.
    pub fn decision(
        &self,
        seq: u32,
        at_ms: u64,
        origin: DecisionOrigin,
        outcome: Option<DecisionOutcome>,
    ) -> AutopilotDecision {
        let (action, reason, payload) = match &self.answer {
            Ok(output) => (
                output.action.name(),
                output.reason.clone(),
                output.action.payload(),
            ),
            Err(reason) => (ActionName::Escalate, reason.clone(), None),
        };
        let (cost, subscription_cost) = match self.billing {
            Billing::Api => (self.cost, None),
            Billing::Subscription => (None, self.cost),
        };
        AutopilotDecision {
            seq,
            at_ms,
            kind: self.kind,
            action,
            subject: origin.subject,
            reason,
            agent: origin.agent,
            tool_use_id: origin.tool_use_id,
            history_len: origin.history_len,
            model: self.model.clone(),
            usage: self
                .model
                .is_some()
                .then(|| self.usage.spent(cost, subscription_cost)),
            cost: self.cost,
            engagement: origin.engagement,
            item: origin.item,
            payload,
            outcome,
        }
    }
}

/// What a decision needs from the session: its autopilot, the chat model an
/// unbound delegate falls back to, and where spend is reported.
pub struct Delegate<'a> {
    pub handle: &'a AutopilotHandle,
    pub chat_provider: &'a Arc<dyn Provider>,
    pub chat_model: &'a Model,
    pub timeouts: Timeouts,
    pub model_policy: &'a ModelPolicy,
    pub limits: &'a AutopilotConfig,
    pub event_tx: &'a EventSender,
    pub cancel: &'a CancelToken,
}

impl Delegate<'_> {
    /// Asks the delegate to decide `request` under the purpose's binding as it
    /// stands now. `None` when the session's cancel token fired first.
    pub async fn decide(&self, request: &DecisionRequest<'_>) -> Option<DelegateOutcome> {
        self.decide_bound(model_registry::binding(ModelPurpose::Autopilot), request)
            .await
    }

    async fn decide_bound(
        &self,
        binding: Option<Binding>,
        request: &DecisionRequest<'_>,
    ) -> Option<DelegateOutcome> {
        self.cancel
            .race(async {
                let _permit = self.handle.permits.acquire().await;
                self.decide_permitted(binding, request).await
            })
            .await
            .ok()
            .flatten()
    }

    async fn decide_permitted(
        &self,
        binding: Option<Binding>,
        request: &DecisionRequest<'_>,
    ) -> Option<DelegateOutcome> {
        let Some(snapshot) = self.handle.snapshot() else {
            return Some(DelegateOutcome::escalated(
                request.kind,
                NO_SNAPSHOT_REASON.into(),
            ));
        };
        let decisions = self.handle.recent_decisions();
        let cached = self.handle.lock().delegate_model.clone();
        let resolved = match resolve_evaluator(
            DELEGATE_ROLE,
            self.chat_provider,
            self.chat_model,
            binding,
            self.timeouts,
            self.model_policy,
            self.cancel,
            cached.as_ref(),
        )
        .await
        {
            Ok(resolved) => resolved,
            Err(AgentError::Cancelled) => return None,
            Err(error) => {
                return Some(DelegateOutcome::escalated(request.kind, error.to_string()));
            }
        };
        self.handle.lock().delegate_model = resolved.cacheable().then(|| resolved.clone());
        let mut outcome = DelegateOutcome::escalated(request.kind, String::new());
        let mut answer = self
            .ask(&resolved, &snapshot, &decisions, request, &mut outcome)
            .await;
        if let Err(error) = &answer
            && let Some(fallback) =
                resolved.fallback_to_current(error, self.chat_provider, self.chat_model)
        {
            warn!(
                model = %resolved.model.spec(),
                fallback_model = %fallback.model.spec(),
                status = error.status(),
                error_kind = error.kind(),
                "autopilot delegate model unavailable, falling back to chat model"
            );
            answer = self
                .ask(&fallback, &snapshot, &decisions, request, &mut outcome)
                .await;
        }
        outcome.answer = match answer {
            Ok(answer) => answer,
            Err(AgentError::Cancelled) => return None,
            Err(error) => Err(error.to_string()),
        };
        Some(outcome)
    }

    /// Asks `judge` until an answer is valid or [`after_invalid`] escalates.
    /// `Err` is the provider error that stopped it, after the transport's own
    /// retries.
    async fn ask(
        &self,
        judge: &ResolvedEvaluator,
        snapshot: &AutopilotSnapshot,
        decisions: &[AutopilotDecision],
        request: &DecisionRequest<'_>,
        outcome: &mut DelegateOutcome,
    ) -> Result<Result<DelegateOutput, String>, AgentError> {
        let model = &judge.model;
        outcome.model = Some(model.spec());
        outcome.billing = model.billing;
        let mut transcript_percent = INITIAL_TRANSCRIPT_PERCENT;
        let mut attempt = 1;
        let mut correction: Option<String> = None;
        loop {
            let mut prompt = user_message(
                snapshot,
                decisions,
                request,
                model.context_window,
                transcript_percent,
            );
            if let Some(correction) = &correction {
                prompt.push_str(CORRECTION_SEPARATOR);
                prompt.push_str(correction);
            }
            let response = match evaluator_request(
                &*judge.provider,
                model,
                &[Message::synthetic(prompt)],
                AUTOPILOT_PROMPT,
                self.event_tx,
                self.cancel,
            )
            .await
            {
                Ok(response) => response,
                Err(error)
                    if error.is_context_overflow()
                        && transcript_percent == INITIAL_TRANSCRIPT_PERCENT =>
                {
                    transcript_percent = RETRY_TRANSCRIPT_PERCENT;
                    continue;
                }
                Err(error) => return Err(error),
            };
            self.account(model, response.usage, outcome);
            let output = answer_text(&response.message);
            let error = match parse_output(&output, request.kind, request.reserved) {
                Ok(answer) => return Ok(Ok(answer)),
                Err(error) => error,
            };
            match after_invalid(attempt, &error, &output) {
                AfterInvalid::Retry(next) => {
                    correction = Some(next);
                    attempt += 1;
                }
                AfterInvalid::Escalate(reason) => return Ok(Err(reason)),
            }
        }
    }

    fn account(&self, model: &Model, usage: TokenUsage, outcome: &mut DelegateOutcome) {
        let cost = model.billed_cost(&usage, false);
        outcome.usage += usage;
        if let Some(cost) = cost {
            outcome.cost = Some(outcome.cost.unwrap_or_default() + cost);
        }
        self.event_tx.try_send(AgentEvent::ModelUsage {
            usage,
            cost,
            billing: model.billing,
            provider: model.provider.to_string(),
            model: model.id.clone(),
            purpose: LedgerPurpose::Autopilot,
        });
        if let Some(ended) = self.handle.record_spend(cost, model.billing, self.limits) {
            self.event_tx.try_send(ended);
        }
    }
}

impl AutopilotHandle {
    /// Decides `request` with the chat model the main loop last published and
    /// the limits set now. This is how a caller outside the main loop decides,
    /// such as a prompt raised by any agent sharing the session's permissions.
    /// `None` when `cancel` fired first.
    pub async fn decide(
        &self,
        request: &DecisionRequest<'_>,
        event_tx: &EventSender,
        cancel: &CancelToken,
    ) -> Option<DelegateOutcome> {
        let Some(snapshot) = self.snapshot() else {
            return Some(DelegateOutcome::escalated(
                request.kind,
                NO_SNAPSHOT_REASON.into(),
            ));
        };
        let limits = self.limits();
        let chat = &snapshot.chat;
        Delegate {
            handle: self,
            chat_provider: &chat.provider,
            chat_model: &chat.model,
            timeouts: chat.timeouts,
            model_policy: &chat.model_policy,
            limits: &limits,
            event_tx,
            cancel,
        }
        .decide(request)
        .await
    }
}

fn answer_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use caudra_providers::provider::BoxFuture;
    use caudra_providers::{
        CacheKey, ModelInfo, ProviderEvent, RequestOptions, Role, StopReason, StreamResponse,
    };
    use caudra_storage::autopilot::{AutopilotBudget, AutopilotEnd, DecisionPayload};
    use caudra_storage::sessions::PermissionMode;
    use serde_json::{Value, json};
    use smol::LocalExecutor;
    use test_case::test_case;

    use super::*;
    use crate::Envelope;
    use crate::agent::History;
    use crate::automation::clock::FakeClock;
    use crate::autopilot::wire::{DelegateAction, MAX_DECISION_ATTEMPTS, WireError};
    use crate::autopilot::{ChatSession, MAX_CONCURRENT_DECISIONS};
    use crate::cancel::CancelTrigger;
    use crate::types::AutopilotEvent;

    const START_MS: i64 = 1_790_000_000_000;
    const CHAT_SPEC: &str = "anthropic/claude-sonnet-4-20250514";
    const BOUND_SPEC: &str = "anthropic/claude-opus-4-6-20260101";
    const OTHER_PROVIDER_SPEC: &str = "openai/gpt-5";
    const PROMPT: &str = "run the tests and fix what fails";
    const ITEM: &str = "shell: cargo test";
    const ALLOW: &str = r#"{"action":"allow","reason":"the user asked for the tests"}"#;
    const ALLOW_REASON: &str = "the user asked for the tests";
    const PROSE: &str = "I would allow it.";
    const UNAVAILABLE: &str = "model 'claude-opus-4-6' not found";
    const NOT_FOUND: u16 = 404;
    const OVERFLOW: &str = "prompt is too long";
    const PAYLOAD_TOO_LARGE: u16 = 413;
    const INPUT_TOKENS: u32 = 1_000;
    const OUTPUT_TOKENS: u32 = 40;
    const SEQ: u32 = 5;
    const AT_MS: u64 = 1_790_000_000_500;
    const HISTORY_LEN: usize = 3;
    const AGENT: &str = "review";
    const TOOL_USE_ID: &str = "toolu_1";
    const ENGAGEMENT: u32 = 2;
    const DENY_REASON: &str = "the tests are not part of the task";
    const DENY_GUIDANCE: &str = "stay in the parser crate";
    const NO_TOOLS: &str = "the delegate must be offered no tools";
    const OWN_PROMPT: &str = "the delegate must run on its own system prompt";
    const CORRECTED: &str = "a retry must carry the correction for the answer it replaces";
    const NEVER_REPLACED: &str = "a bound model must never be replaced";
    const FALLS_BACK_ONCE: &str = "an unbound delegate must fall back to the chat model once";
    const CAPPED: &str = "no more than the session's limit may decide at once";
    const NOT_CACHED: &str = "only an exactly bound model may be kept";
    const KEPT: &str = "an exactly bound model must keep its provider while the binding holds";
    const SPEND_ENDS: &str = "the delegate's spend must count toward the engagement's budget";

    pub(crate) enum Step {
        Answer(&'static str),
        Fail(AgentError),
        Cancel(CancelTrigger),
    }

    struct Request {
        model: String,
        system: String,
        tools: Value,
        prompt: String,
    }

    pub(crate) struct ScriptedProvider {
        steps: Mutex<VecDeque<Step>>,
        requests: Mutex<Vec<Request>>,
    }

    impl ScriptedProvider {
        pub(crate) fn new(steps: Vec<Step>) -> Arc<Self> {
            Arc::new(Self {
                steps: Mutex::new(steps.into()),
                requests: Mutex::default(),
            })
        }

        fn models(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|request| request.model.clone())
                .collect()
        }

        /// The user message of every request, oldest first.
        pub(crate) fn prompts(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|request| request.prompt.clone())
                .collect()
        }
    }

    impl Provider for ScriptedProvider {
        fn stream_message<'a>(
            &'a self,
            model: &'a Model,
            messages: &'a [Message],
            system: &'a str,
            tools: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a CacheKey>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(Request {
                    model: model.spec(),
                    system: system.to_owned(),
                    tools: tools.clone(),
                    prompt: messages.iter().map(answer_text).collect(),
                });
                let step = self.steps.lock().unwrap().pop_front();
                match step.expect("the script ran out") {
                    Step::Answer(text) => Ok(response(text)),
                    Step::Fail(error) => Err(error),
                    Step::Cancel(trigger) => {
                        trigger.cancel();
                        futures_lite::future::pending().await
                    }
                }
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    /// Holds every request until the test releases one with its answer. Each
    /// request reports its start with a receiver that disconnects once the
    /// request ends, so a test can see a request dropped unanswered.
    pub(crate) struct GatedProvider {
        pub(crate) started: flume::Sender<flume::Receiver<()>>,
        pub(crate) release: flume::Receiver<&'static str>,
    }

    impl Provider for GatedProvider {
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
            Box::pin(async move {
                let (_running, ended) = flume::bounded(0);
                self.started.send(ended).unwrap();
                let answer = self.release.recv_async().await.unwrap();
                Ok(response(answer))
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    struct Fixture {
        handle: AutopilotHandle,
        chat_provider: Arc<dyn Provider>,
        chat_model: Model,
        model_policy: ModelPolicy,
        limits: AutopilotConfig,
        event_tx: EventSender,
        events: flume::Receiver<Envelope>,
        cancel: CancelToken,
    }

    /// An engaged autopilot that has seen one user turn and decides with
    /// `chat_provider`.
    pub(crate) fn engaged_handle(chat_provider: Arc<dyn Provider>) -> AutopilotHandle {
        let handle = AutopilotHandle::new(FakeClock::new(START_MS));
        handle.engage(false, None);
        let history = History::new(vec![Message::user(PROMPT.into())]);
        handle.observe(AutopilotSnapshot::new(
            history.as_slice(),
            history.active_items(),
            None,
            PermissionMode::Ask,
            ChatSession {
                provider: chat_provider,
                model: Arc::new(Model::from_spec(CHAT_SPEC).unwrap()),
                timeouts: Timeouts::default(),
                model_policy: Arc::default(),
            },
        ));
        handle
    }

    impl Fixture {
        fn new(chat_provider: Arc<dyn Provider>, cancel: CancelToken) -> Self {
            let handle = engaged_handle(Arc::clone(&chat_provider));
            let chat_model = Model::from_spec(CHAT_SPEC).unwrap();
            let (raw_tx, events) = flume::unbounded();
            Self {
                handle,
                chat_provider,
                chat_model,
                model_policy: ModelPolicy::default(),
                limits: AutopilotConfig::default(),
                event_tx: EventSender::new(raw_tx, 0),
                events,
                cancel,
            }
        }

        fn delegate(&self) -> Delegate<'_> {
            Delegate {
                handle: &self.handle,
                chat_provider: &self.chat_provider,
                chat_model: &self.chat_model,
                timeouts: Timeouts::default(),
                model_policy: &self.model_policy,
                limits: &self.limits,
                event_tx: &self.event_tx,
                cancel: &self.cancel,
            }
        }

        fn decide(&self, binding: Option<Binding>, kind: DecisionKind) -> Option<DelegateOutcome> {
            let request = DecisionRequest {
                kind,
                reserved: false,
                item: ITEM,
            };
            smol::block_on(self.delegate().decide_bound(binding, &request))
        }

        fn events(&self) -> Vec<AgentEvent> {
            self.events.drain().map(|envelope| envelope.event).collect()
        }
    }

    fn scripted(steps: Vec<Step>) -> (Arc<ScriptedProvider>, Fixture) {
        let provider = ScriptedProvider::new(steps);
        let fixture = Fixture::new(provider.clone(), CancelToken::none());
        (provider, fixture)
    }

    fn permission_request() -> DecisionRequest<'static> {
        DecisionRequest {
            kind: DecisionKind::Permission,
            reserved: false,
            item: ITEM,
        }
    }

    fn usage() -> TokenUsage {
        TokenUsage {
            input: INPUT_TOKENS,
            output: OUTPUT_TOKENS,
            ..TokenUsage::default()
        }
    }

    fn response(text: &str) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: text.into() }],
                ..Message::default()
            },
            usage: usage(),
            stop_reason: Some(StopReason::EndTurn),
            ..StreamResponse::default()
        }
    }

    fn unavailable() -> Step {
        Step::Fail(AgentError::api(NOT_FOUND, UNAVAILABLE))
    }

    fn overflow() -> Step {
        Step::Fail(AgentError::api(PAYLOAD_TOO_LARGE, OVERFLOW))
    }

    fn allowed() -> Result<DelegateOutput, String> {
        Ok(DelegateOutput {
            action: DelegateAction::Allow,
            reason: ALLOW_REASON.into(),
        })
    }

    fn denied() -> Result<DelegateOutput, String> {
        Ok(DelegateOutput {
            action: DelegateAction::Deny {
                guidance: DENY_GUIDANCE.into(),
            },
            reason: DENY_REASON.into(),
        })
    }

    fn invalid_prose() -> WireError {
        parse_output(PROSE, DecisionKind::Permission, false).unwrap_err()
    }

    fn origin() -> DecisionOrigin {
        DecisionOrigin {
            subject: ITEM.into(),
            agent: Some(AGENT.into()),
            tool_use_id: Some(TOOL_USE_ID.into()),
            history_len: HISTORY_LEN,
            item: Some(ITEM.into()),
            engagement: Some(ENGAGEMENT),
        }
    }

    #[test]
    fn a_valid_first_answer_decides() {
        let (provider, fixture) = scripted(vec![Step::Answer(ALLOW)]);

        let outcome = fixture.decide(None, DecisionKind::Permission).unwrap();

        assert_eq!(outcome.answer, allowed());
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].system, AUTOPILOT_PROMPT, "{OWN_PROMPT}");
        assert_eq!(requests[0].tools, json!([]), "{NO_TOOLS}");
        assert!(requests[0].prompt.contains(ITEM));
        assert_eq!(outcome.model.as_deref(), Some(requests[0].model.as_str()));
        assert!(
            fixture.handle.lock().delegate_model.is_none(),
            "{NOT_CACHED}"
        );
    }

    #[test]
    fn invalid_answers_are_corrected_until_one_is_valid() {
        let (provider, fixture) = scripted(vec![
            Step::Answer(PROSE),
            Step::Answer(PROSE),
            Step::Answer(ALLOW),
        ]);

        let outcome = fixture.decide(None, DecisionKind::Permission).unwrap();

        assert_eq!(outcome.answer, allowed());
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        for (attempt, request) in (1..).zip(&requests[1..]) {
            let AfterInvalid::Retry(correction) = after_invalid(attempt, &invalid_prose(), PROSE)
            else {
                panic!("attempt {attempt} must be retried");
            };
            assert!(request.prompt.ends_with(&correction), "{CORRECTED}");
        }
    }

    #[test]
    fn invalid_answers_escalate_after_the_last_attempt() {
        let attempts = usize::try_from(MAX_DECISION_ATTEMPTS).unwrap();
        let (provider, fixture) = scripted((0..attempts).map(|_| Step::Answer(PROSE)).collect());

        let outcome = fixture.decide(None, DecisionKind::Permission).unwrap();

        let AfterInvalid::Escalate(reason) =
            after_invalid(MAX_DECISION_ATTEMPTS, &invalid_prose(), PROSE)
        else {
            panic!("the last attempt must escalate");
        };
        assert_eq!(outcome.answer, Err(reason));
        assert_eq!(provider.requests.lock().unwrap().len(), attempts);
    }

    #[test]
    fn a_refused_action_escalates_without_a_retry() {
        let (provider, fixture) = scripted(vec![Step::Answer(ALLOW)]);

        let outcome = fixture.decide(None, DecisionKind::Question).unwrap();

        let refused = WireError::Refused {
            kind: DecisionKind::Question,
            action: ActionName::Allow.name().into(),
        };
        assert_eq!(outcome.answer, Err(refused.to_string()));
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_bound_model_that_fails_escalates_without_falling_back() {
        let (provider, fixture) = scripted(vec![unavailable()]);

        let outcome = fixture
            .decide(
                Some(Binding::Exact(BOUND_SPEC.into())),
                DecisionKind::Permission,
            )
            .unwrap();

        assert_eq!(
            outcome.answer,
            Err(AgentError::api(NOT_FOUND, UNAVAILABLE).to_string())
        );
        assert_eq!(provider.models(), [BOUND_SPEC], "{NEVER_REPLACED}");
        assert_eq!(outcome.model.as_deref(), Some(BOUND_SPEC));
    }

    #[test_case(Step::Answer(ALLOW), allowed() ; "the_chat_model_answers")]
    #[test_case(unavailable(), Err(AgentError::api(NOT_FOUND, UNAVAILABLE).to_string()) ; "the_chat_model_fails_too")]
    fn an_unavailable_unbound_model_falls_back_to_the_chat_model_once(
        fallback: Step,
        expected: Result<DelegateOutput, String>,
    ) {
        let (provider, fixture) = scripted(vec![unavailable(), fallback]);

        let outcome = fixture.decide(None, DecisionKind::Permission).unwrap();

        assert_eq!(outcome.answer, expected);
        let models = provider.models();
        assert_eq!(models.len(), 2, "{FALLS_BACK_ONCE}");
        assert_ne!(models[0], CHAT_SPEC, "{FALLS_BACK_ONCE}");
        assert_eq!(models[1], CHAT_SPEC, "{FALLS_BACK_ONCE}");
        assert_eq!(outcome.model.as_deref(), Some(CHAT_SPEC));
    }

    #[test_case(Step::Answer(ALLOW), allowed() ; "then_answers")]
    #[test_case(overflow(), Err(AgentError::api(PAYLOAD_TOO_LARGE, OVERFLOW).to_string()) ; "overflows_again")]
    fn a_context_overflow_retries_once_with_a_smaller_transcript(
        retry: Step,
        expected: Result<DelegateOutput, String>,
    ) {
        let (provider, fixture) = scripted(vec![overflow(), retry]);

        let outcome = fixture.decide(None, DecisionKind::Permission).unwrap();

        assert_eq!(outcome.answer, expected);
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_decision_without_a_snapshot_escalates_without_a_call() {
        let (provider, mut fixture) = scripted(Vec::new());
        fixture.handle = AutopilotHandle::new(FakeClock::new(START_MS));

        let outcome = fixture.decide(None, DecisionKind::Permission).unwrap();

        assert_eq!(outcome.answer, Err(NO_SNAPSHOT_REASON.into()));
        assert!(provider.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn cancellation_ends_the_decision_without_an_outcome() {
        let (trigger, cancel) = CancelToken::new();
        let provider = ScriptedProvider::new(vec![Step::Cancel(trigger)]);
        let fixture = Fixture::new(provider, cancel);

        assert_eq!(fixture.decide(None, DecisionKind::Permission), None);
        assert!(
            !fixture
                .events()
                .iter()
                .any(|event| matches!(event, AgentEvent::ModelUsage { .. }))
        );
    }

    #[test]
    fn every_call_reports_its_spend_as_autopilot() {
        let (_, fixture) = scripted(vec![Step::Answer(PROSE), Step::Answer(ALLOW)]);

        let outcome = fixture.decide(None, DecisionKind::Permission).unwrap();

        let reported: Vec<(TokenUsage, Option<f64>)> = fixture
            .events()
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::ModelUsage {
                    usage,
                    cost,
                    purpose: LedgerPurpose::Autopilot,
                    ..
                } => Some((usage, cost)),
                _ => None,
            })
            .collect();
        assert_eq!(
            reported,
            [(usage(), outcome.cost.map(|cost| cost / 2.0)); 2]
        );
        let mut total = usage();
        total += usage();
        assert_eq!(outcome.usage, total);
        assert_eq!(
            fixture.handle.stored().unwrap().cost_usd,
            outcome.cost.unwrap_or_default()
        );
    }

    #[test]
    fn delegate_spend_counts_toward_the_budget() {
        let (_, mut fixture) = scripted(vec![Step::Answer(ALLOW)]);
        fixture.limits.max_cost = Some(f64::MIN_POSITIVE);

        let outcome = fixture.decide(None, DecisionKind::Permission).unwrap();

        assert!(
            outcome.cost.is_some(),
            "the chat provider's model is priced"
        );
        assert!(!fixture.handle.is_engaged(), "{SPEND_ENDS}");
        assert!(fixture.events().iter().any(|event| matches!(
            event,
            AgentEvent::Autopilot(AutopilotEvent::Ended {
                reason: AutopilotEnd::Budget {
                    kind: AutopilotBudget::Cost
                },
                ..
            })
        )));
    }

    #[test]
    fn a_handle_decides_with_the_published_chat_model_under_its_limits() {
        let (provider, fixture) = scripted(vec![Step::Answer(ALLOW)]);
        fixture.handle.set_limits(AutopilotConfig {
            max_cost: Some(f64::MIN_POSITIVE),
            ..AutopilotConfig::default()
        });

        let outcome = smol::block_on(fixture.handle.decide(
            &permission_request(),
            &fixture.event_tx,
            &fixture.cancel,
        ))
        .unwrap();

        assert_eq!(outcome.answer, allowed());
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        assert!(!fixture.handle.is_engaged(), "{SPEND_ENDS}");
    }

    #[test]
    fn a_handle_that_saw_nothing_escalates_without_a_call() {
        let (provider, fixture) = scripted(Vec::new());
        let handle = AutopilotHandle::new(FakeClock::new(START_MS));

        let outcome = smol::block_on(handle.decide(
            &permission_request(),
            &fixture.event_tx,
            &fixture.cancel,
        ))
        .unwrap();

        assert_eq!(outcome.answer, Err(NO_SNAPSHOT_REASON.into()));
        assert!(provider.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn an_exactly_bound_model_keeps_its_provider_while_the_binding_holds() {
        let bound = ScriptedProvider::new(vec![Step::Answer(ALLOW)]);
        let (chat, fixture) = scripted(Vec::new());
        let binding = Binding::Exact(OTHER_PROVIDER_SPEC.into());
        let kept: Arc<dyn Provider> = bound.clone();
        fixture.handle.lock().delegate_model = Some(ResolvedEvaluator {
            binding: Some(binding.clone()),
            provider: Arc::clone(&kept),
            model: Model::from_spec(OTHER_PROVIDER_SPEC).unwrap(),
        });

        let outcome = fixture
            .decide(Some(binding), DecisionKind::Permission)
            .unwrap();

        assert_eq!(outcome.answer, allowed());
        assert_eq!(bound.models(), [OTHER_PROVIDER_SPEC], "{KEPT}");
        assert!(chat.requests.lock().unwrap().is_empty(), "{KEPT}");
        let cached = fixture.handle.lock().delegate_model.clone().unwrap();
        assert!(Arc::ptr_eq(&cached.provider, &kept), "{KEPT}");
    }

    #[test]
    fn at_most_the_session_limit_decides_at_once() {
        let (started_tx, started) = flume::unbounded();
        let (release, release_rx) = flume::unbounded();
        let fixture = Fixture::new(
            Arc::new(GatedProvider {
                started: started_tx,
                release: release_rx,
            }),
            CancelToken::none(),
        );
        let delegate = fixture.delegate();
        let request = DecisionRequest {
            kind: DecisionKind::Permission,
            reserved: false,
            item: ITEM,
        };
        let executor = LocalExecutor::new();

        smol::block_on(executor.run(async {
            let running: Vec<_> = (0..MAX_CONCURRENT_DECISIONS)
                .map(|_| executor.spawn(delegate.decide_bound(None, &request)))
                .collect();
            for _ in 0..MAX_CONCURRENT_DECISIONS {
                started.recv_async().await.unwrap();
            }
            assert!(fixture.handle.permits.try_acquire().is_none(), "{CAPPED}");

            let queued = executor.spawn(delegate.decide_bound(None, &request));
            release.send(ALLOW).unwrap();
            started.recv_async().await.unwrap();
            for _ in 0..MAX_CONCURRENT_DECISIONS {
                release.send(ALLOW).unwrap();
            }
            for decision in running {
                assert!(decision.await.is_some());
            }
            assert!(queued.await.is_some());
        }));
    }

    #[test_case(allowed(), ActionName::Allow, ALLOW_REASON, None ; "an_answer")]
    #[test_case(denied(), ActionName::Deny, DENY_REASON, Some(DecisionPayload::Guidance(DENY_GUIDANCE.into())) ; "an_answer_with_guidance")]
    #[test_case(Err(NO_SNAPSHOT_REASON.into()), ActionName::Escalate, NO_SNAPSHOT_REASON, None ; "a_failure")]
    fn an_outcome_is_recorded_as_its_decision(
        answer: Result<DelegateOutput, String>,
        action: ActionName,
        reason: &str,
        payload: Option<DecisionPayload>,
    ) {
        let (_, fixture) = scripted(Vec::new());
        let outcome = DelegateOutcome {
            answer,
            model: Some(CHAT_SPEC.into()),
            usage: usage(),
            cost: Some(f64::from(OUTPUT_TOKENS)),
            ..DelegateOutcome::escalated(DecisionKind::Permission, String::new())
        };

        let decision = outcome.decision(SEQ, AT_MS, origin(), None);
        let event = fixture.handle.record(decision.clone());

        assert_eq!(
            (decision.seq, decision.at_ms, decision.kind, decision.action),
            (SEQ, AT_MS, DecisionKind::Permission, action)
        );
        assert_eq!(decision.reason, reason);
        assert_eq!(
            (decision.subject.as_str(), decision.history_len),
            (ITEM, HISTORY_LEN)
        );
        assert_eq!(decision.agent.as_deref(), Some(AGENT));
        assert_eq!(decision.tool_use_id.as_deref(), Some(TOOL_USE_ID));
        assert_eq!(
            (decision.item.as_deref(), decision.engagement),
            (Some(ITEM), Some(ENGAGEMENT))
        );
        assert_eq!(decision.payload, payload);
        assert_eq!(decision.outcome, None);
        assert_eq!(decision.usage.and_then(|usage| usage.cost), outcome.cost);
        assert_eq!(event, AutopilotEvent::Decided(Box::new(decision.clone())));
        assert_eq!(fixture.handle.recent_decisions(), [decision]);
    }
}
