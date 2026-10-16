//! The `question` tool's form, put to the delegate. The tool races the human
//! for the answer, so this module only frames the form and keeps the
//! decision's place in the engagement.

use std::fmt::Write;

use super::context::{DecisionRequest, clip};
use super::delegate::{DecisionOrigin, DecisionSlot, DelegateOutcome};
use super::wire::DecisionKind;
use super::{AutopilotHandle, DecisionOutcome};
use crate::EventSender;
use crate::cancel::CancelToken;
use crate::mcp::elicitation::describe_answer;
use crate::types::{AskedQuestion, QUESTION_DISMISSED_REPLY, QuestionReply};

const MAX_ITEM_BYTES: usize = 32 * 1024;
const MAX_SUBJECT_BYTES: usize = 160;
const FORM_LABEL: &str = "The form the agent put to the user. Answer with one list of labels per \
question, in order. A label may be an offered one or your own words.";
const SERVER_FORM_LABEL: &str = "A form an MCP server the agent called put to the user. Answer \
with one list of labels per question, in order. A label may be an offered one or your own words.";
const SERVER_PREFIX: &str = "Server: ";
const MESSAGE_PREFIX: &str = "Its message: ";
const ONE_LABEL: &str = "one label";
const ANY_LABELS: &str = "one or more labels";
const MAY_BE_EMPTY: &str = ", or none to leave it unanswered";
const LABEL_SEPARATOR: &str = ", ";
const QUESTION_SEPARATOR: &str = "; ";
const NO_ANSWER: &str = "(no answer)";
const NOTE_SEPARATOR: &str = "; note: ";

/// Autopilot's part in one `question` form, made for the main agent. Dropped
/// before the form is settled, the decision ends as cancelled.
pub struct QuestionDelegate {
    item: String,
    /// The form's claim key: the call that put it up.
    key: String,
    slot: DecisionSlot,
}

impl QuestionDelegate {
    /// Takes the decision's place in the engagement. The form is a new one, so
    /// no claim on an earlier form carries over to it.
    pub fn new(
        handle: AutopilotHandle,
        questions: &[AskedQuestion],
        tool_use_id: Option<String>,
        event_tx: &EventSender,
    ) -> Self {
        Self::framed(
            handle,
            FORM_LABEL.to_owned(),
            questions,
            tool_use_id,
            event_tx,
        )
    }

    /// As [`Self::new`], for a form an MCP server put during a tool call,
    /// framed by the server's name and message.
    pub fn for_server(
        handle: AutopilotHandle,
        server: &str,
        message: &str,
        questions: &[AskedQuestion],
        tool_use_id: Option<String>,
        event_tx: &EventSender,
    ) -> Self {
        let label =
            format!("{SERVER_FORM_LABEL}\n{SERVER_PREFIX}{server}\n{MESSAGE_PREFIX}{message}");
        Self::framed(handle, label, questions, tool_use_id, event_tx)
    }

    fn framed(
        handle: AutopilotHandle,
        label: String,
        questions: &[AskedQuestion],
        tool_use_id: Option<String>,
        event_tx: &EventSender,
    ) -> Self {
        let key = tool_use_id.clone().unwrap_or_default();
        handle.reset_question_claim(&key);
        let item = clip(item(label, questions), MAX_ITEM_BYTES);
        let origin = DecisionOrigin {
            tool_use_id,
            ..DecisionOrigin::new(&handle, clip(subject(questions), MAX_SUBJECT_BYTES), &item)
        };
        Self {
            item,
            key,
            slot: DecisionSlot::new(handle, DecisionKind::Question, origin, event_tx),
        }
    }

    pub fn announce(&self) {
        self.slot.announce();
    }

    /// `None` when `cancel` fired first.
    pub async fn decide(&self, cancel: &CancelToken) -> Option<DelegateOutcome> {
        let request = DecisionRequest {
            kind: DecisionKind::Question,
            reserved: false,
            item: &self.item,
        };
        self.slot.decide(&request, cancel).await
    }

    /// Records the delegate's answer with what became of it.
    pub fn record(&self, outcome: &DelegateOutcome, settled: DecisionOutcome) {
        self.slot.record(outcome, Some(settled));
    }

    /// Records the delegate's answer while the form stays the human's, whose
    /// reply settles it.
    pub fn record_open(&self, outcome: &DelegateOutcome) {
        self.slot.record_open(outcome);
    }

    /// The human replied with `raw`, whatever autopilot made of the form.
    pub fn human_answered(&self, raw: &str) {
        self.slot.human_answered(&human_answer(raw));
    }

    /// Takes the form for autopilot, `false` when the human took it first.
    pub fn claim(&self) -> bool {
        self.slot.handle().claim_question(&self.key)
    }
}

/// A human's reply as the log words it: the labels picked for each question,
/// or the reply itself when it picks none, such as a dismissal, then the note
/// the user wrote for the whole form.
fn human_answer(raw: &str) -> String {
    let QuestionReply { answers, note } = QuestionReply::decode(raw);
    let picked = match answers {
        Some(picked) => picked
            .iter()
            .map(|labels| match labels.as_slice() {
                [] => NO_ANSWER.to_owned(),
                labels => labels.join(LABEL_SEPARATOR),
            })
            .collect::<Vec<_>>()
            .join(QUESTION_SEPARATOR),
        None if note.is_empty() => return raw.to_owned(),
        None => QUESTION_DISMISSED_REPLY.to_owned(),
    };
    match note.is_empty() {
        true => picked,
        false => format!("{picked}{NOTE_SEPARATOR}{note}"),
    }
}

/// The first question's header, or the question itself when it has none.
fn subject(questions: &[AskedQuestion]) -> String {
    questions
        .first()
        .map(|question| match question.header.trim() {
            "" => question.question.trim(),
            header => header,
        })
        .unwrap_or_default()
        .to_owned()
}

/// `item` followed by each question with what its answer has to be: how many
/// labels, and for one that is typed rather than picked, the kind of text and
/// its limits.
fn item(mut item: String, questions: &[AskedQuestion]) -> String {
    for (number, question) in (1..).zip(questions) {
        let mut picks = match describe_answer(&question.answer) {
            Some(kind) => format!("{ONE_LABEL}: {kind}"),
            None if question.multiple => ANY_LABELS.to_owned(),
            None => ONE_LABEL.to_owned(),
        };
        if question.optional {
            picks.push_str(MAY_BE_EMPTY);
        }
        let _ = write!(
            item,
            "\n{number}. {}: {} ({picks})",
            question.header, question.question
        );
        for option in &question.options {
            let _ = write!(item, "\n   - {}: {}", option.label, option.description);
        }
    }
    item
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::Envelope;
    use crate::types::{AgentEvent, AnswerKind, AutopilotEvent, QuestionOption, TextFormat};

    const HEADER: &str = "Cache";
    const QUESTION: &str = "Which store backs the cache?";
    const SECOND_QUESTION: &str = "Which crates get it?";
    const LABEL: &str = "SQLite";
    const DESCRIPTION: &str = "one file, no server";
    const FILLER: &str = "x";
    const SERVER: &str = "deploys";
    const SERVER_MESSAGE: &str = "Which environment should the release go to?";
    const NAMED: &str = "a form must be named by its first question";
    const SHOWN: &str = "the delegate must read every question, option and pick count";
    const BOUNDED: &str = "a form must fit the delegate's bound";
    const DISMISSAL: &str = "(question dismissed)";
    const LOGGED: &str = "a human's reply must be logged as the labels it picked";

    fn asked(header: &str, question: &str, multiple: bool) -> AskedQuestion {
        AskedQuestion {
            question: question.into(),
            header: header.into(),
            options: vec![QuestionOption {
                label: LABEL.into(),
                description: DESCRIPTION.into(),
            }],
            multiple,
            ..AskedQuestion::default()
        }
    }

    #[test_case(HEADER, HEADER ; "by_its_header")]
    #[test_case(" ", QUESTION ; "by_its_question_without_a_header")]
    fn a_form_is_named_by_its_first_question(header: &str, expected: &str) {
        let questions = [
            asked(header, QUESTION, false),
            asked(HEADER, SECOND_QUESTION, true),
        ];

        assert_eq!(subject(&questions), expected, "{NAMED}");
    }

    #[test]
    fn the_item_shows_every_question_option_and_pick_count() {
        let questions = [
            asked(HEADER, QUESTION, false),
            asked(HEADER, SECOND_QUESTION, true),
        ];

        let item = item(FORM_LABEL.to_owned(), &questions);

        assert!(item.starts_with(FORM_LABEL), "{SHOWN}");
        for shown in [
            format!("1. {HEADER}: {QUESTION} ({ONE_LABEL})"),
            format!("2. {HEADER}: {SECOND_QUESTION} ({ANY_LABELS})"),
            format!("- {LABEL}: {DESCRIPTION}"),
        ] {
            assert!(item.contains(&shown), "{SHOWN}: {shown}");
        }
    }

    #[test_case(AnswerKind::Number { integer: true, minimum: Some(1.into()), maximum: Some(9.into()) }, false, "(one label: a whole number from 1 to 9)" ; "a_number_with_its_range")]
    #[test_case(AnswerKind::Text { format: Some(TextFormat::Email), min_length: None, max_length: None }, true, "(one label: an email address, or none to leave it unanswered)" ; "optional_text_with_its_format")]
    #[test_case(AnswerKind::Boolean, false, "(one label: yes or no)" ; "a_boolean")]
    fn a_typed_question_tells_the_delegate_what_it_takes(
        answer: AnswerKind,
        optional: bool,
        expected: &str,
    ) {
        let question = AskedQuestion {
            answer,
            optional,
            ..asked(HEADER, QUESTION, false)
        };

        let item = item(FORM_LABEL.to_owned(), &[question]);

        assert!(
            item.contains(&format!("1. {HEADER}: {QUESTION} {expected}")),
            "{SHOWN}: {item}"
        );
    }

    fn event_sender() -> (EventSender, flume::Receiver<Envelope>) {
        let (raw_tx, events) = flume::unbounded();
        (EventSender::new(raw_tx, 0), events)
    }

    #[test]
    fn a_servers_form_names_the_server_and_its_message() {
        let (event_tx, _events) = event_sender();
        let delegate = QuestionDelegate::for_server(
            AutopilotHandle::default(),
            SERVER,
            SERVER_MESSAGE,
            &[asked(HEADER, QUESTION, false)],
            None,
            &event_tx,
        );

        assert!(delegate.item.starts_with(SERVER_FORM_LABEL), "{SHOWN}");
        for shown in [
            format!("{SERVER_PREFIX}{SERVER}"),
            format!("{MESSAGE_PREFIX}{SERVER_MESSAGE}"),
            format!("1. {HEADER}: {QUESTION} ({ONE_LABEL})"),
        ] {
            assert!(delegate.item.contains(&shown), "{SHOWN}: {shown}");
        }
    }

    #[test]
    fn a_form_is_bounded() {
        let long = FILLER.repeat(MAX_ITEM_BYTES);
        let (event_tx, events) = event_sender();

        let delegate = QuestionDelegate::new(
            AutopilotHandle::default(),
            &[asked(&long, &long, false)],
            None,
            &event_tx,
        );
        delegate.announce();

        assert!(delegate.item.len() <= MAX_ITEM_BYTES, "{BOUNDED}");
        let Ok(Envelope {
            event: AgentEvent::Autopilot(AutopilotEvent::Deciding { subject, .. }),
            ..
        }) = events.try_recv()
        else {
            panic!("an announced form must say it is deciding");
        };
        assert!(subject.len() <= MAX_SUBJECT_BYTES, "{BOUNDED}");
    }

    #[test_case(r#"[["SQLite"],["core","cli"]]"#, "SQLite; core, cli" ; "the_labels_of_each_question")]
    #[test_case(r#"[["SQLite"],[]]"#, "SQLite; (no answer)" ; "a_question_left_unanswered")]
    #[test_case(DISMISSAL, DISMISSAL ; "a_reply_that_picks_none")]
    #[test_case(r#"{"answers":[["SQLite"]],"note":"stay portable"}"#, "SQLite; note: stay portable" ; "the_labels_and_the_note")]
    #[test_case(r#"{"answers":null,"note":"stay portable"}"#, "dismissed; note: stay portable" ; "a_dismissal_and_the_note")]
    fn a_human_reply_is_logged_as_the_labels_it_picked(raw: &str, expected: &str) {
        assert_eq!(human_answer(raw), expected, "{LOGGED}");
    }
}
