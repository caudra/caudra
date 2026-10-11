//! Autopilot's persisted half: the engagement a session's meta carries across
//! resumes, and the log of the decisions it made. Decisions belong to a session
//! and go with it through the schema's cascade. Each session keeps its newest
//! [`MAX_DECISIONS_PER_SESSION`], a session rewrite keeps them, and a fork
//! starts with none.

use rusqlite::{Row, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::StorageError;
use crate::id::CaudraId;
use crate::sessions::{
    SessionDatabase, SessionError, StoredTokenUsage, deserialize_json, from_i64, from_i64_usize,
    to_i64,
};

/// Decisions each session keeps. An insert past it drops the oldest.
pub const MAX_DECISIONS_PER_SESSION: usize = 2000;
const DECISION_COLUMNS: &str =
    "seq, at, kind, action, subject, reason, agent, tool_use_id, history_len, model, usage, cost";

/// Kind and action are the agent's names and carry no CHECK, so a new one
/// needs no migration.
pub(crate) const TABLE: &str = r#"
CREATE TABLE autopilot_decisions (
    session_id  BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL CHECK(seq >= 0),
    at          INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    action      TEXT NOT NULL,
    subject     TEXT NOT NULL,
    reason      TEXT NOT NULL,
    agent       TEXT,
    tool_use_id TEXT,
    history_len INTEGER NOT NULL CHECK(history_len >= 0),
    model       TEXT,
    usage       TEXT CHECK(usage IS NULL OR json_valid(usage)),
    cost        REAL,
    PRIMARY KEY(session_id, seq)
) STRICT;
"#;

/// Who ended an engagement by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutopilotStopper {
    /// `/autopilot off`.
    Command,
    /// Esc Esc, or Ctrl-C while streaming.
    Interrupt,
    /// The question form's Cancel.
    QuestionCancel,
    Sdk,
}

/// The limit an engagement ran into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutopilotBudget {
    Continuations,
    Cost,
    Time,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum AutopilotEnd {
    Stopped { by: AutopilotStopper },
    GoalMet,
    GoalImpossible,
    Budget { kind: AutopilotBudget },
    Error { message: String },
}

/// An engagement as the session was last written. The limits are not stored:
/// they come from config, so a resume measures this spend against the limits
/// in force then.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredAutopilot {
    pub engaged: bool,
    /// Whether the active goal is the one autopilot set.
    pub owns_goal: bool,
    /// Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engaged_at_ms: Option<u64>,
    pub continuations: u32,
    /// API-billed spend, the part the cost limit caps.
    pub cost_usd: f64,
    /// Spend a subscription covered: shown, never capped.
    #[serde(default)]
    pub subscription_cost_usd: f64,
    /// Accumulated rather than measured from `engaged_at_ms`, so the time a
    /// session spends closed is not counted against it.
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_end: Option<AutopilotEnd>,
}

/// One decision autopilot made.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredAutopilotDecision {
    pub seq: u64,
    /// Unix milliseconds.
    pub at_ms: u64,
    pub kind: String,
    pub action: String,
    pub subject: String,
    pub reason: String,
    /// The subagent it was made for, `None` for the main agent.
    pub agent: Option<String>,
    pub tool_use_id: Option<String>,
    /// History items the session held when it was made, which places the
    /// decision among them on restore.
    pub history_len: usize,
    pub model: Option<String>,
    pub usage: Option<StoredTokenUsage>,
    pub cost: Option<f64>,
}

impl SessionDatabase {
    /// Appends a decision, dropping the session's oldest past
    /// [`MAX_DECISIONS_PER_SESSION`] in the same transaction.
    pub fn insert_autopilot_decision(
        &self,
        session_id: CaudraId,
        decision: &StoredAutopilotDecision,
    ) -> Result<(), SessionError> {
        let usage = decision
            .usage
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(StorageError::from)?;
        let session_id = session_id.as_bytes().as_slice();
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        transaction.execute(
            &format!(
                "INSERT INTO autopilot_decisions (session_id, {DECISION_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
            ),
            params![
                session_id,
                to_i64(decision.seq, "autopilot_decisions.seq")?,
                to_i64(decision.at_ms, "autopilot_decisions.at")?,
                decision.kind,
                decision.action,
                decision.subject,
                decision.reason,
                decision.agent,
                decision.tool_use_id,
                to_i64(decision.history_len, "autopilot_decisions.history_len")?,
                decision.model,
                usage,
                decision.cost,
            ],
        )?;
        transaction.execute(
            "DELETE FROM autopilot_decisions WHERE session_id = ?1 AND seq <= (\
                 SELECT seq FROM autopilot_decisions WHERE session_id = ?1 \
                 ORDER BY seq DESC LIMIT 1 OFFSET ?2)",
            params![
                session_id,
                to_i64(MAX_DECISIONS_PER_SESSION, "autopilot_decisions.limit")?
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// A session's decisions, oldest first.
    pub fn load_autopilot_decisions(
        &self,
        session_id: CaudraId,
    ) -> Result<Vec<StoredAutopilotDecision>, SessionError> {
        let mut statement = self.connection().prepare(&format!(
            "SELECT {DECISION_COLUMNS} FROM autopilot_decisions WHERE session_id = ?1 \
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

fn read_decision(row: &Row<'_>) -> Result<StoredAutopilotDecision, SessionError> {
    Ok(StoredAutopilotDecision {
        seq: from_i64(row.get(0)?, "autopilot_decisions.seq")?,
        at_ms: from_i64(row.get(1)?, "autopilot_decisions.at")?,
        kind: row.get(2)?,
        action: row.get(3)?,
        subject: row.get(4)?,
        reason: row.get(5)?,
        agent: row.get(6)?,
        tool_use_id: row.get(7)?,
        history_len: from_i64_usize(row.get(8)?, "autopilot_decisions.history_len")?,
        model: row.get(9)?,
        usage: row
            .get::<_, Option<String>>(10)?
            .map(|usage| deserialize_json(&usage, "autopilot_decisions.usage"))
            .transpose()?,
        cost: row.get(11)?,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::StateDir;
    use crate::sessions::{Session, SessionMeta};

    const CWD: &str = "/project";
    const MODEL: &str = "test/model";
    const DELEGATE_MODEL: &str = "test/delegate";
    const KIND: &str = "question";
    const ACTION: &str = "answer";
    const SUBJECT: &str = "Which database?";
    const REASON: &str = "the plan names SQLite";
    const AGENT: &str = "review";
    const TOOL_USE_ID: &str = "toolu_1";
    const AT_MS: u64 = 1_767_225_600_000;
    const HISTORY_LEN: usize = 12;
    const COST: f64 = 0.0125;
    const INPUT_TOKENS: u32 = 900;
    const OUTPUT_TOKENS: u32 = 40;
    const CACHE_READ_TOKENS: u32 = 600;
    const ENGAGED_AT_MS: u64 = 1_767_225_000_000;
    const CONTINUATIONS: u32 = 4;
    const COST_USD: f64 = 1.75;
    const SUBSCRIPTION_COST_USD: f64 = 0.5;
    const ELAPSED_MS: u64 = 90_000;
    const EXTRA_DECISIONS: usize = 3;
    const OTHER_SESSION_DECISIONS: usize = 2;
    const META_BEFORE_AUTOPILOT: &str = r#"{"context_size":1200,"turns":3}"#;
    const LOADS_IN_ORDER: &str = "decisions must load whole and in seq order";
    const TRIM_KEEPS_NEWEST: &str = "an insert must keep only the session's newest decisions";
    const TRIM_PER_SESSION: &str = "trimming one session must leave the others alone";
    const REWRITE_KEEPS_ROWS: &str = "saving a session must keep its autopilot decisions";
    const CASCADE: &str = "autopilot decisions must go with their session";
    const META_ROUND_TRIPS: &str = "the engagement must survive a save and a load";
    const OLD_META_DISENGAGED: &str = "a meta saved before autopilot must load without one";

    type TestSession = Session<Value, Value, Value>;

    fn open() -> (TempDir, SessionDatabase, CaudraId) {
        let temp = TempDir::new().unwrap();
        let mut database =
            SessionDatabase::open(&StateDir::from_path(temp.path().to_path_buf())).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        (temp, database, session.id)
    }

    fn decision(seq: u64) -> StoredAutopilotDecision {
        StoredAutopilotDecision {
            seq,
            at_ms: AT_MS,
            kind: KIND.into(),
            action: ACTION.into(),
            subject: SUBJECT.into(),
            reason: REASON.into(),
            agent: None,
            tool_use_id: None,
            history_len: HISTORY_LEN,
            model: None,
            usage: None,
            cost: None,
        }
    }

    fn priced(seq: u64) -> StoredAutopilotDecision {
        StoredAutopilotDecision {
            agent: Some(AGENT.into()),
            tool_use_id: Some(TOOL_USE_ID.into()),
            model: Some(DELEGATE_MODEL.into()),
            usage: Some(StoredTokenUsage {
                input: INPUT_TOKENS,
                output: OUTPUT_TOKENS,
                cache_read: CACHE_READ_TOKENS,
                cost: Some(COST),
                ..StoredTokenUsage::default()
            }),
            cost: Some(COST),
            ..decision(seq)
        }
    }

    /// Fills a session with `count` decisions in one statement, so a trim test
    /// does not pay a commit per row.
    fn seed(database: &SessionDatabase, session_id: CaudraId, count: usize) {
        database
            .connection()
            .execute(
                "WITH RECURSIVE seqs(seq) AS (SELECT 0 UNION ALL SELECT seq + 1 FROM seqs \
                     WHERE seq + 1 < ?2) \
                 INSERT INTO autopilot_decisions \
                     (session_id, seq, at, kind, action, subject, reason, history_len) \
                 SELECT ?1, seq, ?3, ?4, ?5, ?6, ?7, ?8 FROM seqs",
                params![
                    session_id.as_bytes().as_slice(),
                    to_i64(count, "count").unwrap(),
                    to_i64(AT_MS, "at").unwrap(),
                    KIND,
                    ACTION,
                    SUBJECT,
                    REASON,
                    to_i64(HISTORY_LEN, "history_len").unwrap(),
                ],
            )
            .unwrap();
    }

    fn seqs(database: &SessionDatabase, session_id: CaudraId) -> Vec<u64> {
        database
            .load_autopilot_decisions(session_id)
            .unwrap()
            .iter()
            .map(|decision| decision.seq)
            .collect()
    }

    fn engagement(last_end: Option<AutopilotEnd>) -> StoredAutopilot {
        StoredAutopilot {
            engaged: last_end.is_none(),
            owns_goal: true,
            engaged_at_ms: Some(ENGAGED_AT_MS),
            continuations: CONTINUATIONS,
            cost_usd: COST_USD,
            subscription_cost_usd: SUBSCRIPTION_COST_USD,
            elapsed_ms: ELAPSED_MS,
            last_end,
        }
    }

    #[test]
    fn decisions_load_whole_in_seq_order() {
        let (_temp, database, session_id) = open();
        database
            .insert_autopilot_decision(session_id, &priced(1))
            .unwrap();
        database
            .insert_autopilot_decision(session_id, &decision(0))
            .unwrap();

        assert_eq!(
            database.load_autopilot_decisions(session_id).unwrap(),
            vec![decision(0), priced(1)],
            "{LOADS_IN_ORDER}"
        );
    }

    #[test]
    fn an_insert_keeps_only_the_newest_decisions() {
        let (_temp, mut database, session_id) = open();
        let other = TestSession::new(MODEL, CWD);
        database.save(&other, None).unwrap();
        seed(&database, other.id, OTHER_SESSION_DECISIONS);
        let newest = MAX_DECISIONS_PER_SESSION + EXTRA_DECISIONS - 1;
        seed(&database, session_id, newest);

        database
            .insert_autopilot_decision(session_id, &decision(newest as u64))
            .unwrap();

        let kept = seqs(&database, session_id);
        assert_eq!(kept.len(), MAX_DECISIONS_PER_SESSION, "{TRIM_KEEPS_NEWEST}");
        assert_eq!(
            (kept.first().copied(), kept.last().copied()),
            (Some(EXTRA_DECISIONS as u64), Some(newest as u64)),
            "{TRIM_KEEPS_NEWEST}"
        );
        assert_eq!(
            seqs(&database, other.id).len(),
            OTHER_SESSION_DECISIONS,
            "{TRIM_PER_SESSION}"
        );
    }

    #[test]
    fn a_rewritten_session_keeps_its_decisions() {
        let (_temp, mut database, session_id) = open();
        database
            .insert_autopilot_decision(session_id, &priced(0))
            .unwrap();
        let mut session = database.load::<Value, Value, Value>(session_id).unwrap();
        session.push_message(json!({"role": "user", "content": []}));

        database.save(&session, None).unwrap();

        assert_eq!(
            database.load_autopilot_decisions(session_id).unwrap(),
            vec![priced(0)],
            "{REWRITE_KEEPS_ROWS}"
        );
    }

    #[test]
    fn deleting_a_session_removes_its_decisions() {
        let (_temp, mut database, session_id) = open();
        database
            .insert_autopilot_decision(session_id, &priced(0))
            .unwrap();

        database.delete(session_id, None).unwrap();

        assert!(seqs(&database, session_id).is_empty(), "{CASCADE}");
    }

    #[test_case(None ; "engaged")]
    #[test_case(Some(AutopilotEnd::Stopped { by: AutopilotStopper::QuestionCancel }) ; "stopped")]
    #[test_case(Some(AutopilotEnd::GoalMet) ; "goal_met")]
    #[test_case(Some(AutopilotEnd::GoalImpossible) ; "goal_impossible")]
    #[test_case(Some(AutopilotEnd::Budget { kind: AutopilotBudget::Cost }) ; "budget")]
    #[test_case(Some(AutopilotEnd::Error { message: REASON.into() }) ; "error")]
    fn an_engagement_round_trips_through_session_meta(last_end: Option<AutopilotEnd>) {
        let (_temp, mut database, _) = open();
        let mut session = TestSession::new(MODEL, CWD);
        session.meta.autopilot = Some(Box::new(engagement(last_end)));
        database.save(&session, None).unwrap();

        let loaded = database.load::<Value, Value, Value>(session.id).unwrap();

        assert_eq!(
            loaded.meta.autopilot, session.meta.autopilot,
            "{META_ROUND_TRIPS}"
        );
    }

    #[test_case(AutopilotEnd::Stopped { by: AutopilotStopper::QuestionCancel }, r#"{"reason":"stopped","by":"question_cancel"}"# ; "stopped")]
    #[test_case(AutopilotEnd::GoalImpossible, r#"{"reason":"goal_impossible"}"# ; "goal_impossible")]
    #[test_case(AutopilotEnd::Budget { kind: AutopilotBudget::Continuations }, r#"{"reason":"budget","kind":"continuations"}"# ; "budget")]
    fn an_end_keeps_its_stored_shape(end: AutopilotEnd, stored: &str) {
        assert_eq!(serde_json::to_string(&end).unwrap(), stored);
        assert_eq!(serde_json::from_str::<AutopilotEnd>(stored).unwrap(), end);
    }

    #[test]
    fn a_meta_saved_before_autopilot_has_no_engagement() {
        let meta: SessionMeta = serde_json::from_str(META_BEFORE_AUTOPILOT).unwrap();

        assert!(meta.autopilot.is_none(), "{OLD_META_DISENGAGED}");
    }
}
