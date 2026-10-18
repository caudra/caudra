//! What a history picker previews of a session: the first and the last prompt
//! its person typed, and its model. Items are scanned from either end, and a
//! payload is decompressed whole only once its head shows a user item.

use std::io::Read;

use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;
use zstd::stream::read::Decoder;

use crate::history_search::strip_reminders;
use crate::id::CaudraId;
use crate::sessions::{SessionDatabase, SessionError, decompress_payload, deserialize_json};

/// The longest prompt preview, in characters.
const PROMPT_PREVIEW_CHARS: usize = 280;
/// Enough of a payload to reach its kind tag, which only its ids precede.
const KIND_PROBE_BYTES: u64 = 512;
const KIND_TAG: &[u8] = br#""type":""#;
const USER_KIND: &str = "user";
const TURN_ORIGIN: &str = "turn";
const TYPE_FIELD: &str = "type";
const ORIGIN_FIELD: &str = "origin";
const TEXT_FIELD: &str = "text";
const DISPLAY_TEXT_FIELD: &str = "display_text";
const PAYLOAD_FIELD: &str = "main_history_items.payload";
const MODEL_QUERY: &str = "SELECT model FROM sessions WHERE id = ?1";
const FIRST_ITEMS: &str =
    "SELECT payload FROM main_history_items WHERE session_id = ?1 ORDER BY ordinal ASC";
const LAST_ITEMS: &str =
    "SELECT payload FROM main_history_items WHERE session_id = ?1 ORDER BY ordinal DESC";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SessionPreview {
    pub model: Option<String>,
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
}

impl SessionDatabase {
    /// A session's model and its first and last typed prompt, trimmed, cut
    /// to [`PROMPT_PREVIEW_CHARS`], and without images, mentions, or the
    /// reminders Caudra adds.
    pub fn session_preview(&self, id: CaudraId) -> Result<SessionPreview, SessionError> {
        let session = id.as_bytes().as_slice();
        let model = self
            .connection()
            .query_row(MODEL_QUERY, params![session], |row| row.get::<_, String>(0))
            .optional()?
            .filter(|model| !model.is_empty());
        let first_prompt = self.typed_prompt(session, FIRST_ITEMS)?;
        let last_prompt = match first_prompt {
            Some(_) => self.typed_prompt(session, LAST_ITEMS)?,
            None => None,
        };
        Ok(SessionPreview {
            model,
            first_prompt,
            last_prompt,
        })
    }

    fn typed_prompt(&self, session: &[u8], items: &str) -> Result<Option<String>, SessionError> {
        let mut statement = self.connection().prepare_cached(items)?;
        let mut rows = statement.query(params![session])?;
        while let Some(row) = rows.next()? {
            let stored = row.get_ref(0)?.as_blob().map_err(rusqlite::Error::from)?;
            if known_not_user(stored) {
                continue;
            }
            let payload: Value =
                deserialize_json(&decompress_payload(stored, PAYLOAD_FIELD)?, PAYLOAD_FIELD)?;
            if let Some(prompt) = prompt_text(&payload) {
                return Ok(Some(prompt));
            }
        }
        Ok(None)
    }
}

/// Whether a payload's head already shows another kind than a user item:
/// its kind tag is found before any nested value and is complete.
fn known_not_user(stored: &[u8]) -> bool {
    let mut head = Vec::new();
    let probed = Decoder::with_buffer(stored)
        .and_then(|decoder| decoder.take(KIND_PROBE_BYTES).read_to_end(&mut head));
    if probed.is_err() {
        return false;
    }
    let Some(tag) = head
        .windows(KIND_TAG.len())
        .position(|window| window == KIND_TAG)
    else {
        return false;
    };
    if head
        .get(1..tag)
        .is_none_or(|before| before.iter().any(|byte| matches!(byte, b'{' | b'[')))
    {
        return false;
    }
    let kind = &head[tag + KIND_TAG.len()..];
    kind.iter()
        .position(|&byte| byte == b'"')
        .is_some_and(|end| &kind[..end] != USER_KIND.as_bytes())
}

/// What the person typed, as the title reads it: the display text when there
/// is one, else the text without reminders.
fn prompt_text(payload: &Value) -> Option<String> {
    if payload[TYPE_FIELD] != USER_KIND || payload[ORIGIN_FIELD] != TURN_ORIGIN {
        return None;
    }
    let mut text = match payload[DISPLAY_TEXT_FIELD].as_str() {
        Some(display) => display.trim().to_owned(),
        None => strip_reminders(payload[TEXT_FIELD].as_str()?),
    };
    if let Some((end, _)) = text.char_indices().nth(PROMPT_PREVIEW_CHARS) {
        text.truncate(end);
    }
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::StateDir;
    use crate::sessions::Session;
    use crate::sessions::database::compress_payload;

    type TestSession = Session<Value, Value, Value>;

    const MODEL: &str = "anthropic/claude-sonnet-4-5";
    const CWD: &str = "/project";
    const FIRST: &str = "fix the flaky retry test";
    const LAST: &str = "now run the whole suite";
    const TYPED: &str = "explain @src/lib.rs";
    const EXPANDED: &str = "explain src/lib.rs\n<file>fn main() {}</file>";
    const REMINDER: &str = "<system-reminder>\n# Environment\n</system-reminder>";
    const ANSWER: &str = "The retry loop sleeps too long.";
    const WIDE_CHAR: &str = "é";

    fn user(text: &str) -> Value {
        json!({"type": USER_KIND, "text": text, "origin": TURN_ORIGIN})
    }

    fn with_id(mut item: Value) -> Value {
        item["id"] = json!(CaudraId::generate().to_string());
        item
    }

    fn preview(items: Vec<Value>) -> SessionPreview {
        let temp = TempDir::new().unwrap();
        let mut database =
            SessionDatabase::open(&StateDir::from_path(temp.path().to_path_buf())).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        session.replace_messages(items.into_iter().map(with_id).collect());
        database.save(&session, None).unwrap();
        database.session_preview(session.id).unwrap()
    }

    #[test]
    fn previews_the_first_and_last_typed_prompt_and_the_model() {
        let synthetic = json!({"type": USER_KIND, "text": ANSWER, "origin": "synthetic"});
        let previewed = preview(vec![
            synthetic.clone(),
            user(&format!("{REMINDER}\n  {FIRST}  ")),
            json!({"type": "assistant_text", "text": ANSWER, "state": "complete"}),
            user(LAST),
            json!({"type": "tool_call", "call_id": "c1", "name": "shell", "input": {"type": USER_KIND}}),
            json!({"type": "tool_result", "call_id": "c1", "content": ANSWER, "is_error": false}),
            synthetic,
        ]);

        assert_eq!(
            previewed,
            SessionPreview {
                model: Some(MODEL.to_owned()),
                first_prompt: Some(FIRST.to_owned()),
                last_prompt: Some(LAST.to_owned()),
            }
        );
    }

    #[test_case(vec![json!({"type": "assistant_text", "text": ANSWER, "state": "complete"})]; "assistant_only")]
    #[test_case(vec![user(REMINDER)]; "reminder_only")]
    #[test_case(vec![json!({"type": USER_KIND, "text": ANSWER, "origin": "mention"})]; "mention_only")]
    #[test_case(Vec::new(); "empty")]
    fn a_session_without_a_typed_prompt_previews_none(items: Vec<Value>) {
        let previewed = preview(items);

        assert_eq!(previewed.first_prompt, None);
        assert_eq!(previewed.last_prompt, None);
    }

    #[test]
    fn a_long_prompt_is_cut_to_the_cap_on_a_char_boundary() {
        let long = WIDE_CHAR.repeat(PROMPT_PREVIEW_CHARS + 1);

        let previewed = preview(vec![user(&long)]);

        let expected = WIDE_CHAR.repeat(PROMPT_PREVIEW_CHARS);
        assert_eq!(previewed.first_prompt.as_deref(), Some(expected.as_str()));
        assert_eq!(previewed.last_prompt.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn images_are_left_out_and_the_display_text_wins() {
        let image = json!({"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="});
        let previewed = preview(vec![json!({
            "images": [image],
            "type": USER_KIND,
            "text": EXPANDED,
            "display_text": TYPED,
            "origin": TURN_ORIGIN,
        })]);

        assert_eq!(previewed.first_prompt.as_deref(), Some(TYPED));
    }

    #[test_case(json!({"id": "a", "type": "tool_result", "content": ANSWER}), true; "other_kind")]
    #[test_case(json!({"id": "a", "type": USER_KIND, "text": FIRST}), false; "user")]
    #[test_case(json!({"images": [{"type": "base64"}], "type": USER_KIND}), false; "nested_tag_first")]
    #[test_case(json!({"text": "x".repeat(KIND_PROBE_BYTES as usize), "type": "tool_result"}), false; "tag_past_the_probe")]
    fn the_probe_rules_out_only_what_its_head_proves(payload: Value, expected: bool) {
        let stored = compress_payload(&payload.to_string()).unwrap();

        assert_eq!(known_not_user(&stored), expected);
    }
}
