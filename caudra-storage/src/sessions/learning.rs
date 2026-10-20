//! Continual learning's bookkeeping, per project: how far its history has
//! been learned from, the lease that keeps two processes from learning it at
//! once, every run with what it changed, and the before and after of each
//! change so a run can be reverted. The newest runs also keep their learner's
//! conversation, compressed and outside the history index, so no search finds
//! it.
//!
//! A run is recorded `running` when it starts, so the memory entries it writes
//! can name it. A run whose holder died is closed as `interrupted` by the next
//! one to start, which also takes its lease once it lapses.

use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};

use crate::sessions::{RuntimeRetry, SessionDatabase, SessionError};
use crate::sessions::{compress_payload, decompress_payload};
use crate::{StateClass, StateDir};

const BEGIN_OPERATION: &str = "learning run begin";
const FINISH_OPERATION: &str = "learning run finish";
const RENEW_OPERATION: &str = "learning lease renew";
const PAUSE_OPERATION: &str = "learning pause";
const REVERT_OPERATION: &str = "learning edit revert";
const RUNNING: &str = "running";
const INTERRUPTED: &str = "interrupted";
const RUN_COLUMNS: &str =
    "id, started_ms, finished_ms, from_ms, through_ms, outcome, model, tokens, summary, dropped";
const EDIT_COLUMNS: &str = "run_id, seq, target, path, before, after, evidence, reverted_ms";
const TRANSCRIPT_FIELD: &str = "learning_transcripts.body";
/// Runs per project whose transcript is kept; older ones lose it.
pub const KEPT_TRANSCRIPTS: i64 = 20;

pub(crate) const TABLES: &str = r#"
CREATE TABLE learning_projects (
    project        TEXT PRIMARY KEY,
    through_ms     INTEGER NOT NULL,
    last_run_ms    INTEGER NOT NULL,
    paused         INTEGER NOT NULL DEFAULT 0,
    lease_owner    TEXT,
    lease_until_ms INTEGER
) STRICT, WITHOUT ROWID;

CREATE TABLE learning_runs (
    id          INTEGER PRIMARY KEY,
    project     TEXT NOT NULL,
    started_ms  INTEGER NOT NULL,
    finished_ms INTEGER,
    from_ms     INTEGER NOT NULL,
    through_ms  INTEGER NOT NULL,
    outcome     TEXT NOT NULL,
    model       TEXT NOT NULL DEFAULT '',
    tokens      INTEGER NOT NULL DEFAULT 0,
    summary     TEXT NOT NULL DEFAULT '',
    dropped     TEXT NOT NULL DEFAULT '[]'
) STRICT;
CREATE INDEX learning_runs_project ON learning_runs(project, id);

CREATE TABLE learning_edits (
    run_id      INTEGER NOT NULL REFERENCES learning_runs(id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    target      TEXT NOT NULL,
    path        TEXT NOT NULL,
    before      TEXT,
    after       TEXT,
    evidence    TEXT NOT NULL,
    reverted_ms INTEGER,
    PRIMARY KEY(run_id, seq)
) STRICT, WITHOUT ROWID;
"#;

/// Each run's learner conversation, a zstd-compressed JSON array of its
/// messages. Nothing indexes it for search.
pub(crate) const TRANSCRIPTS: &str = r#"
CREATE TABLE learning_transcripts (
    run_id INTEGER PRIMARY KEY REFERENCES learning_runs(id) ON DELETE CASCADE,
    body   BLOB NOT NULL
) STRICT;
"#;

/// A project's learning state. `None` fields mean no lease is held.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LearningProject {
    /// History before this was learned from.
    pub through_ms: i64,
    /// When the last run finished, 0 before the first.
    pub last_run_ms: i64,
    pub paused: bool,
    pub lease_owner: Option<String>,
    pub lease_until_ms: Option<i64>,
}

/// The run a [`LearningStore::begin`] opened, and the history it starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BegunRun {
    pub id: i64,
    pub from_ms: i64,
}

/// One change a run made. `before` and `after` are `None` where the file,
/// block or note was absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearningEdit {
    pub target: String,
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
    /// JSON array of the history references that support the change.
    pub evidence: String,
}

/// How a run ended. `through_ms` is where the next run starts, `None` to
/// leave the cursor where it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishedRun {
    pub outcome: String,
    pub model: String,
    pub tokens: u64,
    pub summary: String,
    /// JSON array of the items the run proposed and Caudra refused, with why.
    pub dropped: String,
    pub through_ms: Option<i64>,
    pub edits: Vec<LearningEdit>,
    /// The learner's conversation as a JSON array of messages, when it had one.
    pub transcript: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearningRun {
    pub id: i64,
    pub started_ms: i64,
    pub finished_ms: Option<i64>,
    pub from_ms: i64,
    pub through_ms: i64,
    pub outcome: String,
    pub model: String,
    pub tokens: u64,
    pub summary: String,
    pub dropped: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEdit {
    pub run_id: i64,
    pub seq: i64,
    pub edit: LearningEdit,
    pub reverted_ms: Option<i64>,
}

pub struct LearningStore {
    database: SessionDatabase,
}

impl LearningStore {
    pub fn open(state: &StateDir) -> Result<Self, SessionError> {
        Ok(Self {
            database: SessionDatabase::open_state(&state.for_class(StateClass::Persistent))?,
        })
    }

    pub fn project(&self, project: &str) -> Result<Option<LearningProject>, SessionError> {
        project_row(self.connection(), project)
    }

    /// Opens a run for `owner` unless another owner holds an unexpired lease
    /// or the project is paused. A project seen for the first time starts at
    /// `first_from_ms`.
    pub fn begin(
        &self,
        project: &str,
        owner: &str,
        now_ms: i64,
        lease_ms: i64,
        first_from_ms: i64,
    ) -> Result<Option<BegunRun>, SessionError> {
        let transaction = self.begin_write(BEGIN_OPERATION)?;
        let state = match project_row(&transaction, project)? {
            Some(state) => state,
            None => {
                transaction.execute(
                    "INSERT INTO learning_projects (project, through_ms, last_run_ms) \
                     VALUES (?1, ?2, 0)",
                    params![project, first_from_ms],
                )?;
                LearningProject {
                    through_ms: first_from_ms,
                    ..LearningProject::default()
                }
            }
        };
        let held_elsewhere = state
            .lease_owner
            .as_deref()
            .is_some_and(|holder| holder != owner)
            && state.lease_until_ms.is_some_and(|until| until > now_ms);
        if state.paused || held_elsewhere {
            return Ok(None);
        }
        let from_ms = match state.through_ms {
            0 => first_from_ms,
            through => through,
        };
        transaction.execute(
            "UPDATE learning_runs SET outcome = ?2, finished_ms = ?3 \
             WHERE project = ?1 AND outcome = ?4",
            params![project, INTERRUPTED, now_ms, RUNNING],
        )?;
        transaction.execute(
            "UPDATE learning_projects SET lease_owner = ?2, lease_until_ms = ?3 WHERE project = ?1",
            params![project, owner, now_ms.saturating_add(lease_ms)],
        )?;
        transaction.execute(
            "INSERT INTO learning_runs (project, started_ms, from_ms, through_ms, outcome) \
             VALUES (?1, ?2, ?3, ?3, ?4)",
            params![project, now_ms, from_ms, RUNNING],
        )?;
        let id = transaction.last_insert_rowid();
        transaction.commit()?;
        Ok(Some(BegunRun { id, from_ms }))
    }

    /// Extends the lease `owner` holds. False when it no longer holds it.
    pub fn renew(&self, project: &str, owner: &str, until_ms: i64) -> Result<bool, SessionError> {
        let transaction = self.begin_write(RENEW_OPERATION)?;
        let renewed = transaction.execute(
            "UPDATE learning_projects SET lease_until_ms = ?3 \
             WHERE project = ?1 AND lease_owner = ?2",
            params![project, owner, until_ms],
        )?;
        transaction.commit()?;
        Ok(renewed > 0)
    }

    /// Closes the run, records its edits, moves the cursor when the run says
    /// so, and gives up the lease.
    pub fn finish(
        &self,
        project: &str,
        owner: &str,
        run_id: i64,
        now_ms: i64,
        finished: &FinishedRun,
    ) -> Result<(), SessionError> {
        let transaction = self.begin_write(FINISH_OPERATION)?;
        let through = finished.through_ms;
        transaction.execute(
            "UPDATE learning_runs SET finished_ms = ?2, outcome = ?3, model = ?4, tokens = ?5, \
             summary = ?6, dropped = ?7, through_ms = coalesce(?8, through_ms) WHERE id = ?1",
            params![
                run_id,
                now_ms,
                finished.outcome,
                finished.model,
                i64::try_from(finished.tokens).unwrap_or(i64::MAX),
                finished.summary,
                finished.dropped,
                through,
            ],
        )?;
        for (seq, edit) in finished.edits.iter().enumerate() {
            transaction.execute(
                "INSERT INTO learning_edits (run_id, seq, target, path, before, after, evidence) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    run_id,
                    i64::try_from(seq).unwrap_or(i64::MAX),
                    edit.target,
                    edit.path,
                    edit.before,
                    edit.after,
                    edit.evidence
                ],
            )?;
        }
        if let Some(transcript) = &finished.transcript {
            transaction.execute(
                "INSERT OR REPLACE INTO learning_transcripts (run_id, body) VALUES (?1, ?2)",
                params![run_id, compress_payload(transcript)?],
            )?;
        }
        transaction.execute(
            "DELETE FROM learning_transcripts WHERE run_id IN (SELECT id FROM learning_runs \
             WHERE project = ?1 ORDER BY id DESC LIMIT -1 OFFSET ?2)",
            params![project, KEPT_TRANSCRIPTS],
        )?;
        transaction.execute(
            "UPDATE learning_projects SET through_ms = max(through_ms, coalesce(?3, through_ms)), \
             last_run_ms = CASE WHEN ?3 IS NULL THEN last_run_ms ELSE ?4 END, \
             lease_owner = NULL, lease_until_ms = NULL \
             WHERE project = ?1 AND (lease_owner = ?2 OR lease_owner IS NULL)",
            params![project, owner, through, now_ms],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn set_paused(&self, project: &str, paused: bool) -> Result<(), SessionError> {
        let transaction = self.begin_write(PAUSE_OPERATION)?;
        transaction.execute(
            "INSERT INTO learning_projects (project, through_ms, last_run_ms, paused) \
             VALUES (?1, 0, 0, ?2) ON CONFLICT(project) DO UPDATE SET paused = excluded.paused",
            params![project, paused],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// The project's runs, newest first.
    pub fn runs(&self, project: &str, limit: usize) -> Result<Vec<LearningRun>, SessionError> {
        let mut statement = self.connection().prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM learning_runs WHERE project = ?1 ORDER BY id DESC LIMIT ?2"
        ))?;
        let rows = statement.query_map(
            params![project, i64::try_from(limit).unwrap_or(i64::MAX)],
            run_row,
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Runs of the project that started at or after `since_ms`.
    pub fn runs_since(&self, project: &str, since_ms: i64) -> Result<usize, SessionError> {
        let count: i64 = self.connection().query_row(
            "SELECT count(*) FROM learning_runs WHERE project = ?1 AND started_ms >= ?2",
            params![project, since_ms],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(count).unwrap_or_default())
    }

    pub fn edits(&self, run_id: i64) -> Result<Vec<StoredEdit>, SessionError> {
        let mut statement = self.connection().prepare(&format!(
            "SELECT {EDIT_COLUMNS} FROM learning_edits WHERE run_id = ?1 ORDER BY seq"
        ))?;
        let rows = statement.query_map([run_id], edit_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The run's learner conversation as a JSON array of messages, `None`
    /// when it kept none.
    pub fn transcript(&self, run_id: i64) -> Result<Option<String>, SessionError> {
        let stored: Option<Vec<u8>> = self
            .connection()
            .query_row(
                "SELECT body FROM learning_transcripts WHERE run_id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .optional()?;
        stored
            .map(|body| decompress_payload(&body, TRANSCRIPT_FIELD))
            .transpose()
    }

    /// Marks an edit reverted. False when it already was.
    pub fn mark_reverted(&self, run_id: i64, seq: i64, now_ms: i64) -> Result<bool, SessionError> {
        let transaction = self.begin_write(REVERT_OPERATION)?;
        let marked = transaction.execute(
            "UPDATE learning_edits SET reverted_ms = ?3 \
             WHERE run_id = ?1 AND seq = ?2 AND reverted_ms IS NULL",
            params![run_id, seq, now_ms],
        )?;
        transaction.commit()?;
        Ok(marked > 0)
    }

    fn connection(&self) -> &Connection {
        self.database.connection()
    }

    fn begin_write(&self, operation: &'static str) -> Result<Transaction<'_>, SessionError> {
        let retry = RuntimeRetry::new(None, &|| false);
        retry.run(operation, || {
            Ok(Transaction::new_unchecked(
                self.connection(),
                TransactionBehavior::Immediate,
            )?)
        })
    }
}

fn project_row(
    connection: &Connection,
    project: &str,
) -> Result<Option<LearningProject>, SessionError> {
    Ok(connection
        .query_row(
            "SELECT through_ms, last_run_ms, paused, lease_owner, lease_until_ms \
             FROM learning_projects WHERE project = ?1",
            [project],
            |row| {
                Ok(LearningProject {
                    through_ms: row.get(0)?,
                    last_run_ms: row.get(1)?,
                    paused: row.get(2)?,
                    lease_owner: row.get(3)?,
                    lease_until_ms: row.get(4)?,
                })
            },
        )
        .optional()?)
}

fn run_row(row: &Row<'_>) -> rusqlite::Result<LearningRun> {
    Ok(LearningRun {
        id: row.get(0)?,
        started_ms: row.get(1)?,
        finished_ms: row.get(2)?,
        from_ms: row.get(3)?,
        through_ms: row.get(4)?,
        outcome: row.get(5)?,
        model: row.get(6)?,
        tokens: u64::try_from(row.get::<_, i64>(7)?).unwrap_or_default(),
        summary: row.get(8)?,
        dropped: row.get(9)?,
    })
}

fn edit_row(row: &Row<'_>) -> rusqlite::Result<StoredEdit> {
    Ok(StoredEdit {
        run_id: row.get(0)?,
        seq: row.get(1)?,
        edit: LearningEdit {
            target: row.get(2)?,
            path: row.get(3)?,
            before: row.get(4)?,
            after: row.get(5)?,
            evidence: row.get(6)?,
        },
        reverted_ms: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const PROJECT: &str = "/work/project";
    const OWNER: &str = "owner-a";
    const OTHER_OWNER: &str = "owner-b";
    const NOW_MS: i64 = 1_767_225_600_000;
    const LEASE_MS: i64 = 60_000;
    const FIRST_FROM_MS: i64 = NOW_MS - 7 * 24 * 3_600_000;
    const THROUGH_MS: i64 = NOW_MS - 60_000;
    const DONE: &str = "done";
    const MODEL: &str = "anthropic/claude-opus-5-5";
    const NOTE: &str = "flaky-tests.md";
    const BLOCKED: &str = "another owner's live lease blocks a second run";
    const TRANSCRIPT: &str = r#"[{"role":"user","content":"learn"}]"#;

    fn store() -> (TempDir, LearningStore) {
        let temp = TempDir::new().unwrap();
        let store = LearningStore::open(&StateDir::from_path(temp.path().to_path_buf())).unwrap();
        (temp, store)
    }

    fn finished(through_ms: Option<i64>) -> FinishedRun {
        FinishedRun {
            outcome: DONE.to_owned(),
            model: MODEL.to_owned(),
            tokens: 1_200,
            summary: String::new(),
            dropped: "[]".to_owned(),
            through_ms,
            edits: vec![LearningEdit {
                target: "memory".to_owned(),
                path: NOTE.to_owned(),
                before: None,
                after: Some("# Flaky tests".to_owned()),
                evidence: "[]".to_owned(),
            }],
            transcript: Some(TRANSCRIPT.to_owned()),
        }
    }

    #[test]
    fn a_new_project_starts_from_the_first_window() {
        let (_temp, store) = store();
        let begun = store
            .begin(PROJECT, OWNER, NOW_MS, LEASE_MS, FIRST_FROM_MS)
            .unwrap()
            .unwrap();
        assert_eq!(begun.from_ms, FIRST_FROM_MS);
        assert_eq!(store.runs(PROJECT, 10).unwrap()[0].outcome, RUNNING);
    }

    #[test_case(NOW_MS, false ; "while_the_lease_runs")]
    #[test_case(NOW_MS + LEASE_MS, true ; "once_it_lapses")]
    fn another_owner_waits_for_the_lease(at_ms: i64, admitted: bool) {
        let (_temp, store) = store();
        store
            .begin(PROJECT, OWNER, NOW_MS, LEASE_MS, FIRST_FROM_MS)
            .unwrap()
            .unwrap();
        let second = store
            .begin(PROJECT, OTHER_OWNER, at_ms, LEASE_MS, FIRST_FROM_MS)
            .unwrap();
        assert_eq!(second.is_some(), admitted, "{BLOCKED}");
        if admitted {
            assert_eq!(store.runs(PROJECT, 10).unwrap()[1].outcome, INTERRUPTED);
        }
    }

    #[test_case(Some(THROUGH_MS), THROUGH_MS ; "a_learned_run_moves_the_cursor")]
    #[test_case(None, FIRST_FROM_MS ; "a_failed_run_leaves_it")]
    fn finishing_records_the_run(through: Option<i64>, expected: i64) {
        let (_temp, store) = store();
        let begun = store
            .begin(PROJECT, OWNER, NOW_MS, LEASE_MS, FIRST_FROM_MS)
            .unwrap()
            .unwrap();
        store
            .finish(PROJECT, OWNER, begun.id, NOW_MS, &finished(through))
            .unwrap();
        let project = store.project(PROJECT).unwrap().unwrap();
        assert_eq!(project.through_ms, expected);
        assert_eq!(project.lease_owner, None);
        let edits = store.edits(begun.id).unwrap();
        assert_eq!(edits[0].edit.path, NOTE);
        assert!(store.mark_reverted(begun.id, 0, NOW_MS).unwrap());
        assert!(!store.mark_reverted(begun.id, 0, NOW_MS).unwrap());
    }

    #[test]
    fn only_the_newest_runs_keep_their_transcript() {
        let (_temp, store) = store();
        let runs: Vec<i64> = (0..=KEPT_TRANSCRIPTS)
            .map(|offset| {
                let begun = store
                    .begin(PROJECT, OWNER, NOW_MS + offset, LEASE_MS, FIRST_FROM_MS)
                    .unwrap()
                    .unwrap();
                store
                    .finish(PROJECT, OWNER, begun.id, NOW_MS + offset, &finished(None))
                    .unwrap();
                begun.id
            })
            .collect();
        assert_eq!(store.transcript(runs[0]).unwrap(), None);
        assert_eq!(
            store.transcript(*runs.last().unwrap()).unwrap().as_deref(),
            Some(TRANSCRIPT)
        );
    }

    #[test]
    fn a_paused_project_opens_no_run() {
        let (_temp, store) = store();
        store.set_paused(PROJECT, true).unwrap();
        assert_eq!(
            store
                .begin(PROJECT, OWNER, NOW_MS, LEASE_MS, FIRST_FROM_MS)
                .unwrap(),
            None
        );
        store.set_paused(PROJECT, false).unwrap();
        assert!(
            store
                .begin(PROJECT, OWNER, NOW_MS, LEASE_MS, FIRST_FROM_MS)
                .unwrap()
                .is_some()
        );
        assert_eq!(store.runs_since(PROJECT, NOW_MS).unwrap(), 1);
    }
}
