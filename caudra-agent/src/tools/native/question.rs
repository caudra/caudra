//! `question`: park the run and ask the user.
//!
//! The tool owns the contract, not the widget. It publishes the questions as
//! an event, blocks on the same answer channel re-authentication uses, and
//! turns whatever comes back into an answer per question. The form itself is
//! the UI's, so a headless or ACP front end can satisfy the same request its
//! own way.

use std::borrow::Cow;
use std::sync::Arc;

use caudra_storage::background::{JobOwner, QuestionJobMetadata};
use caudra_storage::tool_ledger::ToolOutcome;
use flume::Receiver;
use futures_lite::future;
use serde_json::Value;
use tracing::{debug, warn};

use crate::autopilot::question::QuestionDelegate;
use crate::autopilot::{
    AutopilotHandle, DecisionOutcome, DelegateAction, DelegateOutcome, DelegateOutput,
};
use crate::background::{JobScope, MAX_DEFERRED_QUESTIONS};
use crate::cancel::CancelToken;
use crate::subagent_history::metadata_task_id;
use crate::tools::QUESTION_TOOL_NAME;
use crate::tools::native::task;
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolExecResult, ToolFailure,
    ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{DescriptionContext, ToolAudience, ToolContext};
use crate::types::{
    Answer, AnswerSheet, Answerer, AskedQuestion, QUESTION_DEFERRED_REPLY, QuestionDelivery,
    QuestionEvent, QuestionOption, QuestionReply, TaskCard, TaskProvenance, ToolDoneEvent,
    ToolOutput,
};
use crate::{AgentEvent, EventSender};

/// The deferral section, from [`DEFERRAL_HEADING`] on, is withdrawn with the
/// `background` parameter wherever a question cannot be deferred.
pub const DESCRIPTION: &str = "Use this tool when you need to ask the user questions during execution. This allows you to:
1. Gather user preferences or requirements
2. Clarify ambiguous instructions
3. Get decisions on implementation choices as you work
4. Offer choices to the user about what direction to take.

Usage notes:
- A \"Type your own answer\" option is added automatically; don't include \"Other\" or catch-all options
- Answers are returned as arrays of labels; set `multiSelect: true` to allow selecting more than one
- If you recommend a specific option, make that the first option in the list and add \"(Recommended)\" at the end of the label

Deferred questions:
- Set `background: true` only for questions whose answer you do not need for your next steps, such as a preference, a name, or a follow-up. The call returns a question ID at once, and the answer arrives automatically as a message, including after you end your turn.
- Keep working on what does not depend on the answer. Never act on a guessed answer; if you must assume one, say so and revisit it when the answer arrives.
- When you cannot continue without an answer, end your turn and tell the user which answer you are waiting for.
- At most 3 deferred questions can be open. Group related questions in one call, and withdraw one you no longer need with `task_control` `cancel`.
- The user may defer a question you asked without `background`; you then get a question ID instead of the answer.";

const DEFERRAL_HEADING: &str = "\n\nDeferred questions:\n";
const BACKGROUND_FIELD: &str = "background";
const NO_QUESTIONS: &str = "error: at least one question is required";
const DISMISSED: &str = "(question dismissed by user)";
/// Heads the note the user wrote for the whole form, under the answers.
const NOTE_LEAD: &str = "Note from the user:";
const NO_ANSWER: &str = "(no answer)";
const NO_UI: &str = "no answer channel: the question tool needs an interactive front end";
const CANCELLED: &str = "cancelled";
const MULTI_SELECT_FIELD: &str = "multiSelect";
const DEFERRAL_UNAVAILABLE: &str = "this question cannot be deferred here: deferred questions are \
turned off (agent.deferred_questions = false) or only the main agent of an interactive session \
can defer one. Ask again without background and wait for the answer";
const DEFERRAL_FULL: &str = "deferred questions are already open. Group related questions in \
one call, withdraw one you no longer need with task_control cancel, or end your turn and tell the \
user which answer you are waiting for";
const NO_CALL_ID: &str = "a deferred question needs the id of the call that asked it";
/// How a delivered deferred question reads: `Question <id>: <kind>.`, then
/// the body.
const DELIVERY_LEAD: &str = "Question ";
const DELIVERY_KIND_SEPARATOR: &str = ": ";
const DELIVERY_KIND_END: &str = ".\n\n";
/// The delivery kind of a question the user, or autopilot, answered.
pub(crate) const ANSWERED_KIND: &str = "answered";
/// The delivery kind of a question withdrawn before it was answered.
pub(crate) const WITHDRAWN_KIND: &str = "withdrawn";
const DEFERRED_CLOSING: &str =
    "The answer to a deferred question: revisit any work you did on a different assumption.";
const WITHDRAWN: &str = "The question was withdrawn before the user answered it.";
const DEFERRED_BY_AGENT: &str = "The question is deferred and waits for the user.";
const DEFERRED_BY_USER: &str =
    "The user chose to answer later, so the question is deferred instead of answered.";
const DEFERRED_GUIDANCE: &str = "The answer arrives automatically as a message, including after \
you end this turn. Keep working on what does not depend on it. If you cannot continue without it, \
end your turn and tell the user which answer you are waiting for. Do not ask it again.";
const LABEL_SEPARATOR: &str = ", ";
const MAX_LABEL_BYTES: usize = 160;
/// Opens the result of a form autopilot answered, so the model never takes
/// the picks for the user's own.
pub const AUTOPILOT_ANSWERED: &str = "Autopilot answered on the user's behalf:";
const UNFIT_ANSWER: &str = "the answer does not fit the form: it needs one list of labels per \
question, each with a label, and a single label where the question takes one";

static LABEL_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Display text (1-5 words, concise)",
};
static OPTION_DESC_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Explanation of choice",
};
static OPTION_PROPERTIES: &[Property] = &[
    ("label", &LABEL_PARAM, true, &[]),
    ("description", &OPTION_DESC_PARAM, true, &[]),
];
static OPTION_SCHEMA: ParamSchema = ParamSchema::Object {
    properties: OPTION_PROPERTIES,
    description: "",
    reject_unknown: false,
};
static QUESTION_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Complete question",
};
static HEADER_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::String,
    description: "Very short label (max 30 chars)",
};
static OPTIONS_PARAM: ParamSchema = ParamSchema::Array {
    items: &OPTION_SCHEMA,
    description: "Available choices",
};
static MULTI_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Bool,
    description: "Allow selecting multiple choices",
};
static QUESTION_PROPERTIES: &[Property] = &[
    ("question", &QUESTION_PARAM, true, &[]),
    ("header", &HEADER_PARAM, true, &[]),
    ("options", &OPTIONS_PARAM, true, &[]),
    (MULTI_SELECT_FIELD, &MULTI_PARAM, false, &["multiple"]),
];
static QUESTION_SCHEMA: ParamSchema = ParamSchema::Object {
    properties: QUESTION_PROPERTIES,
    description: "",
    reject_unknown: false,
};
static QUESTIONS_PARAM: ParamSchema = ParamSchema::Array {
    items: &QUESTION_SCHEMA,
    description: "Questions to ask",
};
static BACKGROUND_PARAM: ParamSchema = ParamSchema::Primitive {
    kind: ParamKind::Bool,
    description: "Ask without waiting: return a question ID now, and the answer arrives automatically later, including after you end your turn. Default false waits for the answer.",
};
static PROPERTIES: &[Property] = &[
    ("questions", &QUESTIONS_PARAM, true, &[]),
    (BACKGROUND_FIELD, &BACKGROUND_PARAM, false, &[]),
];
static SCHEMA: ParamSchema = ParamSchema::Object {
    properties: PROPERTIES,
    description: "",
    reject_unknown: false,
};

pub struct QuestionTool;

impl Tool for QuestionTool {
    fn name(&self) -> &str {
        crate::tools::QUESTION_TOOL_NAME
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
        Cow::Borrowed(DESCRIPTION)
    }

    fn schema(&self) -> Value {
        to_json_schema(&SCHEMA)
    }

    fn audience(&self) -> ToolAudience {
        ToolAudience::MAIN
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let input = validate(&SCHEMA, input.clone())?;
        let questions = asked_questions(&input);
        if questions.is_empty() {
            return Err(ParseError::custom(NO_QUESTIONS));
        }
        let background = input
            .get(BACKGROUND_FIELD)
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(Box::new(QuestionCall {
            questions,
            background,
        }))
    }
}

/// Publishes the `background` parameter and the deferral section only where
/// a question can be deferred: `agent.deferred_questions` is on and the
/// session runs background jobs.
pub fn configure_deferral(tools: &mut Value, available: bool) {
    let Some(definitions) = tools.as_array_mut() else {
        return;
    };
    for definition in definitions {
        if definition.get("name").and_then(Value::as_str) != Some(QUESTION_TOOL_NAME) {
            continue;
        }
        if let Some(Value::String(description)) = definition.get_mut("description") {
            if let Some(offset) = description.find(DEFERRAL_HEADING) {
                description.truncate(offset);
            }
            if available && let Some(offset) = DESCRIPTION.find(DEFERRAL_HEADING) {
                description.push_str(&DESCRIPTION[offset..]);
            }
        }
        if let Some(properties) = definition
            .get_mut("input_schema")
            .and_then(|schema| schema.get_mut("properties"))
            .and_then(Value::as_object_mut)
        {
            match available {
                true => {
                    properties.insert(BACKGROUND_FIELD.into(), to_json_schema(&BACKGROUND_PARAM))
                }
                false => properties.remove(BACKGROUND_FIELD),
            };
        }
    }
}

/// Where this call may be deferred: the main agent of a session that runs
/// background jobs and answers through a front end, with deferral on.
fn deferral(ctx: &ToolContext) -> Option<JobScope> {
    if !ctx.config.deferred_questions
        || !asked_by_main_agent(ctx)
        || ctx.background.is_none()
        || ctx.user_response_rx.is_none()
    {
        return None;
    }
    ctx.job_scope()
        .filter(|scope| *scope.owner() == JobOwner::Main)
}

/// Whether one more deferred question fits under the cap.
fn has_room(ctx: &ToolContext) -> bool {
    ctx.background
        .as_ref()
        .is_some_and(|tasks| tasks.open_questions() < MAX_DEFERRED_QUESTIONS)
}

fn deferral_full() -> String {
    format!("{MAX_DEFERRED_QUESTIONS} {DEFERRAL_FULL}")
}

/// The questions a call put to the user, read back out of its own input.
///
/// A front end restoring a finished call has the input and not the invocation,
/// and the shape of a question is this tool's to say, so it says it here rather
/// than leaving each reader to rediscover the field names.
pub fn asked_questions(input: &Value) -> Vec<AskedQuestion> {
    input
        .get("questions")
        .and_then(Value::as_array)
        .map(|questions| questions.iter().map(parse_question).collect())
        .unwrap_or_default()
}

fn parse_question(raw: &Value) -> AskedQuestion {
    let field = |name: &str| raw.get(name).and_then(Value::as_str).unwrap_or_default();
    AskedQuestion {
        question: field("question").to_owned(),
        header: field("header").to_owned(),
        options: raw
            .get("options")
            .and_then(Value::as_array)
            .map(|options| options.iter().map(parse_option).collect())
            .unwrap_or_default(),
        multiple: raw
            .get(MULTI_SELECT_FIELD)
            .and_then(Value::as_bool)
            .unwrap_or(false),
        ..AskedQuestion::default()
    }
}

fn parse_option(raw: &Value) -> QuestionOption {
    let field = |name: &str| raw.get(name).and_then(Value::as_str).unwrap_or_default();
    QuestionOption {
        label: field("label").to_owned(),
        description: field("description").to_owned(),
    }
}

/// Whether a `question` result is autopilot's answer, which is never the
/// user's own words.
pub fn answered_by_autopilot(result: &str) -> bool {
    result
        .strip_prefix(AUTOPILOT_ANSWERED)
        .is_some_and(|rest| rest.starts_with('\n'))
}

/// What a delivered question observation carries when the user, or
/// autopilot, answered it: the answer text, `None` for any other delivery.
pub(crate) fn deferred_answer(observation: &str) -> Option<&str> {
    let (head, body) = observation
        .strip_prefix(DELIVERY_LEAD)?
        .split_once(DELIVERY_KIND_END)?;
    let (_, kind) = head.rsplit_once(DELIVERY_KIND_SEPARATOR)?;
    let answers = body
        .split_once(DEFERRED_CLOSING)
        .map_or(body, |(answers, _)| answers);
    (kind == ANSWERED_KIND).then_some(answers.trim_end())
}

/// How background delivery words a settled deferred question.
pub(crate) fn delivery_text(task_id: &str, kind: &str, body: &str) -> String {
    format!("{DELIVERY_LEAD}{task_id}{DELIVERY_KIND_SEPARATOR}{kind}{DELIVERY_KIND_END}{body}")
}

/// The question ID a `question` result hands back when it was deferred
/// rather than answered.
pub(crate) fn deferred_receipt(result: &str) -> Option<&str> {
    metadata_task_id(result)
}

struct QuestionCall {
    questions: Vec<AskedQuestion>,
    background: bool,
}

/// What ended the wait for an answer: the human's raw reply, or the
/// delegate's answer as the form took it.
pub(crate) enum Reply<T> {
    Human(String),
    Autopilot(T),
}

enum Woken {
    Human(Option<String>),
    Delegate(Option<DelegateOutcome>),
}

impl ToolInvocation for QuestionCall {
    fn start_header(&self) -> HeaderFuture {
        let n = self.questions.len();
        let plural = if n == 1 { "" } else { "s" };
        HeaderFuture::Ready(HeaderResult::plain(format!("{n} question{plural}")))
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        Box::pin(async move {
            let Some(rx) = ctx.user_response_rx.as_ref() else {
                return error(ToolFailure::Other, NO_UI);
            };
            let deferral = deferral(ctx);
            if self.background {
                let Some(scope) = deferral else {
                    return error(ToolFailure::InvalidInput, DEFERRAL_UNAVAILABLE);
                };
                if !has_room(ctx) {
                    return ToolExecResult::failed(ToolFailure::InvalidInput, deferral_full());
                }
                return self
                    .defer(ctx, scope, DeferredBy::Agent)
                    .await
                    .unwrap_or_else(|message| ToolExecResult::failed(ToolFailure::Other, message));
            }
            let mut deferral = deferral.filter(|_| has_room(ctx));
            let mut delegated = true;
            loop {
                let delivery = match deferral {
                    Some(_) => QuestionDelivery::Deferrable,
                    None => QuestionDelivery::Blocking,
                };
                match self.ask(ctx, rx, delivery, delegated).await {
                    Some(Reply::Human(raw)) if raw == QUESTION_DEFERRED_REPLY => {
                        let Some(scope) = deferral.take() else {
                            return self.report(&raw);
                        };
                        match self.defer(ctx, scope, DeferredBy::User).await {
                            Ok(receipt) => return receipt,
                            // The user is still on the form, so it stays up
                            // for them, now without the way out that failed.
                            Err(error) => {
                                warn!(%error, "could not defer a question the user put off");
                                delegated = false;
                            }
                        }
                    }
                    Some(Reply::Human(raw)) => return self.report(&raw),
                    Some(Reply::Autopilot(answers)) => return self.answered(answers.into()),
                    // A cancel and a dropped channel are both "no answer is
                    // coming"; the run is ending either way.
                    None => return error(ToolFailure::Cancelled, CANCELLED),
                }
            }
        })
    }
}

/// Who put a question off: the agent asking with `background`, or the user
/// from a form the agent waited on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DeferredBy {
    Agent,
    User,
}

impl QuestionCall {
    /// Puts the form up and waits for whoever answers it. The answer channel
    /// is shared with re-authentication, so the lock is taken before the ask
    /// goes out: nothing else may consume the reply meant for this form.
    async fn ask(
        &self,
        ctx: &ToolContext,
        replies: &async_lock::Mutex<Receiver<String>>,
        delivery: QuestionDelivery,
        delegated: bool,
    ) -> Option<Reply<Vec<Answer>>> {
        let guard = replies.lock().await;
        let autopilot = ctx.permissions.autopilot();
        if autopilot.is_some() {
            drop_stale_replies(&guard);
        }
        let delegate = autopilot
            .filter(|handle| delegated && asked_by_main_agent(ctx) && handle.is_engaged())
            .map(|handle| {
                QuestionDelegate::new(
                    handle,
                    &self.questions,
                    ctx.tool_use_id.clone(),
                    &ctx.event_tx,
                )
            });
        let _ = ctx
            .event_tx
            .send(AgentEvent::Question(Box::new(QuestionEvent {
                questions: self.questions.clone(),
                source: None,
                call_id: ctx.tool_use_id.clone(),
                delivery,
            })));
        let fit = |picked: &[Vec<String>]| {
            delegated_answers(&self.questions, picked).ok_or_else(|| UNFIT_ANSWER.to_owned())
        };
        let wait = wait_for_answer(&guard, delegate, &ctx.cancel, fit);
        ctx.cancel.race(wait).await.ok().flatten()
    }

    /// Admits the question as a background job and returns its receipt. The
    /// job puts the form up and waits on a channel of its own, so it never
    /// holds the answer lock other forms wait on.
    async fn defer(
        &self,
        ctx: &ToolContext,
        scope: JobScope,
        by: DeferredBy,
    ) -> Result<ToolExecResult, String> {
        let call_id = ctx.tool_use_id.clone().ok_or(NO_CALL_ID)?;
        let metadata = QuestionJobMetadata {
            call_id: call_id.clone(),
            root_call_id: ctx
                .local_root_tool_use_id
                .clone()
                .unwrap_or_else(|| call_id.clone()),
            label: label(&self.questions),
        };
        // The user put the form off themselves, so it is theirs and not the
        // delegate's to answer, and it waits until they open it again.
        let pending = PendingQuestion {
            questions: self.questions.clone(),
            call_id,
            event_tx: ctx.event_tx.clone(),
            autopilot: ctx
                .permissions
                .autopilot()
                .filter(|handle| by == DeferredBy::Agent && handle.is_engaged()),
            docked: by == DeferredBy::Agent,
        };
        let card = scope
            .admit_question(
                metadata,
                &ctx.subagent_history,
                &ctx.cancel,
                ctx.deadline,
                move |cancel, provenance, answers| pending.wait(cancel, provenance, answers),
            )
            .await?;
        ctx.event_tx
            .try_send(AgentEvent::TaskAdmitted(card.clone()));
        Ok(receipt(card, by))
    }
}

/// A deferred question's job: the form, and the wait for its answer.
struct PendingQuestion {
    questions: Vec<AskedQuestion>,
    call_id: String,
    event_tx: EventSender,
    autopilot: Option<AutopilotHandle>,
    docked: bool,
}

impl PendingQuestion {
    async fn wait(
        self,
        cancel: CancelToken,
        provenance: TaskProvenance,
        answers: Receiver<String>,
    ) -> ToolDoneEvent {
        let task_id = provenance.task_id.clone();
        let event_tx = self.event_tx.clone().with_task(provenance);
        // Admission can fail after the job was set up, and a form nobody
        // will answer must never go up.
        if cancel.is_cancelled() {
            return self.settle(withdrawn());
        }
        let delegate = self.autopilot.clone().map(|handle| {
            QuestionDelegate::new(
                handle,
                &self.questions,
                Some(self.call_id.clone()),
                &event_tx,
            )
        });
        let _ = event_tx.send(AgentEvent::Question(Box::new(QuestionEvent {
            questions: self.questions.clone(),
            source: None,
            call_id: Some(self.call_id.clone()),
            delivery: QuestionDelivery::Deferred {
                task_id,
                docked: self.docked,
            },
        })));
        let fit = |picked: &[Vec<String>]| {
            delegated_answers(&self.questions, picked).ok_or_else(|| UNFIT_ANSWER.to_owned())
        };
        let wait = wait_for_answer(&answers, delegate, &cancel, fit);
        let result = match cancel.race(wait).await.ok().flatten() {
            Some(Reply::Human(raw)) => match human_reply(&self.questions, &raw) {
                HumanReply::Answered(sheet) => deferred_answers(&self.questions, sheet),
                HumanReply::Dismissed { note } => dismissed(&note),
            },
            Some(Reply::Autopilot(answers)) => deferred_answers(&self.questions, answers.into()),
            None => {
                let _ = event_tx.send(AgentEvent::QuestionClosed);
                withdrawn()
            }
        };
        self.settle(result)
    }

    fn settle(self, result: ToolExecResult) -> ToolDoneEvent {
        let mut done = ToolDoneEvent::error(self.call_id, String::new());
        done.tool = Arc::from(QUESTION_TOOL_NAME);
        done.is_error = result.is_error;
        done.output = result
            .output
            .unwrap_or_else(|message| ToolOutput::Plain(message.into()));
        done.model_output = result.model_output;
        done.accounting.outcome = result.failure.map(ToolOutcome::from);
        done
    }
}

fn withdrawn() -> ToolExecResult {
    error(ToolFailure::Cancelled, WITHDRAWN)
}

/// The answers, with each question again: the call that asked may be far
/// back by the time they arrive.
fn deferred_answers(questions: &[AskedQuestion], sheet: AnswerSheet) -> ToolExecResult {
    let lines = questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let header = match question.header.is_empty() {
                false => question.header.clone(),
                true => format!("Q{}", index + 1),
            };
            let picked = sheet
                .answers
                .get(index)
                .map(|answer| answer.labels.join(LABEL_SEPARATOR))
                .filter(|labels| !labels.is_empty())
                .unwrap_or_else(|| NO_ANSWER.to_owned());
            format!("{header} — {}: {picked}", question.question)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let lead = match sheet.answers.iter().all(|answer| answer.by.is_user()) {
        true => String::new(),
        false => format!("{AUTOPILOT_ANSWERED}\n"),
    };
    let body = with_note(format!("{lead}{lines}"), &sheet.note);
    ToolExecResult {
        model_output: Some(format!("{body}\n\n{DEFERRED_CLOSING}")),
        ..ToolExecResult::from(Ok(ToolOutput::Answers(sheet)))
    }
}

/// `text`, and under it the note the user wrote for the whole form, if any.
fn with_note(text: String, note: &str) -> String {
    match note.is_empty() {
        true => text,
        false => format!("{text}\n\n{NOTE_LEAD}\n{note}"),
    }
}

fn dismissed(note: &str) -> ToolExecResult {
    let text = with_note(DISMISSED.to_owned(), note);
    ToolExecResult::from(Ok(ToolOutput::Plain(text.into())))
}

fn receipt(card: TaskCard, by: DeferredBy) -> ToolExecResult {
    let lead = match by {
        DeferredBy::Agent => DEFERRED_BY_AGENT,
        DeferredBy::User => DEFERRED_BY_USER,
    };
    task::receipt_with(card, &format!("{lead} {DEFERRED_GUIDANCE}"))
}

/// What lists and reminders show for a deferred question: its headers.
fn label(questions: &[AskedQuestion]) -> String {
    let label = questions
        .iter()
        .map(|question| match question.header.trim() {
            "" => question.question.trim(),
            header => header,
        })
        .collect::<Vec<_>>()
        .join(LABEL_SEPARATOR);
    label[..label.floor_char_boundary(MAX_LABEL_BYTES)].to_owned()
}

/// Whether the main agent, the one the user is watching, made this call.
/// Dispatch roots every call at its own id, or its batch's, unless an agent
/// above spawned the agent that made it: then the root is that spawning call.
pub(crate) fn asked_by_main_agent(ctx: &ToolContext) -> bool {
    ctx.audience.contains(ToolAudience::MAIN) && ctx.root_tool_use_id == ctx.local_root_tool_use_id
}

/// In a session with autopilot, a front end replies only to a form it shows,
/// and this form is not shown yet. A reply already waiting answered an earlier
/// form, one autopilot answered while the human was still on it, and must not
/// answer this one.
pub(crate) fn drop_stale_replies(replies: &Receiver<String>) {
    let stale = replies.drain().count();
    if stale > 0 {
        debug!(stale, "dropped replies to an earlier question");
    }
}

/// Waits for the human, racing the delegate when autopilot may answer.
/// The human always wins: an answer of theirs ends the delegate's call,
/// and one that lands with the delegate's is taken over it. `fit` turns the
/// delegate's picks into what the form accepts, or says why it does not.
/// `None` when the channel closed or `cancel` fired.
pub(crate) async fn wait_for_answer<T>(
    replies: &Receiver<String>,
    delegate: Option<QuestionDelegate>,
    cancel: &CancelToken,
    fit: impl FnOnce(&[Vec<String>]) -> Result<T, String>,
) -> Option<Reply<T>> {
    let Some(delegate) = delegate else {
        return replies.recv_async().await.ok().map(Reply::Human);
    };
    delegate.announce();
    let woken = future::or(
        async { Woken::Human(replies.recv_async().await.ok()) },
        async { Woken::Delegate(delegate.decide(cancel).await) },
    )
    .await;
    let raw = match woken {
        Woken::Human(reply) => reply?,
        Woken::Delegate(None) => return None,
        Woken::Delegate(Some(mut outcome)) => {
            if let Some(answer) = claimed(&delegate, &mut outcome, fit)
                && replies.is_empty()
            {
                delegate.record(&outcome, DecisionOutcome::Applied);
                return Some(Reply::Autopilot(answer));
            }
            delegate.record_open(&outcome);
            replies.recv_async().await.ok()?
        }
    };
    delegate.human_answered(&raw);
    Some(Reply::Human(raw))
}

/// The delegate's answer as the form takes it, when the form accepts it and
/// the human has not taken the form first. An answer the form does not accept
/// becomes an escalation.
fn claimed<T>(
    delegate: &QuestionDelegate,
    outcome: &mut DelegateOutcome,
    fit: impl FnOnce(&[Vec<String>]) -> Result<T, String>,
) -> Option<T> {
    let answer = match &outcome.answer {
        Ok(DelegateOutput {
            action: DelegateAction::Answer { answers },
            ..
        }) => fit(answers)
            .inspect_err(|unfit| outcome.answer = Err(unfit.clone()))
            .ok(),
        _ => None,
    };
    answer.filter(|_| delegate.claim())
}

impl QuestionCall {
    /// The front end answers with one label list per question, in order. A
    /// reply that does not parse is a dismissal: the form is gone and there is
    /// nothing left to ask.
    fn report(&self, raw: &str) -> ToolExecResult {
        match human_reply(&self.questions, raw) {
            HumanReply::Answered(sheet) => self.answered(sheet),
            HumanReply::Dismissed { note } => dismissed(&note),
        }
    }

    fn answered(&self, sheet: AnswerSheet) -> ToolExecResult {
        let text = with_note(format_answers(&self.questions, &sheet.answers), &sheet.note);
        ToolExecResult {
            model_output: Some(text),
            ..ToolExecResult::from(Ok(ToolOutput::Answers(sheet)))
        }
    }
}

/// What the user sent back for a form, either way with the note they wrote
/// for the whole of it.
enum HumanReply {
    Answered(AnswerSheet),
    Dismissed { note: String },
}

/// The form leaves a hole for every question the user skipped, so the list is
/// padded back out to one entry per question: a caller indexes by question
/// number, never by answer position.
fn human_reply(questions: &[AskedQuestion], raw: &str) -> HumanReply {
    let QuestionReply { answers, note } = QuestionReply::decode(raw);
    match answers {
        Some(picked) => HumanReply::Answered(AnswerSheet {
            answers: answers_to(questions, &picked, Answerer::User),
            note,
        }),
        None => HumanReply::Dismissed { note },
    }
}

/// The delegate's picks as the answers a human reply would make, `None` when
/// the form could not have sent them: one list per question, each with a
/// label, and a single one where the question takes one. A label may be the
/// delegate's own words, as the human's may be.
fn delegated_answers(questions: &[AskedQuestion], picked: &[Vec<String>]) -> Option<Vec<Answer>> {
    let fits = picked.len() == questions.len()
        && questions.iter().zip(picked).all(|(question, labels)| {
            !labels.is_empty()
                && (question.multiple || labels.len() == 1)
                && labels.iter().all(|label| !label.trim().is_empty())
        });
    fits.then(|| answers_to(questions, picked, Answerer::Autopilot))
}

fn answers_to(questions: &[AskedQuestion], picked: &[Vec<String>], by: Answerer) -> Vec<Answer> {
    questions
        .iter()
        .enumerate()
        .map(|(index, question)| Answer {
            header: question.header.clone(),
            labels: picked.get(index).cloned().unwrap_or_default(),
            question: question.question.clone(),
            options: question.options.clone(),
            by,
        })
        .collect()
}

/// The questions sit in the tool input right above this result, and inputs
/// outlive outputs through compaction, so only the picked labels are sent.
fn format_answers(questions: &[AskedQuestion], answers: &[Answer]) -> String {
    let lines = answers
        .iter()
        .enumerate()
        .map(|(index, answer)| {
            let label = match answer.header.is_empty() {
                false => answer.header.clone(),
                true => format!("Q{}", index + 1),
            };
            let picked = if answer.labels.is_empty() {
                NO_ANSWER.to_owned()
            } else {
                answer.labels.join(", ")
            };
            format!("{label}: {picked}")
        })
        .chain(
            // A question the front end never reported still owes the model a
            // line, or the answer list silently shrinks.
            questions
                .iter()
                .skip(answers.len())
                .map(|q| format!("{}: {NO_ANSWER}", q.header)),
        )
        .collect::<Vec<_>>()
        .join("\n");
    if answers.iter().all(|answer| answer.by.is_user()) {
        return lines;
    }
    format!("{AUTOPILOT_ANSWERED}\n{lines}")
}

fn error(failure: ToolFailure, message: &'static str) -> ToolExecResult {
    ToolExecResult::from(Ok(ToolOutput::Plain(message.into()))).with_failure(failure)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use caudra_config::PermissionsConfig;
    use caudra_storage::{
        StateDir,
        autopilot::{AutopilotEnd, AutopilotStopper},
        background::JobKind,
    };
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::autopilot::{
        ActionName, AutopilotDecision, AutopilotHandle, DecisionKind, GatedProvider,
        ScriptedProvider, Step, engaged_handle,
    };
    use crate::background::BackgroundTasks;
    use crate::permissions::PermissionManager;
    use crate::tools::test_support::{stub_ctx, stub_ctx_with_permissions};
    use crate::types::AutopilotEvent;
    use crate::{AgentMode, Envelope, EventSender, StoredSession};

    const PICK: &str = "Pick one";
    const HEADER: &str = "Choice";
    const YES: &str = "Yes";
    const NO: &str = "No";
    const CUSTOM: &str = "something else entirely";
    const CWD: &str = "/tmp";
    const TOOL_USE_ID: &str = "toolu_question";
    const PARENT_CALL: &str = "toolu_task";
    const BATCH_CALL: &str = "toolu_batch";
    const ANSWER_YES: &str =
        r#"{"action":"answer","reason":"the user asked for yes","answers":[["Yes"]]}"#;
    const ANSWER_EACH: &str =
        r#"{"action":"answer","reason":"one for each","answers":[["Yes"],["No"]]}"#;
    const ANSWER_BOTH: &str =
        r#"{"action":"answer","reason":"both apply","answers":[["Yes","No"]]}"#;
    const ANSWER_NOTHING: &str = r#"{"action":"answer","reason":"no idea","answers":[[]]}"#;
    const ANSWER_BLANK: &str = r#"{"action":"answer","reason":"blank","answers":[["  "]]}"#;
    const ESCALATE_REASON: &str = "only the user can choose";
    const ESCALATE: &str = r#"{"action":"escalate","reason":"only the user can choose"}"#;
    const STOPPED: AutopilotEnd = AutopilotEnd::Stopped {
        by: AutopilotStopper::Command,
    };
    const AUTOPILOT_WINS: &str = "the delegate must answer a form the human leaves alone";
    const HUMAN_WINS: &str = "the human's answer must win over the delegate's";
    const CUT_OFF: &str = "the human's answer must end the delegate's call";
    const LEFT_TO_HUMAN: &str = "a form the delegate cannot answer must stay the human's";
    const UNTOUCHED: &str = "without an engaged main autopilot the question must wait as before";
    const STALE: &str = "a late reply to an earlier form must never answer this one";
    const OLD_SHAPE: &str = "a user's answer must keep the shape older sessions stored";
    const TAKEN_OVER: &str = "a human answering first must stand the delegate down";
    const SETTLED_BY_HUMAN: &str = "a recorded decision must settle with the human's answer";
    const TASK_ID: &str = "question-task";
    const MODEL: &str = "test-model";
    const OFFERED: &str = "a form the agent waits on must let the user put it off";
    const FULL: &str = "past the cap a form must not offer a way to put it off";
    const NOT_DEFERRED: &str = "a refused deferral must open no question";
    const CAP_IN_DESCRIPTION: &str = "the description must state the cap the tool enforces";
    const NOTE: &str = "keep the schema portable";
    const NOTE_KEPT: &str = "the note the user wrote must reach the model with the answer";

    /// A session that runs background jobs, where a question can be deferred.
    struct Jobs {
        _temp: TempDir,
        tasks: BackgroundTasks,
    }

    impl Jobs {
        async fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let dir = StateDir::from_path(temp.path().to_owned());
            let mut session = StoredSession::new(MODEL, temp.path().to_str().unwrap());
            session.save(&dir).unwrap();
            let tasks = BackgroundTasks::spawn(dir, session.id).await.unwrap();
            Self { _temp: temp, tasks }
        }
    }

    /// A main-agent question context that answers on `replies` and reports on
    /// `events`, with `autopilot` on its session's permissions.
    struct Asked {
        ctx: ToolContext,
        events: Receiver<Envelope>,
        replies: flume::Sender<String>,
    }

    impl Asked {
        fn deferring(jobs: &Jobs) -> Self {
            let mut asked = Self::new(None);
            asked.ctx.background = Some(jobs.tasks.clone());
            asked
        }

        async fn delivery(&self) -> QuestionDelivery {
            let AgentEvent::Question(event) = self
                .next(|event| matches!(event, AgentEvent::Question(_)))
                .await
            else {
                unreachable!("only a question is waited for");
            };
            event.delivery
        }

        fn new(autopilot: Option<AutopilotHandle>) -> Self {
            let permissions = Arc::new(PermissionManager::new_nonpersistent(
                PermissionsConfig::default(),
                PathBuf::from(CWD),
                Arc::default(),
            ));
            permissions.set_autopilot(autopilot);
            let mut ctx = stub_ctx_with_permissions(&AgentMode::Build, permissions);
            let (raw_tx, events) = flume::unbounded();
            ctx.event_tx = EventSender::new(raw_tx, 0);
            let (replies, rx) = flume::unbounded();
            ctx.user_response_rx = Some(Arc::new(async_lock::Mutex::new(rx)));
            let mut asked = Self {
                ctx,
                events,
                replies,
            };
            asked.call(TOOL_USE_ID);
            asked
        }

        /// Makes the next ask call `id`, rooted the way dispatch roots a
        /// main-agent call: at itself.
        fn call(&mut self, id: &str) {
            self.ctx.tool_use_id = Some(id.into());
            self.ctx.root_tool_use_id = Some(id.into());
            self.ctx.local_root_tool_use_id = Some(id.into());
        }

        fn ask(&self) -> ToolExecResult {
            smol::block_on(call().execute(&self.ctx))
        }

        /// Asks, and runs `human` beside the wait.
        fn ask_with<T>(&self, human: impl Future<Output = T>) -> (ToolExecResult, T) {
            smol::block_on(future::zip(call().execute(&self.ctx), human))
        }

        async fn next(&self, wanted: impl Fn(&AgentEvent) -> bool) -> AgentEvent {
            loop {
                let event = self.events.recv_async().await.unwrap().event;
                if wanted(&event) {
                    return event;
                }
            }
        }

        async fn asked(&self) {
            self.next(|event| matches!(event, AgentEvent::Question(_)))
                .await;
        }

        async fn decided(&self) -> AutopilotDecision {
            let event = self
                .next(|event| matches!(event, AgentEvent::Autopilot(AutopilotEvent::Decided(_))))
                .await;
            let AgentEvent::Autopilot(AutopilotEvent::Decided(decision)) = event else {
                unreachable!("only a decision is waited for");
            };
            *decision
        }

        fn reply(&self, labels: &[&str]) {
            self.replies.send(json!([labels]).to_string()).unwrap();
        }

        fn autopilot_events(&self) -> Vec<AutopilotEvent> {
            self.events
                .drain()
                .filter_map(|envelope| match envelope.event {
                    AgentEvent::Autopilot(event) => Some(event),
                    _ => None,
                })
                .collect()
        }
    }

    fn settled(asked: &Asked) -> Vec<DecisionOutcome> {
        asked
            .autopilot_events()
            .into_iter()
            .filter_map(|event| match event {
                AutopilotEvent::Settled { outcome, .. } => Some(outcome),
                _ => None,
            })
            .collect()
    }

    fn call() -> Box<QuestionCall> {
        Box::new(QuestionCall {
            questions: vec![question(HEADER, false)],
            background: false,
        })
    }

    fn answers(result: ToolExecResult) -> Vec<Answer> {
        let Ok(ToolOutput::Answers(sheet)) = result.output else {
            panic!("the question was answered");
        };
        sheet.answers
    }

    /// `None` for a dismissal.
    fn parse_answers(questions: &[AskedQuestion], raw: &str) -> Option<Vec<Answer>> {
        match human_reply(questions, raw) {
            HumanReply::Answered(sheet) => Some(sheet.answers),
            HumanReply::Dismissed { .. } => None,
        }
    }

    fn gated() -> (
        Arc<GatedProvider>,
        Receiver<Receiver<()>>,
        flume::Sender<&'static str>,
    ) {
        let (started_tx, started) = flume::unbounded();
        let (release, release_rx) = flume::unbounded();
        let provider = Arc::new(GatedProvider {
            started: started_tx,
            release: release_rx,
        });
        (provider, started, release)
    }

    #[test]
    fn the_delegate_answers_a_form_the_human_leaves_alone() {
        let handle = engaged_handle(ScriptedProvider::new(vec![Step::Answer(ANSWER_YES)]));
        let asked = Asked::new(Some(handle.clone()));

        let result = asked.ask();

        assert_eq!(
            result.model_output.as_deref(),
            Some(format!("{AUTOPILOT_ANSWERED}\n{HEADER}: {YES}").as_str()),
            "{AUTOPILOT_WINS}"
        );
        let answers = answers(result);
        assert_eq!(answers[0].labels, [YES], "{AUTOPILOT_WINS}");
        assert_eq!(answers[0].by, Answerer::Autopilot, "{AUTOPILOT_WINS}");
        assert!(handle.question_claimed(TOOL_USE_ID), "{AUTOPILOT_WINS}");
        let events = asked.autopilot_events();
        let [
            AutopilotEvent::Deciding {
                kind: DecisionKind::Question,
                subject,
                ..
            },
            AutopilotEvent::Decided(decision),
        ] = events.as_slice()
        else {
            panic!("a decision must be announced and recorded: {events:?}");
        };
        assert_eq!(subject, HEADER);
        assert_eq!(
            (decision.kind, decision.action),
            (DecisionKind::Question, ActionName::Answer)
        );
        assert_eq!(decision.tool_use_id.as_deref(), Some(TOOL_USE_ID));
        assert_eq!(decision.agent, None);
        assert_eq!(decision.outcome, Some(DecisionOutcome::Applied));
    }

    #[test]
    fn the_humans_answer_wins_and_ends_the_delegates_call() {
        let (provider, started, _release) = gated();
        let handle = engaged_handle(provider);
        let asked = Asked::new(Some(handle.clone()));

        let (result, running) = asked.ask_with(async {
            let running = started.recv_async().await.unwrap();
            asked.reply(&[NO]);
            running
        });

        let answers = answers(result);
        assert_eq!(answers[0].labels, [NO], "{HUMAN_WINS}");
        assert_eq!(answers[0].by, Answerer::User, "{HUMAN_WINS}");
        assert!(running.recv().is_err(), "{CUT_OFF}");
        assert!(handle.recent_decisions().is_empty(), "{CUT_OFF}");
        let events = asked.autopilot_events();
        assert!(
            matches!(
                events.as_slice(),
                [AutopilotEvent::Deciding { .. }, AutopilotEvent::StoodDown(stood_down)]
                    if stood_down.outcome == DecisionOutcome::TakenOver
            ),
            "{TAKEN_OVER}: {events:?}"
        );
    }

    #[test]
    fn a_claim_drops_the_delegates_answer_and_still_records_it() {
        let (provider, started, release) = gated();
        let handle = engaged_handle(provider);
        let asked = Asked::new(Some(handle.clone()));

        let (result, ()) = asked.ask_with(async {
            let _running = started.recv_async().await.unwrap();
            assert!(
                handle.claim_question(TOOL_USE_ID),
                "the human touched the form first"
            );
            release.send(ANSWER_YES).unwrap();
            assert_eq!(asked.decided().await.action, ActionName::Answer);
            asked.reply(&[NO]);
        });

        let answers = answers(result);
        assert_eq!(answers[0].labels, [NO], "{HUMAN_WINS}");
        assert_eq!(answers[0].by, Answerer::User, "{HUMAN_WINS}");
        assert_eq!(handle.recent_decisions().len(), 1);
        assert_eq!(
            settled(&asked),
            [DecisionOutcome::HumanFirst { answer: NO.into() }],
            "{SETTLED_BY_HUMAN}"
        );
    }

    #[test_case(ANSWER_EACH, UNFIT_ANSWER ; "an_answer_for_a_question_not_asked")]
    #[test_case(ANSWER_BOTH, UNFIT_ANSWER ; "two_labels_for_a_single_choice")]
    #[test_case(ANSWER_NOTHING, UNFIT_ANSWER ; "a_question_left_without_a_label")]
    #[test_case(ANSWER_BLANK, UNFIT_ANSWER ; "a_blank_label")]
    #[test_case(ESCALATE, ESCALATE_REASON ; "an_escalation")]
    fn a_form_the_delegate_cannot_answer_stays_the_humans(reply: &'static str, reason: &str) {
        let handle = engaged_handle(ScriptedProvider::new(vec![Step::Answer(reply)]));
        let asked = Asked::new(Some(handle));

        let (result, ()) = asked.ask_with(async {
            let decision = asked.decided().await;
            assert_eq!(decision.action, ActionName::Escalate, "{LEFT_TO_HUMAN}");
            assert_eq!(decision.reason, reason, "{LEFT_TO_HUMAN}");
            asked.reply(&[NO]);
        });

        assert_eq!(
            result.model_output.as_deref(),
            Some(format!("{HEADER}: {NO}").as_str()),
            "{LEFT_TO_HUMAN}"
        );
        assert_eq!(answers(result)[0].by, Answerer::User, "{LEFT_TO_HUMAN}");
        assert_eq!(
            settled(&asked),
            [DecisionOutcome::Answered { answer: NO.into() }],
            "{SETTLED_BY_HUMAN}"
        );
    }

    #[test_case(None, None ; "without_autopilot")]
    #[test_case(Some(false), None ; "with_autopilot_disengaged")]
    #[test_case(Some(true), Some(PARENT_CALL) ; "for_a_subagent")]
    fn only_an_engaged_main_autopilot_takes_part(engaged: Option<bool>, parent: Option<&str>) {
        let provider = ScriptedProvider::new(Vec::new());
        let handle = engaged.map(|engaged| {
            let handle = engaged_handle(provider.clone());
            if !engaged {
                handle.end(STOPPED);
            }
            handle
        });
        let mut asked = Asked::new(handle);
        if let Some(parent) = parent {
            asked.ctx.root_tool_use_id = Some(parent.to_owned());
        }

        let (result, ()) = asked.ask_with(async {
            asked.asked().await;
            asked.reply(&[YES]);
        });

        assert_eq!(
            result.model_output.as_deref(),
            Some(format!("{HEADER}: {YES}").as_str()),
            "{UNTOUCHED}"
        );
        assert_eq!(answers(result)[0].by, Answerer::User, "{UNTOUCHED}");
        assert!(asked.autopilot_events().is_empty(), "{UNTOUCHED}");
        assert!(provider.prompts().is_empty(), "{UNTOUCHED}");
    }

    #[test]
    fn a_late_reply_to_a_form_autopilot_answered_never_answers_the_next() {
        let handle = engaged_handle(ScriptedProvider::new(vec![Step::Answer(ANSWER_YES)]));
        let asked = Asked::new(Some(handle.clone()));
        assert_eq!(answers(asked.ask())[0].by, Answerer::Autopilot);
        asked.reply(&[NO]);
        handle.end(STOPPED);
        asked.events.drain().for_each(drop);

        let (result, ()) = asked.ask_with(async {
            asked.asked().await;
            asked.reply(&[CUSTOM]);
        });

        assert_eq!(answers(result)[0].labels, [CUSTOM], "{STALE}");
    }

    #[test]
    fn without_autopilot_a_reply_sent_ahead_still_answers() {
        let asked = Asked::new(None);
        asked.reply(&[YES]);

        assert_eq!(answers(asked.ask())[0].labels, [YES], "{UNTOUCHED}");
    }

    #[test_case(&[YES], true ; "an_offered_label")]
    #[test_case(&[CUSTOM], true ; "the_delegates_own_words")]
    #[test_case(&[YES, NO], false ; "two_labels_for_one_choice")]
    #[test_case(&[], false ; "no_label")]
    fn the_delegates_picks_fit_the_form_as_a_humans_would(labels: &[&str], fits: bool) {
        let picked: Vec<Vec<String>> =
            vec![labels.iter().map(|label| (*label).to_owned()).collect()];

        let answers = delegated_answers(&[question(HEADER, false)], &picked);

        assert_eq!(answers.is_some(), fits);
    }

    #[test]
    fn several_labels_fit_a_multi_select() {
        let picked = vec![vec![YES.to_owned(), NO.to_owned()]];

        assert!(delegated_answers(&[question(HEADER, true)], &picked).is_some());
    }

    #[test]
    fn a_users_answer_keeps_its_stored_shape_and_autopilots_says_so() {
        let user = parse_answers(&[question(HEADER, false)], &json!([[YES]]).to_string()).unwrap();
        let autopilot =
            delegated_answers(&[question(HEADER, false)], &[vec![YES.to_owned()]]).unwrap();

        let stored_user = serde_json::to_value(&user).unwrap();
        let stored_autopilot = serde_json::to_value(&autopilot).unwrap();

        assert_eq!(
            stored_user,
            json!([{ "header": HEADER, "labels": [YES] }]),
            "{OLD_SHAPE}"
        );
        let restored: Vec<Answer> = serde_json::from_value(stored_user).unwrap();
        assert_eq!(restored[0].by, Answerer::User, "{OLD_SHAPE}");
        let restored: Vec<Answer> = serde_json::from_value(stored_autopilot).unwrap();
        assert_eq!(restored[0].by, Answerer::Autopilot);
    }

    #[test]
    fn only_a_result_autopilot_answered_reads_as_its() {
        let questions = [question(AUTOPILOT_ANSWERED, false)];
        let user = parse_answers(&questions, &json!([[YES]]).to_string()).unwrap();
        let autopilot = delegated_answers(&questions, &[vec![YES.to_owned()]]).unwrap();

        assert!(!answered_by_autopilot(&format_answers(&questions, &user)));
        assert!(answered_by_autopilot(&format_answers(
            &questions, &autopilot
        )));
    }

    fn question(header: &str, multiple: bool) -> AskedQuestion {
        AskedQuestion {
            question: PICK.into(),
            header: header.into(),
            options: vec![
                QuestionOption {
                    label: YES.into(),
                    description: "affirmative".into(),
                },
                QuestionOption {
                    label: NO.into(),
                    description: "negative".into(),
                },
            ],
            multiple,
            ..AskedQuestion::default()
        }
    }

    fn input(questions: Value) -> Value {
        json!({ "questions": questions })
    }

    fn one_question() -> Value {
        json!([{
            "question": PICK,
            "header": HEADER,
            "options": [{ "label": YES, "description": "affirmative" }],
        }])
    }

    fn parsed(value: Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        QuestionTool.parse(&value)
    }

    #[test]
    fn a_question_round_trips_into_typed_form() {
        let raw = json!({
            "question": PICK,
            "header": HEADER,
            "options": [{ "label": YES, "description": "affirmative" }],
            MULTI_SELECT_FIELD: true,
        });
        let parsed = parse_question(&raw);
        assert_eq!(parsed.question, PICK);
        assert_eq!(parsed.header, HEADER);
        assert_eq!(parsed.options[0].label, YES);
        assert!(parsed.multiple);
    }

    #[test]
    fn multi_select_defaults_to_a_single_answer() {
        assert!(!parse_question(&one_question()[0]).multiple);
    }

    #[test]
    fn the_alias_the_model_actually_sends_is_accepted() {
        let mut raw = one_question();
        raw[0]["multiple"] = json!(true);
        assert!(
            parsed(input(raw)).is_ok(),
            "multiple is an alias for multiSelect"
        );
    }

    #[test]
    fn an_empty_question_list_is_refused() {
        assert!(parsed(input(json!([]))).is_err());
    }

    #[test]
    fn the_header_counts_the_questions() {
        let two = json!([one_question()[0].clone(), one_question()[0].clone()]);
        let HeaderFuture::Ready(header) = parsed(input(two)).unwrap().start_header() else {
            panic!("the header needs no context");
        };
        assert_eq!(header.text(), "2 questions");
    }

    #[test]
    fn one_question_is_not_pluralised() {
        let HeaderFuture::Ready(header) = parsed(input(one_question())).unwrap().start_header()
        else {
            panic!("the header needs no context");
        };
        assert_eq!(header.text(), "1 question");
    }

    #[test]
    fn every_question_gets_an_answer_even_when_the_user_skipped_it() {
        let questions = vec![question(HEADER, false), question("Second", false)];
        let answers = parse_answers(&questions, &json!([[YES]]).to_string()).unwrap();
        assert_eq!(answers.len(), 2, "a skipped question still holds its place");
        assert_eq!(answers[0].labels, [YES]);
        assert!(answers[1].labels.is_empty());
    }

    #[test]
    fn a_custom_answer_comes_back_verbatim() {
        let questions = vec![question(HEADER, false)];
        let answers = parse_answers(&questions, &json!([[CUSTOM]]).to_string()).unwrap();
        assert_eq!(answers[0].labels, [CUSTOM]);
    }

    #[test]
    fn several_picks_survive_a_multi_select() {
        let questions = vec![question(HEADER, true)];
        let answers = parse_answers(&questions, &json!([[YES, NO]]).to_string()).unwrap();
        assert_eq!(answers[0].labels, [YES, NO]);
    }

    #[test]
    fn a_reply_that_is_not_an_answer_list_reads_as_a_dismissal() {
        assert!(parse_answers(&[question(HEADER, false)], "not json").is_none());
    }

    #[test]
    fn the_model_is_told_which_question_each_pick_belongs_to() {
        let questions = vec![question(HEADER, false), question("Second", false)];
        let answers = parse_answers(&questions, &json!([[YES], []]).to_string()).unwrap();
        assert_eq!(
            format_answers(&questions, &answers),
            format!("{HEADER}: {YES}\nSecond: {NO_ANSWER}")
        );
    }

    #[test]
    fn a_headerless_question_is_numbered_instead() {
        let questions = vec![question("", false)];
        let answers = parse_answers(&questions, &json!([[YES]]).to_string()).unwrap();
        assert_eq!(format_answers(&questions, &answers), format!("Q1: {YES}"));
    }

    #[test]
    fn a_dismissal_is_not_an_error_the_model_should_retry() {
        let call = QuestionCall {
            questions: vec![question(HEADER, false)],
            background: false,
        };
        let result = call.report("not json");
        assert!(!result.is_error, "the user chose not to answer");
        assert_eq!(
            result.output.expect("output").as_text(),
            DISMISSED,
            "the model is told the form was dismissed"
        );
    }

    #[test_case(false, ToolFailure::Other ; "without_a_front_end")]
    #[test_case(true, ToolFailure::Cancelled ; "when_the_answer_channel_closes")]
    fn an_unanswered_question_states_why(interactive: bool, expected: ToolFailure) {
        let mut ctx = stub_ctx(&AgentMode::Build);
        let (_, response_rx) = flume::unbounded::<String>();
        ctx.user_response_rx = interactive.then(|| Arc::new(async_lock::Mutex::new(response_rx)));
        let call = Box::new(QuestionCall {
            questions: vec![question(HEADER, false)],
            background: false,
        });
        let result = smol::block_on(call.execute(&ctx));
        assert_eq!(result.failure, Some(expected));
    }

    #[test]
    fn answers_reach_the_model_as_text_and_the_reader_as_structure() {
        let call = QuestionCall {
            questions: vec![question(HEADER, false)],
            background: false,
        };
        let result = call.report(&json!([[YES]]).to_string());
        assert_eq!(result.model_output.as_deref(), Some("Choice: Yes"));
        let Ok(ToolOutput::Answers(sheet)) = result.output else {
            panic!("the transcript keeps the structured answers");
        };
        assert_eq!(sheet.answers[0].labels, [YES]);
    }

    fn noted(answers: Option<Vec<Vec<String>>>) -> String {
        QuestionReply {
            answers,
            note: NOTE.into(),
        }
        .encode()
    }

    #[test]
    fn a_note_reaches_the_model_under_the_answers_and_the_card_with_them() {
        let result = call().report(&noted(Some(vec![vec![YES.into()]])));

        assert_eq!(
            result.model_output.as_deref(),
            Some(format!("{HEADER}: {YES}\n\n{NOTE_LEAD}\n{NOTE}").as_str())
        );
        let Ok(ToolOutput::Answers(sheet)) = result.output else {
            panic!("the transcript keeps the structured answers");
        };
        assert_eq!(sheet.answers[0].labels, [YES]);
        assert_eq!(sheet.note, NOTE, "{NOTE_KEPT}");
    }

    #[test]
    fn a_note_with_a_dismissal_reaches_the_model() {
        let result = call().report(&noted(None));

        assert!(!result.is_error, "the user chose not to answer");
        assert_eq!(
            result.output.expect("output").as_text(),
            format!("{DISMISSED}\n\n{NOTE_LEAD}\n{NOTE}"),
            "{NOTE_KEPT}"
        );
    }

    #[test]
    fn autopilots_answer_carries_no_note() {
        let autopilot = delegated_answers(&[question(HEADER, false)], &[vec![YES.to_owned()]]);

        let result = call().answered(autopilot.unwrap().into());

        let Ok(ToolOutput::Answers(sheet)) = result.output else {
            panic!("the transcript keeps the structured answers");
        };
        assert!(sheet.note.is_empty());
        assert!(!result.model_output.unwrap().contains(NOTE_LEAD));
    }

    /// The card redraws the form, so the answer has to carry what the pick was
    /// made against and not just the pick.
    #[test]
    fn an_answer_carries_the_question_it_answers() {
        let questions = vec![question(HEADER, false)];
        let answers = parse_answers(&questions, &json!([[YES]]).to_string()).unwrap();
        assert_eq!(answers[0].question, PICK);
        assert_eq!(answers[0].options, questions[0].options);
    }

    /// The questions sit in the input right above the result and inputs outlive
    /// outputs through compaction, so widening what the reader sees must not
    /// widen what the model is sent.
    #[test]
    fn the_model_is_still_sent_the_picks_alone() {
        let questions = vec![question(HEADER, false)];
        let answers = parse_answers(&questions, &json!([[YES]]).to_string()).unwrap();
        let text = format_answers(&questions, &answers);
        assert_eq!(text, format!("{HEADER}: {YES}"));
        assert!(
            !text.contains(PICK),
            "the question text stays out of the text"
        );
        assert!(
            !text.contains(NO),
            "a declined option stays out of the text"
        );
    }

    #[test]
    fn questions_are_read_back_out_of_a_stored_input() {
        let restored = asked_questions(&input(one_question()));
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].question, PICK);
        assert_eq!(restored[0].options[0].label, YES);
    }

    #[test]
    fn an_input_without_questions_reads_back_as_none() {
        assert!(asked_questions(&json!({})).is_empty());
    }

    fn deferred_call() -> Box<QuestionCall> {
        Box::new(QuestionCall {
            background: true,
            ..*call()
        })
    }

    fn text(result: ToolExecResult) -> String {
        match result.output {
            Ok(output) => output.as_text().to_string(),
            Err(message) => message,
        }
    }

    /// The question ID a deferral handed back, after checking it reads as one.
    fn receipt_task(result: &ToolExecResult, lead: &str) -> String {
        let Ok(ToolOutput::Tasks(cards)) = &result.output else {
            panic!("a deferred question returns its receipt");
        };
        assert_eq!(cards[0].kind, JobKind::Question);
        let guidance = result.model_suffix.as_deref().unwrap_or_default();
        assert!(guidance.contains(lead), "{guidance}");
        cards[0].task_id.clone()
    }

    #[test]
    fn a_background_ask_returns_at_once_and_docks_its_form() {
        smol::block_on(async {
            let jobs = Jobs::new().await;
            let asked = Asked::deferring(&jobs);

            let result = deferred_call().execute(&asked.ctx).await;

            let task_id = receipt_task(&result, DEFERRED_BY_AGENT);
            assert_eq!(
                asked.delivery().await,
                QuestionDelivery::Deferred {
                    task_id: task_id.clone(),
                    docked: true
                }
            );
            assert_eq!(jobs.tasks.open_questions(), 1);
            jobs.tasks
                .answer_question(&task_id, json!([[YES]]).to_string())
                .unwrap();
            jobs.tasks.shutdown().await.unwrap();
        });
    }

    #[test]
    fn a_form_the_user_puts_off_returns_a_receipt_and_waits_undocked() {
        smol::block_on(async {
            let jobs = Jobs::new().await;
            let asked = Asked::deferring(&jobs);

            let (result, deferred) = future::zip(call().execute(&asked.ctx), async {
                assert_eq!(
                    asked.delivery().await,
                    QuestionDelivery::Deferrable,
                    "{OFFERED}"
                );
                asked.replies.send(QUESTION_DEFERRED_REPLY.into()).unwrap();
                asked.delivery().await
            })
            .await;

            let task_id = receipt_task(&result, DEFERRED_BY_USER);
            assert_eq!(
                deferred,
                QuestionDelivery::Deferred {
                    task_id,
                    docked: false
                }
            );
            jobs.tasks.shutdown().await.unwrap();
        });
    }

    #[test]
    fn past_the_cap_a_question_can_neither_be_deferred_nor_put_off() {
        smol::block_on(async {
            let jobs = Jobs::new().await;
            let mut asked = Asked::deferring(&jobs);
            for index in 0..MAX_DEFERRED_QUESTIONS {
                asked.call(&format!("{TOOL_USE_ID}-{index}"));
                assert!(!deferred_call().execute(&asked.ctx).await.is_error);
            }
            asked.call(TOOL_USE_ID);

            let refused = deferred_call().execute(&asked.ctx).await;
            let (waited, ()) = future::zip(call().execute(&asked.ctx), async {
                while asked.delivery().await != QuestionDelivery::Blocking {}
                asked.reply(&[YES]);
            })
            .await;

            assert_eq!(refused.failure, Some(ToolFailure::InvalidInput));
            assert_eq!(text(refused), deferral_full());
            assert_eq!(jobs.tasks.open_questions(), MAX_DEFERRED_QUESTIONS);
            assert_eq!(answers(waited)[0].labels, [YES], "{FULL}");
            jobs.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(false, true, None ; "without_background_jobs")]
    #[test_case(true, false, None ; "with_deferral_turned_off")]
    #[test_case(true, true, Some(PARENT_CALL) ; "for_a_subagent")]
    fn a_background_ask_where_none_can_be_deferred_is_refused(
        with_jobs: bool,
        enabled: bool,
        parent: Option<&str>,
    ) {
        smol::block_on(async {
            let jobs = Jobs::new().await;
            let mut asked = Asked::new(None);
            asked.ctx.background = with_jobs.then(|| jobs.tasks.clone());
            asked.ctx.config.deferred_questions = enabled;
            if let Some(parent) = parent {
                asked.ctx.root_tool_use_id = Some(parent.to_owned());
            }

            let result = deferred_call().execute(&asked.ctx).await;

            assert_eq!(result.failure, Some(ToolFailure::InvalidInput));
            assert_eq!(text(result), DEFERRAL_UNAVAILABLE);
            assert_eq!(jobs.tasks.open_questions(), 0, "{NOT_DEFERRED}");
            jobs.tasks.shutdown().await.unwrap();
        });
    }

    #[test_case(TOOL_USE_ID, TOOL_USE_ID, true ; "a_main_agent_call_rooted_at_itself")]
    #[test_case(BATCH_CALL, BATCH_CALL, true ; "a_main_agent_call_in_a_batch")]
    #[test_case(PARENT_CALL, TOOL_USE_ID, false ; "a_subagent_call")]
    #[test_case(PARENT_CALL, BATCH_CALL, false ; "a_subagent_call_in_a_batch")]
    fn the_main_agent_is_told_apart_by_how_dispatch_roots_its_calls(
        root: &str,
        local_root: &str,
        main: bool,
    ) {
        let mut asked = Asked::new(None);
        asked.ctx.root_tool_use_id = Some(root.into());
        asked.ctx.local_root_tool_use_id = Some(local_root.into());

        assert_eq!(asked_by_main_agent(&asked.ctx), main);
    }

    #[test]
    fn a_put_off_reply_to_a_form_that_offered_none_reads_as_a_dismissal() {
        let asked = Asked::new(None);
        asked.replies.send(QUESTION_DEFERRED_REPLY.into()).unwrap();

        assert_eq!(text(asked.ask()), DISMISSED);
    }

    #[test_case(true ; "where_a_question_can_be_deferred")]
    #[test_case(false ; "where_none_can")]
    fn deferral_is_published_only_where_available(available: bool) {
        let mut tools = json!([{
            "name": QUESTION_TOOL_NAME,
            "description": DESCRIPTION,
            "input_schema": QuestionTool.schema(),
        }]);

        configure_deferral(&mut tools, !available);
        configure_deferral(&mut tools, available);

        let description = tools[0]["description"].as_str().unwrap();
        assert_eq!(
            description.matches(DEFERRAL_HEADING).count(),
            usize::from(available)
        );
        assert_eq!(
            tools[0]["input_schema"]["properties"]
                .get(BACKGROUND_FIELD)
                .is_some(),
            available
        );
    }

    #[test]
    fn the_description_states_the_enforced_cap() {
        assert!(
            DESCRIPTION.contains(&format!("At most {MAX_DEFERRED_QUESTIONS} deferred")),
            "{CAP_IN_DESCRIPTION}"
        );
    }

    #[test]
    fn a_delivered_answer_reads_back_without_its_closing() {
        let questions = [question(HEADER, false)];
        let answers = parse_answers(&questions, &json!([[YES]]).to_string()).unwrap();
        let body = deferred_answers(&questions, answers.into())
            .model_output
            .unwrap();

        let answered = delivery_text(TASK_ID, ANSWERED_KIND, &body);
        let withdrawn = delivery_text(TASK_ID, WITHDRAWN_KIND, WITHDRAWN);

        assert_eq!(
            deferred_answer(&answered),
            Some(format!("{HEADER} — {PICK}: {YES}").as_str())
        );
        assert_eq!(deferred_answer(&withdrawn), None);
    }

    #[test]
    fn a_delivered_note_reads_back_as_the_users_and_the_closing_stays_last() {
        let questions = [question(HEADER, false)];
        let HumanReply::Answered(sheet) =
            human_reply(&questions, &noted(Some(vec![vec![YES.into()]])))
        else {
            panic!("the noted reply answers the form");
        };
        let body = deferred_answers(&questions, sheet).model_output.unwrap();

        let answered = delivery_text(TASK_ID, ANSWERED_KIND, &body);

        assert!(body.ends_with(DEFERRED_CLOSING));
        assert_eq!(
            deferred_answer(&answered),
            Some(format!("{HEADER} — {PICK}: {YES}\n\n{NOTE_LEAD}\n{NOTE}").as_str()),
            "{NOTE_KEPT}"
        );
    }

    #[test]
    fn a_label_names_each_question_and_stays_bounded() {
        let named = label(&[question(HEADER, false), question("", false)]);
        let long = label(&[question(&"é".repeat(MAX_LABEL_BYTES), false)]);

        assert_eq!(named, format!("{HEADER}{LABEL_SEPARATOR}{PICK}"));
        assert!(long.len() <= MAX_LABEL_BYTES);
    }
}
