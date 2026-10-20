//! The provider's own count of the last request, and the calibration sample
//! two such counts make.

use std::slice;

use caudra_providers::{ContentBlock, Message, Role, TokenScale};

use super::estimate_message_tokens;

/// Above this share of the appended estimate, the model's own turns decide
/// the sample. Their replay is what an estimate knows least about: reasoning
/// travels signed, encrypted, or not at all depending on the provider.
const MAX_ASSISTANT_SHARE: f64 = 0.25;

/// The last request the provider billed, and where the transcript stood when it
/// was sent.
///
/// Auto-compaction and the status bar both need one number for how full the
/// window is, and only the provider can supply it. The estimate is kept for what
/// it is good at — how much the transcript has *grown* since — which needs no
/// tokenizer agreement, only that the same counter answers at both ends.
#[derive(Debug, Clone, Copy)]
pub(super) struct MeasuredContext {
    pub(super) reported: u32,
    pub(super) history_len: usize,
    /// What the next request is compared against to learn the model's
    /// tokenizer. `None` where nothing was recorded at send time.
    pub(super) sent: Option<SentRequest>,
}

/// What a request looked like to the local estimate when it left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SentRequest {
    /// The whole request as o200k counts it, never scaled: calibration
    /// compares raw estimates with billed counts.
    pub(super) estimated: u32,
    /// System prompt, tools, model and options. A change there moves the
    /// billed count by something the appended messages did not cause.
    pub(super) prefix: u64,
    /// Changes when history is rewritten rather than appended to.
    pub(super) epoch: u64,
}

impl MeasuredContext {
    /// `None` once the transcript is shorter than it was when the count was
    /// taken: compaction and rollback replace what was billed, so the anchor no
    /// longer describes anything and would otherwise hold a stale high count.
    pub(super) fn extended_by(self, history: &[Message], scale: TokenScale) -> Option<u32> {
        let appended = history.get(self.history_len..)?;
        Some(
            self.reported
                .saturating_add(scale.apply(estimate_message_tokens(appended))),
        )
    }

    /// The estimated and billed tokens of what was appended between this
    /// request and `later`, when that difference measures the tokenizer and
    /// nothing else: the same prefix, history only grown, and the growth a
    /// tool loop's turns and text results.
    pub(super) fn sample(self, later: Self, history: &[Message]) -> Option<(u32, u32)> {
        let (earlier_sent, later_sent) = (self.sent?, later.sent?);
        if earlier_sent.prefix != later_sent.prefix || earlier_sent.epoch != later_sent.epoch {
            return None;
        }
        let span = history.get(self.history_len..later.history_len)?;
        if span.is_empty() || !span.iter().all(continues_tool_loop) {
            return None;
        }
        let assistant: u32 = span
            .iter()
            .filter(|message| matches!(message.role, Role::Assistant))
            .map(|message| estimate_message_tokens(slice::from_ref(message)))
            .sum();
        if f64::from(assistant) > f64::from(estimate_message_tokens(span)) * MAX_ASSISTANT_SHARE {
            return None;
        }
        let estimated = later_sent.estimated.checked_sub(earlier_sent.estimated)?;
        let reported = later.reported.checked_sub(self.reported)?;
        (estimated > 0 && reported > 0).then_some((estimated, reported))
    }
}

/// A turn of the model, or the text results it asked for. A new prompt is
/// not: providers may drop earlier reasoning once one arrives, and that
/// shrinks the billed count by what the estimate still counts. Images and
/// documents are priced by their own rules, not by a tokenizer.
fn continues_tool_loop(message: &Message) -> bool {
    match message.role {
        Role::Assistant => true,
        Role::User => {
            !message.content.is_empty()
                && message
                    .content
                    .iter()
                    .all(|block| matches!(block, ContentBlock::ToolResult { .. }))
                && message.tool_result_image_owners.is_empty()
                && message.tool_result_documents.is_empty()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use caudra_providers::{ContentBlock, ImageMediaType, ImageSource, Message, Role, TokenScale};
    use serde_json::json;
    use test_case::test_case;

    use super::{MeasuredContext, SentRequest, estimate_message_tokens};

    const PREFIX: u64 = 7;
    const EPOCH: u64 = 3;
    const BASE_ESTIMATE: u32 = 10_000;
    const BASE_REPORTED: u32 = 12_000;
    const GROWTH: u32 = 2_000;
    const BILLED_GROWTH: u32 = 2_400;
    const NO_SAMPLE: &str = "this growth must not make a calibration sample";
    const SAMPLED: &str = "a tool loop's growth is one sample of the tokenizer";

    fn prompt() -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "read the file".into(),
            }],
            ..Default::default()
        }
    }

    fn call(input: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "file_read".into(),
                input: json!({ "path": input }),
                thought_signature: None,
            }],
            ..Default::default()
        }
    }

    fn result(content: String) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content,
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        }
    }

    fn large_result() -> Message {
        result("fn main() { println!(\"hello\"); }\n".repeat(200))
    }

    fn with_image(mut message: Message) -> Message {
        message.content.push(ContentBlock::Image {
            source: ImageSource::new(ImageMediaType::Png, Arc::from("")),
        });
        message
    }

    fn measured(reported: u32, history_len: usize, sent: SentRequest) -> MeasuredContext {
        MeasuredContext {
            reported,
            history_len,
            sent: Some(sent),
        }
    }

    fn sent(estimated: u32, prefix: u64, epoch: u64) -> SentRequest {
        SentRequest {
            estimated,
            prefix,
            epoch,
        }
    }

    fn sample(history: &[Message], later: SentRequest) -> Option<(u32, u32)> {
        let earlier = measured(BASE_REPORTED, 1, sent(BASE_ESTIMATE, PREFIX, EPOCH));
        let later = measured(BASE_REPORTED + BILLED_GROWTH, history.len(), later);
        earlier.sample(later, history)
    }

    #[test]
    fn a_tool_loop_makes_a_sample() {
        let history = [prompt(), call("a"), large_result()];
        assert_eq!(
            sample(&history, sent(BASE_ESTIMATE + GROWTH, PREFIX, EPOCH)),
            Some((GROWTH, BILLED_GROWTH)),
            "{SAMPLED}"
        );
    }

    #[test_case(vec![prompt(), call("a"), large_result(), prompt()], sent(BASE_ESTIMATE + GROWTH, PREFIX, EPOCH) ; "a_new_prompt")]
    #[test_case(vec![prompt(), call("a"), with_image(large_result())], sent(BASE_ESTIMATE + GROWTH, PREFIX, EPOCH) ; "an_image")]
    #[test_case(vec![prompt(), call("a"), large_result()], sent(BASE_ESTIMATE + GROWTH, PREFIX + 1, EPOCH) ; "a_changed_prefix")]
    #[test_case(vec![prompt(), call("a"), large_result()], sent(BASE_ESTIMATE + GROWTH, PREFIX, EPOCH + 1) ; "a_rewritten_history")]
    #[test_case(vec![prompt(), call("a"), result("ok".into())], sent(BASE_ESTIMATE + GROWTH, PREFIX, EPOCH) ; "mostly_the_models_turn")]
    #[test_case(vec![prompt(), call("a"), large_result()], sent(BASE_ESTIMATE - 1, PREFIX, EPOCH) ; "a_shrunk_estimate")]
    #[test_case(vec![prompt()], sent(BASE_ESTIMATE + GROWTH, PREFIX, EPOCH) ; "a_shorter_history")]
    fn other_growth_makes_no_sample(history: Vec<Message>, later: SentRequest) {
        assert_eq!(sample(&history, later), None, "{NO_SAMPLE}");
    }

    #[test]
    fn an_unrecorded_request_makes_no_sample() {
        let history = [prompt(), call("a"), large_result()];
        let earlier = MeasuredContext {
            reported: BASE_REPORTED,
            history_len: 1,
            sent: None,
        };
        let later = measured(
            BASE_REPORTED + BILLED_GROWTH,
            history.len(),
            sent(BASE_ESTIMATE + GROWTH, PREFIX, EPOCH),
        );
        assert_eq!(earlier.sample(later, &history), None, "{NO_SAMPLE}");
    }

    #[test]
    fn growth_since_the_count_is_scaled() {
        let history = [prompt(), call("a"), large_result()];
        let appended = estimate_message_tokens(&history[1..]);
        let anchor = MeasuredContext {
            reported: BASE_REPORTED,
            history_len: 1,
            sent: None,
        };
        assert_eq!(
            anchor.extended_by(&history, TokenScale::new(2.0)),
            Some(BASE_REPORTED + appended * 2)
        );
    }
}
