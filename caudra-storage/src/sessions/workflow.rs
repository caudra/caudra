//! Durable workflow runs: one canonical row per run plus the journal of host
//! calls it made. A run belongs to a session and is deleted with it through
//! the schema's cascade; a call belongs to a run the same way.
//!
//! Every run update is conditional on the revision and execution epoch the
//! writer last saw, so an engine that lost a race learns it is stale instead
//! of overwriting newer state. The journal is what makes a run resumable:
//! a committed call replays from its stored result, so committing twice is
//! refused rather than merged.

use std::fmt;
use std::io;
use std::str::FromStr;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::StorageError;
use crate::id::CaudraId;
use crate::sessions::{SessionDatabase, SessionError};

const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_PATH_BYTES: usize = 32 * 1024;
const MAX_RUNS_PER_LOAD: i64 = 64;
const MAX_CALLS_PER_RUN: u64 = 16_384;
const MAX_CALL_LOAD_BYTES: u64 = 64 * 1024 * 1024;
pub const WORKFLOW_CALL_ALREADY_COMMITTED: &str = "workflow call already committed";
pub const WORKFLOW_CALL_REQUEST_MISMATCH: &str =
    "workflow call request differs from the journaled call";
pub const WORKFLOW_CALL_NOT_STARTED: &str = "workflow call is not started";
/// The error a `started` call receives when its run is interrupted.
pub const WORKFLOW_CALL_INTERRUPTED: &str = "interrupted";
/// Runs a trim ends: everything a journal could still resume.
const RESUMABLE_STATUSES: [WorkflowRunStatus; 3] = [
    WorkflowRunStatus::Active,
    WorkflowRunStatus::Paused,
    WorkflowRunStatus::BudgetLimited,
];
/// Per-session workflow row bytes, as a scalar over the `sessions` alias.
pub(crate) const SESSION_WORKFLOW_BYTES: &str = "coalesce((SELECT sum(bytes) FROM workflow_runs \
        WHERE session_id = sessions.id), 0) \
     + coalesce((SELECT sum(calls.bytes) FROM workflow_calls AS calls \
        JOIN workflow_runs AS runs ON runs.run_id = calls.run_id \
        WHERE runs.session_id = sessions.id), 0)";
const RUN_COLUMNS: &str = "run_id, session_id, display_name, workflow_name, source_kind, \
     source_path, source_digest, language_version, abi_version, source, args, objective, \
     launch_mode, status, pause_kind, pause_message, revision, execution_epoch, phase, \
     agent_budget, agents_admitted, usage, roster, result, error, outbox_pending, \
     created_at, updated_at, bytes";
const CALL_COLUMNS: &str = "run_id, call_key, kind, request_hash, request, state, result, \
     error, task_id, started_at, finished_at, tokens_used, duration_ms, bytes";

/// A stored text that names no variant of the enum it should decode to.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {kind} {value:?}")]
pub struct UnknownVariant {
    pub kind: &'static str,
    pub value: String,
}

macro_rules! text_enum {
    ($name:ident as $kind:literal { $($variant:ident = $text:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }

        impl FromStr for $name {
            type Err = UnknownVariant;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($text => Ok(Self::$variant),)+
                    _ => Err(UnknownVariant {
                        kind: $kind,
                        value: value.to_owned(),
                    }),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

text_enum! {
    WorkflowRunStatus as "workflow run status" {
        Active = "active",
        Paused = "paused",
        BudgetLimited = "budget_limited",
        Interrupted = "interrupted",
        Completed = "completed",
        Cancelled = "cancelled",
        Failed = "failed",
    }
}

text_enum! {
    WorkflowSourceKind as "workflow source kind" {
        Builtin = "builtin",
        Project = "project",
        User = "user",
    }
}

text_enum! {
    WorkflowCallKind as "workflow call kind" {
        Agent = "agent",
        Parallel = "parallel",
        ScratchFile = "scratch_file",
    }
}

text_enum! {
    WorkflowCallState as "workflow call state" {
        Started = "started",
        Completed = "completed",
        Failed = "failed",
    }
}

/// One `workflow_runs` row. JSON columns stay opaque strings the caller has
/// already validated. `revision`, `execution_epoch`, the timestamps, and
/// `bytes` are assigned by storage: an insert writes the counters given here
/// and stamps the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunRow {
    pub run_id: String,
    pub session_id: CaudraId,
    pub display_name: String,
    pub workflow_name: String,
    pub source_kind: WorkflowSourceKind,
    pub source_path: Option<String>,
    pub source_digest: String,
    pub language_version: u32,
    pub abi_version: u32,
    pub source: String,
    pub args: String,
    pub objective: Option<String>,
    pub launch_mode: String,
    pub status: WorkflowRunStatus,
    pub pause_kind: Option<String>,
    pub pause_message: Option<String>,
    pub revision: u64,
    pub execution_epoch: u64,
    pub phase: Option<String>,
    pub agent_budget: u64,
    pub agents_admitted: u64,
    pub usage: String,
    pub roster: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub outbox_pending: bool,
    pub created_at: u64,
    pub updated_at: u64,
    pub bytes: u64,
}

/// What one conditional update changes. `None` leaves a column alone; for
/// nullable columns `Some(None)` clears it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowRunPatch {
    pub status: Option<WorkflowRunStatus>,
    pub pause_kind: Option<Option<String>>,
    pub pause_message: Option<Option<String>>,
    pub phase: Option<Option<String>>,
    pub agents_admitted: Option<u64>,
    pub usage: Option<String>,
    pub roster: Option<String>,
    pub result: Option<Option<String>>,
    pub error: Option<Option<String>>,
    pub outbox_pending: Option<bool>,
    pub agent_budget: Option<u64>,
    pub execution_epoch: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowUpdate {
    Applied {
        revision: u64,
    },
    /// The row moved past the revision or epoch the writer expected.
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowCallRow {
    pub run_id: String,
    pub call_key: u64,
    pub kind: WorkflowCallKind,
    pub request_hash: String,
    pub request: String,
    pub state: WorkflowCallState,
    pub result: Option<String>,
    pub error: Option<String>,
    pub task_id: Option<String>,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub tokens_used: u64,
    pub duration_ms: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowCallStart {
    pub run_id: String,
    pub call_key: u64,
    pub kind: WorkflowCallKind,
    pub request_hash: String,
    pub request: String,
    pub task_id: Option<String>,
}

/// How a started call ended. An error makes it `failed`; otherwise it is
/// `completed` with whatever result it carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowCallFinish {
    pub result: Option<String>,
    pub error: Option<String>,
    pub task_id: Option<String>,
    pub tokens_used: u64,
    pub duration_ms: u64,
}

pub(crate) struct WorkflowTotals {
    pub(crate) run_count: u64,
    pub(crate) call_count: u64,
    pub(crate) bytes: u64,
}

pub(crate) struct WorkflowTrim {
    pub(crate) call_rows: u64,
    pub(crate) call_bytes: u64,
}

impl SessionDatabase {
    pub fn insert_workflow_run(&self, row: &WorkflowRunRow) -> Result<(), SessionError> {
        validate_run(row)?;
        self.connection().execute(
            "INSERT INTO workflow_runs (run_id, session_id, display_name, workflow_name, \
                 source_kind, source_path, source_digest, language_version, abi_version, \
                 source, args, objective, launch_mode, status, pause_kind, pause_message, \
                 revision, execution_epoch, phase, agent_budget, agents_admitted, usage, \
                 roster, result, error, outbox_pending, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
                 ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, unixepoch(), unixepoch())",
            params![
                row.run_id,
                row.session_id.as_bytes().as_slice(),
                row.display_name,
                row.workflow_name,
                row.source_kind.as_str(),
                row.source_path,
                row.source_digest,
                row.language_version,
                row.abi_version,
                row.source,
                row.args,
                row.objective,
                row.launch_mode,
                row.status.as_str(),
                row.pause_kind,
                row.pause_message,
                signed(row.revision)?,
                signed(row.execution_epoch)?,
                row.phase,
                signed(row.agent_budget)?,
                signed(row.agents_admitted)?,
                row.usage,
                row.roster,
                row.result,
                row.error,
                row.outbox_pending,
            ],
        )?;
        Ok(())
    }

    /// Applies `patch` only when the row still carries the revision and epoch
    /// the caller last read; the revision then advances by one.
    pub fn update_workflow_run(
        &self,
        run_id: &str,
        expected_revision: u64,
        expected_epoch: u64,
        patch: &WorkflowRunPatch,
    ) -> Result<WorkflowUpdate, SessionError> {
        validate_patch(patch)?;
        let changed = self.connection().execute(
            "UPDATE workflow_runs SET \
                 revision = revision + 1, \
                 updated_at = unixepoch(), \
                 status = coalesce(?4, status), \
                 pause_kind = CASE WHEN ?5 THEN ?6 ELSE pause_kind END, \
                 pause_message = CASE WHEN ?7 THEN ?8 ELSE pause_message END, \
                 phase = CASE WHEN ?9 THEN ?10 ELSE phase END, \
                 agents_admitted = coalesce(?11, agents_admitted), \
                 usage = coalesce(?12, usage), \
                 roster = coalesce(?13, roster), \
                 result = CASE WHEN ?14 THEN ?15 ELSE result END, \
                 error = CASE WHEN ?16 THEN ?17 ELSE error END, \
                 outbox_pending = coalesce(?18, outbox_pending), \
                 agent_budget = coalesce(?19, agent_budget), \
                 execution_epoch = coalesce(?20, execution_epoch) \
             WHERE run_id = ?1 AND revision = ?2 AND execution_epoch = ?3",
            params![
                run_id,
                signed(expected_revision)?,
                signed(expected_epoch)?,
                patch.status.map(WorkflowRunStatus::as_str),
                patch.pause_kind.is_some(),
                patched(&patch.pause_kind),
                patch.pause_message.is_some(),
                patched(&patch.pause_message),
                patch.phase.is_some(),
                patched(&patch.phase),
                patch.agents_admitted.map(signed).transpose()?,
                patch.usage,
                patch.roster,
                patch.result.is_some(),
                patched(&patch.result),
                patch.error.is_some(),
                patched(&patch.error),
                patch.outbox_pending,
                patch.agent_budget.map(signed).transpose()?,
                patch.execution_epoch.map(signed).transpose()?,
            ],
        )?;
        Ok(if changed == 0 {
            WorkflowUpdate::Stale
        } else {
            WorkflowUpdate::Applied {
                revision: expected_revision + 1,
            }
        })
    }

    pub fn load_workflow_run(&self, run_id: &str) -> Result<Option<WorkflowRunRow>, SessionError> {
        self.connection()
            .query_row(
                &format!("SELECT {RUN_COLUMNS} FROM workflow_runs WHERE run_id = ?1"),
                params![run_id],
                |row| Ok(read_run(row)),
            )
            .optional()?
            .transpose()
    }

    /// The newest runs of one session, at most [`MAX_RUNS_PER_LOAD`].
    pub fn load_workflow_runs(
        &self,
        session_id: CaudraId,
    ) -> Result<Vec<WorkflowRunRow>, SessionError> {
        let mut statement = self.connection().prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM workflow_runs WHERE session_id = ?1 \
             ORDER BY created_at DESC, run_id DESC LIMIT ?2"
        ))?;
        let mut rows =
            statement.query(params![session_id.as_bytes().as_slice(), MAX_RUNS_PER_LOAD])?;
        let mut runs = Vec::new();
        while let Some(row) = rows.next()? {
            runs.push(read_run(row)?);
        }
        Ok(runs)
    }

    /// Ends every active run of a session: the run becomes `interrupted` in a
    /// new execution epoch with its outbox pending, and each call it had in
    /// flight fails. Returns how many runs changed.
    pub fn interrupt_active_workflow_runs(
        &self,
        session_id: CaudraId,
    ) -> Result<u64, SessionError> {
        let transaction = self.connection().unchecked_transaction()?;
        transaction.execute(
            "UPDATE workflow_calls SET state = 'failed', error = ?2, finished_at = unixepoch() \
             WHERE state = 'started' AND run_id IN \
                 (SELECT run_id FROM workflow_runs WHERE session_id = ?1 AND status = 'active')",
            params![session_id.as_bytes().as_slice(), WORKFLOW_CALL_INTERRUPTED],
        )?;
        let interrupted = interrupt_runs(&transaction, session_id, &[WorkflowRunStatus::Active])?;
        transaction.commit()?;
        Ok(interrupted)
    }

    /// Journals a call before it runs. A key already committed is refused; a
    /// key left `started` or `failed` by an earlier attempt restarts under the
    /// same request, which must hash the same.
    pub fn start_workflow_call(&self, start: &WorkflowCallStart) -> Result<(), SessionError> {
        bounded_identifier("workflow run id", &start.run_id)?;
        bounded_identifier("workflow call request hash", &start.request_hash)?;
        bounded_text(
            "workflow call task id",
            start.task_id.as_deref(),
            MAX_IDENTIFIER_BYTES,
        )?;
        SessionDatabase::validate_payload_json("workflow call request", &start.request)?;
        if start.call_key >= MAX_CALLS_PER_RUN {
            return Err(SessionError::LimitExceeded {
                kind: "workflow call key",
                actual: usize::try_from(start.call_key).unwrap_or(usize::MAX),
                maximum: usize::try_from(MAX_CALLS_PER_RUN).unwrap_or(usize::MAX),
            });
        }
        let call_key = signed(start.call_key)?;
        let transaction = self.connection().unchecked_transaction()?;
        let existing = transaction
            .query_row(
                "SELECT state, request_hash FROM workflow_calls \
                 WHERE run_id = ?1 AND call_key = ?2",
                params![start.run_id, call_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        match existing {
            None => {
                transaction.execute(
                    "INSERT INTO workflow_calls (run_id, call_key, kind, request_hash, request, \
                         state, task_id, started_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 'started', ?6, unixepoch())",
                    params![
                        start.run_id,
                        call_key,
                        start.kind.as_str(),
                        start.request_hash,
                        start.request,
                        start.task_id,
                    ],
                )?;
            }
            Some((state, _)) if state == WorkflowCallState::Completed.as_str() => {
                return Err(refused(
                    io::ErrorKind::AlreadyExists,
                    WORKFLOW_CALL_ALREADY_COMMITTED,
                ));
            }
            Some((_, request_hash)) if request_hash != start.request_hash => {
                return Err(refused(
                    io::ErrorKind::InvalidInput,
                    WORKFLOW_CALL_REQUEST_MISMATCH,
                ));
            }
            Some(_) => {
                transaction.execute(
                    "UPDATE workflow_calls SET state = 'started', kind = ?3, request = ?4, \
                         task_id = ?5, result = NULL, error = NULL, started_at = unixepoch(), \
                         finished_at = NULL, tokens_used = 0, duration_ms = 0 \
                     WHERE run_id = ?1 AND call_key = ?2",
                    params![
                        start.run_id,
                        call_key,
                        start.kind.as_str(),
                        start.request,
                        start.task_id,
                    ],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn finish_workflow_call(
        &self,
        run_id: &str,
        call_key: u64,
        finish: &WorkflowCallFinish,
    ) -> Result<(), SessionError> {
        bounded_json("workflow call result", finish.result.as_deref())?;
        bounded_text(
            "workflow call error",
            finish.error.as_deref(),
            MAX_TEXT_BYTES,
        )?;
        bounded_text(
            "workflow call task id",
            finish.task_id.as_deref(),
            MAX_IDENTIFIER_BYTES,
        )?;
        let state = if finish.error.is_some() {
            WorkflowCallState::Failed
        } else {
            WorkflowCallState::Completed
        };
        let call_key = signed(call_key)?;
        let changed = self.connection().execute(
            "UPDATE workflow_calls SET state = ?3, result = ?4, error = ?5, \
                 task_id = coalesce(?6, task_id), finished_at = unixepoch(), \
                 tokens_used = ?7, duration_ms = ?8 \
             WHERE run_id = ?1 AND call_key = ?2 AND state = 'started'",
            params![
                run_id,
                call_key,
                state.as_str(),
                finish.result,
                finish.error,
                finish.task_id,
                signed(finish.tokens_used)?,
                signed(finish.duration_ms)?,
            ],
        )?;
        if changed == 1 {
            return Ok(());
        }
        let exists = self
            .connection()
            .query_row(
                "SELECT 1 FROM workflow_calls WHERE run_id = ?1 AND call_key = ?2",
                params![run_id, call_key],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Err(if exists {
            refused(io::ErrorKind::InvalidInput, WORKFLOW_CALL_NOT_STARTED)
        } else {
            StorageError::NotFound(format!("workflow call {run_id}#{call_key}")).into()
        })
    }

    /// The journal of one run in call order. A journal past the row or byte
    /// bound is refused whole rather than truncated, because a partial journal
    /// replays as a different run.
    pub fn load_workflow_calls(&self, run_id: &str) -> Result<Vec<WorkflowCallRow>, SessionError> {
        let transaction = self.connection().unchecked_transaction()?;
        let (count, bytes) = transaction.query_row(
            "SELECT count(*), coalesce(sum(bytes), 0) FROM workflow_calls WHERE run_id = ?1",
            params![run_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        bounded_count(
            "workflow call rows",
            unsigned(count, "workflow_calls count")?,
            MAX_CALLS_PER_RUN,
        )?;
        bounded_count(
            "workflow call bytes",
            unsigned(bytes, "workflow_calls.bytes")?,
            MAX_CALL_LOAD_BYTES,
        )?;
        let mut statement = transaction.prepare(&format!(
            "SELECT {CALL_COLUMNS} FROM workflow_calls WHERE run_id = ?1 ORDER BY call_key"
        ))?;
        let mut rows = statement.query(params![run_id])?;
        let mut calls = Vec::new();
        while let Some(row) = rows.next()? {
            calls.push(read_call(row)?);
        }
        drop(rows);
        drop(statement);
        transaction.commit()?;
        Ok(calls)
    }

    /// Runs whose latest state has not been delivered yet, as `(run_id,
    /// revision)` pairs oldest change first.
    pub fn pending_workflow_outbox(
        &self,
        session_id: CaudraId,
    ) -> Result<Vec<(String, u64)>, SessionError> {
        let mut statement = self.connection().prepare(
            "SELECT run_id, revision FROM workflow_runs \
             WHERE session_id = ?1 AND outbox_pending = 1 ORDER BY updated_at, run_id",
        )?;
        let mut rows = statement.query(params![session_id.as_bytes().as_slice()])?;
        let mut pending = Vec::new();
        while let Some(row) = rows.next()? {
            pending.push((
                row.get(0)?,
                unsigned(row.get(1)?, "workflow_runs.revision")?,
            ));
        }
        Ok(pending)
    }

    /// Marks one delivery done. Returns `false` when the run has moved on
    /// since, in which case the newer state still awaits delivery.
    pub fn ack_workflow_outbox(&self, run_id: &str, revision: u64) -> Result<bool, SessionError> {
        let changed = self.connection().execute(
            "UPDATE workflow_runs SET outbox_pending = 0 \
             WHERE run_id = ?1 AND revision = ?2 AND outbox_pending = 1",
            params![run_id, signed(revision)?],
        )?;
        Ok(changed == 1)
    }

    /// Bytes every workflow row of a session accounts for.
    pub fn workflow_bytes(&self, session_id: CaudraId) -> Result<u64, SessionError> {
        let bytes: i64 = self.connection().query_row(
            &format!("SELECT {SESSION_WORKFLOW_BYTES} FROM (SELECT ?1 AS id) AS sessions"),
            params![session_id.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        unsigned(bytes, "workflow bytes")
    }
}

pub(crate) fn workflow_totals(connection: &Connection) -> Result<WorkflowTotals, SessionError> {
    let (run_count, run_bytes, call_count, call_bytes) = connection.query_row(
        "SELECT (SELECT count(*) FROM workflow_runs), \
                (SELECT coalesce(sum(bytes), 0) FROM workflow_runs), \
                (SELECT count(*) FROM workflow_calls), \
                (SELECT coalesce(sum(bytes), 0) FROM workflow_calls)",
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        },
    )?;
    Ok(WorkflowTotals {
        run_count: unsigned(run_count, "workflow run count")?,
        call_count: unsigned(call_count, "workflow call count")?,
        bytes: unsigned(run_bytes, "workflow run bytes")?
            + unsigned(call_bytes, "workflow call bytes")?,
    })
}

/// What a trim does to a session's workflows: the journal goes, and every run
/// the journal could still have resumed becomes `interrupted`.
pub(crate) fn trim_workflow_runs(
    connection: &Connection,
    session_id: CaudraId,
) -> Result<WorkflowTrim, SessionError> {
    let (rows, bytes) = connection.query_row(
        "SELECT count(*), coalesce(sum(bytes), 0) FROM workflow_calls WHERE run_id IN \
             (SELECT run_id FROM workflow_runs WHERE session_id = ?1)",
        params![session_id.as_bytes().as_slice()],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    )?;
    connection.execute(
        "DELETE FROM workflow_calls WHERE run_id IN \
             (SELECT run_id FROM workflow_runs WHERE session_id = ?1)",
        params![session_id.as_bytes().as_slice()],
    )?;
    interrupt_runs(connection, session_id, &RESUMABLE_STATUSES)?;
    Ok(WorkflowTrim {
        call_rows: unsigned(rows, "trimmed workflow call rows")?,
        call_bytes: unsigned(bytes, "trimmed workflow call bytes")?,
    })
}

fn interrupt_runs(
    connection: &Connection,
    session_id: CaudraId,
    statuses: &[WorkflowRunStatus],
) -> Result<u64, SessionError> {
    let statuses = serde_json::to_string(
        &statuses
            .iter()
            .map(|status| status.as_str())
            .collect::<Vec<_>>(),
    )
    .map_err(StorageError::from)?;
    let changed = connection.execute(
        "UPDATE workflow_runs SET status = ?3, revision = revision + 1, \
             execution_epoch = execution_epoch + 1, outbox_pending = 1, updated_at = unixepoch() \
         WHERE session_id = ?1 AND status IN (SELECT value FROM json_each(?2))",
        params![
            session_id.as_bytes().as_slice(),
            statuses,
            WorkflowRunStatus::Interrupted.as_str()
        ],
    )?;
    Ok(changed as u64)
}

fn validate_run(row: &WorkflowRunRow) -> Result<(), SessionError> {
    bounded_identifier("workflow run id", &row.run_id)?;
    bounded_identifier("workflow display name", &row.display_name)?;
    bounded_identifier("workflow name", &row.workflow_name)?;
    bounded_text(
        "workflow source path",
        row.source_path.as_deref(),
        MAX_PATH_BYTES,
    )?;
    bounded_identifier("workflow source digest", &row.source_digest)?;
    SessionDatabase::validate_len("workflow source", row.source.len(), MAX_SOURCE_BYTES)?;
    SessionDatabase::validate_payload_json("workflow args", &row.args)?;
    bounded_text(
        "workflow objective",
        row.objective.as_deref(),
        MAX_TEXT_BYTES,
    )?;
    bounded_identifier("workflow launch mode", &row.launch_mode)?;
    bounded_text(
        "workflow pause kind",
        row.pause_kind.as_deref(),
        MAX_IDENTIFIER_BYTES,
    )?;
    bounded_text(
        "workflow pause message",
        row.pause_message.as_deref(),
        MAX_TEXT_BYTES,
    )?;
    bounded_text("workflow phase", row.phase.as_deref(), MAX_IDENTIFIER_BYTES)?;
    SessionDatabase::validate_payload_json("workflow usage", &row.usage)?;
    SessionDatabase::validate_payload_json("workflow roster", &row.roster)?;
    bounded_json("workflow result", row.result.as_deref())?;
    bounded_text("workflow error", row.error.as_deref(), MAX_TEXT_BYTES)
}

fn validate_patch(patch: &WorkflowRunPatch) -> Result<(), SessionError> {
    bounded_text(
        "workflow pause kind",
        patched(&patch.pause_kind),
        MAX_IDENTIFIER_BYTES,
    )?;
    bounded_text(
        "workflow pause message",
        patched(&patch.pause_message),
        MAX_TEXT_BYTES,
    )?;
    bounded_text(
        "workflow phase",
        patched(&patch.phase),
        MAX_IDENTIFIER_BYTES,
    )?;
    bounded_json("workflow usage", patch.usage.as_deref())?;
    bounded_json("workflow roster", patch.roster.as_deref())?;
    bounded_json("workflow result", patched(&patch.result))?;
    bounded_text("workflow error", patched(&patch.error), MAX_TEXT_BYTES)
}

fn bounded_identifier(kind: &'static str, value: &str) -> Result<(), SessionError> {
    SessionDatabase::validate_len(kind, value.len(), MAX_IDENTIFIER_BYTES)
}

fn bounded_text(
    kind: &'static str,
    value: Option<&str>,
    maximum: usize,
) -> Result<(), SessionError> {
    value.map_or(Ok(()), |value| {
        SessionDatabase::validate_len(kind, value.len(), maximum)
    })
}

fn bounded_json(kind: &'static str, value: Option<&str>) -> Result<(), SessionError> {
    value.map_or(Ok(()), |value| {
        SessionDatabase::validate_payload_json(kind, value)
    })
}

fn bounded_count(kind: &'static str, actual: u64, maximum: u64) -> Result<(), SessionError> {
    if actual > maximum {
        return Err(SessionError::LimitExceeded {
            kind,
            actual: usize::try_from(actual).unwrap_or(usize::MAX),
            maximum: usize::try_from(maximum).unwrap_or(usize::MAX),
        });
    }
    Ok(())
}

/// The value a nullable-column patch carries: `None` for "leave alone" and
/// for "clear" alike, which the accompanying `is_some()` flag tells apart.
fn patched(value: &Option<Option<String>>) -> Option<&str> {
    value.as_ref().and_then(|value| value.as_deref())
}

fn refused(kind: io::ErrorKind, message: &'static str) -> SessionError {
    StorageError::Io(io::Error::new(kind, message)).into()
}

fn read_run(row: &Row<'_>) -> Result<WorkflowRunRow, SessionError> {
    Ok(WorkflowRunRow {
        run_id: row.get(0)?,
        session_id: session_id_from_row(row, 1)?,
        display_name: row.get(2)?,
        workflow_name: row.get(3)?,
        source_kind: parse_column(row, 4, "workflow_runs.source_kind")?,
        source_path: row.get(5)?,
        source_digest: row.get(6)?,
        language_version: row.get(7)?,
        abi_version: row.get(8)?,
        source: row.get(9)?,
        args: row.get(10)?,
        objective: row.get(11)?,
        launch_mode: row.get(12)?,
        status: parse_column(row, 13, "workflow_runs.status")?,
        pause_kind: row.get(14)?,
        pause_message: row.get(15)?,
        revision: unsigned(row.get(16)?, "workflow_runs.revision")?,
        execution_epoch: unsigned(row.get(17)?, "workflow_runs.execution_epoch")?,
        phase: row.get(18)?,
        agent_budget: unsigned(row.get(19)?, "workflow_runs.agent_budget")?,
        agents_admitted: unsigned(row.get(20)?, "workflow_runs.agents_admitted")?,
        usage: row.get(21)?,
        roster: row.get(22)?,
        result: row.get(23)?,
        error: row.get(24)?,
        outbox_pending: row.get(25)?,
        created_at: unsigned(row.get(26)?, "workflow_runs.created_at")?,
        updated_at: unsigned(row.get(27)?, "workflow_runs.updated_at")?,
        bytes: unsigned(row.get(28)?, "workflow_runs.bytes")?,
    })
}

fn read_call(row: &Row<'_>) -> Result<WorkflowCallRow, SessionError> {
    Ok(WorkflowCallRow {
        run_id: row.get(0)?,
        call_key: unsigned(row.get(1)?, "workflow_calls.call_key")?,
        kind: parse_column(row, 2, "workflow_calls.kind")?,
        request_hash: row.get(3)?,
        request: row.get(4)?,
        state: parse_column(row, 5, "workflow_calls.state")?,
        result: row.get(6)?,
        error: row.get(7)?,
        task_id: row.get(8)?,
        started_at: unsigned(row.get(9)?, "workflow_calls.started_at")?,
        finished_at: row
            .get::<_, Option<i64>>(10)?
            .map(|value| unsigned(value, "workflow_calls.finished_at"))
            .transpose()?,
        tokens_used: unsigned(row.get(11)?, "workflow_calls.tokens_used")?,
        duration_ms: unsigned(row.get(12)?, "workflow_calls.duration_ms")?,
        bytes: unsigned(row.get(13)?, "workflow_calls.bytes")?,
    })
}

fn parse_column<T>(row: &Row<'_>, index: usize, field: &'static str) -> Result<T, SessionError>
where
    T: FromStr<Err = UnknownVariant>,
{
    row.get::<_, String>(index)?
        .parse()
        .map_err(|error: UnknownVariant| SessionError::CorruptDatabaseValue {
            field,
            reason: error.to_string(),
        })
}

fn session_id_from_row(row: &Row<'_>, index: usize) -> Result<CaudraId, SessionError> {
    let bytes: Vec<u8> = row.get(index)?;
    let bytes: [u8; 16] =
        bytes
            .as_slice()
            .try_into()
            .map_err(|_| SessionError::CorruptDatabaseValue {
                field: "workflow_runs.session_id",
                reason: format!("expected 16 bytes, found {}", bytes.len()),
            })?;
    Ok(CaudraId::from_bytes(bytes))
}

fn signed(value: u64) -> Result<i64, SessionError> {
    i64::try_from(value).map_err(|_| SessionError::CorruptDatabaseValue {
        field: "workflow counter",
        reason: "value exceeds SQLite integer range".into(),
    })
}

fn unsigned(value: i64, field: &'static str) -> Result<u64, SessionError> {
    u64::try_from(value).map_err(|_| SessionError::CorruptDatabaseValue {
        field,
        reason: value.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::StateDir;
    use crate::sessions::{Session, SessionLease, TitleSource};

    const CWD: &str = "/project";
    const MODEL: &str = "test/model";
    const RUN_ID: &str = "run-1";
    const OTHER_RUN_ID: &str = "run-2";
    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const REQUEST: &str = r#"{"prompt":"do it"}"#;
    const REQUEST_HASH: &str = "hash-1";
    const OTHER_REQUEST_HASH: &str = "hash-2";
    const RESULT: &str = r#"{"text":"done"}"#;
    const FAILURE: &str = "boom";
    const TASK_ID: &str = "task-1";
    const STALE_LOSES: &str = "a stale writer must not overwrite newer state";
    const OUTBOX_ACK_IS_EXACT: &str = "an ack must only clear the revision it delivered";
    const REPLAY_IS_EXACT: &str = "a committed call must never be started again";
    const RETRY_KEEPS_REQUEST: &str = "a retried call must repeat the journaled request";
    const INTERRUPT_FAILS_CALLS: &str = "an interrupted run must fail its in-flight calls";
    const CASCADE: &str = "workflow rows must go with their session";
    const FORK_IS_SEPARATE: &str = "a forked session must not inherit workflow runs";
    const BYTES_ARE_ACCOUNTED: &str = "every stored workflow byte must be counted";

    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct TestMessage(String);

    impl TitleSource for TestMessage {
        fn first_user_text(&self) -> Option<&str> {
            Some(&self.0)
        }
    }

    type TestSession = Session<TestMessage, Value, Value>;

    fn open() -> (TempDir, StateDir, SessionDatabase, CaudraId) {
        let temp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        (temp, state_dir, database, session.id)
    }

    fn run(session_id: CaudraId, run_id: &str) -> WorkflowRunRow {
        WorkflowRunRow {
            run_id: run_id.into(),
            session_id,
            display_name: "Review".into(),
            workflow_name: "review".into(),
            source_kind: WorkflowSourceKind::Project,
            source_path: Some(".caudra/workflows/review.rhai".into()),
            source_digest: DIGEST.into(),
            language_version: 1,
            abi_version: 1,
            source: "let meta = #{};".into(),
            args: r#"{"branch":"main"}"#.into(),
            objective: Some("review the branch".into()),
            launch_mode: "interactive".into(),
            status: WorkflowRunStatus::Active,
            pause_kind: None,
            pause_message: None,
            revision: 0,
            execution_epoch: 0,
            phase: Some("start".into()),
            agent_budget: 4,
            agents_admitted: 0,
            usage: "{}".into(),
            roster: "[]".into(),
            result: None,
            error: None,
            outbox_pending: false,
            created_at: 0,
            updated_at: 0,
            bytes: 0,
        }
    }

    fn start(run_id: &str, call_key: u64) -> WorkflowCallStart {
        WorkflowCallStart {
            run_id: run_id.into(),
            call_key,
            kind: WorkflowCallKind::Agent,
            request_hash: REQUEST_HASH.into(),
            request: REQUEST.into(),
            task_id: None,
        }
    }

    fn completion() -> WorkflowCallFinish {
        WorkflowCallFinish {
            result: Some(RESULT.into()),
            error: None,
            task_id: Some(TASK_ID.into()),
            tokens_used: 12,
            duration_ms: 34,
        }
    }

    fn io_message(error: &SessionError) -> Option<String> {
        match error {
            SessionError::Storage(StorageError::Io(error)) => Some(error.to_string()),
            _ => None,
        }
    }

    #[test_case(WorkflowRunStatus::BudgetLimited, "budget_limited"; "run_status")]
    #[test_case(WorkflowCallKind::ScratchFile, "scratch_file"; "call_kind")]
    fn enums_round_trip_through_their_storage_text<T>(value: T, text: &str)
    where
        T: FromStr<Err = UnknownVariant> + PartialEq + fmt::Debug + Copy + fmt::Display,
    {
        assert_eq!(value.to_string(), text);
        assert_eq!(text.parse::<T>().unwrap(), value);
        assert!(matches!(
            "nope".parse::<T>(),
            Err(UnknownVariant { value, .. }) if value == "nope"
        ));
    }

    #[test]
    fn run_round_trips_and_is_stamped_by_storage() {
        let (_temp, _state_dir, database, session_id) = open();
        let row = run(session_id, RUN_ID);

        database.insert_workflow_run(&row).unwrap();
        let loaded = database.load_workflow_run(RUN_ID).unwrap().unwrap();

        assert!(loaded.created_at > 0);
        assert_eq!(loaded.updated_at, loaded.created_at);
        assert!(loaded.bytes > 0, "{BYTES_ARE_ACCOUNTED}");
        assert_eq!(
            WorkflowRunRow {
                created_at: 0,
                updated_at: 0,
                bytes: 0,
                ..loaded
            },
            row
        );
        assert!(database.load_workflow_run(OTHER_RUN_ID).unwrap().is_none());
    }

    #[test]
    fn runs_load_newest_first_for_their_session_only() {
        let (_temp, _state_dir, mut database, session_id) = open();
        let other = TestSession::new(MODEL, CWD);
        database.save(&other, None).unwrap();
        database
            .insert_workflow_run(&run(session_id, RUN_ID))
            .unwrap();
        database
            .insert_workflow_run(&run(session_id, OTHER_RUN_ID))
            .unwrap();
        database
            .insert_workflow_run(&run(other.id, "elsewhere"))
            .unwrap();

        let runs = database.load_workflow_runs(session_id).unwrap();

        let ids: Vec<&str> = runs.iter().map(|run| run.run_id.as_str()).collect();
        assert_eq!(ids, [OTHER_RUN_ID, RUN_ID]);
    }

    #[test]
    fn insert_refuses_an_oversized_source() {
        let (_temp, _state_dir, database, session_id) = open();
        let mut row = run(session_id, RUN_ID);
        row.source = "x".repeat(MAX_SOURCE_BYTES + 1);

        let error = database.insert_workflow_run(&row).unwrap_err();

        assert!(matches!(
            error,
            SessionError::LimitExceeded {
                kind: "workflow source",
                ..
            }
        ));
        assert!(database.load_workflow_run(RUN_ID).unwrap().is_none());
    }

    #[test]
    fn update_is_conditional_on_revision_and_epoch() {
        let (_temp, _state_dir, database, session_id) = open();
        database
            .insert_workflow_run(&run(session_id, RUN_ID))
            .unwrap();
        let patch = WorkflowRunPatch {
            status: Some(WorkflowRunStatus::Paused),
            pause_kind: Some(Some("verification".into())),
            phase: Some(None),
            agents_admitted: Some(2),
            outbox_pending: Some(true),
            ..WorkflowRunPatch::default()
        };

        let applied = database.update_workflow_run(RUN_ID, 0, 0, &patch).unwrap();
        let stale = database.update_workflow_run(RUN_ID, 0, 0, &patch).unwrap();
        let wrong_epoch = database.update_workflow_run(RUN_ID, 1, 1, &patch).unwrap();

        assert_eq!(applied, WorkflowUpdate::Applied { revision: 1 });
        assert_eq!(stale, WorkflowUpdate::Stale, "{STALE_LOSES}");
        assert_eq!(wrong_epoch, WorkflowUpdate::Stale, "{STALE_LOSES}");
        let loaded = database.load_workflow_run(RUN_ID).unwrap().unwrap();
        assert_eq!(loaded.revision, 1);
        assert_eq!(loaded.status, WorkflowRunStatus::Paused);
        assert_eq!(loaded.pause_kind.as_deref(), Some("verification"));
        assert_eq!(loaded.phase, None);
        assert_eq!(loaded.agents_admitted, 2);
        assert!(loaded.outbox_pending);
        assert_eq!(loaded.agent_budget, 4);
    }

    #[test]
    fn outbox_delivers_once_per_revision() {
        let (_temp, _state_dir, database, session_id) = open();
        database
            .insert_workflow_run(&run(session_id, RUN_ID))
            .unwrap();
        let patch = WorkflowRunPatch {
            status: Some(WorkflowRunStatus::Completed),
            outbox_pending: Some(true),
            ..WorkflowRunPatch::default()
        };
        database.update_workflow_run(RUN_ID, 0, 0, &patch).unwrap();

        let pending = database.pending_workflow_outbox(session_id).unwrap();

        assert_eq!(pending, vec![(RUN_ID.to_owned(), 1)]);
        assert!(
            !database.ack_workflow_outbox(RUN_ID, 0).unwrap(),
            "{OUTBOX_ACK_IS_EXACT}"
        );
        assert!(database.ack_workflow_outbox(RUN_ID, 1).unwrap());
        assert!(!database.ack_workflow_outbox(RUN_ID, 1).unwrap());
        assert!(
            database
                .pending_workflow_outbox(session_id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn calls_journal_start_finish_and_load_in_order() {
        let (_temp, _state_dir, database, session_id) = open();
        database
            .insert_workflow_run(&run(session_id, RUN_ID))
            .unwrap();
        database.start_workflow_call(&start(RUN_ID, 1)).unwrap();
        database.start_workflow_call(&start(RUN_ID, 0)).unwrap();

        database
            .finish_workflow_call(RUN_ID, 0, &completion())
            .unwrap();
        database
            .finish_workflow_call(
                RUN_ID,
                1,
                &WorkflowCallFinish {
                    error: Some(FAILURE.into()),
                    ..WorkflowCallFinish::default()
                },
            )
            .unwrap();
        let calls = database.load_workflow_calls(RUN_ID).unwrap();

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].call_key, 0);
        assert_eq!(calls[0].state, WorkflowCallState::Completed);
        assert_eq!(calls[0].result.as_deref(), Some(RESULT));
        assert_eq!(calls[0].task_id.as_deref(), Some(TASK_ID));
        assert_eq!(calls[0].tokens_used, 12);
        assert_eq!(calls[0].duration_ms, 34);
        assert!(calls[0].finished_at.is_some());
        assert_eq!(calls[1].state, WorkflowCallState::Failed);
        assert_eq!(calls[1].error.as_deref(), Some(FAILURE));
        assert!(
            calls.iter().all(|call| call.bytes > 0),
            "{BYTES_ARE_ACCOUNTED}"
        );
    }

    #[test]
    fn a_committed_call_cannot_be_started_again() {
        let (_temp, _state_dir, database, session_id) = open();
        database
            .insert_workflow_run(&run(session_id, RUN_ID))
            .unwrap();
        database.start_workflow_call(&start(RUN_ID, 0)).unwrap();
        database
            .finish_workflow_call(RUN_ID, 0, &completion())
            .unwrap();

        let error = database.start_workflow_call(&start(RUN_ID, 0)).unwrap_err();

        assert_eq!(
            io_message(&error).as_deref(),
            Some(WORKFLOW_CALL_ALREADY_COMMITTED),
            "{REPLAY_IS_EXACT}"
        );
        let finish_again = database
            .finish_workflow_call(RUN_ID, 0, &completion())
            .unwrap_err();
        assert_eq!(
            io_message(&finish_again).as_deref(),
            Some(WORKFLOW_CALL_NOT_STARTED)
        );
    }

    #[test_case(WorkflowCallState::Started; "crashed_while_started")]
    #[test_case(WorkflowCallState::Failed; "failed")]
    fn an_uncommitted_call_restarts_under_the_same_request(state: WorkflowCallState) {
        let (_temp, _state_dir, database, session_id) = open();
        database
            .insert_workflow_run(&run(session_id, RUN_ID))
            .unwrap();
        database.start_workflow_call(&start(RUN_ID, 0)).unwrap();
        if state == WorkflowCallState::Failed {
            database
                .finish_workflow_call(
                    RUN_ID,
                    0,
                    &WorkflowCallFinish {
                        error: Some(FAILURE.into()),
                        ..WorkflowCallFinish::default()
                    },
                )
                .unwrap();
        }
        let mut diverged = start(RUN_ID, 0);
        diverged.request_hash = OTHER_REQUEST_HASH.into();

        let mismatch = database.start_workflow_call(&diverged).unwrap_err();
        database.start_workflow_call(&start(RUN_ID, 0)).unwrap();

        assert_eq!(
            io_message(&mismatch).as_deref(),
            Some(WORKFLOW_CALL_REQUEST_MISMATCH),
            "{RETRY_KEEPS_REQUEST}"
        );
        let calls = database.load_workflow_calls(RUN_ID).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].state, WorkflowCallState::Started);
        assert_eq!(calls[0].error, None);
        assert_eq!(calls[0].finished_at, None);
    }

    #[test]
    fn finishing_an_unknown_call_is_not_found() {
        let (_temp, _state_dir, database, session_id) = open();
        database
            .insert_workflow_run(&run(session_id, RUN_ID))
            .unwrap();

        let error = database
            .finish_workflow_call(RUN_ID, 7, &completion())
            .unwrap_err();

        assert!(matches!(
            error,
            SessionError::Storage(StorageError::NotFound(_))
        ));
    }

    #[test]
    fn interrupting_a_session_ends_active_runs_and_their_calls() {
        let (_temp, _state_dir, database, session_id) = open();
        database
            .insert_workflow_run(&run(session_id, RUN_ID))
            .unwrap();
        let mut paused = run(session_id, OTHER_RUN_ID);
        paused.status = WorkflowRunStatus::Paused;
        database.insert_workflow_run(&paused).unwrap();
        database.start_workflow_call(&start(RUN_ID, 0)).unwrap();
        database
            .start_workflow_call(&start(OTHER_RUN_ID, 0))
            .unwrap();

        let interrupted = database.interrupt_active_workflow_runs(session_id).unwrap();

        assert_eq!(interrupted, 1);
        let active = database.load_workflow_run(RUN_ID).unwrap().unwrap();
        assert_eq!(active.status, WorkflowRunStatus::Interrupted);
        assert_eq!(active.revision, 1);
        assert_eq!(active.execution_epoch, 1);
        assert!(active.outbox_pending);
        let call = &database.load_workflow_calls(RUN_ID).unwrap()[0];
        assert_eq!(
            call.state,
            WorkflowCallState::Failed,
            "{INTERRUPT_FAILS_CALLS}"
        );
        assert_eq!(
            call.error.as_deref(),
            Some(WORKFLOW_CALL_INTERRUPTED),
            "{INTERRUPT_FAILS_CALLS}"
        );
        let untouched = database.load_workflow_run(OTHER_RUN_ID).unwrap().unwrap();
        assert_eq!(untouched.status, WorkflowRunStatus::Paused);
        assert_eq!(untouched.revision, 0);
        assert_eq!(
            database.load_workflow_calls(OTHER_RUN_ID).unwrap()[0].state,
            WorkflowCallState::Started
        );
    }

    #[test]
    fn bytes_and_totals_count_runs_and_calls() {
        let (_temp, _state_dir, database, session_id) = open();
        database
            .insert_workflow_run(&run(session_id, RUN_ID))
            .unwrap();
        database.start_workflow_call(&start(RUN_ID, 0)).unwrap();
        let run_bytes = database.load_workflow_run(RUN_ID).unwrap().unwrap().bytes;
        let call_bytes = database.load_workflow_calls(RUN_ID).unwrap()[0].bytes;

        let bytes = database.workflow_bytes(session_id).unwrap();
        let stats = database.stats().unwrap();
        let facts = database.session_facts(None).unwrap();

        assert_eq!(bytes, run_bytes + call_bytes, "{BYTES_ARE_ACCOUNTED}");
        assert_eq!(stats.workflow_run_count, 1);
        assert_eq!(stats.workflow_call_count, 1);
        assert_eq!(stats.workflow_bytes, bytes, "{BYTES_ARE_ACCOUNTED}");
        assert_eq!(
            facts[0].logical_bytes,
            stats.logical_bytes + bytes,
            "{BYTES_ARE_ACCOUNTED}"
        );
        assert_eq!(database.workflow_bytes(CaudraId::generate()).unwrap(), 0);
    }

    #[test]
    fn deleting_a_session_removes_its_workflow_rows() {
        let (_temp, _state_dir, mut database, session_id) = open();
        database
            .insert_workflow_run(&run(session_id, RUN_ID))
            .unwrap();
        database.start_workflow_call(&start(RUN_ID, 0)).unwrap();

        database.delete(session_id, None).unwrap();

        assert!(
            database.load_workflow_run(RUN_ID).unwrap().is_none(),
            "{CASCADE}"
        );
        let stats = database.stats().unwrap();
        assert_eq!(stats.workflow_run_count, 0, "{CASCADE}");
        assert_eq!(stats.workflow_call_count, 0, "{CASCADE}");
    }

    #[test]
    fn a_rewritten_or_forked_session_keeps_workflow_rows_apart() {
        let (_temp, _state_dir, mut database, session_id) = open();
        database
            .insert_workflow_run(&run(session_id, RUN_ID))
            .unwrap();
        let mut session = database
            .load::<TestMessage, Value, Value>(session_id)
            .unwrap();
        session.push_message(TestMessage("later".into()));
        database.save(&session, None).unwrap();
        let mut fork = TestSession::new(MODEL, CWD);
        fork.push_message(TestMessage("forked".into()));
        database.save(&fork, None).unwrap();

        assert_eq!(database.load_workflow_runs(session_id).unwrap().len(), 1);
        assert!(
            database.load_workflow_runs(fork.id).unwrap().is_empty(),
            "{FORK_IS_SEPARATE}"
        );
    }

    #[test]
    fn trim_drops_the_journal_and_interrupts_resumable_runs() {
        let (_temp, state_dir, mut database, session_id) = open();
        database
            .insert_workflow_run(&run(session_id, RUN_ID))
            .unwrap();
        let mut completed = run(session_id, OTHER_RUN_ID);
        completed.status = WorkflowRunStatus::Completed;
        database.insert_workflow_run(&completed).unwrap();
        database.start_workflow_call(&start(RUN_ID, 0)).unwrap();
        database
            .start_workflow_call(&start(OTHER_RUN_ID, 0))
            .unwrap();
        let lease = SessionLease::acquire(&state_dir, session_id).unwrap();

        let report = database.trim(&lease).unwrap();

        assert_eq!(report.workflow_call_rows, 2);
        assert!(report.workflow_call_bytes > 0, "{BYTES_ARE_ACCOUNTED}");
        assert!(database.load_workflow_calls(RUN_ID).unwrap().is_empty());
        let interrupted = database.load_workflow_run(RUN_ID).unwrap().unwrap();
        assert_eq!(interrupted.status, WorkflowRunStatus::Interrupted);
        assert_eq!(interrupted.execution_epoch, 1);
        let settled = database.load_workflow_run(OTHER_RUN_ID).unwrap().unwrap();
        assert_eq!(settled.status, WorkflowRunStatus::Completed);
        assert_eq!(settled.revision, 0);
    }
}
