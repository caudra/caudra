//! The `question` tool's form, put to the delegate. The tool races the human
//! for the answer, so this module only frames the form and keeps the
//! decision's place in the engagement.

use std::fmt::Write;

use super::AutopilotHandle;
use super::context::{DecisionRequest, clip};
use super::delegate::{DecisionOrigin, DelegateOutcome};
use super::wire::DecisionKind;
use crate::EventSender;
use crate::cancel::CancelToken;
use crate::types::{AskedQuestion, AutopilotEvent};

const MAX_ITEM_BYTES: usize = 32 * 1024;
const MAX_SUBJECT_BYTES: usize = 160;
const FORM_LABEL: &str = "The form the agent put to the user. Answer with one list of labels per \
question, in order. A label may be an offered one or your own words.";
const ONE_LABEL: &str = "one label";
const ANY_LABELS: &str = "one or more labels";

/// Autopilot's part in one `question` form, made for the main agent.
pub struct QuestionDelegate {
    handle: AutopilotHandle,
    seq: u32,
    item: String,
    origin: DecisionOrigin,
}

impl QuestionDelegate {
    /// Takes the decision's place in the engagement. The form is a new one, so
    /// no claim on an earlier form carries over to it.
    pub fn new(
        handle: AutopilotHandle,
        questions: &[AskedQuestion],
        tool_use_id: Option<String>,
    ) -> Self {
        handle.reset_question_claim();
        let origin = DecisionOrigin {
            subject: clip(subject(questions), MAX_SUBJECT_BYTES),
            agent: None,
            tool_use_id,
            history_len: handle.snapshot().map_or(0, |snapshot| snapshot.history_len),
        };
        Self {
            seq: handle.next_seq(),
            item: clip(item(questions), MAX_ITEM_BYTES),
            handle,
            origin,
        }
    }

    pub fn announce(&self, event_tx: &EventSender) {
        event_tx.try_send(AutopilotEvent::Deciding {
            seq: self.seq,
            kind: DecisionKind::Question,
            subject: self.origin.subject.clone(),
        });
    }

    /// `None` when `cancel` fired first. Boxed, because the call sits in the
    /// tool's future and that one in the dispatch path.
    pub async fn decide(
        &self,
        event_tx: &EventSender,
        cancel: &CancelToken,
    ) -> Option<DelegateOutcome> {
        let request = DecisionRequest {
            kind: DecisionKind::Question,
            reserved: false,
            item: &self.item,
        };
        Box::pin(self.handle.decide(&request, event_tx, cancel)).await
    }

    pub fn record(&self, outcome: &DelegateOutcome, event_tx: &EventSender) {
        let decision = outcome.decision(self.seq, self.handle.now_ms(), self.origin.clone());
        event_tx.try_send(self.handle.record(decision));
    }

    /// Takes the form for autopilot, `false` when the human took it first.
    pub fn claim(&self) -> bool {
        self.handle.claim_question()
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

fn item(questions: &[AskedQuestion]) -> String {
    let mut item = FORM_LABEL.to_owned();
    for (number, question) in (1..).zip(questions) {
        let picks = if question.multiple {
            ANY_LABELS
        } else {
            ONE_LABEL
        };
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
    use crate::types::QuestionOption;

    const HEADER: &str = "Cache";
    const QUESTION: &str = "Which store backs the cache?";
    const SECOND_QUESTION: &str = "Which crates get it?";
    const LABEL: &str = "SQLite";
    const DESCRIPTION: &str = "one file, no server";
    const FILLER: &str = "x";
    const NAMED: &str = "a form must be named by its first question";
    const SHOWN: &str = "the delegate must read every question, option and pick count";
    const BOUNDED: &str = "a form must fit the delegate's bound";

    fn asked(header: &str, question: &str, multiple: bool) -> AskedQuestion {
        AskedQuestion {
            question: question.into(),
            header: header.into(),
            options: vec![QuestionOption {
                label: LABEL.into(),
                description: DESCRIPTION.into(),
            }],
            multiple,
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

        let item = item(&questions);

        assert!(item.starts_with(FORM_LABEL), "{SHOWN}");
        for shown in [
            format!("1. {HEADER}: {QUESTION} ({ONE_LABEL})"),
            format!("2. {HEADER}: {SECOND_QUESTION} ({ANY_LABELS})"),
            format!("- {LABEL}: {DESCRIPTION}"),
        ] {
            assert!(item.contains(&shown), "{SHOWN}: {shown}");
        }
    }

    #[test]
    fn a_form_is_bounded() {
        let long = FILLER.repeat(MAX_ITEM_BYTES);
        let handle = AutopilotHandle::default();

        let delegate = QuestionDelegate::new(handle, &[asked(&long, &long, false)], None);

        assert!(delegate.item.len() <= MAX_ITEM_BYTES, "{BOUNDED}");
        assert!(
            delegate.origin.subject.len() <= MAX_SUBJECT_BYTES,
            "{BOUNDED}"
        );
    }
}
