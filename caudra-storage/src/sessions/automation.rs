//! Durable automations: the binding a session keeps for every automation it
//! armed, the script versions its firings ran, and a trace of each firing with
//! the actions it took. Bindings, sources and firings belong to a session and
//! go with it through the schema's cascade; actions belong to a firing the
//! same way.
//!
//! State commits only at the revision its writer loaded, so a firing that
//! raced a human edit learns it lost instead of writing old state back, and a
//! firing commits in the same transaction as its final status. The trace is
//! bounded: an oversized body keeps a valid JSON preview flagged as cut, and
//! each automation keeps its newest finished firings while pending ones stay.

use std::borrow::Cow;
use std::fmt;
use std::io;
use std::str::FromStr;

use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::StorageError;
use crate::id::CaudraId;
use crate::sessions::{SessionDatabase, SessionError, from_i64, id_from_row, to_i64};
use crate::workflow::{UnknownVariant, parse_column, text_enum};

/// What one firing stores in full across its event, action bodies and state
/// patch. Past it, each further body keeps a [`PREVIEW_BYTES`] preview.
pub const MAX_FIRING_BYTES: usize = 256 * 1024;
pub const MAX_EVENT_BYTES: usize = 64 * 1024;
pub const MAX_REQUEST_BYTES: usize = 16 * 1024;
pub const MAX_RESULT_BYTES: usize = 64 * 1024;
pub const PREVIEW_BYTES: usize = 4 * 1024;
pub const MAX_STATE_BYTES: usize = 64 * 1024;
pub const MAX_ARGS_BYTES: usize = 16 * 1024;
pub const MAX_SOURCE_BYTES: usize = 64 * 1024;
const MAX_MARKS_BYTES: usize = 64 * 1024;
const MAX_TEXT_BYTES: usize = 4 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 256;
/// Finished firings each automation keeps per session. Pending ones stay.
pub const MAX_FINISHED_FIRINGS: usize = 100;
/// The most firings one listing answers with.
pub const MAX_FIRINGS_PER_LOAD: usize = 256;
const MAX_BINDINGS_PER_LOAD: i64 = 256;
const MAX_PENDING_PER_LOAD: i64 = 1024;
const MAX_OUTBOX_PER_LOAD: i64 = 256;
/// The most sessions the swarm view lists.
pub const MAX_HISTORY_SESSIONS: usize = 50;
const HISTORY_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1000;
pub const AUTOMATION_FIRING_NOT_WAITING: &str = "automation firing is not waiting";
pub const AUTOMATION_FIRING_FINISHED: &str = "automation firing already finished";
pub const AUTOMATION_FIRING_END_PENDING: &str = "an automation firing must end in a final status";
pub const AUTOMATION_COMMIT_INCOMPLETE: &str = "only a firing that completes commits state";
pub const AUTOMATION_ACTION_FINISHED: &str = "automation action already finished";
pub const AUTOMATION_ACTION_END_INVALID: &str =
    "an automation action must end in a final status other than delivered";
/// Why waiting `armed`, `idle`, `needs_input` and `schedule` events are
/// dropped at startup.
pub const SUPERSEDED_ON_RESUME: &str = "resume fires armed and schedules catch up";
/// Why waiting firings end when their session moves to another directory.
pub const INTERRUPTED_BY_RELOCATION: &str = "the session moved to another directory";
const EMPTY_OBJECT: &str = "{}";
const NOW_MS: &str = "CAST(unixepoch('subsec') * 1000 AS INTEGER)";
const PENDING_STATUSES: &str = "('queued', 'deferred', 'running')";
const WAITING_STATUSES: &str = "('queued', 'deferred')";
/// The endings that keep a consumed message from the model for good.
const KEEPING_STATUSES: &str = "('completed', 'skipped')";
const SUPERSEDED_TRIGGERS: &str = "('armed', 'idle', 'needs_input', 'schedule')";

/// The bindings of every automation a session armed, the script versions its
/// firings ran, the firings, and the actions each took. Small columns come
/// before the bodies so reading a row's status or `bytes` never walks its
/// overflow pages. Byte bounds live in this module rather than in CHECKs, so
/// raising one needs no table rebuild.
pub(crate) const TABLES: &str = r#"
CREATE TABLE automation_bindings (
    session_id       BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    automation       TEXT NOT NULL,
    scope            TEXT NOT NULL CHECK(scope IN ('user', 'project')),
    origin           TEXT NOT NULL CHECK(origin IN ('cli', 'profile', 'manual', 'always', 'sdk')),
    armed            INTEGER NOT NULL CHECK(armed IN (0, 1)),
    args_digest      TEXT,
    state_revision   INTEGER NOT NULL DEFAULT 0 CHECK(state_revision >= 0),
    state_writer     TEXT,
    state_digest     TEXT,
    state_written_ms INTEGER,
    created_ms       INTEGER NOT NULL,
    updated_ms       INTEGER NOT NULL,
    bytes            INTEGER NOT NULL GENERATED ALWAYS AS (
        length(CAST(automation AS BLOB)) + coalesce(length(CAST(args_digest AS BLOB)), 0)
        + coalesce(length(CAST(state_writer AS BLOB)), 0) + coalesce(length(CAST(state_digest AS BLOB)), 0)
        + length(CAST(args AS BLOB)) + length(CAST(state AS BLOB)) + length(CAST(limiter AS BLOB))
        + length(CAST(schedule AS BLOB)) + length(CAST(work_cursor AS BLOB))
    ) STORED,
    args             TEXT NOT NULL CHECK(json_valid(args)),
    state            TEXT NOT NULL CHECK(json_valid(state)),
    limiter          TEXT NOT NULL CHECK(json_valid(limiter)),
    schedule         TEXT NOT NULL CHECK(json_valid(schedule)),
    work_cursor      TEXT NOT NULL CHECK(json_valid(work_cursor)),
    PRIMARY KEY(session_id, automation)
) STRICT;

CREATE TABLE automation_sources (
    session_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    digest     TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    bytes      INTEGER NOT NULL GENERATED ALWAYS AS (
        length(CAST(digest AS BLOB)) + length(CAST(source AS BLOB))
    ) STORED,
    source     TEXT NOT NULL,
    PRIMARY KEY(session_id, digest)
) STRICT;

CREATE TABLE automation_firings (
    fire_id           TEXT PRIMARY KEY,
    session_id        BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    automation        TEXT NOT NULL,
    digest            TEXT NOT NULL,
    trigger_kind      TEXT NOT NULL CHECK(trigger_kind IN ('armed', 'idle', 'needs_input', 'goal_finished', 'message_received', 'work_finished', 'workflow_finished', 'schedule')),
    trigger_index     INTEGER NOT NULL CHECK(trigger_index >= 0),
    event_key         TEXT,
    consumed          INTEGER NOT NULL CHECK(consumed IN (0, 1)),
    status            TEXT NOT NULL CHECK(status IN ('queued', 'deferred', 'running', 'completed', 'skipped', 'released', 'failed', 'rate_limited', 'cancelled', 'paused', 'dropped', 'interrupted')),
    reason            TEXT,
    error_kind        TEXT,
    error_message     TEXT,
    error_line        INTEGER,
    error_column      INTEGER,
    repeats           INTEGER NOT NULL DEFAULT 1 CHECK(repeats >= 1),
    attempts          INTEGER NOT NULL DEFAULT 0 CHECK(attempts >= 0),
    operations        INTEGER NOT NULL DEFAULT 0 CHECK(operations >= 0),
    state_outcome     TEXT CHECK(state_outcome IN ('committed', 'conflict')),
    event_cut         INTEGER NOT NULL CHECK(event_cut IN (0, 1)),
    patch_cut         INTEGER NOT NULL DEFAULT 0 CHECK(patch_cut IN (0, 1)),
    queued_ms         INTEGER NOT NULL,
    deferred_until_ms INTEGER,
    started_ms        INTEGER,
    finished_ms       INTEGER,
    bytes             INTEGER NOT NULL GENERATED ALWAYS AS (
        length(CAST(fire_id AS BLOB)) + length(CAST(automation AS BLOB)) + length(CAST(digest AS BLOB))
        + coalesce(length(CAST(event_key AS BLOB)), 0) + coalesce(length(CAST(reason AS BLOB)), 0)
        + coalesce(length(CAST(error_kind AS BLOB)), 0) + coalesce(length(CAST(error_message AS BLOB)), 0)
        + length(CAST(event AS BLOB)) + coalesce(length(CAST(state_patch AS BLOB)), 0)
    ) STORED,
    event             TEXT NOT NULL CHECK(json_valid(event)),
    state_patch       TEXT CHECK(state_patch IS NULL OR json_valid(state_patch))
) STRICT;

CREATE INDEX automation_firings_by_automation ON automation_firings(session_id, automation, queued_ms);
CREATE INDEX automation_firings_pending ON automation_firings(session_id, queued_ms)
    WHERE status IN ('queued', 'deferred', 'running');

CREATE TABLE automation_actions (
    fire_id       TEXT NOT NULL REFERENCES automation_firings(fire_id) ON DELETE CASCADE,
    seq           INTEGER NOT NULL CHECK(seq >= 0),
    kind          TEXT NOT NULL CHECK(kind IN ('message', 'set_goal', 'notify', 'http', 'reply', 'send', 'publish', 'broadcast', 'start_workflow', 'pause', 'log')),
    source_line   INTEGER,
    source_column INTEGER,
    request_hash  TEXT NOT NULL,
    request_cut   INTEGER NOT NULL CHECK(request_cut IN (0, 1)),
    status        TEXT NOT NULL CHECK(status IN ('running', 'done', 'failed', 'refused', 'queued', 'delivered', 'deduplicated', 'dropped', 'expired', 'interrupted')),
    result_cut    INTEGER NOT NULL DEFAULT 0 CHECK(result_cut IN (0, 1)),
    error         TEXT,
    target        TEXT,
    delivery      TEXT CHECK(delivery IN ('next', 'guide')),
    expires_ms    INTEGER,
    turn_outcome  TEXT,
    turn_cost     REAL,
    started_ms    INTEGER NOT NULL,
    finished_ms   INTEGER,
    delivered_ms  INTEGER,
    bytes         INTEGER NOT NULL GENERATED ALWAYS AS (
        length(CAST(fire_id AS BLOB)) + length(CAST(request_hash AS BLOB))
        + coalesce(length(CAST(error AS BLOB)), 0) + coalesce(length(CAST(target AS BLOB)), 0)
        + coalesce(length(CAST(turn_outcome AS BLOB)), 0)
        + length(CAST(request AS BLOB)) + coalesce(length(CAST(result AS BLOB)), 0)
    ) STORED,
    request       TEXT NOT NULL CHECK(json_valid(request)),
    result        TEXT CHECK(result IS NULL OR json_valid(result)),
    PRIMARY KEY(fire_id, seq)
) STRICT;

CREATE INDEX automation_actions_queued ON automation_actions(fire_id, seq) WHERE status = 'queued';
"#;
/// Per-session automation row bytes, as a scalar over the `sessions` alias.
pub(crate) const SESSION_AUTOMATION_BYTES: &str = "coalesce((SELECT sum(bytes) \
        FROM automation_bindings WHERE session_id = sessions.id), 0) \
     + coalesce((SELECT sum(bytes) FROM automation_sources WHERE session_id = sessions.id), 0) \
     + coalesce((SELECT sum(bytes) FROM automation_firings WHERE session_id = sessions.id), 0) \
     + coalesce((SELECT sum(actions.bytes) FROM automation_actions AS actions \
        JOIN automation_firings AS firings ON firings.fire_id = actions.fire_id \
        WHERE firings.session_id = sessions.id), 0)";
const BINDING_COLUMNS: &str = "session_id, automation, scope, origin, armed, args, args_digest, \
     state, state_revision, state_writer, state_digest, state_written_ms, limiter, schedule, \
     work_cursor, created_ms, updated_ms, bytes";
const FIRING_SUMMARY_COLUMNS: &str = "f.fire_id, f.session_id, f.automation, f.digest, \
     f.trigger_kind, f.trigger_index, f.event_key, f.consumed, f.status, f.reason, f.error_kind, \
     f.error_message, f.error_line, f.error_column, f.repeats, f.attempts, f.operations, \
     f.state_outcome, f.event_cut, f.patch_cut, f.queued_ms, f.deferred_until_ms, f.started_ms, \
     f.finished_ms, f.bytes, \
     (SELECT count(*) FROM automation_actions AS a WHERE a.fire_id = f.fire_id), \
     (SELECT kind FROM automation_actions AS a WHERE a.fire_id = f.fire_id ORDER BY seq LIMIT 1)";
const FIRING_SUMMARY_WIDTH: usize = 27;
const ACTION_SUMMARY_COLUMNS: &str = "a.fire_id, a.seq, a.kind, a.source_line, a.source_column, \
     a.request_hash, a.request_cut, a.status, a.result_cut, a.error, a.target, a.delivery, \
     a.expires_ms, a.turn_outcome, a.turn_cost, a.started_ms, a.finished_ms, a.delivered_ms, \
     a.bytes";
const ACTION_SUMMARY_WIDTH: usize = 19;

text_enum! {
    AutomationScope as "automation scope" {
        User = "user",
        Project = "project",
    }
}

text_enum! {
    AutomationOrigin as "automation origin" {
        Cli = "cli",
        Profile = "profile",
        Manual = "manual",
        Always = "always",
        Sdk = "sdk",
    }
}

text_enum! {
    AutomationTrigger as "automation trigger" {
        Armed = "armed",
        Idle = "idle",
        NeedsInput = "needs_input",
        GoalFinished = "goal_finished",
        MessageReceived = "message_received",
        WorkFinished = "work_finished",
        WorkflowFinished = "workflow_finished",
        Schedule = "schedule",
    }
}

text_enum! {
    AutomationFiringStatus as "automation firing status" {
        Queued = "queued",
        Deferred = "deferred",
        Running = "running",
        Completed = "completed",
        Skipped = "skipped",
        Released = "released",
        Failed = "failed",
        RateLimited = "rate_limited",
        Cancelled = "cancelled",
        Paused = "paused",
        Dropped = "dropped",
        Interrupted = "interrupted",
    }
}

text_enum! {
    AutomationStateOutcome as "automation state outcome" {
        Committed = "committed",
        Conflict = "conflict",
    }
}

text_enum! {
    AutomationActionKind as "automation action kind" {
        Message = "message",
        SetGoal = "set_goal",
        Notify = "notify",
        Http = "http",
        Reply = "reply",
        Send = "send",
        Publish = "publish",
        Broadcast = "broadcast",
        StartWorkflow = "start_workflow",
        Pause = "pause",
        Log = "log",
    }
}

text_enum! {
    AutomationActionStatus as "automation action status" {
        Running = "running",
        Done = "done",
        Failed = "failed",
        Refused = "refused",
        Queued = "queued",
        Delivered = "delivered",
        Deduplicated = "deduplicated",
        Dropped = "dropped",
        Expired = "expired",
        Interrupted = "interrupted",
    }
}

text_enum! {
    AutomationDelivery as "automation delivery" {
        Next = "next",
        Guide = "guide",
    }
}

impl AutomationFiringStatus {
    /// Waiting or running: a firing the trace never prunes.
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Queued | Self::Deferred | Self::Running)
    }

    /// The endings that commit state: running to the end, `skip` or `release`.
    const fn completes(self) -> bool {
        matches!(self, Self::Completed | Self::Skipped | Self::Released)
    }
}

impl AutomationActionStatus {
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Running | Self::Queued)
    }
}

/// One `automation_bindings` row. JSON columns stay opaque strings the caller
/// has already validated; the revision, timestamps and `bytes` are assigned by
/// storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationBindingRow {
    pub session_id: CaudraId,
    pub automation: String,
    pub scope: AutomationScope,
    pub origin: AutomationOrigin,
    pub armed: bool,
    pub args: String,
    /// The script digest `args` were last validated against.
    pub args_digest: Option<String>,
    pub state: String,
    pub state_revision: u64,
    /// The firing that wrote `state`, or `None` after a human edit or clear.
    pub state_writer: Option<String>,
    /// The script digest the writing firing ran.
    pub state_digest: Option<String>,
    pub state_written_ms: Option<u64>,
    /// Acting times, failure streak and backoff, as the runtime keeps them.
    pub limiter: String,
    pub schedule: String,
    pub work_cursor: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub bytes: u64,
}

/// How a session arms an automation. Arming again replaces these and keeps
/// the binding's state and marks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationArming {
    pub session_id: CaudraId,
    pub automation: String,
    pub scope: AutomationScope,
    pub origin: AutomationOrigin,
    pub armed: bool,
    pub args: String,
    pub args_digest: Option<String>,
}

/// Binding marks that survive a restart. `None` leaves a mark alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutomationMarks {
    pub limiter: Option<String>,
    pub schedule: Option<String>,
    pub work_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateWrite {
    Committed {
        revision: u64,
    },
    /// The state moved past the revision the writer loaded, so nothing was
    /// written.
    Conflict {
        revision: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAutomationFiring {
    pub fire_id: String,
    pub session_id: CaudraId,
    pub automation: String,
    pub digest: String,
    pub trigger: AutomationTrigger,
    /// The entry of `meta.triggers` the event matched.
    pub trigger_index: u32,
    pub event: String,
    /// The delivery a message event carries, which a later copy of the same
    /// message is recognized by.
    pub event_key: Option<String>,
    /// The firing owns the message it handles.
    pub consumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiringError {
    pub kind: String,
    pub message: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

/// A firing as listings show it: everything but its event and state patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationFiringSummary {
    pub fire_id: String,
    pub session_id: CaudraId,
    pub automation: String,
    pub digest: String,
    pub trigger: AutomationTrigger,
    pub trigger_index: u32,
    pub event_key: Option<String>,
    pub consumed: bool,
    pub status: AutomationFiringStatus,
    pub reason: Option<String>,
    pub error: Option<FiringError>,
    /// How many firings the row stands for, counting the quiet skips it
    /// absorbed.
    pub repeats: u64,
    /// How often a limit deferred the firing.
    pub attempts: u64,
    pub operations: u64,
    pub state_outcome: Option<AutomationStateOutcome>,
    pub event_cut: bool,
    pub patch_cut: bool,
    pub queued_ms: u64,
    pub deferred_until_ms: Option<u64>,
    pub started_ms: Option<u64>,
    pub finished_ms: Option<u64>,
    pub bytes: u64,
    pub action_count: u64,
    pub first_action: Option<AutomationActionKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationFiringRow {
    pub summary: AutomationFiringSummary,
    pub event: String,
    /// The RFC 7396 patch the firing made to its state.
    pub state_patch: Option<String>,
    /// The consumed message the firing holds, as messaging releases it.
    pub delivery: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AutomationFiringDetail {
    pub firing: AutomationFiringRow,
    pub actions: Vec<AutomationActionSummary>,
}

/// How a firing ended. A commit is refused unless the firing completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationFiringEnd {
    pub status: AutomationFiringStatus,
    pub reason: Option<String>,
    pub error: Option<FiringError>,
    pub operations: u64,
    pub state_patch: Option<String>,
    pub commit: Option<StateCommit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateCommit {
    pub expected_revision: u64,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiringFinished {
    pub state: Option<StateWrite>,
    /// The earlier quiet skip this firing absorbed. Its row is gone, and this
    /// one counts its repeats.
    pub absorbed: Option<String>,
    /// The delivery of the consumed message an ending that does not keep it
    /// hands back. The row holds it until the release succeeds.
    pub release: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAutomationAction {
    pub fire_id: String,
    pub seq: u64,
    pub kind: AutomationActionKind,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub request_hash: String,
    /// The redacted request.
    pub request: String,
    pub status: AutomationActionStatus,
    pub delivery: Option<AutomationDelivery>,
    pub expires_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationActionEnd {
    pub status: AutomationActionStatus,
    pub result: Option<String>,
    pub error: Option<String>,
    /// The run id, the message id, the item it was deduplicated into, or the origin an `http`
    /// request went to.
    pub target: Option<String>,
}

/// An action as a trace lists it: everything but its request and result,
/// which load on demand.
#[derive(Debug, Clone, PartialEq)]
pub struct AutomationActionSummary {
    pub fire_id: String,
    pub seq: u64,
    pub kind: AutomationActionKind,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub request_hash: String,
    pub request_cut: bool,
    pub status: AutomationActionStatus,
    pub result_cut: bool,
    pub error: Option<String>,
    pub target: Option<String>,
    pub delivery: Option<AutomationDelivery>,
    pub expires_ms: Option<u64>,
    /// What the turn a delivery started led to, written when it settled.
    pub turn_outcome: Option<String>,
    pub turn_cost: Option<f64>,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
    pub delivered_ms: Option<u64>,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AutomationActionRow {
    pub summary: AutomationActionSummary,
    /// The redacted request.
    pub request: String,
    pub result: Option<String>,
}

/// A delivery waiting in a session's outbox.
#[derive(Debug, Clone, PartialEq)]
pub struct AutomationOutboxRow {
    pub automation: String,
    pub action: AutomationActionRow,
}

/// Another session the swarm view lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationSessionRow {
    pub session_id: CaudraId,
    pub title: String,
    /// The messaging name the session reclaims, known while it is offline.
    pub handle: Option<String>,
    pub last_activity_ms: u64,
}

/// What session start changed before anything was armed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutomationStartup {
    pub interrupted: usize,
    pub dropped: usize,
    /// Consumed messages that finished firings still hand back to normal
    /// delivery, oldest first.
    pub releases: Vec<AutomationRelease>,
}

/// A consumed message a finished firing hands back, kept until the release
/// succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationRelease {
    pub fire_id: String,
    pub delivery: String,
}

/// What an automation already recorded of a message event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationEventSeen {
    Unseen,
    /// A firing holds the message: it waits or runs on it, kept it by
    /// completing or skipping, or still has to release it.
    Holding,
    /// The firings saw the message without holding it, or released it.
    Handed,
}

pub(crate) struct AutomationTotals {
    pub(crate) firing_count: u64,
    pub(crate) action_count: u64,
    pub(crate) bytes: u64,
}

pub(crate) struct AutomationTrim {
    pub(crate) firing_rows: u64,
    pub(crate) action_rows: u64,
    pub(crate) source_rows: u64,
    pub(crate) bytes: u64,
}

/// The firing a finish applies to, as storage holds it.
struct Finishing {
    session_id: CaudraId,
    automation: String,
    digest: String,
    status: AutomationFiringStatus,
    consumed: bool,
    stored: u64,
    actions: u64,
    delivery: Option<String>,
}

impl SessionDatabase {
    /// Arms, re-arms or disarms an automation for a session. A binding that
    /// already exists keeps its state and marks.
    pub fn upsert_automation_binding(&self, arming: &AutomationArming) -> Result<(), SessionError> {
        bounded_identifier("automation name", &arming.automation)?;
        bounded_optional_identifier("automation args digest", arming.args_digest.as_deref())?;
        bounded_json("automation args", &arming.args, MAX_ARGS_BYTES)?;
        self.connection().execute(
            &format!(
                "INSERT INTO automation_bindings (session_id, automation, scope, origin, armed, \
                     args, args_digest, state, limiter, schedule, work_cursor, created_ms, \
                     updated_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?8, ?8, {NOW_MS}, {NOW_MS}) \
                 ON CONFLICT(session_id, automation) DO UPDATE SET scope = excluded.scope, \
                     origin = excluded.origin, armed = excluded.armed, args = excluded.args, \
                     args_digest = excluded.args_digest, updated_ms = excluded.updated_ms"
            ),
            params![
                arming.session_id.as_bytes().as_slice(),
                arming.automation,
                arming.scope.as_str(),
                arming.origin.as_str(),
                arming.armed,
                arming.args,
                arming.args_digest,
                EMPTY_OBJECT,
            ],
        )?;
        Ok(())
    }

    pub fn load_automation_binding(
        &self,
        session_id: CaudraId,
        automation: &str,
    ) -> Result<Option<AutomationBindingRow>, SessionError> {
        self.connection()
            .query_row(
                &format!(
                    "SELECT {BINDING_COLUMNS} FROM automation_bindings \
                     WHERE session_id = ?1 AND automation = ?2"
                ),
                params![session_id.as_bytes().as_slice(), automation],
                |row| Ok(read_binding(row)),
            )
            .optional()?
            .transpose()
    }

    /// Every binding of a session by name. A plain read, so the swarm view
    /// uses it for other sessions too.
    pub fn load_automation_bindings(
        &self,
        session_id: CaudraId,
    ) -> Result<Vec<AutomationBindingRow>, SessionError> {
        let mut statement = self.connection().prepare(&format!(
            "SELECT {BINDING_COLUMNS} FROM automation_bindings WHERE session_id = ?1 \
             ORDER BY automation LIMIT ?2"
        ))?;
        let mut rows = statement.query(params![
            session_id.as_bytes().as_slice(),
            MAX_BINDINGS_PER_LOAD
        ])?;
        let mut bindings = Vec::new();
        while let Some(row) = rows.next()? {
            bindings.push(read_binding(row)?);
        }
        Ok(bindings)
    }

    pub fn arm_automation(
        &self,
        session_id: CaudraId,
        automation: &str,
        origin: AutomationOrigin,
    ) -> Result<(), SessionError> {
        let changed = self.connection().execute(
            &format!(
                "UPDATE automation_bindings SET armed = 1, origin = ?3, updated_ms = {NOW_MS} \
                 WHERE session_id = ?1 AND automation = ?2"
            ),
            params![
                session_id.as_bytes().as_slice(),
                automation,
                origin.as_str()
            ],
        )?;
        binding_changed(changed, session_id, automation)
    }

    pub fn disarm_automation(
        &self,
        session_id: CaudraId,
        automation: &str,
    ) -> Result<(), SessionError> {
        let changed = self.connection().execute(
            &format!(
                "UPDATE automation_bindings SET armed = 0, updated_ms = {NOW_MS} \
                 WHERE session_id = ?1 AND automation = ?2"
            ),
            params![session_id.as_bytes().as_slice(), automation],
        )?;
        binding_changed(changed, session_id, automation)
    }

    /// Replaces the args and the script digest they were validated against.
    pub fn set_automation_args(
        &self,
        session_id: CaudraId,
        automation: &str,
        args: &str,
        args_digest: Option<&str>,
    ) -> Result<(), SessionError> {
        bounded_optional_identifier("automation args digest", args_digest)?;
        bounded_json("automation args", args, MAX_ARGS_BYTES)?;
        let changed = self.connection().execute(
            &format!(
                "UPDATE automation_bindings SET args = ?3, args_digest = ?4, \
                     updated_ms = {NOW_MS} \
                 WHERE session_id = ?1 AND automation = ?2"
            ),
            params![
                session_id.as_bytes().as_slice(),
                automation,
                args,
                args_digest
            ],
        )?;
        binding_changed(changed, session_id, automation)
    }

    /// Replaces the state as a human edit does: only at the revision the
    /// editor loaded, and with no firing as its writer. A firing that loaded
    /// the older revision then fails to commit.
    pub fn commit_automation_state(
        &self,
        session_id: CaudraId,
        automation: &str,
        expected_revision: u64,
        state: &str,
    ) -> Result<StateWrite, SessionError> {
        write_state(
            self.connection(),
            session_id,
            automation,
            expected_revision,
            state,
            None,
        )
    }

    /// Empties the state and bumps its revision, so a firing running across
    /// the clear commits nothing. Returns the new revision.
    pub fn clear_automation_state(
        &self,
        session_id: CaudraId,
        automation: &str,
    ) -> Result<u64, SessionError> {
        let revision: Option<i64> = self
            .connection()
            .query_row(
                &format!(
                    "UPDATE automation_bindings SET state = ?3, \
                         state_revision = state_revision + 1, state_writer = NULL, \
                         state_digest = NULL, state_written_ms = {NOW_MS}, \
                         updated_ms = {NOW_MS} \
                     WHERE session_id = ?1 AND automation = ?2 RETURNING state_revision"
                ),
                params![session_id.as_bytes().as_slice(), automation, EMPTY_OBJECT],
                |row| row.get(0),
            )
            .optional()?;
        let revision = revision.ok_or_else(|| missing_binding(session_id, automation))?;
        from_i64(revision, "automation_bindings.state_revision")
    }

    /// Saves the schedule, limiter and `work_finished` marks, so a restart
    /// neither re-fires a schedule, resets a limit window, nor repeats a work
    /// outcome.
    pub fn save_automation_marks(
        &self,
        session_id: CaudraId,
        automation: &str,
        marks: &AutomationMarks,
    ) -> Result<(), SessionError> {
        bounded_optional_json(
            "automation limiter marks",
            marks.limiter.as_deref(),
            MAX_MARKS_BYTES,
        )?;
        bounded_optional_json(
            "automation schedule marks",
            marks.schedule.as_deref(),
            MAX_MARKS_BYTES,
        )?;
        bounded_optional_json(
            "automation work cursor",
            marks.work_cursor.as_deref(),
            MAX_MARKS_BYTES,
        )?;
        let changed = self.connection().execute(
            &format!(
                "UPDATE automation_bindings SET limiter = coalesce(?3, limiter), \
                     schedule = coalesce(?4, schedule), \
                     work_cursor = coalesce(?5, work_cursor), updated_ms = {NOW_MS} \
                 WHERE session_id = ?1 AND automation = ?2"
            ),
            params![
                session_id.as_bytes().as_slice(),
                automation,
                marks.limiter,
                marks.schedule,
                marks.work_cursor,
            ],
        )?;
        binding_changed(changed, session_id, automation)
    }

    /// Keeps the text of a script version a firing ran, so its trace shows
    /// the right lines after an edit. Returns whether the version was new.
    pub fn insert_automation_source(
        &self,
        session_id: CaudraId,
        digest: &str,
        source: &str,
    ) -> Result<bool, SessionError> {
        bounded_identifier("automation source digest", digest)?;
        SessionDatabase::validate_len("automation source", source.len(), MAX_SOURCE_BYTES)?;
        let inserted = self.connection().execute(
            &format!(
                "INSERT OR IGNORE INTO automation_sources (session_id, digest, source, created_ms) \
                 VALUES (?1, ?2, ?3, {NOW_MS})"
            ),
            params![session_id.as_bytes().as_slice(), digest, source],
        )?;
        Ok(inserted == 1)
    }

    pub fn load_automation_source(
        &self,
        session_id: CaudraId,
        digest: &str,
    ) -> Result<Option<String>, SessionError> {
        Ok(self
            .connection()
            .query_row(
                "SELECT source FROM automation_sources WHERE session_id = ?1 AND digest = ?2",
                params![session_id.as_bytes().as_slice(), digest],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Records an event waiting for its automation, cut to
    /// [`MAX_EVENT_BYTES`] when larger, and prunes the automation's finished
    /// firings past [`MAX_FINISHED_FIRINGS`].
    pub fn insert_automation_firing(
        &self,
        firing: &NewAutomationFiring,
    ) -> Result<(), SessionError> {
        self.insert_firing(firing, None)
    }

    /// [`Self::insert_automation_firing`] for a firing that consumed a
    /// message: it holds the message's `delivery`, whole, until it completes
    /// or skips, or until a release of it succeeds.
    pub fn insert_consuming_automation_firing(
        &self,
        firing: &NewAutomationFiring,
        delivery: &str,
    ) -> Result<(), SessionError> {
        SessionDatabase::validate_payload_json("automation delivery", delivery)?;
        self.insert_firing(firing, Some(delivery))
    }

    fn insert_firing(
        &self,
        firing: &NewAutomationFiring,
        delivery: Option<&str>,
    ) -> Result<(), SessionError> {
        bounded_identifier("automation fire id", &firing.fire_id)?;
        bounded_identifier("automation name", &firing.automation)?;
        bounded_identifier("automation digest", &firing.digest)?;
        bounded_optional_identifier("automation event key", firing.event_key.as_deref())?;
        let (event, event_cut) = stored_body("automation event", &firing.event, MAX_EVENT_BYTES)?;
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        transaction.execute(
            &format!(
                "INSERT INTO automation_firings (fire_id, session_id, automation, digest, \
                     trigger_kind, trigger_index, event_key, consumed, status, event_cut, \
                     queued_ms, event, delivery) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, {NOW_MS}, ?11, ?12)"
            ),
            params![
                firing.fire_id,
                firing.session_id.as_bytes().as_slice(),
                firing.automation,
                firing.digest,
                firing.trigger.as_str(),
                firing.trigger_index,
                firing.event_key,
                firing.consumed,
                AutomationFiringStatus::Queued.as_str(),
                event_cut,
                event.as_ref(),
                delivery,
            ],
        )?;
        let pruned = prune_finished(&transaction, firing.session_id, &firing.automation)?;
        transaction.commit()?;
        if pruned > 0 {
            tracing::debug!(
                session = %firing.session_id,
                automation = firing.automation,
                pruned,
                "pruned finished automation firings"
            );
        }
        Ok(())
    }

    /// Puts a firing back to wait until `until_ms`, as a limit does to a
    /// one-shot event, and counts the attempt. Returns the attempts so far.
    pub fn defer_automation_firing(
        &self,
        fire_id: &str,
        until_ms: u64,
    ) -> Result<u64, SessionError> {
        let attempts: Option<i64> = self
            .connection()
            .query_row(
                &format!(
                    "UPDATE automation_firings SET status = ?3, deferred_until_ms = ?2, \
                         attempts = attempts + 1 \
                     WHERE fire_id = ?1 AND status IN {PENDING_STATUSES} RETURNING attempts"
                ),
                params![
                    fire_id,
                    to_i64(until_ms, "automation_firings.deferred_until_ms")?,
                    AutomationFiringStatus::Deferred.as_str()
                ],
                |row| row.get(0),
            )
            .optional()?;
        match attempts {
            Some(attempts) => from_i64(attempts, "automation_firings.attempts"),
            None => Err(firing_refusal(
                self.connection(),
                fire_id,
                AUTOMATION_FIRING_FINISHED,
            )?),
        }
    }

    pub fn start_automation_firing(&self, fire_id: &str) -> Result<(), SessionError> {
        let changed = self.connection().execute(
            &format!(
                "UPDATE automation_firings SET status = ?2, started_ms = {NOW_MS}, \
                     deferred_until_ms = NULL \
                 WHERE fire_id = ?1 AND status IN {WAITING_STATUSES}"
            ),
            params![fire_id, AutomationFiringStatus::Running.as_str()],
        )?;
        if changed == 1 {
            return Ok(());
        }
        Err(firing_refusal(
            self.connection(),
            fire_id,
            AUTOMATION_FIRING_NOT_WAITING,
        )?)
    }

    /// Gives up the message a pending firing consumed, because the session
    /// queued it for its model again: the firing neither holds nor releases
    /// it, and `event` says it was not consumed.
    pub fn downgrade_automation_firing(
        &self,
        fire_id: &str,
        event: &str,
    ) -> Result<(), SessionError> {
        let (event, event_cut) = stored_body("automation event", event, MAX_EVENT_BYTES)?;
        let changed = self.connection().execute(
            &format!(
                "UPDATE automation_firings SET consumed = 0, delivery = NULL, event = ?2, \
                     event_cut = ?3 \
                 WHERE fire_id = ?1 AND status IN {PENDING_STATUSES}"
            ),
            params![fire_id, event.as_ref(), event_cut],
        )?;
        if changed == 1 {
            return Ok(());
        }
        Err(firing_refusal(
            self.connection(),
            fire_id,
            AUTOMATION_FIRING_FINISHED,
        )?)
    }

    /// Forgets the delivery a firing held once its release succeeded, or once
    /// it turned out unreadable.
    pub fn clear_automation_delivery(&self, fire_id: &str) -> Result<(), SessionError> {
        self.connection().execute(
            "UPDATE automation_firings SET delivery = NULL WHERE fire_id = ?1",
            params![fire_id],
        )?;
        Ok(())
    }

    /// Records a firing's final status. A commit applies in the same
    /// transaction at the revision the firing loaded; a conflict leaves the
    /// state alone and says so in the trace. Actions still running become
    /// `interrupted`. A completed or skipped firing keeps its consumed message
    /// for good, so it forgets the delivery; any other ending answers with it
    /// and holds it until the release succeeds. A quiet ending, without
    /// actions, state change or a consumed message, absorbs the automation's
    /// newest row when that row is the same quiet skip.
    pub fn finish_automation_firing(
        &self,
        fire_id: &str,
        end: &AutomationFiringEnd,
    ) -> Result<FiringFinished, SessionError> {
        if end.status.is_pending() {
            return Err(refused(
                io::ErrorKind::InvalidInput,
                AUTOMATION_FIRING_END_PENDING,
            ));
        }
        if end.commit.is_some() && !end.status.completes() {
            return Err(refused(
                io::ErrorKind::InvalidInput,
                AUTOMATION_COMMIT_INCOMPLETE,
            ));
        }
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        let firing = load_finishing(&transaction, fire_id)?;
        if !firing.status.is_pending() {
            return Err(refused(
                io::ErrorKind::InvalidInput,
                AUTOMATION_FIRING_FINISHED,
            ));
        }
        let state = end
            .commit
            .as_ref()
            .map(|commit| {
                write_state(
                    &transaction,
                    firing.session_id,
                    &firing.automation,
                    commit.expected_revision,
                    &commit.state,
                    Some((fire_id, &firing.digest)),
                )
            })
            .transpose()?;
        let patch = end
            .state_patch
            .as_deref()
            .map(|patch| {
                stored_body(
                    "automation state patch",
                    patch,
                    body_cap(patch.len(), MAX_STATE_BYTES, firing.stored),
                )
            })
            .transpose()?;
        let outcome = state.map(|state| match state {
            StateWrite::Committed { .. } => AutomationStateOutcome::Committed,
            StateWrite::Conflict { .. } => AutomationStateOutcome::Conflict,
        });
        let error = end.error.as_ref();
        transaction.execute(
            &format!(
                "UPDATE automation_actions SET status = ?2, finished_ms = {NOW_MS} \
                 WHERE fire_id = ?1 AND status = ?3"
            ),
            params![
                fire_id,
                AutomationActionStatus::Interrupted.as_str(),
                AutomationActionStatus::Running.as_str()
            ],
        )?;
        transaction.execute(
            &format!(
                "UPDATE automation_firings SET status = ?2, reason = ?3, error_kind = ?4, \
                     error_message = ?5, error_line = ?6, error_column = ?7, operations = ?8, \
                     state_patch = ?9, patch_cut = ?10, state_outcome = ?11, \
                     finished_ms = {NOW_MS}, \
                     delivery = CASE WHEN ?2 IN {KEEPING_STATUSES} THEN NULL ELSE delivery END \
                 WHERE fire_id = ?1"
            ),
            params![
                fire_id,
                end.status.as_str(),
                end.reason
                    .as_deref()
                    .map(|reason| clipped(reason, MAX_TEXT_BYTES)),
                error.map(|error| clipped(&error.kind, MAX_IDENTIFIER_BYTES)),
                error.map(|error| clipped(&error.message, MAX_TEXT_BYTES)),
                error.and_then(|error| error.line),
                error.and_then(|error| error.column),
                to_i64(end.operations, "automation_firings.operations")?,
                patch.as_ref().map(|(patch, _)| patch.as_ref()),
                patch.as_ref().is_some_and(|(_, cut)| *cut),
                outcome.map(AutomationStateOutcome::as_str),
            ],
        )?;
        let keeps = matches!(
            end.status,
            AutomationFiringStatus::Completed | AutomationFiringStatus::Skipped
        );
        let quiet = keeps
            && !firing.consumed
            && firing.actions == 0
            && end.error.is_none()
            && end.state_patch.is_none()
            && end.commit.is_none();
        let absorbed = if quiet {
            absorb_previous(&transaction, fire_id)?
        } else {
            None
        };
        transaction.commit()?;
        Ok(FiringFinished {
            state,
            absorbed,
            release: firing.delivery.filter(|_| !keeps),
        })
    }

    /// Journals an action as it starts, or as it is queued for delivery. The
    /// request is cut to [`MAX_REQUEST_BYTES`], and to a preview once the
    /// firing stores [`MAX_FIRING_BYTES`].
    pub fn start_automation_action(
        &self,
        action: &NewAutomationAction,
    ) -> Result<(), SessionError> {
        bounded_identifier("automation fire id", &action.fire_id)?;
        bounded_identifier("automation request hash", &action.request_hash)?;
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        let stored = firing_bytes(&transaction, &action.fire_id)?;
        let (request, request_cut) = stored_body(
            "automation action request",
            &action.request,
            body_cap(action.request.len(), MAX_REQUEST_BYTES, stored),
        )?;
        transaction.execute(
            &format!(
                "INSERT INTO automation_actions (fire_id, seq, kind, source_line, source_column, \
                     request_hash, request, request_cut, status, delivery, expires_ms, \
                     started_ms, finished_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, {NOW_MS}, \
                     CASE WHEN ?12 THEN NULL ELSE {NOW_MS} END)"
            ),
            params![
                action.fire_id,
                to_i64(action.seq, "automation_actions.seq")?,
                action.kind.as_str(),
                action.line,
                action.column,
                action.request_hash,
                request.as_ref(),
                request_cut,
                action.status.as_str(),
                action.delivery.map(AutomationDelivery::as_str),
                action
                    .expires_ms
                    .map(|expires| to_i64(expires, "automation_actions.expires_ms"))
                    .transpose()?,
                action.status.is_pending(),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Records how a running or queued action ended. The result is cut to
    /// [`MAX_RESULT_BYTES`], and to a preview once the firing stores
    /// [`MAX_FIRING_BYTES`]. Deliveries go through
    /// [`Self::mark_automation_delivered`].
    pub fn finish_automation_action(
        &self,
        fire_id: &str,
        seq: u64,
        end: &AutomationActionEnd,
    ) -> Result<(), SessionError> {
        if end.status.is_pending() || end.status == AutomationActionStatus::Delivered {
            return Err(refused(
                io::ErrorKind::InvalidInput,
                AUTOMATION_ACTION_END_INVALID,
            ));
        }
        bounded_optional_identifier("automation action target", end.target.as_deref())?;
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        let stored = firing_bytes(&transaction, fire_id)?;
        let result = end
            .result
            .as_deref()
            .map(|result| {
                stored_body(
                    "automation action result",
                    result,
                    body_cap(result.len(), MAX_RESULT_BYTES, stored),
                )
            })
            .transpose()?;
        let seq = to_i64(seq, "automation_actions.seq")?;
        let changed = transaction.execute(
            &format!(
                "UPDATE automation_actions SET status = ?3, result = ?4, result_cut = ?5, \
                     error = ?6, target = ?7, finished_ms = {NOW_MS} \
                 WHERE fire_id = ?1 AND seq = ?2 AND status IN (?8, ?9)"
            ),
            params![
                fire_id,
                seq,
                end.status.as_str(),
                result.as_ref().map(|(result, _)| result.as_ref()),
                result.as_ref().is_some_and(|(_, cut)| *cut),
                end.error
                    .as_deref()
                    .map(|error| clipped(error, MAX_TEXT_BYTES)),
                end.target,
                AutomationActionStatus::Running.as_str(),
                AutomationActionStatus::Queued.as_str(),
            ],
        )?;
        if changed == 0 {
            return Err(action_refusal(&transaction, fire_id, seq)?);
        }
        transaction.commit()?;
        Ok(())
    }

    /// Records that the frontend handed a queued item to the agent loop, at
    /// once, so a crash afterwards never repeats it. Returns `false` when the
    /// item is no longer queued.
    pub fn mark_automation_delivered(&self, fire_id: &str, seq: u64) -> Result<bool, SessionError> {
        let changed = self.connection().execute(
            &format!(
                "UPDATE automation_actions SET status = ?3, delivered_ms = {NOW_MS}, \
                     finished_ms = {NOW_MS} \
                 WHERE fire_id = ?1 AND seq = ?2 AND status = ?4"
            ),
            params![
                fire_id,
                to_i64(seq, "automation_actions.seq")?,
                AutomationActionStatus::Delivered.as_str(),
                AutomationActionStatus::Queued.as_str(),
            ],
        )?;
        Ok(changed == 1)
    }

    /// Writes what a settled busy period led to onto every delivery it made,
    /// given as `(fire_id, seq)` pairs. Returns how many deliveries took it.
    pub fn record_automation_turn(
        &self,
        deliveries: &[(String, u64)],
        outcome: &str,
        cost: Option<f64>,
    ) -> Result<usize, SessionError> {
        let outcome = clipped(outcome, MAX_TEXT_BYTES);
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        let mut recorded = 0;
        for (fire_id, seq) in deliveries {
            recorded += transaction.execute(
                "UPDATE automation_actions SET turn_outcome = ?3, turn_cost = ?4 \
                 WHERE fire_id = ?1 AND seq = ?2 AND status = ?5",
                params![
                    fire_id,
                    to_i64(*seq, "automation_actions.seq")?,
                    outcome,
                    cost,
                    AutomationActionStatus::Delivered.as_str(),
                ],
            )?;
        }
        transaction.commit()?;
        Ok(recorded)
    }

    /// The deliveries waiting in a session's outbox, oldest first.
    pub fn load_automation_outbox(
        &self,
        session_id: CaudraId,
    ) -> Result<Vec<AutomationOutboxRow>, SessionError> {
        let mut statement = self.connection().prepare(&format!(
            "SELECT f.automation, {ACTION_SUMMARY_COLUMNS}, a.request, a.result \
             FROM automation_actions AS a \
             JOIN automation_firings AS f ON f.fire_id = a.fire_id \
             WHERE a.status = ?2 AND f.session_id = ?1 \
             ORDER BY a.started_ms, a.rowid LIMIT ?3"
        ))?;
        let mut rows = statement.query(params![
            session_id.as_bytes().as_slice(),
            AutomationActionStatus::Queued.as_str(),
            MAX_OUTBOX_PER_LOAD,
        ])?;
        let mut outbox = Vec::new();
        while let Some(row) = rows.next()? {
            outbox.push(AutomationOutboxRow {
                automation: row.get(0)?,
                action: read_action(row, 1)?,
            });
        }
        Ok(outbox)
    }

    /// The newest firings of a session, of one automation or of all, at most
    /// `limit` and never more than [`MAX_FIRINGS_PER_LOAD`]. A plain read
    /// without bodies, so the swarm view polls it for other sessions.
    pub fn load_automation_firings(
        &self,
        session_id: CaudraId,
        automation: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AutomationFiringSummary>, SessionError> {
        let mut statement = self.connection().prepare(&format!(
            "SELECT {FIRING_SUMMARY_COLUMNS} FROM automation_firings AS f \
             WHERE f.session_id = ?1 AND (?2 IS NULL OR f.automation = ?2) \
             ORDER BY f.queued_ms DESC, f.rowid DESC LIMIT ?3"
        ))?;
        let mut rows = statement.query(params![
            session_id.as_bytes().as_slice(),
            automation,
            to_i64(limit.min(MAX_FIRINGS_PER_LOAD), "automation firing limit")?,
        ])?;
        let mut firings = Vec::new();
        while let Some(row) = rows.next()? {
            firings.push(read_summary(row)?);
        }
        Ok(firings)
    }

    /// One firing with its event, state patch and every action in call order.
    /// Action bodies load on their own through [`Self::load_automation_action`].
    pub fn load_automation_firing(
        &self,
        fire_id: &str,
    ) -> Result<Option<AutomationFiringDetail>, SessionError> {
        let transaction = self.connection().unchecked_transaction()?;
        let firing = transaction
            .query_row(
                &format!(
                    "SELECT {FIRING_SUMMARY_COLUMNS}, f.event, f.state_patch, f.delivery \
                     FROM automation_firings AS f WHERE f.fire_id = ?1"
                ),
                params![fire_id],
                |row| Ok(read_firing(row)),
            )
            .optional()?
            .transpose()?;
        let Some(firing) = firing else {
            return Ok(None);
        };
        let mut statement = transaction.prepare(&format!(
            "SELECT {ACTION_SUMMARY_COLUMNS} FROM automation_actions AS a \
             WHERE a.fire_id = ?1 ORDER BY a.seq"
        ))?;
        let mut rows = statement.query(params![fire_id])?;
        let mut actions = Vec::new();
        while let Some(row) = rows.next()? {
            actions.push(read_action_summary(row, 0)?);
        }
        drop(rows);
        drop(statement);
        transaction.commit()?;
        Ok(Some(AutomationFiringDetail { firing, actions }))
    }

    /// One action with its request and result, for display.
    pub fn load_automation_action(
        &self,
        fire_id: &str,
        seq: u64,
    ) -> Result<Option<AutomationActionRow>, SessionError> {
        self.connection()
            .query_row(
                &format!(
                    "SELECT {ACTION_SUMMARY_COLUMNS}, a.request, a.result \
                     FROM automation_actions AS a WHERE a.fire_id = ?1 AND a.seq = ?2"
                ),
                params![fire_id, to_i64(seq, "automation_actions.seq")?],
                |row| Ok(read_action(row, 0)),
            )
            .optional()?
            .transpose()
    }

    /// A session's pending firings with their events, in the order they were
    /// queued. After [`Self::interrupt_automation_firings`] these are the
    /// waiting events a resumed session queues again.
    pub fn load_pending_automation_firings(
        &self,
        session_id: CaudraId,
    ) -> Result<Vec<AutomationFiringRow>, SessionError> {
        let mut statement = self.connection().prepare(&format!(
            "SELECT {FIRING_SUMMARY_COLUMNS}, f.event, f.state_patch, f.delivery \
             FROM automation_firings AS f \
             WHERE f.session_id = ?1 AND f.status IN {PENDING_STATUSES} \
             ORDER BY f.queued_ms, f.rowid LIMIT ?2"
        ))?;
        let mut rows = statement.query(params![
            session_id.as_bytes().as_slice(),
            MAX_PENDING_PER_LOAD
        ])?;
        let mut firings = Vec::new();
        while let Some(row) = rows.next()? {
            firings.push(read_firing(row)?);
        }
        Ok(firings)
    }

    /// Whether the automation already has a firing for this delivery, so a
    /// message seen again is not handled twice, and whether its newest such
    /// firing holds the message.
    pub fn automation_event_seen(
        &self,
        session_id: CaudraId,
        automation: &str,
        event_key: &str,
    ) -> Result<AutomationEventSeen, SessionError> {
        let holding: Option<bool> = self
            .connection()
            .query_row(
                &format!(
                    "SELECT consumed = 1 AND (delivery IS NOT NULL \
                         OR status IN {KEEPING_STATUSES}) \
                     FROM automation_firings \
                     WHERE session_id = ?1 AND automation = ?2 AND event_key = ?3 \
                     ORDER BY queued_ms DESC, rowid DESC LIMIT 1"
                ),
                params![session_id.as_bytes().as_slice(), automation, event_key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match holding {
            None => AutomationEventSeen::Unseen,
            Some(true) => AutomationEventSeen::Holding,
            Some(false) => AutomationEventSeen::Handed,
        })
    }

    /// Applies the startup rules before a session arms anything: running
    /// firings and their running actions become `interrupted`, and waiting
    /// `armed`, `idle`, `needs_input` and `schedule` events are dropped,
    /// because resume fires `armed` and schedules catch up. Returns the
    /// consumed messages finished firings still have to release, those just
    /// interrupted included.
    pub fn interrupt_automation_firings(
        &self,
        session_id: CaudraId,
    ) -> Result<AutomationStartup, SessionError> {
        let session = session_id.as_bytes();
        let transaction =
            Transaction::new_unchecked(self.connection(), TransactionBehavior::Immediate)?;
        transaction.execute(
            &format!(
                "UPDATE automation_actions SET status = ?2, finished_ms = {NOW_MS} \
                 WHERE status = ?3 AND fire_id IN (SELECT fire_id FROM automation_firings \
                     WHERE session_id = ?1 AND status = ?3)"
            ),
            params![
                session.as_slice(),
                AutomationActionStatus::Interrupted.as_str(),
                AutomationFiringStatus::Running.as_str(),
            ],
        )?;
        let interrupted = transaction.execute(
            &format!(
                "UPDATE automation_firings SET status = ?2, finished_ms = {NOW_MS} \
                 WHERE session_id = ?1 AND status = ?3"
            ),
            params![
                session.as_slice(),
                AutomationFiringStatus::Interrupted.as_str(),
                AutomationFiringStatus::Running.as_str(),
            ],
        )?;
        let dropped = transaction.execute(
            &format!(
                "UPDATE automation_firings SET status = ?2, reason = ?3, finished_ms = {NOW_MS} \
                 WHERE session_id = ?1 AND status IN {WAITING_STATUSES} \
                     AND trigger_kind IN {SUPERSEDED_TRIGGERS}"
            ),
            params![
                session.as_slice(),
                AutomationFiringStatus::Dropped.as_str(),
                SUPERSEDED_ON_RESUME,
            ],
        )?;
        let releases = {
            let mut statement = transaction.prepare(&format!(
                "SELECT fire_id, delivery FROM automation_firings \
                 WHERE session_id = ?1 AND delivery IS NOT NULL \
                     AND status NOT IN {PENDING_STATUSES} \
                 ORDER BY queued_ms, rowid"
            ))?;
            let mut rows = statement.query(params![session.as_slice()])?;
            let mut releases = Vec::new();
            while let Some(row) = rows.next()? {
                releases.push(AutomationRelease {
                    fire_id: row.get(0)?,
                    delivery: row.get(1)?,
                });
            }
            releases
        };
        transaction.commit()?;
        Ok(AutomationStartup {
            interrupted,
            dropped,
            releases,
        })
    }

    /// Other sessions with an armed automation or a firing in the last seven
    /// days, newest activity first, at most `limit` and never more than
    /// [`MAX_HISTORY_SESSIONS`]. A plain query on this connection: the swarm
    /// view never probes another session's lease.
    pub fn load_automation_history(
        &self,
        exclude: CaudraId,
        limit: usize,
    ) -> Result<Vec<AutomationSessionRow>, SessionError> {
        let mut statement = self.connection().prepare(&format!(
            "SELECT s.id, s.title, json_extract(s.metadata, '$.peer_controls.handle'), \
                 max(coalesce((SELECT max(queued_ms) FROM automation_firings \
                         WHERE session_id = s.id), 0), \
                     coalesce((SELECT max(updated_ms) FROM automation_bindings \
                         WHERE session_id = s.id), 0)) AS activity \
             FROM sessions AS s \
             WHERE s.id != ?1 AND s.id IN ( \
                 SELECT session_id FROM automation_bindings WHERE armed = 1 \
                 UNION SELECT session_id FROM automation_firings \
                     WHERE queued_ms >= {NOW_MS} - ?2) \
             ORDER BY activity DESC, s.id DESC LIMIT ?3"
        ))?;
        let mut rows = statement.query(params![
            exclude.as_bytes().as_slice(),
            HISTORY_WINDOW_MS,
            to_i64(limit.min(MAX_HISTORY_SESSIONS), "automation history limit")?,
        ])?;
        let mut sessions = Vec::new();
        while let Some(row) = rows.next()? {
            sessions.push(AutomationSessionRow {
                session_id: id_from_row(row, 0)?,
                title: row.get(1)?,
                handle: row.get(2)?,
                last_activity_ms: from_i64(row.get(3)?, "automation activity")?,
            });
        }
        Ok(sessions)
    }

    /// Bytes every automation row of a session accounts for.
    pub fn automation_bytes(&self, session_id: CaudraId) -> Result<u64, SessionError> {
        let bytes: i64 = self.connection().query_row(
            &format!("SELECT {SESSION_AUTOMATION_BYTES} FROM (SELECT ?1 AS id) AS sessions"),
            params![session_id.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        from_i64(bytes, "automation bytes")
    }
}

pub(crate) fn automation_totals(connection: &Connection) -> Result<AutomationTotals, SessionError> {
    let (firing_count, action_count, bytes) = connection.query_row(
        "SELECT (SELECT count(*) FROM automation_firings), \
                (SELECT count(*) FROM automation_actions), \
                (SELECT coalesce(sum(bytes), 0) FROM automation_bindings) \
                + (SELECT coalesce(sum(bytes), 0) FROM automation_sources) \
                + (SELECT coalesce(sum(bytes), 0) FROM automation_firings) \
                + (SELECT coalesce(sum(bytes), 0) FROM automation_actions)",
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )?;
    Ok(AutomationTotals {
        firing_count: from_i64(firing_count, "automation firing count")?,
        action_count: from_i64(action_count, "automation action count")?,
        bytes: from_i64(bytes, "automation bytes")?,
    })
}

/// What a trim does to a session's automations: every firing, action and
/// source goes, pending ones included, while bindings and their state stay.
pub(crate) fn trim_automations(
    connection: &Connection,
    session_id: CaudraId,
) -> Result<AutomationTrim, SessionError> {
    let session = session_id.as_bytes();
    let (firing_rows, action_rows, source_rows, bytes) = connection.query_row(
        "SELECT (SELECT count(*) FROM automation_firings WHERE session_id = ?1), \
                (SELECT count(*) FROM automation_actions WHERE fire_id IN \
                    (SELECT fire_id FROM automation_firings WHERE session_id = ?1)), \
                (SELECT count(*) FROM automation_sources WHERE session_id = ?1), \
                (SELECT coalesce(sum(bytes), 0) FROM automation_firings WHERE session_id = ?1) \
                + (SELECT coalesce(sum(bytes), 0) FROM automation_actions WHERE fire_id IN \
                    (SELECT fire_id FROM automation_firings WHERE session_id = ?1)) \
                + (SELECT coalesce(sum(bytes), 0) FROM automation_sources WHERE session_id = ?1)",
        params![session.as_slice()],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        },
    )?;
    connection.execute(
        "DELETE FROM automation_firings WHERE session_id = ?1",
        params![session.as_slice()],
    )?;
    connection.execute(
        "DELETE FROM automation_sources WHERE session_id = ?1",
        params![session.as_slice()],
    )?;
    Ok(AutomationTrim {
        firing_rows: from_i64(firing_rows, "trimmed automation firing rows")?,
        action_rows: from_i64(action_rows, "trimmed automation action rows")?,
        source_rows: from_i64(source_rows, "trimmed automation source rows")?,
        bytes: from_i64(bytes, "trimmed automation bytes")?,
    })
}

/// Whether a firing of the session is running, which refuses a relocation.
pub(crate) fn automation_running(
    connection: &Connection,
    session_id: CaudraId,
) -> Result<bool, SessionError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM automation_firings WHERE session_id = ?1 AND status = ?2)",
        params![
            session_id.as_bytes().as_slice(),
            AutomationFiringStatus::Running.as_str()
        ],
        |row| row.get(0),
    )?)
}

/// What moving a session to another directory does to its automations:
/// waiting firings are interrupted and project-scope bindings disarmed,
/// because their scripts belong to the old root.
pub(crate) fn relocate_automations(
    connection: &Connection,
    session_id: CaudraId,
) -> Result<(), SessionError> {
    let session = session_id.as_bytes();
    connection.execute(
        &format!(
            "UPDATE automation_firings SET status = ?2, reason = ?3, finished_ms = {NOW_MS} \
             WHERE session_id = ?1 AND status IN {WAITING_STATUSES}"
        ),
        params![
            session.as_slice(),
            AutomationFiringStatus::Interrupted.as_str(),
            INTERRUPTED_BY_RELOCATION,
        ],
    )?;
    connection.execute(
        &format!(
            "UPDATE automation_bindings SET armed = 0, updated_ms = {NOW_MS} \
             WHERE session_id = ?1 AND scope = ?2 AND armed = 1"
        ),
        params![session.as_slice(), AutomationScope::Project.as_str()],
    )?;
    Ok(())
}

/// Replaces a binding's state when it is still at `expected_revision`.
/// `writer` names the firing and its script digest; `None` is a human edit.
fn write_state(
    connection: &Connection,
    session_id: CaudraId,
    automation: &str,
    expected_revision: u64,
    state: &str,
    writer: Option<(&str, &str)>,
) -> Result<StateWrite, SessionError> {
    bounded_json("automation state", state, MAX_STATE_BYTES)?;
    let session = session_id.as_bytes();
    let changed = connection.execute(
        &format!(
            "UPDATE automation_bindings SET state = ?4, state_revision = state_revision + 1, \
                 state_writer = ?5, state_digest = ?6, state_written_ms = {NOW_MS}, \
                 updated_ms = {NOW_MS} \
             WHERE session_id = ?1 AND automation = ?2 AND state_revision = ?3"
        ),
        params![
            session.as_slice(),
            automation,
            to_i64(expected_revision, "automation_bindings.state_revision")?,
            state,
            writer.map(|(fire_id, _)| fire_id),
            writer.map(|(_, digest)| digest),
        ],
    )?;
    if changed == 1 {
        return Ok(StateWrite::Committed {
            revision: expected_revision + 1,
        });
    }
    let revision: Option<i64> = connection
        .query_row(
            "SELECT state_revision FROM automation_bindings \
             WHERE session_id = ?1 AND automation = ?2",
            params![session.as_slice(), automation],
            |row| row.get(0),
        )
        .optional()?;
    let revision = revision.ok_or_else(|| missing_binding(session_id, automation))?;
    let revision = from_i64(revision, "automation_bindings.state_revision")?;
    tracing::info!(
        session = %session_id,
        automation,
        writer = writer.map(|(fire_id, _)| fire_id),
        expected_revision,
        revision,
        "automation state write lost to a newer revision"
    );
    Ok(StateWrite::Conflict { revision })
}

/// Deletes the automation's finished firings past the newest
/// [`MAX_FINISHED_FIRINGS`]. Pending firings, and finished ones that still
/// have a consumed message to release, never count and never go.
fn prune_finished(
    connection: &Connection,
    session_id: CaudraId,
    automation: &str,
) -> Result<usize, SessionError> {
    Ok(connection.execute(
        &format!(
            "DELETE FROM automation_firings WHERE fire_id IN ( \
                 SELECT fire_id FROM automation_firings \
                 WHERE session_id = ?1 AND automation = ?2 AND status NOT IN {PENDING_STATUSES} \
                     AND delivery IS NULL \
                 ORDER BY queued_ms DESC, rowid DESC LIMIT -1 OFFSET ?3)"
        ),
        params![
            session_id.as_bytes().as_slice(),
            automation,
            to_i64(MAX_FINISHED_FIRINGS, "automation firing retention")?,
        ],
    )?)
}

/// Folds the automation's newest earlier row into the firing that just
/// finished when both are the same quiet skip: same status, trigger and
/// reason, with no actions, error, state change or consumed message.
fn absorb_previous(connection: &Connection, fire_id: &str) -> Result<Option<String>, SessionError> {
    let previous = connection
        .query_row(
            &format!(
                "SELECT previous.fire_id, previous.repeats, \
                     previous.status = current.status \
                     AND previous.trigger_kind = current.trigger_kind \
                     AND previous.trigger_index = current.trigger_index \
                     AND previous.reason IS current.reason AND previous.consumed = 0 \
                     AND previous.error_kind IS NULL AND previous.state_outcome IS NULL \
                     AND previous.state_patch IS NULL \
                     AND NOT EXISTS (SELECT 1 FROM automation_actions AS a \
                         WHERE a.fire_id = previous.fire_id) \
                 FROM automation_firings AS current \
                 JOIN automation_firings AS previous \
                     ON previous.session_id = current.session_id \
                     AND previous.automation = current.automation \
                 WHERE current.fire_id = ?1 AND previous.fire_id != ?1 \
                     AND previous.status NOT IN {PENDING_STATUSES} \
                 ORDER BY previous.queued_ms DESC, previous.rowid DESC LIMIT 1"
            ),
            params![fire_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((previous, repeats, true)) = previous else {
        return Ok(None);
    };
    connection.execute(
        "UPDATE automation_firings SET repeats = repeats + ?2 WHERE fire_id = ?1",
        params![fire_id, repeats],
    )?;
    connection.execute(
        "DELETE FROM automation_firings WHERE fire_id = ?1",
        params![previous],
    )?;
    Ok(Some(previous))
}

fn load_finishing(connection: &Connection, fire_id: &str) -> Result<Finishing, SessionError> {
    let mut statement = connection.prepare(
        "SELECT session_id, automation, digest, status, consumed, bytes \
             + coalesce((SELECT sum(bytes) FROM automation_actions WHERE fire_id = ?1), 0), \
             (SELECT count(*) FROM automation_actions WHERE fire_id = ?1), delivery \
         FROM automation_firings WHERE fire_id = ?1",
    )?;
    let mut rows = statement.query(params![fire_id])?;
    let row = rows.next()?.ok_or_else(|| missing_firing(fire_id))?;
    Ok(Finishing {
        session_id: id_from_row(row, 0)?,
        automation: row.get(1)?,
        digest: row.get(2)?,
        status: parse_column(row, 3, "automation_firings.status")?,
        consumed: row.get(4)?,
        stored: from_i64(row.get(5)?, "automation firing bytes")?,
        actions: from_i64(row.get(6)?, "automation action count")?,
        delivery: row.get(7)?,
    })
}

/// What a firing stores so far, its actions included.
fn firing_bytes(connection: &Connection, fire_id: &str) -> Result<u64, SessionError> {
    let bytes: Option<i64> = connection
        .query_row(
            "SELECT bytes + coalesce((SELECT sum(bytes) FROM automation_actions \
                 WHERE fire_id = ?1), 0) \
             FROM automation_firings WHERE fire_id = ?1",
            params![fire_id],
            |row| row.get(0),
        )
        .optional()?;
    from_i64(
        bytes.ok_or_else(|| missing_firing(fire_id))?,
        "automation firing bytes",
    )
}

/// The cap a body gets in a firing that already stores `stored` bytes: its own
/// maximum while that still fits [`MAX_FIRING_BYTES`], else a preview.
fn body_cap(len: usize, maximum: usize, stored: u64) -> usize {
    let stored = usize::try_from(stored).unwrap_or(usize::MAX);
    if stored.saturating_add(len.min(maximum)) <= MAX_FIRING_BYTES {
        maximum
    } else {
        PREVIEW_BYTES
    }
}

/// A body as stored: whole when it fits `maximum`, else a JSON string holding
/// as much of its text as fits, flagged as cut.
fn stored_body<'a>(
    kind: &'static str,
    body: &'a str,
    maximum: usize,
) -> Result<(Cow<'a, str>, bool), SessionError> {
    if body.len() <= maximum {
        SessionDatabase::validate_payload_json(kind, body)?;
        return Ok((Cow::Borrowed(body), false));
    }
    let mut end = maximum;
    loop {
        let prefix = clipped(body, end);
        let preview = Value::String(prefix.to_owned()).to_string();
        if preview.len() <= maximum {
            return Ok((Cow::Owned(preview), true));
        }
        end = prefix.len() - (preview.len() - maximum).min(prefix.len());
    }
}

/// The longest prefix of `text` within `maximum` bytes that ends on a
/// character boundary.
fn clipped(text: &str, maximum: usize) -> &str {
    let mut end = text.len().min(maximum);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn bounded_identifier(kind: &'static str, value: &str) -> Result<(), SessionError> {
    SessionDatabase::validate_len(kind, value.len(), MAX_IDENTIFIER_BYTES)
}

fn bounded_optional_identifier(
    kind: &'static str,
    value: Option<&str>,
) -> Result<(), SessionError> {
    value.map_or(Ok(()), |value| bounded_identifier(kind, value))
}

fn bounded_json(kind: &'static str, value: &str, maximum: usize) -> Result<(), SessionError> {
    SessionDatabase::validate_len(kind, value.len(), maximum)?;
    SessionDatabase::validate_payload_json(kind, value)
}

fn bounded_optional_json(
    kind: &'static str,
    value: Option<&str>,
    maximum: usize,
) -> Result<(), SessionError> {
    value.map_or(Ok(()), |value| bounded_json(kind, value, maximum))
}

fn binding_changed(
    changed: usize,
    session_id: CaudraId,
    automation: &str,
) -> Result<(), SessionError> {
    if changed == 0 {
        return Err(missing_binding(session_id, automation));
    }
    Ok(())
}

fn missing_binding(session_id: CaudraId, automation: &str) -> SessionError {
    StorageError::NotFound(format!("automation {automation} in session {session_id}")).into()
}

fn missing_firing(fire_id: &str) -> SessionError {
    StorageError::NotFound(format!("automation firing {fire_id}")).into()
}

/// Why a conditional update of a firing changed nothing: it is missing, or
/// it is past the status the update needs.
fn firing_refusal(
    connection: &Connection,
    fire_id: &str,
    message: &'static str,
) -> Result<SessionError, SessionError> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM automation_firings WHERE fire_id = ?1)",
        params![fire_id],
        |row| row.get(0),
    )?;
    Ok(if exists {
        refused(io::ErrorKind::InvalidInput, message)
    } else {
        missing_firing(fire_id)
    })
}

fn action_refusal(
    connection: &Connection,
    fire_id: &str,
    seq: i64,
) -> Result<SessionError, SessionError> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM automation_actions WHERE fire_id = ?1 AND seq = ?2)",
        params![fire_id, seq],
        |row| row.get(0),
    )?;
    Ok(if exists {
        refused(io::ErrorKind::InvalidInput, AUTOMATION_ACTION_FINISHED)
    } else {
        StorageError::NotFound(format!("automation action {fire_id}#{seq}")).into()
    })
}

fn refused(kind: io::ErrorKind, message: &'static str) -> SessionError {
    StorageError::Io(io::Error::new(kind, message)).into()
}

fn read_binding(row: &Row<'_>) -> Result<AutomationBindingRow, SessionError> {
    Ok(AutomationBindingRow {
        session_id: id_from_row(row, 0)?,
        automation: row.get(1)?,
        scope: parse_column(row, 2, "automation_bindings.scope")?,
        origin: parse_column(row, 3, "automation_bindings.origin")?,
        armed: row.get(4)?,
        args: row.get(5)?,
        args_digest: row.get(6)?,
        state: row.get(7)?,
        state_revision: from_i64(row.get(8)?, "automation_bindings.state_revision")?,
        state_writer: row.get(9)?,
        state_digest: row.get(10)?,
        state_written_ms: optional_u64(row, 11, "automation_bindings.state_written_ms")?,
        limiter: row.get(12)?,
        schedule: row.get(13)?,
        work_cursor: row.get(14)?,
        created_ms: from_i64(row.get(15)?, "automation_bindings.created_ms")?,
        updated_ms: from_i64(row.get(16)?, "automation_bindings.updated_ms")?,
        bytes: from_i64(row.get(17)?, "automation_bindings.bytes")?,
    })
}

fn read_summary(row: &Row<'_>) -> Result<AutomationFiringSummary, SessionError> {
    let error_kind: Option<String> = row.get(10)?;
    Ok(AutomationFiringSummary {
        fire_id: row.get(0)?,
        session_id: id_from_row(row, 1)?,
        automation: row.get(2)?,
        digest: row.get(3)?,
        trigger: parse_column(row, 4, "automation_firings.trigger_kind")?,
        trigger_index: row.get(5)?,
        event_key: row.get(6)?,
        consumed: row.get(7)?,
        status: parse_column(row, 8, "automation_firings.status")?,
        reason: row.get(9)?,
        error: error_kind
            .map(|kind| -> Result<FiringError, SessionError> {
                Ok(FiringError {
                    kind,
                    message: row.get::<_, Option<String>>(11)?.unwrap_or_default(),
                    line: row.get(12)?,
                    column: row.get(13)?,
                })
            })
            .transpose()?,
        repeats: from_i64(row.get(14)?, "automation_firings.repeats")?,
        attempts: from_i64(row.get(15)?, "automation_firings.attempts")?,
        operations: from_i64(row.get(16)?, "automation_firings.operations")?,
        state_outcome: parse_optional(row, 17, "automation_firings.state_outcome")?,
        event_cut: row.get(18)?,
        patch_cut: row.get(19)?,
        queued_ms: from_i64(row.get(20)?, "automation_firings.queued_ms")?,
        deferred_until_ms: optional_u64(row, 21, "automation_firings.deferred_until_ms")?,
        started_ms: optional_u64(row, 22, "automation_firings.started_ms")?,
        finished_ms: optional_u64(row, 23, "automation_firings.finished_ms")?,
        bytes: from_i64(row.get(24)?, "automation_firings.bytes")?,
        action_count: from_i64(row.get(25)?, "automation action count")?,
        first_action: parse_optional(row, 26, "automation_actions.kind")?,
    })
}

/// Reads [`FIRING_SUMMARY_COLUMNS`] followed by the event, the state patch and
/// the delivery.
fn read_firing(row: &Row<'_>) -> Result<AutomationFiringRow, SessionError> {
    Ok(AutomationFiringRow {
        summary: read_summary(row)?,
        event: row.get(FIRING_SUMMARY_WIDTH)?,
        state_patch: row.get(FIRING_SUMMARY_WIDTH + 1)?,
        delivery: row.get(FIRING_SUMMARY_WIDTH + 2)?,
    })
}

/// Reads [`ACTION_SUMMARY_COLUMNS`] starting at column `first`.
fn read_action_summary(
    row: &Row<'_>,
    first: usize,
) -> Result<AutomationActionSummary, SessionError> {
    Ok(AutomationActionSummary {
        fire_id: row.get(first)?,
        seq: from_i64(row.get(first + 1)?, "automation_actions.seq")?,
        kind: parse_column(row, first + 2, "automation_actions.kind")?,
        line: row.get(first + 3)?,
        column: row.get(first + 4)?,
        request_hash: row.get(first + 5)?,
        request_cut: row.get(first + 6)?,
        status: parse_column(row, first + 7, "automation_actions.status")?,
        result_cut: row.get(first + 8)?,
        error: row.get(first + 9)?,
        target: row.get(first + 10)?,
        delivery: parse_optional(row, first + 11, "automation_actions.delivery")?,
        expires_ms: optional_u64(row, first + 12, "automation_actions.expires_ms")?,
        turn_outcome: row.get(first + 13)?,
        turn_cost: row.get(first + 14)?,
        started_ms: from_i64(row.get(first + 15)?, "automation_actions.started_ms")?,
        finished_ms: optional_u64(row, first + 16, "automation_actions.finished_ms")?,
        delivered_ms: optional_u64(row, first + 17, "automation_actions.delivered_ms")?,
        bytes: from_i64(row.get(first + 18)?, "automation_actions.bytes")?,
    })
}

/// Reads [`ACTION_SUMMARY_COLUMNS`] from column `first`, followed by the
/// request and result.
fn read_action(row: &Row<'_>, first: usize) -> Result<AutomationActionRow, SessionError> {
    Ok(AutomationActionRow {
        summary: read_action_summary(row, first)?,
        request: row.get(first + ACTION_SUMMARY_WIDTH)?,
        result: row.get(first + ACTION_SUMMARY_WIDTH + 1)?,
    })
}

fn parse_optional<T>(
    row: &Row<'_>,
    index: usize,
    field: &'static str,
) -> Result<Option<T>, SessionError>
where
    T: FromStr<Err = UnknownVariant>,
{
    row.get::<_, Option<String>>(index)?
        .map(|value| {
            value
                .parse()
                .map_err(|error: UnknownVariant| SessionError::CorruptDatabaseValue {
                    field,
                    reason: error.to_string(),
                })
        })
        .transpose()
}

fn optional_u64(
    row: &Row<'_>,
    index: usize,
    field: &'static str,
) -> Result<Option<u64>, SessionError> {
    row.get::<_, Option<i64>>(index)?
        .map(|value| from_i64(value, field))
        .transpose()
}

#[cfg(test)]
mod tests {
    use std::array;
    use std::collections::HashSet;
    use std::fmt::Debug;

    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::StateDir;
    use crate::sessions::{Session, SessionLease, StoredPeerControls, TitleSource};

    const CWD: &str = "/project";
    const MODEL: &str = "test/model";
    const AUTOMATION: &str = "ci-watch";
    const OTHER_AUTOMATION: &str = "keep-going";
    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_DIGEST: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
    const SOURCE: &str = "let meta = #{ triggers: [#{ on: \"idle\" }] };";
    const ARGS: &str = r#"{"branch":"main"}"#;
    const OTHER_ARGS: &str = r#"{"branch":"next"}"#;
    const EVENT: &str = r#"{"kind":"idle","outcome":"done"}"#;
    const STATE: &str = r#"{"seen":1}"#;
    const OTHER_STATE: &str = r#"{"seen":2}"#;
    const PATCH: &str = r#"{"seen":1}"#;
    const LIMITER: &str = r#"{"acted":[1]}"#;
    const REQUEST: &str = r#"{"text":"look at CI"}"#;
    const REQUEST_HASH: &str = "request-hash";
    const SOURCE_LINE: u32 = 3;
    const SOURCE_COLUMN: u32 = 5;
    const FIRE_ID: &str = "fire-1";
    const OTHER_FIRE_ID: &str = "fire-2";
    const PENDING_FIRE_ID: &str = "fire-pending";
    const LAST_FIRE_ID: &str = "fire-last";
    const MISSING_FIRE_ID: &str = "fire-missing";
    const RETAINED_FIRE_ID: &str = "fire-retained";
    const QUIET_FIRE_IDS: [&str; 3] = ["quiet-1", "quiet-2", "quiet-3"];
    const SKIP_REASON: &str = "nothing new";
    const OTHER_REASON: &str = "still running";
    const EVENT_KEY: &str = "delivery-1";
    const DELIVERY: &str = r#"{"delivery":{"message_id":"message-1"}}"#;
    const DOWNGRADED_EVENT: &str = r#"{"kind":"message_received","consumed":false}"#;
    const KEPT_FOR_GOOD: &str = "a completed or skipped firing must forget the delivery it kept";
    const HELD_UNTIL_RELEASED: &str = "a releasing firing must hold the delivery until released";
    const RELEASED_AT_START: &str = "startup must return every delivery still to release";
    const DOWNGRADE_GIVES_UP: &str = "a downgraded firing must neither hold nor own the message";
    const FINISH_HANDS_BACK: &str = "only an ending that does not keep the message hands it back";
    const RELEASE_OUTLIVES_RETENTION: &str =
        "a finished firing must outlive retention until its message is released";
    const TURN_OUTCOME: &str = "done";
    const TURN_COST: f64 = 0.25;
    const HANDLE: &str = "librarian";
    const LATER_TEXT: &str = "later";
    const FORKED_TEXT: &str = "forked";
    const ARMED_TITLE: &str = "armed session";
    const FIRED_TITLE: &str = "fired session";
    const DEFER_UNTIL_MS: u64 = 4_102_444_800_000;
    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    const RECENT_AGES_MS: [i64; 4] = [DAY_MS, 2 * DAY_MS, 3 * DAY_MS, 4 * DAY_MS];
    const INSIDE_WINDOW_MS: i64 = 6 * DAY_MS;
    const OUTSIDE_WINDOW_MS: i64 = 8 * DAY_MS;
    const BUDGET_BODY_BYTES: usize = 60 * 1024;
    const BUDGET_ACTIONS: u64 = 5;
    const ARMING_KEEPS_STATE: &str = "arming again must keep state and marks";
    const STALE_LOSES: &str = "a firing that loaded an older revision must not write state";
    const ONLY_COMPLETION_COMMITS: &str = "only a completing firing may commit state";
    const RUNNING_ACTIONS_INTERRUPTED: &str = "a finished firing must not leave actions running";
    const OUTBOX_SURVIVES: &str = "a queued delivery must outlive the firing that queued it";
    const DELIVERED_ONCE: &str = "a delivered item must leave the outbox for good";
    const RESUME_RESTORES: &str = "resume must keep waiting one-shot events in their order";
    const RESUME_DROPS: &str = "resume must drop waiting events it fires again";
    const RETENTION_KEEPS_NEWEST: &str = "each automation keeps only its newest finished firings";
    const PENDING_NEVER_PRUNED: &str = "a pending firing must never be pruned";
    const CUT_IS_FLAGGED: &str =
        "an oversized body must keep a flagged JSON preview within its cap";
    const BUDGET_PREVIEWS: &str = "past the firing budget a body must keep only a preview";
    const QUIET_MERGES: &str = "a quiet skip merges only into an identical quiet row";
    const TRIM_DROPS_TRACE: &str = "trim must drop every firing, action and source";
    const TRIM_KEEPS_BINDINGS: &str = "trim must keep bindings and their state";
    const BYTES_ARE_ACCOUNTED: &str = "every stored automation byte must be counted";
    const CASCADE: &str = "automation rows must go with their session";
    const REWRITE_KEEPS_ROWS: &str = "saving a session must keep its automation rows";
    const FORK_STARTS_EMPTY: &str = "a forked session must not inherit automation rows";
    const HISTORY_LISTS_OTHERS: &str =
        "history must list other sessions with an armed or a recent automation";
    const HISTORY_WINDOW: &str =
        "history must list a session only while it is armed or fired in the last seven days";
    const NEWEST_FIRST: &str =
        "history must list the newest activity first, from bindings and firings alike";
    const HISTORY_CAPPED: &str = "history must keep only the newest sessions up to its cap";
    const NAMED_ONLINE_OR_NOT: &str =
        "history must name a session by title and handle whether or not it holds a lease";
    const EPHEMERAL_APART: &str =
        "an ephemeral store and the persistent one must never list each other's sessions";
    const NOT_REFUSED: &str = "expected an invalid input refusal";
    const NOT_MISSING: &str = "expected a not found error";

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

    fn arming(session_id: CaudraId, automation: &str) -> AutomationArming {
        AutomationArming {
            session_id,
            automation: automation.into(),
            scope: AutomationScope::User,
            origin: AutomationOrigin::Manual,
            armed: true,
            args: ARGS.into(),
            args_digest: Some(DIGEST.into()),
        }
    }

    fn firing(
        session_id: CaudraId,
        fire_id: &str,
        trigger: AutomationTrigger,
    ) -> NewAutomationFiring {
        NewAutomationFiring {
            fire_id: fire_id.into(),
            session_id,
            automation: AUTOMATION.into(),
            digest: DIGEST.into(),
            trigger,
            trigger_index: 0,
            event: EVENT.into(),
            event_key: None,
            consumed: false,
        }
    }

    /// A message event that consumed its message.
    fn consuming(session_id: CaudraId) -> NewAutomationFiring {
        NewAutomationFiring {
            event_key: Some(EVENT_KEY.into()),
            consumed: true,
            ..firing(session_id, FIRE_ID, AutomationTrigger::MessageReceived)
        }
    }

    fn delivery(database: &SessionDatabase, fire_id: &str) -> Option<String> {
        database
            .load_automation_firing(fire_id)
            .unwrap()
            .unwrap()
            .firing
            .delivery
    }

    fn action(fire_id: &str, seq: u64, status: AutomationActionStatus) -> NewAutomationAction {
        NewAutomationAction {
            fire_id: fire_id.into(),
            seq,
            kind: AutomationActionKind::Message,
            line: Some(SOURCE_LINE),
            column: Some(SOURCE_COLUMN),
            request_hash: REQUEST_HASH.into(),
            request: REQUEST.into(),
            status,
            delivery: Some(AutomationDelivery::Next),
            expires_ms: None,
        }
    }

    fn http(fire_id: &str, seq: u64) -> NewAutomationAction {
        NewAutomationAction {
            kind: AutomationActionKind::Http,
            delivery: None,
            ..action(fire_id, seq, AutomationActionStatus::Running)
        }
    }

    fn done(result: Option<String>) -> AutomationActionEnd {
        AutomationActionEnd {
            status: AutomationActionStatus::Done,
            result,
            error: None,
            target: None,
        }
    }

    fn end(status: AutomationFiringStatus) -> AutomationFiringEnd {
        AutomationFiringEnd {
            status,
            reason: None,
            error: None,
            operations: 0,
            state_patch: None,
            commit: None,
        }
    }

    fn skip(reason: &str) -> AutomationFiringEnd {
        AutomationFiringEnd {
            reason: Some(reason.into()),
            ..end(AutomationFiringStatus::Skipped)
        }
    }

    fn committing(expected_revision: u64, state: &str) -> AutomationFiringEnd {
        AutomationFiringEnd {
            state_patch: Some(PATCH.into()),
            commit: Some(StateCommit {
                expected_revision,
                state: state.into(),
            }),
            ..end(AutomationFiringStatus::Completed)
        }
    }

    fn running(database: &SessionDatabase, firing: &NewAutomationFiring) {
        database.insert_automation_firing(firing).unwrap();
        database.start_automation_firing(&firing.fire_id).unwrap();
    }

    fn summary(database: &SessionDatabase, fire_id: &str) -> AutomationFiringSummary {
        database
            .load_automation_firing(fire_id)
            .unwrap()
            .unwrap()
            .firing
            .summary
    }

    fn binding(database: &SessionDatabase, session_id: CaudraId) -> AutomationBindingRow {
        database
            .load_automation_binding(session_id, AUTOMATION)
            .unwrap()
            .unwrap()
    }

    fn firings(database: &SessionDatabase, session_id: CaudraId) -> Vec<AutomationFiringSummary> {
        database
            .load_automation_firings(session_id, Some(AUTOMATION), MAX_FIRINGS_PER_LOAD)
            .unwrap()
    }

    fn other_session(database: &mut SessionDatabase) -> CaudraId {
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        session.id
    }

    /// Arms `AUTOMATION` as if its binding last changed `age_ms` ago, and
    /// returns that time.
    fn arm_ago(database: &SessionDatabase, session_id: CaudraId, age_ms: i64) -> u64 {
        database
            .upsert_automation_binding(&arming(session_id, AUTOMATION))
            .unwrap();
        database
            .connection()
            .execute(
                &format!(
                    "UPDATE automation_bindings SET updated_ms = {NOW_MS} - ?2 \
                     WHERE session_id = ?1"
                ),
                params![session_id.as_bytes().as_slice(), age_ms],
            )
            .unwrap();
        binding(database, session_id).updated_ms
    }

    /// Queues a firing as if it were queued `age_ms` ago, and returns that time.
    fn fire_ago(database: &SessionDatabase, session_id: CaudraId, age_ms: i64) -> u64 {
        let fire_id = session_id.to_string();
        database
            .insert_automation_firing(&firing(session_id, &fire_id, AutomationTrigger::Idle))
            .unwrap();
        database
            .connection()
            .execute(
                &format!(
                    "UPDATE automation_firings SET queued_ms = {NOW_MS} - ?2 WHERE fire_id = ?1"
                ),
                params![fire_id, age_ms],
            )
            .unwrap();
        summary(database, &fire_id).queued_ms
    }

    fn activity(history: &[AutomationSessionRow]) -> Vec<(CaudraId, u64)> {
        history
            .iter()
            .map(|row| (row.session_id, row.last_activity_ms))
            .collect()
    }

    fn refusal<T: Debug>(result: Result<T, SessionError>) -> String {
        match result {
            Err(SessionError::Storage(StorageError::Io(error)))
                if error.kind() == io::ErrorKind::InvalidInput =>
            {
                error.to_string()
            }
            other => panic!("{NOT_REFUSED}: {other:?}"),
        }
    }

    fn is_missing<T>(result: Result<T, SessionError>) -> bool {
        matches!(
            result,
            Err(SessionError::Storage(StorageError::NotFound(_)))
        )
    }

    #[test]
    fn arming_again_keeps_state_and_marks() {
        let (_temp, _state_dir, database, session_id) = open();
        database
            .upsert_automation_binding(&arming(session_id, AUTOMATION))
            .unwrap();
        database
            .commit_automation_state(session_id, AUTOMATION, 0, STATE)
            .unwrap();
        database
            .save_automation_marks(
                session_id,
                AUTOMATION,
                &AutomationMarks {
                    limiter: Some(LIMITER.into()),
                    ..AutomationMarks::default()
                },
            )
            .unwrap();

        database
            .upsert_automation_binding(&AutomationArming {
                origin: AutomationOrigin::Cli,
                args: OTHER_ARGS.into(),
                ..arming(session_id, AUTOMATION)
            })
            .unwrap();
        database.disarm_automation(session_id, AUTOMATION).unwrap();

        let rearmed = binding(&database, session_id);
        assert_eq!(
            (rearmed.origin, rearmed.armed, rearmed.args.as_str()),
            (AutomationOrigin::Cli, false, OTHER_ARGS)
        );
        assert_eq!(
            (rearmed.state.as_str(), rearmed.state_revision),
            (STATE, 1),
            "{ARMING_KEEPS_STATE}"
        );
        assert_eq!(rearmed.limiter, LIMITER, "{ARMING_KEEPS_STATE}");
        assert_eq!(rearmed.schedule, EMPTY_OBJECT);
        assert!(
            is_missing(database.arm_automation(
                session_id,
                OTHER_AUTOMATION,
                AutomationOrigin::Manual
            )),
            "{NOT_MISSING}"
        );
    }

    #[test_case(true; "clear")]
    #[test_case(false; "edit")]
    fn a_firing_that_raced_a_human_change_commits_nothing(clear: bool) {
        let (_temp, _state_dir, database, session_id) = open();
        database
            .upsert_automation_binding(&arming(session_id, AUTOMATION))
            .unwrap();
        running(
            &database,
            &firing(session_id, FIRE_ID, AutomationTrigger::Idle),
        );
        let first = database
            .finish_automation_firing(FIRE_ID, &committing(0, STATE))
            .unwrap();
        let written = binding(&database, session_id);
        running(
            &database,
            &firing(session_id, OTHER_FIRE_ID, AutomationTrigger::Idle),
        );

        let (revision, human_state) = if clear {
            (
                database
                    .clear_automation_state(session_id, AUTOMATION)
                    .unwrap(),
                EMPTY_OBJECT,
            )
        } else {
            let edit = database
                .commit_automation_state(session_id, AUTOMATION, 1, OTHER_STATE)
                .unwrap();
            assert_eq!(edit, StateWrite::Committed { revision: 2 });
            (2, OTHER_STATE)
        };
        let racing = database
            .finish_automation_firing(OTHER_FIRE_ID, &committing(1, STATE))
            .unwrap();

        assert_eq!(first.state, Some(StateWrite::Committed { revision: 1 }));
        assert_eq!(
            (
                written.state_writer.as_deref(),
                written.state_digest.as_deref()
            ),
            (Some(FIRE_ID), Some(DIGEST))
        );
        assert_eq!(
            racing.state,
            Some(StateWrite::Conflict { revision }),
            "{STALE_LOSES}"
        );
        let after = binding(&database, session_id);
        assert_eq!(
            (
                after.state.as_str(),
                after.state_revision,
                after.state_writer
            ),
            (human_state, revision, None),
            "{STALE_LOSES}"
        );
        let lost = summary(&database, OTHER_FIRE_ID);
        assert_eq!(
            (lost.status, lost.state_outcome),
            (
                AutomationFiringStatus::Completed,
                Some(AutomationStateOutcome::Conflict)
            ),
            "{STALE_LOSES}"
        );
        assert_eq!(
            summary(&database, FIRE_ID).state_outcome,
            Some(AutomationStateOutcome::Committed)
        );
    }

    #[test_case(AutomationFiringStatus::Completed, true; "completed")]
    #[test_case(AutomationFiringStatus::Skipped, true; "skipped")]
    #[test_case(AutomationFiringStatus::Released, true; "released")]
    #[test_case(AutomationFiringStatus::Failed, false; "failed")]
    #[test_case(AutomationFiringStatus::Cancelled, false; "cancelled")]
    #[test_case(AutomationFiringStatus::RateLimited, false; "rate_limited")]
    #[test_case(AutomationFiringStatus::Paused, false; "paused")]
    fn only_a_completing_firing_commits(status: AutomationFiringStatus, commits: bool) {
        let (_temp, _state_dir, database, session_id) = open();
        database
            .upsert_automation_binding(&arming(session_id, AUTOMATION))
            .unwrap();
        running(
            &database,
            &firing(session_id, FIRE_ID, AutomationTrigger::Idle),
        );

        let finished = database.finish_automation_firing(
            FIRE_ID,
            &AutomationFiringEnd {
                status,
                ..committing(0, STATE)
            },
        );

        let stored = binding(&database, session_id);
        if commits {
            assert_eq!(
                finished.unwrap().state,
                Some(StateWrite::Committed { revision: 1 })
            );
            assert_eq!(stored.state, STATE);
            assert_eq!(summary(&database, FIRE_ID).status, status);
        } else {
            assert_eq!(
                refusal(finished),
                AUTOMATION_COMMIT_INCOMPLETE,
                "{ONLY_COMPLETION_COMMITS}"
            );
            assert_eq!(
                (stored.state.as_str(), stored.state_revision),
                (EMPTY_OBJECT, 0),
                "{ONLY_COMPLETION_COMMITS}"
            );
            assert_eq!(
                summary(&database, FIRE_ID).status,
                AutomationFiringStatus::Running
            );
        }
    }

    #[test]
    fn a_firing_only_moves_forward() {
        let (_temp, _state_dir, database, session_id) = open();
        database
            .insert_automation_firing(&firing(
                session_id,
                FIRE_ID,
                AutomationTrigger::MessageReceived,
            ))
            .unwrap();

        let attempts = database
            .defer_automation_firing(FIRE_ID, DEFER_UNTIL_MS)
            .unwrap();
        let deferred = summary(&database, FIRE_ID);
        database.start_automation_firing(FIRE_ID).unwrap();
        let restarted = refusal(database.start_automation_firing(FIRE_ID));
        let unfinished = refusal(
            database.finish_automation_firing(FIRE_ID, &end(AutomationFiringStatus::Queued)),
        );
        database
            .finish_automation_firing(FIRE_ID, &end(AutomationFiringStatus::Failed))
            .unwrap();

        assert_eq!(attempts, 1);
        assert_eq!(
            (deferred.status, deferred.deferred_until_ms),
            (AutomationFiringStatus::Deferred, Some(DEFER_UNTIL_MS))
        );
        assert_eq!(restarted, AUTOMATION_FIRING_NOT_WAITING);
        assert_eq!(unfinished, AUTOMATION_FIRING_END_PENDING);
        assert_eq!(
            refusal(
                database.finish_automation_firing(FIRE_ID, &end(AutomationFiringStatus::Failed))
            ),
            AUTOMATION_FIRING_FINISHED
        );
        assert_eq!(
            refusal(database.defer_automation_firing(FIRE_ID, DEFER_UNTIL_MS)),
            AUTOMATION_FIRING_FINISHED
        );
        assert!(
            is_missing(database.start_automation_firing(MISSING_FIRE_ID)),
            "{NOT_MISSING}"
        );
    }

    #[test]
    fn finishing_interrupts_running_actions_and_keeps_queued_deliveries() {
        let (_temp, _state_dir, database, session_id) = open();
        running(
            &database,
            &firing(session_id, FIRE_ID, AutomationTrigger::Idle),
        );
        database
            .start_automation_action(&action(FIRE_ID, 0, AutomationActionStatus::Queued))
            .unwrap();
        database.start_automation_action(&http(FIRE_ID, 1)).unwrap();

        database
            .finish_automation_firing(FIRE_ID, &end(AutomationFiringStatus::Failed))
            .unwrap();

        let detail = database.load_automation_firing(FIRE_ID).unwrap().unwrap();
        assert_eq!(
            detail
                .actions
                .iter()
                .map(|action| action.status)
                .collect::<Vec<_>>(),
            [
                AutomationActionStatus::Queued,
                AutomationActionStatus::Interrupted
            ],
            "{RUNNING_ACTIONS_INTERRUPTED}"
        );
        assert_eq!(
            (
                detail.firing.summary.action_count,
                detail.firing.summary.first_action
            ),
            (2, Some(AutomationActionKind::Message))
        );
        let outbox = database.load_automation_outbox(session_id).unwrap();
        assert_eq!(
            outbox
                .iter()
                .map(|item| (item.automation.as_str(), item.action.request.as_str()))
                .collect::<Vec<_>>(),
            [(AUTOMATION, REQUEST)],
            "{OUTBOX_SURVIVES}"
        );
    }

    #[test]
    fn a_delivery_leaves_the_outbox_once_and_records_its_turn() {
        let (_temp, _state_dir, database, session_id) = open();
        running(
            &database,
            &firing(session_id, FIRE_ID, AutomationTrigger::Idle),
        );
        database
            .start_automation_action(&action(FIRE_ID, 0, AutomationActionStatus::Queued))
            .unwrap();
        database
            .finish_automation_firing(FIRE_ID, &end(AutomationFiringStatus::Completed))
            .unwrap();

        let delivered = database.mark_automation_delivered(FIRE_ID, 0).unwrap();
        let again = database.mark_automation_delivered(FIRE_ID, 0).unwrap();
        let recorded = database
            .record_automation_turn(&[(FIRE_ID.into(), 0)], TURN_OUTCOME, Some(TURN_COST))
            .unwrap();

        assert!(delivered);
        assert!(!again, "{DELIVERED_ONCE}");
        assert!(
            database
                .load_automation_outbox(session_id)
                .unwrap()
                .is_empty(),
            "{DELIVERED_ONCE}"
        );
        assert_eq!(recorded, 1);
        let item = database
            .load_automation_action(FIRE_ID, 0)
            .unwrap()
            .unwrap()
            .summary;
        assert_eq!(
            (item.status, item.turn_outcome.as_deref(), item.turn_cost),
            (
                AutomationActionStatus::Delivered,
                Some(TURN_OUTCOME),
                Some(TURN_COST)
            )
        );
        assert!(item.delivered_ms.is_some());
        assert_eq!(
            refusal(database.finish_automation_action(
                FIRE_ID,
                0,
                &AutomationActionEnd {
                    status: AutomationActionStatus::Dropped,
                    ..done(None)
                },
            )),
            AUTOMATION_ACTION_FINISHED,
            "{DELIVERED_ONCE}"
        );
    }

    #[test]
    fn startup_interrupts_running_firings_and_keeps_restorable_events() {
        const WAITING: [AutomationTrigger; 8] = [
            AutomationTrigger::Armed,
            AutomationTrigger::MessageReceived,
            AutomationTrigger::Idle,
            AutomationTrigger::GoalFinished,
            AutomationTrigger::NeedsInput,
            AutomationTrigger::WorkFinished,
            AutomationTrigger::Schedule,
            AutomationTrigger::WorkflowFinished,
        ];
        let (_temp, _state_dir, database, session_id) = open();
        running(
            &database,
            &firing(session_id, FIRE_ID, AutomationTrigger::Idle),
        );
        database
            .start_automation_action(&action(FIRE_ID, 0, AutomationActionStatus::Queued))
            .unwrap();
        database.start_automation_action(&http(FIRE_ID, 1)).unwrap();
        for trigger in WAITING {
            database
                .insert_automation_firing(&firing(session_id, trigger.as_str(), trigger))
                .unwrap();
        }
        database
            .defer_automation_firing(AutomationTrigger::MessageReceived.as_str(), DEFER_UNTIL_MS)
            .unwrap();

        let startup = database.interrupt_automation_firings(session_id).unwrap();

        assert_eq!(
            startup,
            AutomationStartup {
                interrupted: 1,
                dropped: 4,
                releases: Vec::new(),
            }
        );
        let restored = database
            .load_pending_automation_firings(session_id)
            .unwrap();
        assert_eq!(
            restored
                .iter()
                .map(|firing| firing.summary.trigger)
                .collect::<Vec<_>>(),
            [
                AutomationTrigger::MessageReceived,
                AutomationTrigger::GoalFinished,
                AutomationTrigger::WorkFinished,
                AutomationTrigger::WorkflowFinished
            ],
            "{RESUME_RESTORES}"
        );
        assert!(
            restored.iter().all(|firing| firing.event == EVENT),
            "{RESUME_RESTORES}"
        );
        for trigger in [
            AutomationTrigger::Armed,
            AutomationTrigger::Idle,
            AutomationTrigger::NeedsInput,
            AutomationTrigger::Schedule,
        ] {
            let dropped = summary(&database, trigger.as_str());
            assert_eq!(
                (dropped.status, dropped.reason.as_deref()),
                (AutomationFiringStatus::Dropped, Some(SUPERSEDED_ON_RESUME)),
                "{RESUME_DROPS}"
            );
        }
        let interrupted = database.load_automation_firing(FIRE_ID).unwrap().unwrap();
        assert_eq!(
            interrupted.firing.summary.status,
            AutomationFiringStatus::Interrupted
        );
        assert_eq!(
            interrupted
                .actions
                .iter()
                .map(|action| action.status)
                .collect::<Vec<_>>(),
            [
                AutomationActionStatus::Queued,
                AutomationActionStatus::Interrupted
            ],
            "{RUNNING_ACTIONS_INTERRUPTED}"
        );
        assert_eq!(
            database.load_automation_outbox(session_id).unwrap().len(),
            1,
            "{OUTBOX_SURVIVES}"
        );
    }

    #[test]
    fn retention_keeps_the_newest_finished_firings_and_every_pending_one() {
        let (_temp, _state_dir, database, session_id) = open();
        let fire_id = |index: usize| format!("{RETAINED_FIRE_ID}-{index}");
        database
            .insert_automation_firing(&firing(
                session_id,
                PENDING_FIRE_ID,
                AutomationTrigger::MessageReceived,
            ))
            .unwrap();
        running(
            &database,
            &NewAutomationFiring {
                automation: OTHER_AUTOMATION.into(),
                ..firing(session_id, OTHER_FIRE_ID, AutomationTrigger::Idle)
            },
        );
        database
            .finish_automation_firing(OTHER_FIRE_ID, &end(AutomationFiringStatus::Failed))
            .unwrap();
        for index in 0..=MAX_FINISHED_FIRINGS {
            running(
                &database,
                &firing(session_id, &fire_id(index), AutomationTrigger::Idle),
            );
            database
                .finish_automation_firing(&fire_id(index), &end(AutomationFiringStatus::Failed))
                .unwrap();
        }

        database
            .insert_automation_firing(&firing(session_id, LAST_FIRE_ID, AutomationTrigger::Idle))
            .unwrap();

        let kept = firings(&database, session_id);
        let finished: HashSet<_> = kept
            .iter()
            .filter(|firing| !firing.status.is_pending())
            .map(|firing| firing.fire_id.clone())
            .collect();
        assert_eq!(
            finished.len(),
            MAX_FINISHED_FIRINGS,
            "{RETENTION_KEEPS_NEWEST}"
        );
        assert!(!finished.contains(&fire_id(0)), "{RETENTION_KEEPS_NEWEST}");
        assert!(
            finished.contains(&fire_id(MAX_FINISHED_FIRINGS)),
            "{RETENTION_KEEPS_NEWEST}"
        );
        assert!(
            kept.iter().any(|firing| firing.fire_id == PENDING_FIRE_ID),
            "{PENDING_NEVER_PRUNED}"
        );
        assert_eq!(
            database
                .load_automation_firings(session_id, Some(OTHER_AUTOMATION), MAX_FIRINGS_PER_LOAD)
                .unwrap()
                .len(),
            1,
            "{RETENTION_KEEPS_NEWEST}"
        );
    }

    #[test]
    fn oversized_bodies_keep_a_flagged_json_preview_within_their_cap() {
        let (_temp, _state_dir, database, session_id) = open();
        let oversized = |maximum: usize| json!({ "text": "é\"".repeat(maximum) }).to_string();
        running(
            &database,
            &NewAutomationFiring {
                event: oversized(MAX_EVENT_BYTES),
                ..firing(session_id, FIRE_ID, AutomationTrigger::Idle)
            },
        );
        database
            .start_automation_action(&NewAutomationAction {
                request: oversized(MAX_REQUEST_BYTES),
                ..http(FIRE_ID, 0)
            })
            .unwrap();
        database
            .finish_automation_action(FIRE_ID, 0, &done(Some(oversized(MAX_RESULT_BYTES))))
            .unwrap();

        let detail = database.load_automation_firing(FIRE_ID).unwrap().unwrap();
        let action = database
            .load_automation_action(FIRE_ID, 0)
            .unwrap()
            .unwrap();
        let result = action.result.unwrap_or_default();
        for (body, cut, maximum) in [
            (
                detail.firing.event.as_str(),
                detail.firing.summary.event_cut,
                MAX_EVENT_BYTES,
            ),
            (
                action.request.as_str(),
                action.summary.request_cut,
                MAX_REQUEST_BYTES,
            ),
            (result.as_str(), action.summary.result_cut, MAX_RESULT_BYTES),
        ] {
            assert!(cut, "{CUT_IS_FLAGGED}");
            assert!(
                body.len() <= maximum && body.len() * 2 > maximum,
                "{CUT_IS_FLAGGED}"
            );
            assert!(
                serde_json::from_str::<Value>(body).unwrap().is_string(),
                "{CUT_IS_FLAGGED}"
            );
        }
    }

    #[test]
    fn a_firing_past_its_byte_budget_keeps_previews() {
        let (_temp, _state_dir, database, session_id) = open();
        let body = Value::String("x".repeat(BUDGET_BODY_BYTES)).to_string();
        running(
            &database,
            &NewAutomationFiring {
                event: body.clone(),
                ..firing(session_id, FIRE_ID, AutomationTrigger::Idle)
            },
        );
        for seq in 0..BUDGET_ACTIONS {
            database
                .start_automation_action(&http(FIRE_ID, seq))
                .unwrap();
            database
                .finish_automation_action(FIRE_ID, seq, &done(Some(body.clone())))
                .unwrap();
        }

        let event_bytes = summary(&database, FIRE_ID).bytes;
        let actions: Vec<_> = (0..BUDGET_ACTIONS)
            .map(|seq| {
                database
                    .load_automation_action(FIRE_ID, seq)
                    .unwrap()
                    .unwrap()
            })
            .collect();
        let whole = actions
            .iter()
            .take_while(|action| !action.summary.result_cut)
            .count();
        assert!(whole > 0 && whole < actions.len(), "{BUDGET_PREVIEWS}");
        assert!(
            event_bytes
                + actions[..whole]
                    .iter()
                    .map(|action| action.summary.bytes)
                    .sum::<u64>()
                <= MAX_FIRING_BYTES as u64,
            "{BUDGET_PREVIEWS}"
        );
        assert!(
            actions[whole..]
                .iter()
                .all(|action| action.summary.result_cut
                    && action
                        .result
                        .as_ref()
                        .is_some_and(|result| result.len() <= PREVIEW_BYTES)),
            "{BUDGET_PREVIEWS}"
        );
    }

    #[test_case(SKIP_REASON, false, false, true; "identical_quiet_skip")]
    #[test_case(OTHER_REASON, false, false, false; "different_reason")]
    #[test_case(SKIP_REASON, true, false, false; "consumed_message")]
    #[test_case(SKIP_REASON, false, true, false; "logging_firing")]
    fn a_quiet_skip_merges_only_into_an_identical_quiet_row(
        reason: &str,
        consumed: bool,
        logs: bool,
        merges: bool,
    ) {
        let (_temp, _state_dir, database, session_id) = open();
        running(
            &database,
            &firing(session_id, FIRE_ID, AutomationTrigger::Idle),
        );
        database
            .finish_automation_firing(FIRE_ID, &skip(SKIP_REASON))
            .unwrap();
        running(
            &database,
            &NewAutomationFiring {
                consumed,
                ..firing(session_id, OTHER_FIRE_ID, AutomationTrigger::Idle)
            },
        );
        if logs {
            database
                .start_automation_action(&NewAutomationAction {
                    kind: AutomationActionKind::Log,
                    delivery: None,
                    ..action(OTHER_FIRE_ID, 0, AutomationActionStatus::Done)
                })
                .unwrap();
        }

        let finished = database
            .finish_automation_firing(OTHER_FIRE_ID, &skip(reason))
            .unwrap();

        let rows = firings(&database, session_id);
        assert_eq!(
            finished.absorbed.as_deref(),
            merges.then_some(FIRE_ID),
            "{QUIET_MERGES}"
        );
        assert_eq!(
            rows.iter()
                .map(|row| (row.fire_id.as_str(), row.repeats))
                .collect::<Vec<_>>(),
            if merges {
                vec![(OTHER_FIRE_ID, 2)]
            } else {
                vec![(OTHER_FIRE_ID, 1), (FIRE_ID, 1)]
            },
            "{QUIET_MERGES}"
        );
    }

    #[test]
    fn repeated_quiet_skips_collapse_into_the_newest_row() {
        let (_temp, _state_dir, database, session_id) = open();
        let event = |index: usize| json!({ "index": index }).to_string();
        for (index, fire_id) in QUIET_FIRE_IDS.into_iter().enumerate() {
            running(
                &database,
                &NewAutomationFiring {
                    event: event(index),
                    ..firing(session_id, fire_id, AutomationTrigger::Idle)
                },
            );
            database
                .finish_automation_firing(fire_id, &skip(SKIP_REASON))
                .unwrap();
        }

        let rows = firings(&database, session_id);
        let newest = QUIET_FIRE_IDS.len() - 1;

        assert_eq!(
            rows.iter()
                .map(|row| (row.fire_id.as_str(), row.repeats))
                .collect::<Vec<_>>(),
            [(QUIET_FIRE_IDS[newest], QUIET_FIRE_IDS.len() as u64)],
            "{QUIET_MERGES}"
        );
        assert_eq!(
            database
                .load_automation_firing(QUIET_FIRE_IDS[newest])
                .unwrap()
                .unwrap()
                .firing
                .event,
            event(newest),
            "{QUIET_MERGES}"
        );
    }

    #[test]
    fn trim_drops_the_trace_and_keeps_bindings_and_state() {
        let (_temp, state_dir, mut database, session_id) = open();
        database
            .upsert_automation_binding(&arming(session_id, AUTOMATION))
            .unwrap();
        database
            .insert_automation_source(session_id, DIGEST, SOURCE)
            .unwrap();
        running(
            &database,
            &firing(session_id, FIRE_ID, AutomationTrigger::Idle),
        );
        database.start_automation_action(&http(FIRE_ID, 0)).unwrap();
        database
            .finish_automation_firing(FIRE_ID, &committing(0, STATE))
            .unwrap();
        database
            .insert_automation_firing(&firing(
                session_id,
                OTHER_FIRE_ID,
                AutomationTrigger::MessageReceived,
            ))
            .unwrap();
        let traced =
            database.automation_bytes(session_id).unwrap() - binding(&database, session_id).bytes;
        let lease = SessionLease::acquire(&state_dir, session_id).unwrap();

        let report = database.trim(&lease).unwrap();

        assert_eq!(
            (
                report.automation_firing_rows,
                report.automation_action_rows,
                report.automation_source_rows
            ),
            (2, 1, 1),
            "{TRIM_DROPS_TRACE}"
        );
        assert_eq!(report.automation_bytes, traced, "{BYTES_ARE_ACCOUNTED}");
        assert!(
            database
                .load_automation_firings(session_id, None, MAX_FIRINGS_PER_LOAD)
                .unwrap()
                .is_empty(),
            "{TRIM_DROPS_TRACE}"
        );
        assert_eq!(
            database.load_automation_source(session_id, DIGEST).unwrap(),
            None,
            "{TRIM_DROPS_TRACE}"
        );
        let kept = binding(&database, session_id);
        assert_eq!(
            (kept.state.as_str(), kept.armed),
            (STATE, true),
            "{TRIM_KEEPS_BINDINGS}"
        );
        assert_eq!(
            database.automation_bytes(session_id).unwrap(),
            kept.bytes,
            "{BYTES_ARE_ACCOUNTED}"
        );
    }

    #[test]
    fn bytes_are_accounted_in_facts_and_stats() {
        let (_temp, _state_dir, database, session_id) = open();
        database
            .upsert_automation_binding(&arming(session_id, AUTOMATION))
            .unwrap();
        database
            .insert_automation_source(session_id, DIGEST, SOURCE)
            .unwrap();
        running(
            &database,
            &firing(session_id, FIRE_ID, AutomationTrigger::Idle),
        );
        database
            .start_automation_action(&action(FIRE_ID, 0, AutomationActionStatus::Queued))
            .unwrap();
        let detail = database.load_automation_firing(FIRE_ID).unwrap().unwrap();
        let source_bytes = (DIGEST.len() + SOURCE.len()) as u64;

        let bytes = database.automation_bytes(session_id).unwrap();
        let stats = database.stats().unwrap();
        let facts = database.session_facts(None).unwrap();

        assert_eq!(
            bytes,
            binding(&database, session_id).bytes
                + detail.firing.summary.bytes
                + detail.actions[0].bytes
                + source_bytes,
            "{BYTES_ARE_ACCOUNTED}"
        );
        assert_eq!(
            (stats.automation_firing_count, stats.automation_action_count),
            (1, 1)
        );
        assert_eq!(stats.automation_bytes, bytes, "{BYTES_ARE_ACCOUNTED}");
        assert_eq!(
            facts[0].logical_bytes,
            stats.logical_bytes + bytes,
            "{BYTES_ARE_ACCOUNTED}"
        );
        assert_eq!(database.automation_bytes(CaudraId::generate()).unwrap(), 0);
    }

    #[test]
    fn deleting_a_session_removes_its_automation_rows() {
        let (_temp, _state_dir, mut database, session_id) = open();
        database
            .upsert_automation_binding(&arming(session_id, AUTOMATION))
            .unwrap();
        database
            .insert_automation_source(session_id, DIGEST, SOURCE)
            .unwrap();
        running(
            &database,
            &firing(session_id, FIRE_ID, AutomationTrigger::Idle),
        );
        database.start_automation_action(&http(FIRE_ID, 0)).unwrap();

        database.delete(session_id, None).unwrap();

        assert!(
            database
                .load_automation_binding(session_id, AUTOMATION)
                .unwrap()
                .is_none(),
            "{CASCADE}"
        );
        assert!(
            database.load_automation_firing(FIRE_ID).unwrap().is_none(),
            "{CASCADE}"
        );
        let stats = database.stats().unwrap();
        assert_eq!(
            (
                stats.automation_firing_count,
                stats.automation_action_count,
                stats.automation_bytes
            ),
            (0, 0, 0),
            "{CASCADE}"
        );
    }

    #[test]
    fn a_rewritten_or_forked_session_keeps_automation_rows_apart() {
        let (_temp, _state_dir, mut database, session_id) = open();
        database
            .upsert_automation_binding(&arming(session_id, AUTOMATION))
            .unwrap();
        running(
            &database,
            &firing(session_id, FIRE_ID, AutomationTrigger::Idle),
        );
        database.start_automation_action(&http(FIRE_ID, 0)).unwrap();
        let mut session = database
            .load::<TestMessage, Value, Value>(session_id)
            .unwrap();
        session.push_message(TestMessage(LATER_TEXT.into()));
        database.save(&session, None).unwrap();
        let mut fork = TestSession::new(MODEL, CWD);
        fork.push_message(TestMessage(FORKED_TEXT.into()));
        database.save(&fork, None).unwrap();

        let kept = database.load_automation_firing(FIRE_ID).unwrap().unwrap();
        assert_eq!(kept.actions.len(), 1, "{REWRITE_KEEPS_ROWS}");
        assert!(
            database
                .load_automation_binding(session_id, AUTOMATION)
                .unwrap()
                .is_some(),
            "{REWRITE_KEEPS_ROWS}"
        );
        assert!(
            database
                .load_automation_bindings(fork.id)
                .unwrap()
                .is_empty(),
            "{FORK_STARTS_EMPTY}"
        );
        assert!(
            firings(&database, fork.id).is_empty(),
            "{FORK_STARTS_EMPTY}"
        );
    }

    #[test]
    fn history_lists_other_sessions_with_armed_or_recent_automations() {
        let (_temp, _state_dir, mut database, session_id) = open();
        let mut armed = TestSession::new(MODEL, CWD);
        armed.push_message(TestMessage(ARMED_TITLE.into()));
        armed.meta.peer_controls = Some(StoredPeerControls {
            handle: Some(HANDLE.into()),
            ..StoredPeerControls::default()
        });
        database.save(&armed, None).unwrap();
        let mut fired = TestSession::new(MODEL, CWD);
        fired.push_message(TestMessage(FIRED_TITLE.into()));
        database.save(&fired, None).unwrap();
        let disarmed = TestSession::new(MODEL, CWD);
        database.save(&disarmed, None).unwrap();
        database
            .upsert_automation_binding(&arming(session_id, AUTOMATION))
            .unwrap();
        database
            .upsert_automation_binding(&arming(armed.id, AUTOMATION))
            .unwrap();
        database
            .upsert_automation_binding(&AutomationArming {
                armed: false,
                ..arming(disarmed.id, AUTOMATION)
            })
            .unwrap();
        database
            .insert_automation_firing(&firing(fired.id, FIRE_ID, AutomationTrigger::Idle))
            .unwrap();

        let history = database
            .load_automation_history(session_id, MAX_HISTORY_SESSIONS)
            .unwrap();
        let limited = database.load_automation_history(session_id, 1).unwrap();
        let traces = database
            .load_automation_firings(fired.id, None, MAX_FIRINGS_PER_LOAD)
            .unwrap();

        assert_eq!(
            history
                .iter()
                .map(|row| row.session_id)
                .collect::<HashSet<_>>(),
            HashSet::from([armed.id, fired.id]),
            "{HISTORY_LISTS_OTHERS}"
        );
        let listed = history
            .iter()
            .find(|row| row.session_id == armed.id)
            .expect(HISTORY_LISTS_OTHERS);
        assert_eq!(
            (listed.title.as_str(), listed.handle.as_deref()),
            (armed.title.as_str(), Some(HANDLE)),
            "{HISTORY_LISTS_OTHERS}"
        );
        assert_eq!(limited.len(), 1);
        assert_eq!(
            traces
                .iter()
                .map(|firing| firing.fire_id.as_str())
                .collect::<Vec<_>>(),
            [FIRE_ID],
            "{HISTORY_LISTS_OTHERS}"
        );
    }

    #[test_case(fire_ago, INSIDE_WINDOW_MS, true; "fired_inside_the_window")]
    #[test_case(fire_ago, OUTSIDE_WINDOW_MS, false; "fired_outside_the_window")]
    #[test_case(arm_ago, OUTSIDE_WINDOW_MS, true; "armed_outside_the_window")]
    fn history_lists_a_session_armed_or_fired_within_the_window(
        seed: fn(&SessionDatabase, CaudraId, i64) -> u64,
        age_ms: i64,
        listed: bool,
    ) {
        let (_temp, _state_dir, mut database, session_id) = open();
        let other = other_session(&mut database);
        let activity_ms = seed(&database, other, age_ms);

        let history = database
            .load_automation_history(session_id, MAX_HISTORY_SESSIONS)
            .unwrap();

        assert_eq!(
            activity(&history),
            listed
                .then_some((other, activity_ms))
                .into_iter()
                .collect::<Vec<_>>(),
            "{HISTORY_WINDOW}"
        );
    }

    #[test]
    fn history_lists_the_newest_activity_first_from_bindings_and_firings() {
        let (_temp, _state_dir, mut database, session_id) = open();
        let [binding_newer, firing_newer, armed, fired] =
            array::from_fn(|_| other_session(&mut database));
        let [newest, newer, older, oldest] = RECENT_AGES_MS;
        let armed_ms = arm_ago(&database, armed, newest);
        let fired_ms = fire_ago(&database, fired, newer);
        arm_ago(&database, firing_newer, OUTSIDE_WINDOW_MS);
        let firing_ms = fire_ago(&database, firing_newer, older);
        fire_ago(&database, binding_newer, OUTSIDE_WINDOW_MS);
        let binding_ms = arm_ago(&database, binding_newer, oldest);

        let history = database
            .load_automation_history(session_id, MAX_HISTORY_SESSIONS)
            .unwrap();

        assert_eq!(
            activity(&history),
            [
                (armed, armed_ms),
                (fired, fired_ms),
                (firing_newer, firing_ms),
                (binding_newer, binding_ms),
            ],
            "{NEWEST_FIRST}"
        );
    }

    #[test]
    fn history_keeps_the_newest_sessions_up_to_its_cap() {
        let (_temp, _state_dir, mut database, session_id) = open();
        let oldest = other_session(&mut database);
        arm_ago(&database, oldest, DAY_MS);
        for _ in 0..MAX_HISTORY_SESSIONS {
            let other = other_session(&mut database);
            database
                .upsert_automation_binding(&arming(other, AUTOMATION))
                .unwrap();
        }

        let history = database
            .load_automation_history(session_id, MAX_HISTORY_SESSIONS + 1)
            .unwrap();

        assert_eq!(
            (
                history.len(),
                history.iter().any(|row| row.session_id == oldest)
            ),
            (MAX_HISTORY_SESSIONS, false),
            "{HISTORY_CAPPED}"
        );
    }

    #[test_case(true; "online")]
    #[test_case(false; "offline")]
    fn history_names_a_session_by_title_and_handle_online_or_offline(online: bool) {
        let (_temp, state_dir, mut database, session_id) = open();
        let mut named = TestSession::new(MODEL, CWD);
        named.push_message(TestMessage(ARMED_TITLE.into()));
        named.meta.peer_controls = Some(StoredPeerControls {
            handle: Some(HANDLE.into()),
            ..StoredPeerControls::default()
        });
        let lease = SessionLease::acquire(&state_dir, named.id).unwrap();
        database.save(&named, None).unwrap();
        database
            .upsert_automation_binding(&arming(named.id, AUTOMATION))
            .unwrap();
        if !online {
            drop(lease);
        }

        let history = database
            .load_automation_history(session_id, MAX_HISTORY_SESSIONS)
            .unwrap();

        assert_eq!(
            history
                .iter()
                .map(|row| (row.session_id, row.title.as_str(), row.handle.as_deref()))
                .collect::<Vec<_>>(),
            [(named.id, named.title.as_str(), Some(HANDLE))],
            "{NAMED_ONLINE_OR_NOT}"
        );
    }

    #[test]
    fn an_ephemeral_store_and_the_persistent_one_never_list_each_others_sessions() {
        let (_temp, state_dir, mut database, session_id) = open();
        let volatile = TempDir::new().unwrap();
        let mut ephemeral = SessionDatabase::open(&StateDir::split(
            volatile.path().to_path_buf(),
            state_dir.path().to_path_buf(),
        ))
        .unwrap();
        let persistent_peer = other_session(&mut database);
        let ephemeral_session = other_session(&mut ephemeral);
        let peer_ms = arm_ago(&database, persistent_peer, DAY_MS);
        arm_ago(&ephemeral, ephemeral_session, DAY_MS);

        let persistent_view = database
            .load_automation_history(session_id, MAX_HISTORY_SESSIONS)
            .unwrap();
        let ephemeral_view = ephemeral
            .load_automation_history(ephemeral_session, MAX_HISTORY_SESSIONS)
            .unwrap();

        assert_eq!(
            (activity(&persistent_view), activity(&ephemeral_view)),
            (vec![(persistent_peer, peer_ms)], Vec::new()),
            "{EPHEMERAL_APART}"
        );
    }

    #[test]
    fn a_seen_delivery_and_a_kept_source_are_recognized() {
        let (_temp, _state_dir, database, session_id) = open();
        database
            .insert_consuming_automation_firing(&consuming(session_id), DELIVERY)
            .unwrap();

        let first = database
            .insert_automation_source(session_id, DIGEST, SOURCE)
            .unwrap();
        let again = database
            .insert_automation_source(session_id, DIGEST, SOURCE)
            .unwrap();

        assert_eq!(
            database
                .automation_event_seen(session_id, AUTOMATION, EVENT_KEY)
                .unwrap(),
            AutomationEventSeen::Holding
        );
        assert_eq!(
            database
                .automation_event_seen(session_id, OTHER_AUTOMATION, EVENT_KEY)
                .unwrap(),
            AutomationEventSeen::Unseen
        );
        assert!(first && !again);
        assert_eq!(
            database
                .load_automation_source(session_id, DIGEST)
                .unwrap()
                .as_deref(),
            Some(SOURCE)
        );
        assert_eq!(
            database
                .load_automation_source(session_id, OTHER_DIGEST)
                .unwrap(),
            None
        );
    }

    #[test_case(AutomationFiringStatus::Completed, false; "completed")]
    #[test_case(AutomationFiringStatus::Skipped, false; "skipped")]
    #[test_case(AutomationFiringStatus::Released, true; "released")]
    #[test_case(AutomationFiringStatus::Failed, true; "failed")]
    #[test_case(AutomationFiringStatus::Cancelled, true; "cancelled")]
    #[test_case(AutomationFiringStatus::Dropped, true; "dropped")]
    fn a_consumed_message_is_held_until_it_can_no_longer_be_released(
        status: AutomationFiringStatus,
        releases: bool,
    ) {
        let (_temp, _state_dir, database, session_id) = open();
        database
            .insert_consuming_automation_firing(&consuming(session_id), DELIVERY)
            .unwrap();
        database.start_automation_firing(FIRE_ID).unwrap();

        let finished = database
            .finish_automation_firing(FIRE_ID, &end(status))
            .unwrap();

        let held = delivery(&database, FIRE_ID);
        let startup = database.interrupt_automation_firings(session_id).unwrap();
        assert_eq!(finished.release.is_some(), releases, "{FINISH_HANDS_BACK}");
        if releases {
            assert_eq!(held.as_deref(), Some(DELIVERY), "{HELD_UNTIL_RELEASED}");
            assert_eq!(
                startup.releases,
                [AutomationRelease {
                    fire_id: FIRE_ID.into(),
                    delivery: DELIVERY.into(),
                }],
                "{RELEASED_AT_START}"
            );
        } else {
            assert_eq!(
                (held, startup.releases),
                (None, Vec::new()),
                "{KEPT_FOR_GOOD}"
            );
        }
        assert_eq!(
            database
                .automation_event_seen(session_id, AUTOMATION, EVENT_KEY)
                .unwrap(),
            AutomationEventSeen::Holding
        );
        database.clear_automation_delivery(FIRE_ID).unwrap();
        let seen = if releases {
            AutomationEventSeen::Handed
        } else {
            AutomationEventSeen::Holding
        };
        assert_eq!(
            database
                .automation_event_seen(session_id, AUTOMATION, EVENT_KEY)
                .unwrap(),
            seen
        );
    }

    #[test]
    fn startup_releases_the_message_of_an_interrupted_firing() {
        let (_temp, _state_dir, database, session_id) = open();
        database
            .insert_consuming_automation_firing(&consuming(session_id), DELIVERY)
            .unwrap();
        database.start_automation_firing(FIRE_ID).unwrap();

        let startup = database.interrupt_automation_firings(session_id).unwrap();

        assert_eq!(
            startup,
            AutomationStartup {
                interrupted: 1,
                dropped: 0,
                releases: vec![AutomationRelease {
                    fire_id: FIRE_ID.into(),
                    delivery: DELIVERY.into(),
                }],
            },
            "{RELEASED_AT_START}"
        );
    }

    #[test]
    fn a_downgraded_firing_gives_its_message_up() {
        let (_temp, _state_dir, database, session_id) = open();
        database
            .insert_consuming_automation_firing(&consuming(session_id), DELIVERY)
            .unwrap();

        database
            .downgrade_automation_firing(FIRE_ID, DOWNGRADED_EVENT)
            .unwrap();

        let firing = database
            .load_automation_firing(FIRE_ID)
            .unwrap()
            .unwrap()
            .firing;
        assert_eq!(
            (
                firing.summary.consumed,
                firing.delivery,
                firing.event.as_str()
            ),
            (false, None, DOWNGRADED_EVENT),
            "{DOWNGRADE_GIVES_UP}"
        );
        assert_eq!(
            database
                .automation_event_seen(session_id, AUTOMATION, EVENT_KEY)
                .unwrap(),
            AutomationEventSeen::Handed,
            "{DOWNGRADE_GIVES_UP}"
        );
        database
            .finish_automation_firing(FIRE_ID, &end(AutomationFiringStatus::Completed))
            .unwrap();
        assert_eq!(
            refusal(database.downgrade_automation_firing(FIRE_ID, DOWNGRADED_EVENT)),
            AUTOMATION_FIRING_FINISHED
        );
    }

    #[test]
    fn retention_keeps_a_finished_firing_until_its_message_is_released() {
        let (_temp, _state_dir, database, session_id) = open();
        let fire_id = |index: usize| format!("{RETAINED_FIRE_ID}-{index}");
        let kept = |database: &SessionDatabase| {
            database.load_automation_firing(FIRE_ID).unwrap().is_some()
        };
        database
            .insert_consuming_automation_firing(&consuming(session_id), DELIVERY)
            .unwrap();
        database.start_automation_firing(FIRE_ID).unwrap();
        database
            .finish_automation_firing(FIRE_ID, &end(AutomationFiringStatus::Failed))
            .unwrap();
        for index in 0..=MAX_FINISHED_FIRINGS {
            running(
                &database,
                &firing(session_id, &fire_id(index), AutomationTrigger::Idle),
            );
            database
                .finish_automation_firing(&fire_id(index), &end(AutomationFiringStatus::Failed))
                .unwrap();
        }
        let releasing = kept(&database);

        database.clear_automation_delivery(FIRE_ID).unwrap();
        database
            .insert_automation_firing(&firing(session_id, LAST_FIRE_ID, AutomationTrigger::Idle))
            .unwrap();

        assert_eq!(
            (releasing, kept(&database)),
            (true, false),
            "{RELEASE_OUTLIVES_RETENTION}"
        );
    }
}
