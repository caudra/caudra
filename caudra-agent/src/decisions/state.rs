use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde_json::{Map, Value};
use thiserror::Error;
use url::Url;

use crate::permissions::review::{redact_text, secret_key};

pub(super) const MAX_STATE_BYTES: usize = 1_500;
const MAX_INPUT_STRING_BYTES: usize = 8_192;
const MAX_STATE_NODES: usize = 128;
const MAX_STATE_DEPTH: usize = 8;
const REDACTED: &str = "[redacted]";
static URI_PATTERN: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"(?i)\b[a-z][a-z0-9+.-]*://[^\s\"'`<>]+"#).ok());

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecisionStateError {
    #[error("decision state exceeds the bounded context; it was not sent")]
    TooLarge,
}

#[derive(Clone, Debug)]
pub struct DecisionState(Value);

impl DecisionState {
    pub fn new(value: &Value) -> Result<Self, DecisionStateError> {
        Self::bounded(value, true)
    }

    pub(super) fn label(value: &Value) -> Result<Self, DecisionStateError> {
        Self::bounded(value, false)
    }

    fn bounded(value: &Value, redact_fields: bool) -> Result<Self, DecisionStateError> {
        let mut nodes = MAX_STATE_NODES;
        let redacted = redact_value(value, 0, &mut nodes, redact_fields)?;
        if redacted.to_string().len() > MAX_STATE_BYTES {
            return Err(DecisionStateError::TooLarge);
        }
        Ok(Self(redacted))
    }

    pub fn value(&self) -> &Value {
        &self.0
    }
}

fn redact_value(
    value: &Value,
    depth: usize,
    nodes: &mut usize,
    redact_fields: bool,
) -> Result<Value, DecisionStateError> {
    if depth >= MAX_STATE_DEPTH || *nodes == 0 {
        return Err(DecisionStateError::TooLarge);
    }
    *nodes -= 1;
    match value {
        Value::Object(values) => {
            let mut output = Map::new();
            for (key, value) in values {
                if key.len() > MAX_STATE_BYTES || *nodes == 0 {
                    return Err(DecisionStateError::TooLarge);
                }
                *nodes -= 1;
                let key = redact_decision_text(key);
                let value = if redact_fields && secret_key(&key) {
                    Value::String(REDACTED.into())
                } else {
                    redact_value(value, depth + 1, nodes, true)?
                };
                output.insert(key, value);
            }
            Ok(Value::Object(output))
        }
        Value::Array(values) => values
            .iter()
            .map(|value| redact_value(value, depth + 1, nodes, true))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::String(value) => {
            if value.len() > MAX_INPUT_STRING_BYTES {
                return Err(DecisionStateError::TooLarge);
            }
            Ok(Value::String(redact_decision_text(value)))
        }
        _ => Ok(value.clone()),
    }
}

pub(crate) fn redact_decision_text(value: &str) -> String {
    let Some(pattern) = URI_PATTERN.as_ref() else {
        return REDACTED.into();
    };
    let text = redact_text(value);
    pattern
        .replace_all(&text, |captures: &Captures<'_>| {
            let uri = &captures[0];
            match Url::parse(uri) {
                Ok(url)
                    if url.username().is_empty()
                        && url.password().is_none()
                        && !url.query_pairs().any(|(key, _)| secret_key(&key)) =>
                {
                    uri.to_owned()
                }
                _ => REDACTED.into(),
            }
        })
        .into_owned()
}

/// Appends `items` to the array at `key` while the state still fits
/// [`DecisionState::new`], so optional evidence is trimmed instead of the
/// whole state being refused. The key is only created for an item that fits.
pub(crate) fn push_fitting(state: &mut Value, key: &str, items: impl IntoIterator<Item = Value>) {
    for item in items {
        let mut candidate = state.clone();
        let Some(object) = candidate.as_object_mut() else {
            return;
        };
        let Value::Array(array) = object
            .entry(key)
            .or_insert_with(|| Value::Array(Vec::new()))
        else {
            return;
        };
        array.push(item);
        if DecisionState::new(&candidate).is_err() {
            return;
        }
        *state = candidate;
    }
}

/// The start of `text`, redacted and at most `limit` bytes. The raw text is
/// cut on whitespace first so redaction sees whole words, and only a bounded
/// prefix of a long prompt is ever scanned.
pub(crate) fn redacted_excerpt(text: &str, limit: usize) -> String {
    let cut = text.floor_char_boundary(limit);
    let raw = if cut < text.len() {
        text[..cut]
            .rsplit_once(char::is_whitespace)
            .map_or("", |(head, _)| head)
    } else {
        text
    };
    let mut excerpt = redact_decision_text(raw);
    excerpt.truncate(excerpt.floor_char_boundary(limit));
    excerpt
}

#[cfg(test)]
mod tests {
    use std::iter;

    use serde_json::json;
    use test_case::test_case;

    use super::{
        DecisionState, DecisionStateError, MAX_STATE_BYTES, REDACTED, push_fitting,
        redacted_excerpt,
    };

    const SECRET: &str = "do-not-send-this-value";
    const EVIDENCE: &str = "evidence";
    const COMMAND: &str = "cargo build";

    #[test_case(100, true; "trimmed_to_the_cap")]
    #[test_case(MAX_STATE_BYTES, false; "first_item_too_large")]
    fn evidence_fills_the_state_up_to_its_cap(item_bytes: usize, kept: bool) {
        let item = json!("x".repeat(item_bytes));
        let mut state = json!({"command": COMMAND});
        push_fitting(
            &mut state,
            EVIDENCE,
            iter::repeat_n(item.clone(), MAX_STATE_BYTES),
        );
        assert!(DecisionState::new(&state).is_ok());
        assert_eq!(state.get(EVIDENCE).is_some(), kept);
        if kept {
            state[EVIDENCE].as_array_mut().unwrap().push(item);
            assert!(DecisionState::new(&state).is_err());
        }
    }

    #[test_case("fix the build", 64, "fix the build"; "fits")]
    #[test_case("fix the build now", 11, "fix the"; "cut_on_whitespace")]
    #[test_case("refactoring", 4, ""; "no_whitespace_before_the_cut")]
    fn excerpts_keep_whole_words(text: &str, limit: usize, expected: &str) {
        assert_eq!(redacted_excerpt(text, limit), expected);
    }

    #[test]
    fn excerpts_are_redacted_within_the_limit() {
        const LIMIT: usize = 64;
        let text = format!(
            "curl -u {SECRET} https://example.test {}",
            "word ".repeat(100)
        );
        let excerpt = redacted_excerpt(&text, LIMIT);
        assert!(!excerpt.contains(SECRET));
        assert!(excerpt.starts_with("curl -u"));
        assert!(excerpt.len() <= LIMIT);
    }

    #[test_case("api_key"; "api_key")]
    #[test_case("Authorization"; "authorization")]
    #[test_case("private-key"; "private_key")]
    fn credentials_are_redacted_in_nested_values(key: &str) {
        let input = json!({"nested": [{key: SECRET}], "command": "git status"});
        let state = DecisionState::new(&input).unwrap();
        assert_eq!(state.value()["nested"][0][key], REDACTED);
        assert_eq!(state.value()["command"], "git status");
        assert!(!state.value().to_string().contains(SECRET));
    }

    #[test_case("curl -H 'Authorization: Bearer do-not-send-this-value' https://example.com"; "header")]
    #[test_case("TOKEN=do-not-send-this-value command"; "assignment")]
    #[test_case("https://user:do-not-send-this-value@example.com"; "url_credentials")]
    #[test_case("-----BEGIN PRIVATE KEY-----\ndo-not-send-this-value\n-----END PRIVATE KEY-----"; "private_key")]
    fn strings_use_permission_review_redaction(command: &str) {
        let state = DecisionState::new(&json!({"command": command})).unwrap();
        assert!(!state.value().to_string().contains(SECRET));
    }

    #[test_case("postgresql"; "postgres")]
    #[test_case("mysql"; "mysql")]
    #[test_case("redis"; "redis")]
    #[test_case("mongodb+srv"; "mongodb")]
    #[test_case("ssh"; "ssh")]
    fn non_http_uri_credentials_never_leave_state(scheme: &str) {
        let uri = format!("{scheme}://alice:{SECRET}@db.example/app");
        let input = json!({"database_url": uri, "command": format!("connect '{uri}'")});
        let state = DecisionState::new(&input).unwrap();
        assert!(!state.value().to_string().contains(SECRET));
        assert_eq!(state.value()["database_url"], REDACTED);
    }

    #[test]
    fn non_http_uri_secret_query_is_redacted() {
        let state =
            DecisionState::new(&json!({"url": format!("postgresql://db/app?password={SECRET}")}))
                .unwrap();
        assert_eq!(state.value()["url"], REDACTED);
    }

    #[test]
    fn oversized_state_is_not_silently_truncated() {
        let input = json!({"command": "x".repeat(MAX_STATE_BYTES)});
        assert_eq!(
            DecisionState::new(&input).unwrap_err(),
            DecisionStateError::TooLarge
        );
    }

    #[test]
    fn escaped_size_is_bounded() {
        let input = json!({"command": "\n".repeat(MAX_STATE_BYTES / 2)});
        assert_eq!(
            DecisionState::new(&input).unwrap_err(),
            DecisionStateError::TooLarge
        );
    }

    #[test]
    fn deep_values_are_rejected() {
        let mut input = json!("git status");
        for _ in 0..super::MAX_STATE_DEPTH {
            input = json!([input]);
        }
        assert_eq!(
            DecisionState::new(&input).unwrap_err(),
            DecisionStateError::TooLarge
        );
    }
}
