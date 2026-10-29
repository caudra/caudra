//! Asks the engine whether a user message corrects the agent or sets a rule
//! for the project: a sign the agent has something to save as a note.

use serde_json::json;

use super::question_tool_nudge::{excerpt, sole_noul};
use super::{DecisionOutcome, DecisionState, redact_decision_text};

pub(crate) const QUESTION: &str = "durable_correction";
const USER_MESSAGE_BYTES: usize = 640;
const USER_MESSAGE_HEAD_BYTES: usize = 240;
const ASSISTANT_REPLY_BYTES: usize = 640;
const ASSISTANT_REPLY_HEAD_BYTES: usize = 120;

/// The request state, `None` without a message or a reply it answers.
pub(crate) fn state(user_message: &str, assistant_reply: &str) -> Option<DecisionState> {
    if user_message.trim().is_empty() || assistant_reply.trim().is_empty() {
        return None;
    }
    DecisionState::new(&json!({
        "user_message": excerpt(
            &redact_decision_text(user_message),
            USER_MESSAGE_BYTES,
            USER_MESSAGE_HEAD_BYTES,
        ),
        "assistant_reply": excerpt(
            &redact_decision_text(assistant_reply),
            ASSISTANT_REPLY_BYTES,
            ASSISTANT_REPLY_HEAD_BYTES,
        ),
    }))
    .ok()
}

pub(crate) fn probability(outcome: &DecisionOutcome) -> Option<f64> {
    sole_noul(outcome, QUESTION)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::state;
    use crate::decisions::state::MAX_STATE_BYTES;

    const MESSAGE: &str = "No, use make test-fast here.";
    const REPLY: &str = "I ran the full suite.";
    const SECRET: &str = "never-send-this-value";

    #[test_case("", REPLY ; "no_message")]
    #[test_case(MESSAGE, "  \n" ; "no_reply")]
    fn nothing_is_sent_without_both_sides(message: &str, reply: &str) {
        assert!(state(message, reply).is_none());
    }

    #[test]
    fn long_text_is_redacted_and_kept_within_the_state_limit() {
        let filler = SECRET.repeat(MAX_STATE_BYTES);
        let message = format!("{MESSAGE} TOKEN={filler}");
        let state = state(&message, &message).unwrap();
        let encoded = state.value().to_string();

        assert!(encoded.len() <= MAX_STATE_BYTES);
        assert!(!encoded.contains(SECRET));
        assert!(
            state.value()["user_message"]
                .as_str()
                .unwrap()
                .starts_with(MESSAGE)
        );
    }
}
