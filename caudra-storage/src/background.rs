use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    StorageError,
    id::CaudraId,
    sessions::{SessionDatabase, SessionError},
    tool_outputs::ToolOutputRef,
};

pub const MAX_INVOCATIONS: usize = 128;
pub const MAX_REPORTS: usize = 32;
pub const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_SESSION_BYTES: usize = 64 * 1024 * 1024;
const JOB_EVENT_FIELD: &str = "job event provenance";
const INVALID_JOB_EVENT: &str =
    "job event does not match its recorded task, event, or owning invocation";
pub(crate) const TABLES: &str = r#"
CREATE TABLE background_tasks (
    session_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    invocation_id TEXT NOT NULL,
    payload TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    PRIMARY KEY(session_id, invocation_id)
) STRICT, WITHOUT ROWID;
CREATE TABLE background_receipts (
    session_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    event_id TEXT NOT NULL,
    PRIMARY KEY(session_id, event_id)
) STRICT, WITHOUT ROWID;
"#;
pub(crate) const SESSION_BYTES: &str = "coalesce((SELECT sum(bytes) FROM background_tasks WHERE session_id = sessions.id), 0) + coalesce((SELECT sum(length(event_id)) FROM background_receipts WHERE session_id = sessions.id), 0)";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    #[default]
    Agent,
    Shell,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobOwner {
    #[default]
    Main,
    Child {
        invocation_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellJobMetadata {
    pub call_id: String,
    pub root_call_id: String,
    pub command: String,
    pub workdir: String,
    pub timeout_ms: u64,
    pub mode: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "metadata", rename_all = "snake_case")]
pub enum JobPayload {
    #[default]
    Agent,
    Shell(ShellJobMetadata),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEvent {
    pub sequence: u64,
    pub event_id: String,
    pub call_id: String,
    pub body: String,
    pub terminal: bool,
    pub accepted: bool,
    pub suppressed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    #[serde(default)]
    pub payload: JobPayload,
    #[serde(default)]
    pub owner: JobOwner,
    pub created_at: u64,
    pub updated_at: u64,
    pub sequence: u64,
    pub task_id: String,
    pub invocation_id: String,
    pub root_call_id: String,
    pub generation: u64,
    pub state: String,
    pub background: bool,
    pub receipt_accepted: bool,
    pub mode: String,
    pub request: Value,
    pub outcome: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_ref: Option<ToolOutputRef>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub history: Value,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub spec: Value,
    pub events: Vec<TaskEvent>,
}

impl TaskRecord {
    pub fn kind(&self) -> JobKind {
        match self.payload {
            JobPayload::Agent => JobKind::Agent,
            JobPayload::Shell(_) => JobKind::Shell,
        }
    }

    pub fn active(&self) -> bool {
        matches!(self.state.as_str(), "queued" | "running" | "cancelling")
    }
}

pub fn accept_owned_job_event(
    transaction: &Transaction<'_>,
    session: CaudraId,
    owner: &JobOwner,
    message: &Value,
) -> Result<(), SessionError> {
    let Some(origin) = message.get("task_event") else {
        return Ok(());
    };
    let (Some(invocation), Some(task), Some(event)) = (
        origin.get("invocation_id").and_then(Value::as_str),
        origin.get("task_id").and_then(Value::as_str),
        origin.get("event_id").and_then(Value::as_str),
    ) else {
        return Ok(());
    };
    let payload: Option<String> = transaction
        .query_row(
            "SELECT payload FROM background_tasks WHERE session_id = ?1 AND invocation_id = ?2",
            params![session.as_bytes().as_slice(), invocation],
            |row| row.get(0),
        )
        .optional()?;
    let Some(payload) = payload else {
        return Ok(());
    };
    SessionDatabase::validate_len("background task", payload.len(), MAX_RECORD_BYTES)?;
    let record: TaskRecord = serde_json::from_str(&payload).map_err(StorageError::from)?;
    if record.task_id != task
        || !record
            .events
            .iter()
            .any(|candidate| candidate.event_id == event)
    {
        return Err(SessionError::CorruptDatabaseValue {
            field: JOB_EVENT_FIELD,
            reason: INVALID_JOB_EVENT.into(),
        });
    }
    if record.owner != *owner {
        let historical_child = matches!((&record.owner, owner), (JobOwner::Child { .. }, JobOwner::Child { .. }))
            && transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM background_receipts WHERE session_id = ?1 AND event_id = ?2)",
                params![session.as_bytes().as_slice(), event], |row| row.get::<_, bool>(0),
            )?;
        return if historical_child {
            Ok(())
        } else {
            Err(SessionError::CorruptDatabaseValue {
                field: JOB_EVENT_FIELD,
                reason: INVALID_JOB_EVENT.into(),
            })
        };
    }
    transaction.execute(
        "INSERT OR IGNORE INTO background_receipts(session_id, event_id) VALUES (?1, ?2)",
        params![session.as_bytes().as_slice(), event],
    )?;
    Ok(())
}

impl SessionDatabase {
    pub fn task_identity_exists(
        &self,
        session: CaudraId,
        task_id: &str,
    ) -> Result<bool, SessionError> {
        Ok(self.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM subagent_streams WHERE session_id = ?1 AND subagent_id = ?2)
             OR EXISTS(SELECT 1 FROM subagents WHERE session_id = ?1 AND tool_use_id = ?2)
             OR EXISTS(SELECT 1 FROM background_tasks WHERE session_id = ?1 AND json_extract(payload, '$.task_id') = ?2)
             OR EXISTS(SELECT 1 FROM job_owner_checkpoints WHERE session_id = ?1 AND task_id = ?2)
             OR EXISTS(SELECT 1 FROM workflow_calls c JOIN workflow_runs r USING(run_id) WHERE r.session_id = ?1 AND c.task_id = ?2)
             OR EXISTS(SELECT 1 FROM workflow_runs r, json_each(r.roster) entry WHERE r.session_id = ?1 AND json_extract(entry.value, '$.task_id') = ?2)",
            params![session.as_bytes().as_slice(), task_id],
            |row| row.get(0),
        )?)
    }

    pub fn background_tasks(&self, session: CaudraId) -> Result<Vec<TaskRecord>, SessionError> {
        let bytes: u32 = self.connection().query_row(
            "SELECT coalesce(sum(bytes), 0) FROM background_tasks WHERE session_id = ?1",
            params![session.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        Self::validate_len(
            "background session bytes",
            bytes as usize,
            MAX_SESSION_BYTES,
        )?;
        let mut statement = self.connection().prepare(
            "SELECT payload FROM background_tasks WHERE session_id = ?1 ORDER BY invocation_id LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![session.as_bytes().as_slice(), MAX_INVOCATIONS as i64],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| {
            let payload = row?;
            Self::validate_len("background task", payload.len(), MAX_RECORD_BYTES)?;
            Ok(serde_json::from_str(&payload).map_err(StorageError::from)?)
        })
        .collect()
    }

    pub fn save_background_task(
        &self,
        session: CaudraId,
        record: &TaskRecord,
    ) -> Result<(), SessionError> {
        let payload = serde_json::to_string(record).map_err(StorageError::from)?;
        Self::validate_len("background task", payload.len(), MAX_RECORD_BYTES)?;
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        let count: i64 = transaction.query_row(
            "SELECT count(*) FROM background_tasks WHERE session_id = ?1 AND invocation_id != ?2",
            params![session.as_bytes().as_slice(), record.invocation_id],
            |row| row.get(0),
        )?;
        Self::validate_len(
            "background invocations",
            count as usize + 1,
            MAX_INVOCATIONS,
        )?;
        let bytes: u32 = transaction.query_row("SELECT coalesce(sum(bytes), 0) FROM background_tasks WHERE session_id = ?1 AND invocation_id != ?2", params![session.as_bytes().as_slice(), record.invocation_id], |row| row.get(0))?;
        Self::validate_len(
            "background session bytes",
            (bytes as usize).saturating_add(payload.len()),
            MAX_SESSION_BYTES,
        )?;
        transaction.execute(
            "INSERT INTO background_tasks(session_id, invocation_id, payload, bytes) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(session_id, invocation_id) DO UPDATE SET payload = excluded.payload, bytes = excluded.bytes",
            params![session.as_bytes().as_slice(), record.invocation_id, payload, payload.len() as i64],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn background_event_accepted(
        &self,
        session: CaudraId,
        event_id: &str,
    ) -> Result<bool, SessionError> {
        Ok(self
            .connection()
            .query_row(
                "SELECT 1 FROM background_receipts WHERE session_id = ?1 AND event_id = ?2",
                params![session.as_bytes().as_slice(), event_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn background_accepted_events(
        &self,
        session: CaudraId,
        event_ids: &[String],
    ) -> Result<HashSet<String>, SessionError> {
        Self::validate_len(
            "background receipt lookup",
            event_ids.len(),
            MAX_INVOCATIONS * (MAX_REPORTS + 1),
        )?;
        let transaction = self.connection().unchecked_transaction()?;
        let mut accepted = HashSet::new();
        {
            let mut query = transaction.prepare(
                "SELECT 1 FROM background_receipts WHERE session_id = ?1 AND event_id = ?2",
            )?;
            for event_id in event_ids {
                if query
                    .query_row(params![session.as_bytes().as_slice(), event_id], |_| Ok(()))
                    .optional()?
                    .is_some()
                {
                    accepted.insert(event_id.clone());
                }
            }
        }
        transaction.commit()?;
        Ok(accepted)
    }
}

pub(crate) fn protects_session(
    connection: &Connection,
    session: CaudraId,
) -> Result<bool, SessionError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM background_tasks WHERE session_id = ?1 AND (json_extract(payload, '$.state') IN ('queued', 'running', 'cancelling') OR EXISTS(SELECT 1 FROM json_each(payload, '$.events') WHERE json_extract(value, '$.accepted') = 0 AND json_extract(value, '$.suppressed') = 0)))",
        params![session.as_bytes().as_slice()], |row| row.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use std::fs::{self, File, FileTimes};
    use std::slice::from_ref;
    use std::time::SystemTime;
    use test_case::test_case;

    use super::{
        JobKind, JobOwner, JobPayload, ShellJobMetadata, TaskEvent, TaskRecord,
        accept_owned_job_event,
    };
    use crate::{
        StateDir,
        id::CaudraId,
        sessions::{Session, SessionDatabase, SessionError, SessionLease, TitleSource},
        tool_outputs::{TOOL_OUTPUT_DIR, ToolOutputStore},
    };

    const TASK: &str = "task";
    const INVOCATION: &str = "invocation";
    const EVENT: &str = "event";
    const MODEL: &str = "test/model";
    const CWD: &str = "/project";
    const CHILD: &str = "exact-child-invocation";
    const OTHER_CHILD: &str = "other-child-invocation";

    #[derive(Clone, Serialize, Deserialize)]
    struct TestMessage {
        task_event: Value,
    }

    impl TitleSource for TestMessage {
        fn first_user_text(&self) -> Option<&str> {
            None
        }
    }

    type TestSession = Session<TestMessage, Value, Value>;

    fn record() -> TaskRecord {
        TaskRecord {
            payload: Default::default(),
            owner: Default::default(),
            created_at: 1,
            updated_at: 1,
            sequence: 1,
            task_id: TASK.into(),
            invocation_id: INVOCATION.into(),
            root_call_id: TASK.into(),
            generation: 1,
            state: "succeeded".into(),
            background: true,
            receipt_accepted: true,
            mode: "build".into(),
            request: json!({}),
            outcome: Some(json!({"success":true})),
            output_ref: None,
            history: json!([]),
            spec: json!({}),
            events: vec![TaskEvent {
                sequence: 2,
                event_id: EVENT.into(),
                call_id: INVOCATION.into(),
                body: TASK.into(),
                terminal: true,
                accepted: false,
                suppressed: false,
            }],
        }
    }

    #[test]
    fn legacy_task_record_defaults_to_no_output_reference() {
        let value = serde_json::to_value(record()).unwrap();
        assert!(value.get("output_ref").is_none());
        assert!(
            serde_json::from_value::<TaskRecord>(value)
                .unwrap()
                .output_ref
                .is_none()
        );
    }

    #[test_case(false; "legacy_agent")]
    #[test_case(true; "shell_without_agent_history")]
    fn job_record_migration_preserves_kind_and_owner(shell: bool) {
        let mut task = record();
        if shell {
            task.payload = JobPayload::Shell(ShellJobMetadata {
                call_id: TASK.into(),
                root_call_id: TASK.into(),
                command: TASK.into(),
                workdir: CWD.into(),
                timeout_ms: 120_000,
                mode: "build".into(),
            });
            task.owner = JobOwner::Child {
                invocation_id: CHILD.into(),
            };
            task.history = Value::Null;
            task.spec = Value::Null;
        }
        let mut value = serde_json::to_value(&task).unwrap();
        if !shell {
            value.as_object_mut().unwrap().remove("payload");
            value.as_object_mut().unwrap().remove("owner");
        } else {
            assert!(value.get("history").is_none());
            assert!(value.get("spec").is_none());
        }
        let restored: TaskRecord = serde_json::from_value(value).unwrap();
        assert_eq!(
            restored.kind(),
            if shell {
                JobKind::Shell
            } else {
                JobKind::Agent
            }
        );
        assert_eq!(restored.owner, task.owner);
        assert_eq!(restored.events[0].event_id, EVENT);
    }

    #[test_case(false; "rollback_does_not_accept")]
    #[test_case(true; "commit_accepts_exact_owner_only")]
    fn owner_receipts_share_history_transaction(commit: bool) {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let mut session = TestSession::new(MODEL, CWD);
        session.save(&dir).unwrap();
        let mut task = record();
        task.owner = JobOwner::Child {
            invocation_id: CHILD.into(),
        };
        let database = SessionDatabase::open(&dir).unwrap();
        database.save_background_task(session.id, &task).unwrap();
        let message = json!({"task_event": {"invocation_id": INVOCATION, "task_id": TASK, "event_id": EVENT}});
        for owner in [
            JobOwner::Main,
            JobOwner::Child {
                invocation_id: OTHER_CHILD.into(),
            },
        ] {
            let transaction = database.connection().unchecked_transaction().unwrap();
            assert!(accept_owned_job_event(&transaction, session.id, &owner, &message).is_err());
            transaction.commit().unwrap();
            assert!(
                !database
                    .background_event_accepted(session.id, EVENT)
                    .unwrap()
            );
        }
        let transaction = database.connection().unchecked_transaction().unwrap();
        accept_owned_job_event(&transaction, session.id, &task.owner, &message).unwrap();
        if commit {
            transaction.commit().unwrap();
        } else {
            transaction.rollback().unwrap();
        }
        assert_eq!(
            database
                .background_event_accepted(session.id, EVENT)
                .unwrap(),
            commit
        );
    }

    #[test]
    fn continuation_preserves_only_previously_accepted_child_events() {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let mut session = TestSession::new(MODEL, CWD);
        session.save(&dir).unwrap();
        let database = SessionDatabase::open(&dir).unwrap();
        let mut task = record();
        task.owner = JobOwner::Child {
            invocation_id: CHILD.into(),
        };
        database.save_background_task(session.id, &task).unwrap();
        let message = json!({"task_event": {"invocation_id": INVOCATION, "task_id": TASK, "event_id": EVENT}});
        assert!(
            database
                .checkpoint_job_owner(session.id, OTHER_CHILD, TASK, from_ref(&message))
                .is_err()
        );
        database
            .checkpoint_job_owner(session.id, CHILD, TASK, from_ref(&message))
            .unwrap();
        database
            .checkpoint_job_owner(session.id, OTHER_CHILD, TASK, from_ref(&message))
            .unwrap();
        let mut forged = message.clone();
        forged["task_event"]["task_id"] = json!(OTHER_CHILD);
        assert!(
            database
                .checkpoint_job_owner(session.id, OTHER_CHILD, TASK, &[forged])
                .is_err()
        );
        let transaction = database.connection().unchecked_transaction().unwrap();
        assert!(
            accept_owned_job_event(&transaction, session.id, &JobOwner::Main, &message).is_err()
        );
    }

    #[test]
    fn owner_checkpoints_reserve_task_identities_without_agent_streams() {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let mut session = TestSession::new(MODEL, CWD);
        session.save(&dir).unwrap();
        let database = SessionDatabase::open(&dir).unwrap();
        assert!(!database.task_identity_exists(session.id, TASK).unwrap());
        database
            .checkpoint_job_owner::<Value>(session.id, CHILD, TASK, &[])
            .unwrap();
        assert!(database.task_identity_exists(session.id, TASK).unwrap());
        assert!(
            !database
                .task_identity_exists(CaudraId::generate(), TASK)
                .unwrap()
        );
    }

    #[test]
    fn task_outcome_reference_is_retained_accounted_and_session_scoped() {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let mut session = TestSession::new(MODEL, CWD);
        session.save(&dir).unwrap();
        let store = ToolOutputStore::new(dir.clone());
        let reference = store.put(session.id, TASK).unwrap();
        let orphan = store.put(session.id, INVOCATION).unwrap();
        let mut task = record();
        task.output_ref = Some(reference.clone());
        let db = SessionDatabase::open(&dir).unwrap();
        db.save_background_task(session.id, &task).unwrap();
        for entry in fs::read_dir(
            dir.path()
                .join(TOOL_OUTPUT_DIR)
                .join(session.id.to_string()),
        )
        .unwrap()
        {
            File::open(entry.unwrap().path())
                .unwrap()
                .set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
                .unwrap();
        }
        assert_eq!(store.cleanup_orphans(&[session.id]).unwrap(), 1);
        assert_eq!(
            store.load_text(session.id, reference.id.clone()).unwrap(),
            TASK
        );
        assert!(store.load_text(session.id, orphan.id).is_err());
        assert!(
            store
                .load_text(CaudraId::generate(), reference.id.clone())
                .is_err()
        );
        assert_eq!(
            db.stats().unwrap().tool_output_file_bytes,
            reference.byte_count as u64
        );
        assert_eq!(
            db.stats().unwrap().background_bytes,
            serde_json::to_vec(&task).unwrap().len() as u64
        );
        assert_eq!(
            db.background_tasks(session.id).unwrap()[0].output_ref,
            Some(reference)
        );
    }

    #[test_case(false; "background_record")]
    #[test_case(true; "historical_stream")]
    fn task_identity_lookup_retains_completed_ids_after_reopen(stream: bool) {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let mut session = TestSession::new(MODEL, CWD);
        session.save(&dir).unwrap();
        {
            let db = SessionDatabase::open(&dir).unwrap();
            assert!(!db.task_identity_exists(session.id, TASK).unwrap());
            if stream {
                db.connection()
                    .execute(
                        "INSERT INTO subagent_streams(session_id, subagent_id) VALUES (?1, ?2)",
                        rusqlite::params![session.id.as_bytes().as_slice(), TASK],
                    )
                    .unwrap();
            } else {
                db.save_background_task(session.id, &record()).unwrap();
            }
        }
        let db = SessionDatabase::open(&dir).unwrap();
        assert!(db.task_identity_exists(session.id, TASK).unwrap());
        assert!(!db.task_identity_exists(CaudraId::generate(), TASK).unwrap());
        assert!(!db.task_identity_exists(session.id, INVOCATION).unwrap());
    }

    #[test]
    fn task_rows_are_accounted_isolated_and_deleted_with_the_session() {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let mut session = TestSession::new(MODEL, CWD);
        session.save(&dir).unwrap();
        let mut db = SessionDatabase::open(&dir).unwrap();
        db.save_background_task(session.id, &record()).unwrap();
        assert_eq!(db.background_tasks(session.id).unwrap().len(), 1);
        assert!(
            db.background_tasks(CaudraId::generate())
                .unwrap()
                .is_empty()
        );
        let stats = db.stats().unwrap();
        assert_eq!(stats.background_invocation_count, 1);
        assert_eq!(
            stats.background_bytes,
            serde_json::to_vec(&record()).unwrap().len() as u64
        );
        session.push_message(TestMessage {
            task_event: json!({"task_id":TASK,"invocation_id":INVOCATION,"event_id":EVENT}),
        });
        session.save(&dir).unwrap();
        assert!(db.background_event_accepted(session.id, EVENT).unwrap());
        let event_ids = [EVENT.to_owned(), INVOCATION.to_owned()];
        let accepted = db
            .background_accepted_events(session.id, &event_ids)
            .unwrap();
        assert_eq!(accepted.len(), 1);
        assert!(accepted.contains(EVENT));
        assert!(
            db.background_accepted_events(CaudraId::generate(), &event_ids)
                .unwrap()
                .is_empty()
        );
        session.replace_messages(Vec::new());
        session.save(&dir).unwrap();
        assert!(db.background_event_accepted(session.id, EVENT).unwrap());
        db.delete(session.id, session.persisted_write_version())
            .unwrap();
        assert!(db.background_tasks(session.id).unwrap().is_empty());
        assert!(!db.background_event_accepted(session.id, EVENT).unwrap());
    }

    #[test_case("running", true, false; "active")]
    #[test_case("succeeded", false, false; "undelivered")]
    #[test_case("succeeded", true, true; "delivered")]
    fn trim_protects_active_and_undelivered_tasks(state: &str, accepted: bool, can_trim: bool) {
        let temp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(temp.path().to_path_buf());
        let mut session = TestSession::new(MODEL, CWD);
        session.save(&dir).unwrap();
        let mut db = SessionDatabase::open(&dir).unwrap();
        let mut record = record();
        record.state = state.into();
        record.events[0].accepted = accepted;
        db.save_background_task(session.id, &record).unwrap();
        let lease = SessionLease::acquire(&dir, session.id).unwrap();
        let trimmed = db.trim(&lease);
        if can_trim {
            assert!(trimmed.is_ok());
        } else {
            assert!(
                matches!(trimmed, Err(SessionError::BackgroundTasksPending { id }) if id == session.id)
            );
        }
        assert_eq!(db.background_tasks(session.id).unwrap().len(), 1);
    }
}
