//! Session-owned history of foreground native shell executions. A record is
//! written before a command may start and again once it settles, so a record
//! that never settled can only be reported as interrupted; restoring history
//! never runs anything. Background shells keep their authoritative lifecycle in
//! `background_tasks`.

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use crate::{
    StorageError,
    background::JobOwner,
    id::CaudraId,
    sessions::{RuntimeRetry, SessionDatabase, SessionError, to_i64},
};

pub const MAX_SHELL_EXECUTIONS: usize = 200;
pub const MAX_SHELL_SUMMARY_BYTES: usize = 16 * 1024;
pub const MAX_SHELL_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_SHELL_COMMAND_BYTES: usize = 8 * 1024;
pub const MAX_SHELL_WORKDIR_BYTES: usize = 1024;
pub const MAX_SHELL_REASON_BYTES: usize = 2 * 1024;
const RUNTIME_SHELL_SAVE: &str = "shell execution save";
const RUNTIME_SHELL_INTERRUPT: &str = "shell execution interruption";
const CREATED_FIELD: &str = "shell execution creation time";
const BYTES_FIELD: &str = "shell execution bytes";
const SUMMARY_KIND: &str = "shell execution summary";
const OUTPUT_KIND: &str = "shell execution output";
const PAGE_KIND: &str = "shell execution page";

pub(crate) const TABLES: &str = r#"
CREATE TABLE shell_executions (
    session_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    execution_id TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    active INTEGER NOT NULL CHECK(active IN (0, 1)),
    summary TEXT NOT NULL,
    output TEXT,
    bytes INTEGER NOT NULL CHECK(bytes >= 0),
    PRIMARY KEY(session_id, execution_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX shell_executions_recent ON shell_executions(session_id, created_ms, execution_id);
"#;
pub(crate) const SESSION_BYTES: &str =
    "coalesce((SELECT sum(bytes) FROM shell_executions WHERE session_id = sessions.id), 0)";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellExecutionState {
    Preparing,
    Queued,
    Running,
    Cancelling,
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    Interrupted,
}

impl ShellExecutionState {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Preparing | Self::Queued | Self::Running | Self::Cancelling
        )
    }

    /// The serialized name, which is also what background shell jobs call the
    /// same states.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Cancelling => "cancelling",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellWorkflowOwner {
    pub run_id: String,
    pub epoch: u64,
    pub call_key: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellExecutionOwner {
    pub job: JobOwner,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<ShellWorkflowOwner>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellExecutionRecord {
    pub execution_id: String,
    pub owner: ShellExecutionOwner,
    pub call_id: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub command_truncated: bool,
    pub workdir: String,
    /// `None` when the location was never recorded, as for background jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<bool>,
    /// `None` until the command's own timeout is resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    pub state: ShellExecutionState,
    /// Whether the command was released to its executor, as opposed to settling
    /// while it was still being prepared.
    #[serde(default)]
    pub started: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Keyset position of the oldest record a page returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellHistoryCursor {
    pub created_at_ms: u64,
    pub execution_id: String,
}

impl From<&ShellExecutionRecord> for ShellHistoryCursor {
    fn from(record: &ShellExecutionRecord) -> Self {
        Self {
            created_at_ms: record.created_at_ms,
            execution_id: record.execution_id.clone(),
        }
    }
}

impl SessionDatabase {
    /// Writes one execution record and reports whether it was kept: a session
    /// that was never saved has nothing a record could belong to. Settled
    /// records beyond [`MAX_SHELL_EXECUTIONS`] are evicted oldest first; an
    /// active one never is.
    pub fn save_shell_execution_runtime(
        &self,
        session: CaudraId,
        record: &ShellExecutionRecord,
        output: Option<&Value>,
        retry: &RuntimeRetry<'_>,
    ) -> Result<bool, SessionError> {
        let summary = serde_json::to_string(record).map_err(StorageError::from)?;
        Self::validate_len(SUMMARY_KIND, summary.len(), MAX_SHELL_SUMMARY_BYTES)?;
        let output = output
            .map(serde_json::to_string)
            .transpose()
            .map_err(StorageError::from)?;
        let output_bytes = output.as_ref().map_or(0, String::len);
        Self::validate_len(OUTPUT_KIND, output_bytes, MAX_SHELL_OUTPUT_BYTES)?;
        let created = to_i64(record.created_at_ms, CREATED_FIELD)?;
        let bytes = to_i64(summary.len() + output_bytes, BYTES_FIELD)?;
        self.runtime_transaction(retry, RUNTIME_SHELL_SAVE, |transaction| {
            let saved = transaction.execute(
                "INSERT INTO shell_executions(session_id, execution_id, created_ms, active, summary, output, bytes) \
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7 WHERE EXISTS(SELECT 1 FROM sessions WHERE id = ?1) \
                 ON CONFLICT(session_id, execution_id) DO UPDATE SET active = excluded.active, \
                 summary = excluded.summary, output = excluded.output, bytes = excluded.bytes",
                params![
                    session.as_bytes().as_slice(),
                    record.execution_id,
                    created,
                    record.state.is_active(),
                    summary,
                    output,
                    bytes
                ],
            )?;
            transaction.execute(
                "DELETE FROM shell_executions WHERE session_id = ?1 AND active = 0 AND execution_id NOT IN (\
                    SELECT execution_id FROM shell_executions WHERE session_id = ?1 AND active = 0 \
                    ORDER BY created_ms DESC, execution_id DESC LIMIT ?2)",
                params![session.as_bytes().as_slice(), MAX_SHELL_EXECUTIONS as i64],
            )?;
            Ok(saved > 0)
        })
    }

    /// Newest first, strictly older than `before`. A record whose summary no
    /// longer parses is left out rather than guessed at.
    pub fn shell_executions(
        &self,
        session: CaudraId,
        before: Option<&ShellHistoryCursor>,
        limit: usize,
    ) -> Result<Vec<ShellExecutionRecord>, SessionError> {
        Self::validate_len(PAGE_KIND, limit, MAX_SHELL_EXECUTIONS)?;
        let created = before
            .map(|cursor| to_i64(cursor.created_at_ms, CREATED_FIELD))
            .transpose()?;
        let mut statement = self.connection().prepare(
            "SELECT execution_id, summary FROM shell_executions WHERE session_id = ?1 \
             AND (?2 IS NULL OR created_ms < ?2 OR (created_ms = ?2 AND execution_id < ?3)) \
             ORDER BY created_ms DESC, execution_id DESC LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![
                session.as_bytes().as_slice(),
                created,
                before.map(|cursor| cursor.execution_id.as_str()),
                limit as i64
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        let mut records = Vec::new();
        for row in rows {
            let (execution_id, summary) = row?;
            Self::validate_len(SUMMARY_KIND, summary.len(), MAX_SHELL_SUMMARY_BYTES)?;
            match serde_json::from_str::<ShellExecutionRecord>(&summary) {
                Ok(record) if record.execution_id == execution_id => records.push(record),
                Ok(_) => warn!(%execution_id, "shell execution summary names another execution"),
                Err(error) => warn!(%execution_id, %error, "unreadable shell execution summary"),
            }
        }
        Ok(records)
    }

    pub fn shell_execution_output(
        &self,
        session: CaudraId,
        execution_id: &str,
    ) -> Result<Option<Value>, SessionError> {
        let output = self
            .connection()
            .query_row(
                "SELECT output FROM shell_executions WHERE session_id = ?1 AND execution_id = ?2",
                params![session.as_bytes().as_slice(), execution_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        output
            .map(|output| {
                Self::validate_len(OUTPUT_KIND, output.len(), MAX_SHELL_OUTPUT_BYTES)?;
                Ok(serde_json::from_str(&output).map_err(StorageError::from)?)
            })
            .transpose()
    }

    /// Settles every record a previous runtime left unfinished. Their outcome is
    /// unknown, so they become interrupted and nothing is retried.
    pub fn interrupt_shell_executions(
        &self,
        session: CaudraId,
        reason: &str,
    ) -> Result<usize, SessionError> {
        self.interrupt_shell_executions_inner(session, reason, None)
    }

    pub fn interrupt_shell_executions_runtime(
        &self,
        session: CaudraId,
        reason: &str,
        retry: &RuntimeRetry<'_>,
    ) -> Result<usize, SessionError> {
        self.interrupt_shell_executions_inner(session, reason, Some(retry))
    }

    fn interrupt_shell_executions_inner(
        &self,
        session: CaudraId,
        reason: &str,
        retry: Option<&RuntimeRetry<'_>>,
    ) -> Result<usize, SessionError> {
        let write = |transaction: &Transaction<'_>| {
            let unfinished = {
                let mut statement = transaction.prepare(
                "SELECT summary, coalesce(length(CAST(output AS BLOB)), 0) FROM shell_executions \
                 WHERE session_id = ?1 AND active = 1",
            )?;
                let rows = statement.query_map(params![session.as_bytes().as_slice()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            let mut interrupted = 0;
            for (summary, output_bytes) in unfinished {
                let mut record: ShellExecutionRecord = match serde_json::from_str(&summary) {
                    Ok(record) => record,
                    Err(error) => {
                        warn!(%error, "unreadable unfinished shell execution summary");
                        continue;
                    }
                };
                record.state = ShellExecutionState::Interrupted;
                record.reason.get_or_insert_with(|| reason.to_owned());
                let summary = serde_json::to_string(&record).map_err(StorageError::from)?;
                Self::validate_len(SUMMARY_KIND, summary.len(), MAX_SHELL_SUMMARY_BYTES)?;
                interrupted += transaction.execute(
                    "UPDATE shell_executions SET active = 0, summary = ?3, bytes = ?4 \
                 WHERE session_id = ?1 AND execution_id = ?2",
                    params![
                        session.as_bytes().as_slice(),
                        record.execution_id,
                        summary,
                        to_i64(summary.len(), BYTES_FIELD)? + output_bytes
                    ],
                )?;
            }
            Ok(interrupted)
        };
        if let Some(retry) = retry {
            return self.runtime_transaction(retry, RUNTIME_SHELL_INTERRUPT, write);
        }
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        let interrupted = write(&transaction)?;
        transaction.commit()?;
        Ok(interrupted)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use rusqlite::{Transaction, TransactionBehavior};
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::{
        MAX_SHELL_EXECUTIONS, MAX_SHELL_OUTPUT_BYTES, ShellExecutionOwner, ShellExecutionRecord,
        ShellExecutionState, ShellHistoryCursor, ShellWorkflowOwner,
    };
    use crate::{
        StateDir,
        background::JobOwner,
        id::CaudraId,
        sessions::{RuntimeRetry, Session, SessionDatabase, SessionError, TitleSource},
    };

    const MODEL: &str = "test/model";
    const CWD: &str = "/project";
    const COMMAND: &str = "cargo test";
    const REASON: &str = "the runtime ended before the command settled";
    const CHILD: &str = "child-invocation";
    const TASK: &str = "implement-footer-chips";

    #[derive(Clone, Serialize, Deserialize)]
    struct TestMessage;

    impl TitleSource for TestMessage {
        fn first_user_text(&self) -> Option<&str> {
            None
        }
    }

    type TestSession = Session<TestMessage, Value, Value>;

    fn record(index: u64, state: ShellExecutionState) -> ShellExecutionRecord {
        ShellExecutionRecord {
            execution_id: format!("execution-{index:04}"),
            owner: ShellExecutionOwner {
                job: JobOwner::Child {
                    invocation_id: CHILD.into(),
                },
                task_id: Some(TASK.into()),
                workflow: Some(ShellWorkflowOwner {
                    run_id: "run".into(),
                    epoch: 1,
                    call_key: 2,
                }),
            },
            call_id: format!("call-{index}"),
            command: COMMAND.into(),
            command_truncated: false,
            workdir: ".".into(),
            remote: Some(false),
            timeout_ms: Some(120_000),
            created_at_ms: 1_000 + index,
            finished_at_ms: (!state.is_active()).then_some(2_000 + index),
            state,
            started: true,
            reason: None,
        }
    }

    fn fixture() -> (tempfile::TempDir, SessionDatabase, CaudraId) {
        let temp = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(temp.path().to_path_buf());
        let mut database = SessionDatabase::open(&state).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        (temp, database, session.id)
    }

    fn save(
        database: &SessionDatabase,
        session: CaudraId,
        record: &ShellExecutionRecord,
        output: Option<&Value>,
    ) -> Result<bool, SessionError> {
        database.save_shell_execution_runtime(
            session,
            record,
            output,
            &RuntimeRetry::new(None, &|| false),
        )
    }

    fn ids(records: &[ShellExecutionRecord]) -> Vec<String> {
        records
            .iter()
            .map(|record| record.execution_id.clone())
            .collect()
    }

    #[test_case(ShellExecutionState::Preparing)]
    #[test_case(ShellExecutionState::Queued)]
    #[test_case(ShellExecutionState::Running)]
    #[test_case(ShellExecutionState::Cancelling)]
    #[test_case(ShellExecutionState::Succeeded)]
    #[test_case(ShellExecutionState::Failed)]
    #[test_case(ShellExecutionState::TimedOut)]
    #[test_case(ShellExecutionState::Cancelled)]
    #[test_case(ShellExecutionState::Interrupted)]
    fn state_names_match_their_serialized_form(state: ShellExecutionState) {
        assert_eq!(serde_json::to_value(state).unwrap(), state.as_str());
    }

    #[test]
    fn pages_are_newest_first_and_resume_after_the_cursor() {
        let (_temp, database, session) = fixture();
        for index in 0..5 {
            save(
                &database,
                session,
                &record(index, ShellExecutionState::Succeeded),
                None,
            )
            .unwrap();
        }
        let first = database.shell_executions(session, None, 2).unwrap();
        assert_eq!(ids(&first), ["execution-0004", "execution-0003"]);
        let cursor = ShellHistoryCursor::from(first.last().unwrap());
        let second = database
            .shell_executions(session, Some(&cursor), 10)
            .unwrap();
        assert_eq!(
            ids(&second),
            ["execution-0002", "execution-0001", "execution-0000"]
        );
        assert_eq!(second[0], record(2, ShellExecutionState::Succeeded));
    }

    #[test]
    fn output_is_loaded_separately_and_replaced_by_a_later_save() {
        let (_temp, database, session) = fixture();
        let running = record(0, ShellExecutionState::Preparing);
        save(&database, session, &running, None).unwrap();
        assert_eq!(
            database
                .shell_execution_output(session, &running.execution_id)
                .unwrap(),
            None
        );
        let output = json!({"stdout": "ok", "exit_code": 0});
        let settled = record(0, ShellExecutionState::Succeeded);
        save(&database, session, &settled, Some(&output)).unwrap();
        assert_eq!(
            database
                .shell_execution_output(session, &settled.execution_id)
                .unwrap(),
            Some(output)
        );
        assert_eq!(
            database.shell_executions(session, None, 10).unwrap(),
            [settled]
        );
    }

    #[test]
    fn retention_evicts_the_oldest_settled_records_but_never_an_active_one() {
        let (_temp, database, session) = fixture();
        save(
            &database,
            session,
            &record(0, ShellExecutionState::Running),
            None,
        )
        .unwrap();
        for index in 1..=MAX_SHELL_EXECUTIONS as u64 + 2 {
            save(
                &database,
                session,
                &record(index, ShellExecutionState::Failed),
                None,
            )
            .unwrap();
        }
        let mut records = Vec::new();
        let mut cursor = None;
        loop {
            let page = database
                .shell_executions(session, cursor.as_ref(), MAX_SHELL_EXECUTIONS)
                .unwrap();
            let Some(last) = page.last() else {
                break;
            };
            cursor = Some(ShellHistoryCursor::from(last));
            records.extend(page);
        }
        assert_eq!(records.len(), MAX_SHELL_EXECUTIONS + 1);
        assert!(
            records
                .iter()
                .any(|record| record.execution_id == "execution-0000")
        );
        assert!(!records.iter().any(|record| {
            record.execution_id == "execution-0001" || record.execution_id == "execution-0002"
        }));
    }

    #[test]
    fn interruption_settles_only_unfinished_records_and_keeps_their_output() {
        let (_temp, database, session) = fixture();
        let output = json!({"stdout": "partial"});
        save(
            &database,
            session,
            &record(0, ShellExecutionState::Running),
            Some(&output),
        )
        .unwrap();
        save(
            &database,
            session,
            &record(1, ShellExecutionState::Succeeded),
            None,
        )
        .unwrap();
        assert_eq!(
            database
                .interrupt_shell_executions(session, REASON)
                .unwrap(),
            1
        );
        assert_eq!(
            database
                .interrupt_shell_executions(session, REASON)
                .unwrap(),
            0
        );
        let records = database.shell_executions(session, None, 10).unwrap();
        assert_eq!(records[0].state, ShellExecutionState::Succeeded);
        assert_eq!(records[1].state, ShellExecutionState::Interrupted);
        assert_eq!(records[1].reason.as_deref(), Some(REASON));
        assert_eq!(
            database
                .shell_execution_output(session, &records[1].execution_id)
                .unwrap(),
            Some(output)
        );
    }

    #[test_case(true; "oversized_output")]
    #[test_case(false; "oversized_command")]
    fn oversized_records_are_refused_before_writing(output: bool) {
        let (_temp, database, session) = fixture();
        let mut oversized = record(0, ShellExecutionState::Preparing);
        let body = json!("x".repeat(MAX_SHELL_OUTPUT_BYTES));
        if !output {
            oversized.command = "x".repeat(MAX_SHELL_OUTPUT_BYTES);
        }
        let result = save(&database, session, &oversized, output.then_some(&body));
        assert!(matches!(result, Err(SessionError::LimitExceeded { .. })));
        assert!(
            database
                .shell_executions(session, None, 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn runtime_save_retries_writer_acquisition_with_the_identical_record() {
        let (temp, database, session) = fixture();
        let state = StateDir::from_path(temp.path().to_path_buf());
        let runtime =
            SessionDatabase::open_runtime(&state, &RuntimeRetry::new(None, &|| false)).unwrap();
        let writer = RefCell::new(Some(
            Transaction::new_unchecked(database.connection(), TransactionBehavior::Immediate)
                .unwrap(),
        ));
        let checks = Cell::new(0);
        let cancelled = || {
            checks.set(checks.get() + 1);
            if checks.get() == 2 {
                writer.borrow_mut().take().unwrap().rollback().unwrap();
            }
            false
        };
        let saved = record(0, ShellExecutionState::Preparing);
        runtime
            .save_shell_execution_runtime(
                session,
                &saved,
                None,
                &RuntimeRetry::new(None, &cancelled),
            )
            .unwrap();
        assert!(writer.borrow().is_none());
        assert_eq!(
            runtime.shell_executions(session, None, 10).unwrap(),
            [saved]
        );
    }

    #[test_case(false; "empty_history")]
    #[test_case(true; "unfinished_execution")]
    fn runtime_interruption_retries_writer_contention(unfinished: bool) {
        let (temp, database, session) = fixture();
        let mut expected = record(0, ShellExecutionState::Running);
        if unfinished {
            save(&database, session, &expected, None).unwrap();
            expected.state = ShellExecutionState::Interrupted;
            expected.reason = Some(REASON.into());
        }
        let state = StateDir::from_path(temp.path().to_path_buf());
        let runtime =
            SessionDatabase::open_runtime(&state, &RuntimeRetry::new(None, &|| false)).unwrap();
        let writer = RefCell::new(Some(
            Transaction::new_unchecked(database.connection(), TransactionBehavior::Immediate)
                .unwrap(),
        ));
        let checks = Cell::new(0);
        let cancelled = || {
            checks.set(checks.get() + 1);
            if checks.get() == 2 {
                writer.borrow_mut().take().unwrap().rollback().unwrap();
            }
            false
        };
        assert_eq!(
            runtime
                .interrupt_shell_executions_runtime(
                    session,
                    REASON,
                    &RuntimeRetry::new(None, &cancelled)
                )
                .unwrap(),
            usize::from(unfinished)
        );
        assert!(writer.borrow().is_none());
        assert_eq!(
            runtime
                .shell_executions(session, None, MAX_SHELL_EXECUTIONS)
                .unwrap(),
            if unfinished {
                vec![expected]
            } else {
                Vec::new()
            }
        );
    }

    #[test]
    fn a_session_that_was_never_saved_keeps_no_record() {
        let (_temp, database, _session) = fixture();
        let unsaved = CaudraId::generate();
        let saved = save(
            &database,
            unsaved,
            &record(0, ShellExecutionState::Running),
            None,
        )
        .unwrap();
        assert!(!saved);
        assert!(
            database
                .shell_executions(unsaved, None, 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_cancelled_initial_write_is_rolled_back() {
        let (_temp, database, session) = fixture();
        let result = database.save_shell_execution_runtime(
            session,
            &record(0, ShellExecutionState::Preparing),
            None,
            &RuntimeRetry::new(None, &|| true),
        );
        assert!(matches!(result, Err(SessionError::RuntimeCancelled { .. })));
        assert!(
            database
                .shell_executions(session, None, 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn retention_facts_account_for_summary_and_output_bytes() {
        let (_temp, database, session) = fixture();
        let facts =
            |database: &SessionDatabase| database.session_facts(None).unwrap()[0].logical_bytes;
        let before = facts(&database);
        let saved = record(0, ShellExecutionState::Succeeded);
        let output = json!({"stdout": "ok"});
        save(&database, session, &saved, Some(&output)).unwrap();
        let expected = serde_json::to_string(&saved).unwrap().len()
            + serde_json::to_string(&output).unwrap().len();
        assert_eq!(facts(&database), before + expected as u64);
    }

    #[test]
    fn deleting_the_session_removes_its_history() {
        let (_temp, mut database, session) = fixture();
        save(
            &database,
            session,
            &record(0, ShellExecutionState::Succeeded),
            Some(&json!({})),
        )
        .unwrap();
        database.delete(session, None).unwrap();
        let remaining: i64 = database
            .connection()
            .query_row("SELECT count(*) FROM shell_executions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 0);
    }
}
