//! The main conversations of a scope in time order, for continual learning:
//! what people typed, what the agent answered, and the moments a person
//! pushed back, read from the index and the stored payloads.

use std::collections::HashSet;

use rusqlite::params_from_iter;
use rusqlite::types::Value as SqlValue;

use super::extract::{authored, classify, failed, full_text, interrupted, refused, typed_text};
use super::query::{in_list, kind_from_code, payload_at};
use super::{HistoryKind, HistoryRef, HistoryScope, HistoryStream, MAIN_STREAM};
use crate::sessions::{
    SessionDatabase, SessionError, from_i64, from_i64_usize, id_from_row, to_i64,
};

const AT_FIELD: &str = "history_search_docs.at_ms";
const ORDINAL_FIELD: &str = "history_search_docs.ordinal";

/// One item of a main conversation, with what a learner reads from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineItem {
    pub reference: HistoryRef,
    pub session_title: String,
    pub at_ms: u64,
    pub kind: HistoryKind,
    /// The typed text of a user item, the whole text of any other.
    pub text: String,
    /// A user item a person typed.
    pub authored: bool,
    /// An answer the user cut off.
    pub interrupted: bool,
    /// A tool result with a call refused before it ran.
    pub refused: bool,
    /// A tool result that reported an error.
    pub failed: bool,
}

impl SessionDatabase {
    /// Main-conversation items of `kinds` made in `[since_ms, until_ms)`,
    /// oldest first, at most `limit`, each item once however many forks
    /// copied it. Only what the index already holds is read.
    pub fn history_timeline(
        &self,
        scope: &HistoryScope,
        kinds: &[HistoryKind],
        since_ms: u64,
        until_ms: u64,
        limit: usize,
    ) -> Result<Vec<TimelineItem>, SessionError> {
        let connection = self.connection();
        let (filter, mut values) = scope.filter();
        values.extend(kinds.iter().map(|kind| SqlValue::Integer(kind.code())));
        values.extend([
            SqlValue::Text(MAIN_STREAM.to_owned()),
            SqlValue::Integer(to_i64(since_ms, AT_FIELD)?),
            SqlValue::Integer(to_i64(until_ms, AT_FIELD)?),
        ]);
        let mut statement = connection.prepare(&format!(
            "SELECT d.session_id, d.ordinal, d.item_id, d.kind, d.at_ms, s.title \
             FROM history_search_docs d JOIN sessions s ON s.id = d.session_id \
             WHERE {filter} AND {} AND d.stream = ? AND d.at_ms >= ? AND d.at_ms < ? \
             ORDER BY d.at_ms, d.doc",
            in_list("d.kind", kinds.len())
        ))?;
        let mut rows = statement.query(params_from_iter(values))?;
        let mut seen = HashSet::new();
        let mut items = Vec::new();
        while items.len() < limit {
            let Some(row) = rows.next()? else { break };
            let reference = HistoryRef {
                session: id_from_row(row, 0)?,
                stream: HistoryStream::Main,
                item: id_from_row(row, 2)?,
            };
            if !seen.insert(reference.item) {
                continue;
            }
            let ordinal = from_i64_usize(row.get(1)?, ORDINAL_FIELD)?;
            let kind = kind_from_code(row.get(3)?)?;
            let at_ms = from_i64(row.get(4)?, AT_FIELD)?;
            let session_title: String = row.get(5)?;
            let Some(payload) = payload_at(connection, &reference, ordinal)? else {
                continue;
            };
            if classify(&payload).is_none() {
                continue;
            }
            let text = match kind {
                HistoryKind::User => typed_text(&payload),
                _ => full_text(&payload),
            };
            let Some(text) = text else { continue };
            items.push(TimelineItem {
                reference,
                session_title,
                at_ms,
                kind,
                text,
                authored: kind == HistoryKind::User && authored(&payload),
                interrupted: interrupted(&payload),
                refused: refused(&payload),
                failed: failed(&payload),
            });
        }
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::super::ProjectScope;
    use super::super::fixtures::{
        BASE_MS, OTHER_PROJECT, PROJECT, assistant, catch_up, database, id_at, session_with,
        user_at,
    };
    use super::*;

    const TITLE: &str = "Learn from this";
    const TYPED: &str = "always run make lint before pushing";
    const SHOWN: &str = "use the fake provider";
    const SENT: &str = "use the fake provider\n<file>…</file>";
    const ANSWER: &str = "Done.";
    const HOUR_MS: u64 = 3_600_000;
    const ALL_KINDS: [HistoryKind; 3] = [
        HistoryKind::User,
        HistoryKind::Assistant,
        HistoryKind::ToolResult,
    ];

    fn project() -> HistoryScope {
        HistoryScope::Project(ProjectScope::Local(vec![PROJECT.into()]))
    }

    fn seeded(sessions: Vec<(&str, Vec<Value>)>) -> (tempfile::TempDir, SessionDatabase) {
        let (temp, mut database) = database();
        for (cwd, items) in sessions {
            database
                .save(&session_with(cwd, TITLE, items), None)
                .unwrap();
        }
        catch_up(&database, &HistoryScope::All);
        (temp, database)
    }

    fn timeline(database: &SessionDatabase, since: u64, until: u64) -> Vec<TimelineItem> {
        database
            .history_timeline(&project(), &ALL_KINDS, since, until, usize::MAX)
            .unwrap()
    }

    fn user_with(at_ms: u64, fields: Value) -> Value {
        let mut item = user_at(at_ms, TYPED);
        for (key, value) in fields.as_object().unwrap() {
            item[key] = value.clone();
        }
        item
    }

    #[test]
    fn items_come_oldest_first_within_the_window_and_project() {
        let (_temp, database) = seeded(vec![
            (
                PROJECT,
                vec![
                    user_at(BASE_MS + 2 * HOUR_MS, "third"),
                    user_at(BASE_MS, "first"),
                    user_at(BASE_MS + HOUR_MS, "second"),
                    user_at(BASE_MS + 3 * HOUR_MS, "outside"),
                ],
            ),
            (OTHER_PROJECT, vec![user_at(BASE_MS, "elsewhere")]),
        ]);
        let texts: Vec<String> = timeline(&database, BASE_MS, BASE_MS + 3 * HOUR_MS)
            .into_iter()
            .map(|item| item.text)
            .collect();
        assert_eq!(texts, ["first", "second", "third"]);
    }

    #[test_case(json!({}), true ; "typed")]
    #[test_case(json!({"origin": "synthetic"}), false ; "synthetic")]
    #[test_case(json!({"automation_event": {"name": "keep-going"}}), false ; "automation")]
    #[test_case(json!({"peer_event": {"sender": "@other"}}), false ; "peer")]
    fn only_what_a_person_typed_counts_as_authored(fields: Value, expected: bool) {
        let (_temp, database) = seeded(vec![(PROJECT, vec![user_with(BASE_MS, fields)])]);
        assert_eq!(
            timeline(&database, BASE_MS, BASE_MS + 1)[0].authored,
            expected
        );
    }

    #[test]
    fn a_user_item_reads_as_the_prompt_showed_it() {
        let item = user_with(BASE_MS, json!({"text": SENT, "display_text": SHOWN}));
        let (_temp, database) = seeded(vec![(PROJECT, vec![item])]);
        assert_eq!(timeline(&database, BASE_MS, BASE_MS + 1)[0].text, SHOWN);
    }

    #[test]
    fn signals_of_pushback_are_read() {
        let mut cut = assistant(ANSWER);
        cut["state"] = json!("interrupted");
        cut["id"] = json!(id_at(BASE_MS).to_string());
        let refusal = json!({
            "id": id_at(BASE_MS + 1).to_string(), "type": "tool_result", "call_id": "c1",
            "content": "denied", "is_error": true, "refused_calls": [0]
        });
        let (_temp, database) = seeded(vec![(PROJECT, vec![cut, refusal])]);
        let items = timeline(&database, BASE_MS, BASE_MS + 2);
        assert!(items[0].interrupted);
        assert!(items[1].refused && items[1].failed);
    }
}
