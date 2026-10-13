//! `question`: park the run and ask the user.
//!
//! The tool owns the contract, not the widget. It publishes the questions as
//! an event, blocks on the same answer channel re-authentication uses, and
//! turns whatever comes back into an answer per question. The form itself is
//! the UI's, so a headless or ACP front end can satisfy the same request its
//! own way.

use std::borrow::Cow;

use flume::Receiver;
use futures_lite::future;
use serde_json::Value;
use tracing::debug;

use crate::autopilot::question::QuestionDelegate;
use crate::autopilot::{DelegateAction, DelegateOutcome, DelegateOutput};
use crate::cancel::CancelToken;
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolExecResult, ToolFailure,
    ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{DescriptionContext, ToolAudience, ToolContext};
use crate::types::{Answer, Answerer, AskedQuestion, QuestionEvent, QuestionOption, ToolOutput};
use crate::{AgentEvent, EventSender};

pub const DESCRIPTION: &str = "Use this tool when you need to ask the user questions during execution. This allows you to:
1. Gather user preferences or requirements
2. Clarify ambiguous instructions
3. Get decisions on implementation choices as you work
4. Offer choices to the user about what direction to take.

Usage notes:
- A \"Type your own answer\" option is added automatically; don't include \"Other\" or catch-all options
- Answers are returned as arrays of labels; set `multiSelect: true` to allow selecting more than one
- If you recommend a specific option, make that the first option in the list and add \"(Recommended)\" at the end of the label";

const NO_QUESTIONS: &str = "error: at least one question is required";
const DISMISSED: &str = "(question dismissed by user)";
const NO_ANSWER: &str = "(no answer)";
const NO_UI: &str = "no answer channel: the question tool needs an interactive front end";
const CANCELLED: &str = "cancelled";
const MULTI_SELECT_FIELD: &str = "multiSelect";
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
static PROPERTIES: &[Property] = &[("questions", &QUESTIONS_PARAM, true, &[])];
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
        Ok(Box::new(QuestionCall { questions }))
    }
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

struct QuestionCall {
    questions: Vec<AskedQuestion>,
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
            // The channel is shared with re-authentication, so the lock is
            // taken before the ask goes out: nothing else may consume the
            // reply meant for this form.
            let guard = rx.lock().await;
            let autopilot = ctx.permissions.autopilot();
            if autopilot.is_some() {
                drop_stale_replies(&guard);
            }
            let delegate = autopilot
                .filter(|handle| asked_by_main_agent(ctx) && handle.is_engaged())
                .map(|handle| {
                    QuestionDelegate::new(handle, &self.questions, ctx.tool_use_id.clone())
                });
            let _ = ctx
                .event_tx
                .send(AgentEvent::Question(Box::new(QuestionEvent {
                    questions: self.questions.clone(),
                    source: None,
                })));
            let fit = |picked: &[Vec<String>]| {
                delegated_answers(&self.questions, picked).ok_or_else(|| UNFIT_ANSWER.to_owned())
            };
            let wait = wait_for_answer(&guard, delegate, &ctx.event_tx, &ctx.cancel, fit);
            let reply = ctx.cancel.race(wait).await;
            drop(guard);

            match reply {
                Ok(Some(Reply::Human(raw))) => self.report(&raw),
                Ok(Some(Reply::Autopilot(answers))) => self.answered(answers),
                // A cancel and a dropped channel are both "no answer is
                // coming"; the run is ending either way.
                _ => error(ToolFailure::Cancelled, CANCELLED),
            }
        })
    }
}

/// Autopilot answers only the main agent, the one the user is watching.
pub(crate) fn asked_by_main_agent(ctx: &ToolContext) -> bool {
    ctx.audience.contains(ToolAudience::MAIN) && ctx.root_tool_use_id.is_none()
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
    event_tx: &EventSender,
    cancel: &CancelToken,
    fit: impl FnOnce(&[Vec<String>]) -> Result<T, String>,
) -> Option<Reply<T>> {
    if let Some(delegate) = delegate {
        delegate.announce(event_tx);
        let woken = future::or(
            async { Woken::Human(replies.recv_async().await.ok()) },
            async { Woken::Delegate(delegate.decide(event_tx, cancel).await) },
        )
        .await;
        match woken {
            Woken::Human(reply) => return reply.map(Reply::Human),
            Woken::Delegate(None) => return None,
            Woken::Delegate(Some(outcome)) => {
                if let Some(answer) = conclude(&delegate, outcome, event_tx, fit) {
                    return Some(
                        replies
                            .try_recv()
                            .map_or(Reply::Autopilot(answer), Reply::Human),
                    );
                }
            }
        }
    }
    replies.recv_async().await.ok().map(Reply::Human)
}

/// Takes the delegate's answer when the form accepts it and the human has not
/// taken the form first, and records the decision either way. An answer the
/// form does not accept is recorded as an escalation.
fn conclude<T>(
    delegate: &QuestionDelegate,
    mut outcome: DelegateOutcome,
    event_tx: &EventSender,
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
    let taken = answer.filter(|_| delegate.claim());
    delegate.record(&outcome, event_tx);
    taken
}

impl QuestionCall {
    /// The front end answers with one label list per question, in order. A
    /// reply that does not parse is a dismissal: the form is gone and there is
    /// nothing left to ask.
    fn report(&self, raw: &str) -> ToolExecResult {
        let Some(answers) = parse_answers(&self.questions, raw) else {
            return ToolExecResult::from(Ok(ToolOutput::Plain(DISMISSED.into())));
        };
        self.answered(answers)
    }

    fn answered(&self, answers: Vec<Answer>) -> ToolExecResult {
        let text = format_answers(&self.questions, &answers);
        ToolExecResult {
            model_output: Some(text),
            ..ToolExecResult::from(Ok(ToolOutput::Answers(answers)))
        }
    }
}

/// `None` for a dismissal. The form leaves a hole for every question the user
/// skipped, so the list is padded back out to one entry per question: a caller
/// indexes by question number, never by answer position.
fn parse_answers(questions: &[AskedQuestion], raw: &str) -> Option<Vec<Answer>> {
    let picked: Vec<Vec<String>> = serde_json::from_str(raw).ok()?;
    Some(answers_to(questions, &picked, Answerer::User))
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
    use caudra_storage::autopilot::{AutopilotEnd, AutopilotStopper};
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::autopilot::{
        ActionName, AutopilotDecision, AutopilotHandle, DecisionKind, GatedProvider,
        ScriptedProvider, Step, engaged_handle,
    };
    use crate::permissions::PermissionManager;
    use crate::tools::test_support::{stub_ctx, stub_ctx_with_permissions};
    use crate::types::AutopilotEvent;
    use crate::{AgentMode, Envelope};

    const PICK: &str = "Pick one";
    const HEADER: &str = "Choice";
    const YES: &str = "Yes";
    const NO: &str = "No";
    const CUSTOM: &str = "something else entirely";
    const CWD: &str = "/tmp";
    const TOOL_USE_ID: &str = "toolu_question";
    const PARENT_CALL: &str = "toolu_task";
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

    /// A main-agent question context that answers on `replies` and reports on
    /// `events`, with `autopilot` on its session's permissions.
    struct Asked {
        ctx: ToolContext,
        events: Receiver<Envelope>,
        replies: flume::Sender<String>,
    }

    impl Asked {
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
            ctx.tool_use_id = Some(TOOL_USE_ID.into());
            let (replies, rx) = flume::unbounded();
            ctx.user_response_rx = Some(Arc::new(async_lock::Mutex::new(rx)));
            Self {
                ctx,
                events,
                replies,
            }
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

    fn call() -> Box<QuestionCall> {
        Box::new(QuestionCall {
            questions: vec![question(HEADER, false)],
        })
    }

    fn answers(result: ToolExecResult) -> Vec<Answer> {
        let Ok(ToolOutput::Answers(answers)) = result.output else {
            panic!("the question was answered");
        };
        answers
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
        assert!(handle.question_claimed(), "{AUTOPILOT_WINS}");
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
    }

    #[test]
    fn a_claim_drops_the_delegates_answer_and_still_records_it() {
        let (provider, started, release) = gated();
        let handle = engaged_handle(provider);
        let asked = Asked::new(Some(handle.clone()));

        let (result, ()) = asked.ask_with(async {
            let _running = started.recv_async().await.unwrap();
            assert!(handle.claim_question(), "the human touched the form first");
            release.send(ANSWER_YES).unwrap();
            assert_eq!(asked.decided().await.action, ActionName::Answer);
            asked.reply(&[NO]);
        });

        let answers = answers(result);
        assert_eq!(answers[0].labels, [NO], "{HUMAN_WINS}");
        assert_eq!(answers[0].by, Answerer::User, "{HUMAN_WINS}");
        assert_eq!(handle.recent_decisions().len(), 1);
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
        asked.ctx.root_tool_use_id = parent.map(str::to_owned);

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
        });
        let result = smol::block_on(call.execute(&ctx));
        assert_eq!(result.failure, Some(expected));
    }

    #[test]
    fn answers_reach_the_model_as_text_and_the_reader_as_structure() {
        let call = QuestionCall {
            questions: vec![question(HEADER, false)],
        };
        let result = call.report(&json!([[YES]]).to_string());
        assert_eq!(result.model_output.as_deref(), Some("Choice: Yes"));
        let Ok(ToolOutput::Answers(answers)) = result.output else {
            panic!("the transcript keeps the structured answers");
        };
        assert_eq!(answers[0].labels, [YES]);
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
}
