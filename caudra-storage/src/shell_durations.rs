//! Operational shell history, independent of decision logging. Callers gate writes
//! on shell-duration enablement and supply the workspace identity and command family.
//! Only completed calls enter the latency sketch; censored calls contribute counts.

use crate::{
    StateClass, StateDir, now_epoch,
    sessions::{SessionDatabase, SessionError, to_i64},
    tool_ledger::Latency,
};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

pub const MAX_HISTORY_KEYS: usize = 4096;
pub const MAX_WORKSPACE_BYTES: usize = 4096;
pub const MAX_FAMILY_BYTES: usize = 256;
/// Keep the sketch's next bucket edge representable, even for the largest duration.
pub const MAX_DURATION_MS: u64 = i64::MAX as u64;
const EXACT_MIN_SAMPLES: u64 = 3;
const FAMILY_MIN_SAMPLES: u64 = 5;
const MAX_OUTCOME_COUNT: u64 = u32::MAX as u64;
const P50: f64 = 0.5;
const P90: f64 = 0.9;
const LATENCY_FIELD: &str = "shell_durations.latency";
const WORKSPACE_FIELD: &str = "shell duration workspace";
const FAMILY_FIELD: &str = "shell duration family";
const DURATION_FIELD: &str = "shell duration elapsed milliseconds";
const INVALID_KEY: &str = "must be nonempty and contain no control characters";

pub(crate) const TABLES: &str = r#"
CREATE TABLE shell_durations (
    workspace TEXT NOT NULL,
    family TEXT NOT NULL,
    digest BLOB NOT NULL CHECK(length(digest) = 32),
    ok INTEGER NOT NULL CHECK(ok BETWEEN 0 AND 4294967295),
    timeout INTEGER NOT NULL CHECK(timeout BETWEEN 0 AND 4294967295),
    cancelled INTEGER NOT NULL CHECK(cancelled BETWEEN 0 AND 4294967295),
    latency BLOB NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY(workspace, family, digest)
) STRICT, WITHOUT ROWID;
CREATE INDEX shell_durations_updated ON shell_durations(updated_at);
"#;

/// A SHA-256 digest, not a string that could accidentally persist a raw command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandDigest([u8; 32]);

impl CommandDigest {
    /// Normalization belongs to the shell parser; this hashes the supplied bytes exactly.
    pub fn of_normalized(command: &str) -> Self {
        Self(Sha256::digest(command.as_bytes()).into())
    }

    pub fn from_sha256(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellDurationKey {
    /// Stable workspace identity, including authority for remote workspaces, not just cwd.
    pub workspace: String,
    /// The parser's command-arity prefix, never the full command or arbitrary arguments.
    pub family: String,
    pub digest: CommandDigest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DurationOutcome {
    /// Any normal process exit, including a nonzero exit status.
    Ok,
    Timeout,
    Cancelled,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DurationCounts {
    pub ok: u64,
    pub timeout: u64,
    pub cancelled: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DurationSource {
    Exact,
    Family,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurationEstimate {
    /// Conservative upper edges of the reused logarithmic latency buckets.
    pub p50_ms: u64,
    pub p90_ms: u64,
    /// Completed samples only, equal to `counts.ok`.
    pub samples: u64,
    pub source: DurationSource,
    pub counts: DurationCounts,
}

/// A synchronous handle for use on a storage actor/blocking thread. History persists
/// across sessions, including ephemeral ones, and retains at most `MAX_HISTORY_KEYS`
/// exact keys globally. Family estimates aggregate only those retained keys.
pub struct ShellDurations {
    database: SessionDatabase,
}

impl ShellDurations {
    pub fn open(dir: &StateDir) -> Result<Self, SessionError> {
        Ok(Self {
            database: SessionDatabase::open_state(&dir.for_class(StateClass::Persistent))?,
        })
    }

    pub fn record(
        &self,
        key: &ShellDurationKey,
        outcome: DurationOutcome,
        elapsed_ms: u64,
    ) -> Result<(), SessionError> {
        self.database
            .record_shell_duration(key, outcome, elapsed_ms)
    }

    pub fn estimate(
        &self,
        key: &ShellDurationKey,
    ) -> Result<Option<DurationEstimate>, SessionError> {
        self.database.shell_duration_estimate(key)
    }
}

impl SessionDatabase {
    pub fn record_shell_duration(
        &self,
        key: &ShellDurationKey,
        outcome: DurationOutcome,
        elapsed_ms: u64,
    ) -> Result<(), SessionError> {
        validate_key(key)?;
        to_i64(elapsed_ms, DURATION_FIELD)?;
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        let mut history = exact_on(&transaction, key)?.unwrap_or_default();
        match outcome {
            DurationOutcome::Ok if history.counts.ok < MAX_OUTCOME_COUNT => {
                history.counts.ok += 1;
                history.latency.record(elapsed_ms);
            }
            DurationOutcome::Timeout => {
                history.counts.timeout = (history.counts.timeout + 1).min(MAX_OUTCOME_COUNT);
            }
            DurationOutcome::Cancelled => {
                history.counts.cancelled = (history.counts.cancelled + 1).min(MAX_OUTCOME_COUNT);
            }
            DurationOutcome::Ok => {}
        }
        transaction.execute(
            "INSERT INTO shell_durations (workspace, family, digest, ok, timeout, cancelled, latency, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(workspace, family, digest) DO UPDATE SET
                ok = excluded.ok, timeout = excluded.timeout, cancelled = excluded.cancelled,
                latency = excluded.latency, updated_at = excluded.updated_at",
            params![key.workspace, key.family, key.digest.0.as_slice(), history.counts.ok as i64,
                history.counts.timeout as i64, history.counts.cancelled as i64,
                history.latency.encode(), to_i64(now_epoch(), "shell_durations.updated_at")?],
        )?;
        // Keep the just-recorded key even when the clock moves backwards or timestamps tie.
        transaction.execute(
            "DELETE FROM shell_durations WHERE (workspace, family, digest) IN (
                SELECT workspace, family, digest FROM shell_durations
                ORDER BY (workspace = ?1 AND family = ?2 AND digest = ?3) DESC,
                    updated_at DESC, workspace, family, digest
                LIMIT -1 OFFSET ?4
             )",
            params![
                key.workspace,
                key.family,
                key.digest.0.as_slice(),
                MAX_HISTORY_KEYS as i64
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn shell_duration_estimate(
        &self,
        key: &ShellDurationKey,
    ) -> Result<Option<DurationEstimate>, SessionError> {
        validate_key(key)?;
        let transaction = self.connection().unchecked_transaction()?;
        if let Some(history) = exact_on(&transaction, key)?
            && history.counts.ok >= EXACT_MIN_SAMPLES
        {
            return Ok(history.estimate(DurationSource::Exact));
        }
        let mut statement = transaction.prepare(
            "SELECT ok, timeout, cancelled, latency FROM shell_durations
             WHERE workspace = ?1 AND family = ?2",
        )?;
        let mut rows = statement.query(params![key.workspace, key.family])?;
        let mut family = History::default();
        while let Some(row) = rows.next()? {
            let history = history_from_row(row)?;
            family.counts.ok += history.counts.ok;
            family.counts.timeout += history.counts.timeout;
            family.counts.cancelled += history.counts.cancelled;
            family.latency.merge(&history.latency);
        }
        Ok((family.counts.ok >= FAMILY_MIN_SAMPLES)
            .then(|| family.estimate(DurationSource::Family))
            .flatten())
    }
}

#[derive(Default)]
struct History {
    counts: DurationCounts,
    latency: Latency,
}

impl History {
    fn estimate(self, source: DurationSource) -> Option<DurationEstimate> {
        Some(DurationEstimate {
            p50_ms: self.latency.percentile(P50)?,
            p90_ms: self.latency.percentile(P90)?,
            samples: self.counts.ok,
            source,
            counts: self.counts,
        })
    }
}

fn exact_on(
    connection: &Connection,
    key: &ShellDurationKey,
) -> Result<Option<History>, SessionError> {
    connection
        .query_row(
            "SELECT ok, timeout, cancelled, latency FROM shell_durations
         WHERE workspace = ?1 AND family = ?2 AND digest = ?3",
            params![key.workspace, key.family, key.digest.0.as_slice()],
            |row| Ok(history_from_row(row)),
        )
        .optional()?
        .transpose()
}

fn history_from_row(row: &Row<'_>) -> Result<History, SessionError> {
    Ok(History {
        counts: DurationCounts {
            ok: u64::from(row.get::<_, u32>(0)?),
            timeout: u64::from(row.get::<_, u32>(1)?),
            cancelled: u64::from(row.get::<_, u32>(2)?),
        },
        latency: Latency::decode(&row.get::<_, Vec<u8>>(3)?, LATENCY_FIELD)?,
    })
}

fn validate_key(key: &ShellDurationKey) -> Result<(), SessionError> {
    for (field, value, maximum) in [
        (WORKSPACE_FIELD, key.workspace.as_str(), MAX_WORKSPACE_BYTES),
        (FAMILY_FIELD, key.family.as_str(), MAX_FAMILY_BYTES),
    ] {
        SessionDatabase::validate_len(field, value.len(), maximum)?;
        if value.trim().is_empty() || value.chars().any(char::is_control) {
            return Err(SessionError::CorruptDatabaseValue {
                field,
                reason: INVALID_KEY.into(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CommandDigest, DURATION_FIELD, DurationCounts, DurationOutcome, DurationSource,
        FAMILY_FIELD, INVALID_KEY, MAX_DURATION_MS, MAX_FAMILY_BYTES, MAX_HISTORY_KEYS,
        MAX_OUTCOME_COUNT, MAX_WORKSPACE_BYTES, ShellDurationKey, ShellDurations, WORKSPACE_FIELD,
        exact_on,
    };
    use crate::{StateDir, sessions::SessionError};
    use rusqlite::params;
    use tempfile::TempDir;
    use test_case::test_case;

    const WORKSPACE: &str = "local:/project";
    const OTHER_WORKSPACE: &str = "remote:authority:/project";
    const FAMILY: &str = "cargo test";
    const COMMAND: &str = "cargo test -p secret-package-name";
    const OTHER_COMMAND: &str = "cargo test --workspace";
    const OTHER_FAMILY: &str = "cargo check";
    const COMPLETED_MS: u64 = 100;
    const CENSORED_MS: u64 = 1_000_000;

    fn dir() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let dir = StateDir::from_path(temp.path().to_owned());
        (temp, dir)
    }

    fn key(command: &str) -> ShellDurationKey {
        ShellDurationKey {
            workspace: WORKSPACE.into(),
            family: FAMILY.into(),
            digest: CommandDigest::of_normalized(command),
        }
    }

    fn complete(history: &ShellDurations, key: &ShellDurationKey, samples: usize) {
        for _ in 0..samples {
            history
                .record(key, DurationOutcome::Ok, COMPLETED_MS)
                .unwrap();
        }
    }

    #[test_case(false; "persistent")]
    #[test_case(true; "ephemeral")]
    fn history_survives_reopen_without_command_text(ephemeral: bool) {
        let (_temp, persistent) = dir();
        let (volatile, _) = dir();
        let state = if ephemeral {
            StateDir::split(volatile.path().to_owned(), persistent.path().to_owned())
        } else {
            persistent.clone()
        };
        let history = ShellDurations::open(&state).unwrap();
        let key = key(COMMAND);
        complete(&history, &key, 3);
        history
            .record(&key, DurationOutcome::Timeout, CENSORED_MS)
            .unwrap();
        let before = history.estimate(&key).unwrap().unwrap();
        let stored: (String, String, Vec<u8>) = history
            .database
            .connection()
            .query_row(
                "SELECT workspace, family, digest FROM shell_durations",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            stored,
            (WORKSPACE.into(), FAMILY.into(), key.digest.0.to_vec())
        );
        assert!(!format!("{key:?}").contains(COMMAND));
        drop(history);
        let reopened = ShellDurations::open(&persistent).unwrap();
        assert_eq!(reopened.estimate(&key).unwrap(), Some(before));
        if ephemeral {
            assert_eq!(volatile.path().read_dir().unwrap().count(), 0);
        }
    }

    #[test_case(0, 0, None; "no_samples")]
    #[test_case(2, 2, None; "below_both_thresholds")]
    #[test_case(3, 0, Some(DurationSource::Exact); "exact_threshold")]
    #[test_case(0, 5, Some(DurationSource::Family); "family_without_exact")]
    #[test_case(2, 3, Some(DurationSource::Family); "family_threshold")]
    #[test_case(3, 5, Some(DurationSource::Exact); "exact_wins")]
    fn completed_thresholds_and_censoring(
        exact_samples: usize,
        other_samples: usize,
        expected: Option<DurationSource>,
    ) {
        let (_temp, dir) = dir();
        let history = ShellDurations::open(&dir).unwrap();
        let exact = key(COMMAND);
        let other = key(OTHER_COMMAND);
        complete(&history, &exact, exact_samples);
        complete(&history, &other, other_samples);
        for key in [&exact, &other] {
            history
                .record(key, DurationOutcome::Timeout, CENSORED_MS)
                .unwrap();
            history
                .record(key, DurationOutcome::Cancelled, CENSORED_MS)
                .unwrap();
        }
        let estimate = history.estimate(&exact).unwrap();
        assert_eq!(
            estimate.as_ref().map(|value| value.source.clone()),
            expected
        );
        if let Some(estimate) = estimate {
            let is_exact = estimate.source == DurationSource::Exact;
            let samples = if is_exact {
                exact_samples
            } else {
                exact_samples + other_samples
            } as u64;
            assert_eq!(estimate.samples, samples);
            assert_eq!(
                estimate.counts,
                DurationCounts {
                    ok: samples,
                    timeout: if is_exact { 1 } else { 2 },
                    cancelled: if is_exact { 1 } else { 2 },
                }
            );
            assert!(estimate.p50_ms >= COMPLETED_MS);
            assert!(estimate.p90_ms < CENSORED_MS);
        }
        let stored = exact_on(history.database.connection(), &exact)
            .unwrap()
            .unwrap();
        assert_eq!(stored.counts.ok, exact_samples as u64);
        assert_eq!(stored.latency.is_empty(), exact_samples == 0);
    }

    #[test_case(true; "workspace")]
    #[test_case(false; "family")]
    fn history_is_isolated(workspace: bool) {
        let (_temp, dir) = dir();
        let history = ShellDurations::open(&dir).unwrap();
        let original = key(COMMAND);
        complete(&history, &original, 5);
        let mut different = original.clone();
        if workspace {
            different.workspace = OTHER_WORKSPACE.into();
        } else {
            different.family = OTHER_FAMILY.into();
        }
        assert!(history.estimate(&different).unwrap().is_none());
        history
            .record(&different, DurationOutcome::Timeout, CENSORED_MS)
            .unwrap();
        assert_eq!(
            history.estimate(&original).unwrap().unwrap().counts.timeout,
            0
        );
        let other = key(OTHER_COMMAND);
        assert_eq!(
            history.estimate(&other).unwrap().unwrap().source,
            DurationSource::Family
        );
    }

    #[test_case(0; "zero_duration")]
    #[test_case(MAX_DURATION_MS; "maximum_duration")]
    fn duration_bounds_roundtrip(elapsed_ms: u64) {
        let (_temp, dir) = dir();
        let history = ShellDurations::open(&dir).unwrap();
        let key = key(COMMAND);
        for _ in 0..3 {
            history
                .record(&key, DurationOutcome::Ok, elapsed_ms)
                .unwrap();
        }
        let estimate = history.estimate(&key).unwrap().unwrap();
        assert_eq!(estimate.p50_ms, elapsed_ms);
        assert_eq!(estimate.p90_ms, elapsed_ms);
    }

    #[test_case(MAX_DURATION_MS + 1; "over_limit")]
    #[test_case(u64::MAX; "unsigned_maximum")]
    fn excessive_duration_does_not_write(elapsed_ms: u64) {
        let (_temp, dir) = dir();
        let history = ShellDurations::open(&dir).unwrap();
        let key = key(COMMAND);
        assert!(matches!(
            history.record(&key, DurationOutcome::Ok, elapsed_ms),
            Err(SessionError::CorruptDatabaseValue {
                field: DURATION_FIELD,
                ..
            })
        ));
        assert!(
            exact_on(history.database.connection(), &key)
                .unwrap()
                .is_none()
        );
    }

    #[test_case(true; "workspace_bound")]
    #[test_case(false; "family_bound")]
    fn oversized_keys_do_not_write(workspace: bool) {
        let (_temp, dir) = dir();
        let history = ShellDurations::open(&dir).unwrap();
        let mut key = key(COMMAND);
        let field = if workspace {
            key.workspace = "x".repeat(MAX_WORKSPACE_BYTES + 1);
            WORKSPACE_FIELD
        } else {
            key.family = "x".repeat(MAX_FAMILY_BYTES + 1);
            FAMILY_FIELD
        };
        assert!(
            matches!(history.record(&key, DurationOutcome::Ok, COMPLETED_MS),
            Err(SessionError::LimitExceeded { kind, .. }) if kind == field)
        );
        assert!(history.estimate(&key).is_err());
        let count: i64 = history
            .database
            .connection()
            .query_row("SELECT count(*) FROM shell_durations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test_case(""; "empty")]
    #[test_case("  "; "blank")]
    #[test_case("cargo\ntest"; "control_character")]
    fn invalid_family_rejected(family: &str) {
        let (_temp, dir) = dir();
        let history = ShellDurations::open(&dir).unwrap();
        let mut key = key(COMMAND);
        key.family = family.into();
        assert!(
            matches!(history.record(&key, DurationOutcome::Ok, COMPLETED_MS),
            Err(SessionError::CorruptDatabaseValue { field: FAMILY_FIELD, reason }) if reason == INVALID_KEY)
        );
    }

    #[test_case(0; "new_key")]
    #[test_case(1; "existing_key")]
    fn retention_is_bounded_and_preserves_current_key(existing: usize) {
        let (_temp, dir) = dir();
        let history = ShellDurations::open(&dir).unwrap();
        let connection = history.database.connection();
        let transaction = connection.unchecked_transaction().unwrap();
        for index in 0..MAX_HISTORY_KEYS + 1 {
            let digest = CommandDigest::of_normalized(&index.to_string());
            transaction
                .execute(
                    "INSERT INTO shell_durations VALUES (?1, ?2, ?3, 0, 0, 0, X'', ?4)",
                    params![WORKSPACE, FAMILY, digest.0.as_slice(), i64::MAX],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        let current = key(if existing == 1 { "0" } else { COMMAND });
        history
            .record(&current, DurationOutcome::Ok, COMPLETED_MS)
            .unwrap();
        let count: i64 = connection
            .query_row("SELECT count(*) FROM shell_durations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, MAX_HISTORY_KEYS as i64);
        assert_eq!(
            exact_on(connection, &current).unwrap().unwrap().counts.ok,
            1
        );
    }

    #[test_case(DurationOutcome::Ok; "completed")]
    #[test_case(DurationOutcome::Timeout; "timeout")]
    #[test_case(DurationOutcome::Cancelled; "cancelled")]
    fn outcome_counts_saturate_without_sketch_growth(outcome: DurationOutcome) {
        let (_temp, dir) = dir();
        let history = ShellDurations::open(&dir).unwrap();
        let key = key(COMMAND);
        history
            .record(&key, DurationOutcome::Ok, COMPLETED_MS)
            .unwrap();
        let connection = history.database.connection();
        connection
            .execute(
                "UPDATE shell_durations SET ok = ?1, timeout = ?1, cancelled = ?1",
                [MAX_OUTCOME_COUNT as i64],
            )
            .unwrap();
        let before = exact_on(connection, &key).unwrap().unwrap();
        history.record(&key, outcome, CENSORED_MS).unwrap();
        let after = exact_on(connection, &key).unwrap().unwrap();
        assert_eq!(after.counts, before.counts);
        assert_eq!(after.latency, before.latency);
    }
}
