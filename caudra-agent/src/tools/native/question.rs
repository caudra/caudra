//! `question`: park the run and ask the user.
//!
//! The tool owns the contract, not the widget. It publishes the questions as
//! an event, blocks on the same answer channel re-authentication uses, and
//! turns whatever comes back into an answer per question. The form itself is
//! the UI's, so a headless or ACP front end can satisfy the same request its
//! own way.

use std::borrow::Cow;

use serde_json::Value;

use crate::AgentEvent;
use crate::tools::registry::{
    ExecFuture, HeaderFuture, HeaderResult, ParseError, Tool, ToolExecResult, ToolFailure,
    ToolInvocation,
};
use crate::tools::schema::{ParamKind, ParamSchema, Property, to_json_schema, validate};
use crate::tools::{DescriptionContext, ToolAudience, ToolContext};
use crate::types::{Answer, AskedQuestion, QuestionEvent, QuestionOption, ToolOutput};

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
    }
}

fn parse_option(raw: &Value) -> QuestionOption {
    let field = |name: &str| raw.get(name).and_then(Value::as_str).unwrap_or_default();
    QuestionOption {
        label: field("label").to_owned(),
        description: field("description").to_owned(),
    }
}

struct QuestionCall {
    questions: Vec<AskedQuestion>,
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
            let _ = ctx
                .event_tx
                .send(AgentEvent::Question(Box::new(QuestionEvent {
                    questions: self.questions.clone(),
                })));
            let reply = ctx.cancel.race(guard.recv_async()).await;
            drop(guard);

            let raw = match reply {
                Ok(Ok(raw)) => raw,
                // A cancel and a dropped channel are both "no answer is
                // coming"; the run is ending either way.
                _ => return error(ToolFailure::Cancelled, CANCELLED),
            };
            self.report(&raw)
        })
    }
}

impl QuestionCall {
    /// The front end answers with one label list per question, in order. A
    /// reply that does not parse is a dismissal: the form is gone and there is
    /// nothing left to ask.
    fn report(&self, raw: &str) -> ToolExecResult {
        let Some(answers) = parse_answers(&self.questions, raw) else {
            return ToolExecResult::from(Ok(ToolOutput::Plain(DISMISSED.into())));
        };
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
    Some(
        questions
            .iter()
            .enumerate()
            .map(|(index, question)| Answer {
                header: question.header.clone(),
                labels: picked.get(index).cloned().unwrap_or_default(),
                question: question.question.clone(),
                options: question.options.clone(),
            })
            .collect(),
    )
}

/// The questions sit in the tool input right above this result, and inputs
/// outlive outputs through compaction, so only the picked labels are sent.
fn format_answers(questions: &[AskedQuestion], answers: &[Answer]) -> String {
    answers
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
        .join("\n")
}

fn error(failure: ToolFailure, message: &'static str) -> ToolExecResult {
    ToolExecResult::from(Ok(ToolOutput::Plain(message.into()))).with_failure(failure)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;
    use serde_json::json;
    use test_case::test_case;

    const PICK: &str = "Pick one";
    const HEADER: &str = "Choice";
    const YES: &str = "Yes";
    const NO: &str = "No";
    const CUSTOM: &str = "something else entirely";

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
