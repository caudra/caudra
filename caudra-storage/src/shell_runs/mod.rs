//! What the shell ETA model learns from: the newest finished shell runs, each
//! kept as its call's [`Reading`], how long it took and how it ended, beside
//! what the model predicted for it. The command itself is never stored.

mod backfill;
mod host;
mod lex;
pub mod reading;

use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use tracing::warn;

use crate::sessions::{RuntimeRetry, SessionDatabase, SessionError, from_i64, to_i64};
use crate::{StateClass, StateDir, now_epoch};
use reading::{Reading, decode, encode, separate_projects};

pub use host::HostSnapshot;

/// Runs kept, newest first; older ones are pruned on every append.
pub const MAX_SHELL_RUNS: usize = 100_000;
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

pub(crate) const HOST_COLUMN: &str = "ALTER TABLE shell_runs ADD COLUMN host TEXT;";

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
    pub host: Option<HostSnapshot>,
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
        prune(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    /// The newest `limit` runs, oldest first, the order they are learned in.
    /// A reading this build cannot decode is skipped, so a newer build's rows
    /// never stop an older one from learning. An older reading's project
    /// leaves its labels where a newer run names it.
    pub fn newest(&self, limit: usize) -> Result<Vec<ShellRun>, SessionError> {
        let mut statement = self.database.connection().prepare(
            "SELECT reading, elapsed_ms, outcome, host FROM shell_runs ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([to_i64(limit, RECORDED_FIELD)?], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3),
            ))
        })?;
        let mut runs = Vec::new();
        for row in rows {
            let (reading, elapsed_ms, outcome, host) = row?;
            let Ok(reading) = decode(&reading) else {
                continue;
            };
            let host = host
                .map_err(SessionError::from)
                .and_then(|value| value.as_deref().map(host::decode).transpose())
                .unwrap_or_else(|_| {
                    warn!("Ignoring unreadable optional shell-run host snapshot");
                    None
                });
            runs.push(ShellRun {
                reading,
                host,
                elapsed_ms: from_i64(elapsed_ms, ELAPSED_FIELD)?,
                outcome: RunOutcome::from_code(outcome)?,
            });
        }
        runs.reverse();
        separate_projects(runs.iter_mut().map(|run| &mut run.reading));
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

/// Drops all but the newest [`MAX_SHELL_RUNS`].
pub(crate) fn prune(connection: &Connection) -> Result<(), SessionError> {
    connection.execute(
        "DELETE FROM shell_runs WHERE id <= \
         (SELECT id FROM shell_runs ORDER BY id DESC LIMIT 1 OFFSET ?1)",
        [to_i64(MAX_SHELL_RUNS, RECORDED_FIELD)?],
    )?;
    Ok(())
}

pub(crate) fn insert(
    connection: &Connection,
    recorded_at: u64,
    run: &ShellRun,
    prediction: Option<&Prediction>,
) -> Result<(), SessionError> {
    let host = run.host.as_ref().map(host::encode).transpose()?;
    let sql = if host.is_some() {
        "INSERT INTO shell_runs \
         (recorded_at, reading, elapsed_ms, outcome, predicted_class, confidence, predicted_ms, host) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
    } else {
        "INSERT INTO shell_runs \
         (recorded_at, reading, elapsed_ms, outcome, predicted_class, confidence, predicted_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
    };
    let mut statement = connection.prepare_cached(sql)?;
    let parameters = statement.parameter_count();
    statement.execute(
        &params![
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
            host,
        ][..parameters],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::params;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::reading::{Call, decode, legacy, read};
    use super::{HostSnapshot, MAX_SHELL_RUNS, Prediction, RunOutcome, ShellRun, ShellRuns};
    use crate::StateDir;
    use crate::sessions::SessionError;

    const NEWEST: usize = 3;
    const COMMAND: &str = "cargo test";
    const UNREADABLE: &[u8] = &[u8::MAX];
    const CORRUPT_HOST: &str = r#"{"version":1,"snapshot":{"cpu_usage":2.0}}"#;
    const OVERSIZED_HOST_BYTES: usize = 513;
    const OTHER_DIR: &str = "/work/other";

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
            host: None,
            outcome: RunOutcome::Completed,
        }
    }

    fn elapsed(runs: &[ShellRun]) -> Vec<u64> {
        runs.iter().map(|run| run.elapsed_ms).collect()
    }

    #[test_case(None; "missing")]
    #[test_case(Some(HostSnapshot::default()); "all_unknown")]
    #[test_case(Some(HostSnapshot {
        cpu_usage: Some(0.0),
        cpu_usage_mean: Some(0.5),
        cpu_usage_window_ms: Some(10_000),
        normalized_load: Some(2.5),
        cpu_pressure: Some(1.0),
        host_cpus: Some(16),
        available_cpus: Some(2),
        sample_age_ms: Some(u64::MAX),
    }); "all_measured")]
    #[test_case(Some(HostSnapshot {
        available_cpus: Some(2),
        ..HostSnapshot::default()
    }); "partial")]
    fn host_snapshot_round_trips(host: Option<HostSnapshot>) {
        let (_temp, runs) = store();
        let mut expected = run(20);
        expected.host = host;
        runs.append(&expected, None).unwrap();
        assert_eq!(runs.newest(NEWEST).unwrap(), [expected.clone()]);
        let absent: bool = runs
            .database
            .connection()
            .query_row("SELECT host IS NULL FROM shell_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(absent, expected.host.is_none());
    }

    #[test_case(f32::NAN; "nan")]
    #[test_case(f32::INFINITY; "infinity")]
    #[test_case(-0.1; "negative")]
    #[test_case(1.1; "above_one")]
    fn invalid_host_snapshot_is_not_written(cpu_usage: f32) {
        let (_temp, runs) = store();
        let mut invalid = run(20);
        invalid.host = Some(HostSnapshot {
            cpu_usage: Some(cpu_usage),
            ..HostSnapshot::default()
        });
        assert!(matches!(
            runs.append(&invalid, None),
            Err(SessionError::CorruptDatabaseValue { .. })
        ));
        assert!(runs.newest(NEWEST).unwrap().is_empty());
    }

    #[test_case(CORRUPT_HOST; "out_of_range")]
    #[test_case("not json"; "malformed")]
    #[test_case(r#"{"version":2,"snapshot":{}}"#; "unknown_version")]
    #[test_case(&" ".repeat(OVERSIZED_HOST_BYTES); "oversized")]
    fn corrupt_host_snapshot_preserves_measured_run(host: &str) {
        let (_temp, runs) = store();
        let expected = run(20);
        runs.append(&expected, None).unwrap();
        runs.database
            .connection()
            .execute("UPDATE shell_runs SET host = ?1", [host])
            .unwrap();
        assert_eq!(runs.newest(NEWEST).unwrap(), [expected]);
    }

    #[test]
    fn invalid_host_text_preserves_measured_run() {
        let (_temp, runs) = store();
        let expected = run(20);
        runs.append(&expected, None).unwrap();
        runs.database
            .connection()
            .execute("UPDATE shell_runs SET host = CAST(x'ff' AS TEXT)", [])
            .unwrap();
        assert_eq!(runs.newest(NEWEST).unwrap(), [expected]);
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
    fn older_readings_lose_projects_newer_runs_name() {
        let (_temp, runs) = store();
        let named = run(0);
        let unnamed = ShellRun {
            reading: read(&Call {
                command: COMMAND,
                workdir: Some(OTHER_DIR),
                cwd: None,
                timeout_secs: None,
            }),
            ..run(1)
        };
        for stored in [&named, &unnamed, &run(2)] {
            runs.append(stored, None).unwrap();
        }
        let connection = runs.database.connection();
        for stored in [&named, &unnamed] {
            connection
                .execute(
                    "UPDATE shell_runs SET reading = ?1 WHERE elapsed_ms = ?2",
                    params![legacy(&stored.reading), stored.elapsed_ms.cast_signed()],
                )
                .unwrap();
        }
        let older_unnamed = ShellRun {
            reading: decode(&legacy(&unnamed.reading)).unwrap(),
            ..unnamed
        };
        assert_eq!(runs.newest(NEWEST).unwrap(), [named, older_unnamed, run(2)]);
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
        let newest = runs.newest(NEWEST).unwrap();
        assert_eq!(
            elapsed(&newest),
            ((last - NEWEST as u64 + 1)..=last).collect::<Vec<_>>()
        );
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
