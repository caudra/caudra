//! Decision-engine transcript rows: what a decision feature did to a session,
//! kept for display only and never sent to the model. Rows belong to a session
//! and go with it through the schema's cascade. Each session keeps its newest
//! [`MAX_ENGINE_DECISIONS_PER_SESSION`], a session rewrite keeps them, and a
//! fork starts with none.

use rusqlite::{Row, Transaction, TransactionBehavior, params};

use crate::StorageError;
use crate::id::CaudraId;
use crate::sessions::{
    SessionDatabase, SessionError, deserialize_json, from_i64, from_i64_usize, to_i64,
};

/// Rows each session keeps. An insert past it drops the oldest.
pub const MAX_ENGINE_DECISIONS_PER_SESSION: usize = 2000;
const ENGINE_DECISION_COLUMNS: &str =
    "seq, at, feature, mode, outcome, summary, details, agent, tool_use_id, history_len, screening";
const ACTED: &str = "acted";
const SHADOW: &str = "shadow";
const FELL_BACK: &str = "fell_back";
const OUTCOME_FIELD: &str = "engine_decisions.outcome";
const DETAILS_FIELD: &str = "engine_decisions.details";
const ANCHOR_FIELD: &str = "engine_decisions.anchor";
const HISTORY_LEN_FIELD: &str = "engine_decisions.history_len";
const SEQ_FIELD: &str = "engine_decisions.seq";
const AT_FIELD: &str = "engine_decisions.at";
const LIMIT_FIELD: &str = "engine_decisions.limit";

/// Feature and mode are the decision log's names and carry no CHECK, so a new
/// one needs no migration. Exactly one of `tool_use_id` and `history_len`
/// places the row.
pub(crate) const TABLE: &str = r#"
CREATE TABLE engine_decisions (
    session_id  BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL CHECK(seq > 0),
    at          INTEGER NOT NULL,
    feature     TEXT NOT NULL,
    mode        TEXT NOT NULL,
    outcome     TEXT NOT NULL CHECK(outcome IN ('acted', 'shadow', 'fell_back')),
    summary     TEXT NOT NULL,
    details     TEXT NOT NULL CHECK(json_valid(details)),
    agent       TEXT,
    tool_use_id TEXT,
    history_len INTEGER CHECK(history_len >= 0),
    CHECK((tool_use_id IS NULL) <> (history_len IS NULL)),
    PRIMARY KEY(session_id, seq)
) STRICT, WITHOUT ROWID;
"#;

/// Schema 40: what content screening concluded, by name, so a restored row
/// tells a real flag from a result that could not be screened. Older rows
/// have none.
pub(crate) const SCREENING_COLUMN: &str = r#"
ALTER TABLE engine_decisions ADD COLUMN screening TEXT CHECK(length(screening) <= 32);
"#;

/// What became of the feature's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredDecisionOutcome {
    /// The answer changed what happened.
    Acted,
    /// Shadow mode: what the answer would have changed.
    Shadow,
    /// The engine failed and the default applied.
    FellBack,
}

impl StoredDecisionOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Acted => ACTED,
            Self::Shadow => SHADOW,
            Self::FellBack => FELL_BACK,
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            ACTED => Some(Self::Acted),
            SHADOW => Some(Self::Shadow),
            FELL_BACK => Some(Self::FellBack),
            _ => None,
        }
    }
}

/// Where a row goes among the transcript on restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredDecisionAnchor {
    /// Transcript items the session held when the row was made.
    History(usize),
    /// The tool call it was made in.
    Tool(String),
}

/// One decision-engine transcript row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEngineDecision {
    /// Assigned by the store on insert, ignored on input.
    pub seq: u64,
    /// Unix milliseconds.
    pub at_ms: u64,
    /// The decision log's feature name, such as `memory_search`.
    pub feature: String,
    /// The feature's mode: `shadow`, `advise` or `enforce`.
    pub mode: String,
    pub outcome: StoredDecisionOutcome,
    pub summary: String,
    pub details: Vec<String>,
    /// The subagent it was made for, `None` for the main agent.
    pub agent: Option<String>,
    pub anchor: StoredDecisionAnchor,
    /// Content screening's verdict or the reason it had none, by name. `None`
    /// for every other feature and for rows written before schema 40.
    pub screening: Option<String>,
}

impl SessionDatabase {
    /// Appends `decisions` in order after the session's newest, then drops its
    /// oldest past [`MAX_ENGINE_DECISIONS_PER_SESSION`] in the same
    /// transaction.
    pub fn insert_engine_decisions(
        &self,
        session_id: CaudraId,
        decisions: &[StoredEngineDecision],
    ) -> Result<(), SessionError> {
        if decisions.is_empty() {
            return Ok(());
        }
        let session_id = session_id.as_bytes().as_slice();
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        {
            let mut insert = transaction.prepare(&format!(
                "INSERT INTO engine_decisions (session_id, {ENGINE_DECISION_COLUMNS}) \
                 SELECT ?1, coalesce(max(seq), 0) + 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11 \
                 FROM engine_decisions WHERE session_id = ?1"
            ))?;
            for decision in decisions {
                let (tool_use_id, history_len) = match &decision.anchor {
                    StoredDecisionAnchor::History(len) => {
                        (None, Some(to_i64(*len, HISTORY_LEN_FIELD)?))
                    }
                    StoredDecisionAnchor::Tool(id) => (Some(id.as_str()), None),
                };
                insert.execute(params![
                    session_id,
                    to_i64(decision.at_ms, AT_FIELD)?,
                    decision.feature,
                    decision.mode,
                    decision.outcome.as_str(),
                    decision.summary,
                    serde_json::to_string(&decision.details).map_err(StorageError::from)?,
                    decision.agent,
                    tool_use_id,
                    history_len,
                    decision.screening,
                ])?;
            }
        }
        transaction.execute(
            "DELETE FROM engine_decisions WHERE session_id = ?1 AND seq <= (\
                 SELECT seq FROM engine_decisions WHERE session_id = ?1 \
                 ORDER BY seq DESC LIMIT 1 OFFSET ?2)",
            params![
                session_id,
                to_i64(MAX_ENGINE_DECISIONS_PER_SESSION, LIMIT_FIELD)?
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// A session's rows, oldest first.
    pub fn load_engine_decisions(
        &self,
        session_id: CaudraId,
    ) -> Result<Vec<StoredEngineDecision>, SessionError> {
        let mut statement = self.connection().prepare(&format!(
            "SELECT {ENGINE_DECISION_COLUMNS} FROM engine_decisions WHERE session_id = ?1 \
             ORDER BY seq"
        ))?;
        let mut rows = statement.query(params![session_id.as_bytes().as_slice()])?;
        let mut decisions = Vec::new();
        while let Some(row) = rows.next()? {
            decisions.push(read_decision(row)?);
        }
        Ok(decisions)
    }
}

fn read_decision(row: &Row<'_>) -> Result<StoredEngineDecision, SessionError> {
    let outcome: String = row.get(4)?;
    let Some(parsed_outcome) = StoredDecisionOutcome::parse(&outcome) else {
        return Err(SessionError::CorruptDatabaseValue {
            field: OUTCOME_FIELD,
            reason: outcome,
        });
    };
    Ok(StoredEngineDecision {
        seq: from_i64(row.get(0)?, SEQ_FIELD)?,
        at_ms: from_i64(row.get(1)?, AT_FIELD)?,
        feature: row.get(2)?,
        mode: row.get(3)?,
        outcome: parsed_outcome,
        summary: row.get(5)?,
        details: deserialize_json(&row.get::<_, String>(6)?, DETAILS_FIELD)?,
        agent: row.get(7)?,
        anchor: read_anchor(row.get(8)?, row.get(9)?)?,
        screening: row.get(10)?,
    })
}

fn read_anchor(
    tool_use_id: Option<String>,
    history_len: Option<i64>,
) -> Result<StoredDecisionAnchor, SessionError> {
    match (tool_use_id, history_len) {
        (Some(tool_use_id), None) => Ok(StoredDecisionAnchor::Tool(tool_use_id)),
        (None, Some(history_len)) => Ok(StoredDecisionAnchor::History(from_i64_usize(
            history_len,
            HISTORY_LEN_FIELD,
        )?)),
        (tool_use_id, history_len) => Err(SessionError::CorruptDatabaseValue {
            field: ANCHOR_FIELD,
            reason: format!("tool_use_id {tool_use_id:?} with history_len {history_len:?}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::ErrorCode;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::StateDir;
    use crate::sessions::Session;

    const CWD: &str = "/project";
    const MODEL: &str = "test/model";
    const FEATURE: &str = "memory_search";
    const MODE: &str = "advise";
    const SUMMARY: &str = "memory_search · 14 keyword matches ranked";
    const DETAIL_KEPT: &str = "0.97  42+1 release-preview-7  kept 1st";
    const DETAIL_LEFT_OUT: &str = "0.12  7+1 old-notes  left out";
    const AGENT: &str = "review";
    const SCREENING: &str = "timed_out";
    const TOOL_USE_ID: &str = "toolu_1";
    const AT_MS: u64 = 1_767_225_600_000;
    const HISTORY_LEN: usize = 12;
    const IGNORED_SEQ: u64 = 99;
    const FIRST_SEQ: u64 = 1;
    const EXTRA_DECISIONS: usize = 3;
    const OTHER_SESSION_DECISIONS: usize = 2;
    const UNKNOWN_OUTCOME: &str = "skipped";
    const EMPTY_DETAILS: &str = "[]";
    const INVALID_DETAILS: &str = "not json";
    const OBJECT_DETAILS: &str = r#"{"line":1}"#;
    const ROUND_TRIPS: &str = "rows must load whole, in order, with store-assigned seqs";
    const SEQ_CONTINUES: &str = "seq must continue after the session's newest row";
    const TRIM_KEEPS_NEWEST: &str = "an insert must keep only the session's newest rows";
    const TRIM_PER_SESSION: &str = "trimming one session must leave the others alone";
    const ANCHOR_CHECK: &str = "a row must carry exactly one anchor";
    const REWRITE_KEEPS_ROWS: &str = "saving a session must keep its engine decisions";
    const CASCADE: &str = "engine decisions must go with their session";
    const CORRUPT_ROW: &str = "a corrupt row must load as a corrupt database value";

    type TestSession = Session<Value, Value, Value>;

    fn open() -> (TempDir, SessionDatabase, CaudraId) {
        let temp = TempDir::new().unwrap();
        let mut database =
            SessionDatabase::open(&StateDir::from_path(temp.path().to_path_buf())).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        (temp, database, session.id)
    }

    fn decision(
        outcome: StoredDecisionOutcome,
        anchor: StoredDecisionAnchor,
    ) -> StoredEngineDecision {
        StoredEngineDecision {
            seq: IGNORED_SEQ,
            at_ms: AT_MS,
            feature: FEATURE.into(),
            mode: MODE.into(),
            outcome,
            summary: SUMMARY.into(),
            details: vec![DETAIL_KEPT.into(), DETAIL_LEFT_OUT.into()],
            agent: None,
            anchor,
            screening: None,
        }
    }

    fn history() -> StoredDecisionAnchor {
        StoredDecisionAnchor::History(HISTORY_LEN)
    }

    fn tool() -> StoredDecisionAnchor {
        StoredDecisionAnchor::Tool(TOOL_USE_ID.into())
    }

    fn numbered(decisions: &[StoredEngineDecision], first: u64) -> Vec<StoredEngineDecision> {
        (first..)
            .zip(decisions)
            .map(|(seq, decision)| StoredEngineDecision {
                seq,
                ..decision.clone()
            })
            .collect()
    }

    /// Fills a session with `count` rows numbered from one in one statement, so
    /// a trim test does not pay a commit per row.
    fn seed(database: &SessionDatabase, session_id: CaudraId, count: usize) {
        database
            .connection()
            .execute(
                "WITH RECURSIVE seqs(seq) AS (SELECT 1 UNION ALL SELECT seq + 1 FROM seqs \
                     WHERE seq < ?2) \
                 INSERT INTO engine_decisions \
                     (session_id, seq, at, feature, mode, outcome, summary, details, history_len) \
                 SELECT ?1, seq, ?3, ?4, ?5, ?6, ?7, ?8, ?9 FROM seqs",
                params![
                    session_id.as_bytes().as_slice(),
                    to_i64(count, "count").unwrap(),
                    to_i64(AT_MS, "at").unwrap(),
                    FEATURE,
                    MODE,
                    ACTED,
                    SUMMARY,
                    EMPTY_DETAILS,
                    to_i64(HISTORY_LEN, "history_len").unwrap(),
                ],
            )
            .unwrap();
    }

    fn insert_raw(
        database: &SessionDatabase,
        session_id: CaudraId,
        outcome: &str,
        details: &str,
        tool_use_id: Option<&str>,
        history_len: Option<i64>,
    ) -> rusqlite::Result<usize> {
        database.connection().execute(
            "INSERT INTO engine_decisions \
                 (session_id, seq, at, feature, mode, outcome, summary, details, tool_use_id, \
                 history_len) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                session_id.as_bytes().as_slice(),
                to_i64(FIRST_SEQ, "seq").unwrap(),
                to_i64(AT_MS, "at").unwrap(),
                FEATURE,
                MODE,
                outcome,
                SUMMARY,
                details,
                tool_use_id,
                history_len,
            ],
        )
    }

    fn seqs(database: &SessionDatabase, session_id: CaudraId) -> Vec<u64> {
        database
            .load_engine_decisions(session_id)
            .unwrap()
            .iter()
            .map(|decision| decision.seq)
            .collect()
    }

    #[test_case(StoredDecisionOutcome::Acted ; "acted")]
    #[test_case(StoredDecisionOutcome::Shadow ; "shadow")]
    #[test_case(StoredDecisionOutcome::FellBack ; "fell_back")]
    fn decisions_round_trip_with_either_anchor(outcome: StoredDecisionOutcome) {
        let (_temp, database, session_id) = open();
        let written = [
            decision(outcome, history()),
            StoredEngineDecision {
                agent: Some(AGENT.into()),
                details: Vec::new(),
                screening: Some(SCREENING.into()),
                ..decision(outcome, tool())
            },
        ];

        database
            .insert_engine_decisions(session_id, &written)
            .unwrap();

        assert_eq!(
            database.load_engine_decisions(session_id).unwrap(),
            numbered(&written, FIRST_SEQ),
            "{ROUND_TRIPS}"
        );
    }

    #[test]
    fn seq_continues_after_the_newest_row() {
        let (_temp, database, session_id) = open();
        let row = decision(StoredDecisionOutcome::Acted, history());

        database
            .insert_engine_decisions(session_id, &[row.clone(), row.clone()])
            .unwrap();
        database
            .insert_engine_decisions(session_id, &[row])
            .unwrap();

        assert_eq!(seqs(&database, session_id), [1, 2, 3], "{SEQ_CONTINUES}");
    }

    #[test]
    fn an_insert_keeps_only_the_newest_decisions() {
        let (_temp, mut database, session_id) = open();
        let other = TestSession::new(MODEL, CWD);
        database.save(&other, None).unwrap();
        seed(&database, other.id, OTHER_SESSION_DECISIONS);
        seed(
            &database,
            session_id,
            MAX_ENGINE_DECISIONS_PER_SESSION + EXTRA_DECISIONS - 1,
        );

        database
            .insert_engine_decisions(
                session_id,
                &[decision(StoredDecisionOutcome::Shadow, tool())],
            )
            .unwrap();

        let kept = seqs(&database, session_id);
        let newest = (MAX_ENGINE_DECISIONS_PER_SESSION + EXTRA_DECISIONS) as u64;
        assert_eq!(
            kept.len(),
            MAX_ENGINE_DECISIONS_PER_SESSION,
            "{TRIM_KEEPS_NEWEST}"
        );
        assert_eq!(
            (kept.first().copied(), kept.last().copied()),
            (Some(EXTRA_DECISIONS as u64 + FIRST_SEQ), Some(newest)),
            "{TRIM_KEEPS_NEWEST}"
        );
        assert_eq!(
            seqs(&database, other.id).len(),
            OTHER_SESSION_DECISIONS,
            "{TRIM_PER_SESSION}"
        );
    }

    #[test_case(Some(TOOL_USE_ID), Some(0) ; "both")]
    #[test_case(None, None ; "neither")]
    fn a_row_needs_exactly_one_anchor(tool_use_id: Option<&str>, history_len: Option<i64>) {
        let (_temp, database, session_id) = open();

        let result = insert_raw(
            &database,
            session_id,
            ACTED,
            EMPTY_DETAILS,
            tool_use_id,
            history_len,
        );

        assert!(
            matches!(result, Err(error) if error.sqlite_error_code() == Some(ErrorCode::ConstraintViolation)),
            "{ANCHOR_CHECK}"
        );
    }

    #[test]
    fn a_rewritten_session_keeps_its_decisions() {
        let (_temp, mut database, session_id) = open();
        let written = [decision(StoredDecisionOutcome::FellBack, tool())];
        database
            .insert_engine_decisions(session_id, &written)
            .unwrap();
        let mut session = database.load::<Value, Value, Value>(session_id).unwrap();
        session.push_message(json!({"role": "user", "content": []}));

        database.save(&session, None).unwrap();

        assert_eq!(
            database.load_engine_decisions(session_id).unwrap(),
            numbered(&written, FIRST_SEQ),
            "{REWRITE_KEEPS_ROWS}"
        );
    }

    #[test]
    fn deleting_a_session_removes_its_decisions() {
        let (_temp, mut database, session_id) = open();
        database
            .insert_engine_decisions(
                session_id,
                &[decision(StoredDecisionOutcome::Acted, history())],
            )
            .unwrap();

        database.delete(session_id, None).unwrap();

        assert!(seqs(&database, session_id).is_empty(), "{CASCADE}");
    }

    #[test_case(UNKNOWN_OUTCOME, EMPTY_DETAILS, Some(0), OUTCOME_FIELD ; "unknown_outcome")]
    #[test_case(ACTED, INVALID_DETAILS, Some(0), DETAILS_FIELD ; "invalid_json_details")]
    #[test_case(ACTED, OBJECT_DETAILS, Some(0), DETAILS_FIELD ; "details_not_a_list")]
    #[test_case(ACTED, EMPTY_DETAILS, None, ANCHOR_FIELD ; "missing_anchor")]
    fn a_corrupt_row_loads_as_corrupt(
        outcome: &str,
        details: &str,
        history_len: Option<i64>,
        expected_field: &'static str,
    ) {
        let (_temp, database, session_id) = open();
        database
            .connection()
            .execute_batch("PRAGMA ignore_check_constraints = ON")
            .unwrap();
        insert_raw(&database, session_id, outcome, details, None, history_len).unwrap();

        let error = database.load_engine_decisions(session_id).unwrap_err();

        assert!(
            matches!(error, SessionError::CorruptDatabaseValue { field, .. } if field == expected_field),
            "{CORRUPT_ROW}: {error}"
        );
    }
}
