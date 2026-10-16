//! What one stored history item says, read from its JSON so this crate never
//! depends on the provider types that wrote it. The index body is bounded per
//! kind; a read gets the whole text.

use serde_json::Value;

use super::HistoryKind;
use crate::id::CaudraId;

const TOOL_CALL_BODY_BYTES: usize = 4 * 1024;
const TOOL_RESULT_HEAD_BYTES: usize = 12 * 1024;
const TOOL_RESULT_TAIL_BYTES: usize = 4 * 1024;
const ELISION: &str = "\n…\n";
const REMINDER_OPEN: &str = "<system-reminder>";
const REMINDER_CLOSE: &str = "</system-reminder>";
const ID_FIELD: &str = "id";
const TYPE_FIELD: &str = "type";
const TEXT_FIELD: &str = "text";
const NAME_FIELD: &str = "name";
const INPUT_FIELD: &str = "input";
const CONTENT_FIELD: &str = "content";
const CALL_ID_FIELD: &str = "call_id";
const REDACTED_FIELD: &str = "redacted";
const SUMMARY_FIELD: &str = "is_compaction_summary";
const STANDING_REMINDER_FIELD: &str = "standing_reminder";
const USER_TYPE: &str = "user";
const ASSISTANT_TYPE: &str = "assistant_text";
const REASONING_TYPE: &str = "reasoning";
const TOOL_CALL_TYPE: &str = "tool_call";
const TOOL_RESULT_TYPE: &str = "tool_result";

/// An item's kind and the tool a call names. `None` for anything that is not
/// a history item this search knows.
pub(super) fn classify(payload: &Value) -> Option<(HistoryKind, Option<&str>)> {
    let kind = match field(payload, TYPE_FIELD)? {
        USER_TYPE => HistoryKind::User,
        ASSISTANT_TYPE if flag(payload, SUMMARY_FIELD) => HistoryKind::Summary,
        ASSISTANT_TYPE => HistoryKind::Assistant,
        REASONING_TYPE => HistoryKind::Reasoning,
        TOOL_CALL_TYPE => return Some((HistoryKind::ToolCall, field(payload, NAME_FIELD))),
        TOOL_RESULT_TYPE => HistoryKind::ToolResult,
        _ => return None,
    };
    Some((kind, None))
}

pub(super) fn item_id(payload: &Value) -> Option<CaudraId> {
    field(payload, ID_FIELD)?.parse().ok()
}

pub(super) fn call_id(payload: &Value) -> Option<&str> {
    field(payload, CALL_ID_FIELD)
}

/// The text an item is indexed by, or `None` when it has nothing a person
/// would search for: a reminder Caudra injected, redacted reasoning, or no
/// text at all.
pub(super) fn index_body(payload: &Value) -> Option<String> {
    let (kind, tool) = classify(payload)?;
    let body = match kind {
        HistoryKind::ToolCall => {
            let mut body = tool.unwrap_or_default().to_owned();
            if let Some(input) = payload.get(INPUT_FIELD) {
                push_strings(input, &mut body);
            }
            body.truncate(body.floor_char_boundary(TOOL_CALL_BODY_BYTES));
            body
        }
        HistoryKind::ToolResult => head_and_tail(field(payload, CONTENT_FIELD)?),
        _ => return full_text(payload),
    };
    (!body.trim().is_empty()).then_some(body)
}

/// Everything an item says, for reading it back whole.
pub(super) fn full_text(payload: &Value) -> Option<String> {
    let (kind, tool) = classify(payload)?;
    let text = match kind {
        HistoryKind::User if !payload[STANDING_REMINDER_FIELD].is_null() => return None,
        HistoryKind::User => strip_reminders(field(payload, TEXT_FIELD)?),
        HistoryKind::Reasoning if flag(payload, REDACTED_FIELD) => return None,
        HistoryKind::Assistant | HistoryKind::Summary | HistoryKind::Reasoning => {
            field(payload, TEXT_FIELD)?.trim().to_owned()
        }
        HistoryKind::ToolCall => {
            let input = payload
                .get(INPUT_FIELD)
                .and_then(|input| serde_json::to_string_pretty(input).ok())
                .unwrap_or_default();
            format!("{}\n{input}", tool.unwrap_or_default())
        }
        HistoryKind::ToolResult => field(payload, CONTENT_FIELD)?.to_owned(),
    };
    (!text.trim().is_empty()).then_some(text)
}

fn field<'a>(payload: &'a Value, name: &str) -> Option<&'a str> {
    payload.get(name)?.as_str()
}

fn flag(payload: &Value, name: &str) -> bool {
    payload.get(name).and_then(Value::as_bool).unwrap_or(false)
}

/// Removes every `<system-reminder>` block, and an unterminated one to the
/// end, since only Caudra writes them.
fn strip_reminders(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(REMINDER_OPEN) {
        kept.push_str(&rest[..start]);
        rest = &rest[start..];
        rest = rest
            .find(REMINDER_CLOSE)
            .map_or("", |end| &rest[end + REMINDER_CLOSE.len()..]);
    }
    kept.push_str(rest);
    kept.trim().to_owned()
}

/// Appends the string values of a tool input, one per line, until the body
/// reaches its cap.
fn push_strings(value: &Value, body: &mut String) {
    if body.len() >= TOOL_CALL_BODY_BYTES {
        return;
    }
    match value {
        Value::String(text) => {
            body.push('\n');
            body.push_str(text);
        }
        Value::Array(values) => values.iter().for_each(|value| push_strings(value, body)),
        Value::Object(fields) => fields.values().for_each(|value| push_strings(value, body)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn head_and_tail(content: &str) -> String {
    if content.len() <= TOOL_RESULT_HEAD_BYTES + TOOL_RESULT_TAIL_BYTES {
        return content.to_owned();
    }
    let head = &content[..content.floor_char_boundary(TOOL_RESULT_HEAD_BYTES)];
    let tail = &content[content.ceil_char_boundary(content.len() - TOOL_RESULT_TAIL_BYTES)..];
    format!("{head}{ELISION}{tail}")
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const ITEM_ID: &str = "item";
    const PROMPT: &str = "fix the flaky retry test";
    const REMINDER: &str = "<system-reminder>\n# Environment\n- Date: today\n</system-reminder>";
    const ANSWER: &str = "The retry loop sleeps too long.";
    const SHELL: &str = "shell";
    const COMMAND: &str = "cargo nextest run -p caudra-storage";
    const WIDE_CHAR: &str = "é";

    fn user(text: &str) -> Value {
        json!({"id": ITEM_ID, "type": USER_TYPE, "text": text, "origin": "turn"})
    }

    #[test_case(user(PROMPT), Some((HistoryKind::User, PROMPT.to_owned())); "user")]
    #[test_case(user(&format!("{REMINDER}\n{PROMPT}")), Some((HistoryKind::User, PROMPT.to_owned())); "user_with_reminder")]
    #[test_case(user(&format!("{PROMPT}\n{REMINDER}")), Some((HistoryKind::User, PROMPT.to_owned())); "user_with_trailing_reminder")]
    #[test_case(user(REMINDER), None; "reminder_only")]
    #[test_case(user(&format!("{PROMPT}<system-reminder>unterminated")), Some((HistoryKind::User, PROMPT.to_owned())); "unterminated_reminder")]
    #[test_case(json!({"id": ITEM_ID, "type": USER_TYPE, "text": PROMPT, "origin": "synthetic", "standing_reminder": "background"}), None; "standing_reminder")]
    #[test_case(json!({"id": ITEM_ID, "type": ASSISTANT_TYPE, "text": ANSWER, "state": "complete"}), Some((HistoryKind::Assistant, ANSWER.to_owned())); "assistant")]
    #[test_case(json!({"id": ITEM_ID, "type": ASSISTANT_TYPE, "text": ANSWER, "state": "complete", "is_compaction_summary": true}), Some((HistoryKind::Summary, ANSWER.to_owned())); "summary")]
    #[test_case(json!({"id": ITEM_ID, "type": REASONING_TYPE, "text": ANSWER, "redacted": false, "interrupted": false}), Some((HistoryKind::Reasoning, ANSWER.to_owned())); "reasoning")]
    #[test_case(json!({"id": ITEM_ID, "type": REASONING_TYPE, "text": ANSWER, "redacted": true, "interrupted": false}), None; "redacted_reasoning")]
    #[test_case(json!({"id": ITEM_ID, "type": TOOL_CALL_TYPE, "call_id": "c1", "name": SHELL, "input": {"command": COMMAND, "timeoutSec": 30}}), Some((HistoryKind::ToolCall, format!("{SHELL}\n{COMMAND}"))); "tool_call")]
    #[test_case(json!({"id": ITEM_ID, "type": TOOL_RESULT_TYPE, "call_id": "c1", "content": ANSWER, "is_error": false}), Some((HistoryKind::ToolResult, ANSWER.to_owned())); "tool_result")]
    #[test_case(json!({"id": ITEM_ID, "type": ASSISTANT_TYPE, "text": "  ", "state": "complete"}), None; "blank_assistant")]
    #[test_case(json!({"id": ITEM_ID, "type": "unknown"}), None; "unknown_type")]
    fn extraction_per_kind(payload: Value, expected: Option<(HistoryKind, String)>) {
        let extracted = index_body(&payload).map(|body| (classify(&payload).unwrap().0, body));
        assert_eq!(extracted, expected);
    }

    #[test]
    fn tool_call_body_is_capped_on_a_char_boundary() {
        let long = WIDE_CHAR.repeat(TOOL_CALL_BODY_BYTES);
        let payload = json!({"id": ITEM_ID, "type": TOOL_CALL_TYPE, "call_id": "c1", "name": SHELL, "input": {"a": long, "b": COMMAND}});
        let body = index_body(&payload).unwrap();
        assert!(body.len() <= TOOL_CALL_BODY_BYTES);
        assert!(body.starts_with(SHELL));
        assert!(!body.contains(COMMAND));
    }

    #[test]
    fn tool_result_body_keeps_head_and_tail() {
        let head = WIDE_CHAR.repeat(TOOL_RESULT_HEAD_BYTES / WIDE_CHAR.len());
        let middle = "m".repeat(TOOL_RESULT_HEAD_BYTES);
        let tail = "t".repeat(TOOL_RESULT_TAIL_BYTES);
        let content = format!("a{head}{middle}{tail}");
        let payload = json!({"id": ITEM_ID, "type": TOOL_RESULT_TYPE, "call_id": "c1", "content": content, "is_error": false});
        let body = index_body(&payload).unwrap();
        assert_eq!(
            body.len(),
            TOOL_RESULT_HEAD_BYTES - 1 + ELISION.len() + TOOL_RESULT_TAIL_BYTES
        );
        assert!(!body.contains('m'));
        assert!(body.ends_with(&tail));
        assert_eq!(full_text(&payload).unwrap(), content);
    }

    #[test]
    fn full_text_of_a_tool_call_shows_its_whole_input() {
        let id = CaudraId::generate();
        let payload = json!({"id": id.to_string(), "type": TOOL_CALL_TYPE, "call_id": "c1", "name": SHELL, "input": {"command": COMMAND, "timeoutSec": 30}});
        let text = full_text(&payload).unwrap();
        assert!(text.starts_with(SHELL));
        assert!(text.contains("\"timeoutSec\": 30"));
        assert_eq!(call_id(&payload), Some("c1"));
        assert_eq!(item_id(&payload), Some(id));
    }
}
