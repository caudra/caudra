//! Autopilot's persisted half: the engagement a session's meta carries across
//! resumes, the engagements it ran, and the log of the decisions it made. Both
//! logs belong to a session and go with it through the schema's cascade. Each
//! session keeps its newest [`MAX_DECISIONS_PER_SESSION`] decisions and
//! [`MAX_ENGAGEMENTS_PER_SESSION`] engagements, a session rewrite keeps them,
//! and a fork starts with none.

use rusqlite::{Row, Transaction, TransactionBehavior, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::StorageError;
use crate::id::CaudraId;
use crate::sessions::{
    SessionDatabase, SessionError, StoredTokenUsage, deserialize_json, from_i64, from_i64_usize,
    to_i64,
};

/// Decisions each session keeps. An insert past it drops the oldest.
pub const MAX_DECISIONS_PER_SESSION: usize = 2000;
/// Engagements each session keeps. A new one past it drops the oldest.
pub const MAX_ENGAGEMENTS_PER_SESSION: usize = 200;
const DECISION_COLUMNS: &str = "seq, at, kind, action, subject, reason, agent, tool_use_id, \
     history_len, model, usage, cost, engagement, item, payload, outcome";
const ENGAGEMENT_COLUMNS: &str = "engagement, engaged_at, engaged_history_len, goal, owns_goal, \
     ended_at, ended_history_len, end, continuations, cost_usd, subscription_cost_usd, \
     elapsed_ms, retries";
const SEQ_FIELD: &str = "autopilot_decisions.seq";
const AT_FIELD: &str = "autopilot_decisions.at";
const HISTORY_LEN_FIELD: &str = "autopilot_decisions.history_len";
const USAGE_FIELD: &str = "autopilot_decisions.usage";
const PAYLOAD_FIELD: &str = "autopilot_decisions.payload";
const OUTCOME_FIELD: &str = "autopilot_decisions.outcome";
const DECISIONS_LIMIT_FIELD: &str = "autopilot_decisions.limit";
const ENGAGED_AT_FIELD: &str = "autopilot_engagements.engaged_at";
const ENGAGED_HISTORY_LEN_FIELD: &str = "autopilot_engagements.engaged_history_len";
const ENDED_AT_FIELD: &str = "autopilot_engagements.ended_at";
const ENDED_HISTORY_LEN_FIELD: &str = "autopilot_engagements.ended_history_len";
const END_FIELD: &str = "autopilot_engagements.end";
const ELAPSED_FIELD: &str = "autopilot_engagements.elapsed_ms";
const ENGAGEMENTS_LIMIT_FIELD: &str = "autopilot_engagements.limit";

/// Both logs as a fresh database creates them. A literal, so the schema 27
/// step can splice it into its own SQL.
macro_rules! tables {
    () => {
        r#"
CREATE TABLE autopilot_decisions (
    session_id  BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL CHECK(seq >= 0),
    at          INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    action      TEXT,
    subject     TEXT NOT NULL,
    reason      TEXT NOT NULL,
    agent       TEXT,
    tool_use_id TEXT,
    history_len INTEGER NOT NULL CHECK(history_len >= 0),
    model       TEXT,
    usage       TEXT CHECK(usage IS NULL OR json_valid(usage)),
    cost        REAL,
    engagement  INTEGER CHECK(engagement IS NULL OR engagement > 0),
    item        TEXT,
    payload     TEXT CHECK(payload IS NULL OR json_valid(payload)),
    outcome     TEXT CHECK(outcome IS NULL OR json_valid(outcome)),
    PRIMARY KEY(session_id, seq)
) STRICT;

CREATE TABLE autopilot_engagements (
    session_id            BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    engagement            INTEGER NOT NULL CHECK(engagement > 0),
    engaged_at            INTEGER NOT NULL,
    engaged_history_len   INTEGER NOT NULL CHECK(engaged_history_len >= 0),
    goal                  TEXT,
    owns_goal             INTEGER NOT NULL CHECK(owns_goal IN (0, 1)),
    ended_at              INTEGER,
    ended_history_len     INTEGER CHECK(ended_history_len IS NULL OR ended_history_len >= 0),
    end                   TEXT CHECK(end IS NULL OR json_valid(end)),
    continuations         INTEGER NOT NULL CHECK(continuations >= 0),
    cost_usd              REAL NOT NULL,
    subscription_cost_usd REAL NOT NULL,
    elapsed_ms            INTEGER NOT NULL CHECK(elapsed_ms >= 0),
    retries               INTEGER NOT NULL CHECK(retries >= 0),
    PRIMARY KEY(session_id, engagement)
) STRICT;
"#
    };
}

/// Kind and action are the agent's names and carry no CHECK, so a new one
/// needs no migration. There is no key from a decision to its engagement,
/// because the two logs trim independently.
pub(crate) const TABLES: &str = tables!();

/// `autopilot_decisions` as schema 24 created it. Only that step still does:
/// the schema 27 step rebuilds it as [`TABLES`] has it.
pub(crate) const DECISIONS_TABLE_V24: &str = r#"
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

/// The decision log is rebuilt rather than altered because `ALTER TABLE ADD
/// COLUMN` appends past the table constraints, and a migrated schema has to
/// read back byte for byte as a fresh one. Earlier rows keep NULL in the new
/// columns.
pub(crate) const AUDIT_MIGRATION: &str = concat!(
    "\nALTER TABLE autopilot_decisions RENAME TO autopilot_decisions_v26;\n",
    tables!(),
    r#"
INSERT INTO autopilot_decisions (
    session_id, seq, at, kind, action, subject, reason, agent, tool_use_id, history_len, model,
    usage, cost
)
SELECT session_id, seq, at, kind, action, subject, reason, agent, tool_use_id, history_len, model,
    usage, cost
FROM autopilot_decisions_v26;

DROP TABLE autopilot_decisions_v26;
"#
);

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
    /// The session's latest engagement, counted from 1. `0` before the first,
    /// and on a meta written before engagements were numbered.
    #[serde(default)]
    pub engagement: u32,
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
    /// Main-agent runs retried after an error.
    #[serde(default)]
    pub retries: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_end: Option<AutopilotEnd>,
}

/// What autopilot's action carried beside its reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum DecisionPayload {
    /// A deny's or a continue's guidance.
    Guidance(String),
    /// A reply's text.
    Text(String),
    /// A revise's feedback.
    Feedback(String),
    /// One list of labels per question, in order.
    Answers(Vec<Vec<String>>),
}

/// What became of a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DecisionOutcome {
    /// Autopilot's action took effect.
    Applied,
    /// Someone answered before autopilot, so its decision went unused.
    HumanFirst { answer: String },
    /// The human claimed it while autopilot was deciding.
    TakenOver,
    /// A reload or a matching rule settled it first.
    Superseded,
    /// The run stopped before anyone answered.
    Cancelled,
    /// Left to the human, who then answered.
    Answered { answer: String },
    /// Left to the human, with nobody there to ask.
    DeniedUnattended,
}

/// One decision autopilot made, or stood down from.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredAutopilotDecision {
    pub seq: u64,
    /// Unix milliseconds.
    pub at_ms: u64,
    pub kind: String,
    /// `None` when autopilot stood down before it answered.
    pub action: Option<String>,
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
    /// The engagement it was made in. This and the fields below are `None` on
    /// rows written before schema 27.
    pub engagement: Option<u32>,
    /// What the delegate was shown.
    pub item: Option<String>,
    pub payload: Option<DecisionPayload>,
    pub outcome: Option<DecisionOutcome>,
}

/// One engagement, from when it was engaged to how it ended.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredAutopilotEngagement {
    /// Counted from 1 within the session.
    pub engagement: u32,
    /// Unix milliseconds.
    pub engaged_at_ms: u64,
    /// History items the session held when it was engaged.
    pub engaged_history_len: usize,
    pub goal: Option<String>,
    /// Whether autopilot set that goal.
    pub owns_goal: bool,
    /// Unix milliseconds. This and the two below are `None` while it runs.
    pub ended_at_ms: Option<u64>,
    pub ended_history_len: Option<usize>,
    pub end: Option<AutopilotEnd>,
    pub continuations: u32,
    pub cost_usd: f64,
    pub subscription_cost_usd: f64,
    pub elapsed_ms: u64,
    /// Main-agent runs retried after an error.
    pub retries: u32,
}

impl SessionDatabase {
    /// Appends a decision, dropping the session's oldest past
    /// [`MAX_DECISIONS_PER_SESSION`] in the same transaction.
    pub fn insert_autopilot_decision(
        &self,
        session_id: CaudraId,
        decision: &StoredAutopilotDecision,
    ) -> Result<(), SessionError> {
        let usage = to_json(decision.usage.as_ref())?;
        let payload = to_json(decision.payload.as_ref())?;
        let outcome = to_json(decision.outcome.as_ref())?;
        let session_id = session_id.as_bytes().as_slice();
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        transaction.execute(
            &format!(
                "INSERT INTO autopilot_decisions (session_id, {DECISION_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
                 ?17)"
            ),
            params![
                session_id,
                to_i64(decision.seq, SEQ_FIELD)?,
                to_i64(decision.at_ms, AT_FIELD)?,
                decision.kind,
                decision.action,
                decision.subject,
                decision.reason,
                decision.agent,
                decision.tool_use_id,
                to_i64(decision.history_len, HISTORY_LEN_FIELD)?,
                decision.model,
                usage,
                decision.cost,
                decision.engagement,
                decision.item,
                payload,
                outcome,
            ],
        )?;
        transaction.execute(
            "DELETE FROM autopilot_decisions WHERE session_id = ?1 AND seq <= (\
                 SELECT seq FROM autopilot_decisions WHERE session_id = ?1 \
                 ORDER BY seq DESC LIMIT 1 OFFSET ?2)",
            params![
                session_id,
                to_i64(MAX_DECISIONS_PER_SESSION, DECISIONS_LIMIT_FIELD)?
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Records what became of decision `seq`. `false` when the session has no
    /// decision under it, such as one the trim already dropped.
    pub fn settle_autopilot_decision(
        &self,
        session_id: CaudraId,
        seq: u64,
        outcome: &DecisionOutcome,
    ) -> Result<bool, SessionError> {
        let outcome = serde_json::to_string(outcome).map_err(StorageError::from)?;
        let updated = self.connection().execute(
            "UPDATE autopilot_decisions SET outcome = ?3 WHERE session_id = ?1 AND seq = ?2",
            params![
                session_id.as_bytes().as_slice(),
                to_i64(seq, SEQ_FIELD)?,
                outcome
            ],
        )?;
        Ok(updated > 0)
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
        statement
            .query_and_then(params![session_id.as_bytes().as_slice()], read_decision)?
            .collect()
    }

    /// Up to `limit` of a session's decisions, newest first: only those made
    /// in `engagement`, and only those before `before_seq`, when given.
    pub fn page_autopilot_decisions(
        &self,
        session_id: CaudraId,
        engagement: Option<u32>,
        before_seq: Option<u64>,
        limit: usize,
    ) -> Result<Vec<StoredAutopilotDecision>, SessionError> {
        let before_seq = before_seq.map(|seq| to_i64(seq, SEQ_FIELD)).transpose()?;
        let mut statement = self.connection().prepare(&format!(
            "SELECT {DECISION_COLUMNS} FROM autopilot_decisions WHERE session_id = ?1 \
             AND (?2 IS NULL OR engagement = ?2) AND (?3 IS NULL OR seq < ?3) \
             ORDER BY seq DESC LIMIT ?4"
        ))?;
        statement
            .query_and_then(
                params![
                    session_id.as_bytes().as_slice(),
                    engagement,
                    before_seq,
                    to_i64(limit, DECISIONS_LIMIT_FIELD)?
                ],
                read_decision,
            )?
            .collect()
    }

    /// Records an engagement's start, dropping the session's oldest past
    /// [`MAX_ENGAGEMENTS_PER_SESSION`] in the same transaction. It replaces a
    /// row already under its number, which a crash before the session's meta
    /// was saved can leave behind.
    pub fn begin_autopilot_engagement(
        &self,
        session_id: CaudraId,
        engagement: &StoredAutopilotEngagement,
    ) -> Result<(), SessionError> {
        let end = to_json(engagement.end.as_ref())?;
        let session_id = session_id.as_bytes().as_slice();
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        transaction.execute(
            &format!(
                "INSERT OR REPLACE INTO autopilot_engagements (session_id, {ENGAGEMENT_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
            ),
            params![
                session_id,
                engagement.engagement,
                to_i64(engagement.engaged_at_ms, ENGAGED_AT_FIELD)?,
                to_i64(engagement.engaged_history_len, ENGAGED_HISTORY_LEN_FIELD)?,
                engagement.goal,
                engagement.owns_goal,
                optional_i64(engagement.ended_at_ms, ENDED_AT_FIELD)?,
                optional_i64(engagement.ended_history_len, ENDED_HISTORY_LEN_FIELD)?,
                end,
                engagement.continuations,
                engagement.cost_usd,
                engagement.subscription_cost_usd,
                to_i64(engagement.elapsed_ms, ELAPSED_FIELD)?,
                engagement.retries,
            ],
        )?;
        transaction.execute(
            "DELETE FROM autopilot_engagements WHERE session_id = ?1 AND engagement <= (\
                 SELECT engagement FROM autopilot_engagements WHERE session_id = ?1 \
                 ORDER BY engagement DESC LIMIT 1 OFFSET ?2)",
            params![
                session_id,
                to_i64(MAX_ENGAGEMENTS_PER_SESSION, ENGAGEMENTS_LIMIT_FIELD)?
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Records how an engagement ended and what it spent. `false` when the
    /// session has no engagement under its number.
    pub fn end_autopilot_engagement(
        &self,
        session_id: CaudraId,
        engagement: &StoredAutopilotEngagement,
    ) -> Result<bool, SessionError> {
        let end = to_json(engagement.end.as_ref())?;
        let updated = self.connection().execute(
            "UPDATE autopilot_engagements SET ended_at = ?3, ended_history_len = ?4, end = ?5, \
             continuations = ?6, cost_usd = ?7, subscription_cost_usd = ?8, elapsed_ms = ?9, \
             retries = ?10 WHERE session_id = ?1 AND engagement = ?2",
            params![
                session_id.as_bytes().as_slice(),
                engagement.engagement,
                optional_i64(engagement.ended_at_ms, ENDED_AT_FIELD)?,
                optional_i64(engagement.ended_history_len, ENDED_HISTORY_LEN_FIELD)?,
                end,
                engagement.continuations,
                engagement.cost_usd,
                engagement.subscription_cost_usd,
                to_i64(engagement.elapsed_ms, ELAPSED_FIELD)?,
                engagement.retries,
            ],
        )?;
        Ok(updated > 0)
    }

    /// A session's engagements, oldest first.
    pub fn load_autopilot_engagements(
        &self,
        session_id: CaudraId,
    ) -> Result<Vec<StoredAutopilotEngagement>, SessionError> {
        let mut statement = self.connection().prepare(&format!(
            "SELECT {ENGAGEMENT_COLUMNS} FROM autopilot_engagements WHERE session_id = ?1 \
             ORDER BY engagement"
        ))?;
        statement
            .query_and_then(params![session_id.as_bytes().as_slice()], read_engagement)?
            .collect()
    }
}

fn read_decision(row: &Row<'_>) -> Result<StoredAutopilotDecision, SessionError> {
    Ok(StoredAutopilotDecision {
        seq: from_i64(row.get(0)?, SEQ_FIELD)?,
        at_ms: from_i64(row.get(1)?, AT_FIELD)?,
        kind: row.get(2)?,
        action: row.get(3)?,
        subject: row.get(4)?,
        reason: row.get(5)?,
        agent: row.get(6)?,
        tool_use_id: row.get(7)?,
        history_len: from_i64_usize(row.get(8)?, HISTORY_LEN_FIELD)?,
        model: row.get(9)?,
        usage: from_json(row, 10, USAGE_FIELD)?,
        cost: row.get(11)?,
        engagement: row.get(12)?,
        item: row.get(13)?,
        payload: from_json(row, 14, PAYLOAD_FIELD)?,
        outcome: from_json(row, 15, OUTCOME_FIELD)?,
    })
}

fn read_engagement(row: &Row<'_>) -> Result<StoredAutopilotEngagement, SessionError> {
    Ok(StoredAutopilotEngagement {
        engagement: row.get(0)?,
        engaged_at_ms: from_i64(row.get(1)?, ENGAGED_AT_FIELD)?,
        engaged_history_len: from_i64_usize(row.get(2)?, ENGAGED_HISTORY_LEN_FIELD)?,
        goal: row.get(3)?,
        owns_goal: row.get(4)?,
        ended_at_ms: row
            .get::<_, Option<i64>>(5)?
            .map(|at| from_i64(at, ENDED_AT_FIELD))
            .transpose()?,
        ended_history_len: row
            .get::<_, Option<i64>>(6)?
            .map(|len| from_i64_usize(len, ENDED_HISTORY_LEN_FIELD))
            .transpose()?,
        end: from_json(row, 7, END_FIELD)?,
        continuations: row.get(8)?,
        cost_usd: row.get(9)?,
        subscription_cost_usd: row.get(10)?,
        elapsed_ms: from_i64(row.get(11)?, ELAPSED_FIELD)?,
        retries: row.get(12)?,
    })
}

fn to_json<T: Serialize>(value: Option<&T>) -> Result<Option<String>, StorageError> {
    value
        .map(serde_json::to_string)
        .transpose()
        .map_err(StorageError::from)
}

fn from_json<T: DeserializeOwned>(
    row: &Row<'_>,
    index: usize,
    field: &'static str,
) -> Result<Option<T>, SessionError> {
    row.get::<_, Option<String>>(index)?
        .map(|text| deserialize_json(&text, field))
        .transpose()
}

fn optional_i64(
    value: Option<impl TryInto<i64>>,
    field: &'static str,
) -> Result<Option<i64>, SessionError> {
    value.map(|value| to_i64(value, field)).transpose()
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
    const ESCALATE: &str = "escalate";
    const SUBJECT: &str = "Which database?";
    const REASON: &str = "the plan names SQLite";
    const AGENT: &str = "review";
    const TOOL_USE_ID: &str = "toolu_1";
    const ITEM: &str = "Which database?\n1. SQLite\n2. Postgres";
    const GUIDANCE: &str = "stay in the crate";
    const REPLY_TEXT: &str = "Yes, go ahead.";
    const FEEDBACK: &str = "add the tests";
    const ANSWER_LABEL: &str = "SQLite";
    const HUMAN_ANSWER: &str = "allowed once";
    const GOAL: &str = "the parser passes its tests";
    const AT_MS: u64 = 1_767_225_600_000;
    const HISTORY_LEN: usize = 12;
    const COST: f64 = 0.0125;
    const INPUT_TOKENS: u32 = 900;
    const OUTPUT_TOKENS: u32 = 40;
    const CACHE_READ_TOKENS: u32 = 600;
    const ENGAGED_AT_MS: u64 = 1_767_225_000_000;
    const ENDED_AT_MS: u64 = 1_767_226_000_000;
    const ENGAGED_HISTORY_LEN: usize = 4;
    const ENDED_HISTORY_LEN: usize = 30;
    const ENGAGEMENT: u32 = 3;
    const OTHER_ENGAGEMENT: u32 = 4;
    const CONTINUATIONS: u32 = 4;
    const COST_USD: f64 = 1.75;
    const SUBSCRIPTION_COST_USD: f64 = 0.5;
    const ELAPSED_MS: u64 = 90_000;
    const RETRIES: u32 = 2;
    const MISSING_SEQ: u64 = 99;
    const PAGED_DECISIONS: u64 = 6;
    const EXTRA_DECISIONS: usize = 3;
    const OTHER_SESSION_DECISIONS: usize = 2;
    const EXTRA_ENGAGEMENTS: usize = 3;
    const OTHER_SESSION_ENGAGEMENTS: usize = 2;
    const META_BEFORE_AUTOPILOT: &str = r#"{"context_size":1200,"turns":3}"#;
    const META_BEFORE_NUMBERING: &str =
        r#"{"engaged":true,"owns_goal":false,"continuations":1,"cost_usd":0.5,"elapsed_ms":1000}"#;
    const LOADS_IN_ORDER: &str = "decisions must load whole and in seq order";
    const AUDIT_ROUND_TRIPS: &str = "a decision must read back with what it said and came to";
    const SETTLES: &str = "a settle must record the outcome on its decision alone";
    const PAGES: &str = "a page must hold the matching decisions, newest first";
    const TRIM_KEEPS_NEWEST: &str = "an insert must keep only the session's newest decisions";
    const ENGAGEMENT_TRIM_KEEPS_NEWEST: &str =
        "a new engagement must keep only the session's newest engagements";
    const TRIM_PER_SESSION: &str = "trimming one session must leave the others alone";
    const ENDS: &str = "ending an engagement must record its end and spend";
    const REWRITE_KEEPS_ROWS: &str = "saving a session must keep its autopilot log";
    const CASCADE: &str = "the autopilot log must go with its session";
    const META_ROUND_TRIPS: &str = "the engagement must survive a save and a load";
    const OLD_META_DISENGAGED: &str = "a meta saved before autopilot must load without one";
    const OLD_META_UNNUMBERED: &str = "a meta saved before engagements were numbered must read 0";

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
            action: Some(ACTION.into()),
            subject: SUBJECT.into(),
            reason: REASON.into(),
            agent: None,
            tool_use_id: None,
            history_len: HISTORY_LEN,
            model: None,
            usage: None,
            cost: None,
            engagement: None,
            item: None,
            payload: None,
            outcome: None,
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
            engagement: Some(ENGAGEMENT),
            item: Some(ITEM.into()),
            ..decision(seq)
        }
    }

    fn in_engagement(seq: u64, engagement: u32) -> StoredAutopilotDecision {
        StoredAutopilotDecision {
            engagement: Some(engagement),
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

    /// Fills a session with engagements 1 to `count` in one statement.
    fn seed_engagements(database: &SessionDatabase, session_id: CaudraId, count: usize) {
        database
            .connection()
            .execute(
                "WITH RECURSIVE numbers(engagement) AS (SELECT 1 UNION ALL \
                     SELECT engagement + 1 FROM numbers WHERE engagement < ?2) \
                 INSERT INTO autopilot_engagements \
                     (session_id, engagement, engaged_at, engaged_history_len, owns_goal, \
                      continuations, cost_usd, subscription_cost_usd, elapsed_ms, retries) \
                 SELECT ?1, engagement, ?3, ?4, 0, 0, 0.0, 0.0, 0, 0 FROM numbers",
                params![
                    session_id.as_bytes().as_slice(),
                    to_i64(count, "count").unwrap(),
                    to_i64(ENGAGED_AT_MS, "engaged_at").unwrap(),
                    to_i64(ENGAGED_HISTORY_LEN, "engaged_history_len").unwrap(),
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

    fn engagement_numbers(database: &SessionDatabase, session_id: CaudraId) -> Vec<u32> {
        database
            .load_autopilot_engagements(session_id)
            .unwrap()
            .iter()
            .map(|engagement| engagement.engagement)
            .collect()
    }

    fn begun(engagement: u32) -> StoredAutopilotEngagement {
        StoredAutopilotEngagement {
            engagement,
            engaged_at_ms: ENGAGED_AT_MS,
            engaged_history_len: ENGAGED_HISTORY_LEN,
            goal: Some(GOAL.into()),
            owns_goal: true,
            ended_at_ms: None,
            ended_history_len: None,
            end: None,
            continuations: 0,
            cost_usd: 0.0,
            subscription_cost_usd: 0.0,
            elapsed_ms: 0,
            retries: 0,
        }
    }

    fn ended(engagement: u32) -> StoredAutopilotEngagement {
        StoredAutopilotEngagement {
            ended_at_ms: Some(ENDED_AT_MS),
            ended_history_len: Some(ENDED_HISTORY_LEN),
            end: Some(AutopilotEnd::Budget {
                kind: AutopilotBudget::Cost,
            }),
            continuations: CONTINUATIONS,
            cost_usd: COST_USD,
            subscription_cost_usd: SUBSCRIPTION_COST_USD,
            elapsed_ms: ELAPSED_MS,
            retries: RETRIES,
            ..begun(engagement)
        }
    }

    fn engagement(last_end: Option<AutopilotEnd>) -> StoredAutopilot {
        StoredAutopilot {
            engaged: last_end.is_none(),
            engagement: ENGAGEMENT,
            owns_goal: true,
            engaged_at_ms: Some(ENGAGED_AT_MS),
            continuations: CONTINUATIONS,
            cost_usd: COST_USD,
            subscription_cost_usd: SUBSCRIPTION_COST_USD,
            elapsed_ms: ELAPSED_MS,
            retries: RETRIES,
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

    #[test_case(Some(ACTION), Some(DecisionPayload::Guidance(GUIDANCE.into())), Some(DecisionOutcome::Applied) ; "guidance_applied")]
    #[test_case(Some(ACTION), Some(DecisionPayload::Text(REPLY_TEXT.into())), Some(DecisionOutcome::HumanFirst { answer: HUMAN_ANSWER.into() }) ; "text_human_first")]
    #[test_case(Some(ACTION), Some(DecisionPayload::Feedback(FEEDBACK.into())), None ; "feedback_handed_back")]
    #[test_case(Some(ACTION), Some(DecisionPayload::Answers(vec![vec![ANSWER_LABEL.into()], Vec::new()])), Some(DecisionOutcome::Applied) ; "answers_applied")]
    #[test_case(Some(ESCALATE), None, Some(DecisionOutcome::Answered { answer: HUMAN_ANSWER.into() }) ; "answered")]
    #[test_case(Some(ESCALATE), None, Some(DecisionOutcome::DeniedUnattended) ; "denied_unattended")]
    #[test_case(None, None, Some(DecisionOutcome::TakenOver) ; "taken_over")]
    #[test_case(None, None, Some(DecisionOutcome::Superseded) ; "superseded")]
    #[test_case(None, None, Some(DecisionOutcome::Cancelled) ; "cancelled")]
    fn an_audited_decision_round_trips(
        action: Option<&str>,
        payload: Option<DecisionPayload>,
        outcome: Option<DecisionOutcome>,
    ) {
        let (_temp, database, session_id) = open();
        let written = StoredAutopilotDecision {
            action: action.map(str::to_owned),
            payload,
            outcome,
            ..priced(0)
        };

        database
            .insert_autopilot_decision(session_id, &written)
            .unwrap();

        assert_eq!(
            database.load_autopilot_decisions(session_id).unwrap(),
            vec![written],
            "{AUDIT_ROUND_TRIPS}"
        );
    }

    #[test_case(DecisionPayload::Guidance(GUIDANCE.into()), r#"{"kind":"guidance","value":"stay in the crate"}"# ; "guidance")]
    #[test_case(DecisionPayload::Text(REPLY_TEXT.into()), r#"{"kind":"text","value":"Yes, go ahead."}"# ; "text")]
    #[test_case(DecisionPayload::Feedback(FEEDBACK.into()), r#"{"kind":"feedback","value":"add the tests"}"# ; "feedback")]
    #[test_case(DecisionPayload::Answers(vec![vec![ANSWER_LABEL.into()]]), r#"{"kind":"answers","value":[["SQLite"]]}"# ; "answers")]
    fn a_payload_keeps_its_stored_shape(payload: DecisionPayload, stored: &str) {
        assert_eq!(serde_json::to_string(&payload).unwrap(), stored);
        assert_eq!(
            serde_json::from_str::<DecisionPayload>(stored).unwrap(),
            payload
        );
    }

    #[test_case(DecisionOutcome::Applied, r#"{"status":"applied"}"# ; "applied")]
    #[test_case(DecisionOutcome::HumanFirst { answer: HUMAN_ANSWER.into() }, r#"{"status":"human_first","answer":"allowed once"}"# ; "human_first")]
    #[test_case(DecisionOutcome::TakenOver, r#"{"status":"taken_over"}"# ; "taken_over")]
    #[test_case(DecisionOutcome::Superseded, r#"{"status":"superseded"}"# ; "superseded")]
    #[test_case(DecisionOutcome::Cancelled, r#"{"status":"cancelled"}"# ; "cancelled")]
    #[test_case(DecisionOutcome::Answered { answer: HUMAN_ANSWER.into() }, r#"{"status":"answered","answer":"allowed once"}"# ; "answered")]
    #[test_case(DecisionOutcome::DeniedUnattended, r#"{"status":"denied_unattended"}"# ; "denied_unattended")]
    fn an_outcome_keeps_its_stored_shape(outcome: DecisionOutcome, stored: &str) {
        assert_eq!(serde_json::to_string(&outcome).unwrap(), stored);
        assert_eq!(
            serde_json::from_str::<DecisionOutcome>(stored).unwrap(),
            outcome
        );
    }

    #[test_case(0, true ; "a_stored_decision")]
    #[test_case(MISSING_SEQ, false ; "no_decision")]
    fn a_settle_records_the_outcome_on_its_decision(seq: u64, found: bool) {
        let (_temp, database, session_id) = open();
        database
            .insert_autopilot_decision(session_id, &decision(0))
            .unwrap();
        database
            .insert_autopilot_decision(session_id, &decision(1))
            .unwrap();
        let outcome = DecisionOutcome::Answered {
            answer: HUMAN_ANSWER.into(),
        };

        let settled = database
            .settle_autopilot_decision(session_id, seq, &outcome)
            .unwrap();

        let outcomes: Vec<_> = database
            .load_autopilot_decisions(session_id)
            .unwrap()
            .into_iter()
            .map(|decision| decision.outcome)
            .collect();
        let expected = if found {
            vec![Some(outcome), None]
        } else {
            vec![None, None]
        };
        assert_eq!((settled, outcomes), (found, expected), "{SETTLES}");
    }

    #[test_case(None, None, 10, &[5, 4, 3, 2, 1, 0] ; "everything")]
    #[test_case(None, None, 2, &[5, 4] ; "limited")]
    #[test_case(Some(ENGAGEMENT), None, 10, &[2, 1, 0] ; "one_engagement")]
    #[test_case(None, Some(4), 2, &[3, 2] ; "before_a_seq")]
    #[test_case(Some(OTHER_ENGAGEMENT), Some(5), 10, &[4, 3] ; "engagement_before_a_seq")]
    fn a_page_holds_the_matching_decisions_newest_first(
        engagement: Option<u32>,
        before_seq: Option<u64>,
        limit: usize,
        expected: &[u64],
    ) {
        let (_temp, database, session_id) = open();
        for seq in 0..PAGED_DECISIONS {
            let number = if seq < PAGED_DECISIONS / 2 {
                ENGAGEMENT
            } else {
                OTHER_ENGAGEMENT
            };
            database
                .insert_autopilot_decision(session_id, &in_engagement(seq, number))
                .unwrap();
        }

        let page: Vec<u64> = database
            .page_autopilot_decisions(session_id, engagement, before_seq, limit)
            .unwrap()
            .iter()
            .map(|decision| decision.seq)
            .collect();

        assert_eq!(page, expected, "{PAGES}");
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
    fn a_new_engagement_keeps_only_the_newest_engagements() {
        let (_temp, mut database, session_id) = open();
        let other = TestSession::new(MODEL, CWD);
        database.save(&other, None).unwrap();
        seed_engagements(&database, other.id, OTHER_SESSION_ENGAGEMENTS);
        let newest = MAX_ENGAGEMENTS_PER_SESSION + EXTRA_ENGAGEMENTS;
        seed_engagements(&database, session_id, newest - 1);

        database
            .begin_autopilot_engagement(session_id, &begun(newest as u32))
            .unwrap();

        let kept = engagement_numbers(&database, session_id);
        assert_eq!(
            kept.len(),
            MAX_ENGAGEMENTS_PER_SESSION,
            "{ENGAGEMENT_TRIM_KEEPS_NEWEST}"
        );
        assert_eq!(
            (kept.first().copied(), kept.last().copied()),
            (Some(EXTRA_ENGAGEMENTS as u32 + 1), Some(newest as u32)),
            "{ENGAGEMENT_TRIM_KEEPS_NEWEST}"
        );
        assert_eq!(
            engagement_numbers(&database, other.id).len(),
            OTHER_SESSION_ENGAGEMENTS,
            "{TRIM_PER_SESSION}"
        );
    }

    #[test_case(true ; "begun")]
    #[test_case(false ; "never_begun")]
    fn ending_an_engagement_records_its_end_and_spend(was_begun: bool) {
        let (_temp, database, session_id) = open();
        database
            .begin_autopilot_engagement(session_id, &begun(OTHER_ENGAGEMENT))
            .unwrap();
        if was_begun {
            database
                .begin_autopilot_engagement(session_id, &begun(ENGAGEMENT))
                .unwrap();
        }

        let updated = database
            .end_autopilot_engagement(session_id, &ended(ENGAGEMENT))
            .unwrap();

        let expected = if was_begun {
            vec![ended(ENGAGEMENT), begun(OTHER_ENGAGEMENT)]
        } else {
            vec![begun(OTHER_ENGAGEMENT)]
        };
        assert_eq!(
            (
                updated,
                database.load_autopilot_engagements(session_id).unwrap()
            ),
            (was_begun, expected),
            "{ENDS}"
        );
    }

    #[test]
    fn a_rewritten_session_keeps_its_log() {
        let (_temp, mut database, session_id) = open();
        database
            .insert_autopilot_decision(session_id, &priced(0))
            .unwrap();
        database
            .begin_autopilot_engagement(session_id, &begun(ENGAGEMENT))
            .unwrap();
        let mut session = database.load::<Value, Value, Value>(session_id).unwrap();
        session.push_message(json!({"role": "user", "content": []}));

        database.save(&session, None).unwrap();

        assert_eq!(
            (
                database.load_autopilot_decisions(session_id).unwrap(),
                database.load_autopilot_engagements(session_id).unwrap()
            ),
            (vec![priced(0)], vec![begun(ENGAGEMENT)]),
            "{REWRITE_KEEPS_ROWS}"
        );
    }

    #[test]
    fn deleting_a_session_removes_its_log() {
        let (_temp, mut database, session_id) = open();
        database
            .insert_autopilot_decision(session_id, &priced(0))
            .unwrap();
        database
            .begin_autopilot_engagement(session_id, &begun(ENGAGEMENT))
            .unwrap();

        database.delete(session_id, None).unwrap();

        assert!(seqs(&database, session_id).is_empty(), "{CASCADE}");
        assert!(
            engagement_numbers(&database, session_id).is_empty(),
            "{CASCADE}"
        );
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

    #[test]
    fn a_meta_saved_before_engagements_were_numbered_reads_zero() {
        let stored: StoredAutopilot = serde_json::from_str(META_BEFORE_NUMBERING).unwrap();

        assert_eq!(stored.engagement, 0, "{OLD_META_UNNUMBERED}");
    }
}
