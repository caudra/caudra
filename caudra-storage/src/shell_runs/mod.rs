//! What the shell ETA model learns from: the newest finished shell runs, each
//! kept as its call's [`Reading`], how long it took and how it ended, beside
//! what the model predicted for it. The command itself is never stored.

mod backfill;
mod lex;
pub mod reading;

use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use crate::sessions::{RuntimeRetry, SessionDatabase, SessionError, from_i64, to_i64};
use crate::{StateClass, StateDir, now_epoch};
use reading::{Reading, decode, encode};

/// Runs kept, newest first; older ones are pruned on every append.
pub const MAX_SHELL_RUNS: usize = 20_000;
const APPEND_OPERATION: &str = "shell run append";
const ELAPSED_FIELD: &str = "shell_runs.elapsed_ms";
const OUTCOME_FIELD: &str = "shell_runs.outcome";
const RECORDED_FIELD: &str = "shell_runs.recorded_at";
const PREDICTED_FIELD: &str = "shell_runs.predicted_ms";

pub(crate) const TABLE: &str = r#"
CREATE TABLE shell_runs (
    id              INTEGER PRIMARY KEY,
    recorded_at     INTEGER NOT NULL,
    reading         BLOB NOT NULL,
    elapsed_ms      INTEGER NOT NULL CHECK(elapsed_ms >= 0),
    outcome         INTEGER NOT NULL CHECK(outcome IN (0, 1, 2)),
    predicted_class INTEGER CHECK(predicted_class IN (0, 1, 2)),
    confidence      REAL,
    predicted_ms    INTEGER
) STRICT;
"#;

/// The table as schema 36 added it, keeping feature hashes that schema 38
/// replaced with readings.
pub(crate) const TABLE_V1: &str = r#"
CREATE TABLE shell_runs (
    id              INTEGER PRIMARY KEY,
    recorded_at     INTEGER NOT NULL,
    features        BLOB NOT NULL,
    elapsed_ms      INTEGER NOT NULL CHECK(elapsed_ms >= 0),
    outcome         INTEGER NOT NULL CHECK(outcome IN (0, 1, 2)),
    predicted_class INTEGER CHECK(predicted_class IN (0, 1, 2)),
    confidence      REAL,
    predicted_ms    INTEGER
) STRICT;
"#;

/// Feature hashes cannot be turned back into a reading, so schema 38 starts
/// the log over from session history.
pub(crate) const DROP_V1: &str = "DROP TABLE shell_runs;";

/// What the step to schema 36 retires: per-command duration sketches and the
/// decision rows of the engine feature the model replaced.
const RETIRED: &str = "DROP TABLE shell_durations; \
     DELETE FROM decisions WHERE feature = 'shell_duration';";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    TimedOut,
    Stopped,
}

impl RunOutcome {
    const fn code(self) -> i64 {
        match self {
            Self::Completed => 0,
            Self::TimedOut => 1,
            Self::Stopped => 2,
        }
    }

    fn from_code(code: i64) -> Result<Self, SessionError> {
        match code {
            0 => Ok(Self::Completed),
            1 => Ok(Self::TimedOut),
            2 => Ok(Self::Stopped),
            other => Err(SessionError::CorruptDatabaseValue {
                field: OUTCOME_FIELD,
                reason: other.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShellRun {
    pub reading: Reading,
    pub elapsed_ms: u64,
    pub outcome: RunOutcome,
}

/// The model's answer for a run, kept so its accuracy can be read back.
#[derive(Debug, Clone, PartialEq)]
pub struct Prediction {
    pub class: u8,
    pub confidence: f64,
    pub eta_ms: Option<u64>,
}

pub struct ShellRuns {
    database: SessionDatabase,
}

impl ShellRuns {
    pub fn open(state: &StateDir) -> Result<Self, SessionError> {
        Ok(Self {
            database: SessionDatabase::open_state(&state.for_class(StateClass::Persistent))?,
        })
    }

    pub fn append(
        &self,
        run: &ShellRun,
        prediction: Option<&Prediction>,
    ) -> Result<(), SessionError> {
        let retry = RuntimeRetry::new(None, &|| false);
        let transaction = retry.run(APPEND_OPERATION, || {
            Ok(Transaction::new_unchecked(
                self.database.connection(),
                TransactionBehavior::Immediate,
            )?)
        })?;
        insert(&transaction, now_epoch(), run, prediction)?;
        transaction.execute(
            "DELETE FROM shell_runs WHERE id <= \
             (SELECT id FROM shell_runs ORDER BY id DESC LIMIT 1 OFFSET ?1)",
            [to_i64(MAX_SHELL_RUNS, RECORDED_FIELD)?],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// The newest `limit` runs, oldest first, the order they are learned in.
    /// A reading this build cannot decode is skipped, so a newer build's rows
    /// never stop an older one from learning.
    pub fn newest(&self, limit: usize) -> Result<Vec<ShellRun>, SessionError> {
        let mut statement = self.database.connection().prepare(
            "SELECT reading, elapsed_ms, outcome FROM shell_runs ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([to_i64(limit, RECORDED_FIELD)?], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut runs = Vec::new();
        for row in rows {
            let (reading, elapsed_ms, outcome) = row?;
            let Ok(reading) = decode(&reading) else {
                continue;
            };
            runs.push(ShellRun {
                reading,
                elapsed_ms: from_i64(elapsed_ms, ELAPSED_FIELD)?,
                outcome: RunOutcome::from_code(outcome)?,
            });
        }
        runs.reverse();
        Ok(runs)
    }
}

/// The step to schema 36, after it adds [`TABLE_V1`]: retires what the model
/// replaced.
pub(crate) fn retire(transaction: &Transaction) -> Result<(), SessionError> {
    transaction.execute_batch(RETIRED)?;
    Ok(())
}

/// The step to schema 38, after [`DROP_V1`]: adds [`TABLE`] and fills it from
/// the shell runs session history still holds.
pub(crate) fn migrate(transaction: &Transaction) -> Result<(), SessionError> {
    transaction.execute_batch(TABLE)?;
    backfill::backfill(transaction)?;
    Ok(())
}

fn insert(
    connection: &Connection,
    recorded_at: u64,
    run: &ShellRun,
    prediction: Option<&Prediction>,
) -> Result<(), SessionError> {
    connection.execute(
        "INSERT INTO shell_runs \
         (recorded_at, reading, elapsed_ms, outcome, predicted_class, confidence, predicted_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            to_i64(recorded_at, RECORDED_FIELD)?,
            encode(&run.reading),
            to_i64(run.elapsed_ms, ELAPSED_FIELD)?,
            run.outcome.code(),
            prediction.map(|prediction| prediction.class),
            prediction.map(|prediction| prediction.confidence),
            prediction
                .and_then(|prediction| prediction.eta_ms)
                .map(|eta| to_i64(eta, PREDICTED_FIELD))
                .transpose()?,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::reading::{Call, read};
    use super::{MAX_SHELL_RUNS, Prediction, RunOutcome, ShellRun, ShellRuns};
    use crate::StateDir;

    const NEWEST: usize = 3;
    const COMMAND: &str = "cargo test";
    const UNREADABLE: &[u8] = &[u8::MAX];

    fn store() -> (TempDir, ShellRuns) {
        let temp = TempDir::new().unwrap();
        let runs = ShellRuns::open(&StateDir::from_path(temp.path().to_path_buf())).unwrap();
        (temp, runs)
    }

    fn run(elapsed_ms: u64) -> ShellRun {
        ShellRun {
            reading: read(&Call {
                command: COMMAND,
                workdir: None,
                cwd: None,
                timeout_secs: None,
            }),
            elapsed_ms,
            outcome: RunOutcome::Completed,
        }
    }

    fn elapsed(runs: &[ShellRun]) -> Vec<u64> {
        runs.iter().map(|run| run.elapsed_ms).collect()
    }

    #[test]
    fn newest_runs_come_back_oldest_first() {
        let (_temp, runs) = store();
        for elapsed in 0..5 {
            runs.append(&run(elapsed), None).unwrap();
        }
        assert_eq!(elapsed(&runs.newest(NEWEST).unwrap()), [2, 3, 4]);
    }

    #[test]
    fn unreadable_readings_are_skipped() {
        let (_temp, runs) = store();
        for elapsed in 0..3 {
            runs.append(&run(elapsed), None).unwrap();
        }
        runs.database
            .connection()
            .execute(
                "UPDATE shell_runs SET reading = ?1 WHERE elapsed_ms = 1",
                [UNREADABLE],
            )
            .unwrap();
        assert_eq!(elapsed(&runs.newest(NEWEST).unwrap()), [0, 2]);
    }

    #[test]
    fn appending_prunes_to_the_cap() {
        let (_temp, runs) = store();
        let connection = runs.database.connection();
        let filling = connection.unchecked_transaction().unwrap();
        for elapsed in 0..MAX_SHELL_RUNS as u64 {
            super::insert(&filling, 0, &run(elapsed), None).unwrap();
        }
        filling.commit().unwrap();
        let prediction = Prediction {
            class: 1,
            confidence: 0.95,
            eta_ms: Some(4_000),
        };
        let last = MAX_SHELL_RUNS as u64;
        runs.append(&run(last), Some(&prediction)).unwrap();
        let kept = runs.newest(MAX_SHELL_RUNS + 1).unwrap();
        assert_eq!(kept.len(), MAX_SHELL_RUNS);
        assert_eq!(kept.first().map(|run| run.elapsed_ms), Some(1));
        assert_eq!(kept.last(), Some(&run(last)));
        let stored: (u8, f64, i64) = connection
            .query_row(
                "SELECT predicted_class, confidence, predicted_ms FROM shell_runs \
                 ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(stored, (1, 0.95, 4_000));
    }
}
