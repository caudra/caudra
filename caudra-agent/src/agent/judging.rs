//! Shared by the side models that judge the conversation without continuing
//! it, the goal evaluator and the autopilot delegate: the transcript with its
//! tool traffic flattened to text and cut to a budget, and the one JSON object
//! they answer with.

use std::collections::HashMap;

use caudra_providers::{ContentBlock, Message, Role};
use serde_json::Value;

pub(crate) const TOOL_CALL_LINE: &str = "[tool call]";
pub(crate) const TOOL_RESULT_LINE: &str = "[tool result]";
pub(crate) const TOOL_ERROR_LINE: &str = "[tool error]";
pub(crate) const UNNAMED_TOOL: &str = "tool";
pub(crate) const ONE_JSON_OBJECT_ERROR: &str = "response must contain exactly one JSON object";
/// The share of a judge's context window its transcript may fill, and the
/// smaller share it retries with once the provider reports an overflow.
pub(crate) const INITIAL_TRANSCRIPT_PERCENT: u32 = 50;
pub(crate) const RETRY_TRANSCRIPT_PERCENT: u32 = 25;
const MAX_ECHOED_OUTPUT_CHARS: usize = 2_000;

/// A tool block as [`flatten_tool_blocks`] hands it to its renderer.
pub(crate) enum ToolBlock<'a> {
    Call {
        name: &'a str,
        input: &'a Value,
    },
    /// `name` is [`UNNAMED_TOOL`] once the owning call is truncated away.
    Result {
        name: &'a str,
        content: &'a str,
        is_error: bool,
    },
}

pub(crate) fn result_line(is_error: bool) -> &'static str {
    if is_error {
        TOOL_ERROR_LINE
    } else {
        TOOL_RESULT_LINE
    }
}

/// Replaces every tool block with the text `render` writes for it, and leaves
/// every other block alone.
///
/// Message count is preserved, which an omission notice counts on, and no tool
/// pair survives for [`truncate_oldest_round`] to orphan.
pub(crate) fn flatten_tool_blocks(
    messages: &mut [Message],
    render: impl Fn(ToolBlock<'_>) -> String,
) {
    let names: HashMap<String, String> = messages
        .iter()
        .flat_map(|message| message.tool_uses())
        .map(|(id, name, _)| (id.to_owned(), name.to_owned()))
        .collect();

    for message in messages {
        for block in &mut message.content {
            let text = match &*block {
                ContentBlock::ToolUse { name, input, .. } => {
                    render(ToolBlock::Call { name, input })
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } => render(ToolBlock::Result {
                    name: names.get(tool_use_id).map_or(UNNAMED_TOOL, String::as_str),
                    content,
                    is_error: *is_error,
                }),
                _ => continue,
            };
            *block = ContentBlock::Text { text };
        }
    }
}

/// Drops the oldest rounds until `fits` accepts what is left, and returns how
/// many messages went.
///
/// Rescans the whole transcript per iteration. The token counts themselves
/// are cached, so an iteration hashes the transcript rather than tokenizing
/// it, but the walk is still proportional to its size. A running total is not
/// worth the state here, and this runs once per judgement rather than per
/// turn.
pub(crate) fn drop_oldest_rounds(
    messages: &mut Vec<Message>,
    fits: impl Fn(&[Message]) -> bool,
) -> usize {
    let before = messages.len();
    while !fits(messages) && truncate_oldest_round(messages) {}
    before - messages.len()
}

fn truncate_oldest_round(messages: &mut Vec<Message>) -> bool {
    if messages.len() <= 1 {
        return false;
    }
    let next_user = messages
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, message)| matches!(message.role, Role::User))
        .map(|(index, _)| index)
        .unwrap_or(1);
    messages.drain(..next_user);
    while messages.len() > 1
        && matches!(
            messages.first().map(|message| &message.role),
            Some(Role::Assistant)
        )
    {
        messages.remove(0);
    }
    true
}

/// The one JSON object an answer holds, out of a ```json fence when it came
/// in one. `None` unless the text opens and closes as an object; a second
/// object or prose between two is then left for the JSON parser to refuse.
pub(crate) fn json_object(output: &str) -> Option<&str> {
    let output = output.trim();
    let candidate = strip_json_fence(output).unwrap_or(output);
    (candidate.starts_with('{') && candidate.ends_with('}')).then_some(candidate)
}

fn strip_json_fence(output: &str) -> Option<&str> {
    let output = output
        .strip_prefix("```json")
        .or_else(|| output.strip_prefix("```"))?;
    output.strip_suffix("```").map(str::trim)
}

/// An invalid answer, cut short enough to echo back in a correction.
pub(crate) fn truncate_output(output: &str) -> String {
    output.chars().take(MAX_ECHOED_OUTPUT_CHARS).collect()
}

/// What a retry tells the model about the answer it is replacing.
pub(crate) fn correction(error: &str, previous_output: Option<&str>) -> String {
    let mut correction = format!("Your previous response was invalid ({error}).");
    if let Some(output) = previous_output {
        correction.push_str(&format!(" Previous response:\n{output}"));
    }
    correction.push_str("\n\nReturn only the required JSON object.");
    correction
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const OBJECT: &str = r#"{"ok":true}"#;
    const FENCED_JSON: &str = "```json\n{\"ok\":true}\n```";
    const FENCED_BARE: &str = "```\n{\"ok\":true}\n```";
    const PADDED: &str = "  {\"ok\":true}\n";
    const PROSE_AHEAD: &str = "Result: {\"ok\":true}";
    const ARRAY: &str = "[{\"ok\":true}]";
    const UNCLOSED_FENCE: &str = "```json\n{\"ok\":true}";
    const LONG_OUTPUT_CHARS: usize = MAX_ECHOED_OUTPUT_CHARS + 10;
    const ERROR: &str = "reason is empty";
    const PREVIOUS: &str = "{\"reason\":\"\"}";
    const NAMES_ERROR: &str = "a correction must name what was wrong";
    const ECHOES_OUTPUT: &str = "a correction must echo the answer it replaces";

    #[test_case(OBJECT ; "bare")]
    #[test_case(FENCED_JSON ; "json_fence")]
    #[test_case(FENCED_BARE ; "bare_fence")]
    #[test_case(PADDED ; "surrounding_whitespace")]
    fn an_answer_yields_its_one_object(output: &str) {
        assert_eq!(json_object(output), Some(OBJECT));
    }

    #[test_case(PROSE_AHEAD ; "prose_ahead")]
    #[test_case(ARRAY ; "array")]
    #[test_case(UNCLOSED_FENCE ; "unclosed_fence")]
    #[test_case("" ; "empty")]
    fn an_answer_that_is_not_one_object_yields_none(output: &str) {
        assert_eq!(json_object(output), None);
    }

    #[test]
    fn an_echoed_answer_is_cut_by_characters() {
        let output = "é".repeat(LONG_OUTPUT_CHARS);

        assert_eq!(
            truncate_output(&output).chars().count(),
            MAX_ECHOED_OUTPUT_CHARS
        );
    }

    #[test_case(Some(PREVIOUS) ; "with_output")]
    #[test_case(None ; "without_output")]
    fn a_correction_names_the_error_and_echoes_the_answer(previous: Option<&str>) {
        let correction = correction(ERROR, previous);

        assert!(correction.contains(ERROR), "{NAMES_ERROR}");
        assert_eq!(
            correction.contains(PREVIOUS),
            previous.is_some(),
            "{ECHOES_OUTPUT}"
        );
    }
}
