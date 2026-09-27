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
    pub history: Value,
    pub spec: Value,
    pub events: Vec<TaskEvent>,
}

impl TaskRecord {
    pub fn active(&self) -> bool {
        matches!(self.state.as_str(), "queued" | "running" | "cancelling")
    }
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
    use std::time::SystemTime;
    use test_case::test_case;

    use super::{TaskEvent, TaskRecord};
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
