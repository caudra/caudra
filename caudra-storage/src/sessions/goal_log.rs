//! The goal log: every goal a session set, from when it was set to how it
//! ended, and every check of it. Both logs belong to a session and go with it
//! through the schema's cascade. Each session keeps its newest
//! [`MAX_GOALS_PER_SESSION`] goals and [`MAX_GOAL_EVALUATIONS_PER_SESSION`]
//! evaluations, a session rewrite keeps them, and a fork starts with none.

use rusqlite::{Row, Transaction, TransactionBehavior, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::StorageError;
use crate::id::CaudraId;
use crate::sessions::{
    SessionDatabase, SessionError, StoredTokenUsage, deserialize_json, from_i64, from_i64_usize,
    to_i64,
};

/// Goals each session keeps. A new one past it drops the oldest.
pub const MAX_GOALS_PER_SESSION: usize = 200;
/// Evaluations each session keeps. An insert past it drops the oldest.
pub const MAX_GOAL_EVALUATIONS_PER_SESSION: usize = 2000;
const GOAL_COLUMNS: &str = "goal, condition, set_at, set_history_len, ended_at, \
     ended_history_len, end, evaluations, elapsed_ms, usage, cost_usd, subscription_cost_usd";
const EVALUATION_COLUMNS: &str = "seq, goal, evaluation, started_at, at, history_len, verdict, \
     reason, outcome, model, usage, cost";
const SET_AT_FIELD: &str = "goals.set_at";
const SET_HISTORY_LEN_FIELD: &str = "goals.set_history_len";
const ENDED_AT_FIELD: &str = "goals.ended_at";
const ENDED_HISTORY_LEN_FIELD: &str = "goals.ended_history_len";
const END_FIELD: &str = "goals.end";
const ELAPSED_FIELD: &str = "goals.elapsed_ms";
const GOAL_USAGE_FIELD: &str = "goals.usage";
const GOALS_LIMIT_FIELD: &str = "goals.limit";
const SEQ_FIELD: &str = "goal_evaluations.seq";
const STARTED_AT_FIELD: &str = "goal_evaluations.started_at";
const AT_FIELD: &str = "goal_evaluations.at";
const HISTORY_LEN_FIELD: &str = "goal_evaluations.history_len";
const OUTCOME_FIELD: &str = "goal_evaluations.outcome";
const EVALUATION_USAGE_FIELD: &str = "goal_evaluations.usage";
const EVALUATIONS_LIMIT_FIELD: &str = "goal_evaluations.limit";

/// Both logs as a fresh database creates them, and as the schema 28 step
/// adds them. There is no key from an evaluation to its goal, because the two
/// logs trim independently.
pub(crate) const TABLES: &str = r#"
CREATE TABLE goals (
    session_id            BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    goal                  INTEGER NOT NULL CHECK(goal > 0),
    condition             TEXT NOT NULL,
    set_at                INTEGER NOT NULL,
    set_history_len       INTEGER NOT NULL CHECK(set_history_len >= 0),
    ended_at              INTEGER,
    ended_history_len     INTEGER CHECK(ended_history_len IS NULL OR ended_history_len >= 0),
    end                   TEXT CHECK(end IS NULL OR json_valid(end)),
    evaluations           INTEGER NOT NULL CHECK(evaluations >= 0),
    elapsed_ms            INTEGER NOT NULL CHECK(elapsed_ms >= 0),
    usage                 TEXT NOT NULL CHECK(json_valid(usage)),
    cost_usd              REAL NOT NULL,
    subscription_cost_usd REAL NOT NULL,
    PRIMARY KEY(session_id, goal)
) STRICT;

CREATE TABLE goal_evaluations (
    session_id  BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL CHECK(seq >= 0),
    goal        INTEGER NOT NULL CHECK(goal >= 0),
    evaluation  INTEGER NOT NULL CHECK(evaluation >= 0),
    started_at  INTEGER NOT NULL,
    at          INTEGER NOT NULL,
    history_len INTEGER NOT NULL CHECK(history_len >= 0),
    verdict     TEXT,
    reason      TEXT NOT NULL,
    outcome     TEXT NOT NULL CHECK(json_valid(outcome)),
    model       TEXT,
    usage       TEXT CHECK(usage IS NULL OR json_valid(usage)),
    cost        REAL,
    PRIMARY KEY(session_id, seq)
) STRICT;
"#;

/// How a goal ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "end", rename_all = "snake_case")]
pub enum GoalEnd {
    Met,
    Impossible,
    /// `/goal clear`, an SDK clear, or a reset.
    Cleared,
    /// A new goal took its place.
    Replaced,
    /// The run failed and cleared it.
    Error {
        message: String,
    },
}

/// What became of one check of a goal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GoalEvaluationOutcome {
    /// The verdict took effect.
    Applied,
    /// An impossible verdict the second opinion overruled, applied as not met.
    Reconsidered,
    /// Work was still running when the verdict came, so the check waits.
    Deferred,
    /// The goal it judged was replaced or cleared before the verdict came.
    Stale,
    /// The prescreen judged the goal unlikely met, so the evaluator never ran.
    Skipped { pass_probability: f64 },
    /// The check failed. `applied` is whether its goal was still the active
    /// one, so the failure stood.
    Failed { message: String, applied: bool },
}

/// One goal, from when it was set to how it ended.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredGoalRecord {
    /// Counted from 1 within the session.
    pub goal: u32,
    pub condition: String,
    /// Unix milliseconds.
    pub set_at_ms: u64,
    /// History items the session held when it was set.
    pub set_history_len: usize,
    /// Unix milliseconds. This and the two below are `None` while it is
    /// active.
    pub ended_at_ms: Option<u64>,
    pub ended_history_len: Option<usize>,
    pub end: Option<GoalEnd>,
    pub evaluations: u32,
    pub elapsed_ms: u64,
    pub usage: StoredTokenUsage,
    pub cost_usd: f64,
    pub subscription_cost_usd: f64,
}

/// One check of a goal, whatever became of it.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredGoalEvaluation {
    /// Counted from 1 within the session, across its goals.
    pub seq: u64,
    /// The goal it judged, numbered as [`StoredGoalRecord::goal`].
    pub goal: u32,
    /// The goal's evaluation number, counted from 1.
    pub evaluation: u32,
    /// Unix milliseconds.
    pub started_at_ms: u64,
    /// Unix milliseconds.
    pub at_ms: u64,
    /// History items the session held when it was made, which places it among
    /// them on restore.
    pub history_len: usize,
    /// The evaluator's own verdict, before a second opinion overruled it.
    /// `None` when no verdict came.
    pub verdict: Option<String>,
    /// Empty when no verdict came.
    pub reason: String,
    pub outcome: GoalEvaluationOutcome,
    pub model: Option<String>,
    pub usage: Option<StoredTokenUsage>,
    pub cost: Option<f64>,
}

/// The numbering a session's meta carries, so goals and evaluations keep
/// counting across resumes and after the log trimmed its oldest rows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredGoalCursor {
    pub last_goal: u32,
    pub last_seq: u64,
}

/// One write the goal log owes, in the order it must land: a goal's end never
/// overtakes its start.
#[derive(Debug, Clone, PartialEq)]
pub enum GoalLogWrite {
    /// `set_history_len` is 0 until [`Self::stamp_history_len`].
    Begun(StoredGoalRecord),
    Evaluated(Box<StoredGoalEvaluation>),
    /// `ended_history_len` is `None` until [`Self::stamp_history_len`].
    Ended(StoredGoalRecord),
}

impl GoalLogWrite {
    /// Places a goal's start or end at `history_len`, the history the session
    /// holds as the write is drained. An evaluation is placed when it is made.
    pub fn stamp_history_len(&mut self, history_len: usize) {
        match self {
            Self::Begun(record) => record.set_history_len = history_len,
            Self::Ended(record) => record.ended_history_len = Some(history_len),
            Self::Evaluated(_) => {}
        }
    }

    /// Stamps every start and end drained together at `history_len`, or later
    /// when a check among them was placed later: a goal's end is journaled
    /// before the check that ended it, so it must not land before that check.
    pub fn stamp_batch(writes: &mut [Self], history_len: usize) {
        let history_len = writes
            .iter()
            .filter_map(|write| match write {
                Self::Evaluated(evaluation) => Some(evaluation.history_len),
                Self::Begun(_) | Self::Ended(_) => None,
            })
            .fold(history_len, usize::max);
        for write in writes {
            write.stamp_history_len(history_len);
        }
    }
}

impl SessionDatabase {
    /// Records a goal's start, dropping the session's oldest past
    /// [`MAX_GOALS_PER_SESSION`] in the same transaction. It replaces a row
    /// already under its number, which a crash before the session's meta was
    /// saved can leave behind.
    pub fn begin_goal(
        &self,
        session_id: CaudraId,
        goal: &StoredGoalRecord,
    ) -> Result<(), SessionError> {
        let end = to_json(goal.end.as_ref())?;
        let usage = serde_json::to_string(&goal.usage).map_err(StorageError::from)?;
        let session_id = session_id.as_bytes().as_slice();
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        transaction.execute(
            &format!(
                "INSERT OR REPLACE INTO goals (session_id, {GOAL_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
            ),
            params![
                session_id,
                goal.goal,
                goal.condition,
                to_i64(goal.set_at_ms, SET_AT_FIELD)?,
                to_i64(goal.set_history_len, SET_HISTORY_LEN_FIELD)?,
                optional_i64(goal.ended_at_ms, ENDED_AT_FIELD)?,
                optional_i64(goal.ended_history_len, ENDED_HISTORY_LEN_FIELD)?,
                end,
                goal.evaluations,
                to_i64(goal.elapsed_ms, ELAPSED_FIELD)?,
                usage,
                goal.cost_usd,
                goal.subscription_cost_usd,
            ],
        )?;
        transaction.execute(
            "DELETE FROM goals WHERE session_id = ?1 AND goal <= (\
                 SELECT goal FROM goals WHERE session_id = ?1 \
                 ORDER BY goal DESC LIMIT 1 OFFSET ?2)",
            params![
                session_id,
                to_i64(MAX_GOALS_PER_SESSION, GOALS_LIMIT_FIELD)?
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Records how a goal ended and what it spent. `false` when the session
    /// has no goal under its number, such as one the trim already dropped.
    pub fn end_goal(
        &self,
        session_id: CaudraId,
        goal: &StoredGoalRecord,
    ) -> Result<bool, SessionError> {
        let end = to_json(goal.end.as_ref())?;
        let usage = serde_json::to_string(&goal.usage).map_err(StorageError::from)?;
        let updated = self.connection().execute(
            "UPDATE goals SET ended_at = ?3, ended_history_len = ?4, end = ?5, evaluations = ?6, \
             elapsed_ms = ?7, usage = ?8, cost_usd = ?9, subscription_cost_usd = ?10 \
             WHERE session_id = ?1 AND goal = ?2",
            params![
                session_id.as_bytes().as_slice(),
                goal.goal,
                optional_i64(goal.ended_at_ms, ENDED_AT_FIELD)?,
                optional_i64(goal.ended_history_len, ENDED_HISTORY_LEN_FIELD)?,
                end,
                goal.evaluations,
                to_i64(goal.elapsed_ms, ELAPSED_FIELD)?,
                usage,
                goal.cost_usd,
                goal.subscription_cost_usd,
            ],
        )?;
        Ok(updated > 0)
    }

    /// Appends an evaluation, dropping the session's oldest past
    /// [`MAX_GOAL_EVALUATIONS_PER_SESSION`] in the same transaction.
    pub fn insert_goal_evaluation(
        &self,
        session_id: CaudraId,
        evaluation: &StoredGoalEvaluation,
    ) -> Result<(), SessionError> {
        let outcome = serde_json::to_string(&evaluation.outcome).map_err(StorageError::from)?;
        let usage = to_json(evaluation.usage.as_ref())?;
        let session_id = session_id.as_bytes().as_slice();
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        transaction.execute(
            &format!(
                "INSERT INTO goal_evaluations (session_id, {EVALUATION_COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
            ),
            params![
                session_id,
                to_i64(evaluation.seq, SEQ_FIELD)?,
                evaluation.goal,
                evaluation.evaluation,
                to_i64(evaluation.started_at_ms, STARTED_AT_FIELD)?,
                to_i64(evaluation.at_ms, AT_FIELD)?,
                to_i64(evaluation.history_len, HISTORY_LEN_FIELD)?,
                evaluation.verdict,
                evaluation.reason,
                outcome,
                evaluation.model,
                usage,
                evaluation.cost,
            ],
        )?;
        transaction.execute(
            "DELETE FROM goal_evaluations WHERE session_id = ?1 AND seq <= (\
                 SELECT seq FROM goal_evaluations WHERE session_id = ?1 \
                 ORDER BY seq DESC LIMIT 1 OFFSET ?2)",
            params![
                session_id,
                to_i64(MAX_GOAL_EVALUATIONS_PER_SESSION, EVALUATIONS_LIMIT_FIELD)?
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// A session's goals, oldest first.
    pub fn load_goals(&self, session_id: CaudraId) -> Result<Vec<StoredGoalRecord>, SessionError> {
        let mut statement = self.connection().prepare(&format!(
            "SELECT {GOAL_COLUMNS} FROM goals WHERE session_id = ?1 ORDER BY goal"
        ))?;
        statement
            .query_and_then(params![session_id.as_bytes().as_slice()], read_goal)?
            .collect()
    }

    /// A session's evaluations, oldest first.
    pub fn load_goal_evaluations(
        &self,
        session_id: CaudraId,
    ) -> Result<Vec<StoredGoalEvaluation>, SessionError> {
        let mut statement = self.connection().prepare(&format!(
            "SELECT {EVALUATION_COLUMNS} FROM goal_evaluations WHERE session_id = ?1 ORDER BY seq"
        ))?;
        statement
            .query_and_then(params![session_id.as_bytes().as_slice()], read_evaluation)?
            .collect()
    }

    /// Up to `limit` of a session's evaluations, newest first: only those of
    /// `goal`, and only those before `before_seq`, when given.
    pub fn page_goal_evaluations(
        &self,
        session_id: CaudraId,
        goal: Option<u32>,
        before_seq: Option<u64>,
        limit: usize,
    ) -> Result<Vec<StoredGoalEvaluation>, SessionError> {
        let before_seq = before_seq.map(|seq| to_i64(seq, SEQ_FIELD)).transpose()?;
        let mut statement = self.connection().prepare(&format!(
            "SELECT {EVALUATION_COLUMNS} FROM goal_evaluations WHERE session_id = ?1 \
             AND (?2 IS NULL OR goal = ?2) AND (?3 IS NULL OR seq < ?3) \
             ORDER BY seq DESC LIMIT ?4"
        ))?;
        statement
            .query_and_then(
                params![
                    session_id.as_bytes().as_slice(),
                    goal,
                    before_seq,
                    to_i64(limit, EVALUATIONS_LIMIT_FIELD)?
                ],
                read_evaluation,
            )?
            .collect()
    }
}

fn read_goal(row: &Row<'_>) -> Result<StoredGoalRecord, SessionError> {
    Ok(StoredGoalRecord {
        goal: row.get(0)?,
        condition: row.get(1)?,
        set_at_ms: from_i64(row.get(2)?, SET_AT_FIELD)?,
        set_history_len: from_i64_usize(row.get(3)?, SET_HISTORY_LEN_FIELD)?,
        ended_at_ms: row
            .get::<_, Option<i64>>(4)?
            .map(|at| from_i64(at, ENDED_AT_FIELD))
            .transpose()?,
        ended_history_len: row
            .get::<_, Option<i64>>(5)?
            .map(|len| from_i64_usize(len, ENDED_HISTORY_LEN_FIELD))
            .transpose()?,
        end: from_json(row, 6, END_FIELD)?,
        evaluations: row.get(7)?,
        elapsed_ms: from_i64(row.get(8)?, ELAPSED_FIELD)?,
        usage: deserialize_json(&row.get::<_, String>(9)?, GOAL_USAGE_FIELD)?,
        cost_usd: row.get(10)?,
        subscription_cost_usd: row.get(11)?,
    })
}

fn read_evaluation(row: &Row<'_>) -> Result<StoredGoalEvaluation, SessionError> {
    Ok(StoredGoalEvaluation {
        seq: from_i64(row.get(0)?, SEQ_FIELD)?,
        goal: row.get(1)?,
        evaluation: row.get(2)?,
        started_at_ms: from_i64(row.get(3)?, STARTED_AT_FIELD)?,
        at_ms: from_i64(row.get(4)?, AT_FIELD)?,
        history_len: from_i64_usize(row.get(5)?, HISTORY_LEN_FIELD)?,
        verdict: row.get(6)?,
        reason: row.get(7)?,
        outcome: deserialize_json(&row.get::<_, String>(8)?, OUTCOME_FIELD)?,
        model: row.get(9)?,
        usage: from_json(row, 10, EVALUATION_USAGE_FIELD)?,
        cost: row.get(11)?,
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
    const EVALUATOR_MODEL: &str = "test/evaluator";
    const CONDITION: &str = "the parser passes its tests";
    const VERDICT: &str = "not_met";
    const REASON: &str = "two tests still fail";
    const FAILURE: &str = "evaluator timed out";
    const SET_AT_MS: u64 = 1_767_225_000_000;
    const STARTED_AT_MS: u64 = 1_767_225_500_000;
    const AT_MS: u64 = 1_767_225_600_000;
    const ENDED_AT_MS: u64 = 1_767_226_000_000;
    const SET_HISTORY_LEN: usize = 4;
    const HISTORY_LEN: usize = 12;
    const ENDED_HISTORY_LEN: usize = 30;
    const GOAL: u32 = 3;
    const OTHER_GOAL: u32 = 4;
    const EVALUATION: u32 = 2;
    const EVALUATIONS: u32 = 5;
    const ELAPSED_MS: u64 = 90_000;
    const COST: f64 = 0.0125;
    const COST_USD: f64 = 1.75;
    const SUBSCRIPTION_COST_USD: f64 = 0.5;
    const PASS_PROBABILITY: f64 = 0.25;
    const INPUT_TOKENS: u32 = 900;
    const OUTPUT_TOKENS: u32 = 40;
    const PAGED_EVALUATIONS: u64 = 6;
    const EXTRA_EVALUATIONS: usize = 3;
    const OTHER_SESSION_ROWS: usize = 2;
    const EXTRA_GOALS: usize = 3;
    const LAST_SEQ: u64 = 41;
    const META_BEFORE_GOAL_LOG: &str = r#"{"context_size":1200,"turns":3}"#;
    const ROUND_TRIPS: &str = "a goal and its evaluation must read back as written";
    const PAGES: &str = "a page must hold the matching evaluations, newest first";
    const ENDS: &str = "ending a goal must record its end and spend";
    const GOAL_TRIM_KEEPS_NEWEST: &str = "a new goal must keep only the session's newest goals";
    const EVALUATION_TRIM_KEEPS_NEWEST: &str =
        "an insert must keep only the session's newest evaluations";
    const TRIM_PER_SESSION: &str = "trimming one session must leave the others alone";
    const REWRITE_KEEPS_ROWS: &str = "saving a session must keep its goal log";
    const CASCADE: &str = "the goal log must go with its session";
    const CURSOR_ROUND_TRIPS: &str = "the goal cursor must survive a save and a load";
    const OLD_META_NO_CURSOR: &str = "a meta saved before the goal log must load without a cursor";

    type TestSession = Session<Value, Value, Value>;

    fn open() -> (TempDir, SessionDatabase, CaudraId) {
        let temp = TempDir::new().unwrap();
        let mut database =
            SessionDatabase::open(&StateDir::from_path(temp.path().to_path_buf())).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        (temp, database, session.id)
    }

    fn usage() -> StoredTokenUsage {
        StoredTokenUsage {
            input: INPUT_TOKENS,
            output: OUTPUT_TOKENS,
            cost: Some(COST),
            ..StoredTokenUsage::default()
        }
    }

    fn begun(goal: u32) -> StoredGoalRecord {
        StoredGoalRecord {
            goal,
            condition: CONDITION.into(),
            set_at_ms: SET_AT_MS,
            set_history_len: SET_HISTORY_LEN,
            ended_at_ms: None,
            ended_history_len: None,
            end: None,
            evaluations: 0,
            elapsed_ms: 0,
            usage: StoredTokenUsage::default(),
            cost_usd: 0.0,
            subscription_cost_usd: 0.0,
        }
    }

    fn ended(goal: u32, end: GoalEnd) -> StoredGoalRecord {
        StoredGoalRecord {
            ended_at_ms: Some(ENDED_AT_MS),
            ended_history_len: Some(ENDED_HISTORY_LEN),
            end: Some(end),
            evaluations: EVALUATIONS,
            elapsed_ms: ELAPSED_MS,
            usage: usage(),
            cost_usd: COST_USD,
            subscription_cost_usd: SUBSCRIPTION_COST_USD,
            ..begun(goal)
        }
    }

    fn evaluation(seq: u64, goal: u32) -> StoredGoalEvaluation {
        StoredGoalEvaluation {
            seq,
            goal,
            evaluation: EVALUATION,
            started_at_ms: STARTED_AT_MS,
            at_ms: AT_MS,
            history_len: HISTORY_LEN,
            verdict: None,
            reason: String::new(),
            outcome: GoalEvaluationOutcome::Applied,
            model: None,
            usage: None,
            cost: None,
        }
    }

    fn judged(seq: u64, outcome: GoalEvaluationOutcome) -> StoredGoalEvaluation {
        StoredGoalEvaluation {
            verdict: Some(VERDICT.into()),
            reason: REASON.into(),
            outcome,
            model: Some(EVALUATOR_MODEL.into()),
            usage: Some(usage()),
            cost: Some(COST),
            ..evaluation(seq, GOAL)
        }
    }

    /// Fills a session with evaluations 1 to `count` in one statement, so a
    /// trim test does not pay a commit per row.
    fn seed_evaluations(database: &SessionDatabase, session_id: CaudraId, count: usize) {
        database
            .connection()
            .execute(
                "WITH RECURSIVE seqs(seq) AS (SELECT 1 UNION ALL SELECT seq + 1 FROM seqs \
                     WHERE seq < ?2) \
                 INSERT INTO goal_evaluations \
                     (session_id, seq, goal, evaluation, started_at, at, history_len, reason, \
                      outcome) \
                 SELECT ?1, seq, ?3, seq, ?4, ?4, ?5, '', '{\"status\":\"applied\"}' FROM seqs",
                params![
                    session_id.as_bytes().as_slice(),
                    to_i64(count, "count").unwrap(),
                    GOAL,
                    to_i64(AT_MS, "at").unwrap(),
                    to_i64(HISTORY_LEN, "history_len").unwrap(),
                ],
            )
            .unwrap();
    }

    /// Fills a session with goals 1 to `count` in one statement.
    fn seed_goals(database: &SessionDatabase, session_id: CaudraId, count: usize) {
        database
            .connection()
            .execute(
                "WITH RECURSIVE numbers(goal) AS (SELECT 1 UNION ALL \
                     SELECT goal + 1 FROM numbers WHERE goal < ?2) \
                 INSERT INTO goals \
                     (session_id, goal, condition, set_at, set_history_len, evaluations, \
                      elapsed_ms, usage, cost_usd, subscription_cost_usd) \
                 SELECT ?1, goal, ?3, ?4, 0, 0, 0, '{}', 0.0, 0.0 FROM numbers",
                params![
                    session_id.as_bytes().as_slice(),
                    to_i64(count, "count").unwrap(),
                    CONDITION,
                    to_i64(SET_AT_MS, "set_at").unwrap(),
                ],
            )
            .unwrap();
    }

    fn seqs(database: &SessionDatabase, session_id: CaudraId) -> Vec<u64> {
        database
            .load_goal_evaluations(session_id)
            .unwrap()
            .iter()
            .map(|evaluation| evaluation.seq)
            .collect()
    }

    fn goal_numbers(database: &SessionDatabase, session_id: CaudraId) -> Vec<u32> {
        database
            .load_goals(session_id)
            .unwrap()
            .iter()
            .map(|goal| goal.goal)
            .collect()
    }

    #[test_case(GoalEvaluationOutcome::Applied ; "applied")]
    #[test_case(GoalEvaluationOutcome::Reconsidered ; "reconsidered")]
    #[test_case(GoalEvaluationOutcome::Deferred ; "deferred")]
    #[test_case(GoalEvaluationOutcome::Stale ; "stale")]
    #[test_case(GoalEvaluationOutcome::Skipped { pass_probability: PASS_PROBABILITY } ; "skipped")]
    #[test_case(GoalEvaluationOutcome::Failed { message: FAILURE.into(), applied: true } ; "failed")]
    fn a_goal_and_its_evaluation_round_trip(outcome: GoalEvaluationOutcome) {
        let (_temp, database, session_id) = open();
        let written = judged(1, outcome);

        database.begin_goal(session_id, &begun(GOAL)).unwrap();
        database
            .insert_goal_evaluation(session_id, &written)
            .unwrap();

        assert_eq!(
            (
                database.load_goals(session_id).unwrap(),
                database.load_goal_evaluations(session_id).unwrap()
            ),
            (vec![begun(GOAL)], vec![written]),
            "{ROUND_TRIPS}"
        );
    }

    #[test_case(GoalEvaluationOutcome::Applied, r#"{"status":"applied"}"# ; "applied")]
    #[test_case(GoalEvaluationOutcome::Reconsidered, r#"{"status":"reconsidered"}"# ; "reconsidered")]
    #[test_case(GoalEvaluationOutcome::Deferred, r#"{"status":"deferred"}"# ; "deferred")]
    #[test_case(GoalEvaluationOutcome::Stale, r#"{"status":"stale"}"# ; "stale")]
    #[test_case(GoalEvaluationOutcome::Skipped { pass_probability: PASS_PROBABILITY }, r#"{"status":"skipped","pass_probability":0.25}"# ; "skipped")]
    #[test_case(GoalEvaluationOutcome::Failed { message: FAILURE.into(), applied: false }, r#"{"status":"failed","message":"evaluator timed out","applied":false}"# ; "failed")]
    fn an_outcome_keeps_its_stored_shape(outcome: GoalEvaluationOutcome, stored: &str) {
        assert_eq!(serde_json::to_string(&outcome).unwrap(), stored);
        assert_eq!(
            serde_json::from_str::<GoalEvaluationOutcome>(stored).unwrap(),
            outcome
        );
    }

    #[test_case(GoalEnd::Met, r#"{"end":"met"}"# ; "met")]
    #[test_case(GoalEnd::Impossible, r#"{"end":"impossible"}"# ; "impossible")]
    #[test_case(GoalEnd::Cleared, r#"{"end":"cleared"}"# ; "cleared")]
    #[test_case(GoalEnd::Replaced, r#"{"end":"replaced"}"# ; "replaced")]
    #[test_case(GoalEnd::Error { message: FAILURE.into() }, r#"{"end":"error","message":"evaluator timed out"}"# ; "error")]
    fn an_end_keeps_its_stored_shape(end: GoalEnd, stored: &str) {
        assert_eq!(serde_json::to_string(&end).unwrap(), stored);
        assert_eq!(serde_json::from_str::<GoalEnd>(stored).unwrap(), end);
    }

    #[test_case(true ; "begun")]
    #[test_case(false ; "never_begun")]
    fn ending_a_goal_records_its_end_and_spend(was_begun: bool) {
        let (_temp, database, session_id) = open();
        database.begin_goal(session_id, &begun(OTHER_GOAL)).unwrap();
        if was_begun {
            database.begin_goal(session_id, &begun(GOAL)).unwrap();
        }

        let updated = database
            .end_goal(session_id, &ended(GOAL, GoalEnd::Met))
            .unwrap();

        let expected = if was_begun {
            vec![ended(GOAL, GoalEnd::Met), begun(OTHER_GOAL)]
        } else {
            vec![begun(OTHER_GOAL)]
        };
        assert_eq!(
            (updated, database.load_goals(session_id).unwrap()),
            (was_begun, expected),
            "{ENDS}"
        );
    }

    #[test_case(None, None, 10, &[6, 5, 4, 3, 2, 1] ; "everything")]
    #[test_case(None, None, 2, &[6, 5] ; "limited")]
    #[test_case(Some(GOAL), None, 10, &[3, 2, 1] ; "one_goal")]
    #[test_case(None, Some(5), 2, &[4, 3] ; "before_a_seq")]
    #[test_case(Some(OTHER_GOAL), Some(6), 10, &[5, 4] ; "goal_before_a_seq")]
    fn a_page_holds_the_matching_evaluations_newest_first(
        goal: Option<u32>,
        before_seq: Option<u64>,
        limit: usize,
        expected: &[u64],
    ) {
        let (_temp, database, session_id) = open();
        for seq in 1..=PAGED_EVALUATIONS {
            let number = if seq <= PAGED_EVALUATIONS / 2 {
                GOAL
            } else {
                OTHER_GOAL
            };
            database
                .insert_goal_evaluation(session_id, &evaluation(seq, number))
                .unwrap();
        }

        let page: Vec<u64> = database
            .page_goal_evaluations(session_id, goal, before_seq, limit)
            .unwrap()
            .iter()
            .map(|evaluation| evaluation.seq)
            .collect();

        assert_eq!(page, expected, "{PAGES}");
    }

    #[test]
    fn an_insert_keeps_only_the_newest_evaluations() {
        let (_temp, mut database, session_id) = open();
        let other = TestSession::new(MODEL, CWD);
        database.save(&other, None).unwrap();
        seed_evaluations(&database, other.id, OTHER_SESSION_ROWS);
        let newest = MAX_GOAL_EVALUATIONS_PER_SESSION + EXTRA_EVALUATIONS;
        seed_evaluations(&database, session_id, newest - 1);

        database
            .insert_goal_evaluation(session_id, &evaluation(newest as u64, GOAL))
            .unwrap();

        let kept = seqs(&database, session_id);
        assert_eq!(
            kept.len(),
            MAX_GOAL_EVALUATIONS_PER_SESSION,
            "{EVALUATION_TRIM_KEEPS_NEWEST}"
        );
        assert_eq!(
            (kept.first().copied(), kept.last().copied()),
            (Some(EXTRA_EVALUATIONS as u64 + 1), Some(newest as u64)),
            "{EVALUATION_TRIM_KEEPS_NEWEST}"
        );
        assert_eq!(
            seqs(&database, other.id).len(),
            OTHER_SESSION_ROWS,
            "{TRIM_PER_SESSION}"
        );
    }

    #[test]
    fn a_new_goal_keeps_only_the_newest_goals() {
        let (_temp, mut database, session_id) = open();
        let other = TestSession::new(MODEL, CWD);
        database.save(&other, None).unwrap();
        seed_goals(&database, other.id, OTHER_SESSION_ROWS);
        let newest = MAX_GOALS_PER_SESSION + EXTRA_GOALS;
        seed_goals(&database, session_id, newest - 1);

        database
            .begin_goal(session_id, &begun(newest as u32))
            .unwrap();

        let kept = goal_numbers(&database, session_id);
        assert_eq!(
            kept.len(),
            MAX_GOALS_PER_SESSION,
            "{GOAL_TRIM_KEEPS_NEWEST}"
        );
        assert_eq!(
            (kept.first().copied(), kept.last().copied()),
            (Some(EXTRA_GOALS as u32 + 1), Some(newest as u32)),
            "{GOAL_TRIM_KEEPS_NEWEST}"
        );
        assert_eq!(
            goal_numbers(&database, other.id).len(),
            OTHER_SESSION_ROWS,
            "{TRIM_PER_SESSION}"
        );
    }

    #[test]
    fn a_rewritten_session_keeps_its_goal_log() {
        let (_temp, mut database, session_id) = open();
        let written = judged(1, GoalEvaluationOutcome::Applied);
        database.begin_goal(session_id, &begun(GOAL)).unwrap();
        database
            .insert_goal_evaluation(session_id, &written)
            .unwrap();
        let mut session = database.load::<Value, Value, Value>(session_id).unwrap();
        session.push_message(json!({"role": "user", "content": []}));

        database.save(&session, None).unwrap();

        assert_eq!(
            (
                database.load_goals(session_id).unwrap(),
                database.load_goal_evaluations(session_id).unwrap()
            ),
            (vec![begun(GOAL)], vec![written]),
            "{REWRITE_KEEPS_ROWS}"
        );
    }

    #[test]
    fn deleting_a_session_removes_its_goal_log() {
        let (_temp, mut database, session_id) = open();
        database.begin_goal(session_id, &begun(GOAL)).unwrap();
        database
            .insert_goal_evaluation(session_id, &judged(1, GoalEvaluationOutcome::Applied))
            .unwrap();

        database.delete(session_id, None).unwrap();

        assert!(seqs(&database, session_id).is_empty(), "{CASCADE}");
        assert!(goal_numbers(&database, session_id).is_empty(), "{CASCADE}");
    }

    #[test]
    fn the_cursor_round_trips_through_session_meta() {
        let (_temp, mut database, _) = open();
        let mut session = TestSession::new(MODEL, CWD);
        session.meta.goal_log = Some(StoredGoalCursor {
            last_goal: GOAL,
            last_seq: LAST_SEQ,
        });
        database.save(&session, None).unwrap();

        let loaded = database.load::<Value, Value, Value>(session.id).unwrap();

        assert_eq!(
            loaded.meta.goal_log, session.meta.goal_log,
            "{CURSOR_ROUND_TRIPS}"
        );
    }

    #[test]
    fn a_meta_saved_before_the_goal_log_has_no_cursor() {
        let meta: SessionMeta = serde_json::from_str(META_BEFORE_GOAL_LOG).unwrap();

        assert!(meta.goal_log.is_none(), "{OLD_META_NO_CURSOR}");
    }

    #[test_case(GoalLogWrite::Begun(begun(GOAL)), GoalLogWrite::Begun(StoredGoalRecord { set_history_len: HISTORY_LEN, ..begun(GOAL) }) ; "begun_takes_its_start")]
    #[test_case(GoalLogWrite::Ended(begun(GOAL)), GoalLogWrite::Ended(StoredGoalRecord { ended_history_len: Some(HISTORY_LEN), ..begun(GOAL) }) ; "ended_takes_its_end")]
    #[test_case(GoalLogWrite::Evaluated(Box::new(evaluation(1, GOAL))), GoalLogWrite::Evaluated(Box::new(evaluation(1, GOAL))) ; "evaluated_keeps_its_own")]
    fn stamping_places_a_write_in_history(mut write: GoalLogWrite, expected: GoalLogWrite) {
        write.stamp_history_len(HISTORY_LEN);

        assert_eq!(write, expected);
    }

    #[test_case(0, HISTORY_LEN ; "an_end_waits_for_the_check_drained_after_it")]
    #[test_case(HISTORY_LEN + 1, HISTORY_LEN + 1 ; "a_later_history_wins")]
    fn a_batch_places_an_end_no_earlier_than_its_check(history_len: usize, expected: usize) {
        let mut writes = [
            GoalLogWrite::Ended(begun(GOAL)),
            GoalLogWrite::Evaluated(Box::new(evaluation(1, GOAL))),
        ];

        GoalLogWrite::stamp_batch(&mut writes, history_len);

        let GoalLogWrite::Ended(record) = &writes[0] else {
            panic!("the end stays first");
        };
        assert_eq!(record.ended_history_len, Some(expected));
    }
}
