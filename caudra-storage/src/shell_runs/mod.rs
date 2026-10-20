//! What the shell ETA model learns from: the newest finished shell runs, each
//! kept as its command's feature hashes, how long it took and how it ended,
//! beside what the model predicted for it. The command itself is never stored.

mod backfill;
pub mod features;

use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use crate::sessions::{RuntimeRetry, SessionDatabase, SessionError, from_i64, to_i64};
use crate::{StateClass, StateDir, now_epoch};

/// Runs kept, newest first; older ones are pruned on every append.
pub const MAX_SHELL_RUNS: usize = 20_000;
const APPEND_OPERATION: &str = "shell run append";
const FEATURES_FIELD: &str = "shell_runs.features";
const ELAPSED_FIELD: &str = "shell_runs.elapsed_ms";
const OUTCOME_FIELD: &str = "shell_runs.outcome";
const RECORDED_FIELD: &str = "shell_runs.recorded_at";
const PREDICTED_FIELD: &str = "shell_runs.predicted_ms";
const FEATURE_BYTES: usize = size_of::<u64>();
const MISALIGNED_FEATURES: &str = "feature bytes are not whole hashes";

pub(crate) const TABLE: &str = r#"
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

/// What the step to this table retires: per-command duration sketches and the
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRun {
    pub features: Vec<u64>,
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
    pub fn newest(&self, limit: usize) -> Result<Vec<ShellRun>, SessionError> {
        let mut statement = self.database.connection().prepare(
            "SELECT features, elapsed_ms, outcome FROM shell_runs ORDER BY id DESC LIMIT ?1",
        )?;
        let mut runs = statement
            .query_map([to_i64(limit, RECORDED_FIELD)?], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .map(|row| {
                let (features, elapsed_ms, outcome) = row?;
                Ok(ShellRun {
                    features: decode(&features)?,
                    elapsed_ms: from_i64(elapsed_ms, ELAPSED_FIELD)?,
                    outcome: RunOutcome::from_code(outcome)?,
                })
            })
            .collect::<Result<Vec<_>, SessionError>>()?;
        runs.reverse();
        Ok(runs)
    }
}

/// The step that adds `shell_runs`: retires what it replaces, then fills it
/// from the shell runs session history still holds.
pub(crate) fn migrate(transaction: &Transaction) -> Result<(), SessionError> {
    transaction.execute_batch(RETIRED)?;
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
         (recorded_at, features, elapsed_ms, outcome, predicted_class, confidence, predicted_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            to_i64(recorded_at, RECORDED_FIELD)?,
            encode(&run.features),
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

fn encode(features: &[u64]) -> Vec<u8> {
    features
        .iter()
        .flat_map(|hash| hash.to_le_bytes())
        .collect()
}

fn decode(bytes: &[u8]) -> Result<Vec<u64>, SessionError> {
    let (hashes, rest) = bytes.as_chunks::<FEATURE_BYTES>();
    if !rest.is_empty() {
        return Err(SessionError::CorruptDatabaseValue {
            field: FEATURES_FIELD,
            reason: MISALIGNED_FEATURES.into(),
        });
    }
    Ok(hashes
        .iter()
        .map(|hash| u64::from_le_bytes(*hash))
        .collect())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{MAX_SHELL_RUNS, Prediction, RunOutcome, ShellRun, ShellRuns, decode, encode};
    use crate::StateDir;

    const NEWEST: usize = 3;

    fn store() -> (TempDir, ShellRuns) {
        let temp = TempDir::new().unwrap();
        let runs = ShellRuns::open(&StateDir::from_path(temp.path().to_path_buf())).unwrap();
        (temp, runs)
    }

    fn run(elapsed_ms: u64) -> ShellRun {
        ShellRun {
            features: vec![elapsed_ms, u64::MAX],
            elapsed_ms,
            outcome: RunOutcome::Completed,
        }
    }

    #[test]
    fn newest_runs_come_back_oldest_first() {
        let (_temp, runs) = store();
        for elapsed in 0..5 {
            runs.append(&run(elapsed), None).unwrap();
        }
        let newest: Vec<u64> = runs
            .newest(NEWEST)
            .unwrap()
            .iter()
            .map(|run| run.elapsed_ms)
            .collect();
        assert_eq!(newest, [2, 3, 4]);
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

    #[test_case(&[]; "empty")]
    #[test_case(&[0, 1, u64::MAX]; "several")]
    fn features_round_trip(features: &[u64]) {
        assert_eq!(decode(&encode(features)).unwrap(), features);
    }

    #[test]
    fn misaligned_features_are_corrupt() {
        assert!(decode(&[0; 9]).is_err());
    }
}
