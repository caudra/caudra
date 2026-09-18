//! Canonical SQLite storage for structured session state.
//!
//! One global database owns one current row for each retained logical value.
//! History suffixes are immutable inserts, but replacements overwrite canonical
//! rows transactionally; there is deliberately no event or projection table.
//! Generic message, usage, and rich-output values stay opaque Serde JSON so this
//! crate does not acquire dependencies on provider or agent types.
//!
//! New canonical values serialize and enforce compiled payload/scalar limits
//! before `BEGIN IMMEDIATE`, then use `write_version` to reject stale
//! same-session snapshots. A cursor advances only after commit. Global growth is
//! not capped: diagnostics and warnings must not delete canonical sessions or
//! reject otherwise valid writes.
//!
//! Managed full tool outputs, rewind archives, and workspace snapshots remain
//! external. Deletes enqueue their idempotent cleanup in the same transaction as
//! the structured delete because SQLite cannot atomically remove those files.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, c_int};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::ops::ControlFlow;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf, absolute};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use crate::permission_state::{
    PermissionHistoryScan, RawPermissionSession, RawPermissionSnapshot,
    validate_conversation_record, validate_review_only_change,
};
use caudra_workspace::WorkspacePath;
use rusqlite::backup::{Backup, Progress as BackupProgress};
use rusqlite::ffi::{self, Error as SqliteErrorCode};
use rusqlite::limits::Limit;
use rusqlite::types::Value as SqlValue;
use rusqlite::{
    Connection, Error as SqliteError, MAIN_DB, OpenFlags, OptionalExtension, Transaction,
    TransactionBehavior, params, params_from_iter,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tempfile::NamedTempFile;
use tracing::warn;

use super::lease::{SessionLease, held_session_ids};
use super::progress::{MIGRATION, MigrationEvent, PRUNE, PruneEvent};
use super::{
    ProjectUsageRelocation, SESSION_VERSION, Session, SessionError, SessionLocation, SessionMeta,
    SessionRelocation, SessionRelocationResult, SessionSummary, StoredSubagent,
    StoredSubagentOutcome, StoredSubagentTaskSpec, StoredTokenUsage, StoredToolUsage, next_epoch,
};
use crate::id::CaudraId;
use crate::retention::SessionFacts;
use crate::state::{
    WorkspaceTabs, remove_relocated_workspace_tabs, write_relocated_workspace_tabs,
};
use crate::tool_ledger::{Latency, ToolOutcome};
use crate::tool_outputs::{TOOL_OUTPUT_DIR, delete_session_outputs};
use crate::usage_ledger::LedgerPurpose;
use crate::workflow::{
    SESSION_WORKFLOW_BYTES, WorkflowRunStatus, interrupt_runs, trim_workflow_runs, workflow_totals,
};
use crate::workflow_scratch::{remove_session as remove_scratch_session, session_scratch_bytes};
use crate::workspace_binding::StoredWorkspaceBinding;
use crate::{
    StateDir, StorageError, existing_state_lock, lock_session_artifacts,
    shared_existing_state_lock, shared_state_lock, try_exclusive_existing_state_lock,
};

pub const SESSIONS_DB_FILE: &str = "caudra.sqlite";
pub const SESSIONS_DB_LOCK_FILE: &str = "caudra.sqlite.lock";

const SCHEMA_VERSION: i64 = 10;
const APPLICATION_ID: i64 = i32::from_be_bytes(*b"CAUD") as i64;
/// Pages copied per step of the pre-migration backup. The whole file is copied
/// under the initialization lock, so this only bounds how long the backup holds
/// SQLite's read lock between steps.
const BACKUP_PAGES_PER_STEP: c_int = 1024;
const INCREMENTAL_AUTO_VACUUM: i64 = 2;
const PAGE_SIZE: i64 = 4096;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const WAL_AUTO_CHECKPOINT_PAGES: i64 = 1000;
/// What `journal_size_limit` keeps. The WAL is a ring buffer that a checkpoint
/// resets rather than shrinks, and this limit only truncates it back down to
/// itself, so this is a floor for the observed `-wal` size after any burst, not
/// a cap on an active WAL. A size alarm therefore has to sit above it.
pub const WAL_RETENTION_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PAYLOAD_BYTES: usize = 32 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_PATH_BYTES: usize = 32 * 1024;
const MAX_JSON_DEPTH: usize = 64;
const MAX_IMAGES_PER_ITEM: usize = 16;
const MAX_DECODED_IMAGE_BYTES: usize = 24 * 1024 * 1024;
const MAX_SQLITE_VALUE_BYTES: i32 = 40 * 1024 * 1024;
const MAX_EAGER_LOAD_BYTES: usize = 512 * 1024 * 1024;
/// Level 3 won the codec gate in `benches/payload_codecs.rs`: its ratio ties
/// gzip-6 to within a few percent on every size band, while decompressing 2.4x
/// to 3.2x faster, and decompression is what a session load waits on. Level 9
/// buys under 0.4x more ratio for a 30x slower compress.
const PAYLOAD_COMPRESSION_LEVEL: i32 = 3;
const PAYLOAD_DECOMPRESSION_FAILED: &str = "payload could not be decompressed";
/// Rows between progress reports during a payload rewrite.
const PAYLOAD_PROGRESS_INTERVAL: u64 = 512;
/// Pages between progress reports while a freelist is being reclaimed.
const RECLAIM_PROGRESS_INTERVAL: u64 = 1024;
const OWNER_FILE_MODE: u32 = 0o600;
const OTHER_USER_PERMISSIONS: u32 = 0o077;
const DATABASE_SIDECAR_SUFFIXES: [&str; 3] = ["-wal", "-shm", "-journal"];
pub(super) const SESSION_SNAPSHOT_DIR: &str = "session-snapshots";
const CLEANUP_RETRY_DELAY_MS: i64 = 60_000;
const PENDING_ARCHIVE_ORPHAN_GRACE: Duration = Duration::from_secs(60 * 60);
const ARTIFACT_CLEANUP_KINDS: [&str; 4] =
    ["tool_output", "archive", "snapshot", "workflow_scratch"];
const STATE_SCOPE_GLOBAL: &str = "global";
const PERMISSION_RULES_KEY: &str = "permission.rules";
const PERMISSION_METADATA_KEY: &str = "structured_permission_rules";
const PERMISSION_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
const PERMISSION_SNAPSHOT_SESSIONS: usize = 10_000;
const INVALID_PERMISSION_REPAIR: &str =
    "invalid or stale permission review repair; refresh the preview";
const NO_NAMED_HOLDERS: &str =
    "another Caudra process, a storage command, or a reader has it open; close it and start again";
const NAMED_HOLDERS: &str = "close these open sessions and start again:";
const UNKNOWN_SESSION_TITLE: &str = "<untitled>";
const MAX_HOLDER_TITLE_BYTES: usize = 60;
const TITLE_ELLIPSIS: &str = "...";
const MAX_BASE58_UUID_BYTES: usize = 22;
const UUID_VERSION_BYTE: usize = 6;
const UUID_VARIANT_BYTE: usize = 8;
const UUID_VERSION_SHIFT: u32 = 4;
const UUID_VARIANT_SHIFT: u32 = 6;
const UUID_V7: u8 = 7;
const UUID_RFC4122_VARIANT: u8 = 2;
/// Rich tool output rows at or below this size survive a trim so old
/// transcripts keep their todo panels and other small structured records.
const TRIM_KEEP_OUTPUT_BYTES: i64 = 4096;
const SESSION_OPEN_ELSEWHERE: &str = "session is open in another Caudra instance";
const UNKNOWN_CLEANUP_KIND: &str = "unknown cleanup job kind";
const READ_ONLY_DATABASE_UNAVAILABLE: &str = "read-only inspection requires both WAL and SHM sidecars or exclusive access to an offline database without sidecars; retry after other connections close";
const WAL_PERSISTENCE_CONFIGURATION_FAILED: &str = "failed to configure SQLite WAL persistence";
const URI_HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";
#[cfg(not(unix))]
const INVALID_SQLITE_URI_PATH: &str = "SQLite URI paths must be valid UTF-8 on this platform";
const RELOCATION_REMOTE: &str = "remote sessions cannot be relocated";
const RELOCATION_PENDING_REVERT: &str = "the source workspace has a pending revert or restore";
const RELOCATION_WORKFLOW: &str = "stop active or resumable workflows before relocating";
const RELOCATION_METADATA_FIELDS: [&str; 6] = [
    "plan_path",
    "plan_target",
    "plan_written",
    "structured_permission_rules",
    "yolo",
    "snapshots_unavailable",
];

/// Lifetime spend, aggregated into hourly buckets. Deliberately carries no
/// `session_id` and no foreign key: forgetting a session must not erase what it
/// cost. Growth is bounded by time, model, and project rather than by session
/// count, and nothing reads it to decide whether a session is correct, so losing
/// it costs reporting and nothing else.
///
/// `cost` is the priced sum rather than a nullable per-entry price: a bucket
/// mixes priced and unpriced turns, so `SUM(cost)` alone would quietly
/// understate. `unpriced_turns` is what makes the shortfall countable.
///
/// `purpose` separates the conversation from what Caudra spends on its own, so
/// a bill can name goal evaluation, compaction, or titles. The model cannot
/// stand in for it: those workloads often run on the chat model.
///
/// `subscription` says whether anyone was invoiced. A subscription bucket holds
/// the API list price for its tokens, which is a real number about a bill that
/// never arrives, so summing the two together would overstate spend.
const USAGE_LEDGER_TABLE: &str = r#"
CREATE TABLE usage_ledger (
    bucket_start   INTEGER NOT NULL,
    provider       TEXT NOT NULL,
    model          TEXT NOT NULL,
    cwd            TEXT NOT NULL,
    purpose        TEXT NOT NULL,
    ephemeral      INTEGER NOT NULL CHECK(ephemeral IN (0, 1)),
    subscription   INTEGER NOT NULL CHECK(subscription IN (0, 1)),
    input_tokens   INTEGER NOT NULL,
    output_tokens  INTEGER NOT NULL,
    cache_creation INTEGER NOT NULL,
    cache_read     INTEGER NOT NULL,
    cost           REAL NOT NULL,
    priced_turns   INTEGER NOT NULL,
    unpriced_turns INTEGER NOT NULL,
    PRIMARY KEY(bucket_start, provider, model, cwd, purpose, ephemeral, subscription)
) STRICT, WITHOUT ROWID;
"#;

/// What the `1 -> 2` step actually produced. Frozen: replaying history must
/// rebuild the shape of the day, not today's shape, or `2 -> 3` would find its
/// work already done and the chain would stop describing what happened.
const USAGE_LEDGER_TABLE_V2: &str = r#"
CREATE TABLE usage_ledger (
    bucket_start   INTEGER NOT NULL,
    provider       TEXT NOT NULL,
    model          TEXT NOT NULL,
    cwd            TEXT NOT NULL,
    ephemeral      INTEGER NOT NULL CHECK(ephemeral IN (0, 1)),
    input_tokens   INTEGER NOT NULL,
    output_tokens  INTEGER NOT NULL,
    cache_creation INTEGER NOT NULL,
    cache_read     INTEGER NOT NULL,
    cost           REAL NOT NULL,
    priced_turns   INTEGER NOT NULL,
    unpriced_turns INTEGER NOT NULL,
    PRIMARY KEY(bucket_start, provider, model, cwd, ephemeral)
) STRICT, WITHOUT ROWID;
"#;

/// A `WITHOUT ROWID` primary key cannot be widened in place, so the table is
/// rebuilt. Everything recorded before the split was the conversation or was
/// billed as if it were, and `chat` is the honest name for that.
const USAGE_LEDGER_ADD_PURPOSE: &str = r#"
ALTER TABLE usage_ledger RENAME TO usage_ledger_v2;
CREATE TABLE usage_ledger (
    bucket_start   INTEGER NOT NULL,
    provider       TEXT NOT NULL,
    model          TEXT NOT NULL,
    cwd            TEXT NOT NULL,
    purpose        TEXT NOT NULL,
    ephemeral      INTEGER NOT NULL CHECK(ephemeral IN (0, 1)),
    input_tokens   INTEGER NOT NULL,
    output_tokens  INTEGER NOT NULL,
    cache_creation INTEGER NOT NULL,
    cache_read     INTEGER NOT NULL,
    cost           REAL NOT NULL,
    priced_turns   INTEGER NOT NULL,
    unpriced_turns INTEGER NOT NULL,
    PRIMARY KEY(bucket_start, provider, model, cwd, purpose, ephemeral)
) STRICT, WITHOUT ROWID;
INSERT INTO usage_ledger
    SELECT bucket_start, provider, model, cwd, 'chat', ephemeral, input_tokens,
           output_tokens, cache_creation, cache_read, cost, priced_turns, unpriced_turns
    FROM usage_ledger_v2;
DROP TABLE usage_ledger_v2;
"#;

/// Rebuilt again for the same reason `2 -> 3` rebuilt it: the payer joins the
/// primary key. Rows recorded before the split cannot say which payer they had,
/// and calling them billed leaves every existing all-time total exactly where it
/// was. `model_usage` only gains a column, so it is altered in place.
const USAGE_LEDGER_ADD_SUBSCRIPTION: &str = r#"
ALTER TABLE usage_ledger RENAME TO usage_ledger_v3;
CREATE TABLE usage_ledger (
    bucket_start   INTEGER NOT NULL,
    provider       TEXT NOT NULL,
    model          TEXT NOT NULL,
    cwd            TEXT NOT NULL,
    purpose        TEXT NOT NULL,
    ephemeral      INTEGER NOT NULL CHECK(ephemeral IN (0, 1)),
    subscription   INTEGER NOT NULL CHECK(subscription IN (0, 1)),
    input_tokens   INTEGER NOT NULL,
    output_tokens  INTEGER NOT NULL,
    cache_creation INTEGER NOT NULL,
    cache_read     INTEGER NOT NULL,
    cost           REAL NOT NULL,
    priced_turns   INTEGER NOT NULL,
    unpriced_turns INTEGER NOT NULL,
    PRIMARY KEY(bucket_start, provider, model, cwd, purpose, ephemeral, subscription)
) STRICT, WITHOUT ROWID;
INSERT INTO usage_ledger
    SELECT bucket_start, provider, model, cwd, purpose, ephemeral, 0, input_tokens,
           output_tokens, cache_creation, cache_read, cost, priced_turns, unpriced_turns
    FROM usage_ledger_v3;
DROP TABLE usage_ledger_v3;
ALTER TABLE model_usage ADD COLUMN subscription_cost REAL;
"#;

/// What tools did, in the same two shapes spend is kept in: a per-session
/// breakdown that dies with its transcript, and an hourly ledger that outlives
/// it. A forgotten session must not take the project's tool history with it,
/// for the same reason it must not take what it cost.
///
/// `outcome` joins both primary keys rather than sitting beside a plain error
/// counter. It holds `ok` or one of the agent's low-cardinality failure
/// classes, so the grain is bounded at seven rows per key and an error rate can
/// say why it is what it is. Errors are `calls` summed where `outcome` is not
/// `ok`.
///
/// `duration_ms` is a sum, and `latency` a log-scale histogram of the same
/// durations. A sum survives being merged across buckets and a percentile does
/// not, so the distribution is stored rather than the answer taken from it.
///
/// `tokens` is the estimated size of the model-facing result, which is what a
/// tool actually costs the context window.
const TOOL_USAGE_TABLES: &str = r#"
CREATE TABLE session_tool_usage (
    session_id      BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    tool            TEXT NOT NULL,
    source          TEXT NOT NULL,
    outcome         TEXT NOT NULL,
    calls           INTEGER NOT NULL,
    duration_ms     INTEGER NOT NULL,
    tokens          INTEGER NOT NULL,
    latency         BLOB NOT NULL,
    PRIMARY KEY(session_id, tool, source, outcome)
) STRICT, WITHOUT ROWID;

CREATE TABLE tool_ledger (
    bucket_start    INTEGER NOT NULL,
    tool            TEXT NOT NULL,
    source          TEXT NOT NULL,
    cwd             TEXT NOT NULL,
    outcome         TEXT NOT NULL,
    calls           INTEGER NOT NULL,
    duration_ms     INTEGER NOT NULL,
    tokens          INTEGER NOT NULL,
    latency         BLOB NOT NULL,
    PRIMARY KEY(bucket_start, tool, source, cwd, outcome)
) STRICT, WITHOUT ROWID;
"#;

/// Shared so the reader and the project move cannot drift into different column
/// orders behind the same row mapper.
const TOOL_BUCKET_COLUMNS: &str =
    "bucket_start, tool, source, cwd, outcome, calls, duration_ms, tokens, latency";
const TOOL_LEDGER_LATENCY: &str = "tool_ledger.latency";
const SESSION_TOOL_LATENCY: &str = "session_tool_usage.latency";

/// Durable workflow runs and the journal of host calls each one made. Runs
/// belong to a session and go with it; calls belong to a run. `bytes` is
/// generated so the accounting can never drift from the row it describes.
const WORKFLOW_TABLES: &str = r#"
CREATE TABLE workflow_runs (
    run_id           TEXT PRIMARY KEY,
    session_id       BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    display_name     TEXT NOT NULL,
    workflow_name    TEXT NOT NULL,
    source_kind      TEXT NOT NULL CHECK(source_kind IN ('builtin', 'project', 'user')),
    source_path      TEXT,
    source_digest    TEXT NOT NULL,
    language_version INTEGER NOT NULL,
    abi_version      INTEGER NOT NULL,
    source           TEXT NOT NULL,
    args             TEXT NOT NULL CHECK(json_valid(args)),
    objective        TEXT,
    launch_mode      TEXT NOT NULL,
    status           TEXT NOT NULL CHECK(status IN ('active', 'paused', 'budget_limited', 'interrupted', 'completed', 'cancelled', 'failed')),
    pause_kind       TEXT,
    pause_message    TEXT,
    revision         INTEGER NOT NULL DEFAULT 0,
    execution_epoch  INTEGER NOT NULL DEFAULT 0,
    phase            TEXT,
    agent_budget     INTEGER NOT NULL,
    agents_admitted  INTEGER NOT NULL DEFAULT 0,
    usage            TEXT NOT NULL CHECK(json_valid(usage)),
    roster           TEXT NOT NULL CHECK(json_valid(roster)),
    result           TEXT CHECK(result IS NULL OR json_valid(result)),
    error            TEXT,
    outbox_pending   INTEGER NOT NULL DEFAULT 0 CHECK(outbox_pending IN (0, 1)),
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    bytes            INTEGER NOT NULL GENERATED ALWAYS AS (
        length(CAST(run_id AS BLOB)) + length(CAST(display_name AS BLOB))
        + length(CAST(workflow_name AS BLOB)) + coalesce(length(CAST(source_path AS BLOB)), 0)
        + length(CAST(source_digest AS BLOB)) + length(CAST(source AS BLOB))
        + length(CAST(args AS BLOB)) + coalesce(length(CAST(objective AS BLOB)), 0)
        + length(CAST(launch_mode AS BLOB)) + coalesce(length(CAST(pause_kind AS BLOB)), 0)
        + coalesce(length(CAST(pause_message AS BLOB)), 0) + coalesce(length(CAST(phase AS BLOB)), 0)
        + length(CAST(usage AS BLOB)) + length(CAST(roster AS BLOB))
        + coalesce(length(CAST(result AS BLOB)), 0) + coalesce(length(CAST(error AS BLOB)), 0)
    ) STORED
) STRICT;

CREATE INDEX workflow_runs_by_session ON workflow_runs(session_id, created_at);

CREATE TABLE workflow_calls (
    run_id       TEXT NOT NULL REFERENCES workflow_runs(run_id) ON DELETE CASCADE,
    call_key     INTEGER NOT NULL,
    kind         TEXT NOT NULL CHECK(kind IN ('agent', 'parallel', 'scratch_file')),
    request_hash TEXT NOT NULL,
    request      TEXT NOT NULL CHECK(json_valid(request)),
    state        TEXT NOT NULL CHECK(state IN ('started', 'completed', 'failed')),
    result       TEXT CHECK(result IS NULL OR json_valid(result)),
    error        TEXT,
    task_id      TEXT,
    started_at   INTEGER NOT NULL,
    finished_at  INTEGER,
    tokens_used  INTEGER NOT NULL DEFAULT 0,
    duration_ms  INTEGER NOT NULL DEFAULT 0,
    bytes        INTEGER NOT NULL GENERATED ALWAYS AS (
        length(CAST(run_id AS BLOB)) + length(CAST(request_hash AS BLOB))
        + length(CAST(request AS BLOB)) + coalesce(length(CAST(result AS BLOB)), 0)
        + coalesce(length(CAST(error AS BLOB)), 0) + coalesce(length(CAST(task_id AS BLOB)), 0)
    ) STORED,
    PRIMARY KEY(run_id, call_key)
) STRICT;
"#;

/// The timeline of a run: every phase it entered and every line it logged,
/// in the order they happened. Viewing data only; replay never reads it.
const WORKFLOW_EVENTS_TABLE: &str = r#"
CREATE TABLE workflow_run_events (
    run_id TEXT NOT NULL REFERENCES workflow_runs(run_id) ON DELETE CASCADE,
    seq    INTEGER NOT NULL,
    at     INTEGER NOT NULL,
    kind   TEXT NOT NULL CHECK(kind IN ('phase', 'log')),
    text   TEXT NOT NULL,
    bytes  INTEGER NOT NULL GENERATED ALWAYS AS (
        length(CAST(run_id AS BLOB)) + length(CAST(text AS BLOB))
    ) STORED,
    PRIMARY KEY(run_id, seq)
) STRICT;
"#;

/// Payload columns become compressed blobs. The three tables are `WITHOUT
/// ROWID`, so each is rebuilt rather than altered, and the rows are copied by
/// [`compress_existing_payloads`] rather than by SQL: SQLite cannot compress,
/// and a SQL copy would write the uncompressed bytes a second time before the
/// rewrite replaced them.
///
/// `byte_count` keeps meaning the uncompressed length, so its two CHECK
/// constraints go with the column type. `json_valid` goes for the same reason;
/// [`SessionDatabase::validate_payload_json`] already enforces it on write.
///
/// Rebuilding leaves every original page on the freelist, so the file grows
/// before it shrinks: a 1.9 GB database measured 2.7 GB immediately after this
/// step and 781 MB once the pages came back. Reclaiming them takes far longer
/// than the rewrite itself, so it is left to `sweep::prune`, which already
/// vacuums the whole freelist on the background sweep rather than at startup.
const PAYLOAD_COMPRESSION_SCHEMA: &str = r#"
ALTER TABLE main_history_items RENAME TO main_history_items_v9;
ALTER TABLE tool_outputs RENAME TO tool_outputs_v9;
ALTER TABLE subagent_history_items RENAME TO subagent_history_items_v9;

CREATE TABLE main_history_items (
    session_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ordinal    INTEGER NOT NULL,
    payload    BLOB NOT NULL,
    byte_count INTEGER NOT NULL,
    PRIMARY KEY(session_id, ordinal)
) STRICT, WITHOUT ROWID;

CREATE TABLE tool_outputs (
    session_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    tool_id    TEXT NOT NULL,
    payload    BLOB NOT NULL,
    byte_count INTEGER NOT NULL,
    PRIMARY KEY(session_id, tool_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE subagent_history_items (
    session_id  BLOB NOT NULL,
    subagent_id TEXT NOT NULL,
    ordinal     INTEGER NOT NULL,
    payload     BLOB NOT NULL,
    byte_count  INTEGER NOT NULL,
    PRIMARY KEY(session_id, subagent_id, ordinal),
    FOREIGN KEY(session_id, subagent_id)
        REFERENCES subagent_streams(session_id, subagent_id)
        ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
"#;

/// The renamed tables [`PAYLOAD_COMPRESSION_SCHEMA`] leaves behind, paired with
/// the key columns that accompany each payload through the rewrite.
const COMPRESSED_PAYLOAD_TABLES: [PayloadRewrite; 3] = [
    PayloadRewrite {
        table: "main_history_items",
        legacy: "main_history_items_v9",
        keys: "session_id, ordinal",
        placeholders: "?1, ?2",
    },
    PayloadRewrite {
        table: "tool_outputs",
        legacy: "tool_outputs_v9",
        keys: "session_id, tool_id",
        placeholders: "?1, ?2",
    },
    PayloadRewrite {
        table: "subagent_history_items",
        legacy: "subagent_history_items_v9",
        keys: "session_id, subagent_id, ordinal",
        placeholders: "?1, ?2, ?3",
    },
];

/// One table's move from [`PAYLOAD_COMPRESSION_SCHEMA`]'s renamed original into
/// its compressed replacement.
struct PayloadRewrite {
    table: &'static str,
    legacy: &'static str,
    keys: &'static str,
    placeholders: &'static str,
}

/// One step of the schema chain. A fresh database gets [`SCHEMA`] at
/// [`SCHEMA_VERSION`] directly; only an existing database replays these.
struct Migration {
    from: i64,
    to: i64,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        from: 1,
        to: 2,
        sql: USAGE_LEDGER_TABLE_V2,
    },
    Migration {
        from: 2,
        to: 3,
        sql: USAGE_LEDGER_ADD_PURPOSE,
    },
    Migration {
        from: 3,
        to: 4,
        sql: USAGE_LEDGER_ADD_SUBSCRIPTION,
    },
    Migration {
        from: 4,
        to: 5,
        sql: WORKFLOW_TABLES,
    },
    Migration {
        from: 5,
        to: 6,
        sql: WORKFLOW_EVENTS_TABLE,
    },
    Migration {
        from: 6,
        to: 7,
        sql: SESSION_WORKSPACE_BINDING_COLUMNS,
    },
    Migration {
        from: 7,
        to: 8,
        sql: PERMISSION_REVISION_SCHEMA,
    },
    Migration {
        from: 8,
        to: 9,
        sql: TOOL_USAGE_TABLES,
    },
    Migration {
        from: 9,
        to: 10,
        sql: PAYLOAD_COMPRESSION_SCHEMA,
    },
];

const PERMISSION_REVISION_SCHEMA: &str = r#"
ALTER TABLE sessions ADD COLUMN permission_generation INTEGER NOT NULL DEFAULT 0 CHECK(permission_generation >= 0);
ALTER TABLE sessions ADD COLUMN permission_lineage TEXT NOT NULL DEFAULT '';
UPDATE sessions SET permission_lineage = lower(hex(randomblob(16)));

CREATE TABLE permission_clock (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    store_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation >= 0),
    persistent_generation INTEGER NOT NULL CHECK(persistent_generation >= 0)
) STRICT;
INSERT INTO permission_clock VALUES (1, lower(hex(randomblob(16))), 0, 0);

CREATE TABLE permission_receipts (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    operation_id BLOB NOT NULL UNIQUE CHECK(length(operation_id) = 16),
    fingerprint TEXT NOT NULL,
    receipt TEXT NOT NULL CHECK(json_valid(receipt))
) STRICT;

CREATE TRIGGER permission_state_insert AFTER INSERT ON state
WHEN NEW.scope = 'global' AND NEW.key = 'permission.rules'
BEGIN
    UPDATE permission_clock SET generation = generation + 1, persistent_generation = persistent_generation + 1;
END;
CREATE TRIGGER permission_state_update AFTER UPDATE OF value ON state
WHEN NEW.scope = 'global' AND NEW.key = 'permission.rules' AND OLD.value IS NOT NEW.value
BEGIN
    UPDATE permission_clock SET generation = generation + 1, persistent_generation = persistent_generation + 1;
END;
CREATE TRIGGER permission_state_delete AFTER DELETE ON state
WHEN OLD.scope = 'global' AND OLD.key = 'permission.rules'
BEGIN
    UPDATE permission_clock SET generation = generation + 1, persistent_generation = persistent_generation + 1;
END;
CREATE TRIGGER permission_session_insert AFTER INSERT ON sessions
BEGIN
    UPDATE sessions SET permission_lineage = lower(hex(randomblob(16))) WHERE id = NEW.id;
    UPDATE permission_clock SET generation = generation + 1;
END;
CREATE TRIGGER permission_session_update AFTER UPDATE OF metadata, cwd, workspace_binding ON sessions
WHEN coalesce(OLD.metadata -> '$.structured_permission_rules', '[]')
        IS NOT coalesce(NEW.metadata -> '$.structured_permission_rules', '[]')
    OR OLD.cwd IS NOT NEW.cwd OR OLD.workspace_binding IS NOT NEW.workspace_binding
BEGIN
    UPDATE sessions SET permission_generation = permission_generation + 1 WHERE id = NEW.id;
    UPDATE permission_clock SET generation = generation + 1;
END;
CREATE TRIGGER permission_session_delete AFTER DELETE ON sessions
BEGIN
    UPDATE permission_clock SET generation = generation + 1;
END;
"#;

const SESSION_WORKSPACE_BINDING_COLUMNS: &str = r#"
ALTER TABLE sessions ADD COLUMN workspace_binding TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(workspace_binding));
ALTER TABLE sessions ADD COLUMN workspace_source TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN workspace_authority TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN workspace_principal TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN workspace_project TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN workspace_cursor TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN workspace_cursor_label TEXT;
CREATE INDEX sessions_workspace_updated ON sessions(
    workspace_source, workspace_authority, workspace_principal,
    workspace_project, workspace_cursor, updated_at DESC, id DESC
);
"#;

// Session state in this schema is canonical. The one exception is
// `usage_ledger`, which is a second durable representation on purpose: its
// retention is bounded by time rather than by session count, and its recovery
// semantics are that it is never read to establish session correctness.
const SCHEMA: &str = r#"
CREATE TABLE sessions (
    id                  BLOB PRIMARY KEY CHECK(length(id) = 16),
    format_version      INTEGER NOT NULL,
    title               TEXT NOT NULL,
    cwd                 TEXT NOT NULL,
    model               TEXT NOT NULL,
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL,
    write_version        INTEGER NOT NULL DEFAULT 0,
    logical_bytes        INTEGER NOT NULL DEFAULT 0,
    history_item_count   INTEGER NOT NULL DEFAULT 0,
    tool_output_count    INTEGER NOT NULL DEFAULT 0,
    subagent_item_count  INTEGER NOT NULL DEFAULT 0,
    token_usage          TEXT NOT NULL CHECK(json_valid(token_usage)),
    metadata             TEXT NOT NULL CHECK(json_valid(metadata)),
    workspace_binding    TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(workspace_binding)),
    workspace_source     TEXT NOT NULL DEFAULT '',
    workspace_authority  TEXT NOT NULL DEFAULT '',
    workspace_principal  TEXT NOT NULL DEFAULT '',
    workspace_project    TEXT NOT NULL DEFAULT '',
    workspace_cursor     TEXT NOT NULL DEFAULT '',
    workspace_cursor_label TEXT,
    last_opened_at       INTEGER,
    pinned               INTEGER NOT NULL DEFAULT 0 CHECK(pinned IN (0, 1)),
    trimmed_at           INTEGER
) STRICT;

CREATE INDEX sessions_cwd_updated
    ON sessions(cwd, updated_at DESC, id DESC);

CREATE INDEX sessions_workspace_updated ON sessions(
    workspace_source, workspace_authority, workspace_principal,
    workspace_project, workspace_cursor, updated_at DESC, id DESC
);

CREATE TABLE state (
    scope      TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL CHECK(json_valid(value)),
    updated_at INTEGER NOT NULL,
    PRIMARY KEY(scope, key)
) STRICT, WITHOUT ROWID;

CREATE TABLE main_history_items (
    session_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ordinal    INTEGER NOT NULL,
    payload    BLOB NOT NULL,
    byte_count INTEGER NOT NULL,
    PRIMARY KEY(session_id, ordinal)
) STRICT, WITHOUT ROWID;

CREATE TABLE tool_outputs (
    session_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    tool_id    TEXT NOT NULL,
    payload    BLOB NOT NULL,
    byte_count INTEGER NOT NULL,
    PRIMARY KEY(session_id, tool_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE subagent_streams (
    session_id  BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    subagent_id TEXT NOT NULL,
    task_spec   TEXT CHECK(task_spec IS NULL OR json_valid(task_spec)),
    PRIMARY KEY(session_id, subagent_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE subagent_history_items (
    session_id  BLOB NOT NULL,
    subagent_id TEXT NOT NULL,
    ordinal     INTEGER NOT NULL,
    payload     BLOB NOT NULL,
    byte_count  INTEGER NOT NULL,
    PRIMARY KEY(session_id, subagent_id, ordinal),
    FOREIGN KEY(session_id, subagent_id)
        REFERENCES subagent_streams(session_id, subagent_id)
        ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

CREATE TABLE subagents (
    session_id         BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ordinal            INTEGER NOT NULL,
    tool_use_id        TEXT NOT NULL,
    parent_tool_use_id TEXT,
    root_tool_use_id   TEXT,
    name               TEXT NOT NULL,
    model              TEXT,
    outcome            TEXT CHECK(outcome IS NULL OR outcome IN ('unknown', 'done', 'killed', 'error')),
    PRIMARY KEY(session_id, ordinal),
    UNIQUE(session_id, tool_use_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE model_usage (
    session_id        BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    model             TEXT NOT NULL,
    input_tokens      INTEGER NOT NULL,
    output_tokens     INTEGER NOT NULL,
    cache_creation    INTEGER NOT NULL,
    cache_read        INTEGER NOT NULL,
    cost              REAL,
    subscription_cost REAL,
    PRIMARY KEY(session_id, model)
) STRICT, WITHOUT ROWID;

CREATE TABLE session_tombstones (
    session_id      BLOB PRIMARY KEY CHECK(length(session_id) = 16),
    deleted_version INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE cleanup_jobs (
    id              INTEGER PRIMARY KEY,
    session_id      BLOB NOT NULL CHECK(length(session_id) = 16),
    kind            TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_ms INTEGER NOT NULL,
    last_error      TEXT,
    UNIQUE(session_id, kind)
) STRICT;

CREATE TABLE pending_archives (
    session_id            BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    expected_write_version INTEGER NOT NULL,
    pending_name          TEXT NOT NULL,
    byte_count            INTEGER NOT NULL,
    PRIMARY KEY(session_id, pending_name)
) STRICT, WITHOUT ROWID;
"#;

/// What a fresh database gets: every migration already folded in.
fn full_schema() -> String {
    format!(
        "{SCHEMA}{USAGE_LEDGER_TABLE}{WORKFLOW_TABLES}{WORKFLOW_EVENTS_TABLE}{PERMISSION_REVISION_SCHEMA}{TOOL_USAGE_TABLES}"
    )
}

/// One turn on its way into [`usage_ledger`](USAGE_LEDGER_TABLE). Borrowed
/// because the caller already owns every string and this runs per turn.
pub struct LedgerEntry<'a> {
    pub bucket_start: i64,
    pub provider: &'a str,
    pub model: &'a str,
    pub cwd: &'a str,
    pub purpose: LedgerPurpose,
    pub ephemeral: bool,
    pub subscription: bool,
    pub usage: StoredTokenUsage,
    pub cost: Option<f64>,
}

/// Tool activity on its way into [`tool_ledger`](TOOL_USAGE_TABLES), either one
/// finished call or a whole bucket being moved between projects. Borrowed for
/// the same reason [`LedgerEntry`] is: this runs per call.
pub struct ToolLedgerEntry<'a> {
    pub bucket_start: i64,
    pub tool: &'a str,
    pub source: &'a str,
    pub cwd: &'a str,
    pub outcome: ToolOutcome,
    pub calls: u64,
    pub duration_ms: u64,
    pub tokens: u64,
    pub latency: &'a Latency,
}

/// One accumulated hour of one tool's calls that ended one way. Counters are
/// `u64` for the same reason [`UsageBucket`]'s are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolBucket {
    pub bucket_start: i64,
    pub tool: String,
    pub source: String,
    pub cwd: String,
    pub outcome: ToolOutcome,
    pub calls: u64,
    pub duration_ms: u64,
    pub tokens: u64,
    pub latency: Latency,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryReadLimits {
    pub max_sessions: usize,
    pub max_rows: usize,
    pub max_bytes: usize,
    pub max_row_bytes: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct HistoryReadReport {
    pub sessions: usize,
    pub rows: usize,
    pub bytes: usize,
    pub oversized_rows: usize,
    pub invalid_records: usize,
    pub nonlocal_sessions: usize,
    pub max_rows_per_session: Option<usize>,
    pub session_row_cutoffs: usize,
    pub per_session: Vec<HistorySessionReadReport>,
    pub truncated: bool,
    pub stopped: bool,
}

#[derive(Debug, Serialize)]
pub struct HistorySessionReadReport {
    pub session_id: CaudraId,
    pub rows: usize,
    pub bytes: usize,
    pub row_cutoff: bool,
}

/// Borrowed, bounded JSON with source identity. The timestamp is the history
/// UUIDv7 creation time, not an execution time; cwd is the session's current value.
pub struct HistoryRecord<'a> {
    pub session_id: CaudraId,
    pub current_cwd: &'a str,
    pub subagent_id: Option<&'a str>,
    pub ordinal: u64,
    pub history_id: CaudraId,
    pub timestamp_ms: u64,
    pub payload: &'a Value,
}

/// One accumulated hour of spend. Counters are `u64` because a lifetime total
/// outgrows the `u32` a single session's counters use.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageBucket {
    pub bucket_start: i64,
    pub provider: String,
    pub model: String,
    pub cwd: String,
    pub purpose: String,
    pub ephemeral: bool,
    pub subscription: bool,
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub cost: f64,
    pub priced_turns: u64,
    pub unpriced_turns: u64,
}

#[derive(Debug, Clone)]
pub struct SessionCursor {
    session_id: CaudraId,
    write_version: i64,
    lineage: Arc<AtomicI64>,
    saved_epoch: u64,
    saved_rewrites: u64,
    saved_history_count: usize,
    saved_tool_ids: HashSet<String>,
    saved_subagent_counts: HashMap<String, usize>,
    logical_bytes: usize,
    root_bytes: usize,
    task_spec_bytes: usize,
}

#[derive(Debug)]
pub struct SessionRecreation {
    session_id: CaudraId,
    deleted_write_version: i64,
    removed_existing: bool,
    permissions: Option<String>,
    permission_generation: u64,
}

impl SessionRecreation {
    pub(super) fn removed_existing(&self) -> bool {
        self.removed_existing
    }
}

pub struct SessionDatabase {
    connection: Connection,
    state_dir: StateDir,
    _migration_lock: File,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionStorageStats {
    pub database_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
    pub page_size: u64,
    pub page_count: u64,
    pub freelist_count: u64,
    pub auto_vacuum: i64,
    pub schema_version: i64,
    pub session_count: u64,
    pub pinned_count: u64,
    pub trimmed_count: u64,
    pub history_item_count: u64,
    pub tool_output_count: u64,
    pub subagent_item_count: u64,
    pub logical_bytes: u64,
    pub workflow_run_count: u64,
    pub workflow_call_count: u64,
    pub workflow_bytes: u64,
    pub tool_output_file_bytes: u64,
    pub snapshot_bytes: u64,
    pub archive_bytes: u64,
    pub pending_cleanup_jobs: u64,
}

/// What one trim released. Row bytes are exact; artifact bytes are measured
/// before deletion and describe files the cleanup jobs then remove.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TrimReport {
    pub tool_output_rows: u64,
    pub tool_output_row_bytes: u64,
    pub workflow_call_rows: u64,
    pub workflow_call_bytes: u64,
    pub workflow_event_rows: u64,
    pub workflow_event_bytes: u64,
    pub artifact_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CheckpointResult {
    pub busy: u64,
    pub log_frames: u64,
    pub checkpointed_frames: u64,
}

/// Why a cleanup job may run. `Stale` means the session was written after
/// its trim or was recreated, so the job must be dropped rather than run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupGuard {
    Deleted,
    Trimmed,
    Stale,
}

struct SerializedRoot {
    token_usage: String,
    metadata: String,
    workspace_binding: String,
    workspace_source: String,
    workspace_authority: String,
    workspace_principal: String,
    workspace_project: String,
    workspace_cursor: String,
    workspace_cursor_label: Option<String>,
}

struct SerializedSession {
    root: SerializedRoot,
    messages: Vec<String>,
    tool_outputs: Vec<(String, String)>,
    subagent_messages: Vec<(String, Vec<String>)>,
    task_specs: HashMap<String, String>,
    logical_bytes: usize,
}

/// A synced pre-commit recovery export. The pending name is versioned and
/// restart-reconciled so process death cannot accumulate final archives or
/// consume retention for a rolled-back rewrite.
struct PreparedArchive {
    session_id: CaudraId,
    expected_write_version: i64,
    pending_name: String,
    pending_path: PathBuf,
    bytes: u64,
    cleanup_on_drop: bool,
}

struct RootRow {
    format_version: u32,
    title: String,
    cwd: String,
    model: String,
    created_at: u64,
    updated_at: u64,
    write_version: i64,
    logical_bytes: usize,
    history_item_count: usize,
    tool_output_count: usize,
    subagent_item_count: usize,
    token_usage: String,
    metadata: String,
    workspace_binding: String,
    workspace_source: String,
    workspace_authority: String,
    workspace_principal: String,
    workspace_project: String,
    workspace_cursor: String,
    workspace_cursor_label: Option<String>,
}

impl SessionDatabase {
    pub fn open(state_dir: &StateDir) -> Result<Self, SessionError> {
        let migration_lock = shared_state_lock(
            &state_dir.path().join(SESSIONS_DB_LOCK_FILE),
            OWNER_FILE_MODE,
        )?;
        let connection = open_writable_connection(state_dir, &migration_lock)?;
        let mut database = Self {
            connection,
            state_dir: state_dir.clone(),
            _migration_lock: migration_lock,
        };
        database.process_cleanup_jobs()?;
        // Recovery archives are bounded auxiliary exports. Unsafe or damaged
        // archive paths must never be followed, but they also must not make the
        // canonical database unavailable.
        if let Err(error) = database.reconcile_pending_archives() {
            warn!(%error, "session archive reconciliation deferred");
        }
        Ok(database)
    }

    /// Opens the repository for `state` table work only. Skips artifact
    /// cleanup and archive reconciliation so a preference read never does
    /// filesystem maintenance on the caller's thread.
    pub fn open_state(state_dir: &StateDir) -> Result<Self, SessionError> {
        let migration_lock = shared_state_lock(
            &state_dir.path().join(SESSIONS_DB_LOCK_FILE),
            OWNER_FILE_MODE,
        )?;
        let connection = open_writable_connection(state_dir, &migration_lock)?;
        Ok(Self {
            connection,
            state_dir: state_dir.clone(),
            _migration_lock: migration_lock,
        })
    }

    pub fn path(&self) -> PathBuf {
        self.state_dir.path().join(SESSIONS_DB_FILE)
    }

    pub(crate) fn open_permission_admin(state_dir: &StateDir) -> Result<Self, SessionError> {
        let migration_lock =
            try_exclusive_existing_state_lock(&state_dir.path().join(SESSIONS_DB_LOCK_FILE))?
                .ok_or_else(|| {
                    StorageError::Io(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "stop all sessions and close storage readers before rebinding permissions",
                    ))
                })?;
        let path = state_dir.path().join(SESSIONS_DB_FILE);
        let file = open_owner_only_existing(&path)?;
        validate_existing_database_sidecars(&path)?;
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        configure_wal_retention(&connection, true)?;
        let current = open_owner_only_existing(&path)?;
        validate_existing_database_sidecars(&path)?;
        #[cfg(unix)]
        if file.metadata().map_err(StorageError::from)?.ino()
            != current.metadata().map_err(StorageError::from)?.ino()
            || file.metadata().map_err(StorageError::from)?.dev()
                != current.metadata().map_err(StorageError::from)?.dev()
        {
            return Err(StorageError::Io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "permission database identity changed while opening",
            ))
            .into());
        }
        #[cfg(not(unix))]
        let _ = (&file, &current);
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_SQLITE_VALUE_BYTES)?;
        verify_current_schema(&connection)?;
        connection.execute_batch("PRAGMA trusted_schema = OFF;")?;
        Ok(Self {
            connection,
            state_dir: state_dir.clone(),
            _migration_lock: migration_lock,
        })
    }

    pub fn open_read_only(state_dir: &StateDir) -> Result<Self, SessionError> {
        Self::open_read_only_inner(state_dir, false)
    }

    /// Background inspection must not wait on a migration or SQLite writer.
    pub fn open_read_only_nonblocking(state_dir: &StateDir) -> Result<Self, SessionError> {
        Self::open_read_only_inner(state_dir, true)
    }

    fn open_read_only_inner(state_dir: &StateDir, nonblocking: bool) -> Result<Self, SessionError> {
        let path = absolute(state_dir.path().join(SESSIONS_DB_FILE)).map_err(StorageError::from)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(StorageError::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "session database path {} is not a regular file",
                        path.display()
                    ),
                ))
                .into());
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound(path.display().to_string()).into());
            }
            Err(error) => return Err(StorageError::from(error).into()),
        }
        // Inspection must not create files, change journal mode, or initialize
        // a schema. Normal writable initialization creates this lock file.
        let unavailable = || {
            StorageError::Io(io::Error::new(
                io::ErrorKind::WouldBlock,
                READ_ONLY_DATABASE_UNAVAILABLE,
            ))
        };
        let offline = match database_sidecars_exist(&path)? {
            [true, true, _] => false,
            [false, false, false] => true,
            _ => return Err(unavailable().into()),
        };
        let lock_path = path.with_file_name(SESSIONS_DB_LOCK_FILE);
        let migration_lock = if offline {
            try_exclusive_existing_state_lock(&lock_path)?.ok_or_else(unavailable)?
        } else if nonblocking {
            let file = existing_state_lock(&lock_path)?;
            file.try_lock_shared()
                .map_err(io::Error::from)
                .map_err(StorageError::from)?;
            file
        } else {
            shared_existing_state_lock(&lock_path)?
        };
        match database_sidecars_exist(&path)? {
            [true, true, _] if !offline => {}
            [false, false, false] if offline => {}
            _ => return Err(unavailable().into()),
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let connection = if offline {
            Connection::open_with_flags(
                immutable_database_uri(&path)?,
                flags | OpenFlags::SQLITE_OPEN_URI,
            )?
        } else {
            Connection::open_with_flags(&path, flags)?
        };
        connection.busy_timeout(if nonblocking {
            Duration::ZERO
        } else {
            BUSY_TIMEOUT
        })?;
        connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_SQLITE_VALUE_BYTES)?;
        connection.execute_batch("PRAGMA query_only = ON; PRAGMA trusted_schema = OFF;")?;
        verify_current_schema(&connection)?;
        Ok(Self {
            connection,
            state_dir: state_dir.clone(),
            _migration_lock: migration_lock,
        })
    }

    pub fn raw_permission_snapshot(&self) -> Result<RawPermissionSnapshot, SessionError> {
        let transaction = self.connection.unchecked_transaction()?;
        let snapshot = raw_permission_snapshot_on(&transaction)?;
        transaction.commit()?;
        Ok(snapshot)
    }

    pub fn visit_permission_history(
        &self,
        max_rows: usize,
        max_bytes: usize,
        max_row_bytes: usize,
        mut visit: impl FnMut(&str, &str),
    ) -> Result<PermissionHistoryScan, SessionError> {
        let transaction = self.connection.unchecked_transaction()?;
        let mut report = PermissionHistoryScan::default();
        for table in ["main_history_items", "subagent_history_items"] {
            let mut statement = transaction.prepare(&format!(
                "SELECT s.cwd, h.byte_count, CASE WHEN h.byte_count <= ?1 THEN h.payload END FROM {table} h JOIN sessions s ON s.id = h.session_id ORDER BY h.session_id, {}h.ordinal",
                if table == "subagent_history_items" { "h.subagent_id, " } else { "" }
            ))?;
            let mut rows = statement.query([to_i64(max_row_bytes, "history row limit")?])?;
            while let Some(row) = rows.next()? {
                let bytes = from_i64_usize(row.get(1)?, "history payload bytes")?;
                if report.rows >= max_rows || bytes > max_bytes.saturating_sub(report.bytes) {
                    report.truncated = true;
                    break;
                }
                report.rows += 1;
                report.bytes += bytes;
                if bytes > max_row_bytes {
                    report.oversized_rows += 1;
                    continue;
                }
                let cwd: String = row.get(0)?;
                let payload = payload_from_row(row, 2, "history payload")?;
                visit(&cwd, &payload);
            }
            if report.truncated {
                break;
            }
        }
        transaction.commit()?;
        Ok(report)
    }

    /// Samples current local project history without loading sessions or outputs.
    /// Indexed session/stream scans keep empty sessions, duplicate rows and skipped
    /// payloads inside the same budgets. `report` survives a partial read failure.
    pub fn visit_history_records(
        &self,
        project: &str,
        limits: &HistoryReadLimits,
        report: &mut HistoryReadReport,
        should_stop: impl Fn() -> bool,
        visit: impl FnMut(HistoryRecord<'_>) -> ControlFlow<()>,
    ) -> Result<(), SessionError> {
        self.visit_history_records_with_session_limit(
            project,
            limits,
            None,
            report,
            should_stop,
            visit,
        )
    }

    pub fn visit_history_records_with_session_limit(
        &self,
        project: &str,
        limits: &HistoryReadLimits,
        max_rows_per_session: Option<usize>,
        report: &mut HistoryReadReport,
        should_stop: impl Fn() -> bool,
        mut visit: impl FnMut(HistoryRecord<'_>) -> ControlFlow<()>,
    ) -> Result<(), SessionError> {
        report.max_rows_per_session = max_rows_per_session;
        let max_rows_per_session = max_rows_per_session.unwrap_or(usize::MAX);
        let transaction = self.connection.unchecked_transaction()?;
        let local = StoredWorkspaceBinding::local_from_cwd(project);
        let mut sessions = transaction.prepare(
            "SELECT id, CASE WHEN length(CAST(workspace_source AS BLOB)) <= ?3 THEN workspace_source END \
             FROM sessions INDEXED BY sessions_cwd_updated \
             WHERE cwd = ?1 ORDER BY updated_at DESC, id DESC LIMIT ?2",
        )?;
        let mut session_rows = sessions.query(params![
            project,
            to_i64(
                limits.max_sessions.saturating_add(1),
                "history session limit"
            )?,
            to_i64(MAX_IDENTIFIER_BYTES, "workspace source limit")?,
        ])?;
        'sessions: loop {
            if should_stop() {
                report.stopped = true;
                break;
            }
            let Some(session) = session_rows.next()? else {
                break;
            };
            if report.sessions >= limits.max_sessions {
                report.truncated = true;
                break;
            }
            report.sessions += 1;
            let Some(source) = session.get::<_, Option<String>>(1)? else {
                report.invalid_records += 1;
                continue;
            };
            if !source.is_empty() && source != local.trust_anchor().as_str() {
                report.nonlocal_sessions += 1;
                continue;
            }
            let session_id = id_from_row(session, 0)?;
            if history_uuid_timestamp(session_id).is_none() {
                report.invalid_records += 1;
                continue;
            }
            let session_index = report.per_session.len();
            report.per_session.push(HistorySessionReadReport {
                session_id,
                rows: 0,
                bytes: 0,
                row_cutoff: false,
            });
            let session_report = &mut report.per_session[session_index];
            for (table, stream, order) in [
                ("main_history_items", "NULL", "ordinal DESC"),
                (
                    "subagent_history_items",
                    "subagent_id",
                    "subagent_id DESC, ordinal DESC",
                ),
            ] {
                if should_stop() {
                    report.stopped = true;
                    break 'sessions;
                }
                let mut statement = transaction.prepare(&format!(
                    "SELECT CASE WHEN length(CAST({stream} AS BLOB)) <= {MAX_IDENTIFIER_BYTES} THEN {stream} END, \
                     ordinal, byte_count, \
                     CASE WHEN byte_count <= ?2 THEN payload END \
                     FROM {table} WHERE session_id = ?1 ORDER BY {order} LIMIT ?3"
                ))?;
                let mut rows = statement.query(params![
                    session_id.as_bytes().as_slice(),
                    to_i64(
                        limits.max_row_bytes.min(limits.max_bytes),
                        "history row limit"
                    )?,
                    to_i64(
                        limits
                            .max_rows
                            .saturating_sub(report.rows)
                            .min(max_rows_per_session.saturating_sub(session_report.rows))
                            .saturating_add(1),
                        "history row count"
                    )?,
                ])?;
                loop {
                    if should_stop() {
                        report.stopped = true;
                        break 'sessions;
                    }
                    let Some(row) = rows.next()? else { break };
                    if session_report.rows >= max_rows_per_session {
                        session_report.row_cutoff = true;
                        report.session_row_cutoffs += 1;
                        report.truncated = true;
                        continue 'sessions;
                    }
                    let bytes = from_i64_usize(row.get(2)?, "history payload bytes")?;
                    if report.rows >= limits.max_rows
                        || bytes > limits.max_bytes.saturating_sub(report.bytes)
                    {
                        report.truncated = true;
                        break 'sessions;
                    }
                    report.rows += 1;
                    report.bytes += bytes;
                    session_report.rows += 1;
                    session_report.bytes += bytes;
                    if bytes > limits.max_row_bytes {
                        report.oversized_rows += 1;
                        continue;
                    }
                    let text = payload_from_row(row, 3, "history payload")?;
                    let Ok(payload) = serde_json::from_str::<Value>(&text) else {
                        report.invalid_records += 1;
                        continue;
                    };
                    let Some((history_id, timestamp_ms)) = history_record_identity(&payload) else {
                        report.invalid_records += 1;
                        continue;
                    };
                    let subagent_id: Option<String> = row.get(0)?;
                    if table == "subagent_history_items" && subagent_id.is_none() {
                        report.invalid_records += 1;
                        continue;
                    }
                    let ordinal = from_i64(row.get(1)?, "history ordinal")?;
                    if visit(HistoryRecord {
                        session_id,
                        current_cwd: project,
                        subagent_id: subagent_id.as_deref(),
                        ordinal,
                        history_id,
                        timestamp_ms,
                        payload: &payload,
                    })
                    .is_break()
                    {
                        report.stopped = true;
                        break 'sessions;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn repair_permission_reviews(
        state_dir: &StateDir,
        expected: &RawPermissionSnapshot,
        replacement: &RawPermissionSnapshot,
    ) -> Result<PathBuf, SessionError> {
        validate_permission_replacement(expected, replacement)?;
        let mut database = Self::open_permission_admin(state_dir)?;
        if database.raw_permission_snapshot()? != *expected {
            return Err(invalid_permission_repair());
        }
        let backup_path = state_dir.path().join(format!(
            "{SESSIONS_DB_FILE}.permission-review-{}.bak",
            CaudraId::generate()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options
            .mode(OWNER_FILE_MODE)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
        let backup_file = options.open(&backup_path).map_err(StorageError::from)?;
        let mut destination = Connection::open_with_flags(
            &backup_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        Backup::new(&database.connection, &mut destination)?.run_to_completion(
            BACKUP_PAGES_PER_STEP,
            Duration::ZERO,
            None,
        )?;
        quick_check_on(&destination)?;
        destination.close().map_err(|(_, error)| error)?;
        backup_file.sync_all().map_err(StorageError::from)?;
        #[cfg(unix)]
        File::open(state_dir.path())
            .map_err(StorageError::from)?
            .sync_all()
            .map_err(StorageError::from)?;
        let transaction = database
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if raw_permission_snapshot_on(&transaction)? != *expected {
            return Err(invalid_permission_repair());
        }
        if expected.persistent != replacement.persistent {
            let changed = transaction.execute(
                "UPDATE state SET value = ?1 WHERE scope = ?2 AND key = ?3 AND value = ?4",
                params![
                    replacement.persistent,
                    STATE_SCOPE_GLOBAL,
                    PERMISSION_RULES_KEY,
                    expected.persistent
                ],
            )?;
            if changed != 1 {
                return Err(invalid_permission_repair());
            }
        }
        for (before, after) in expected.sessions.iter().zip(&replacement.sessions) {
            if before.metadata == after.metadata {
                continue;
            }
            let delta = to_i64(after.metadata.len(), "metadata bytes")?
                - to_i64(before.metadata.len(), "metadata bytes")?;
            let changed = transaction.execute(
                "UPDATE sessions SET metadata = ?1, logical_bytes = logical_bytes + ?2, write_version = write_version + 1 WHERE id = ?3 AND write_version = ?4 AND metadata = ?5 AND logical_bytes + ?2 >= 0",
                params![after.metadata, delta, before.id.as_bytes().as_slice(), before.write_version, before.metadata],
            )?;
            if changed != 1 {
                return Err(invalid_permission_repair());
            }
        }
        transaction.commit()?;
        Ok(backup_path)
    }

    pub fn stats(&self) -> Result<SessionStorageStats, SessionError> {
        let path = self.path();
        let database_bytes = file_len(&path);
        let wal_bytes = file_len(&PathBuf::from(format!("{}-wal", path.display())));
        let shm_bytes = file_len(&PathBuf::from(format!("{}-shm", path.display())));
        let page_size = pragma_u64(&self.connection, "page_size")?;
        let page_count = pragma_u64(&self.connection, "page_count")?;
        let freelist_count = pragma_u64(&self.connection, "freelist_count")?;
        let auto_vacuum: i64 = self
            .connection
            .pragma_query_value(None, "auto_vacuum", |row| row.get(0))?;
        let schema_version: i64 =
            self.connection
                .pragma_query_value(None, "user_version", |row| row.get(0))?;
        let (
            session_count,
            pinned_count,
            trimmed_count,
            history_item_count,
            tool_output_count,
            subagent_item_count,
            logical_bytes,
        ) = self.connection.query_row(
            "SELECT count(*), coalesce(sum(pinned), 0),\
                    coalesce(sum(trimmed_at IS NOT NULL AND trimmed_at >= updated_at), 0),\
                    coalesce(sum(history_item_count), 0),\
                    coalesce(sum(tool_output_count), 0),\
                    coalesce(sum(subagent_item_count), 0),\
                    coalesce(sum(logical_bytes), 0) FROM sessions",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )?;
        let pending_cleanup_jobs: i64 =
            self.connection
                .query_row("SELECT count(*) FROM cleanup_jobs", [], |row| row.get(0))?;
        let workflow = workflow_totals(&self.connection)?;
        let state_path = self.state_dir.path();
        Ok(SessionStorageStats {
            database_bytes,
            wal_bytes,
            shm_bytes,
            page_size,
            page_count,
            freelist_count,
            auto_vacuum,
            schema_version,
            session_count: from_i64(session_count, "session count")?,
            pinned_count: from_i64(pinned_count, "pinned count")?,
            trimmed_count: from_i64(trimmed_count, "trimmed count")?,
            history_item_count: from_i64(history_item_count, "history item count")?,
            tool_output_count: from_i64(tool_output_count, "tool output count")?,
            subagent_item_count: from_i64(subagent_item_count, "subagent item count")?,
            logical_bytes: from_i64(logical_bytes, "logical bytes")?,
            workflow_run_count: workflow.run_count,
            workflow_call_count: workflow.call_count,
            workflow_bytes: workflow.bytes,
            tool_output_file_bytes: directory_bytes(&state_path.join(TOOL_OUTPUT_DIR)),
            snapshot_bytes: directory_bytes(&state_path.join(SESSION_SNAPSHOT_DIR)),
            archive_bytes: directory_bytes(
                &state_path
                    .join(super::SESSIONS_DIR)
                    .join(super::ARCHIVE_DIR),
            ),
            pending_cleanup_jobs: from_i64(pending_cleanup_jobs, "pending cleanup jobs")?,
        })
    }

    /// Records that a session was opened without touching `updated_at` or
    /// `write_version`, so list order and in-flight delta saves are unaffected.
    pub fn mark_opened(&self, id: CaudraId) -> Result<(), SessionError> {
        self.connection.execute(
            "UPDATE sessions SET last_opened_at = unixepoch() WHERE id = ?1",
            params![id.as_bytes().as_slice()],
        )?;
        Ok(())
    }

    pub fn set_pinned(&self, id: CaudraId, pinned: bool) -> Result<(), SessionError> {
        let changed = self.connection.execute(
            "UPDATE sessions SET pinned = ?2 WHERE id = ?1",
            params![id.as_bytes().as_slice(), i64::from(pinned)],
        )?;
        if changed == 0 {
            return Err(StorageError::NotFound(id.to_string()).into());
        }
        Ok(())
    }

    /// Folds one turn into its hourly bucket. Accumulating rather than
    /// inserting keeps the table bounded by time instead of by turn count.
    pub fn record_usage(&self, entry: &LedgerEntry) -> Result<(), SessionError> {
        let (cost, priced, unpriced) = match entry.cost {
            Some(cost) => (cost, 1, 0),
            None => (0.0, 0, 1),
        };
        // RAISE(FAIL) can retain statement changes; a failed contribution must be safe to retry.
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO usage_ledger (bucket_start, provider, model, cwd, purpose, ephemeral, \
                 subscription, input_tokens, output_tokens, cache_creation, cache_read, cost, \
                 priced_turns, unpriced_turns) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
             ON CONFLICT(bucket_start, provider, model, cwd, purpose, ephemeral, subscription) \
             DO UPDATE SET \
                 input_tokens = input_tokens + excluded.input_tokens, \
                 output_tokens = output_tokens + excluded.output_tokens, \
                 cache_creation = cache_creation + excluded.cache_creation, \
                 cache_read = cache_read + excluded.cache_read, \
                 cost = cost + excluded.cost, \
                 priced_turns = priced_turns + excluded.priced_turns, \
                 unpriced_turns = unpriced_turns + excluded.unpriced_turns",
            params![
                entry.bucket_start,
                entry.provider,
                entry.model,
                entry.cwd,
                entry.purpose.storage_name(),
                i64::from(entry.ephemeral),
                i64::from(entry.subscription),
                i64::from(entry.usage.input),
                i64::from(entry.usage.output),
                i64::from(entry.usage.cache_creation),
                i64::from(entry.usage.cache_read),
                cost,
                priced,
                unpriced,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Reattributes all recorded usage for one exact cwd, even without surviving sessions.
    pub fn relocate_project_usage(
        &mut self,
        source: &str,
        destination: &str,
    ) -> Result<ProjectUsageRelocation, SessionError> {
        validate_relocation_paths(Some(source), destination)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = relocate_project_usage_on(&transaction, source, destination)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn usage_buckets(&self, since: Option<i64>) -> Result<Vec<UsageBucket>, SessionError> {
        let mut statement = self.connection.prepare(
            "SELECT bucket_start, provider, model, cwd, purpose, ephemeral, subscription, \
                    input_tokens, output_tokens, cache_creation, cache_read, cost, \
                    priced_turns, unpriced_turns  \
             FROM usage_ledger WHERE ?1 IS NULL OR bucket_start >= ?1  \
             ORDER BY bucket_start DESC, provider ASC, model ASC, cwd ASC, purpose ASC",
        )?;
        let mut rows = statement.query(params![since])?;
        let mut buckets = Vec::new();
        while let Some(row) = rows.next()? {
            buckets.push(UsageBucket {
                bucket_start: row.get(0)?,
                provider: row.get(1)?,
                model: row.get(2)?,
                cwd: row.get(3)?,
                purpose: row.get(4)?,
                ephemeral: row.get::<_, i64>(5)? != 0,
                subscription: row.get::<_, i64>(6)? != 0,
                input: from_i64(row.get(7)?, "usage_ledger.input_tokens")?,
                output: from_i64(row.get(8)?, "usage_ledger.output_tokens")?,
                cache_creation: from_i64(row.get(9)?, "usage_ledger.cache_creation")?,
                cache_read: from_i64(row.get(10)?, "usage_ledger.cache_read")?,
                cost: row.get(11)?,
                priced_turns: from_i64(row.get(12)?, "usage_ledger.priced_turns")?,
                unpriced_turns: from_i64(row.get(13)?, "usage_ledger.unpriced_turns")?,
            });
        }
        Ok(buckets)
    }

    pub fn prune_usage_before(&self, bucket_start: i64) -> Result<usize, SessionError> {
        Ok(self.connection.execute(
            "DELETE FROM usage_ledger WHERE bucket_start < ?1",
            params![bucket_start],
        )?)
    }

    /// Folds one call into its hourly bucket, the way [`Self::record_usage`]
    /// folds one turn.
    pub fn record_tool_call(&self, entry: &ToolLedgerEntry) -> Result<(), SessionError> {
        let transaction = self.connection.unchecked_transaction()?;
        merge_tool_bucket(&transaction, entry)?;
        transaction.commit()?;
        Ok(())
    }

    /// Recorded calls, optionally from `since` and optionally for one exact
    /// `cwd`, which is what separates the project answer from the global one.
    pub fn tool_buckets(
        &self,
        since: Option<i64>,
        cwd: Option<&str>,
    ) -> Result<Vec<ToolBucket>, SessionError> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {TOOL_BUCKET_COLUMNS} FROM tool_ledger \
             WHERE (?1 IS NULL OR bucket_start >= ?1) AND (?2 IS NULL OR cwd = ?2) \
             ORDER BY bucket_start DESC, tool ASC, source ASC, cwd ASC, outcome ASC"
        ))?;
        let mut rows = statement.query(params![since, cwd])?;
        let mut buckets = Vec::new();
        while let Some(row) = rows.next()? {
            buckets.push(tool_bucket_from_row(row)?);
        }
        Ok(buckets)
    }

    pub fn prune_tool_calls_before(&self, bucket_start: i64) -> Result<usize, SessionError> {
        Ok(self.connection.execute(
            "DELETE FROM tool_ledger WHERE bucket_start < ?1",
            params![bucket_start],
        )?)
    }

    /// Scalar facts for every session, or for one working directory. Payload
    /// tables are never joined to plan retention; workflow rows contribute
    /// their accounted size only.
    pub fn session_facts(&self, cwd: Option<&str>) -> Result<Vec<SessionFacts>, SessionError> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT id, title, cwd, created_at, updated_at, last_opened_at, pinned, trimmed_at,\
                    logical_bytes + {SESSION_WORKFLOW_BYTES}, \
                    json_extract(metadata, '$.pending_revert') IS NOT NULL \
             FROM sessions WHERE ?1 IS NULL OR cwd = ?1 \
             ORDER BY max(updated_at, coalesce(last_opened_at, 0)) DESC, id DESC"
        ))?;
        let mut rows = statement.query(params![cwd])?;
        let mut facts = Vec::new();
        while let Some(row) = rows.next()? {
            facts.push(SessionFacts {
                id: id_from_row(row, 0)?,
                title: row.get(1)?,
                cwd: row.get(2)?,
                created_at: from_i64(row.get(3)?, "sessions.created_at")?,
                updated_at: from_i64(row.get(4)?, "sessions.updated_at")?,
                last_opened_at: row
                    .get::<_, Option<i64>>(5)?
                    .map(|value| from_i64(value, "sessions.last_opened_at"))
                    .transpose()?,
                pinned: row.get::<_, i64>(6)? != 0,
                trimmed_at: row
                    .get::<_, Option<i64>>(7)?
                    .map(|value| from_i64(value, "sessions.trimmed_at"))
                    .transpose()?,
                logical_bytes: from_i64(row.get(8)?, "sessions.logical_bytes")?,
                pending_revert: row.get::<_, i64>(9)? != 0,
            });
        }
        Ok(facts)
    }

    /// Demotes a session to the transcript tier. The lease proves nobody has
    /// the session open, so the counters this rewrites cannot race a writer.
    pub fn trim(&mut self, lease: &SessionLease) -> Result<TrimReport, SessionError> {
        let id = lease.id();
        lease.validate(&self.state_dir, id)?;
        let artifact_bytes = self.artifact_bytes(id);
        let _artifact_lock = lock_session_artifacts(&self.state_dir)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let root = root_on(&transaction, id)?;
        let (rows, bytes) = transaction.query_row(
            "SELECT count(*), coalesce(sum(byte_count), 0) FROM tool_outputs \
             WHERE session_id = ?1 AND byte_count > ?2",
            params![id.as_bytes().as_slice(), TRIM_KEEP_OUTPUT_BYTES],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        transaction.execute(
            "DELETE FROM tool_outputs WHERE session_id = ?1 AND byte_count > ?2",
            params![id.as_bytes().as_slice(), TRIM_KEEP_OUTPUT_BYTES],
        )?;
        let workflow = trim_workflow_runs(&transaction, id)?;
        let removed_rows = from_i64_usize(rows, "trimmed tool output rows")?;
        let removed_bytes = from_i64_usize(bytes, "trimmed tool output bytes")?;
        let tool_output_count = root.tool_output_count.checked_sub(removed_rows);
        let logical_bytes = root.logical_bytes.checked_sub(removed_bytes);
        let (Some(tool_output_count), Some(logical_bytes)) = (tool_output_count, logical_bytes)
        else {
            return Err(SessionError::CorruptDatabaseValue {
                field: "sessions.tool_output_count",
                reason: "trim removed more than the root row accounts for".into(),
            });
        };
        transaction.execute(
            "UPDATE sessions SET tool_output_count = ?2, logical_bytes = ?3,\
                 trimmed_at = unixepoch(), write_version = write_version + 1 \
             WHERE id = ?1 AND write_version = ?4",
            params![
                id.as_bytes().as_slice(),
                to_i64(tool_output_count, "tool output count")?,
                to_i64(logical_bytes, "logical_bytes")?,
                root.write_version,
            ],
        )?;
        enqueue_cleanup_jobs(&transaction, id)?;
        transaction.commit()?;
        drop(_artifact_lock);
        // The lease is still held, so the jobs run now instead of waiting
        // for the next repository open.
        for kind in ARTIFACT_CLEANUP_KINDS {
            self.run_cleanup_job(id, kind)?;
        }
        Ok(TrimReport {
            tool_output_rows: from_i64(rows, "trimmed tool output rows")?,
            tool_output_row_bytes: from_i64(bytes, "trimmed tool output bytes")?,
            workflow_call_rows: workflow.call_rows,
            workflow_call_bytes: workflow.call_bytes,
            workflow_event_rows: workflow.event_rows,
            workflow_event_bytes: workflow.event_bytes,
            artifact_bytes,
        })
    }

    /// Bytes under every external artifact directory of one session.
    pub fn artifact_bytes(&self, id: CaudraId) -> u64 {
        let state_path = self.state_dir.path();
        let name = id.to_string();
        let directories: u64 = [
            state_path.join(TOOL_OUTPUT_DIR).join(&name),
            state_path.join(SESSION_SNAPSHOT_DIR).join(&name),
            state_path
                .join(super::SESSIONS_DIR)
                .join(super::ARCHIVE_DIR)
                .join(&name),
        ]
        .iter()
        .map(|path| directory_bytes(path))
        .sum();
        directories + session_scratch_bytes(&self.state_dir, id)
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }

    pub(crate) fn state_directory(&self) -> &StateDir {
        &self.state_dir
    }

    /// Payload bounds shared with the workflow repository, which lives outside
    /// this module: byte size against the canonical payload limit, then
    /// nesting depth.
    pub(crate) fn validate_payload_json(
        kind: &'static str,
        value: &str,
    ) -> Result<(), SessionError> {
        validate_len(kind, value.len(), MAX_PAYLOAD_BYTES)?;
        validate_json_depth(value)
    }

    pub(crate) fn validate_len(
        kind: &'static str,
        actual: usize,
        maximum: usize,
    ) -> Result<(), SessionError> {
        validate_len(kind, actual, maximum)
    }

    pub fn state_get<T: DeserializeOwned>(
        &self,
        scope: &str,
        key: &str,
    ) -> Result<Option<T>, SessionError> {
        self.connection
            .query_row(
                "SELECT value FROM state WHERE scope = ?1 AND key = ?2",
                params![scope, key],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|value| deserialize_json(&value, "state value"))
            .transpose()
    }

    pub fn state_set<T: Serialize>(
        &self,
        scope: &str,
        key: &str,
        value: &T,
    ) -> Result<(), SessionError> {
        let value = serialize_json(value, "state value", MAX_METADATA_BYTES)?;
        self.connection.execute(
            "INSERT INTO state (scope, key, value, updated_at) \
             VALUES (?1, ?2, ?3, unixepoch()) \
             ON CONFLICT(scope, key) DO UPDATE SET \
                 value = excluded.value, updated_at = excluded.updated_at",
            params![scope, key, value],
        )?;
        Ok(())
    }

    pub fn state_delete(&self, scope: &str, key: &str) -> Result<bool, SessionError> {
        Ok(self.connection.execute(
            "DELETE FROM state WHERE scope = ?1 AND key = ?2",
            params![scope, key],
        )? != 0)
    }

    /// Read-modify-write under one write transaction, so two processes
    /// updating the same value cannot lose each other's change. A missing
    /// row starts from `T::default()`.
    pub fn state_update<T, R>(
        &mut self,
        scope: &str,
        key: &str,
        update: impl FnOnce(&mut T) -> R,
    ) -> Result<R, SessionError>
    where
        T: DeserializeOwned + Serialize + Default,
    {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut value: T = transaction
            .query_row(
                "SELECT value FROM state WHERE scope = ?1 AND key = ?2",
                params![scope, key],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|value| deserialize_json(&value, "state value"))
            .transpose()?
            .unwrap_or_default();
        let result = update(&mut value);
        let value = serialize_json(&value, "state value", MAX_METADATA_BYTES)?;
        transaction.execute(
            "INSERT INTO state (scope, key, value, updated_at) \
             VALUES (?1, ?2, ?3, unixepoch()) \
             ON CONFLICT(scope, key) DO UPDATE SET \
                 value = excluded.value, updated_at = excluded.updated_at",
            params![scope, key, value],
        )?;
        transaction.commit()?;
        Ok(result)
    }

    /// Like [`Self::state_update`], but leaves the row untouched when the
    /// domain-specific update rejects its current or proposed value.
    pub fn state_try_update<T, R, E>(
        &mut self,
        scope: &str,
        key: &str,
        update: impl FnOnce(&mut T) -> Result<R, E>,
    ) -> Result<Result<R, E>, SessionError>
    where
        T: DeserializeOwned + Serialize + Default,
    {
        self.state_try_update_checked(scope, key, update, || Ok(()))
    }

    pub(crate) fn state_try_update_checked<T, R, E>(
        &mut self,
        scope: &str,
        key: &str,
        update: impl FnOnce(&mut T) -> Result<R, E>,
        before_commit: impl FnOnce() -> Result<(), E>,
    ) -> Result<Result<R, E>, SessionError>
    where
        T: DeserializeOwned + Serialize + Default,
    {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut value: T = transaction
            .query_row(
                "SELECT value FROM state WHERE scope = ?1 AND key = ?2",
                params![scope, key],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|value| deserialize_json(&value, "state value"))
            .transpose()?
            .unwrap_or_default();
        let result = match update(&mut value) {
            Ok(result) => result,
            Err(error) => return Ok(Err(error)),
        };
        let value = serialize_json(&value, "state value", MAX_METADATA_BYTES)?;
        transaction.execute(
            "INSERT INTO state (scope, key, value, updated_at) \
             VALUES (?1, ?2, ?3, unixepoch()) \
             ON CONFLICT(scope, key) DO UPDATE SET \
                 value = excluded.value, updated_at = excluded.updated_at",
            params![scope, key, value],
        )?;
        if let Err(error) = before_commit() {
            return Ok(Err(error));
        }
        transaction.commit()?;
        Ok(Ok(result))
    }

    pub fn global_state_get<T: DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, SessionError> {
        self.state_get(STATE_SCOPE_GLOBAL, key)
    }

    pub fn global_state_set<T: Serialize>(&self, key: &str, value: &T) -> Result<(), SessionError> {
        self.state_set(STATE_SCOPE_GLOBAL, key, value)
    }

    pub fn checkpoint(&self, truncate: bool) -> Result<CheckpointResult, SessionError> {
        // PASSIVE is normal maintenance. TRUNCATE is explicit idle maintenance
        // because long readers, not the size setting, determine active WAL size.
        let mode = if truncate { "TRUNCATE" } else { "PASSIVE" };
        let sql = format!("PRAGMA wal_checkpoint({mode})");
        let (busy, log_frames, checkpointed_frames) =
            self.connection.query_row(&sql, [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
        Ok(CheckpointResult {
            busy: from_i64(busy, "checkpoint busy")?,
            log_frames: from_i64(log_frames, "checkpoint log frames")?,
            checkpointed_frames: from_i64(checkpointed_frames, "checkpoint completed frames")?,
        })
    }

    /// Reclaims up to `pages` freelist pages. Full VACUUM is never an
    /// automatic startup or write-path operation. Returns the pages freed.
    pub fn incremental_vacuum(&self, pages: u32) -> Result<u64, SessionError> {
        // The pragma yields one row per freed page, so it must be stepped to
        // completion; a single step frees exactly one page.
        let mut statement = self
            .connection
            .prepare(&format!("PRAGMA incremental_vacuum({pages})"))?;
        let mut rows = statement.query([])?;
        let mut freed: u64 = 0;
        while rows.next()?.is_some() {
            freed += 1;
            if freed.is_multiple_of(RECLAIM_PROGRESS_INTERVAL) {
                PRUNE.report(PruneEvent::Reclaiming {
                    done: freed,
                    total: u64::from(pages),
                });
            }
        }
        PRUNE.report(PruneEvent::Reclaimed { pages: freed });
        Ok(freed)
    }

    pub fn freelist_pages(&self) -> Result<u64, SessionError> {
        pragma_u64(&self.connection, "freelist_count")
    }

    pub fn due_cleanup_jobs(&self) -> Result<u64, SessionError> {
        let due: i64 = self.connection.query_row(
            "SELECT count(*) FROM cleanup_jobs WHERE next_attempt_ms <= unixepoch('subsec') * 1000",
            [],
            |row| row.get(0),
        )?;
        from_i64(due, "due cleanup jobs")
    }

    pub fn quick_check(&self) -> Result<(), SessionError> {
        quick_check_on(&self.connection)
    }

    pub fn save<M, U, T>(
        &mut self,
        session: &Session<M, U, T>,
        cursor: Option<&SessionCursor>,
    ) -> Result<SessionCursor, SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        if session
            .workspace_binding
            .as_deref()
            .is_some_and(|binding| !binding.is_local())
            && WorkspacePath::new(&session.cwd).is_err()
        {
            return Err(SessionError::InvalidRemoteCwd);
        }
        if let Some(cursor) = cursor {
            if cursor.session_id != session.id {
                return Err(SessionError::IdMismatch {
                    log_id: cursor.session_id,
                    given_id: session.id,
                });
            }
            let base_write_version = session.base_write_version.load(Ordering::Acquire);
            if base_write_version != cursor.write_version {
                return Err(SessionError::ConcurrentSessionWriter {
                    id: session.id,
                    expected: base_write_version,
                    actual: cursor.write_version,
                });
            }
        }
        // Epoch and rewrite counters classify append safety in memory; the
        // persisted version remains authoritative across processes.
        if let Some(cursor) = cursor
            && cursor.session_id == session.id
            && cursor.saved_epoch == session.epoch
            && cursor.saved_rewrites == session.rewrites
            && cursor.saved_history_count <= session.messages.len()
            && cursor
                .saved_tool_ids
                .iter()
                .all(|id| session.tool_outputs.contains_key(id))
            && cursor.saved_subagent_counts.iter().all(|(id, &count)| {
                session
                    .subagent_messages
                    .get(id)
                    .is_some_and(|messages| count <= messages.len())
            })
        {
            return self.save_delta(session, cursor);
        }
        self.save_full(session, cursor.map(SessionCursor::write_version))
    }

    pub fn recreate<M, U, T>(
        &mut self,
        session: &Session<M, U, T>,
        recreation: &SessionRecreation,
    ) -> Result<SessionCursor, SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        if recreation.session_id != session.id {
            return Err(SessionError::IdMismatch {
                log_id: recreation.session_id,
                given_id: session.id,
            });
        }
        let deleted_write_version = recreation.deleted_write_version;
        validate_scalars(session)?;
        let mut serialized = SerializedSession::new(session)?;
        // Cleanup performs slow filesystem work outside SQLite. This lock
        // closes the gap between its tombstone check and deletion so a new
        // canonical generation cannot commit until old cleanup has finished.
        let _artifact_lock = lock_session_artifacts(&self.state_dir)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        match root_on(&transaction, session.id) {
            Ok(_) => return Err(SessionError::AlreadyExists { id: session.id }),
            Err(SessionError::Storage(StorageError::NotFound(_))) => {}
            Err(error) => return Err(error),
        }
        let previous_root_bytes = serialized.root.bytes();
        serialized
            .root
            .replace_permissions(&transaction, recreation.permissions.as_deref())?;
        serialized.logical_bytes =
            serialized.logical_bytes - previous_root_bytes + serialized.root.bytes();
        // Ordinary saves may never cross a tombstone. Only the writer that
        // completed the ordered delete receives the recreation token; advancing
        // its version prevents an old generation from matching a new one.
        match deleted_version(&transaction, session.id)? {
            Some(actual) if actual == deleted_write_version => {}
            Some(actual) => {
                return Err(SessionError::ConcurrentSessionWriter {
                    id: session.id,
                    expected: deleted_write_version,
                    actual,
                });
            }
            None => return Err(StorageError::NotFound(session.id.to_string()).into()),
        }
        let write_version = deleted_write_version.checked_add(1).ok_or_else(|| {
            SessionError::CorruptDatabaseValue {
                field: "session_tombstones.deleted_version",
                reason: "write version overflow".into(),
            }
        })?;
        insert_root(&transaction, session, &serialized)?;
        transaction.execute(
            "UPDATE sessions SET write_version = ?2, permission_generation = ?3 WHERE id = ?1",
            params![
                session.id.as_bytes().as_slice(),
                write_version,
                to_i64(recreation.permission_generation, "permission generation")?
                    .checked_add(1)
                    .ok_or_else(|| SessionError::CorruptDatabaseValue {
                        field: "permission generation",
                        reason: "generation overflow".into()
                    })?
            ],
        )?;
        insert_children(&transaction, session, &serialized)?;
        transaction.execute(
            "DELETE FROM session_tombstones WHERE session_id = ?1",
            params![session.id.as_bytes().as_slice()],
        )?;
        transaction.execute(
            "DELETE FROM cleanup_jobs WHERE session_id = ?1",
            params![session.id.as_bytes().as_slice()],
        )?;
        transaction.commit()?;
        session
            .base_write_version
            .store(write_version, Ordering::Release);
        session
            .write_version
            .store(write_version, Ordering::Release);
        Ok(SessionCursor::new(
            session,
            write_version,
            serialized.logical_bytes,
            serialized.root_bytes(),
            serialized.task_spec_bytes(),
        ))
    }

    pub fn load<M, U, T>(&self, id: CaudraId) -> Result<Session<M, U, T>, SessionError>
    where
        M: DeserializeOwned,
        U: DeserializeOwned + Default,
        T: DeserializeOwned,
    {
        self.load_with_cursor(id).map(|(session, _)| session)
    }

    pub fn load_with_cursor<M, U, T>(
        &self,
        id: CaudraId,
    ) -> Result<(Session<M, U, T>, SessionCursor), SessionError>
    where
        M: DeserializeOwned,
        U: DeserializeOwned + Default,
        T: DeserializeOwned,
    {
        // Root counters, payload rows, and the resulting cursor must describe
        // one snapshot; mixing autocommit reads can lose references.
        let transaction = self.connection.unchecked_transaction()?;
        let loaded = load_on(&transaction, id)?;
        transaction.commit()?;
        Ok(loaded)
    }

    pub fn list(&self, cwd: &str) -> Result<Vec<SessionSummary>, SessionError> {
        // Listing is intentionally scalar-only; payload tables are never joined
        // merely to render or select a session.
        let mut statement = self.connection.prepare(
            "SELECT id, title, updated_at FROM sessions \
             WHERE cwd = ?1 ORDER BY updated_at DESC, id DESC",
        )?;
        let mut rows = statement.query(params![cwd])?;
        let mut summaries = Vec::new();
        while let Some(row) = rows.next()? {
            summaries.push(SessionSummary {
                id: id_from_row(row, 0)?,
                title: row.get(1)?,
                updated_at: from_i64(row.get(2)?, "sessions.updated_at")?,
            });
        }
        Ok(summaries)
    }

    pub fn local_session_locations(&self) -> Result<Vec<SessionLocation>, SessionError> {
        local_session_locations_on(&self.connection, None)
    }

    pub fn relocate_sessions(
        &mut self,
        request: &SessionRelocation,
    ) -> Result<SessionRelocationResult, SessionError> {
        self.relocate_sessions_with_tabs(request, &None)
    }

    pub fn relocate_sessions_with_tabs(
        &mut self,
        request: &SessionRelocation,
        destination_tabs: &Option<WorkspaceTabs>,
    ) -> Result<SessionRelocationResult, SessionError> {
        if self.state_dir.is_ephemeral() {
            return Err(SessionError::RelocationUnavailable);
        }
        validate_relocation_paths(request.source_cwd.as_deref(), &request.destination)?;
        if request.include_project_usage && request.source_cwd.is_none() {
            return Err(SessionError::ProjectUsageRequiresSource);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ids: HashSet<_> = request.sessions.iter().map(|session| session.id).collect();
        if ids.len() != request.sessions.len() {
            return Err(SessionError::RelocationSelectionChanged);
        }
        if let Some(source) = &request.source_cwd {
            let members = local_session_locations_on(&transaction, Some(source))?;
            if members.len() != ids.len()
                || members.iter().any(|session| !ids.contains(&session.id))
            {
                return Err(SessionError::RelocationSelectionChanged);
            }
        }
        let local = StoredWorkspaceBinding::local_from_cwd("");
        let mut sources: HashMap<&str, HashSet<CaudraId>> = HashMap::new();
        for expected in &request.sessions {
            ensure_write_version(&transaction, expected.id, expected.write_version)?;
            let root = root_on(&transaction, expected.id)?;
            if root.format_version != SESSION_VERSION {
                return Err(SessionError::VersionMismatch {
                    found: root.format_version,
                    expected: SESSION_VERSION,
                });
            }
            if root.cwd != expected.cwd
                || request
                    .source_cwd
                    .as_ref()
                    .is_some_and(|source| source != &root.cwd)
            {
                return Err(SessionError::RelocationSelectionChanged);
            }
            if (!root.workspace_source.is_empty()
                && root.workspace_source != local.trust_anchor().as_str())
                || (root.workspace_binding != "{}"
                    && !deserialize_json::<StoredWorkspaceBinding>(
                        &root.workspace_binding,
                        "sessions.workspace_binding",
                    )?
                    .is_local())
            {
                return Err(SessionError::RelocationBlocked {
                    id: expected.id,
                    reason: RELOCATION_REMOTE,
                });
            }
            if root.cwd == request.destination {
                continue;
            }
            if !sources.contains_key(expected.cwd.as_str()) {
                let pending = transaction
                    .query_row(
                        "SELECT id FROM sessions WHERE cwd = ?1 AND workspace_source IN ('', ?2) \
                     AND json_extract(metadata, '$.pending_revert') IS NOT NULL LIMIT 1",
                        params![expected.cwd, local.trust_anchor().as_str()],
                        |row| row.get::<_, Vec<u8>>(0),
                    )
                    .optional()?;
                if let Some(id) = pending {
                    return Err(SessionError::RelocationBlocked {
                        id: id_from_bytes(&id, "sessions.id")?,
                        reason: RELOCATION_PENDING_REVERT,
                    });
                }
            }
            let workflow_pending: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM workflow_runs WHERE session_id = ?1 \
                 AND status IN ('active', 'paused', 'budget_limited'))",
                params![expected.id.as_bytes().as_slice()],
                |row| row.get(0),
            )?;
            if workflow_pending {
                return Err(SessionError::RelocationBlocked {
                    id: expected.id,
                    reason: RELOCATION_WORKFLOW,
                });
            }
            let mut metadata: Value = deserialize_json(&root.metadata, "session metadata")?;
            let object =
                metadata
                    .as_object_mut()
                    .ok_or_else(|| SessionError::CorruptDatabaseValue {
                        field: "session metadata",
                        reason: "expected an object".into(),
                    })?;
            for field in RELOCATION_METADATA_FIELDS {
                object.remove(field);
            }
            let metadata = serialize_json(&metadata, "session metadata", MAX_METADATA_BYTES)?;
            let changed = transaction.execute(
                "UPDATE sessions SET cwd = ?1, write_version = write_version + 1, \
                 metadata = ?2, logical_bytes = logical_bytes - length(CAST(metadata AS BLOB)) \
                 + length(CAST(?2 AS BLOB)) WHERE id = ?3 AND write_version = ?4",
                params![
                    request.destination,
                    metadata,
                    expected.id.as_bytes().as_slice(),
                    expected.write_version
                ],
            )?;
            if changed != 1 {
                return Err(SessionError::RelocationSelectionChanged);
            }
            interrupt_runs(
                &transaction,
                expected.id,
                &[WorkflowRunStatus::Failed, WorkflowRunStatus::Cancelled],
            )?;
            sources
                .entry(&expected.cwd)
                .or_default()
                .insert(expected.id);
        }
        let moved = sources.values().map(HashSet::len).sum();
        let project_usage = if moved > 0 && request.include_project_usage {
            request
                .source_cwd
                .as_deref()
                .map(|source| relocate_project_usage_on(&transaction, source, &request.destination))
                .transpose()?
        } else {
            None
        };
        for (source, moved) in &sources {
            remove_relocated_workspace_tabs(&transaction, source, moved)?;
        }
        if moved > 0
            && let Some(tabs) = destination_tabs
        {
            write_relocated_workspace_tabs(&transaction, &request.destination, tabs)?;
        }
        transaction.commit()?;
        Ok(SessionRelocationResult {
            sessions_moved: moved,
            project_usage,
        })
    }

    pub fn latest_id(&self, cwd: &str) -> Result<Option<CaudraId>, SessionError> {
        self.connection
            .query_row(
                "SELECT id FROM sessions WHERE cwd = ?1 \
                 ORDER BY updated_at DESC, id DESC LIMIT 1",
                params![cwd],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .map(|bytes| id_from_bytes(&bytes, "sessions.id"))
            .transpose()
    }

    pub fn list_for_workspace(
        &self,
        binding: &StoredWorkspaceBinding,
    ) -> Result<Vec<SessionSummary>, SessionError> {
        let mut statement = self.connection.prepare(
            "SELECT id, title, updated_at FROM sessions WHERE workspace_source = ?1 \
             AND workspace_authority = ?2 AND workspace_principal = ?3 \
             AND workspace_project = ?4 AND workspace_cursor = ?5 \
             AND workspace_cursor_label IS ?6 \
             ORDER BY updated_at DESC, id DESC",
        )?;
        let mut rows = statement.query(params![
            binding.trust_anchor().as_str(),
            binding.authority_storage_key(),
            binding.principal_id(),
            binding.project_key().as_str(),
            binding.cwd_handle().as_str(),
            binding.cursor_label(),
        ])?;
        let mut summaries = Vec::new();
        while let Some(row) = rows.next()? {
            summaries.push(SessionSummary {
                id: id_from_row(row, 0)?,
                title: row.get(1)?,
                updated_at: from_i64(row.get(2)?, "sessions.updated_at")?,
            });
        }
        Ok(summaries)
    }

    pub fn latest_id_for_workspace(
        &self,
        binding: &StoredWorkspaceBinding,
    ) -> Result<Option<CaudraId>, SessionError> {
        self.connection
            .query_row(
                "SELECT id FROM sessions WHERE workspace_source = ?1 \
                 AND workspace_authority = ?2 AND workspace_principal = ?3 \
                  AND workspace_project = ?4 AND workspace_cursor = ?5 \
                  AND workspace_cursor_label IS ?6 \
                 ORDER BY updated_at DESC, id DESC LIMIT 1",
                params![
                    binding.trust_anchor().as_str(),
                    binding.authority_storage_key(),
                    binding.principal_id(),
                    binding.project_key().as_str(),
                    binding.cwd_handle().as_str(),
                    binding.cursor_label(),
                ],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .map(|bytes| id_from_bytes(&bytes, "sessions.id"))
            .transpose()
    }

    pub fn list_for_workspace_identity(
        &self,
        binding: &StoredWorkspaceBinding,
    ) -> Result<Vec<SessionSummary>, SessionError> {
        let mut statement = self.connection.prepare(
            "SELECT id, title, updated_at FROM sessions WHERE workspace_source = ?1 \
             AND workspace_authority = ?2 AND workspace_principal = ?3 \
             AND workspace_project = ?4 ORDER BY updated_at DESC, id DESC",
        )?;
        let mut rows = statement.query(params![
            binding.trust_anchor().as_str(),
            binding.authority_storage_key(),
            binding.principal_id(),
            binding.project_key().as_str()
        ])?;
        let mut summaries = Vec::new();
        while let Some(row) = rows.next()? {
            summaries.push(SessionSummary {
                id: id_from_row(row, 0)?,
                title: row.get(1)?,
                updated_at: from_i64(row.get(2)?, "sessions.updated_at")?,
            });
        }
        Ok(summaries)
    }

    pub fn write_version(&self, id: CaudraId) -> Result<Option<i64>, SessionError> {
        Ok(self
            .connection
            .query_row(
                "SELECT write_version FROM sessions WHERE id = ?1",
                params![id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn delete(
        &mut self,
        id: CaudraId,
        expected_write_version: Option<i64>,
    ) -> Result<SessionRecreation, SessionError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored = transaction
            .query_row(
                "SELECT write_version, metadata -> '$.structured_permission_rules', permission_generation FROM sessions WHERE id = ?1",
                params![id.as_bytes().as_slice()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, i64>(2)?)),
            )
            .optional()?;
        let actual = stored.as_ref().map(|(version, _, _)| *version);
        let permission_generation = from_i64(
            stored.as_ref().map_or(0, |(_, _, generation)| *generation),
            "sessions.permission_generation",
        )?;
        let permissions = stored.and_then(|(_, permissions, _)| permissions);
        if let (Some(expected), Some(actual)) = (expected_write_version, actual)
            && expected != actual
        {
            return Err(SessionError::ConcurrentSessionWriter {
                id,
                expected,
                actual,
            });
        }
        let removed_existing = actual.is_some();
        let deleted_write_version = match (actual, deleted_version(&transaction, id)?) {
            (Some(version), _) => version,
            (None, None) if expected_write_version.is_none() => -1,
            (None, _) => return Err(StorageError::NotFound(id.to_string()).into()),
        };
        transaction.execute(
            "DELETE FROM sessions WHERE id = ?1",
            params![id.as_bytes().as_slice()],
        )?;
        transaction.execute(
            "INSERT INTO session_tombstones (session_id, deleted_version) \
             VALUES (?1, ?2) \
             ON CONFLICT(session_id) DO UPDATE SET deleted_version = excluded.deleted_version",
            params![id.as_bytes().as_slice(), deleted_write_version],
        )?;
        // Queue external work before commit so a crash can leak artifacts only
        // temporarily; a later repository open resumes these idempotent jobs.
        enqueue_cleanup_jobs(&transaction, id)?;
        transaction.commit()?;
        Ok(SessionRecreation {
            session_id: id,
            deleted_write_version,
            removed_existing,
            permissions,
            permission_generation,
        })
    }

    pub fn tombstone_version(&self, id: CaudraId) -> Result<Option<i64>, SessionError> {
        Ok(self
            .connection
            .query_row(
                "SELECT deleted_version FROM session_tombstones WHERE session_id = ?1",
                params![id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn persisted_session_ids(&self) -> Result<Vec<CaudraId>, SessionError> {
        let mut statement = self
            .connection
            .prepare("SELECT id FROM sessions ORDER BY id")?;
        let mut rows = statement.query([])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            ids.push(id_from_row(row, 0)?);
        }
        Ok(ids)
    }

    pub(crate) fn visit_payload_json(
        &self,
        id: CaudraId,
        mut visit: impl FnMut(&str),
    ) -> Result<(), SessionError> {
        // Orphan cleanup treats this as a reference snapshot. Separate
        // autocommit queries could miss a reference moved between collections.
        let transaction = self.connection.unchecked_transaction()?;
        let metadata = transaction
            .query_row(
                "SELECT metadata FROM sessions WHERE id = ?1",
                params![id.as_bytes().as_slice()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| SessionError::Storage(StorageError::NotFound(id.to_string())))?;
        visit(&metadata);
        for sql in [
            "SELECT payload FROM main_history_items WHERE session_id = ?1",
            "SELECT payload FROM tool_outputs WHERE session_id = ?1",
            "SELECT payload FROM subagent_history_items WHERE session_id = ?1",
        ] {
            let mut statement = transaction.prepare(sql)?;
            let mut rows = statement.query(params![id.as_bytes().as_slice()])?;
            while let Some(row) = rows.next()? {
                let payload = payload_from_row(row, 0, "payload")?;
                visit(&payload);
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Runs every due cleanup job. Returns how many completed.
    pub fn process_cleanup_jobs(&mut self) -> Result<u64, SessionError> {
        // Cleanup is durable work, not an event history: successful rows are
        // removed, while failures retain only bounded retry metadata.
        let jobs = {
            let mut statement = self.connection.prepare(
                "SELECT session_id, kind FROM cleanup_jobs \
                 WHERE next_attempt_ms <= unixepoch('subsec') * 1000 ORDER BY id",
            )?;
            let mut rows = statement.query([])?;
            let mut jobs = Vec::new();
            while let Some(row) = rows.next()? {
                jobs.push((id_from_row(row, 0)?, row.get::<_, String>(1)?));
            }
            jobs
        };
        let mut completed = 0;
        for (id, kind) in jobs {
            if !ARTIFACT_CLEANUP_KINDS.contains(&kind.as_str()) {
                self.record_cleanup_failure(id, &kind, UNKNOWN_CLEANUP_KIND)?;
                continue;
            }
            match self.cleanup_guard(id, &kind)? {
                CleanupGuard::Stale => self.remove_cleanup_job(id, &kind)?,
                CleanupGuard::Deleted => completed += u64::from(self.run_cleanup_job(id, &kind)?),
                // A trimmed session may be reopened at any time. Holding its
                // lease during deletion keeps new artifacts out of harm's way,
                // and the lease precedes the artifact lock as it does in trim.
                CleanupGuard::Trimmed => match SessionLease::acquire(&self.state_dir, id) {
                    Ok(_lease) => completed += u64::from(self.run_cleanup_job(id, &kind)?),
                    Err(SessionError::SessionInUse { .. }) => {
                        self.record_cleanup_failure(id, &kind, SESSION_OPEN_ELSEWHERE)?;
                    }
                    Err(error) => self.record_cleanup_failure(id, &kind, &error.to_string())?,
                },
            }
        }
        Ok(completed)
    }

    fn cleanup_guard(&self, id: CaudraId, kind: &str) -> Result<CleanupGuard, SessionError> {
        let guard = self
            .connection
            .query_row(
                "SELECT \
                     EXISTS(SELECT 1 FROM session_tombstones \
                            WHERE session_id = job.session_id),\
                     EXISTS(SELECT 1 FROM sessions \
                            WHERE id = job.session_id AND trimmed_at IS NOT NULL \
                              AND trimmed_at >= updated_at) \
                 FROM cleanup_jobs AS job WHERE job.session_id = ?1 AND job.kind = ?2",
                params![id.as_bytes().as_slice(), kind],
                |row| Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?)),
            )
            .optional()?;
        Ok(match guard {
            Some((true, _)) => CleanupGuard::Deleted,
            Some((false, true)) => CleanupGuard::Trimmed,
            Some((false, false)) | None => CleanupGuard::Stale,
        })
    }

    /// Deletes one session's artifact directory for `kind`. The caller holds
    /// the session lease when the session still exists. Returns whether the
    /// job completed.
    fn run_cleanup_job(&mut self, id: CaudraId, kind: &str) -> Result<bool, SessionError> {
        // Recreation takes the same artifact lock. It therefore cannot
        // publish a new generation between this guard check and the
        // filesystem deletion, while unrelated SQLite writers stay free.
        let _artifact_lock = match lock_session_artifacts(&self.state_dir) {
            Ok(lock) => lock,
            Err(error) => {
                self.record_cleanup_failure(id, kind, &error.to_string())?;
                return Ok(false);
            }
        };
        if matches!(self.cleanup_guard(id, kind)?, CleanupGuard::Stale) {
            self.remove_cleanup_job(id, kind)?;
            return Ok(false);
        }
        let cleanup = match kind {
            "tool_output" => delete_session_outputs(&self.state_dir, id),
            "archive" => remove_state_directory(
                &self.state_dir,
                &[super::SESSIONS_DIR, super::ARCHIVE_DIR],
                id,
            ),
            "snapshot" => remove_state_directory(&self.state_dir, &[SESSION_SNAPSHOT_DIR], id),
            "workflow_scratch" => remove_scratch_session(&self.state_dir, id),
            _ => unreachable!("cleanup kind validated"),
        };
        match cleanup {
            Ok(()) => {
                self.remove_cleanup_job(id, kind)?;
                Ok(true)
            }
            Err(error) => {
                self.record_cleanup_failure(id, kind, &error.to_string())?;
                Ok(false)
            }
        }
    }

    fn remove_cleanup_job(&self, id: CaudraId, kind: &str) -> Result<(), SessionError> {
        self.connection.execute(
            "DELETE FROM cleanup_jobs WHERE session_id = ?1 AND kind = ?2",
            params![id.as_bytes().as_slice(), kind],
        )?;
        Ok(())
    }

    fn record_cleanup_failure(
        &mut self,
        id: CaudraId,
        kind: &str,
        error: &str,
    ) -> Result<(), SessionError> {
        self.connection.execute(
            "UPDATE cleanup_jobs SET attempts = attempts + 1, last_error = ?3, \
             next_attempt_ms = unixepoch('subsec') * 1000 + ?4 \
             WHERE session_id = ?1 AND kind = ?2",
            params![
                id.as_bytes().as_slice(),
                kind,
                error,
                CLEANUP_RETRY_DELAY_MS
            ],
        )?;
        Ok(())
    }

    fn save_full<M, U, T>(
        &mut self,
        session: &Session<M, U, T>,
        expected_write_version: Option<i64>,
    ) -> Result<SessionCursor, SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        validate_scalars(session)?;
        let mut serialized = SerializedSession::new(session)?;
        let base_write_version = session.base_write_version.load(Ordering::Acquire);
        let expected = expected_write_version
            .or_else(|| (base_write_version >= 0).then_some(base_write_version));
        let archive = expected
            .map(|expected| self.prepare_archive(session.id, session.messages.len(), expected))
            .transpose()?
            .flatten();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous_root_bytes = serialized.root.bytes();
        serialized
            .root
            .fence_permissions(&transaction, session.id)?;
        serialized.logical_bytes =
            serialized.logical_bytes - previous_root_bytes + serialized.root.bytes();
        let write_version = match expected {
            Some(expected) => {
                ensure_write_version(&transaction, session.id, expected)?;
                // The bounded recovery export is already durable under a
                // pending name. Final publication waits for commit.
                update_root(&transaction, session, &serialized, expected)?;
                clear_children(&transaction, session.id)?;
                expected + 1
            }
            None => {
                if is_tombstoned(&transaction, session.id)? {
                    return Err(StorageError::NotFound(session.id.to_string()).into());
                }
                match insert_root(&transaction, session, &serialized) {
                    Ok(()) => {}
                    Err(SessionError::Sqlite(error))
                        if error.sqlite_error_code()
                            == Some(rusqlite::ErrorCode::ConstraintViolation) =>
                    {
                        return Err(SessionError::AlreadyExists { id: session.id });
                    }
                    Err(error) => return Err(error),
                }
                0
            }
        };
        insert_children(&transaction, session, &serialized)?;
        if let Some(archive) = &archive {
            transaction.execute(
                "INSERT INTO pending_archives \
                 (session_id, expected_write_version, pending_name, byte_count) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    archive.session_id.as_bytes().as_slice(),
                    archive.expected_write_version,
                    &archive.pending_name,
                    to_i64(archive.bytes, "pending archive bytes")?
                ],
            )?;
        }
        transaction.commit()?;
        if let Some(mut archive) = archive {
            archive.cleanup_on_drop = false;
            if let Err(error) =
                self.finalize_registered_archive(archive.session_id, &archive.pending_name)
            {
                warn!(%error, session_id = %session.id, "archive publication deferred to startup reconciliation");
            } else if let Err(error) = self.reconcile_pending_archives() {
                warn!(%error, session_id = %session.id, "later pending archives remain for reconciliation");
            }
        }
        session
            .base_write_version
            .store(write_version, Ordering::Release);
        session
            .write_version
            .store(write_version, Ordering::Release);
        Ok(SessionCursor::new(
            session,
            write_version,
            serialized.logical_bytes,
            serialized.root_bytes(),
            serialized.task_spec_bytes(),
        ))
    }

    fn save_delta<M, U, T>(
        &mut self,
        session: &Session<M, U, T>,
        cursor: &SessionCursor,
    ) -> Result<SessionCursor, SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        validate_scalars(session)?;
        let mut root = SerializedRoot::new(session)?;
        let messages = serialize_values(
            &session.messages[cursor.saved_history_count..],
            "history item",
            MAX_PAYLOAD_BYTES,
        )?;
        let tool_outputs = session
            .tool_outputs
            .iter()
            .filter(|(id, _)| !cursor.saved_tool_ids.contains(*id))
            .map(|(id, output)| {
                serialize_json(output.as_ref(), "tool output", MAX_PAYLOAD_BYTES)
                    .map(|payload| (id.clone(), payload))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut subagent_messages = Vec::new();
        for (id, values) in &session.subagent_messages {
            let saved = cursor.saved_subagent_counts.get(id).copied().unwrap_or(0);
            let payloads =
                serialize_values(&values[saved..], "subagent history item", MAX_PAYLOAD_BYTES)?;
            subagent_messages.push((id.clone(), saved, payloads));
        }
        let task_specs = serialize_task_specs(&session.subagent_task_specs)?;
        let added_bytes = messages.iter().map(String::len).sum::<usize>()
            + tool_outputs
                .iter()
                .map(|(_, payload)| payload.len())
                .sum::<usize>()
            + subagent_messages
                .iter()
                .flat_map(|(_, _, values)| values)
                .map(String::len)
                .sum::<usize>();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        root.fence_permissions(&transaction, session.id)?;
        let logical_bytes = cursor
            .logical_bytes
            .checked_sub(cursor.root_bytes + cursor.task_spec_bytes)
            .and_then(|bytes| bytes.checked_add(root.bytes()))
            .and_then(|bytes| bytes.checked_add(task_specs.values().map(String::len).sum()))
            .and_then(|bytes| bytes.checked_add(added_bytes))
            .ok_or_else(|| SessionError::CorruptDatabaseValue {
                field: "sessions.logical_bytes",
                reason: "logical byte accounting overflow".into(),
            })?;
        update_root_values(
            &transaction,
            session,
            &root,
            logical_bytes,
            cursor.write_version,
        )?;
        for (offset, payload) in messages.iter().enumerate() {
            insert_history(
                &transaction,
                session.id,
                cursor.saved_history_count + offset,
                payload,
            )?;
        }
        for (id, payload) in &tool_outputs {
            insert_tool_output(&transaction, session.id, id, payload)?;
        }
        upsert_streams(
            &transaction,
            session.id,
            &session.subagent_messages,
            &task_specs,
        )?;
        for (id, start, payloads) in &subagent_messages {
            for (offset, payload) in payloads.iter().enumerate() {
                insert_subagent_history(&transaction, session.id, id, start + offset, payload)?;
            }
        }
        replace_auxiliary(&transaction, session)?;
        transaction.commit()?;
        let write_version = cursor.write_version + 1;
        session
            .base_write_version
            .store(write_version, Ordering::Release);
        session
            .write_version
            .store(write_version, Ordering::Release);
        Ok(SessionCursor::new(
            session,
            write_version,
            logical_bytes,
            root.bytes(),
            task_specs.values().map(String::len).sum(),
        ))
    }

    fn prepare_archive(
        &self,
        id: CaudraId,
        new_count: usize,
        expected_write_version: i64,
    ) -> Result<Option<PreparedArchive>, SessionError> {
        let root = root_on(&self.connection, id)?;
        if root.write_version != expected_write_version {
            return Err(SessionError::ConcurrentSessionWriter {
                id,
                expected: expected_write_version,
                actual: root.write_version,
            });
        }
        if root.history_item_count <= new_count {
            return Ok(None);
        }
        let (session, _) = self.load_with_cursor::<Value, Value, Value>(id)?;
        let archive_dir = archive_directory(&self.state_dir, id, true)?
            .expect("create=true returns an archive directory");
        let pending_name = format!(
            ".pending-{expected_write_version}-{}.jsonl",
            CaudraId::generate()
        );
        let pending_path = archive_dir.join(&pending_name);
        let mut file = NamedTempFile::new_in(&archive_dir).map_err(StorageError::from)?;
        super::write_full_session(file.as_file_mut(), &session)?;
        file.as_file().sync_data().map_err(StorageError::from)?;
        let bytes = file.as_file().metadata().map_err(StorageError::from)?.len();
        if bytes > super::ARCHIVE_MAX_BYTES {
            warn!(session_id = %id, bytes, "session archive exceeds byte budget; skipping");
            return Ok(None);
        }
        file.persist_noclobber(&pending_path)
            .map_err(|error| StorageError::from(error.error))?;
        crate::sync_parent_dir_durable(&pending_path)?;
        Ok(Some(PreparedArchive {
            session_id: id,
            expected_write_version,
            pending_name,
            pending_path,
            bytes,
            cleanup_on_drop: true,
        }))
    }

    fn reconcile_pending_archives(&mut self) -> Result<(), SessionError> {
        let registered = {
            let mut statement = self.connection.prepare(
                "SELECT session_id, pending_name FROM pending_archives \
                 ORDER BY session_id, expected_write_version, pending_name",
            )?;
            let mut rows = statement.query([])?;
            let mut registered = Vec::new();
            while let Some(row) = rows.next()? {
                registered.push((id_from_row(row, 0)?, row.get::<_, String>(1)?));
            }
            registered
        };
        for (id, pending_name) in registered {
            if let Err(error) = self.finalize_registered_archive(id, &pending_name) {
                warn!(%error, session_id = %id, "pending session archive remains for retry");
            }
        }
        let registered = {
            let mut statement = self
                .connection
                .prepare("SELECT session_id, pending_name FROM pending_archives")?;
            let mut rows = statement.query([])?;
            let mut registered = HashSet::new();
            while let Some(row) = rows.next()? {
                registered.insert((id_from_row(row, 0)?, row.get::<_, String>(1)?));
            }
            registered
        };
        let Some(root) = archive_root(&self.state_dir, false)? else {
            return Ok(());
        };
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(StorageError::from(error).into()),
        };
        for session_entry in entries {
            let session_entry = session_entry.map_err(StorageError::from)?;
            if !session_entry
                .file_type()
                .map_err(StorageError::from)?
                .is_dir()
            {
                continue;
            }
            let Some(id) = session_entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
            else {
                continue;
            };
            let files = match fs::read_dir(session_entry.path()) {
                Ok(files) => files,
                Err(error) => {
                    warn!(%error, session_id = %id, "cannot inspect pending session archives");
                    continue;
                }
            };
            for file in files.flatten() {
                let name = file.file_name();
                if is_pending_archive(&name)
                    && !registered.contains(&(id, name.to_string_lossy().into_owned()))
                    && pending_archive_is_stale(&file.path())
                {
                    let _ = fs::remove_file(file.path());
                }
            }
        }
        Ok(())
    }

    fn finalize_registered_archive(
        &mut self,
        id: CaudraId,
        pending_name: &str,
    ) -> Result<(), SessionError> {
        let _artifact_lock = lock_session_artifacts(&self.state_dir)?;
        let pending = self
            .connection
            .query_row(
                "SELECT expected_write_version, byte_count FROM pending_archives \
                 WHERE session_id = ?1 AND pending_name = ?2",
                params![id.as_bytes().as_slice(), pending_name],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((expected_write_version, bytes)) = pending else {
            return Ok(());
        };
        let lower_pending = self
            .connection
            .query_row(
                "SELECT 1 FROM pending_archives \
                 WHERE session_id = ?1 AND expected_write_version < ?2 LIMIT 1",
                params![id.as_bytes().as_slice(), expected_write_version],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if lower_pending {
            return Ok(());
        }
        let bytes = from_i64(bytes, "pending archive bytes")?;
        let Some(archive_dir) = archive_directory(&self.state_dir, id, false)? else {
            self.connection.execute(
                "DELETE FROM pending_archives WHERE session_id = ?1 AND pending_name = ?2",
                params![id.as_bytes().as_slice(), pending_name],
            )?;
            return Ok(());
        };
        // Filesystem retention can be slow. The artifact lock preserves archive
        // publication order without extending SQLite's global write lock.
        let pending_path = archive_dir.join(pending_name);
        match fs::symlink_metadata(&pending_path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                let existing = super::archives_newest_first(&archive_dir);
                super::prune_archives(existing, bytes).map_err(StorageError::from)?;
                let existing = super::archives_newest_first(&archive_dir);
                let next = existing.first().map_or(0, |archive| archive.seq) + 1;
                let path = archive_dir.join(format!("{next}.jsonl"));
                crate::durable_rename(&pending_path, &path).map_err(StorageError::from)?;
                crate::sync_parent_dir_durable(&path)?;
            }
            Ok(_) => {
                return Err(StorageError::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "refusing archive publication through {}",
                        pending_path.display()
                    ),
                ))
                .into());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(StorageError::from(error).into()),
        }
        super::prune_archives_to_budget(super::archives_newest_first(&archive_dir))
            .map_err(StorageError::from)?;
        self.connection.execute(
            "DELETE FROM pending_archives WHERE session_id = ?1 AND pending_name = ?2",
            params![id.as_bytes().as_slice(), pending_name],
        )?;
        Ok(())
    }
}

impl Drop for PreparedArchive {
    fn drop(&mut self) {
        if self.cleanup_on_drop && fs::remove_file(&self.pending_path).is_ok() {
            crate::sync_parent_dir(&self.pending_path);
        }
    }
}

fn history_record_identity(payload: &Value) -> Option<(CaudraId, u64)> {
    let text = payload.get("id")?.as_str()?;
    if text.len() > MAX_BASE58_UUID_BYTES {
        return None;
    }
    let id = text.parse::<CaudraId>().ok()?;
    if id.to_string() != text {
        return None;
    }
    Some((id, history_uuid_timestamp(id)?))
}

fn history_uuid_timestamp(id: CaudraId) -> Option<u64> {
    let bytes = id.as_bytes();
    if bytes[UUID_VERSION_BYTE] >> UUID_VERSION_SHIFT != UUID_V7
        || bytes[UUID_VARIANT_BYTE] >> UUID_VARIANT_SHIFT != UUID_RFC4122_VARIANT
    {
        return None;
    }
    let timestamp = u64::from_be_bytes([
        0, 0, bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5],
    ]);
    (timestamp != 0).then_some(timestamp)
}

fn invalid_permission_repair() -> SessionError {
    SessionError::CorruptDatabaseValue {
        field: "permission reviews",
        reason: INVALID_PERMISSION_REPAIR.into(),
    }
}

fn raw_permission_snapshot_on(
    connection: &Connection,
) -> Result<RawPermissionSnapshot, SessionError> {
    let persistent: Option<String> = connection
        .query_row(
            "SELECT value FROM state WHERE scope = ?1 AND key = ?2",
            params![STATE_SCOPE_GLOBAL, PERMISSION_RULES_KEY],
            |row| row.get(0),
        )
        .optional()?;
    let mut bytes = persistent.as_ref().map_or(0, String::len);
    validate_len(
        "permission snapshot bytes",
        bytes,
        PERMISSION_SNAPSHOT_BYTES,
    )?;
    let mut sessions = Vec::new();
    let mut statement =
        connection.prepare("SELECT id, write_version, cwd, metadata FROM sessions ORDER BY id")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        validate_len(
            "permission snapshot sessions",
            sessions.len() + 1,
            PERMISSION_SNAPSHOT_SESSIONS,
        )?;
        let metadata: String = row.get(3)?;
        let cwd: String = row.get(2)?;
        bytes = bytes
            .saturating_add(metadata.len())
            .saturating_add(cwd.len());
        validate_len(
            "permission snapshot bytes",
            bytes,
            PERMISSION_SNAPSHOT_BYTES,
        )?;
        sessions.push(RawPermissionSession {
            id: id_from_row(row, 0)?,
            write_version: row.get(1)?,
            cwd,
            metadata,
        });
    }
    Ok(RawPermissionSnapshot {
        persistent,
        sessions,
    })
}

fn validate_permission_replacement(
    expected: &RawPermissionSnapshot,
    replacement: &RawPermissionSnapshot,
) -> Result<(), SessionError> {
    let parse = |text: &str| -> Result<Value, SessionError> {
        serde_json::from_str(text).map_err(|_| invalid_permission_repair())
    };
    match (&expected.persistent, &replacement.persistent) {
        (Some(before), Some(after)) => {
            validate_len("permission rules", after.len(), MAX_PAYLOAD_BYTES)?;
            validate_review_only_change(&parse(before)?, &parse(after)?, true)
                .map_err(|_| invalid_permission_repair())?;
        }
        (None, None) => {}
        _ => return Err(invalid_permission_repair()),
    }
    if expected.sessions.len() != replacement.sessions.len() {
        return Err(invalid_permission_repair());
    }
    for (before, after) in expected.sessions.iter().zip(&replacement.sessions) {
        if before.id != after.id
            || before.cwd != after.cwd
            || before.write_version != after.write_version
        {
            return Err(invalid_permission_repair());
        }
        validate_len("metadata", after.metadata.len(), MAX_METADATA_BYTES)?;
        let mut before = parse(&before.metadata)?;
        let mut after = parse(&after.metadata)?;
        let before_rules = before
            .as_object_mut()
            .ok_or_else(invalid_permission_repair)?
            .remove(PERMISSION_METADATA_KEY);
        let after_rules = after
            .as_object_mut()
            .ok_or_else(invalid_permission_repair)?
            .remove(PERMISSION_METADATA_KEY);
        if before != after {
            return Err(invalid_permission_repair());
        }
        match (before_rules, after_rules) {
            (Some(before), Some(after)) => validate_review_only_change(&before, &after, false)
                .map_err(|_| invalid_permission_repair())?,
            (None, None) => {}
            _ => return Err(invalid_permission_repair()),
        }
    }
    Ok(())
}

fn validate_relocation_paths(source: Option<&str>, destination: &str) -> Result<(), SessionError> {
    validate_len("destination cwd", destination.len(), MAX_PATH_BYTES)?;
    if !Path::new(destination).is_absolute() || destination.contains('\0') {
        return Err(SessionError::InvalidRelocationDestination);
    }
    if let Some(source) = source {
        validate_len("source cwd", source.len(), MAX_PATH_BYTES)?;
        if !Path::new(source).is_absolute() || source.contains('\0') {
            return Err(SessionError::InvalidRelocationSource);
        }
    }
    Ok(())
}

fn relocate_project_usage_on(
    transaction: &Transaction<'_>,
    source: &str,
    destination: &str,
) -> Result<ProjectUsageRelocation, SessionError> {
    if source == destination {
        return Ok(ProjectUsageRelocation::default());
    }
    let buckets_merged: i64 = transaction.query_row(
        "SELECT count(*) FROM usage_ledger AS source JOIN usage_ledger AS destination \
         USING (bucket_start, provider, model, purpose, ephemeral, subscription) \
         WHERE source.cwd = ?1 AND destination.cwd = ?2",
        params![source, destination],
        |row| row.get(0),
    )?;
    let buckets_moved = transaction.execute(
        "INSERT INTO usage_ledger (bucket_start, provider, model, cwd, purpose, ephemeral, \
             subscription, input_tokens, output_tokens, cache_creation, cache_read, cost, \
             priced_turns, unpriced_turns) \
         SELECT bucket_start, provider, model, ?2, purpose, ephemeral, subscription, \
             input_tokens, output_tokens, cache_creation, cache_read, cost, priced_turns, unpriced_turns \
         FROM usage_ledger WHERE cwd = ?1 \
         ON CONFLICT(bucket_start, provider, model, cwd, purpose, ephemeral, subscription) \
         DO UPDATE SET \
             input_tokens = input_tokens + excluded.input_tokens, \
             output_tokens = output_tokens + excluded.output_tokens, \
             cache_creation = cache_creation + excluded.cache_creation, \
             cache_read = cache_read + excluded.cache_read, \
             cost = cost + excluded.cost, \
             priced_turns = priced_turns + excluded.priced_turns, \
             unpriced_turns = unpriced_turns + excluded.unpriced_turns",
        params![source, destination],
    )?;
    transaction.execute("DELETE FROM usage_ledger WHERE cwd = ?1", params![source])?;
    // Tool history is keyed by the same cwd, so leaving it behind would move a
    // project's spend while its activity stayed with the old path.
    let tool_buckets_merged: i64 = transaction.query_row(
        // Aliased `from`/`into` rather than `source`/`destination`, because
        // `source` is also one of the joined columns.
        "SELECT count(*) FROM tool_ledger AS from_cwd JOIN tool_ledger AS into_cwd \
         USING (bucket_start, tool, source, outcome) \
         WHERE from_cwd.cwd = ?1 AND into_cwd.cwd = ?2",
        params![source, destination],
        |row| row.get(0),
    )?;
    // Read, clear, then merge back under the new path. A histogram cannot be
    // folded in SQL, and clearing first keeps a move onto the same path from
    // counting its own rows twice.
    let mut moving = Vec::new();
    {
        let mut statement = transaction.prepare(&format!(
            "SELECT {TOOL_BUCKET_COLUMNS} FROM tool_ledger WHERE cwd = ?1"
        ))?;
        let mut rows = statement.query(params![source])?;
        while let Some(row) = rows.next()? {
            moving.push(tool_bucket_from_row(row)?);
        }
    }
    transaction.execute("DELETE FROM tool_ledger WHERE cwd = ?1", params![source])?;
    for bucket in &moving {
        merge_tool_bucket(
            transaction,
            &ToolLedgerEntry {
                bucket_start: bucket.bucket_start,
                tool: &bucket.tool,
                source: &bucket.source,
                cwd: destination,
                outcome: bucket.outcome,
                calls: bucket.calls,
                duration_ms: bucket.duration_ms,
                tokens: bucket.tokens,
                latency: &bucket.latency,
            },
        )?;
    }
    let tool_buckets_moved = moving.len();
    Ok(ProjectUsageRelocation {
        buckets_moved,
        buckets_merged: from_i64_usize(buckets_merged, "usage_ledger collisions")?,
        tool_buckets_moved,
        tool_buckets_merged: from_i64_usize(tool_buckets_merged, "tool_ledger collisions")?,
    })
}

fn local_session_locations_on(
    connection: &Connection,
    cwd: Option<&str>,
) -> Result<Vec<SessionLocation>, SessionError> {
    let local = StoredWorkspaceBinding::local_from_cwd("");
    let mut statement = connection.prepare(
        "SELECT id, title, cwd, updated_at, write_version FROM sessions \
         WHERE workspace_source IN ('', ?1) AND (?2 IS NULL OR cwd = ?2) \
         ORDER BY updated_at DESC, id DESC",
    )?;
    let mut rows = statement.query(params![local.trust_anchor().as_str(), cwd])?;
    let mut locations = Vec::new();
    while let Some(row) = rows.next()? {
        locations.push(SessionLocation {
            id: id_from_row(row, 0)?,
            title: row.get(1)?,
            cwd: row.get(2)?,
            updated_at: from_i64(row.get(3)?, "sessions.updated_at")?,
            write_version: row.get(4)?,
        });
    }
    Ok(locations)
}

fn load_on<M, U, T>(
    connection: &Connection,
    id: CaudraId,
) -> Result<(Session<M, U, T>, SessionCursor), SessionError>
where
    M: DeserializeOwned,
    U: DeserializeOwned + Default,
    T: DeserializeOwned,
{
    let root = root_on(connection, id)?;
    if root.format_version != SESSION_VERSION {
        return Err(SessionError::VersionMismatch {
            found: root.format_version,
            expected: SESSION_VERSION,
        });
    }
    if root.logical_bytes > MAX_EAGER_LOAD_BYTES {
        return Err(SessionError::LoadBudgetExceeded {
            id,
            logical_bytes: root.logical_bytes,
            maximum: MAX_EAGER_LOAD_BYTES,
        });
    }
    // Ordinals are authoritative in storage; provider-owned graph IDs remain
    // inside opaque JSON and application graph validation remains authoritative.
    // The root byte budget bounds eager hydration until keyset paging lands.
    let messages = query_json_rows::<M>(
        connection,
        "SELECT payload FROM main_history_items WHERE session_id = ?1 ORDER BY ordinal",
        id,
    )?;
    let tool_outputs = query_keyed_json_rows::<T>(
        connection,
        "SELECT tool_id, payload FROM tool_outputs WHERE session_id = ?1 ORDER BY tool_id",
        id,
    )?
    .into_iter()
    .map(|(key, value)| (key, Arc::new(value)))
    .collect::<HashMap<_, _>>();
    let mut subagent_messages = HashMap::new();
    let mut subagent_task_specs = HashMap::new();
    let mut task_spec_bytes = 0usize;
    {
        let mut statement = connection.prepare(
            "SELECT subagent_id, task_spec FROM subagent_streams \
             WHERE session_id = ?1 ORDER BY subagent_id",
        )?;
        let mut rows = statement.query(params![id.as_bytes().as_slice()])?;
        while let Some(row) = rows.next()? {
            let subagent_id: String = row.get(0)?;
            let task_spec: Option<String> = row.get(1)?;
            if let Some(task_spec) = task_spec {
                task_spec_bytes = task_spec_bytes.saturating_add(task_spec.len());
                subagent_task_specs.insert(
                    subagent_id.clone(),
                    deserialize_json(&task_spec, "subagent task spec")?,
                );
            }
            let values = query_subagent_rows::<M>(connection, id, &subagent_id)?;
            subagent_messages.insert(subagent_id, Arc::new(values));
        }
    }
    let subagents = query_subagents(connection, id)?;
    let usage_by_model = query_model_usage(connection, id)?;
    let tool_usage = query_tool_usage(connection, id)?;
    let subagent_item_count = subagent_messages.values().map(|items| items.len()).sum();
    for (field, actual, expected) in [
        (
            "history_item_count",
            messages.len(),
            root.history_item_count,
        ),
        (
            "tool_output_count",
            tool_outputs.len(),
            root.tool_output_count,
        ),
        (
            "subagent_item_count",
            subagent_item_count,
            root.subagent_item_count,
        ),
    ] {
        if actual != expected {
            return Err(SessionError::CorruptDatabaseValue {
                field,
                reason: format!("expected {expected}, found {actual}"),
            });
        }
    }
    let token_usage = deserialize_json(&root.token_usage, "token usage")?;
    let mut meta: SessionMeta = deserialize_json(&root.metadata, "session metadata")?;
    let permission_generation = connection.query_row(
        "SELECT permission_generation FROM sessions WHERE id = ?1",
        params![id.as_bytes().as_slice()],
        |row| row.get::<_, i64>(0),
    )?;
    meta.permission_generation = from_i64(permission_generation, "sessions.permission_generation")?;
    let workspace_binding: StoredWorkspaceBinding =
        deserialize_json(&root.workspace_binding, "sessions.workspace_binding")?;
    if workspace_binding.trust_anchor().as_str() != root.workspace_source
        || !workspace_binding.matches_authority_storage_key(&root.workspace_authority)
        || workspace_binding.principal_id() != root.workspace_principal
        || workspace_binding.project_key().as_str() != root.workspace_project
        || workspace_binding.cwd_handle().as_str() != root.workspace_cursor
        || workspace_binding.cursor_label() != root.workspace_cursor_label.as_deref()
    {
        return Err(SessionError::CorruptDatabaseValue {
            field: "sessions.workspace_binding",
            reason: "query columns do not match serialized identity".into(),
        });
    }
    let write_version = Arc::new(std::sync::atomic::AtomicI64::new(root.write_version));
    let session = Session {
        version: root.format_version,
        id,
        title: root.title,
        cwd: root.cwd,
        model: root.model,
        workspace_binding: Some(Box::new(workspace_binding)),
        messages: Arc::new(messages),
        token_usage,
        tool_outputs,
        subagent_messages,
        subagent_task_specs,
        subagents,
        usage_by_model,
        tool_usage,
        meta,
        created_at: root.created_at,
        updated_at: root.updated_at,
        revision: 0,
        content_revision: 0,
        epoch: next_epoch(),
        rewrites: 0,
        base_write_version: AtomicI64::new(root.write_version),
        write_version,
    };
    let cursor = SessionCursor::new(
        &session,
        root.write_version,
        root.logical_bytes,
        root.token_usage.len() + root.metadata.len() + root.workspace_binding.len(),
        task_spec_bytes,
    );
    Ok((session, cursor))
}

fn root_on(connection: &Connection, id: CaudraId) -> Result<RootRow, SessionError> {
    connection
        .query_row(
            "SELECT format_version, title, cwd, model, created_at, updated_at, \
                        write_version, logical_bytes, history_item_count, tool_output_count,\
                        subagent_item_count, token_usage, metadata, workspace_binding,\
                        workspace_source, workspace_authority, workspace_principal,\
                        workspace_project, workspace_cursor, workspace_cursor_label \
                 FROM sessions WHERE id = ?1",
            params![id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, String>(13)?,
                    row.get::<_, String>(14)?,
                    row.get::<_, String>(15)?,
                    row.get::<_, String>(16)?,
                    row.get::<_, String>(17)?,
                    row.get::<_, String>(18)?,
                    row.get::<_, Option<String>>(19)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StorageError::NotFound(id.to_string()).into())
        .and_then(
            |(
                format_version,
                title,
                cwd,
                model,
                created_at,
                updated_at,
                write_version,
                logical_bytes,
                history_item_count,
                tool_output_count,
                subagent_item_count,
                token_usage,
                metadata,
                workspace_binding,
                workspace_source,
                workspace_authority,
                workspace_principal,
                workspace_project,
                workspace_cursor,
                workspace_cursor_label,
            )| {
                Ok(RootRow {
                    format_version: u32::try_from(format_version).map_err(|_| {
                        SessionError::CorruptDatabaseValue {
                            field: "sessions.format_version",
                            reason: format_version.to_string(),
                        }
                    })?,
                    title,
                    cwd,
                    model,
                    created_at: from_i64(created_at, "sessions.created_at")?,
                    updated_at: from_i64(updated_at, "sessions.updated_at")?,
                    write_version,
                    logical_bytes: from_i64_usize(logical_bytes, "sessions.logical_bytes")?,
                    history_item_count: from_i64_usize(
                        history_item_count,
                        "sessions.history_item_count",
                    )?,
                    tool_output_count: from_i64_usize(
                        tool_output_count,
                        "sessions.tool_output_count",
                    )?,
                    subagent_item_count: from_i64_usize(
                        subagent_item_count,
                        "sessions.subagent_item_count",
                    )?,
                    token_usage,
                    metadata,
                    workspace_binding,
                    workspace_source,
                    workspace_authority,
                    workspace_principal,
                    workspace_project,
                    workspace_cursor,
                    workspace_cursor_label,
                })
            },
        )
}

impl SessionCursor {
    pub fn write_version(&self) -> i64 {
        self.write_version
    }

    /// Queue ordering can advance a snapshot across an in-flight commit only
    /// when both came from the same live session lineage. Equal IDs and
    /// versions alone cannot distinguish an independently loaded stale copy.
    pub fn shares_lineage<M, U, T>(&self, session: &Session<M, U, T>) -> bool {
        Arc::ptr_eq(&self.lineage, &session.write_version)
    }

    fn new<M, U, T>(
        session: &Session<M, U, T>,
        write_version: i64,
        logical_bytes: usize,
        root_bytes: usize,
        task_spec_bytes: usize,
    ) -> Self {
        Self {
            session_id: session.id,
            write_version,
            lineage: Arc::clone(&session.write_version),
            saved_epoch: session.epoch,
            saved_rewrites: session.rewrites,
            saved_history_count: session.messages.len(),
            saved_tool_ids: session.tool_outputs.keys().cloned().collect(),
            saved_subagent_counts: session
                .subagent_messages
                .iter()
                .map(|(id, messages)| (id.clone(), messages.len()))
                .collect(),
            logical_bytes,
            root_bytes,
            task_spec_bytes,
        }
    }
}

impl SerializedRoot {
    fn replace_permissions(
        &mut self,
        connection: &Connection,
        permissions: Option<&str>,
    ) -> Result<(), SessionError> {
        self.metadata = match permissions {
            Some(permissions) => connection.query_row(
                "SELECT json_set(?1, '$.structured_permission_rules', json(?2))",
                params![self.metadata, permissions],
                |row| row.get(0),
            )?,
            None => connection.query_row(
                "SELECT json_remove(?1, '$.structured_permission_rules')",
                params![self.metadata],
                |row| row.get(0),
            )?,
        };
        validate_len("session metadata", self.metadata.len(), MAX_METADATA_BYTES)?;
        Ok(())
    }

    fn fence_permissions(
        &mut self,
        connection: &Connection,
        id: CaudraId,
    ) -> Result<(), SessionError> {
        let stored: Option<Option<String>> = connection
            .query_row(
                "SELECT metadata -> '$.structured_permission_rules' FROM sessions WHERE id = ?1",
                params![id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        match stored {
            Some(stored) => self.replace_permissions(connection, stored.as_deref()),
            None => {
                let metadata: SessionMeta = deserialize_json(&self.metadata, "session metadata")?;
                let mut ids = HashSet::new();
                for record in &metadata.structured_permission_rules {
                    validate_conversation_record(record).map_err(|error| {
                        SessionError::CorruptDatabaseValue {
                            field: "permission rules",
                            reason: error.to_string(),
                        }
                    })?;
                    if !ids.insert(&record.id) {
                        return Err(SessionError::CorruptDatabaseValue {
                            field: "permission rules",
                            reason: "duplicate record ID".into(),
                        });
                    }
                }
                Ok(())
            }
        }
    }

    fn new<M, U, T>(session: &Session<M, U, T>) -> Result<Self, SessionError>
    where
        U: Serialize,
    {
        let binding = session
            .workspace_binding
            .as_deref()
            .cloned()
            .unwrap_or_else(|| StoredWorkspaceBinding::local_from_cwd(&session.cwd));
        Ok(Self {
            token_usage: serialize_json(&session.token_usage, "token usage", MAX_PAYLOAD_BYTES)?,
            metadata: serialize_json(&session.meta, "session metadata", MAX_METADATA_BYTES)?,
            workspace_binding: serialize_json(&binding, "workspace binding", MAX_METADATA_BYTES)?,
            workspace_source: binding.trust_anchor().as_str().to_owned(),
            workspace_authority: binding.authority_storage_key(),
            workspace_principal: binding.principal_id().to_owned(),
            workspace_project: binding.project_key().as_str().to_owned(),
            workspace_cursor: binding.cwd_handle().as_str().to_owned(),
            workspace_cursor_label: binding.cursor_label().map(str::to_owned),
        })
    }

    fn bytes(&self) -> usize {
        self.token_usage.len() + self.metadata.len() + self.workspace_binding.len()
    }
}

impl SerializedSession {
    fn new<M, U, T>(session: &Session<M, U, T>) -> Result<Self, SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        let root = SerializedRoot::new(session)?;
        let messages = serialize_values(&session.messages, "history item", MAX_PAYLOAD_BYTES)?;
        let tool_outputs = session
            .tool_outputs
            .iter()
            .map(|(id, output)| {
                serialize_json(output.as_ref(), "tool output", MAX_PAYLOAD_BYTES)
                    .map(|payload| (id.clone(), payload))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let subagent_messages = session
            .subagent_messages
            .iter()
            .map(|(id, values)| {
                serialize_values(values, "subagent history item", MAX_PAYLOAD_BYTES)
                    .map(|payloads| (id.clone(), payloads))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let task_specs = serialize_task_specs(&session.subagent_task_specs)?;
        let logical_bytes = root.bytes()
            + messages.iter().map(String::len).sum::<usize>()
            + tool_outputs
                .iter()
                .map(|(_, payload)| payload.len())
                .sum::<usize>()
            + subagent_messages
                .iter()
                .flat_map(|(_, values)| values)
                .map(String::len)
                .sum::<usize>()
            + task_specs.values().map(String::len).sum::<usize>();
        Ok(Self {
            root,
            messages,
            tool_outputs,
            subagent_messages,
            task_specs,
            logical_bytes,
        })
    }

    fn root_bytes(&self) -> usize {
        self.root.bytes()
    }

    fn task_spec_bytes(&self) -> usize {
        self.task_specs.values().map(String::len).sum()
    }
}

fn create_owner_only(path: &Path) -> Result<(), SessionError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => return validate_owner_only_metadata(path, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(StorageError::from(error).into()),
    }
    #[cfg(unix)]
    {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(OWNER_FILE_MODE)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path)
        {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(StorageError::from(error).into()),
        }
    }
    #[cfg(not(unix))]
    {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(StorageError::from)?;
    }
    Ok(())
}

fn validate_owner_only_metadata(path: &Path, metadata: &Metadata) -> Result<(), SessionError> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StorageError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "session database path {} is not a regular file",
                path.display()
            ),
        ))
        .into());
    }
    #[cfg(unix)]
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & OTHER_USER_PERMISSIONS != 0
    {
        return Err(StorageError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("session database {} must be owned by the current user and inaccessible to other users", path.display()),
        )).into());
    }
    Ok(())
}

fn open_owner_only_existing(path: &Path) -> Result<File, SessionError> {
    validate_owner_only_metadata(
        path,
        &fs::symlink_metadata(path).map_err(StorageError::from)?,
    )?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32);
    let file = options.open(path).map_err(StorageError::from)?;
    validate_owner_only_metadata(path, &file.metadata().map_err(StorageError::from)?)?;
    Ok(file)
}

fn validate_existing_database_sidecars(path: &Path) -> Result<(), SessionError> {
    for suffix in DATABASE_SIDECAR_SUFFIXES {
        let path = database_sidecar(path, suffix);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                open_owner_only_existing(&path)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(StorageError::from(error).into()),
        }
    }
    Ok(())
}

fn open_writable_connection(
    state_dir: &StateDir,
    migration_lock: &File,
) -> Result<Connection, SessionError> {
    fs::create_dir_all(state_dir.path()).map_err(StorageError::from)?;
    ensure_real_directory(state_dir.path(), false).map_err(StorageError::from)?;
    let path = state_dir.path().join(SESSIONS_DB_FILE);
    create_owner_only(&path)?;
    let mut connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let cutover = version > 0 && version < SCHEMA_VERSION;
    if cutover {
        migration_lock
            .try_lock()
            .map_err(|_| SessionError::MigrationBlocked {
                found: version,
                supported: SCHEMA_VERSION,
                holders: describe_migration_holders(state_dir, &path),
            })?;
    }
    configure_wal_retention(&connection, true)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_SQLITE_VALUE_BYTES)?;
    initialize(&mut connection, state_dir)?;
    if cutover {
        migration_lock.lock_shared().map_err(StorageError::from)?;
    }
    configure(&connection)?;
    Ok(connection)
}

fn configure_wal_retention(connection: &Connection, persistent: bool) -> Result<(), SessionError> {
    let mut enabled = c_int::from(persistent);
    let result = unsafe {
        ffi::sqlite3_file_control(
            connection.handle(),
            MAIN_DB.as_ptr(),
            ffi::SQLITE_FCNTL_PERSIST_WAL,
            (&raw mut enabled).cast(),
        )
    };
    if result != ffi::SQLITE_OK {
        return Err(SqliteError::SqliteFailure(
            SqliteErrorCode::new(result),
            Some(WAL_PERSISTENCE_CONFIGURATION_FAILED.into()),
        )
        .into());
    }
    connection.pragma_update(None, "journal_size_limit", WAL_RETENTION_LIMIT_BYTES as i64)?;
    Ok(())
}

fn verify_current_schema(connection: &Connection) -> Result<(), SessionError> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(SessionError::UnsupportedSchemaVersion {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    verify_application_id(connection)
}

fn verify_application_id(connection: &Connection) -> Result<(), SessionError> {
    let application_id: i64 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    if application_id != APPLICATION_ID {
        return Err(SessionError::CorruptDatabaseValue {
            field: "PRAGMA application_id",
            reason: format!(
                "expected Caudra schema identity {APPLICATION_ID}, found {application_id}; reset required"
            ),
        });
    }
    Ok(())
}

fn verify_empty_database(connection: &Connection) -> Result<(), SessionError> {
    let application_id: i64 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    if application_id != 0 {
        return Err(SessionError::CorruptDatabaseValue {
            field: "PRAGMA application_id",
            reason: format!(
                "expected 0 for an empty database, found {application_id}; reset required"
            ),
        });
    }
    let object_count: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    if object_count != 0 {
        return Err(SessionError::CorruptDatabaseValue {
            field: "sqlite_schema",
            reason: "version 0 database is not empty; reset required".into(),
        });
    }
    Ok(())
}

fn quick_check_on(connection: &Connection) -> Result<(), SessionError> {
    let result: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if result != "ok" {
        return Err(SessionError::CorruptDatabaseValue {
            field: "PRAGMA quick_check",
            reason: result,
        });
    }
    let foreign_key_errors: i64 =
        connection.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_key_errors != 0 {
        return Err(SessionError::CorruptDatabaseValue {
            field: "PRAGMA foreign_key_check",
            reason: format!("{foreign_key_errors} violations"),
        });
    }
    Ok(())
}

fn archive_root(state_dir: &StateDir, create: bool) -> Result<Option<PathBuf>, SessionError> {
    let mut path = state_dir.path().to_path_buf();
    if !ensure_real_directory(&path, false).map_err(StorageError::from)? {
        return Ok(None);
    }
    for component in [super::SESSIONS_DIR, super::ARCHIVE_DIR] {
        path.push(component);
        if !ensure_real_directory(&path, create).map_err(StorageError::from)? {
            return Ok(None);
        }
    }
    Ok(Some(path))
}

fn archive_directory(
    state_dir: &StateDir,
    id: CaudraId,
    create: bool,
) -> Result<Option<PathBuf>, SessionError> {
    let Some(mut path) = archive_root(state_dir, create)? else {
        return Ok(None);
    };
    path.push(id.to_string());
    if !ensure_real_directory(&path, create).map_err(StorageError::from)? {
        return Ok(None);
    }
    Ok(Some(path))
}

fn ensure_real_directory(path: &Path, create: bool) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
            match fs::create_dir(path) {
                Ok(()) => {
                    crate::sync_parent_dir_io(path)?;
                    fs::symlink_metadata(path)?
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    fs::symlink_metadata(path)?
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing archive access through {}", path.display()),
        ));
    }
    Ok(true)
}

fn remove_state_directory(
    state_dir: &StateDir,
    components: &[&str],
    id: CaudraId,
) -> Result<(), io::Error> {
    let mut root = state_dir.path().to_path_buf();
    for component in components {
        let metadata = match fs::symlink_metadata(&root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("refusing cleanup through {}", root.display()),
            ));
        }
        root.push(component);
    }
    let metadata = match fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing cleanup through {}", root.display()),
        ));
    }
    let target = root.join(id.to_string());
    let metadata = match fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            crate::sync_parent_dir_io(&target)?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(&target)?;
    } else {
        fs::remove_file(&target)?;
    }
    crate::sync_parent_dir_io(&target)?;
    Ok(())
}

fn configure(connection: &Connection) -> Result<(), SessionError> {
    // FULL preserves the prior sync-before-ack durability intent. The journal
    // size limit trims WAL retention after checkpoints and must never justify
    // deleting live sidecars.
    connection.execute_batch(&format!(
        "PRAGMA foreign_keys = ON;\
         PRAGMA synchronous = FULL;\
         PRAGMA wal_autocheckpoint = {WAL_AUTO_CHECKPOINT_PAGES};\
         PRAGMA trusted_schema = OFF;"
    ))?;
    Ok(())
}

/// A database newer than this binary is fatal on purpose: opening a schema we
/// cannot read would corrupt it, and a downgrade is never a silent operation.
fn reject_unmigratable(version: i64) -> Result<(), SessionError> {
    if !(0..=SCHEMA_VERSION).contains(&version) {
        return Err(SessionError::UnsupportedSchemaVersion {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(())
}

/// Names the sessions standing in the way of a migration, so the answer to
/// "close them and start again" is not a search of every open terminal.
///
/// Lease files identify sessions only. A `caudra storage` command, a read-only
/// inspector, or an older binary holds the same lock without ever taking one, so
/// an empty list is reported as exactly that rather than as nothing being open.
fn describe_migration_holders(state_dir: &StateDir, path: &Path) -> String {
    let ids = held_session_ids(state_dir);
    if ids.is_empty() {
        return NO_NAMED_HOLDERS.to_owned();
    }
    let titles = session_titles(path);
    let mut described = String::from(NAMED_HOLDERS);
    for id in ids {
        let title = titles
            .get(&id)
            .map_or(UNKNOWN_SESSION_TITLE, String::as_str);
        described.push_str(&format!("\n  {id}  {title}"));
    }
    described
}

/// Reads titles straight from the old schema. Every version of `sessions` has
/// carried `id` and `title`, so this works without the migration that is being
/// blocked, and a failure to read simply leaves the ids unadorned.
fn session_titles(path: &Path) -> HashMap<CaudraId, String> {
    let Ok(connection) = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    ) else {
        return HashMap::new();
    };
    let Ok(mut statement) = connection.prepare("SELECT id, title FROM sessions") else {
        return HashMap::new();
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
    }) else {
        return HashMap::new();
    };
    rows.flatten()
        .filter_map(|(id, title)| {
            let id = id_from_bytes(&id, "session id").ok()?;
            Some((id, truncate_title(title)))
        })
        .collect()
}

fn truncate_title(mut title: String) -> String {
    if title.len() <= MAX_HOLDER_TITLE_BYTES {
        return title;
    }
    let cut = (0..=MAX_HOLDER_TITLE_BYTES)
        .rev()
        .find(|index| title.is_char_boundary(*index))
        .unwrap_or_default();
    title.truncate(cut);
    title.push_str(TITLE_ELLIPSIS);
    title
}

/// `run_to_completion` takes a bare function pointer, so what this forwards to
/// has to be reachable without a capture.
fn report_backup_progress(progress: BackupProgress) {
    let total = progress.pagecount.max(0) as u64;
    let remaining = progress.remaining.max(0) as u64;
    MIGRATION.report(MigrationEvent::Backup {
        done: total.saturating_sub(remaining),
        total,
    });
}

/// Copies the database beside itself before the first migration step, so a
/// failed upgrade leaves the original readable by the version that wrote it.
fn back_up_before_migration(
    connection: &Connection,
    state_dir: &StateDir,
    from: i64,
) -> Result<PathBuf, SessionError> {
    let path = state_dir
        .path()
        .join(format!("{SESSIONS_DB_FILE}.v{from}.bak"));
    let mut destination = Connection::open(&path)?;
    Backup::new(connection, &mut destination)?.run_to_completion(
        BACKUP_PAGES_PER_STEP,
        Duration::ZERO,
        Some(report_backup_progress),
    )?;
    destination.close().map_err(|(_, error)| error)?;
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(OWNER_FILE_MODE))
        .map_err(StorageError::from)?;
    Ok(path)
}

/// Replays [`MIGRATIONS`] from `version` up to [`SCHEMA_VERSION`]. Each step
/// bumps `user_version` inside its own transaction, so an interrupted upgrade
/// leaves the database at a version some binary can open rather than between
/// two of them.
fn migrate_to_current(
    connection: &mut Connection,
    state_dir: &StateDir,
    version: i64,
) -> Result<(), SessionError> {
    verify_application_id(connection)?;
    MIGRATION.report(MigrationEvent::Started {
        from: version,
        to: SCHEMA_VERSION,
    });
    let backup = back_up_before_migration(connection, state_dir, version)?;
    let mut current = version;
    while current < SCHEMA_VERSION {
        let Some(step) = MIGRATIONS.iter().find(|m| m.from == current) else {
            return Err(SessionError::UnsupportedSchemaVersion {
                found: current,
                supported: SCHEMA_VERSION,
            });
        };
        MIGRATION.report(MigrationEvent::Step {
            from: step.from,
            to: step.to,
        });
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
        transaction.execute_batch(step.sql)?;
        if step.to == 7 {
            backfill_workspace_bindings(&transaction)?;
        }
        if step.to == 10 {
            compress_existing_payloads(&transaction)?;
        }
        transaction.pragma_update(None, "user_version", step.to)?;
        transaction.commit()?;
        current = step.to;
    }
    tracing::info!(
        from = version,
        to = SCHEMA_VERSION,
        backup = %backup.display(),
        "migrated session database schema"
    );
    MIGRATION.report(MigrationEvent::Finished {
        from: version,
        to: SCHEMA_VERSION,
    });
    Ok(())
}

/// Moves every payload from the tables [`PAYLOAD_COMPRESSION_SCHEMA`] renamed
/// into their compressed replacements. Rows stream one at a time: this visits
/// every payload in the database, so collecting them first would make peak
/// memory scale with the transcript history.
fn compress_existing_payloads(transaction: &Transaction<'_>) -> Result<(), SessionError> {
    for rewrite in &COMPRESSED_PAYLOAD_TABLES {
        let PayloadRewrite {
            table,
            legacy,
            keys,
            placeholders,
        } = rewrite;
        let key_count = placeholders.matches('?').count();
        let mut select = transaction.prepare(&format!("SELECT {keys}, payload FROM {legacy}"))?;
        let mut insert = transaction.prepare(&format!(
            "INSERT INTO {table} ({keys}, payload, byte_count) \
             VALUES ({placeholders}, ?{}, ?{})",
            key_count + 1,
            key_count + 2
        ))?;
        let counted: i64 = transaction.query_row(
            &format!("SELECT count(*) FROM {legacy}"),
            [],
            |row| row.get(0),
        )?;
        let total = from_i64(counted, "legacy payload rows")?;
        let mut rows = select.query([])?;
        let mut moved: u64 = 0;
        let mut stored: u64 = 0;
        MIGRATION.report(MigrationEvent::Rewrite {
            table,
            done: 0,
            total,
        });
        while let Some(row) = rows.next()? {
            let mut values = Vec::with_capacity(key_count + 2);
            for index in 0..key_count {
                values.push(row.get::<_, SqlValue>(index)?);
            }
            let payload: String = row.get(key_count)?;
            let compressed = compress_payload(&payload)?;
            stored += compressed.len() as u64;
            values.push(SqlValue::Blob(compressed));
            values.push(SqlValue::Integer(to_i64(payload.len(), "payload bytes")?));
            insert.execute(params_from_iter(values))?;
            moved += 1;
            // Reporting every row would cost more than the rewrite on a large
            // transcript, and no reader can see that resolution anyway.
            if moved.is_multiple_of(PAYLOAD_PROGRESS_INTERVAL) {
                MIGRATION.report(MigrationEvent::Rewrite {
                    table,
                    done: moved,
                    total,
                });
            }
        }
        MIGRATION.report(MigrationEvent::Rewrite {
            table,
            done: moved,
            total,
        });
        tracing::info!(table, rows = moved, stored_bytes = stored, "compressed payloads");
    }
    for rewrite in &COMPRESSED_PAYLOAD_TABLES {
        transaction.execute_batch(&format!("DROP TABLE {};", rewrite.legacy))?;
    }
    Ok(())
}

fn backfill_workspace_bindings(transaction: &Transaction<'_>) -> Result<(), SessionError> {
    let rows = {
        let mut statement = transaction.prepare("SELECT id, cwd FROM sessions")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (id, cwd) in rows {
        let binding = StoredWorkspaceBinding::local_from_cwd(&cwd);
        let serialized = serialize_json(&binding, "workspace binding", MAX_METADATA_BYTES)?;
        transaction.execute(
            "UPDATE sessions SET workspace_binding = ?1, workspace_source = ?2, \
             workspace_authority = ?3, workspace_principal = ?4, workspace_project = ?5, \
             workspace_cursor = ?6, workspace_cursor_label = ?7, \
             logical_bytes = logical_bytes + length(CAST(?1 AS BLOB)) WHERE id = ?8",
            params![
                serialized,
                binding.trust_anchor().as_str(),
                binding.authority_storage_key(),
                binding.principal_id(),
                binding.project_key().as_str(),
                binding.cwd_handle().as_str(),
                binding.cursor_label(),
                id,
            ],
        )?;
    }
    Ok(())
}

fn initialize(connection: &mut Connection, state_dir: &StateDir) -> Result<(), SessionError> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == SCHEMA_VERSION {
        return configure_current_schema(connection);
    }
    reject_unmigratable(version)?;

    // Caudra initializers serialize on the existing artifact lock, while the
    // SQLite exclusive mode below also excludes non-cooperating connections.
    let _initialization_lock = lock_session_artifacts(state_dir)?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == SCHEMA_VERSION {
        return configure_current_schema(connection);
    }
    reject_unmigratable(version)?;
    if version != 0 {
        migrate_to_current(connection, state_dir, version)?;
        return configure_current_schema(connection);
    }

    // Retain SQLite's exclusive file lock across the empty-header pragmas and
    // schema transaction. This prevents a foreign version-zero database from
    // appearing between validation and the first persistent setting.
    connection.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
    let locked_version = {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
        let locked_version: i64 =
            transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if locked_version != 0 && locked_version != SCHEMA_VERSION {
            return Err(SessionError::UnsupportedSchemaVersion {
                found: locked_version,
                supported: SCHEMA_VERSION,
            });
        }
        if locked_version == 0 {
            verify_empty_database(&transaction)?;
        }
        transaction.commit()?;
        locked_version
    };
    if locked_version == 0 {
        connection.pragma_update(None, "page_size", PAGE_SIZE)?;
        connection.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        // The validation read initialized the empty header, so persist the
        // auto-vacuum pointer map before creating any tables.
        connection.execute_batch("VACUUM")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
        verify_empty_database(&transaction)?;
        transaction.execute_batch(&full_schema())?;
        transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
    }
    connection.pragma_update(None, "locking_mode", "NORMAL")?;
    configure_current_schema(connection)
}

fn configure_current_schema(connection: &Connection) -> Result<(), SessionError> {
    // A current database needs no initialization lock; this keeps frequent state
    // opens off the write lock.
    verify_current_schema(connection)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    verify_auto_vacuum(connection)
}

fn verify_auto_vacuum(connection: &Connection) -> Result<(), SessionError> {
    let auto_vacuum: i64 = connection.pragma_query_value(None, "auto_vacuum", |row| row.get(0))?;
    if auto_vacuum != INCREMENTAL_AUTO_VACUUM {
        return Err(SessionError::CorruptDatabaseValue {
            field: "PRAGMA auto_vacuum",
            reason: format!("expected {INCREMENTAL_AUTO_VACUUM}, found {auto_vacuum}"),
        });
    }
    Ok(())
}

fn validate_scalars<M, U, T>(session: &Session<M, U, T>) -> Result<(), SessionError> {
    validate_len("session title", session.title.len(), 1024)?;
    validate_len("session cwd", session.cwd.len(), MAX_PATH_BYTES)?;
    validate_identifier("session model", &session.model)?;
    for id in session
        .tool_outputs
        .keys()
        .chain(session.subagent_messages.keys())
        .chain(session.subagent_task_specs.keys())
        .chain(session.usage_by_model.keys())
        .chain(session.meta.unsent_subagent_messages.keys())
    {
        validate_identifier("session identifier", id)?;
    }
    for subagent in &session.subagents {
        validate_identifier("subagent tool id", &subagent.tool_use_id)?;
        if let Some(id) = &subagent.parent_tool_use_id {
            validate_identifier("subagent parent id", id)?;
        }
        if let Some(id) = &subagent.root_tool_use_id {
            validate_identifier("subagent root id", id)?;
        }
        validate_identifier("subagent name", &subagent.name)?;
        if let Some(model) = &subagent.model {
            validate_identifier("subagent model", model)?;
        }
    }
    if let Some(profile) = &session.meta.system_prompt_profile {
        validate_identifier("system prompt profile", profile)?;
    }
    if let Some(path) = &session.meta.plan_path {
        validate_len("plan path", path.len(), MAX_PATH_BYTES)?;
    }
    if let Some(crate::sessions::StoredPlanTarget::LocalPath { path }) = &session.meta.plan_target {
        validate_len("plan path", path.len(), MAX_PATH_BYTES)?;
    }
    validate_len(
        "draft image count",
        session.meta.input_draft_images.len(),
        MAX_IMAGES_PER_ITEM,
    )?;
    let decoded_image_bytes = session
        .meta
        .input_draft_images
        .iter()
        .try_fold(0usize, |total, image| {
            let decoded = image
                .data
                .len()
                .checked_add(3)
                .and_then(|length| length.checked_div(4))
                .and_then(|groups| groups.checked_mul(3))?;
            total.checked_add(decoded)
        })
        .unwrap_or(usize::MAX);
    validate_len(
        "decoded draft images",
        decoded_image_bytes,
        MAX_DECODED_IMAGE_BYTES,
    )?;
    Ok(())
}

fn validate_identifier(kind: &'static str, value: &str) -> Result<(), SessionError> {
    validate_len(kind, value.len(), MAX_IDENTIFIER_BYTES)
}

fn serialize_task_specs(
    values: &HashMap<String, StoredSubagentTaskSpec>,
) -> Result<HashMap<String, String>, SessionError> {
    values
        .iter()
        .map(|(id, value)| {
            serialize_json(value, "subagent task spec", MAX_PAYLOAD_BYTES)
                .map(|payload| (id.clone(), payload))
        })
        .collect()
}

fn serialize_values<T: Serialize>(
    values: &[T],
    kind: &'static str,
    maximum: usize,
) -> Result<Vec<String>, SessionError> {
    values
        .iter()
        .map(|value| serialize_json(value, kind, maximum))
        .collect()
}

fn serialize_json<T: Serialize + ?Sized>(
    value: &T,
    kind: &'static str,
    maximum: usize,
) -> Result<String, SessionError> {
    let value = serde_json::to_string(value).map_err(StorageError::from)?;
    validate_len(kind, value.len(), maximum)?;
    validate_json_depth(&value)?;
    Ok(value)
}

fn validate_json_depth(value: &str) -> Result<(), SessionError> {
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for byte in value.bytes() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'"' => quoted = true,
            b'{' | b'[' => {
                depth = depth.saturating_add(1);
                validate_len("JSON nesting", depth, MAX_JSON_DEPTH)?;
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

fn deserialize_json<T: DeserializeOwned>(
    value: &str,
    field: &'static str,
) -> Result<T, SessionError> {
    serde_json::from_str(value).map_err(|error| SessionError::CorruptDatabaseValue {
        field,
        reason: error.to_string(),
    })
}

/// The single point where a payload becomes stored bytes. Callers keep the
/// uncompressed length for `byte_count`, which `logical_bytes`, the trim
/// threshold and the history-discovery bounds all read.
fn compress_payload(payload: &str) -> Result<Vec<u8>, SessionError> {
    zstd::bulk::compress(payload.as_bytes(), PAYLOAD_COMPRESSION_LEVEL).map_err(|error| {
        SessionError::CorruptDatabaseValue {
            field: "payload",
            reason: error.to_string(),
        }
    })
}

/// A payload that fails to decode is a named error rather than a panic: before
/// compression a damaged row still parsed as text, and afterwards it fails at
/// the frame instead.
fn decompress_payload(stored: &[u8], field: &'static str) -> Result<String, SessionError> {
    let decoded =
        zstd::decode_all(stored).map_err(|error| SessionError::CorruptDatabaseValue {
            field,
            reason: format!("{PAYLOAD_DECOMPRESSION_FAILED}: {error}"),
        })?;
    String::from_utf8(decoded).map_err(|error| SessionError::CorruptDatabaseValue {
        field,
        reason: error.to_string(),
    })
}

/// Reads one stored payload column and returns the JSON text it holds.
fn payload_from_row(
    row: &rusqlite::Row<'_>,
    index: usize,
    field: &'static str,
) -> Result<String, SessionError> {
    let stored: Vec<u8> = row.get(index)?;
    decompress_payload(&stored, field)
}

fn validate_len(kind: &'static str, actual: usize, maximum: usize) -> Result<(), SessionError> {
    if actual > maximum {
        return Err(SessionError::LimitExceeded {
            kind,
            actual,
            maximum,
        });
    }
    Ok(())
}

fn insert_root<M, U, T>(
    transaction: &Transaction<'_>,
    session: &Session<M, U, T>,
    serialized: &SerializedSession,
) -> Result<(), SessionError> {
    transaction.execute(
        "INSERT INTO sessions (\
             id, format_version, title, cwd, model, created_at, updated_at, write_version,\
             logical_bytes, history_item_count, tool_output_count, subagent_item_count,\
             token_usage, metadata, workspace_binding, workspace_source, workspace_authority,\
             workspace_principal, workspace_project, workspace_cursor, workspace_cursor_label\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9, ?10, ?11, ?12, ?13,\
                   ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
        params![
            session.id.as_bytes().as_slice(),
            i64::from(session.version),
            session.title,
            session.cwd,
            session.model,
            to_i64(session.created_at, "created_at")?,
            to_i64(session.updated_at, "updated_at")?,
            to_i64(serialized.logical_bytes, "logical_bytes")?,
            to_i64(session.messages.len(), "history item count")?,
            to_i64(session.tool_outputs.len(), "tool output count")?,
            to_i64(
                session
                    .subagent_messages
                    .values()
                    .map(|messages| messages.len())
                    .sum::<usize>(),
                "subagent item count"
            )?,
            serialized.root.token_usage,
            serialized.root.metadata,
            serialized.root.workspace_binding,
            serialized.root.workspace_source,
            serialized.root.workspace_authority,
            serialized.root.workspace_principal,
            serialized.root.workspace_project,
            serialized.root.workspace_cursor,
            serialized.root.workspace_cursor_label,
        ],
    )?;
    Ok(())
}

fn update_root<M, U, T>(
    transaction: &Transaction<'_>,
    session: &Session<M, U, T>,
    serialized: &SerializedSession,
    expected: i64,
) -> Result<(), SessionError> {
    update_root_values(
        transaction,
        session,
        &serialized.root,
        serialized.logical_bytes,
        expected,
    )
}

fn ensure_write_version(
    connection: &Connection,
    id: CaudraId,
    expected: i64,
) -> Result<(), SessionError> {
    let actual = connection
        .query_row(
            "SELECT write_version FROM sessions WHERE id = ?1",
            params![id.as_bytes().as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    match actual {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => Err(SessionError::ConcurrentSessionWriter {
            id,
            expected,
            actual,
        }),
        None => Err(StorageError::NotFound(id.to_string()).into()),
    }
}

fn update_root_values<M, U, T>(
    transaction: &Transaction<'_>,
    session: &Session<M, U, T>,
    root: &SerializedRoot,
    logical_bytes: usize,
    expected: i64,
) -> Result<(), SessionError> {
    let stored_binding: Option<String> = transaction
        .query_row(
            "SELECT workspace_binding FROM sessions WHERE id = ?1",
            params![session.id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(stored) = stored_binding.as_deref() {
        let stored: StoredWorkspaceBinding =
            deserialize_json(stored, "sessions.workspace_binding")?;
        let next: StoredWorkspaceBinding =
            deserialize_json(&root.workspace_binding, "sessions.workspace_binding")?;
        if !stored.same_workspace_identity(&next) {
            return Err(SessionError::WorkspaceIdentityImmutable);
        }
    }
    let changed = transaction.execute(
        "UPDATE sessions SET \
             format_version = ?1, title = ?2, cwd = ?3, model = ?4,\
             created_at = ?5, updated_at = ?6, write_version = write_version + 1,\
             logical_bytes = ?7, history_item_count = ?8, tool_output_count = ?9,\
             subagent_item_count = ?10, token_usage = ?11, metadata = ?12,\
             workspace_binding = ?13, workspace_source = ?14, workspace_authority = ?15,\
             workspace_principal = ?16, workspace_project = ?17, workspace_cursor = ?18,\
             workspace_cursor_label = ?19 \
         WHERE id = ?20 AND write_version = ?21",
        params![
            i64::from(session.version),
            session.title,
            session.cwd,
            session.model,
            to_i64(session.created_at, "created_at")?,
            to_i64(session.updated_at, "updated_at")?,
            to_i64(logical_bytes, "logical_bytes")?,
            to_i64(session.messages.len(), "history item count")?,
            to_i64(session.tool_outputs.len(), "tool output count")?,
            to_i64(
                session
                    .subagent_messages
                    .values()
                    .map(|messages| messages.len())
                    .sum::<usize>(),
                "subagent item count"
            )?,
            root.token_usage,
            root.metadata,
            root.workspace_binding,
            root.workspace_source,
            root.workspace_authority,
            root.workspace_principal,
            root.workspace_project,
            root.workspace_cursor,
            root.workspace_cursor_label,
            session.id.as_bytes().as_slice(),
            expected,
        ],
    )?;
    if changed != 1 {
        let actual = transaction
            .query_row(
                "SELECT write_version FROM sessions WHERE id = ?1",
                params![session.id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        return match actual {
            Some(actual) => Err(SessionError::ConcurrentSessionWriter {
                id: session.id,
                expected,
                actual,
            }),
            None => Err(StorageError::NotFound(session.id.to_string()).into()),
        };
    }
    Ok(())
}

fn enqueue_cleanup_jobs(transaction: &Transaction<'_>, id: CaudraId) -> Result<(), SessionError> {
    for kind in ARTIFACT_CLEANUP_KINDS {
        transaction.execute(
            "INSERT INTO cleanup_jobs (session_id, kind, next_attempt_ms) VALUES (?1, ?2, 0) \
             ON CONFLICT(session_id, kind) DO UPDATE SET next_attempt_ms = 0",
            params![id.as_bytes().as_slice(), kind],
        )?;
    }
    Ok(())
}

/// Total size of regular files below `path`, following no symlinks. Missing
/// or unreadable entries count as zero because this feeds diagnostics only.
pub(super) fn directory_bytes(path: &Path) -> u64 {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 0;
    };
    if metadata.is_file() {
        return metadata.len();
    }
    if !metadata.is_dir() {
        return 0;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| directory_bytes(&entry.path()))
        .sum()
}

fn clear_children(transaction: &Transaction<'_>, id: CaudraId) -> Result<(), SessionError> {
    for table in [
        "main_history_items",
        "tool_outputs",
        "subagent_streams",
        "subagents",
        "model_usage",
    ] {
        transaction.execute(
            &format!("DELETE FROM {table} WHERE session_id = ?1"),
            params![id.as_bytes().as_slice()],
        )?;
    }
    Ok(())
}

fn is_tombstoned(transaction: &Transaction<'_>, id: CaudraId) -> Result<bool, SessionError> {
    Ok(transaction
        .query_row(
            "SELECT 1 FROM session_tombstones WHERE session_id = ?1",
            params![id.as_bytes().as_slice()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn deleted_version(
    transaction: &Transaction<'_>,
    id: CaudraId,
) -> Result<Option<i64>, SessionError> {
    Ok(transaction
        .query_row(
            "SELECT deleted_version FROM session_tombstones WHERE session_id = ?1",
            params![id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?
        .flatten())
}

fn insert_children<M, U, T>(
    transaction: &Transaction<'_>,
    session: &Session<M, U, T>,
    serialized: &SerializedSession,
) -> Result<(), SessionError> {
    for (ordinal, payload) in serialized.messages.iter().enumerate() {
        insert_history(transaction, session.id, ordinal, payload)?;
    }
    for (id, payload) in &serialized.tool_outputs {
        insert_tool_output(transaction, session.id, id, payload)?;
    }
    upsert_streams(
        transaction,
        session.id,
        &session.subagent_messages,
        &serialized.task_specs,
    )?;
    for (id, values) in &serialized.subagent_messages {
        for (ordinal, payload) in values.iter().enumerate() {
            insert_subagent_history(transaction, session.id, id, ordinal, payload)?;
        }
    }
    replace_auxiliary(transaction, session)
}

fn insert_history(
    transaction: &Transaction<'_>,
    session_id: CaudraId,
    ordinal: usize,
    payload: &str,
) -> Result<(), SessionError> {
    transaction.execute(
        "INSERT INTO main_history_items (session_id, ordinal, payload, byte_count) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            session_id.as_bytes().as_slice(),
            to_i64(ordinal, "history ordinal")?,
            compress_payload(payload)?,
            to_i64(payload.len(), "history payload bytes")?
        ],
    )?;
    Ok(())
}

fn insert_tool_output(
    transaction: &Transaction<'_>,
    session_id: CaudraId,
    id: &str,
    payload: &str,
) -> Result<(), SessionError> {
    transaction.execute(
        "INSERT INTO tool_outputs (session_id, tool_id, payload, byte_count) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            session_id.as_bytes().as_slice(),
            id,
            compress_payload(payload)?,
            to_i64(payload.len(), "tool output payload bytes")?
        ],
    )?;
    Ok(())
}

fn upsert_streams<M>(
    transaction: &Transaction<'_>,
    session_id: CaudraId,
    streams: &HashMap<String, Arc<Vec<M>>>,
    task_specs: &HashMap<String, String>,
) -> Result<(), SessionError> {
    // The parent stream row is durable even when its history is empty, which
    // preserves completed and resumable empty subagents.
    for id in streams.keys() {
        transaction.execute(
            "INSERT INTO subagent_streams (session_id, subagent_id, task_spec) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT(session_id, subagent_id) DO UPDATE SET task_spec = excluded.task_spec",
            params![session_id.as_bytes().as_slice(), id, task_specs.get(id)],
        )?;
    }
    Ok(())
}

fn insert_subagent_history(
    transaction: &Transaction<'_>,
    session_id: CaudraId,
    subagent_id: &str,
    ordinal: usize,
    payload: &str,
) -> Result<(), SessionError> {
    transaction.execute(
        "INSERT INTO subagent_history_items \
         (session_id, subagent_id, ordinal, payload, byte_count) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            session_id.as_bytes().as_slice(),
            subagent_id,
            to_i64(ordinal, "subagent history ordinal")?,
            compress_payload(payload)?,
            to_i64(payload.len(), "subagent payload bytes")?
        ],
    )?;
    Ok(())
}

fn replace_auxiliary<M, U, T>(
    transaction: &Transaction<'_>,
    session: &Session<M, U, T>,
) -> Result<(), SessionError> {
    transaction.execute(
        "DELETE FROM subagents WHERE session_id = ?1",
        params![session.id.as_bytes().as_slice()],
    )?;
    for (ordinal, subagent) in session.subagents.iter().enumerate() {
        transaction.execute(
            "INSERT INTO subagents (\
                 session_id, ordinal, tool_use_id, parent_tool_use_id, root_tool_use_id, name, model,\
                 outcome\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session.id.as_bytes().as_slice(),
                to_i64(ordinal, "subagent ordinal")?,
                subagent.tool_use_id,
                subagent.parent_tool_use_id,
                subagent.root_tool_use_id,
                subagent.name,
                subagent.model,
                subagent.outcome.storage_name(),
            ],
        )?;
    }
    transaction.execute(
        "DELETE FROM model_usage WHERE session_id = ?1",
        params![session.id.as_bytes().as_slice()],
    )?;
    // Usage is storage-owned and queryable across sessions. Nullable cost keeps
    // unpriced history distinct from a priced zero-dollar value.
    for (model, usage) in &session.usage_by_model {
        transaction.execute(
            "INSERT INTO model_usage (\
                 session_id, model, input_tokens, output_tokens, cache_creation, cache_read, \
                 cost, subscription_cost\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session.id.as_bytes().as_slice(),
                model,
                i64::from(usage.input),
                i64::from(usage.output),
                i64::from(usage.cache_creation),
                i64::from(usage.cache_read),
                usage.cost,
                usage.subscription_cost,
            ],
        )?;
    }
    transaction.execute(
        "DELETE FROM session_tool_usage WHERE session_id = ?1",
        params![session.id.as_bytes().as_slice()],
    )?;
    for usage in &session.tool_usage {
        transaction.execute(
            "INSERT INTO session_tool_usage (\
                 session_id, tool, source, outcome, calls, duration_ms, tokens, latency\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session.id.as_bytes().as_slice(),
                usage.tool,
                usage.source,
                usage.outcome.storage_name(),
                to_i64(usage.calls, "session_tool_usage.calls")?,
                to_i64(usage.duration_ms, "session_tool_usage.duration_ms")?,
                to_i64(usage.tokens, "session_tool_usage.tokens")?,
                usage.latency.encode(),
            ],
        )?;
    }
    Ok(())
}

fn query_json_rows<T: DeserializeOwned>(
    connection: &Connection,
    sql: &str,
    id: CaudraId,
) -> Result<Vec<T>, SessionError> {
    let mut statement = connection.prepare(sql)?;
    let mut rows = statement.query(params![id.as_bytes().as_slice()])?;
    let mut values = Vec::new();
    while let Some(row) = rows.next()? {
        let payload = payload_from_row(row, 0, "payload")?;
        values.push(deserialize_json(&payload, "payload")?);
    }
    Ok(values)
}

fn query_keyed_json_rows<T: DeserializeOwned>(
    connection: &Connection,
    sql: &str,
    id: CaudraId,
) -> Result<Vec<(String, T)>, SessionError> {
    let mut statement = connection.prepare(sql)?;
    let mut rows = statement.query(params![id.as_bytes().as_slice()])?;
    let mut values = Vec::new();
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let payload = payload_from_row(row, 1, "payload")?;
        values.push((key, deserialize_json(&payload, "payload")?));
    }
    Ok(values)
}

fn query_subagent_rows<T: DeserializeOwned>(
    connection: &Connection,
    session_id: CaudraId,
    subagent_id: &str,
) -> Result<Vec<T>, SessionError> {
    let mut statement = connection.prepare(
        "SELECT payload FROM subagent_history_items \
         WHERE session_id = ?1 AND subagent_id = ?2 ORDER BY ordinal",
    )?;
    let mut rows = statement.query(params![session_id.as_bytes().as_slice(), subagent_id])?;
    let mut values = Vec::new();
    while let Some(row) = rows.next()? {
        let payload = payload_from_row(row, 0, "subagent history payload")?;
        values.push(deserialize_json(&payload, "subagent history payload")?);
    }
    Ok(values)
}

fn query_subagents(
    connection: &Connection,
    id: CaudraId,
) -> Result<Vec<StoredSubagent>, SessionError> {
    let mut statement = connection.prepare(
        "SELECT tool_use_id, parent_tool_use_id, root_tool_use_id, name, model, outcome \
         FROM subagents WHERE session_id = ?1 ORDER BY ordinal",
    )?;
    let mut rows = statement.query(params![id.as_bytes().as_slice()])?;
    let mut values = Vec::new();
    while let Some(row) = rows.next()? {
        let stored_outcome: Option<String> = row.get(5)?;
        let outcome = match stored_outcome {
            None => StoredSubagentOutcome::Unknown,
            Some(value) => StoredSubagentOutcome::from_storage_name(&value).ok_or_else(|| {
                SessionError::CorruptDatabaseValue {
                    field: "subagents.outcome",
                    reason: value,
                }
            })?,
        };
        values.push(StoredSubagent {
            tool_use_id: row.get(0)?,
            parent_tool_use_id: row.get(1)?,
            root_tool_use_id: row.get(2)?,
            name: row.get(3)?,
            model: row.get(4)?,
            outcome,
        });
    }
    Ok(values)
}

fn query_model_usage(
    connection: &Connection,
    id: CaudraId,
) -> Result<HashMap<String, StoredTokenUsage>, SessionError> {
    let mut statement = connection.prepare(
        "SELECT model, input_tokens, output_tokens, cache_creation, cache_read, cost, \
                subscription_cost \
         FROM model_usage WHERE session_id = ?1",
    )?;
    let mut rows = statement.query(params![id.as_bytes().as_slice()])?;
    let mut values = HashMap::new();
    while let Some(row) = rows.next()? {
        values.insert(
            row.get(0)?,
            StoredTokenUsage {
                input: from_i64_u32(row.get(1)?, "model_usage.input_tokens")?,
                output: from_i64_u32(row.get(2)?, "model_usage.output_tokens")?,
                cache_creation: from_i64_u32(row.get(3)?, "model_usage.cache_creation")?,
                cache_read: from_i64_u32(row.get(4)?, "model_usage.cache_read")?,
                cost: row.get(5)?,
                subscription_cost: row.get(6)?,
            },
        );
    }
    Ok(values)
}

fn tool_bucket_from_row(row: &rusqlite::Row<'_>) -> Result<ToolBucket, SessionError> {
    let outcome: String = row.get(4)?;
    Ok(ToolBucket {
        bucket_start: row.get(0)?,
        tool: row.get(1)?,
        source: row.get(2)?,
        cwd: row.get(3)?,
        outcome: ToolOutcome::from_storage_name(&outcome).ok_or(
            SessionError::CorruptDatabaseValue {
                field: "tool_ledger.outcome",
                reason: outcome,
            },
        )?,
        calls: from_i64(row.get(5)?, "tool_ledger.calls")?,
        duration_ms: from_i64(row.get(6)?, "tool_ledger.duration_ms")?,
        tokens: from_i64(row.get(7)?, "tool_ledger.tokens")?,
        latency: Latency::decode(&row.get::<_, Vec<u8>>(8)?, TOOL_LEDGER_LATENCY)?,
    })
}

/// Adds one entry to whatever the bucket already holds. SQLite can sum the
/// counters on conflict but cannot add two histograms, so the existing one is
/// read and merged here, and the caller supplies the transaction that makes the
/// pair atomic.
fn merge_tool_bucket(connection: &Connection, entry: &ToolLedgerEntry) -> Result<(), SessionError> {
    let outcome = entry.outcome.storage_name();
    let stored: Option<Vec<u8>> = connection
        .query_row(
            "SELECT latency FROM tool_ledger \
             WHERE bucket_start = ?1 AND tool = ?2 AND source = ?3 AND cwd = ?4 AND outcome = ?5",
            params![
                entry.bucket_start,
                entry.tool,
                entry.source,
                entry.cwd,
                outcome
            ],
            |row| row.get(0),
        )
        .optional()?;
    let mut latency = match stored {
        Some(bytes) => Latency::decode(&bytes, TOOL_LEDGER_LATENCY)?,
        None => Latency::default(),
    };
    latency.merge(entry.latency);
    connection.execute(
        "INSERT INTO tool_ledger (bucket_start, tool, source, cwd, outcome, calls, \
             duration_ms, tokens, latency) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
         ON CONFLICT(bucket_start, tool, source, cwd, outcome) \
         DO UPDATE SET \
             calls = calls + excluded.calls, \
             duration_ms = duration_ms + excluded.duration_ms, \
             tokens = tokens + excluded.tokens, \
             latency = excluded.latency",
        params![
            entry.bucket_start,
            entry.tool,
            entry.source,
            entry.cwd,
            outcome,
            to_i64(entry.calls, "tool_ledger.calls")?,
            to_i64(entry.duration_ms, "tool_ledger.duration_ms")?,
            to_i64(entry.tokens, "tool_ledger.tokens")?,
            latency.encode(),
        ],
    )?;
    Ok(())
}

fn query_tool_usage(
    connection: &Connection,
    id: CaudraId,
) -> Result<Vec<StoredToolUsage>, SessionError> {
    let mut statement = connection.prepare(
        "SELECT tool, source, outcome, calls, duration_ms, tokens, latency \
         FROM session_tool_usage WHERE session_id = ?1 ORDER BY tool, source, outcome",
    )?;
    let mut rows = statement.query(params![id.as_bytes().as_slice()])?;
    let mut values = Vec::new();
    while let Some(row) = rows.next()? {
        let outcome: String = row.get(2)?;
        values.push(StoredToolUsage {
            tool: row.get(0)?,
            source: row.get(1)?,
            outcome: ToolOutcome::from_storage_name(&outcome).ok_or(
                SessionError::CorruptDatabaseValue {
                    field: "session_tool_usage.outcome",
                    reason: outcome,
                },
            )?,
            calls: from_i64(row.get(3)?, "session_tool_usage.calls")?,
            duration_ms: from_i64(row.get(4)?, "session_tool_usage.duration_ms")?,
            tokens: from_i64(row.get(5)?, "session_tool_usage.tokens")?,
            latency: Latency::decode(&row.get::<_, Vec<u8>>(6)?, SESSION_TOOL_LATENCY)?,
        });
    }
    Ok(values)
}

fn id_from_row(row: &rusqlite::Row<'_>, index: usize) -> Result<CaudraId, SessionError> {
    let bytes: Vec<u8> = row.get(index)?;
    id_from_bytes(&bytes, "session id")
}

fn id_from_bytes(bytes: &[u8], field: &'static str) -> Result<CaudraId, SessionError> {
    let bytes: [u8; 16] = bytes
        .try_into()
        .map_err(|_| SessionError::CorruptDatabaseValue {
            field,
            reason: format!("expected 16 bytes, found {}", bytes.len()),
        })?;
    Ok(CaudraId::from_bytes(bytes))
}

pub(crate) fn to_i64(value: impl TryInto<i64>, field: &'static str) -> Result<i64, SessionError> {
    value
        .try_into()
        .map_err(|_| SessionError::CorruptDatabaseValue {
            field,
            reason: "value exceeds SQLite integer range".into(),
        })
}

pub(crate) fn from_i64(value: i64, field: &'static str) -> Result<u64, SessionError> {
    u64::try_from(value).map_err(|_| SessionError::CorruptDatabaseValue {
        field,
        reason: value.to_string(),
    })
}

fn from_i64_usize(value: i64, field: &'static str) -> Result<usize, SessionError> {
    usize::try_from(value).map_err(|_| SessionError::CorruptDatabaseValue {
        field,
        reason: value.to_string(),
    })
}

fn from_i64_u32(value: i64, field: &'static str) -> Result<u32, SessionError> {
    u32::try_from(value).map_err(|_| SessionError::CorruptDatabaseValue {
        field,
        reason: value.to_string(),
    })
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

fn database_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(suffix);
    sidecar.into()
}

fn database_sidecars_exist(
    path: &Path,
) -> Result<[bool; DATABASE_SIDECAR_SUFFIXES.len()], SessionError> {
    let mut exists = [false; DATABASE_SIDECAR_SUFFIXES.len()];
    for (exists, suffix) in exists.iter_mut().zip(DATABASE_SIDECAR_SUFFIXES) {
        *exists = match fs::symlink_metadata(database_sidecar(path, suffix)) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(StorageError::from(error).into()),
        };
    }
    Ok(exists)
}

fn immutable_database_uri(path: &Path) -> Result<String, SessionError> {
    #[cfg(unix)]
    let bytes = path.as_os_str().as_encoded_bytes();
    #[cfg(not(unix))]
    let bytes = path
        .to_str()
        .ok_or_else(|| {
            StorageError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                INVALID_SQLITE_URI_PATH,
            ))
        })?
        .as_bytes();
    let mut uri = String::from("file:");
    for &byte in bytes {
        uri.push('%');
        uri.push(char::from(
            URI_HEX_DIGITS[usize::from(byte) / URI_HEX_DIGITS.len()],
        ));
        uri.push(char::from(
            URI_HEX_DIGITS[usize::from(byte) % URI_HEX_DIGITS.len()],
        ));
    }
    uri.push_str("?mode=ro&immutable=1");
    Ok(uri)
}

fn is_pending_archive(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with(".pending-") && name.ends_with(".jsonl"))
}

fn pending_archive_is_stale(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .and_then(|modified| modified.elapsed().map_err(io::Error::other))
        .is_ok_and(|age| age >= PENDING_ARCHIVE_ORPHAN_GRACE)
}

fn pragma_u64(connection: &Connection, name: &str) -> Result<u64, SessionError> {
    let value: i64 = connection.pragma_query_value(None, name, |row| row.get(0))?;
    from_i64(value, "SQLite pragma")
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    #[cfg(unix)]
    use std::ffi::OsString;
    use std::fs::{self, TryLockError};
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;
    use std::sync::Barrier;

    use super::*;
    use crate::permission_state::StructuredPermissionEffect;
    use crate::sessions::{Session, StoredSubagentOutcome, TitleSource};
    use crate::state::{WorkspaceTabs, project_scope, read_workspace_tabs, write_workspace_tabs};
    use crate::usage_ledger::BUCKET_SECONDS;
    use crate::workflow::{WorkflowEventKind, WorkflowRunPatch, WorkflowRunRow, WorkflowUpdate};
    use crate::workflow_scratch::WORKFLOW_SCRATCH_DIR;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CwdHandle, ProjectIdentity, ProjectKey,
        SessionBindingId, SessionWorkspaceBinding, SourceTrustAnchor,
    };
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    const CWD: &str = "/project";
    const REMOTE_CWD: &str = ".";
    const MISSING_LEGACY_CWD: &str = "/definitely/missing/legacy/project";
    const MODEL: &str = "test/model";
    const NEXT_GENERATION: &str = "next-generation";
    const ARTIFACT_NAME: &str = "artifact";
    const LARGE_OUTPUT_ID: &str = "large";
    const FOREIGN_APPLICATION_ID: i64 = 1;
    const INITIALIZER_COUNT: usize = 2;
    const PERMISSION_PREVIOUS_SCHEMA: i64 = 7;
    const SMALL_OUTPUT_ID: &str = "small";
    const TOMBSTONES_TABLE: &str = "session_tombstones";
    const LEDGER_TABLE: &str = "usage_ledger";
    const WORKFLOW_RUNS_TABLE: &str = "workflow_runs";
    const WORKFLOW_EVENTS_TABLE_NAME: &str = "workflow_run_events";
    const TOOL_LEDGER_TABLE_NAME: &str = "tool_ledger";
    const TOOL_NAME: &str = "file_read";
    const TOOL_SOURCE: &str = "native";
    const TOOL_TOKENS: u32 = 120;
    const OUTCOME_SPLITS_ROWS: &str = "each outcome keeps a row of its own";
    const TOOL_HISTORY_OUTLIVES_SESSION: &str =
        "forgetting a transcript must not erase what the project did";
    const MIGRATED_MATCHES_FRESH: &str =
        "a migrated database must end with exactly the schema a fresh one gets";
    const MIGRATION_KEEPS_TRANSCRIPTS: &str =
        "compressing the payload columns must carry every row across unchanged";
    const PAYLOAD_STORED_COMPRESSED: &str =
        "a stored payload must not be the plaintext it was written from";
    const BYTE_COUNT_IS_UNCOMPRESSED: &str =
        "byte_count must stay the uncompressed length that trim and the history bounds read";
    const COMPRESSION_PREVIOUS_SCHEMA: i64 = 9;
    const BACKUP_KEEPS_ORIGIN: &str =
        "the pre-migration backup must stay readable by the version that wrote it";
    const PARTIAL_MIGRATION: &str = "a failed step must leave a version some binary can open";
    const FRESH_IS_CURRENT: &str =
        "a fresh database must get the current schema without replaying migrations";
    const OLDER_SCHEMA_VERSION: i64 = -1;
    const NEWER_SCHEMA_VERSION: i64 = SCHEMA_VERSION + 1;
    const MIGRATION_KEEPS_SPEND: &str =
        "widening the ledger key must carry every recorded row across";
    const LEDGER_COST: f64 = 3.5;
    const TRIM_KEEPS_SMALL: &str = "trim must keep rich outputs at or below the threshold";
    const TRIM_DROPS_LARGE: &str = "trim must drop rich outputs above the threshold";
    const ARTIFACTS_REMOVED: &str = "trim must remove every artifact directory";
    const VERSION_UNCHANGED: &str = "mark_opened must not bump write_version";
    const RELOCATION_DESTINATION: &str = "/destination with spaces";
    const RELOCATION_NESTED: &str = "/project/nested";
    const RELOCATION_DRAFT: &str = "draft with /project/reference";
    const RELOCATION_PLAN: &str = "/project/plan.md";
    const RELOCATION_FAILURE: &str = "injected relocation failure";
    const RELOCATION_RUN: &str = "relocation-run";
    const LEDGER_PROVIDER: &str = "test/provider";
    const OTHER_LEDGER_PROVIDER: &str = "other/provider";
    const OTHER_LEDGER_MODEL: &str = "other/model";
    const REPAIR_FAILURE: &str = "injected permission repair failure";
    const OLD_REVIEW: &str = "old untyped review";
    const HISTORY_TIME_MS: u64 = 1_767_225_600_000;
    const HISTORY_UUID: u128 = 0x019b_76da_a800_7000_8000_0000_0000_0001;
    const HISTORY_SESSION_CAP: usize = 4;
    const HISTORY_LARGE_SESSION_ROWS: usize = HISTORY_SESSION_CAP * 8;
    const HISTORY_STREAM: &str = "history-stream";
    const UNRECOVERED_JOURNAL: &[u8] = b"pending rollback recovery";
    #[cfg(unix)]
    const URI_PATH_COMPONENT: &str = "file: space #%2F?mode=rw&immutable=0-é";
    #[cfg(unix)]
    const NON_UTF8_PATH_BYTE: u8 = 0xff;
    const SESSIONS_BEFORE_WORKSPACE_BINDING: &str = r#"
DROP INDEX sessions_workspace_updated;
ALTER TABLE sessions DROP COLUMN workspace_cursor_label;
ALTER TABLE sessions DROP COLUMN workspace_cursor;
ALTER TABLE sessions DROP COLUMN workspace_project;
ALTER TABLE sessions DROP COLUMN workspace_principal;
ALTER TABLE sessions DROP COLUMN workspace_authority;
ALTER TABLE sessions DROP COLUMN workspace_source;
ALTER TABLE sessions DROP COLUMN workspace_binding;
"#;

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    struct TestMessage(String);

    impl TitleSource for TestMessage {
        fn first_user_text(&self) -> Option<&str> {
            Some(&self.0)
        }
    }

    type TestSession = Session<TestMessage, Value, Value>;

    fn state_dir() -> (TempDir, StateDir) {
        let temp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(temp.path().to_path_buf());
        (temp, state_dir)
    }

    fn permission_repair_fixture(
        database: &mut SessionDatabase,
    ) -> (RawPermissionSnapshot, RawPermissionSnapshot) {
        let record = |lifetime: &str| {
            json!({
                "id": CaudraId::generate().to_string(), "created_at": 1,
                "review": {"input_summary": OLD_REVIEW}, "unknown_record_field": [1, 2],
                "rule": {"subject": {"kind": "native", "owner": "workcell", "contract": "file.read.v1"},
                    "executor": "native", "resources": [], "arguments": {"constraint": "unconstrained"},
                    "lifetime": lifetime, "effect": "deny"}
            })
        };
        database
            .global_state_set(PERMISSION_RULES_KEY, &json!([record("global")]))
            .unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        session.push_message(TestMessage(OLD_REVIEW.into()));
        database.save(&session, None).unwrap();
        let metadata: String = database
            .connection
            .query_row("SELECT metadata FROM sessions", [], |row| row.get(0))
            .unwrap();
        let mut metadata: Value = serde_json::from_str(&metadata).unwrap();
        metadata[PERMISSION_METADATA_KEY] = json!([record("conversation")]);
        metadata["unknown_session_field"] = json!({"preserve": OLD_REVIEW});
        let metadata = serde_json::to_string(&metadata).unwrap();
        database.connection.execute("UPDATE sessions SET logical_bytes = logical_bytes + length(CAST(?1 AS BLOB)) - length(CAST(metadata AS BLOB)), metadata = ?1", [&metadata]).unwrap();
        let before = database.raw_permission_snapshot().unwrap();
        let mut after = before.clone();
        let review = json!({"tool": "file_read", "authority": "Bound tool; input unconstrained; resources unconstrained", "resources": [], "source": "unavailable"});
        let mut persistent: Value =
            serde_json::from_str(after.persistent.as_ref().unwrap()).unwrap();
        persistent[0]["review"] = review.clone();
        after.persistent = Some(serde_json::to_string(&persistent).unwrap());
        let mut metadata: Value = serde_json::from_str(&after.sessions[0].metadata).unwrap();
        metadata[PERMISSION_METADATA_KEY][0]["review"] = review;
        after.sessions[0].metadata = serde_json::to_string(&metadata).unwrap();
        (before, after)
    }

    #[test_case(false; "backup_and_accounting")]
    #[test_case(true; "rollback_after_persistent_update")]
    fn permission_review_repair_is_atomic_and_backed_up(fail: bool) {
        let (_temp, state) = state_dir();
        let mut database = SessionDatabase::open(&state).unwrap();
        let (before, after) = permission_repair_fixture(&mut database);
        let stats = database.stats().unwrap();
        let protected = |connection: &Connection| -> (String, Vec<u8>, i64, i64) {
            connection.query_row("SELECT token_usage, (SELECT payload FROM main_history_items LIMIT 1), updated_at, history_item_count FROM sessions", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))).unwrap()
        };
        let original = protected(&database.connection);
        if fail {
            database.connection.execute_batch(&format!("CREATE TRIGGER fail_permission_repair BEFORE UPDATE OF metadata ON sessions BEGIN SELECT RAISE(ABORT, '{REPAIR_FAILURE}'); END;")).unwrap();
        }
        drop(database);
        let result = SessionDatabase::repair_permission_reviews(&state, &before, &after);
        let database = SessionDatabase::open_read_only(&state).unwrap();
        assert_eq!(protected(&database.connection), original);
        if fail {
            assert!(result.unwrap_err().to_string().contains(REPAIR_FAILURE));
            assert_eq!(database.raw_permission_snapshot().unwrap(), before);
            assert_eq!(database.stats().unwrap().logical_bytes, stats.logical_bytes);
        } else {
            let backup = Connection::open(result.unwrap()).unwrap();
            assert_eq!(raw_permission_snapshot_on(&backup).unwrap(), before);
            assert_eq!(protected(&backup), original);
            let current = database.raw_permission_snapshot().unwrap();
            assert_eq!(current.persistent, after.persistent);
            assert_eq!(current.sessions[0].metadata, after.sessions[0].metadata);
            assert_eq!(
                current.sessions[0].write_version,
                before.sessions[0].write_version + 1
            );
            let delta =
                after.sessions[0].metadata.len() as i64 - before.sessions[0].metadata.len() as i64;
            assert_eq!(
                database.stats().unwrap().logical_bytes as i64,
                stats.logical_bytes as i64 + delta
            );
        }
    }

    #[test_case("state"; "stale_inventory")]
    #[test_case("session"; "stale_session")]
    #[test_case("authority"; "authority_change")]
    #[test_case("unknown"; "unknown_metadata_change")]
    #[test_case("locked"; "active_reader")]
    #[test_case("invalid"; "invalid_old_record")]
    fn permission_review_repair_refuses_unsafe_apply(case: &str) {
        let (_temp, state) = state_dir();
        let mut database = SessionDatabase::open(&state).unwrap();
        let (mut before, mut after) = permission_repair_fixture(&mut database);
        match case {
            "state" => database
                .global_state_set(PERMISSION_RULES_KEY, &json!([]))
                .unwrap(),
            "session" => {
                database
                    .connection
                    .execute("UPDATE sessions SET write_version = write_version + 1", [])
                    .unwrap();
            }
            "authority" => {
                let mut value: Value =
                    serde_json::from_str(after.persistent.as_ref().unwrap()).unwrap();
                value[0]["rule"]["effect"] = json!("allow");
                after.persistent = Some(value.to_string());
            }
            "unknown" => {
                let mut value: Value = serde_json::from_str(&after.sessions[0].metadata).unwrap();
                value["unknown_session_field"] = json!(false);
                after.sessions[0].metadata = value.to_string();
            }
            "invalid" => {
                let mut value: Value =
                    serde_json::from_str(before.persistent.as_ref().unwrap()).unwrap();
                value[0]["id"] = json!(OLD_REVIEW);
                before.persistent = Some(value.to_string());
            }
            _ => {}
        }
        let unchanged = database.raw_permission_snapshot().unwrap();
        if case == "locked" {
            assert!(SessionDatabase::repair_permission_reviews(&state, &before, &after).is_err());
            assert_eq!(database.raw_permission_snapshot().unwrap(), unchanged);
        } else {
            drop(database);
            assert!(SessionDatabase::repair_permission_reviews(&state, &before, &after).is_err());
            let database = SessionDatabase::open_read_only(&state).unwrap();
            assert_eq!(database.raw_permission_snapshot().unwrap(), unchanged);
        }
    }

    #[test_case(0, usize::MAX, usize::MAX; "row_limit")]
    #[test_case(usize::MAX, 0, usize::MAX; "byte_limit")]
    #[test_case(usize::MAX, usize::MAX, 0; "oversized_row")]
    fn permission_history_scan_reports_bounds(rows: usize, bytes: usize, row_bytes: usize) {
        let (_temp, state) = state_dir();
        let mut database = SessionDatabase::open(&state).unwrap();
        permission_repair_fixture(&mut database);
        let report = database
            .visit_permission_history(
                rows.min(i64::MAX as usize),
                bytes,
                row_bytes.min(i64::MAX as usize),
                |_, _| panic!("out-of-budget payload visited"),
            )
            .unwrap();
        assert!(report.truncated || report.oversized_rows > 0);
    }

    #[test_case("valid", true; "canonical_base58_uuidv7")]
    #[test_case("version", false; "wrong_uuid_version")]
    #[test_case("variant", false; "wrong_uuid_variant")]
    #[test_case("zero_time", false; "zero_creation_timestamp")]
    #[test_case("oversized", false; "bounded_id_parser")]
    fn history_record_identity_requires_uuidv7(case: &str, valid: bool) {
        let mut bytes = HISTORY_UUID.to_be_bytes();
        match case {
            "version" => bytes[UUID_VERSION_BYTE] = 0,
            "variant" => bytes[UUID_VARIANT_BYTE] = 0,
            "zero_time" => bytes[..UUID_VERSION_BYTE].fill(0),
            _ => {}
        }
        let id = CaudraId::from_bytes(bytes);
        let text = if case == "oversized" {
            "x".repeat(MAX_BASE58_UUID_BYTES + 1)
        } else {
            id.to_string()
        };
        let identity = history_record_identity(&json!({"id": text}));
        assert_eq!(identity.is_some(), valid);
        if valid {
            assert_eq!(identity, Some((id, HISTORY_TIME_MS)));
        }
    }

    #[test_case("migration"; "does_not_wait_for_exclusive_migration_lock")]
    fn history_read_only_open_is_nonblocking(_case: &str) {
        let (_temp, state) = state_dir();
        drop(SessionDatabase::open(&state).unwrap());
        let lock = existing_state_lock(&state.path().join(SESSIONS_DB_LOCK_FILE)).unwrap();
        lock.lock().unwrap();
        let result = SessionDatabase::open_read_only_nonblocking(&state);
        assert!(
            matches!(result, Err(SessionError::Storage(StorageError::Io(error))) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }

    #[test_case(false; "source_records_have_validated_id_and_timestamp")]
    #[test_case(true; "partial_read_failure_retains_scan_counts")]
    fn history_record_visitor_preserves_source_and_partial_reports(corrupt: bool) {
        let (_temp, state) = state_dir();
        let mut database = SessionDatabase::open(&state).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        session.push_message(TestMessage(ARTIFACT_NAME.into()));
        session.push_message(TestMessage(ARTIFACT_NAME.into()));
        database.save(&session, None).unwrap();
        let id = CaudraId::from_bytes(HISTORY_UUID.to_be_bytes());
        let payload = json!({"id": id, "type": "tool_call"}).to_string();
        database
            .connection
            .execute(
                "UPDATE main_history_items SET payload = ?1, byte_count = ?2",
                params![compress_payload(&payload).unwrap(), payload.len() as i64],
            )
            .unwrap();
        if corrupt {
            database
                .connection
                .execute(
                    "UPDATE main_history_items SET ordinal = -1 WHERE ordinal = 0",
                    [],
                )
                .unwrap();
        }
        let read_only = SessionDatabase::open_read_only_nonblocking(&state).unwrap();
        let limits = HistoryReadLimits {
            max_sessions: 2,
            max_rows: 4,
            max_bytes: MAX_PAYLOAD_BYTES,
            max_row_bytes: MAX_PAYLOAD_BYTES,
        };
        let mut report = HistoryReadReport::default();
        let mut visited = 0;
        let result = read_only.visit_history_records(
            CWD,
            &limits,
            &mut report,
            || false,
            |record| {
                assert_eq!(record.session_id, session.id);
                assert_eq!(record.history_id, id);
                assert_eq!(record.timestamp_ms, HISTORY_TIME_MS);
                assert_eq!(record.current_cwd, CWD);
                assert_eq!(record.subagent_id, None);
                visited += 1;
                ControlFlow::Continue(())
            },
        );
        assert_eq!(result.is_err(), corrupt);
        assert_eq!(report.rows, 2);
        assert_eq!(report.bytes, payload.len() * 2);
        assert_eq!(visited, if corrupt { 1 } else { 2 });
    }

    fn history_session(
        database: &mut SessionDatabase,
        serial: u64,
        main_rows: usize,
        subagent_rows: usize,
    ) -> CaudraId {
        let mut session = TestSession::new(MODEL, CWD);
        session.id = CaudraId::from_bytes((HISTORY_UUID + u128::from(serial)).to_be_bytes());
        session.replace_messages(vec![TestMessage(ARTIFACT_NAME.into()); main_rows]);
        if subagent_rows > 0 {
            session.set_subagent_messages(
                HISTORY_STREAM.into(),
                vec![TestMessage(ARTIFACT_NAME.into()); subagent_rows],
            );
        }
        session.updated_at = serial;
        database.save(&session, None).unwrap();
        let payload = json!({"id": session.id, "type": "tool_call"}).to_string();
        for table in ["main_history_items", "subagent_history_items"] {
            database.connection.execute(
                &format!("UPDATE {table} SET payload = ?1, byte_count = ?2 WHERE session_id = ?3"),
                params![
                    compress_payload(&payload).unwrap(),
                    payload.len() as i64,
                    session.id.as_bytes().as_slice()
                ],
            ).unwrap();
        }
        session.id
    }

    #[test_case(None; "generic_history_keeps_existing_session_order")]
    #[test_case(Some(HISTORY_SESSION_CAP); "fair_history_reaches_older_parent")]
    fn session_row_limit_is_opt_in(cap: Option<usize>) {
        let (_temp, state) = state_dir();
        let mut database = SessionDatabase::open(&state).unwrap();
        let older = history_session(&mut database, 1, 2, 0);
        let newer = history_session(&mut database, 2, HISTORY_LARGE_SESSION_ROWS, 0);
        let limits = HistoryReadLimits {
            max_sessions: 2,
            max_rows: HISTORY_SESSION_CAP + 2,
            max_bytes: MAX_PAYLOAD_BYTES,
            max_row_bytes: MAX_PAYLOAD_BYTES,
        };
        let read_only = SessionDatabase::open_read_only_nonblocking(&state).unwrap();
        let mut report = HistoryReadReport::default();
        let mut visited = Vec::new();
        let visit = |row: HistoryRecord<'_>| {
            visited.push(row.session_id);
            ControlFlow::Continue(())
        };
        if cap.is_some() {
            read_only
                .visit_history_records_with_session_limit(
                    CWD,
                    &limits,
                    cap,
                    &mut report,
                    || false,
                    visit,
                )
                .unwrap();
        } else {
            read_only
                .visit_history_records(CWD, &limits, &mut report, || false, visit)
                .unwrap();
        }
        assert_eq!(report.max_rows_per_session, cap);
        assert_eq!(report.rows, limits.max_rows);
        assert_eq!(report.sessions, if cap.is_some() { 2 } else { 1 });
        assert_eq!(report.session_row_cutoffs, usize::from(cap.is_some()));
        assert!(report.truncated);
        assert!(!report.stopped);
        let newest_rows = cap.unwrap_or(limits.max_rows);
        assert_eq!(&visited[..newest_rows], vec![newer; newest_rows]);
        if cap.is_some() {
            assert_eq!(&visited[newest_rows..], [older, older]);
        }
        assert_eq!(report.per_session[0].session_id, newer);
        assert_eq!(report.per_session[0].rows, newest_rows);
        assert_eq!(report.per_session[0].row_cutoff, cap.is_some());
        assert_eq!(
            report.per_session.iter().map(|row| row.rows).sum::<usize>(),
            report.rows
        );
        assert_eq!(
            report
                .per_session
                .iter()
                .map(|row| row.bytes)
                .sum::<usize>(),
            report.bytes
        );
        assert_eq!(read_only.connection.total_changes(), 0);
    }

    #[test_case(0, 0, false; "empty_session_is_complete")]
    #[test_case(HISTORY_SESSION_CAP, 0, false; "exact_main_limit_is_complete")]
    #[test_case(2, 2, false; "exact_combined_limit_is_complete")]
    #[test_case(0, HISTORY_SESSION_CAP, false; "exact_subagent_limit_is_complete")]
    #[test_case(HISTORY_SESSION_CAP, 1, true; "subagent_probe_proves_cutoff")]
    #[test_case(1, HISTORY_SESSION_CAP, true; "main_and_subagents_share_one_cap")]
    #[test_case(0, HISTORY_SESSION_CAP + 1, true; "subagents_are_bounded")]
    fn session_cutoff_requires_an_unread_row(main: usize, subagent: usize, cutoff: bool) {
        let (_temp, state) = state_dir();
        let mut database = SessionDatabase::open(&state).unwrap();
        history_session(&mut database, 1, 1, 0);
        let newest = history_session(&mut database, 2, main, subagent);
        let limits = HistoryReadLimits {
            max_sessions: 2,
            max_rows: HISTORY_LARGE_SESSION_ROWS,
            max_bytes: MAX_PAYLOAD_BYTES,
            max_row_bytes: MAX_PAYLOAD_BYTES,
        };
        let mut report = HistoryReadReport::default();
        database
            .visit_history_records_with_session_limit(
                CWD,
                &limits,
                Some(HISTORY_SESSION_CAP),
                &mut report,
                || false,
                |_| ControlFlow::Continue(()),
            )
            .unwrap();
        assert_eq!(report.sessions, 2);
        assert_eq!(report.per_session[0].session_id, newest);
        assert_eq!(
            report.per_session[0].rows,
            HISTORY_SESSION_CAP.min(main + subagent)
        );
        assert_eq!(report.per_session[0].row_cutoff, cutoff);
        assert_eq!(report.per_session[1].rows, 1);
        assert_eq!(report.truncated, cutoff);
        assert_eq!(report.session_row_cutoffs, usize::from(cutoff));
    }

    #[test_case("invalid"; "invalid_rows_consume_session_budget")]
    #[test_case("oversized"; "oversized_rows_consume_session_budget")]
    #[test_case("duplicate"; "duplicate_rows_consume_session_budget")]
    fn skipped_rows_cannot_starve_other_sessions(kind: &str) {
        let (_temp, state) = state_dir();
        let mut database = SessionDatabase::open(&state).unwrap();
        let older = history_session(&mut database, 1, 1, 0);
        let newest = history_session(&mut database, 2, HISTORY_LARGE_SESSION_ROWS, 0);
        let mut limits = HistoryReadLimits {
            max_sessions: 2,
            max_rows: HISTORY_LARGE_SESSION_ROWS,
            max_bytes: MAX_PAYLOAD_BYTES,
            max_row_bytes: MAX_PAYLOAD_BYTES,
        };
        let payload = match kind {
            "invalid" => Some(json!({"type": "tool_call"}).to_string()),
            "oversized" => {
                limits.max_row_bytes = MAX_IDENTIFIER_BYTES;
                Some(
                    json!({
                        "id": newest,
                        "type": "tool_call",
                        "content": "x".repeat(MAX_IDENTIFIER_BYTES + 1),
                    })
                    .to_string(),
                )
            }
            _ => None,
        };
        if let Some(payload) = payload {
            database.connection.execute(
                "UPDATE main_history_items SET payload = ?1, byte_count = ?2 WHERE session_id = ?3",
                params![
                    compress_payload(&payload).unwrap(),
                    payload.len() as i64,
                    newest.as_bytes().as_slice()
                ],
            ).unwrap();
        }
        let mut report = HistoryReadReport::default();
        let mut visited = Vec::new();
        database
            .visit_history_records_with_session_limit(
                CWD,
                &limits,
                Some(HISTORY_SESSION_CAP),
                &mut report,
                || false,
                |row| {
                    visited.push(row.session_id);
                    ControlFlow::Continue(())
                },
            )
            .unwrap();
        assert_eq!(report.rows, HISTORY_SESSION_CAP + 1);
        assert_eq!(report.session_row_cutoffs, 1);
        assert_eq!(
            report.invalid_records,
            if kind == "invalid" {
                HISTORY_SESSION_CAP
            } else {
                0
            }
        );
        assert_eq!(
            report.oversized_rows,
            if kind == "oversized" {
                HISTORY_SESSION_CAP
            } else {
                0
            }
        );
        assert_eq!(visited.last(), Some(&older));
        assert_eq!(
            visited.len(),
            if kind == "duplicate" {
                HISTORY_SESSION_CAP + 1
            } else {
                1
            }
        );
    }

    #[test_case(false; "stop_hook_at_session_boundary")]
    #[test_case(true; "visitor_stop_at_session_boundary")]
    fn fair_history_honors_stop_before_probing_or_advancing(visitor_stop: bool) {
        let (_temp, state) = state_dir();
        let mut database = SessionDatabase::open(&state).unwrap();
        history_session(&mut database, 1, 1, 0);
        history_session(&mut database, 2, HISTORY_LARGE_SESSION_ROWS, 0);
        let limits = HistoryReadLimits {
            max_sessions: 2,
            max_rows: HISTORY_LARGE_SESSION_ROWS,
            max_bytes: MAX_PAYLOAD_BYTES,
            max_row_bytes: MAX_PAYLOAD_BYTES,
        };
        let visited = Cell::new(0);
        let mut report = HistoryReadReport::default();
        database
            .visit_history_records_with_session_limit(
                CWD,
                &limits,
                Some(HISTORY_SESSION_CAP),
                &mut report,
                || !visitor_stop && visited.get() == HISTORY_SESSION_CAP,
                |_| {
                    visited.set(visited.get() + 1);
                    if visitor_stop && visited.get() == HISTORY_SESSION_CAP {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                },
            )
            .unwrap();
        assert!(report.stopped);
        assert!(!report.truncated);
        assert_eq!(report.rows, HISTORY_SESSION_CAP);
        assert_eq!(report.sessions, 1);
        assert_eq!(report.session_row_cutoffs, 0);
    }

    #[test_case(HISTORY_SESSION_CAP; "one_snapshot_across_parent_sessions")]
    fn fair_history_keeps_the_read_only_snapshot(cap: usize) {
        let (_temp, state) = state_dir();
        let mut database = SessionDatabase::open(&state).unwrap();
        let older = history_session(&mut database, 1, 1, 0);
        let newest = history_session(&mut database, 2, 1, 0);
        let read_only = SessionDatabase::open_read_only_nonblocking(&state).unwrap();
        let limits = HistoryReadLimits {
            max_sessions: 2,
            max_rows: HISTORY_LARGE_SESSION_ROWS,
            max_bytes: MAX_PAYLOAD_BYTES,
            max_row_bytes: MAX_PAYLOAD_BYTES,
        };
        let mut report = HistoryReadReport::default();
        read_only.visit_history_records_with_session_limit(
            CWD, &limits, Some(cap), &mut report, || false,
            |row| {
                if row.session_id == newest {
                    let payload = json!({"id": older, "type": "tool_call"}).to_string();
                    database.connection.execute(
                        "INSERT INTO main_history_items (session_id, ordinal, payload, byte_count) VALUES (?1, 1, ?2, ?3)",
                        params![
                            older.as_bytes().as_slice(),
                            compress_payload(&payload).unwrap(),
                            payload.len() as i64
                        ],
                    ).unwrap();
                }
                ControlFlow::Continue(())
            },
        ).unwrap();
        assert_eq!(report.rows, 2);
        assert!(!report.truncated);
        assert_eq!(read_only.connection.total_changes(), 0);
        let mut after = HistoryReadReport::default();
        read_only
            .visit_history_records_with_session_limit(
                CWD,
                &limits,
                Some(cap),
                &mut after,
                || false,
                |_| ControlFlow::Continue(()),
            )
            .unwrap();
        assert_eq!(after.rows, report.rows + 1);
    }

    fn stored_subagent(id: &str, outcome: StoredSubagentOutcome) -> StoredSubagent {
        StoredSubagent {
            tool_use_id: id.into(),
            parent_tool_use_id: Some(id.into()),
            root_tool_use_id: Some(id.into()),
            name: format!("task {id}"),
            model: Some(MODEL.into()),
            outcome,
        }
    }

    fn remote_binding(
        source: &str,
        principal: &str,
        project: &str,
        cursor: &str,
    ) -> StoredWorkspaceBinding {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new(source).unwrap(),
            "authority",
            "workspace",
            "generation",
            "namespace",
        )
        .unwrap();
        let principal = AuthenticatedPrincipalId::new(authority.clone(), principal).unwrap();
        let project = ProjectIdentity::new(authority.clone(), ProjectKey::new(project).unwrap());
        StoredWorkspaceBinding::new(
            SessionWorkspaceBinding::new(
                SessionBindingId::new(format!("binding-{source}-{cursor}")).unwrap(),
                authority,
                principal,
                project,
            )
            .unwrap(),
            CwdHandle::new(cursor).unwrap(),
            Some(cursor.into()),
        )
        .unwrap()
    }

    fn relocation_request(database: &SessionDatabase, cwd: &str, bulk: bool) -> SessionRelocation {
        SessionRelocation {
            sessions: database
                .local_session_locations()
                .unwrap()
                .into_iter()
                .filter(|session| session.cwd == cwd)
                .collect(),
            source_cwd: bulk.then(|| cwd.to_owned()),
            destination: RELOCATION_DESTINATION.into(),
            include_project_usage: bulk,
        }
    }

    fn relocation_workflow(
        database: &SessionDatabase,
        session_id: CaudraId,
        status: WorkflowRunStatus,
    ) -> WorkflowRunRow {
        database.connection.execute(
            "INSERT INTO workflow_runs (run_id, session_id, display_name, workflow_name, source_kind, \
             source_digest, language_version, abi_version, source, args, launch_mode, status, \
             agent_budget, usage, roster, result, error, created_at, updated_at) \
             VALUES (?1, ?2, ?1, ?1, 'project', ?1, 1, 1, ?4, '{}', 'build', ?3, 1, '{}', '[]', '{}', ?5, 0, 0)",
            params![RELOCATION_RUN, session_id.as_bytes().as_slice(), status.as_str(), RELOCATION_PLAN, RELOCATION_FAILURE],
        ).unwrap();
        database.connection.execute(
            "INSERT INTO workflow_calls (run_id, call_key, kind, request_hash, request, state, result, started_at) \
             VALUES (?1, 1, 'agent', ?1, '{}', 'completed', '{}', 0)",
            params![RELOCATION_RUN],
        ).unwrap();
        database
            .append_workflow_event(RELOCATION_RUN, WorkflowEventKind::Log, RELOCATION_DRAFT)
            .unwrap();
        database.load_workflow_run(RELOCATION_RUN).unwrap().unwrap()
    }

    fn ledger_entry(cwd: &str) -> LedgerEntry<'_> {
        LedgerEntry {
            bucket_start: 0,
            provider: LEDGER_PROVIDER,
            model: MODEL,
            cwd,
            purpose: LedgerPurpose::Chat,
            ephemeral: false,
            subscription: false,
            usage: StoredTokenUsage {
                input: 2,
                output: 3,
                cache_creation: 5,
                cache_read: 7,
                ..StoredTokenUsage::default()
            },
            cost: Some(LEDGER_COST),
        }
    }

    fn seed_colliding_usage(database: &SessionDatabase) -> Vec<UsageBucket> {
        for cwd in [CWD, RELOCATION_DESTINATION, RELOCATION_NESTED] {
            database.record_usage(&ledger_entry(cwd)).unwrap();
        }
        database.usage_buckets(None).unwrap()
    }

    /// A percentile reports the top of the bucket the call landed in, so the
    /// expected value is the ceiling of a duration rather than the duration.
    const P95_OF_30_MS: u64 = 31;
    const P95_OF_40_MS: u64 = 43;
    const P95_OF_60_MS: u64 = 63;
    const P95_OF_90_MS: u64 = 95;

    fn record_tool_at(
        database: &SessionDatabase,
        bucket_start: i64,
        cwd: &str,
        outcome: ToolOutcome,
        duration_ms: u64,
    ) {
        database
            .record_tool_call(&ToolLedgerEntry {
                bucket_start,
                tool: TOOL_NAME,
                source: TOOL_SOURCE,
                cwd,
                outcome,
                calls: 1,
                duration_ms,
                tokens: u64::from(TOOL_TOKENS),
                latency: &Latency::of(duration_ms),
            })
            .unwrap();
    }

    fn record_tool(database: &SessionDatabase, cwd: &str, outcome: ToolOutcome, duration_ms: u64) {
        record_tool_at(database, 0, cwd, outcome, duration_ms);
    }

    #[test]
    fn recorded_tool_calls_accumulate_and_keep_the_distribution() {
        let (_temp, state_dir) = state_dir();
        let database = SessionDatabase::open(&state_dir).unwrap();
        record_tool(&database, CWD, ToolOutcome::Ok, 90);
        record_tool(&database, CWD, ToolOutcome::Ok, 10);
        record_tool(&database, CWD, ToolOutcome::Timeout, 5);

        let buckets = database.tool_buckets(None, None).unwrap();
        assert_eq!(buckets.len(), 2, "{OUTCOME_SPLITS_ROWS}");
        let ok = buckets
            .iter()
            .find(|bucket| bucket.outcome == ToolOutcome::Ok)
            .unwrap();
        assert_eq!(ok.calls, 2);
        assert_eq!(ok.duration_ms, 100);
        assert_eq!(ok.tokens, u64::from(TOOL_TOKENS) * 2);
        assert_eq!(ok.latency.percentile(1.0), Some(P95_OF_90_MS));
    }

    #[test]
    fn tool_buckets_filter_to_one_project() {
        let (_temp, state_dir) = state_dir();
        let database = SessionDatabase::open(&state_dir).unwrap();
        for cwd in [CWD, RELOCATION_DESTINATION] {
            record_tool(&database, cwd, ToolOutcome::Ok, 1);
        }

        assert_eq!(database.tool_buckets(None, Some(CWD)).unwrap().len(), 1);
        assert_eq!(database.tool_buckets(None, None).unwrap().len(), 2);
    }

    #[test]
    fn pruning_tool_calls_keeps_buckets_at_the_cutoff() {
        let (_temp, state_dir) = state_dir();
        let database = SessionDatabase::open(&state_dir).unwrap();
        record_tool(&database, CWD, ToolOutcome::Ok, 1);
        record_tool_at(&database, BUCKET_SECONDS as i64, CWD, ToolOutcome::Ok, 1);

        assert_eq!(
            database
                .prune_tool_calls_before(BUCKET_SECONDS as i64)
                .unwrap(),
            1
        );
        assert_eq!(database.tool_buckets(None, None).unwrap().len(), 1);
    }

    #[test_case(false; "disjoint")]
    #[test_case(true; "colliding")]
    fn relocation_moves_tool_activity_with_the_spend(collision: bool) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        record_tool(&database, CWD, ToolOutcome::Ok, 40);
        record_tool(&database, CWD, ToolOutcome::Denied, 4);
        if collision {
            record_tool(&database, RELOCATION_DESTINATION, ToolOutcome::Ok, 60);
        }

        let result = database
            .relocate_project_usage(CWD, RELOCATION_DESTINATION)
            .unwrap();

        assert_eq!(result.tool_buckets_moved, 2);
        assert_eq!(result.tool_buckets_merged, usize::from(collision));
        assert!(database.tool_buckets(None, Some(CWD)).unwrap().is_empty());
        let moved = database
            .tool_buckets(None, Some(RELOCATION_DESTINATION))
            .unwrap();
        let ok = moved
            .iter()
            .find(|bucket| bucket.outcome == ToolOutcome::Ok)
            .unwrap();
        assert_eq!(ok.calls, 1 + u64::from(collision));
        assert_eq!(
            ok.latency.percentile(1.0),
            Some(if collision {
                P95_OF_60_MS
            } else {
                P95_OF_40_MS
            })
        );
    }

    #[test]
    fn a_sessions_tool_usage_survives_a_save_and_load() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        session.add_tool_usage(TOOL_NAME, TOOL_SOURCE, ToolOutcome::Ok, 30, TOOL_TOKENS);
        session.add_tool_usage(TOOL_NAME, TOOL_SOURCE, ToolOutcome::Ok, 10, TOOL_TOKENS);
        session.add_tool_usage(TOOL_NAME, TOOL_SOURCE, ToolOutcome::Denied, 2, 0);
        let id = session.id;
        database.save(&session, None).unwrap();

        let loaded: TestSession = database.load(id).unwrap();
        let usage = loaded.tool_usage();
        assert_eq!(usage.len(), 2, "{OUTCOME_SPLITS_ROWS}");
        let ok = usage
            .iter()
            .find(|row| row.outcome == ToolOutcome::Ok)
            .unwrap();
        assert_eq!(ok.calls, 2);
        assert_eq!(ok.duration_ms, 40);
        assert_eq!(ok.tokens, u64::from(TOOL_TOKENS) * 2);
        assert_eq!(ok.latency.percentile(1.0), Some(P95_OF_30_MS));
    }

    #[test]
    fn forgetting_a_session_keeps_the_project_tool_history() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        session.add_tool_usage(TOOL_NAME, TOOL_SOURCE, ToolOutcome::Ok, 1, TOOL_TOKENS);
        let id = session.id;
        database.save(&session, None).unwrap();
        record_tool(&database, CWD, ToolOutcome::Ok, 1);

        database.delete(id, None).unwrap();

        assert_eq!(
            database.tool_buckets(None, None).unwrap().len(),
            1,
            "{TOOL_HISTORY_OUTLIVES_SESSION}"
        );
    }

    #[test_case(false; "disjoint")]
    #[test_case(true; "colliding")]
    fn project_usage_relocation_preserves_full_key_and_every_measure(collision: bool) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let entries = [
            ledger_entry(CWD),
            LedgerEntry {
                bucket_start: BUCKET_SECONDS as i64,
                ..ledger_entry(CWD)
            },
            LedgerEntry {
                provider: OTHER_LEDGER_PROVIDER,
                ..ledger_entry(CWD)
            },
            LedgerEntry {
                model: OTHER_LEDGER_MODEL,
                ..ledger_entry(CWD)
            },
            LedgerEntry {
                purpose: LedgerPurpose::Goal,
                ..ledger_entry(CWD)
            },
            LedgerEntry {
                ephemeral: true,
                ..ledger_entry(CWD)
            },
            LedgerEntry {
                subscription: true,
                ..ledger_entry(CWD)
            },
        ];
        for mut entry in entries {
            database.record_usage(&entry).unwrap();
            entry.cost = None;
            database.record_usage(&entry).unwrap();
            entry.cost = Some(0.0);
            database.record_usage(&entry).unwrap();
        }
        if collision {
            database
                .record_usage(&ledger_entry(RELOCATION_DESTINATION))
                .unwrap();
        }
        for cwd in [RELOCATION_NESTED, MISSING_LEGACY_CWD] {
            database.record_usage(&ledger_entry(cwd)).unwrap();
        }
        let before = database.usage_buckets(None).unwrap();
        let result = database
            .relocate_project_usage(CWD, RELOCATION_DESTINATION)
            .unwrap();
        assert_eq!(
            result,
            ProjectUsageRelocation {
                buckets_moved: 7,
                buckets_merged: usize::from(collision),
                ..ProjectUsageRelocation::default()
            }
        );
        let after = database.usage_buckets(None).unwrap();
        assert!(!after.iter().any(|row| row.cwd == CWD));
        assert_eq!(after.len(), before.len() - result.buckets_merged);
        for source in before.iter().filter(|row| row.cwd == CWD) {
            let mut expected = source.clone();
            expected.cwd = RELOCATION_DESTINATION.into();
            if let Some(destination) = before.iter().find(|row| {
                row.cwd == RELOCATION_DESTINATION
                    && row.bucket_start == source.bucket_start
                    && row.provider == source.provider
                    && row.model == source.model
                    && row.purpose == source.purpose
                    && row.ephemeral == source.ephemeral
                    && row.subscription == source.subscription
            }) {
                expected.input += destination.input;
                expected.output += destination.output;
                expected.cache_creation += destination.cache_creation;
                expected.cache_read += destination.cache_read;
                expected.cost += destination.cost;
                expected.priced_turns += destination.priced_turns;
                expected.unpriced_turns += destination.unpriced_turns;
            }
            assert!(after.contains(&expected));
        }
        for row in before
            .iter()
            .filter(|row| row.cwd != CWD && row.cwd != RELOCATION_DESTINATION)
        {
            assert!(after.contains(row));
        }
        assert_eq!(
            database
                .relocate_project_usage(CWD, RELOCATION_DESTINATION)
                .unwrap(),
            ProjectUsageRelocation::default()
        );
        assert_eq!(
            database
                .relocate_project_usage(RELOCATION_DESTINATION, RELOCATION_DESTINATION)
                .unwrap(),
            ProjectUsageRelocation::default()
        );
        assert_eq!(database.usage_buckets(None).unwrap(), after);
    }

    #[test_case("", RELOCATION_DESTINATION; "empty_source")]
    #[test_case("relative", RELOCATION_DESTINATION; "relative_source")]
    #[test_case("/source\0", RELOCATION_DESTINATION; "nul_source")]
    #[test_case(CWD, ""; "empty_destination")]
    #[test_case(CWD, "relative"; "relative_destination")]
    #[test_case(CWD, "/destination\0"; "nul_destination")]
    fn project_usage_relocation_rejects_invalid_paths(source: &str, destination: &str) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let before = seed_colliding_usage(&database);
        assert!(matches!(
            database.relocate_project_usage(source, destination),
            Err(SessionError::InvalidRelocationSource | SessionError::InvalidRelocationDestination)
        ));
        assert_eq!(database.usage_buckets(None).unwrap(), before);
    }

    #[test_case(false, false, false, true; "single")]
    #[test_case(true, false, false, true; "bulk_optout")]
    #[test_case(true, true, false, true; "bulk_inclusive")]
    #[test_case(true, true, true, true; "same_directory")]
    #[test_case(true, true, false, false; "bulk_empty_ledger")]
    fn session_relocation_reports_usage_only_for_actual_inclusive_bulk(
        bulk: bool,
        include_usage: bool,
        same: bool,
        has_usage: bool,
    ) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        database.save(&TestSession::new(MODEL, CWD), None).unwrap();
        let before = if has_usage {
            seed_colliding_usage(&database)
        } else {
            Vec::new()
        };
        let mut request = relocation_request(&database, CWD, bulk);
        request.include_project_usage = include_usage;
        if same {
            request.destination = CWD.into();
        }
        let result = database.relocate_sessions(&request).unwrap();
        assert_eq!(result.sessions_moved, usize::from(!same));
        if include_usage && !same {
            assert_eq!(
                result.project_usage,
                Some(ProjectUsageRelocation {
                    buckets_moved: usize::from(has_usage),
                    buckets_merged: usize::from(has_usage),
                    ..ProjectUsageRelocation::default()
                })
            );
            assert!(
                !database
                    .usage_buckets(None)
                    .unwrap()
                    .iter()
                    .any(|row| row.cwd == CWD)
            );
        } else {
            assert_eq!(result.project_usage, None);
            assert_eq!(database.usage_buckets(None).unwrap(), before);
        }
    }

    #[test_case(false; "empty_selection")]
    #[test_case(true; "last_surviving_session")]
    fn project_usage_requires_explicit_bulk_source(has_session: bool) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        if has_session {
            database.save(&TestSession::new(MODEL, CWD), None).unwrap();
        }
        let before = seed_colliding_usage(&database);
        let mut request = relocation_request(&database, CWD, false);
        request.include_project_usage = true;
        assert!(matches!(
            database.relocate_sessions(&request),
            Err(SessionError::ProjectUsageRequiresSource)
        ));
        assert_eq!(
            database.local_session_locations().unwrap(),
            request.sessions
        );
        assert_eq!(database.usage_buckets(None).unwrap(), before);
    }

    #[test]
    fn empty_bulk_does_not_repair_usage_without_sessions() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let before = seed_colliding_usage(&database);
        let request = relocation_request(&database, CWD, true);
        assert_eq!(
            database.relocate_sessions(&request).unwrap(),
            SessionRelocationResult::default()
        );
        assert_eq!(database.usage_buckets(None).unwrap(), before);
    }

    #[test_case(false; "insert")]
    #[test_case(true; "update")]
    fn failed_usage_contribution_rolls_back_before_retry(existing: bool) {
        let (_temp, state_dir) = state_dir();
        let database = SessionDatabase::open(&state_dir).unwrap();
        let entry = ledger_entry(CWD);
        if existing {
            database.record_usage(&entry).unwrap();
        }
        let before = database.usage_buckets(None).unwrap();
        let operation = if existing { "UPDATE" } else { "INSERT" };
        database.connection.execute_batch(&format!(
            "CREATE TRIGGER usage_failure AFTER {operation} ON usage_ledger BEGIN SELECT RAISE(FAIL, '{RELOCATION_FAILURE}'); END"
        )).unwrap();
        assert!(
            database
                .record_usage(&entry)
                .unwrap_err()
                .to_string()
                .contains(RELOCATION_FAILURE)
        );
        assert_eq!(database.usage_buckets(None).unwrap(), before);
        database
            .connection
            .execute_batch("DROP TRIGGER usage_failure")
            .unwrap();
        database.record_usage(&entry).unwrap();
        let after = database.usage_buckets(None).unwrap();
        assert_eq!(
            after[0].input,
            u64::from(entry.usage.input) * (1 + u64::from(existing))
        );
        assert_eq!(after[0].priced_turns, 1 + u64::from(existing));
    }

    #[test]
    fn project_usage_overflow_rolls_back_without_clamping() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        seed_colliding_usage(&database);
        database
            .connection
            .execute(
                "UPDATE usage_ledger SET input_tokens = ?1 WHERE cwd = ?2",
                params![i64::MAX, RELOCATION_DESTINATION],
            )
            .unwrap();
        let before = database.usage_buckets(None).unwrap();
        assert!(matches!(
            database.relocate_project_usage(CWD, RELOCATION_DESTINATION),
            Err(SessionError::Sqlite(_))
        ));
        assert_eq!(database.usage_buckets(None).unwrap(), before);
    }

    #[test]
    fn project_usage_source_delete_failure_rolls_back_merge() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let before = seed_colliding_usage(&database);
        database.connection.execute_batch(&format!(
            "CREATE TRIGGER usage_failure AFTER DELETE ON usage_ledger BEGIN SELECT RAISE(FAIL, '{RELOCATION_FAILURE}'); END"
        )).unwrap();
        assert!(
            database
                .relocate_project_usage(CWD, RELOCATION_DESTINATION)
                .unwrap_err()
                .to_string()
                .contains(RELOCATION_FAILURE)
        );
        assert_eq!(database.usage_buckets(None).unwrap(), before);
        database
            .connection
            .execute_batch("DROP TRIGGER usage_failure")
            .unwrap();
        assert_eq!(
            database
                .relocate_project_usage(CWD, RELOCATION_DESTINATION)
                .unwrap(),
            ProjectUsageRelocation {
                buckets_moved: 1,
                buckets_merged: 1,
                ..ProjectUsageRelocation::default()
            }
        );
    }

    #[test]
    fn relocation_preserves_payloads_activity_and_binding_and_detaches_source_policy() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = TestSession::new(MODEL, MISSING_LEGACY_CWD);
        session.push_message(TestMessage(RELOCATION_DRAFT.into()));
        session.insert_tool_output(SMALL_OUTPUT_ID.into(), json!({"path": RELOCATION_PLAN}));
        session.set_subagent_messages(
            SMALL_OUTPUT_ID.into(),
            vec![TestMessage(RELOCATION_DRAFT.into())],
        );
        session.set_subagents(vec![stored_subagent(
            SMALL_OUTPUT_ID,
            StoredSubagentOutcome::Done,
        )]);
        session.token_usage = json!({"input": 25});
        session.meta.input_draft = Some(RELOCATION_DRAFT.into());
        session.meta.plan_path = Some(RELOCATION_PLAN.into());
        session.meta.plan_written = true;
        session.meta.yolo = Some(true);
        database.save(&session, None).unwrap();
        database
            .connection
            .execute(
                "UPDATE sessions SET metadata = json_set(metadata, '$.future_field', ?1, \
             '$.structured_permission_rules', json('[{\"future_rule\":true}]')) WHERE id = ?2",
                params![RELOCATION_DRAFT, session.id.as_bytes().as_slice()],
            )
            .unwrap();
        let before = root_on(&database.connection, session.id).unwrap();
        let request = relocation_request(&database, MISSING_LEGACY_CWD, false);

        assert_eq!(
            database.relocate_sessions(&request).unwrap().sessions_moved,
            1
        );

        let after = root_on(&database.connection, session.id).unwrap();
        assert_eq!(after.cwd, RELOCATION_DESTINATION);
        assert_eq!(after.updated_at, before.updated_at);
        assert_eq!(after.created_at, before.created_at);
        assert_eq!(after.write_version, before.write_version + 1);
        assert_eq!(after.token_usage, before.token_usage);
        assert_eq!(after.workspace_binding, before.workspace_binding);
        let metadata: Value = serde_json::from_str(&after.metadata).unwrap();
        assert_eq!(metadata["future_field"], RELOCATION_DRAFT);
        for field in RELOCATION_METADATA_FIELDS {
            assert!(metadata.get(field).is_none());
        }
        let loaded: TestSession = database.load(session.id).unwrap();
        assert_eq!(loaded.messages(), session.messages());
        assert_eq!(loaded.tool_outputs(), session.tool_outputs());
        assert_eq!(loaded.subagent_messages(), session.subagent_messages());
        assert_eq!(loaded.subagents(), session.subagents());
        assert_eq!(loaded.meta.input_draft, session.meta.input_draft);
        assert_eq!(
            after.logical_bytes + before.metadata.len(),
            before.logical_bytes + after.metadata.len()
        );
    }

    #[test_case(false; "explicit_current_only")]
    #[test_case(true; "exact_bulk_includes_closed")]
    fn relocation_reconciles_source_tabs_without_replacing_destination(bulk: bool) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let current = TestSession::new(MODEL, CWD);
        let closed = TestSession::new(MODEL, CWD);
        let donor = TestSession::new(MODEL, RELOCATION_DESTINATION);
        let nested = TestSession::new(MODEL, RELOCATION_NESTED);
        for session in [&current, &closed, &donor, &nested] {
            database.save(session, None).unwrap();
        }
        let before = database.local_session_locations().unwrap();
        let source_tabs = WorkspaceTabs {
            open: vec![current.id, closed.id],
            focused: Some(current.id),
        };
        let destination_tabs = WorkspaceTabs {
            open: vec![donor.id],
            focused: Some(donor.id),
        };
        write_workspace_tabs(&state_dir, Path::new(CWD), &source_tabs).unwrap();
        write_workspace_tabs(
            &state_dir,
            Path::new(RELOCATION_DESTINATION),
            &destination_tabs,
        )
        .unwrap();
        let mut request = relocation_request(&database, CWD, bulk);
        if !bulk {
            request.sessions.retain(|session| session.id == current.id);
        }

        assert_eq!(
            database.relocate_sessions(&request).unwrap().sessions_moved,
            request.sessions.len()
        );

        let after = database.local_session_locations().unwrap();
        assert_eq!(
            before.iter().map(|row| row.id).collect::<Vec<_>>(),
            after.iter().map(|row| row.id).collect::<Vec<_>>()
        );
        for original in before {
            let actual = after.iter().find(|row| row.id == original.id).unwrap();
            if request.sessions.iter().any(|row| row.id == original.id) {
                assert_eq!(actual.cwd, RELOCATION_DESTINATION);
                assert_eq!(actual.updated_at, original.updated_at);
                assert_eq!(actual.write_version, original.write_version + 1);
            } else {
                assert_eq!(actual, &original);
            }
        }
        let remaining = if bulk { Vec::new() } else { vec![closed.id] };
        assert_eq!(
            read_workspace_tabs(&state_dir, Path::new(CWD)).unwrap(),
            Some(WorkspaceTabs {
                focused: remaining.first().copied(),
                open: remaining,
            })
        );
        assert_eq!(
            read_workspace_tabs(&state_dir, Path::new(RELOCATION_DESTINATION)).unwrap(),
            Some(destination_tabs)
        );
    }

    #[test]
    fn relocation_inventory_and_bulk_exclude_remote_and_include_legacy_unbound() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let local = TestSession::new(MODEL, CWD);
        let remote = TestSession::new_with_workspace(
            MODEL,
            REMOTE_CWD,
            remote_binding("origin", "principal", "project", "cursor"),
        );
        for session in [&local, &remote] {
            database.save(session, None).unwrap();
        }
        database
            .connection
            .execute(
                "UPDATE sessions SET workspace_binding = '{}', workspace_source = '' WHERE id = ?1",
                params![local.id.as_bytes().as_slice()],
            )
            .unwrap();
        let request = relocation_request(&database, CWD, true);
        assert_eq!(request.sessions.len(), 1);
        assert_eq!(request.sessions[0].id, local.id);
        assert_eq!(
            database.relocate_sessions(&request).unwrap().sessions_moved,
            1
        );
        let root = root_on(&database.connection, remote.id).unwrap();
        assert_eq!(root.cwd, REMOTE_CWD);
        let remote_request = SessionRelocation {
            sessions: vec![SessionLocation {
                id: remote.id,
                title: root.title,
                cwd: root.cwd,
                updated_at: root.updated_at,
                write_version: root.write_version,
            }],
            source_cwd: None,
            destination: RELOCATION_DESTINATION.into(),
            include_project_usage: false,
        };
        assert!(matches!(
            database.relocate_sessions(&remote_request),
            Err(SessionError::RelocationBlocked {
                reason: RELOCATION_REMOTE,
                ..
            })
        ));
        assert_eq!(
            root_on(&database.connection, local.id)
                .unwrap()
                .workspace_binding,
            "{}"
        );
    }

    #[test_case(false, true; "new_member")]
    #[test_case(true, false; "deleted_member")]
    #[test_case(true, true; "same_count_different_members")]
    fn relocation_bulk_revalidates_membership_from_another_connection(delete: bool, insert: bool) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let usage_before = seed_colliding_usage(&database);
        let first = TestSession::new(MODEL, CWD);
        let second = TestSession::new(MODEL, CWD);
        database.save(&first, None).unwrap();
        database.save(&second, None).unwrap();
        let request = relocation_request(&database, CWD, true);
        let mut other = SessionDatabase::open(&state_dir).unwrap();
        if delete {
            other.delete(second.id, None).unwrap();
        }
        if insert {
            other.save(&TestSession::new(MODEL, CWD), None).unwrap();
        }
        let before = database.local_session_locations().unwrap();
        assert!(matches!(
            database.relocate_sessions(&request),
            Err(SessionError::RelocationSelectionChanged)
        ));
        assert_eq!(database.local_session_locations().unwrap(), before);
        assert_eq!(database.usage_buckets(None).unwrap(), usage_before);
    }

    #[test_case(false, false; "version_conflict_after_first_update")]
    #[test_case(true, false; "missing_id_after_first_update")]
    #[test_case(false, true; "bulk_version_conflict_after_first_update")]
    fn relocation_selection_failure_rolls_back_prior_rows(missing: bool, bulk: bool) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let usage_before = seed_colliding_usage(&database);
        database.save(&TestSession::new(MODEL, CWD), None).unwrap();
        database.save(&TestSession::new(MODEL, CWD), None).unwrap();
        let mut request = relocation_request(&database, CWD, bulk);
        if missing {
            request.sessions[1].id = CaudraId::generate();
        } else {
            let mut other = SessionDatabase::open(&state_dir).unwrap();
            let mut changed: TestSession = other.load(request.sessions[1].id).unwrap();
            changed.set_title(RELOCATION_DRAFT.into());
            other.save(&changed, None).unwrap();
        }
        let before = database.local_session_locations().unwrap();
        let error = database.relocate_sessions(&request).unwrap_err();
        if missing {
            assert!(matches!(
                error,
                SessionError::Storage(StorageError::NotFound(_))
            ));
        } else {
            assert!(matches!(
                error,
                SessionError::ConcurrentSessionWriter { .. }
            ));
        }
        assert_eq!(database.local_session_locations().unwrap(), before);
        assert_eq!(database.usage_buckets(None).unwrap(), usage_before);
    }

    #[test_case(Some(LEDGER_TABLE), WorkflowRunStatus::Failed; "ledger_failure_failed_run")]
    #[test_case(Some(LEDGER_TABLE), WorkflowRunStatus::Cancelled; "ledger_failure_cancelled_run")]
    #[test_case(None, WorkflowRunStatus::Failed; "session_failure_failed_run")]
    #[test_case(None, WorkflowRunStatus::Cancelled; "session_failure_cancelled_run")]
    #[test_case(Some(CWD), WorkflowRunStatus::Failed; "source_layout_failure_failed_run")]
    #[test_case(Some(CWD), WorkflowRunStatus::Cancelled; "source_layout_failure_cancelled_run")]
    #[test_case(Some(RELOCATION_DESTINATION), WorkflowRunStatus::Failed; "destination_layout_failure_failed_run")]
    #[test_case(Some(RELOCATION_DESTINATION), WorkflowRunStatus::Cancelled; "destination_layout_failure_cancelled_run")]
    fn relocation_database_failure_rolls_back_sessions_and_tabs(
        failure_scope: Option<&str>,
        status: WorkflowRunStatus,
    ) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        database.save(&TestSession::new(MODEL, CWD), None).unwrap();
        database.save(&TestSession::new(MODEL, CWD), None).unwrap();
        let usage_before = seed_colliding_usage(&database);
        let request = relocation_request(&database, CWD, true);
        let workflow = relocation_workflow(&database, request.sessions[0].id, status);
        let tabs = WorkspaceTabs {
            open: request.sessions.iter().map(|session| session.id).collect(),
            focused: Some(request.sessions[0].id),
        };
        write_workspace_tabs(&state_dir, Path::new(CWD), &tabs).unwrap();
        let donor = TestSession::new(MODEL, RELOCATION_DESTINATION);
        database.save(&donor, None).unwrap();
        let destination_tabs = WorkspaceTabs {
            open: vec![donor.id],
            focused: Some(donor.id),
        };
        write_workspace_tabs(
            &state_dir,
            Path::new(RELOCATION_DESTINATION),
            &destination_tabs,
        )
        .unwrap();
        let trigger = if failure_scope == Some(LEDGER_TABLE) {
            format!(
                "CREATE TRIGGER relocation_failure AFTER UPDATE ON usage_ledger BEGIN SELECT RAISE(FAIL, '{RELOCATION_FAILURE}'); END"
            )
        } else if let Some(scope) = failure_scope {
            format!(
                "CREATE TRIGGER relocation_failure BEFORE UPDATE ON state WHEN OLD.scope = '{}' \
                 AND NOT EXISTS(SELECT 1 FROM usage_ledger WHERE cwd = '{CWD}') \
                 BEGIN SELECT RAISE(ABORT, '{RELOCATION_FAILURE}'); END",
                project_scope(Path::new(scope))
            )
        } else {
            format!(
                "CREATE TRIGGER relocation_failure BEFORE UPDATE ON sessions WHEN OLD.id = X'{}' BEGIN SELECT RAISE(ABORT, '{RELOCATION_FAILURE}'); END",
                request.sessions[1]
                    .id
                    .as_bytes()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            )
        };
        database.connection.execute_batch(&trigger).unwrap();
        let before = database.local_session_locations().unwrap();
        assert!(
            database
                .relocate_sessions_with_tabs(&request, &Some(tabs.clone()))
                .unwrap_err()
                .to_string()
                .contains(RELOCATION_FAILURE)
        );
        assert_eq!(database.local_session_locations().unwrap(), before);
        assert_eq!(database.usage_buckets(None).unwrap(), usage_before);
        assert_eq!(
            database.load_workflow_run(RELOCATION_RUN).unwrap().unwrap(),
            workflow
        );
        assert_eq!(
            read_workspace_tabs(&state_dir, Path::new(CWD)).unwrap(),
            Some(tabs)
        );
        assert_eq!(
            read_workspace_tabs(&state_dir, Path::new(RELOCATION_DESTINATION)).unwrap(),
            Some(destination_tabs)
        );
    }

    #[test_case(false, false; "delta_snapshot")]
    #[test_case(true, false; "full_snapshot")]
    #[test_case(true, true; "cursorless_snapshot")]
    fn relocation_rejects_old_lineage_saves_and_allows_reloaded_lineage(
        full: bool,
        cursorless: bool,
    ) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        let (mut stale, cursor): (TestSession, _) = database.load_with_cursor(session.id).unwrap();
        let request = relocation_request(&database, CWD, false);
        database.relocate_sessions(&request).unwrap();
        if full {
            stale.replace_messages(vec![TestMessage(RELOCATION_DRAFT.into())]);
        } else {
            stale.push_message(TestMessage(RELOCATION_DRAFT.into()));
        }
        assert!(matches!(
            database.save(&stale, (!cursorless).then_some(&cursor)),
            Err(SessionError::ConcurrentSessionWriter { .. })
        ));
        assert!(matches!(
            database.relocate_sessions(&request),
            Err(SessionError::ConcurrentSessionWriter { .. })
        ));
        let (mut reloaded, new_cursor): (TestSession, _) =
            database.load_with_cursor(session.id).unwrap();
        assert!(!cursor.shares_lineage(&reloaded));
        reloaded.push_message(TestMessage(RELOCATION_DRAFT.into()));
        database.save(&reloaded, Some(&new_cursor)).unwrap();
        assert_eq!(
            root_on(&database.connection, session.id).unwrap().cwd,
            RELOCATION_DESTINATION
        );
    }

    #[test_case(false; "affected_session_revert")]
    #[test_case(true; "source_sibling_restore")]
    fn relocation_rejects_pending_source_revert_metadata(sibling: bool) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        let request = relocation_request(&database, CWD, false);
        let blocker = if sibling {
            TestSession::new(MODEL, CWD)
        } else {
            session
        };
        if sibling {
            database.save(&blocker, None).unwrap();
        }
        database.connection.execute("UPDATE sessions SET metadata = json_set(metadata, '$.pending_revert', json(?1)) WHERE id = ?2", params![if sibling { r#"{"restore_operation":{}}"# } else { "{}" }, blocker.id.as_bytes().as_slice()]).unwrap();
        let before = database.local_session_locations().unwrap();
        assert!(
            matches!(database.relocate_sessions(&request), Err(SessionError::RelocationBlocked { id, reason: RELOCATION_PENDING_REVERT }) if id == blocker.id)
        );
        assert_eq!(database.local_session_locations().unwrap(), before);
    }

    #[test_case(WorkflowRunStatus::Active, true, false; "active")]
    #[test_case(WorkflowRunStatus::Paused, true, false; "paused")]
    #[test_case(WorkflowRunStatus::BudgetLimited, true, false; "budget_limited")]
    #[test_case(WorkflowRunStatus::Completed, false, false; "completed")]
    #[test_case(WorkflowRunStatus::Cancelled, false, false; "cancelled")]
    #[test_case(WorkflowRunStatus::Failed, false, false; "failed")]
    #[test_case(WorkflowRunStatus::Interrupted, false, false; "interrupted")]
    #[test_case(WorkflowRunStatus::Cancelled, false, true; "cancelled_noop")]
    #[test_case(WorkflowRunStatus::Failed, false, true; "failed_noop")]
    fn relocation_invalidates_only_moved_resumable_terminal_runs(
        status: WorkflowRunStatus,
        blocked: bool,
        noop: bool,
    ) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        let mut workflow = relocation_workflow(&database, session.id, status);
        let calls = database.load_workflow_calls(RELOCATION_RUN).unwrap();
        let events = database.load_workflow_events(RELOCATION_RUN).unwrap();
        let mut request = relocation_request(&database, CWD, false);
        if noop {
            request.destination = CWD.into();
        }
        let result = database.relocate_sessions(&request);
        if blocked {
            assert!(matches!(
                result,
                Err(SessionError::RelocationBlocked {
                    reason: RELOCATION_WORKFLOW,
                    ..
                })
            ));
            assert_eq!(
                database.local_session_locations().unwrap(),
                request.sessions
            );
        } else {
            assert_eq!(result.unwrap().sessions_moved, usize::from(!noop));
        }
        let moved = database.load_workflow_run(RELOCATION_RUN).unwrap().unwrap();
        if !noop
            && matches!(
                status,
                WorkflowRunStatus::Failed | WorkflowRunStatus::Cancelled
            )
        {
            assert_eq!(
                database
                    .update_workflow_run(
                        RELOCATION_RUN,
                        workflow.revision,
                        workflow.execution_epoch,
                        &WorkflowRunPatch {
                            status: Some(WorkflowRunStatus::Active),
                            ..WorkflowRunPatch::default()
                        },
                    )
                    .unwrap(),
                WorkflowUpdate::Stale
            );
            workflow.status = WorkflowRunStatus::Interrupted;
            workflow.revision += 1;
            workflow.execution_epoch += 1;
            workflow.outbox_pending = true;
            workflow.updated_at = moved.updated_at;
        }
        assert_eq!(moved, workflow);
        assert_eq!(database.load_workflow_calls(RELOCATION_RUN).unwrap(), calls);
        assert_eq!(
            database.load_workflow_events(RELOCATION_RUN).unwrap(),
            events
        );
    }

    #[test_case(WorkflowRunStatus::Failed; "failed")]
    #[test_case(WorkflowRunStatus::Cancelled; "cancelled")]
    fn relocation_preserves_unselected_workflows(status: WorkflowRunStatus) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        database.save(&TestSession::new(MODEL, CWD), None).unwrap();
        let request = relocation_request(&database, CWD, false);
        let sibling = TestSession::new(MODEL, CWD);
        database.save(&sibling, None).unwrap();
        let workflow = relocation_workflow(&database, sibling.id, status);
        assert_eq!(
            database.relocate_sessions(&request).unwrap().sessions_moved,
            1
        );
        assert_eq!(
            database.load_workflow_run(RELOCATION_RUN).unwrap().unwrap(),
            workflow
        );
    }

    #[test]
    fn relocation_noops_and_duplicate_or_changed_cwd_requests_are_safe() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        database.save(&TestSession::new(MODEL, CWD), None).unwrap();
        let before = database.local_session_locations().unwrap();
        let mut request = relocation_request(&database, CWD, true);
        request.destination = CWD.into();
        let tabs = WorkspaceTabs {
            open: vec![request.sessions[0].id],
            focused: Some(request.sessions[0].id),
        };
        write_workspace_tabs(&state_dir, Path::new(CWD), &tabs).unwrap();
        assert_eq!(
            database
                .relocate_sessions_with_tabs(&request, &Some(WorkspaceTabs::default()))
                .unwrap(),
            SessionRelocationResult::default()
        );
        assert_eq!(
            read_workspace_tabs(&state_dir, Path::new(CWD)).unwrap(),
            Some(tabs)
        );
        assert_eq!(database.local_session_locations().unwrap(), before);
        request.sessions.push(request.sessions[0].clone());
        assert!(matches!(
            database.relocate_sessions(&request),
            Err(SessionError::RelocationSelectionChanged)
        ));
        request.sessions.pop();
        request.sessions[0].cwd = MISSING_LEGACY_CWD.into();
        assert!(matches!(
            database.relocate_sessions(&request),
            Err(SessionError::RelocationSelectionChanged)
        ));
        request.sessions.clear();
        assert!(matches!(
            database.relocate_sessions(&request),
            Err(SessionError::RelocationSelectionChanged)
        ));
        request.source_cwd = Some(MISSING_LEGACY_CWD.into());
        assert_eq!(
            database.relocate_sessions(&request).unwrap(),
            SessionRelocationResult::default()
        );
        request.source_cwd = None;
        request.include_project_usage = false;
        assert_eq!(
            database.relocate_sessions(&request).unwrap(),
            SessionRelocationResult::default()
        );
        assert_eq!(database.local_session_locations().unwrap(), before);
    }

    #[test]
    fn workspace_lookup_separates_origin_principal_project_and_nested_cursor() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let exact = remote_binding("origin-a", "principal-a", "project", "root");
        let nested = remote_binding("origin-a", "principal-a", "project", "nested");
        let other_principal = remote_binding("origin-a", "principal-b", "project", "root");
        let other_origin = remote_binding("origin-b", "principal-a", "project", "root");
        for binding in [&exact, &nested, &other_principal, &other_origin] {
            database
                .save(
                    &TestSession::new_with_workspace(MODEL, REMOTE_CWD, binding.clone()),
                    None,
                )
                .unwrap();
        }

        assert_eq!(database.list_for_workspace(&exact).unwrap().len(), 1);
        assert_eq!(database.list_for_workspace(&nested).unwrap().len(), 1);
        assert_eq!(database.list(REMOTE_CWD).unwrap().len(), 4);
        assert_eq!(
            database.list_for_workspace_identity(&exact).unwrap().len(),
            2
        );
    }

    #[test]
    fn workspace_lookup_separates_remote_generations_on_the_same_cursor() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let first = remote_binding("origin", "principal", "project", "cursor");
        let second = StoredWorkspaceBinding::new(
            first.binding().clone(),
            first.cwd_handle().clone(),
            Some(NEXT_GENERATION.into()),
        )
        .unwrap();
        for binding in [&first, &second] {
            database
                .save(
                    &TestSession::new_with_workspace(MODEL, REMOTE_CWD, binding.clone()),
                    None,
                )
                .unwrap();
        }

        assert_eq!(database.list_for_workspace(&first).unwrap().len(), 1);
        assert_eq!(database.list_for_workspace(&second).unwrap().len(), 1);
        assert!(!first.exact_scope_eq(&second));
    }

    #[test]
    fn a_saved_workspace_identity_cannot_be_replaced() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = TestSession::new_with_workspace(
            MODEL,
            REMOTE_CWD,
            remote_binding("origin", "principal", "project", "cursor"),
        );
        let cursor = database.save(&session, None).unwrap();
        session.workspace_binding = Some(Box::new(remote_binding(
            "changed-origin",
            "principal",
            "project",
            "cursor",
        )));

        assert!(matches!(
            database.save(&session, Some(&cursor)),
            Err(SessionError::WorkspaceIdentityImmutable)
        ));
    }

    #[test_case("/client/canary")]
    #[test_case("../escape")]
    #[test_case("nested/../../escape")]
    fn remote_cwd_persistence_rejects_host_paths_and_traversal(cwd: &str) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let session = TestSession::new_with_workspace(
            MODEL,
            cwd,
            remote_binding("origin", "principal", "project", "cursor"),
        );
        assert!(matches!(
            database.save(&session, None),
            Err(SessionError::InvalidRemoteCwd)
        ));
        assert!(database.list(cwd).unwrap().is_empty());
    }

    #[test]
    fn fresh_cursor_is_persisted_but_workspace_generation_is_immutable() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let old = remote_binding("origin", "principal", "project", "cursor");
        let mut session = TestSession::new_with_workspace(MODEL, REMOTE_CWD, old.clone());
        let cursor = database.save(&session, None).unwrap();
        let fresh = remote_binding("origin", "principal", "project", "nested");
        session.replace_workspace_cursor(fresh.clone()).unwrap();
        session.set_cwd("nested".into());
        let cursor = database.save(&session, Some(&cursor)).unwrap();
        assert_eq!(database.list_for_workspace_identity(&old).unwrap().len(), 1);
        assert_eq!(database.list_for_workspace(&fresh).unwrap().len(), 1);
        let serialized = serde_json::to_string(&fresh)
            .unwrap()
            .replace("\"generation\"", "\"changed-generation\"");
        let changed: StoredWorkspaceBinding = serde_json::from_str(&serialized).unwrap();
        session.workspace_binding = Some(Box::new(changed.clone()));
        assert!(matches!(
            database.save(&session, Some(&cursor)),
            Err(SessionError::WorkspaceIdentityImmutable)
        ));
        assert!(
            database
                .list_for_workspace_identity(&changed)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn database_round_trip_and_delta_append() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        session.push_message(TestMessage("one".into()));
        let cursor = database.save(&session, None).unwrap();
        session.push_message(TestMessage("two".into()));

        let cursor = database.save(&session, Some(&cursor)).unwrap();
        let (loaded, loaded_cursor) = database
            .load_with_cursor::<TestMessage, Value, Value>(session.id)
            .unwrap();

        assert_eq!(loaded.messages(), session.messages());
        assert_eq!(cursor.write_version(), loaded_cursor.write_version());
        let rows: i64 = database
            .connection
            .query_row("SELECT count(*) FROM main_history_items", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 2);
    }

    #[test]
    fn subagent_outcomes_round_trip() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        let subagents = vec![
            stored_subagent("unknown", StoredSubagentOutcome::Unknown),
            stored_subagent("done", StoredSubagentOutcome::Done),
            stored_subagent("killed", StoredSubagentOutcome::Killed),
            stored_subagent("error", StoredSubagentOutcome::Error),
        ];
        session.set_subagents(subagents.clone());

        database.save(&session, None).unwrap();

        let mut statement = database
            .connection
            .prepare("SELECT outcome FROM subagents ORDER BY ordinal")
            .unwrap();
        let stored = statement
            .query_map([], |row| row.get::<_, Option<String>>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            stored,
            ["unknown", "done", "killed", "error"].map(|outcome| Some(outcome.to_owned()))
        );
        drop(statement);
        let loaded = database
            .load::<TestMessage, Value, Value>(session.id)
            .unwrap();
        assert_eq!(loaded.subagents(), subagents);
    }

    #[test]
    fn repeated_updates_keep_one_canonical_state() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        session.push_message(TestMessage("message".into()));
        session.insert_tool_output("tool".into(), json!({"text": "initial"}));
        let mut cursor = database.save(&session, None).unwrap();

        for update in 0..1000 {
            session.set_title(format!("title {update}"));
            cursor = database.save(&session, Some(&cursor)).unwrap();
        }

        let session_rows: i64 = database
            .connection
            .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        let history_rows: i64 = database
            .connection
            .query_row("SELECT count(*) FROM main_history_items", [], |row| {
                row.get(0)
            })
            .unwrap();
        let output_rows: i64 = database
            .connection
            .query_row("SELECT count(*) FROM tool_outputs", [], |row| row.get(0))
            .unwrap();
        let event_table: Option<String> = database
            .connection
            .query_row(
                "SELECT name FROM sqlite_schema WHERE type = 'table' AND name = 'event'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(session_rows, 1);
        assert_eq!(history_rows, 1);
        assert_eq!(output_rows, 1);
        assert!(event_table.is_none());
    }

    #[test]
    fn stale_session_writer_is_rejected() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        session.push_message(TestMessage("base".into()));
        let cursor = database.save(&session, None).unwrap();
        session.set_persisted_write_version(Some(cursor.write_version()));
        let mut first = database
            .load::<TestMessage, Value, Value>(session.id)
            .unwrap();
        let mut second = database
            .load::<TestMessage, Value, Value>(session.id)
            .unwrap();
        first.set_title("first".into());
        second.set_title("second".into());

        database.save(&first, None).unwrap();
        let error = database.save(&second, None).unwrap_err();

        assert!(matches!(
            error,
            SessionError::ConcurrentSessionWriter { .. }
        ));
        assert_eq!(
            database
                .load::<TestMessage, Value, Value>(session.id)
                .unwrap()
                .title,
            "first"
        );
    }

    #[test]
    fn newer_cursor_cannot_authenticate_an_older_snapshot() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        session.push_message(TestMessage("base".into()));
        let first_cursor = database.save(&session, None).unwrap();
        let mut current = session.clone();
        let mut stale = session.clone();
        current.set_title("current".into());
        let current_cursor = database.save(&current, Some(&first_cursor)).unwrap();
        stale.set_title("stale".into());

        assert!(matches!(
            database.save(&stale, Some(&current_cursor)),
            Err(SessionError::ConcurrentSessionWriter { .. })
        ));
        assert_eq!(
            database
                .load::<TestMessage, Value, Value>(session.id)
                .unwrap()
                .title,
            "current"
        );
    }

    #[test]
    fn state_dir_apis_ignore_jsonl_sessions() {
        let (_temp, state_dir) = state_dir();
        let sessions_dir = state_dir.ensure_subdir(super::super::SESSIONS_DIR).unwrap();
        let id = CaudraId::generate();
        let source = sessions_dir.join(format!("{id}.jsonl"));
        fs::write(
            &source,
            format!(
                "{}\n{}\n",
                json!({
                    "t": "header",
                    "v": super::super::LOG_FORMAT_VERSION,
                    "id": id,
                    "model": MODEL,
                    "cwd": CWD,
                    "created_at": 0,
                }),
                json!({"t": "msg", "d": TestMessage("legacy".into())}),
            ),
        )
        .unwrap();

        assert!(matches!(
            TestSession::load(id, &state_dir),
            Err(SessionError::Storage(StorageError::NotFound(_)))
        ));
        assert!(TestSession::list(CWD, &state_dir).unwrap().is_empty());
        assert!(source.exists(), "a stray log is ignored, not consumed");
    }

    #[test]
    fn stale_writer_cannot_recreate_deleted_session() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        session.push_message(TestMessage("base".into()));
        database.save(&session, None).unwrap();
        let mut stale = database
            .load::<TestMessage, Value, Value>(session.id)
            .unwrap();
        let recreation = database.delete(session.id, Some(0)).unwrap();
        assert!(matches!(
            database.delete(session.id, None),
            Err(SessionError::Storage(StorageError::NotFound(_)))
        ));
        assert_eq!(database.tombstone_version(session.id).unwrap(), Some(0));
        stale.push_message(TestMessage("stale".into()));

        let error = database.save(&stale, None).unwrap_err();

        assert!(matches!(
            error,
            SessionError::Storage(StorageError::NotFound(_))
        ));
        let older_generation = stale.clone();
        let recreated_cursor = database.recreate(&stale, &recreation).unwrap();
        assert_eq!(
            database
                .load::<TestMessage, Value, Value>(session.id)
                .unwrap()
                .messages()
                .len(),
            2
        );
        assert!(matches!(
            database.save(&older_generation, Some(&recreated_cursor)),
            Err(SessionError::ConcurrentSessionWriter { .. })
        ));
        stale.push_message(TestMessage("new generation".into()));
        database.save(&stale, Some(&recreated_cursor)).unwrap();
    }

    #[test]
    fn stale_shrink_does_not_create_or_prune_archives() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        for message in ["one", "two", "three"] {
            session.push_message(TestMessage(message.into()));
        }
        database.save(&session, None).unwrap();
        let mut current = database
            .load::<TestMessage, Value, Value>(session.id)
            .unwrap();
        let mut stale = current.clone();
        current.set_title("current".into());
        database.save(&current, None).unwrap();
        stale.truncate_messages(1);

        let error = database.save(&stale, None).unwrap_err();

        assert!(matches!(
            error,
            SessionError::ConcurrentSessionWriter { .. }
        ));
        let archive_dir = state_dir
            .path()
            .join(super::super::SESSIONS_DIR)
            .join(super::super::ARCHIVE_DIR)
            .join(session.id.to_string());
        assert!(!archive_dir.exists());
    }

    #[test]
    fn failed_shrink_keeps_existing_archive_retention_unchanged() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        for message in ["one", "two", "three"] {
            session.push_message(TestMessage(message.into()));
        }
        database.save(&session, None).unwrap();
        let mut shrinking = database
            .load::<TestMessage, Value, Value>(session.id)
            .unwrap();
        shrinking.truncate_messages(1);
        let archive_dir = state_dir
            .path()
            .join(super::super::SESSIONS_DIR)
            .join(super::super::ARCHIVE_DIR)
            .join(session.id.to_string());
        fs::create_dir_all(&archive_dir).unwrap();
        let existing = archive_dir.join("1.jsonl");
        fs::write(&existing, b"existing recovery data").unwrap();
        database
            .connection
            .execute_batch(
                "CREATE TRIGGER reject_history_insert BEFORE INSERT ON main_history_items \
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();

        assert!(database.save(&shrinking, None).is_err());

        let archives = super::super::archives_newest_first(&archive_dir);
        assert_eq!(archives.len(), 1);
        assert_eq!(archives[0].path, existing);
        assert_eq!(
            database
                .load::<TestMessage, Value, Value>(session.id)
                .unwrap()
                .messages()
                .len(),
            3
        );
    }

    #[test]
    fn pending_archive_is_reconciled_from_committed_version() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        let mut current = database
            .load::<TestMessage, Value, Value>(session.id)
            .unwrap();
        current.set_title("version one".into());
        database.save(&current, None).unwrap();
        drop(database);
        let archive_dir = state_dir
            .path()
            .join(super::super::SESSIONS_DIR)
            .join(super::super::ARCHIVE_DIR)
            .join(session.id.to_string());
        fs::create_dir_all(&archive_dir).unwrap();
        let pending_name = format!(".pending-0-{}.jsonl", CaudraId::generate());
        fs::write(archive_dir.join(&pending_name), b"recovery").unwrap();
        let connection = Connection::open(state_dir.path().join(SESSIONS_DB_FILE)).unwrap();
        connection
            .execute(
                "INSERT INTO pending_archives \
                 (session_id, expected_write_version, pending_name, byte_count) \
                 VALUES (?1, 0, ?2, ?3)",
                params![session.id.as_bytes().as_slice(), pending_name, 8],
            )
            .unwrap();
        drop(connection);

        drop(SessionDatabase::open(&state_dir).unwrap());

        assert!(archive_dir.join("1.jsonl").exists());
        assert_eq!(fs::read_dir(archive_dir).unwrap().count(), 1);
    }

    #[test]
    fn pending_archives_finalize_in_generation_order() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        let mut current = database
            .load::<TestMessage, Value, Value>(session.id)
            .unwrap();
        current.set_title("one".into());
        database.save(&current, None).unwrap();
        current = database
            .load::<TestMessage, Value, Value>(session.id)
            .unwrap();
        current.set_title("two".into());
        database.save(&current, None).unwrap();
        drop(database);
        let archive_dir = state_dir
            .path()
            .join(super::super::SESSIONS_DIR)
            .join(super::super::ARCHIVE_DIR)
            .join(session.id.to_string());
        fs::create_dir_all(&archive_dir).unwrap();
        let first = format!(".pending-0-{}.jsonl", CaudraId::generate());
        let second = format!(".pending-1-{}.jsonl", CaudraId::generate());
        fs::write(archive_dir.join(&first), b"first").unwrap();
        fs::write(archive_dir.join(&second), b"second").unwrap();
        let connection = Connection::open(state_dir.path().join(SESSIONS_DB_FILE)).unwrap();
        for (version, name, bytes) in [(1, &second, 6), (0, &first, 5)] {
            connection
                .execute(
                    "INSERT INTO pending_archives \
                     (session_id, expected_write_version, pending_name, byte_count) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![session.id.as_bytes().as_slice(), version, name, bytes],
                )
                .unwrap();
        }
        drop(connection);

        drop(SessionDatabase::open(&state_dir).unwrap());

        assert_eq!(fs::read(archive_dir.join("1.jsonl")).unwrap(), b"first");
        assert_eq!(fs::read(archive_dir.join("2.jsonl")).unwrap(), b"second");
    }

    #[test]
    fn fresh_unregistered_pending_archive_is_not_raced_by_cleanup() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        drop(database);
        let archive_dir = state_dir
            .path()
            .join(super::super::SESSIONS_DIR)
            .join(super::super::ARCHIVE_DIR)
            .join(session.id.to_string());
        fs::create_dir_all(&archive_dir).unwrap();
        fs::write(
            archive_dir.join(format!(".pending-0-{}.jsonl", CaudraId::generate())),
            b"uncommitted",
        )
        .unwrap();

        drop(SessionDatabase::open(&state_dir).unwrap());

        assert_eq!(fs::read_dir(archive_dir).unwrap().count(), 1);
    }

    #[test]
    fn pending_cleanup_jobs_resume_when_database_reopens() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        let paths = [
            state_dir
                .path()
                .join(TOOL_OUTPUT_DIR)
                .join(session.id.to_string()),
            state_dir
                .path()
                .join(super::super::SESSIONS_DIR)
                .join(super::super::ARCHIVE_DIR)
                .join(session.id.to_string()),
            state_dir
                .path()
                .join(SESSION_SNAPSHOT_DIR)
                .join(session.id.to_string()),
        ];
        for path in &paths {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join("artifact"), b"data").unwrap();
        }
        database.delete(session.id, Some(0)).unwrap();
        drop(database);

        let database = SessionDatabase::open(&state_dir).unwrap();

        assert!(paths.iter().all(|path| !path.exists()));
        let jobs: i64 = database
            .connection
            .query_row("SELECT count(*) FROM cleanup_jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(jobs, 0);
    }

    #[test]
    fn cleanup_failure_does_not_lose_recreation_capability() {
        let (_temp, state_dir) = state_dir();
        let mut session = TestSession::new(MODEL, CWD);
        session.push_message(TestMessage("base".into()));
        session.save(&state_dir).unwrap();
        let archive_path = state_dir
            .path()
            .join(super::super::SESSIONS_DIR)
            .join(super::super::ARCHIVE_DIR)
            .join(session.id.to_string());
        fs::create_dir_all(archive_path.parent().unwrap()).unwrap();
        fs::write(&archive_path, b"not a directory").unwrap();

        let recreation =
            TestSession::delete_for_recreation(session.id, &state_dir, Some(0)).unwrap();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        database.recreate(&session, &recreation).unwrap();

        assert!(
            database
                .load::<TestMessage, Value, Value>(session.id)
                .is_ok()
        );
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_never_follows_symlinked_artifact_roots() {
        use std::os::unix::fs::symlink;

        let (_temp, state_dir) = state_dir();
        let mut session = TestSession::new(MODEL, CWD);
        session.save(&state_dir).unwrap();
        let outside = state_dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep"), b"data").unwrap();
        let sessions_dir = state_dir.ensure_subdir(super::super::SESSIONS_DIR).unwrap();
        symlink(&outside, sessions_dir.join(super::super::ARCHIVE_DIR)).unwrap();

        TestSession::delete_for_recreation(session.id, &state_dir, Some(0)).unwrap();

        assert!(outside.join("keep").exists());
    }

    #[cfg(unix)]
    #[test]
    fn archive_creation_rejects_symlinked_roots() {
        use std::os::unix::fs::symlink;

        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        session.push_message(TestMessage("one".into()));
        session.push_message(TestMessage("two".into()));
        database.save(&session, None).unwrap();
        let mut shrinking = database
            .load::<TestMessage, Value, Value>(session.id)
            .unwrap();
        shrinking.truncate_messages(1);
        let outside = tempfile::tempdir().unwrap();
        let sessions_dir = state_dir.ensure_subdir(super::super::SESSIONS_DIR).unwrap();
        symlink(outside.path(), sessions_dir.join(super::super::ARCHIVE_DIR)).unwrap();

        assert!(database.save(&shrinking, None).is_err());
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[test]
    fn destructive_shrink_exports_bounded_archive() {
        let (_temp, state_dir) = state_dir();
        let mut session = TestSession::new(MODEL, CWD);
        session.push_message(TestMessage("one".into()));
        session.push_message(TestMessage("two".into()));
        session.save(&state_dir).unwrap();
        session.truncate_messages(1);

        session.save(&state_dir).unwrap();

        let archive_dir = state_dir
            .path()
            .join(super::super::SESSIONS_DIR)
            .join(super::super::ARCHIVE_DIR)
            .join(session.id.to_string());
        let archive = fs::read_dir(archive_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let bytes = fs::read(archive).unwrap();
        assert!(bytes.starts_with(b"{\"t\":\"header\""));
        let database = SessionDatabase::open(&state_dir).unwrap();
        let pending: i64 = database
            .connection
            .query_row("SELECT count(*) FROM pending_archives", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(pending, 0);
    }

    #[test]
    fn new_database_has_incremental_vacuum_and_reports_stats() {
        let (_temp, state_dir) = state_dir();
        let database = SessionDatabase::open(&state_dir).unwrap();
        let stats = database.stats().unwrap();

        assert_eq!(stats.auto_vacuum, 2);
        assert_eq!(stats.schema_version, SCHEMA_VERSION);
        assert_eq!(stats.page_size, PAGE_SIZE as u64);
        database.quick_check().unwrap();
    }

    const LCG_MULTIPLIER: u64 = 6364136223846793005;
    const LCG_INCREMENT: u64 = 1442695040888963407;
    const VACUUM_ROWS: u64 = 64;
    const VACUUM_ROW_BYTES: usize = 8192;

    /// Hex drawn from an LCG, so a stored row still costs pages after
    /// compression. A repeated character would collapse to a few bytes and the
    /// freelist this test measures would never grow.
    fn high_entropy_text(seed: u64, bytes: usize) -> String {
        let mut state = seed | 1;
        let mut text = String::with_capacity(bytes + size_of::<u64>() * 2);
        while text.len() < bytes {
            state = state
                .wrapping_mul(LCG_MULTIPLIER)
                .wrapping_add(LCG_INCREMENT);
            text.push_str(&format!("{state:016x}"));
        }
        text.truncate(bytes);
        text
    }

    #[test]
    fn incremental_vacuum_frees_every_requested_page() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        for index in 0..VACUUM_ROWS {
            session.insert_tool_output(
                format!("output-{index}"),
                json!({"text": high_entropy_text(index, VACUUM_ROW_BYTES)}),
            );
        }
        database.save(&session, None).unwrap();
        database.delete(session.id, None).unwrap();
        let freelist = database.freelist_pages().unwrap();
        assert!(freelist > 1, "deleting rows must leave several free pages");

        let freed = database
            .incremental_vacuum(u32::try_from(freelist).unwrap())
            .unwrap();

        assert_eq!(freed, freelist);
        assert_eq!(database.freelist_pages().unwrap(), 0);
    }

    #[test]
    fn oversized_payload_is_rejected_before_write() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        session.push_message(TestMessage("x".repeat(MAX_PAYLOAD_BYTES)));

        let error = database.save(&session, None).unwrap_err();

        assert!(matches!(error, SessionError::LimitExceeded { .. }));
        assert!(database.persisted_session_ids().unwrap().is_empty());
    }

    #[test]
    fn identifier_and_json_depth_limits_are_enforced() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        session.set_model("x".repeat(MAX_IDENTIFIER_BYTES + 1));

        assert!(matches!(
            database.save(&session, None),
            Err(SessionError::LimitExceeded { .. })
        ));
        let nested = format!(
            "{}null{}",
            "[".repeat(MAX_JSON_DEPTH + 1),
            "]".repeat(MAX_JSON_DEPTH + 1)
        );
        assert!(matches!(
            validate_json_depth(&nested),
            Err(SessionError::LimitExceeded { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn database_and_lock_paths_reject_symlinks() {
        use std::os::unix::fs::symlink;

        for name in [SESSIONS_DB_FILE, SESSIONS_DB_LOCK_FILE] {
            let temp = TempDir::new().unwrap();
            let state_dir = StateDir::from_path(temp.path().join("state"));
            fs::create_dir_all(state_dir.path()).unwrap();
            let outside = temp.path().join("outside");
            fs::write(&outside, b"unchanged").unwrap();
            symlink(&outside, state_dir.path().join(name)).unwrap();

            assert!(SessionDatabase::open(&state_dir).is_err());
            assert_eq!(fs::read(&outside).unwrap(), b"unchanged");
        }
    }

    #[cfg(unix)]
    #[test]
    fn database_and_sidecars_are_owner_only() {
        let (_temp, state_dir) = state_dir();
        let database = SessionDatabase::open(&state_dir).unwrap();
        let path = database.path();

        for path in [
            path.clone(),
            database_sidecar(&path, "-wal"),
            database_sidecar(&path, "-shm"),
        ] {
            let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, OWNER_FILE_MODE);
        }
    }

    #[cfg(unix)]
    #[test]
    fn permissive_database_file_is_rejected() {
        let (_temp, state_dir) = state_dir();
        fs::create_dir_all(state_dir.path()).unwrap();
        let path = state_dir.path().join(SESSIONS_DB_FILE);
        fs::write(&path, []).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        assert!(SessionDatabase::open(&state_dir).is_err());
    }

    fn database_file_snapshot(state_dir: &StateDir) -> HashMap<PathBuf, Vec<u8>> {
        fs::read_dir(state_dir.path())
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let bytes = fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect()
    }

    fn assert_read_only_unavailable(result: Result<SessionDatabase, SessionError>) {
        assert!(
            matches!(result, Err(SessionError::Storage(StorageError::Io(error)))
            if error.kind() == io::ErrorKind::WouldBlock
                && error.to_string() == READ_ONLY_DATABASE_UNAVAILABLE)
        );
    }

    #[test_case(true; "enable")]
    #[test_case(false; "disable")]
    fn wal_retention_refuses_unsupported_file_control(persistent: bool) {
        let connection = Connection::open_in_memory().unwrap();
        assert!(matches!(configure_wal_retention(&connection, persistent),
            Err(SessionError::Sqlite(SqliteError::SqliteFailure(error, Some(message))))
                if error.extended_code == ffi::SQLITE_NOTFOUND
                    && message == WAL_PERSISTENCE_CONFIGURATION_FAILED));
    }

    #[cfg(unix)]
    #[test_case(SessionDatabase::open, true; "normal_writer")]
    #[test_case(SessionDatabase::open_state, true; "state_writer")]
    #[test_case(SessionDatabase::open_permission_admin, false; "permission_admin")]
    fn last_writer_close_preserves_sidecars_for_late_read_only_open(
        open: fn(&StateDir) -> Result<SessionDatabase, SessionError>,
        shared_reader_lock: bool,
    ) {
        let (_temp, state_dir) = state_dir();
        if !shared_reader_lock {
            drop(SessionDatabase::open_state(&state_dir).unwrap());
        }
        let mut database = open(&state_dir).unwrap();
        assert_eq!(
            pragma_u64(&database.connection, "journal_size_limit").unwrap(),
            WAL_RETENTION_LIMIT_BYTES
        );
        database.checkpoint(true).unwrap();
        let path = database.path();
        let checkpointed = fs::read(&path).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), checkpointed);
        let wal = database_sidecar(&path, "-wal");
        let shm = database_sidecar(&path, "-shm");
        assert!(fs::metadata(&wal).unwrap().len() > 0);
        let reader_lock = shared_reader_lock.then(|| {
            shared_existing_state_lock(&state_dir.path().join(SESSIONS_DB_LOCK_FILE)).unwrap()
        });
        assert_eq!(database_sidecars_exist(&path).unwrap(), [true, true, false]);
        let identities = || {
            [&path, &wal, &shm].map(|path| {
                let metadata = fs::metadata(path).unwrap();
                (metadata.dev(), metadata.ino())
            })
        };
        let checked_identities = identities();

        drop(database);
        assert_eq!(identities(), checked_identities);
        assert_eq!(fs::metadata(&wal).unwrap().len(), 0);
        let before = database_file_snapshot(&state_dir);
        let read_only = SessionDatabase::open_read_only_nonblocking(&state_dir).unwrap();
        assert_eq!(read_only.latest_id(CWD).unwrap(), Some(session.id));
        drop(read_only);

        let after = database_file_snapshot(&state_dir);
        assert_eq!(
            after.keys().collect::<HashSet<_>>(),
            before.keys().collect::<HashSet<_>>()
        );
        assert_eq!(identities(), checked_identities);
        for path in [&path, &wal] {
            assert_eq!(after[path], before[path]);
        }
        drop(reader_lock);
    }

    #[test_case("-wal", false; "blocking_missing_wal")]
    #[test_case("-shm", false; "blocking_missing_shm")]
    #[test_case("-wal", true; "nonblocking_missing_wal")]
    #[test_case("-shm", true; "nonblocking_missing_shm")]
    fn read_only_open_never_creates_missing_wal_sidecars(suffix: &str, nonblocking: bool) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        database.save(&TestSession::new(MODEL, CWD), None).unwrap();
        let path = database.path();
        fs::remove_file(database_sidecar(&path, suffix)).unwrap();
        let before = database_file_snapshot(&state_dir);

        let result = SessionDatabase::open_read_only_inner(&state_dir, nonblocking);
        assert_eq!(database_file_snapshot(&state_dir), before);
        assert_read_only_unavailable(result);
    }

    #[test_case("WAL", false; "blocking_clean_wal")]
    #[test_case("WAL", true; "nonblocking_clean_wal")]
    #[test_case("DELETE", false; "blocking_clean_rollback")]
    #[test_case("DELETE", true; "nonblocking_clean_rollback")]
    fn read_only_open_never_creates_files_for_closed_databases(mode: &str, nonblocking: bool) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        configure_wal_retention(&database.connection, false).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        database
            .connection
            .pragma_update(None, "journal_mode", mode)
            .unwrap();
        let path = database.path();
        drop(database);
        for suffix in DATABASE_SIDECAR_SUFFIXES {
            assert!(!database_sidecar(&path, suffix).exists());
        }
        let before = database_file_snapshot(&state_dir);

        let read_only = SessionDatabase::open_read_only_inner(&state_dir, nonblocking).unwrap();
        assert_eq!(read_only.latest_id(CWD).unwrap(), Some(session.id));
        assert_eq!(database_file_snapshot(&state_dir), before);
        drop(read_only);
        assert_eq!(database_file_snapshot(&state_dir), before);
    }

    #[test_case(false; "blocking")]
    #[test_case(true; "nonblocking")]
    fn read_only_open_preserves_uncheckpointed_wal_reads(nonblocking: bool) {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        database.checkpoint(true).unwrap();
        let path = database.path();
        let checkpointed = fs::read(&path).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), checkpointed);
        let before = database_file_snapshot(&state_dir);

        let read_only = SessionDatabase::open_read_only_inner(&state_dir, nonblocking).unwrap();
        assert_eq!(read_only.latest_id(CWD).unwrap(), Some(session.id));
        drop(read_only);

        let after = database_file_snapshot(&state_dir);
        assert_eq!(
            after.keys().collect::<HashSet<_>>(),
            before.keys().collect::<HashSet<_>>()
        );
        for path in [&path, &database_sidecar(&path, "-wal")] {
            assert_eq!(after[path], before[path]);
        }
    }

    #[test_case(false; "blocking")]
    #[test_case(true; "nonblocking")]
    fn read_only_offline_open_refuses_live_writer_without_sidecars(nonblocking: bool) {
        let (_temp, state_dir) = state_dir();
        let database = SessionDatabase::open(&state_dir).unwrap();
        configure_wal_retention(&database.connection, false).unwrap();
        database
            .connection
            .pragma_update(None, "journal_mode", "DELETE")
            .unwrap();
        assert_eq!(
            database_sidecars_exist(&database.path()).unwrap(),
            [false, false, false]
        );
        let before = database_file_snapshot(&state_dir);

        assert_read_only_unavailable(SessionDatabase::open_read_only_inner(
            &state_dir,
            nonblocking,
        ));
        assert_eq!(database_file_snapshot(&state_dir), before);
    }

    #[test_case(false; "blocking")]
    #[test_case(true; "nonblocking")]
    fn read_only_offline_open_holds_exclusive_lock_until_connection_closes(nonblocking: bool) {
        let (_temp, state_dir) = state_dir();
        let database = SessionDatabase::open(&state_dir).unwrap();
        configure_wal_retention(&database.connection, false).unwrap();
        drop(database);
        let before = database_file_snapshot(&state_dir);
        let read_only = SessionDatabase::open_read_only_inner(&state_dir, nonblocking).unwrap();
        let contender = existing_state_lock(&state_dir.path().join(SESSIONS_DB_LOCK_FILE)).unwrap();

        assert!(matches!(
            contender.try_lock_shared(),
            Err(TryLockError::WouldBlock)
        ));
        assert!(matches!(
            contender.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        assert_read_only_unavailable(SessionDatabase::open_read_only_inner(
            &state_dir,
            nonblocking,
        ));
        assert!(matches!(SessionDatabase::open_permission_admin(&state_dir),
            Err(SessionError::Storage(StorageError::Io(error)))
                if error.kind() == io::ErrorKind::WouldBlock));
        assert_eq!(database_file_snapshot(&state_dir), before);

        drop(read_only);
        contender.try_lock_shared().unwrap();
        drop(contender);
        let mut writable = SessionDatabase::open_state(&state_dir).unwrap();
        writable.save(&TestSession::new(MODEL, CWD), None).unwrap();
    }

    #[test_case(false; "blocking")]
    #[test_case(true; "nonblocking")]
    fn read_only_offline_open_never_ignores_rollback_journal(nonblocking: bool) {
        let (_temp, state_dir) = state_dir();
        let database = SessionDatabase::open(&state_dir).unwrap();
        configure_wal_retention(&database.connection, false).unwrap();
        let path = database.path();
        drop(database);
        fs::write(database_sidecar(&path, "-journal"), UNRECOVERED_JOURNAL).unwrap();
        let before = database_file_snapshot(&state_dir);

        assert_read_only_unavailable(SessionDatabase::open_read_only_inner(
            &state_dir,
            nonblocking,
        ));
        assert_eq!(database_file_snapshot(&state_dir), before);
    }

    #[cfg(unix)]
    #[test_case(false, false; "blocking_uri_metacharacters")]
    #[test_case(false, true; "blocking_non_utf8")]
    #[test_case(true, false; "nonblocking_uri_metacharacters")]
    #[test_case(true, true; "nonblocking_non_utf8")]
    fn read_only_offline_uri_preserves_path_bytes(nonblocking: bool, non_utf8: bool) {
        let temp = TempDir::new().unwrap();
        let mut name = URI_PATH_COMPONENT.as_bytes().to_vec();
        if non_utf8 {
            name.push(NON_UTF8_PATH_BYTE);
        }
        let state_dir = StateDir::from_path(temp.path().join(OsString::from_vec(name)));
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        configure_wal_retention(&database.connection, false).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        drop(database);
        let before = database_file_snapshot(&state_dir);

        let read_only = SessionDatabase::open_read_only_inner(&state_dir, nonblocking).unwrap();
        assert_eq!(read_only.latest_id(CWD).unwrap(), Some(session.id));
        assert_eq!(database_file_snapshot(&state_dir), before);
        drop(read_only);
        assert_eq!(database_file_snapshot(&state_dir), before);
    }

    fn artifact_paths(state_dir: &StateDir, id: CaudraId) -> [PathBuf; 4] {
        let name = id.to_string();
        [
            state_dir.path().join(TOOL_OUTPUT_DIR).join(&name),
            state_dir
                .path()
                .join(super::super::SESSIONS_DIR)
                .join(super::super::ARCHIVE_DIR)
                .join(&name),
            state_dir.path().join(SESSION_SNAPSHOT_DIR).join(&name),
            state_dir.path().join(WORKFLOW_SCRATCH_DIR).join(&name),
        ]
    }

    fn seed_artifacts(state_dir: &StateDir, id: CaudraId) -> [PathBuf; 4] {
        let paths = artifact_paths(state_dir, id);
        for path in &paths {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join(ARTIFACT_NAME), b"data").unwrap();
        }
        paths
    }

    fn session_with_outputs() -> TestSession {
        let mut session = TestSession::new(MODEL, CWD);
        session.push_message(TestMessage("prompt".into()));
        session.insert_tool_output(
            LARGE_OUTPUT_ID.into(),
            json!({"text": "x".repeat(usize::try_from(TRIM_KEEP_OUTPUT_BYTES).unwrap())}),
        );
        session.insert_tool_output(SMALL_OUTPUT_ID.into(), json!({"todo": []}));
        session
    }

    #[test]
    fn a_fresh_database_gets_the_current_schema_without_migrating() {
        let (_temp, state_dir) = state_dir();
        let database = SessionDatabase::open(&state_dir).unwrap();
        let table_exists = |name| {
            database
                .connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
                    params![name],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        };

        assert_eq!(database.stats().unwrap().schema_version, SCHEMA_VERSION);
        let application_id: i64 = database
            .connection
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .unwrap();
        assert_eq!(application_id, APPLICATION_ID);
        assert!(table_exists(TOMBSTONES_TABLE));
        assert!(table_exists(LEDGER_TABLE), "{FRESH_IS_CURRENT}");
    }

    #[test]
    fn concurrent_initializers_share_the_complete_schema() {
        let (_temp, state_dir) = state_dir();
        let barrier = Arc::new(Barrier::new(INITIALIZER_COUNT + 1));
        let handles = (0..INITIALIZER_COUNT)
            .map(|_| {
                let state_dir = state_dir.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    SessionDatabase::open(&state_dir)?.stats()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();

        for handle in handles {
            let stats = handle.join().unwrap().unwrap();
            assert_eq!(stats.schema_version, SCHEMA_VERSION);
            assert_eq!(stats.auto_vacuum, INCREMENTAL_AUTO_VACUUM);
        }
    }

    #[test]
    fn historical_v1_without_schema_identity_requires_a_reset() {
        let (_temp, state_dir) = state_dir();
        let path = state_dir.path().join(SESSIONS_DB_FILE);
        create_owner_only(&path).unwrap();
        let connection = Connection::open(path).unwrap();
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION)
            .unwrap();

        let error = verify_current_schema(&connection).unwrap_err();

        assert!(matches!(
            error,
            SessionError::CorruptDatabaseValue {
                field: "PRAGMA application_id",
                ..
            }
        ));
    }

    #[test]
    fn foreign_version_zero_database_is_not_claimed() {
        let (_temp, state_dir) = state_dir();
        let path = state_dir.path().join(SESSIONS_DB_FILE);
        create_owner_only(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "application_id", FOREIGN_APPLICATION_ID)
            .unwrap();
        drop(connection);

        let error = SessionDatabase::open(&state_dir).err().unwrap();
        let connection = Connection::open(path).unwrap();
        let application_id: i64 = connection
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .unwrap();
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();

        assert!(matches!(
            error,
            SessionError::CorruptDatabaseValue {
                field: "PRAGMA application_id",
                ..
            }
        ));
        assert_eq!(application_id, FOREIGN_APPLICATION_ID);
        assert_eq!(version, 0);
    }

    #[test]
    fn populated_version_zero_database_is_not_claimed() {
        let (_temp, state_dir) = state_dir();
        let path = state_dir.path().join(SESSIONS_DB_FILE);
        create_owner_only(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute("CREATE TABLE foreign_data (id)", [])
            .unwrap();
        drop(connection);

        let error = SessionDatabase::open(&state_dir).err().unwrap();
        let connection = Connection::open(path).unwrap();
        let foreign_table_exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'foreign_data')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();

        assert!(matches!(
            error,
            SessionError::CorruptDatabaseValue {
                field: "sqlite_schema",
                ..
            }
        ));
        assert!(foreign_table_exists);
        assert_eq!(version, 0);
    }

    /// A database as the previous release left it: schema 1, no ledger.
    fn seed_v1_database(state_dir: &StateDir) -> CaudraId {
        let path = state_dir.path().join(SESSIONS_DB_FILE);
        create_owner_only(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "page_size", PAGE_SIZE)
            .unwrap();
        connection
            .pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .unwrap();
        connection.execute_batch("VACUUM").unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        connection
            .execute_batch(SESSIONS_BEFORE_WORKSPACE_BINDING)
            .unwrap();
        connection
            .execute_batch(MODEL_USAGE_BEFORE_SUBSCRIPTION)
            .unwrap();
        connection
            .pragma_update(None, "application_id", APPLICATION_ID)
            .unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();
        let id = CaudraId::generate();
        connection
            .execute(
                "INSERT INTO sessions (id, format_version, title, cwd, model, created_at,\
                 updated_at, token_usage, metadata) \
                 VALUES (?1, ?2, 'kept', ?3, 'm', 1, 1, '{}', '{}')",
                params![
                    id.as_bytes().as_slice(),
                    i64::from(SESSION_VERSION),
                    MISSING_LEGACY_CWD
                ],
            )
            .unwrap();
        drop(connection);
        id
    }

    #[test_case(false; "exclusive_cutover")]
    #[test_case(true; "older_writer_must_close")]
    fn a_migration_backs_up_and_excludes_older_writers(hold_reader: bool) {
        let (_temp, dir) = state_dir();
        let path = dir.path().join(SESSIONS_DB_FILE);
        create_owner_only(&path).unwrap();
        let mut old = Connection::open(&path).unwrap();
        old.pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .unwrap();
        old.execute_batch("VACUUM").unwrap();
        old.execute_batch(&format!(
            "{SCHEMA}{USAGE_LEDGER_TABLE}{WORKFLOW_TABLES}{WORKFLOW_EVENTS_TABLE}"
        ))
        .unwrap();
        old.pragma_update(None, "application_id", APPLICATION_ID)
            .unwrap();
        old.pragma_update(None, "user_version", PERMISSION_PREVIOUS_SCHEMA)
            .unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        session.meta.structured_permission_rules = serde_json::from_value(json!([{
            "id": CaudraId::generate().to_string(), "created_at": 1,
            "rule": {"subject": {"kind": "native", "owner": "workcell", "contract": "file.read.v1"},
                "executor": "native", "resources": [], "arguments": {"constraint": "unconstrained"},
                "lifetime": "conversation", "effect": "deny"}
        }]))
        .unwrap();
        let serialized = SerializedSession::new(&session).unwrap();
        let transaction = old.transaction().unwrap();
        insert_root(&transaction, &session, &serialized).unwrap();
        transaction.commit().unwrap();
        let mut before = raw_permission_snapshot_on(&old).unwrap();
        let backup_path = dir.path().join(format!(
            "{SESSIONS_DB_FILE}.v{PERMISSION_PREVIOUS_SCHEMA}.bak"
        ));
        if hold_reader {
            let reader =
                shared_state_lock(&dir.path().join(SESSIONS_DB_LOCK_FILE), OWNER_FILE_MODE)
                    .unwrap();
            let error = SessionDatabase::open_state(&dir).err().unwrap();
            // A bare shared lock is a reader, not a session, so nothing can be
            // named and the message has to say which it is.
            assert!(
                matches!(
                    &error,
                    SessionError::MigrationBlocked { found, supported, holders }
                        if *found == PERMISSION_PREVIOUS_SCHEMA
                            && *supported == SCHEMA_VERSION
                            && holders == NO_NAMED_HOLDERS
                ),
                "{error}"
            );
            assert!(!backup_path.exists());
            assert_eq!(raw_permission_snapshot_on(&old).unwrap(), before);
            assert_eq!(
                old.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                    .unwrap(),
                PERMISSION_PREVIOUS_SCHEMA
            );
            session.meta.structured_permission_rules[0].rule.effect =
                StructuredPermissionEffect::Ask;
            let serialized = SerializedSession::new(&session).unwrap();
            let transaction = old.transaction().unwrap();
            update_root(&transaction, &session, &serialized, 0).unwrap();
            transaction.commit().unwrap();
            before = raw_permission_snapshot_on(&old).unwrap();
            drop(reader);
        }
        drop(old);
        let database = SessionDatabase::open_state(&dir).unwrap();
        assert_eq!(database.stats().unwrap().schema_version, SCHEMA_VERSION);
        assert_eq!(database.raw_permission_snapshot().unwrap(), before);
        let loaded = database
            .load::<TestMessage, Value, Value>(session.id)
            .unwrap();
        assert_eq!(
            loaded.meta.structured_permission_rules,
            session.meta.structured_permission_rules
        );
        assert_eq!(loaded.meta.permission_generation, 0);
        let backup = Connection::open(&backup_path).unwrap();
        assert_eq!(
            backup
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            PERMISSION_PREVIOUS_SCHEMA
        );
        assert_eq!(raw_permission_snapshot_on(&backup).unwrap(), before);
    }

    #[test]
    fn opening_a_v1_database_migrates_it_and_keeps_its_sessions() {
        let (_temp, state_dir) = state_dir();
        let id = seed_v1_database(&state_dir);

        let database = SessionDatabase::open(&state_dir).unwrap();

        assert_eq!(database.stats().unwrap().schema_version, SCHEMA_VERSION);
        assert_eq!(database.persisted_session_ids().unwrap(), vec![id]);
        assert_eq!(database.usage_buckets(None).unwrap(), Vec::new());
        let loaded = database.load::<TestMessage, Value, Value>(id).unwrap();
        assert_eq!(
            loaded.workspace_binding(),
            Some(&StoredWorkspaceBinding::local_from_cwd(MISSING_LEGACY_CWD))
        );
    }

    /// [`SCHEMA`] is always current, so a seeded old database has to give back
    /// the column the `3 -> 4` step adds, or that step finds its own work done.
    const MODEL_USAGE_BEFORE_SUBSCRIPTION: &str =
        "ALTER TABLE model_usage DROP COLUMN subscription_cost;";

    /// A database as the ledger's first release left it: schema 2, no purpose.
    fn seed_v2_database(state_dir: &StateDir) {
        let path = state_dir.path().join(SESSIONS_DB_FILE);
        create_owner_only(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "page_size", PAGE_SIZE)
            .unwrap();
        connection
            .pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .unwrap();
        connection.execute_batch("VACUUM").unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        connection
            .execute_batch(SESSIONS_BEFORE_WORKSPACE_BINDING)
            .unwrap();
        connection
            .execute_batch(MODEL_USAGE_BEFORE_SUBSCRIPTION)
            .unwrap();
        connection.execute_batch(USAGE_LEDGER_TABLE_V2).unwrap();
        connection
            .pragma_update(None, "application_id", APPLICATION_ID)
            .unwrap();
        connection.pragma_update(None, "user_version", 2).unwrap();
        connection
            .execute(
                "INSERT INTO usage_ledger (bucket_start, provider, model, cwd, ephemeral, \
                 input_tokens, output_tokens, cache_creation, cache_read, cost, \
                 priced_turns, unpriced_turns) \
                 VALUES (0, 'anthropic', ?1, ?2, 0, 10, 20, 0, 0, ?3, 1, 0)",
                params![MODEL, CWD, LEDGER_COST],
            )
            .unwrap();
        drop(connection);
    }

    #[test]
    fn migrating_a_v2_ledger_attributes_its_rows_to_chat() {
        let (_temp, state_dir) = state_dir();
        seed_v2_database(&state_dir);

        let database = SessionDatabase::open(&state_dir).unwrap();
        let rows = database.usage_buckets(None).unwrap();

        assert_eq!(database.stats().unwrap().schema_version, SCHEMA_VERSION);
        assert_eq!(rows.len(), 1, "{MIGRATION_KEEPS_SPEND}");
        assert_eq!(
            rows[0].purpose,
            LedgerPurpose::Chat.storage_name(),
            "{MIGRATION_KEEPS_SPEND}"
        );
        assert_eq!(rows[0].cost, LEDGER_COST, "{MIGRATION_KEEPS_SPEND}");
        assert_eq!(rows[0].model, MODEL, "{MIGRATION_KEEPS_SPEND}");
        assert!(!rows[0].subscription, "{MIGRATION_KEEPS_SPEND}");
    }

    /// A row recorded before Caudra tracked the payer cannot say which it had.
    /// Calling it billed is what leaves every existing all-time total alone.
    #[test]
    fn migrating_a_ledger_bills_the_rows_that_predate_the_split() {
        let (_temp, state_dir) = state_dir();
        seed_v2_database(&state_dir);

        let database = SessionDatabase::open(&state_dir).unwrap();
        let lifetime = database.usage_buckets(None).unwrap();

        assert_eq!(database.stats().unwrap().schema_version, SCHEMA_VERSION);
        let billed: f64 = lifetime
            .iter()
            .filter(|row| !row.subscription)
            .map(|row| row.cost)
            .sum();
        assert_eq!(billed, LEDGER_COST, "{MIGRATION_KEEPS_SPEND}");
        assert!(
            lifetime.iter().all(|row| !row.subscription),
            "{MIGRATION_KEEPS_SPEND}"
        );
    }

    /// A database as the release before workflow storage left it: schema 4.
    fn seed_v4_database(state_dir: &StateDir) {
        let path = state_dir.path().join(SESSIONS_DB_FILE);
        create_owner_only(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "page_size", PAGE_SIZE)
            .unwrap();
        connection
            .pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .unwrap();
        connection.execute_batch("VACUUM").unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        connection
            .execute_batch(SESSIONS_BEFORE_WORKSPACE_BINDING)
            .unwrap();
        connection.execute_batch(USAGE_LEDGER_TABLE).unwrap();
        connection
            .pragma_update(None, "application_id", APPLICATION_ID)
            .unwrap();
        connection.pragma_update(None, "user_version", 4).unwrap();
        drop(connection);
    }

    /// A database as the release before the workflow timeline left it: schema 5.
    fn seed_v5_database(state_dir: &StateDir) {
        let path = state_dir.path().join(SESSIONS_DB_FILE);
        create_owner_only(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "page_size", PAGE_SIZE)
            .unwrap();
        connection
            .pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .unwrap();
        connection.execute_batch("VACUUM").unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        connection
            .execute_batch(SESSIONS_BEFORE_WORKSPACE_BINDING)
            .unwrap();
        connection.execute_batch(USAGE_LEDGER_TABLE).unwrap();
        connection.execute_batch(WORKFLOW_TABLES).unwrap();
        connection
            .pragma_update(None, "application_id", APPLICATION_ID)
            .unwrap();
        connection.pragma_update(None, "user_version", 5).unwrap();
        drop(connection);
    }

    /// The three payload tables as schema 9 left them: plain JSON text, with the
    /// two CHECK constraints the compressed shape cannot carry.
    const HISTORY_TABLES_BEFORE_COMPRESSION: &str = r#"
DROP TABLE subagent_history_items;
DROP TABLE tool_outputs;
DROP TABLE main_history_items;

CREATE TABLE main_history_items (
    session_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ordinal    INTEGER NOT NULL,
    payload    TEXT NOT NULL CHECK(json_valid(payload)),
    byte_count INTEGER NOT NULL CHECK(byte_count = length(CAST(payload AS BLOB))),
    PRIMARY KEY(session_id, ordinal)
) STRICT, WITHOUT ROWID;

CREATE TABLE tool_outputs (
    session_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    tool_id    TEXT NOT NULL,
    payload    TEXT NOT NULL CHECK(json_valid(payload)),
    byte_count INTEGER NOT NULL CHECK(byte_count = length(CAST(payload AS BLOB))),
    PRIMARY KEY(session_id, tool_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE subagent_history_items (
    session_id  BLOB NOT NULL,
    subagent_id TEXT NOT NULL,
    ordinal     INTEGER NOT NULL,
    payload     TEXT NOT NULL CHECK(json_valid(payload)),
    byte_count  INTEGER NOT NULL CHECK(byte_count = length(CAST(payload AS BLOB))),
    PRIMARY KEY(session_id, subagent_id, ordinal),
    FOREIGN KEY(session_id, subagent_id)
        REFERENCES subagent_streams(session_id, subagent_id)
        ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
"#;

    /// A database as the release before payload compression left it: schema 9,
    /// carrying one session whose transcript, tool output and subagent stream
    /// are all stored as text.
    fn seed_v9_database(state_dir: &StateDir) -> (CaudraId, String) {
        let path = state_dir.path().join(SESSIONS_DB_FILE);
        create_owner_only(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "page_size", PAGE_SIZE)
            .unwrap();
        connection
            .pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .unwrap();
        connection.execute_batch("VACUUM").unwrap();
        connection.execute_batch(&full_schema()).unwrap();
        connection
            .execute_batch(HISTORY_TABLES_BEFORE_COMPRESSION)
            .unwrap();

        let mut session = TestSession::new(MODEL, CWD);
        session.push_message(TestMessage(ARTIFACT_NAME.into()));
        session.insert_tool_output(SMALL_OUTPUT_ID.into(), json!(ARTIFACT_NAME));
        session.set_subagent_messages(
            HISTORY_STREAM.into(),
            vec![TestMessage(ARTIFACT_NAME.into())],
        );
        let serialized = SerializedSession::new(&session).unwrap();
        let transaction = connection.unchecked_transaction().unwrap();
        insert_root(&transaction, &session, &serialized).unwrap();
        transaction.commit().unwrap();

        let payload = serde_json::to_string(&TestMessage(ARTIFACT_NAME.into())).unwrap();
        let id = session.id.as_bytes();
        connection
            .execute(
                "INSERT INTO main_history_items (session_id, ordinal, payload, byte_count) \
                 VALUES (?1, 0, ?2, length(CAST(?2 AS BLOB)))",
                params![id.as_slice(), payload],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO tool_outputs (session_id, tool_id, payload, byte_count) \
                 VALUES (?1, ?2, ?3, length(CAST(?3 AS BLOB)))",
                params![id.as_slice(), SMALL_OUTPUT_ID, payload],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO subagent_streams (session_id, subagent_id, task_spec) \
                 VALUES (?1, ?2, NULL)",
                params![id.as_slice(), HISTORY_STREAM],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO subagent_history_items \
                 (session_id, subagent_id, ordinal, payload, byte_count) \
                 VALUES (?1, ?2, 0, ?3, length(CAST(?3 AS BLOB)))",
                params![id.as_slice(), HISTORY_STREAM, payload],
            )
            .unwrap();
        connection
            .pragma_update(None, "application_id", APPLICATION_ID)
            .unwrap();
        connection
            .pragma_update(None, "user_version", COMPRESSION_PREVIOUS_SCHEMA)
            .unwrap();
        drop(connection);
        (session.id, payload)
    }

    fn stored_payload(database: &SessionDatabase, sql: &str) -> Vec<u8> {
        database
            .connection
            .query_row(sql, [], |row| row.get::<_, Vec<u8>>(0))
            .unwrap()
    }

    #[test]
    fn migrating_a_v9_database_compresses_its_payloads() {
        let (_migrated_temp, migrated_dir) = state_dir();
        let (_fresh_temp, fresh_dir) = state_dir();
        let (id, payload) = seed_v9_database(&migrated_dir);

        let migrated = SessionDatabase::open(&migrated_dir).unwrap();
        let fresh = SessionDatabase::open(&fresh_dir).unwrap();

        assert_eq!(migrated.stats().unwrap().schema_version, SCHEMA_VERSION);
        assert_eq!(
            schema_objects(&migrated),
            schema_objects(&fresh),
            "{MIGRATED_MATCHES_FRESH}"
        );
        for sql in [
            "SELECT payload FROM main_history_items",
            "SELECT payload FROM tool_outputs",
            "SELECT payload FROM subagent_history_items",
        ] {
            let stored = stored_payload(&migrated, sql);
            assert_ne!(stored, payload.as_bytes(), "{PAYLOAD_STORED_COMPRESSED}");
            assert_eq!(
                decompress_payload(&stored, "payload").unwrap(),
                payload,
                "{MIGRATION_KEEPS_TRANSCRIPTS}"
            );
        }
        let byte_count: i64 = migrated
            .connection
            .query_row("SELECT byte_count FROM main_history_items", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            byte_count,
            payload.len() as i64,
            "{BYTE_COUNT_IS_UNCOMPRESSED}"
        );
        let loaded = migrated.load::<TestMessage, Value, Value>(id).unwrap();
        assert_eq!(
            loaded.messages(),
            &[TestMessage(ARTIFACT_NAME.into())],
            "{MIGRATION_KEEPS_TRANSCRIPTS}"
        );
        // The visitor opens with the session metadata, so only the rows equal to
        // the seeded payload are the transcript ones this migration moved.
        let mut payload_rows = 0;
        migrated
            .visit_payload_json(id, |text| {
                if text == payload {
                    payload_rows += 1;
                }
            })
            .unwrap();
        assert_eq!(
            payload_rows,
            COMPRESSED_PAYLOAD_TABLES.len(),
            "{MIGRATION_KEEPS_TRANSCRIPTS}"
        );
    }

    #[test]
    fn a_saved_payload_is_stored_compressed_with_its_uncompressed_byte_count() {
        let (_temp, state) = state_dir();
        let mut database = SessionDatabase::open(&state).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        let text = high_entropy_text(1, VACUUM_ROW_BYTES);
        session.insert_tool_output(SMALL_OUTPUT_ID.into(), json!({"text": text}));
        database.save(&session, None).unwrap();

        let (stored, byte_count): (Vec<u8>, i64) = database
            .connection
            .query_row("SELECT payload, byte_count FROM tool_outputs", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();

        let decoded = decompress_payload(&stored, "payload").unwrap();
        assert!(stored.len() < decoded.len(), "{PAYLOAD_STORED_COMPRESSED}");
        assert_eq!(
            byte_count,
            decoded.len() as i64,
            "{BYTE_COUNT_IS_UNCOMPRESSED}"
        );
        let loaded = database.load::<TestMessage, Value, Value>(session.id).unwrap();
        assert_eq!(loaded.tool_outputs(), session.tool_outputs());
    }

    #[test]
    fn a_corrupt_compressed_payload_is_a_named_error() {
        let (_temp, state) = state_dir();
        let mut database = SessionDatabase::open(&state).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        session.push_message(TestMessage(ARTIFACT_NAME.into()));
        database.save(&session, None).unwrap();
        database
            .connection
            .execute(
                "UPDATE main_history_items SET payload = ?1",
                params![vec![0_u8; size_of::<u64>()]],
            )
            .unwrap();

        let error = database
            .load::<TestMessage, Value, Value>(session.id)
            .unwrap_err();

        assert!(
            matches!(
                &error,
                SessionError::CorruptDatabaseValue { reason, .. }
                    if reason.contains(PAYLOAD_DECOMPRESSION_FAILED)
            ),
            "{error}"
        );
    }

    const BLOCKED_NAMES_SESSIONS: &str =
        "a blocked migration must name the open sessions holding it up";

    #[test]
    fn a_blocked_migration_names_the_open_sessions_by_id_and_title() {
        let (_temp, state) = state_dir();
        let title = "x".repeat(MAX_HOLDER_TITLE_BYTES * 2);
        let id = {
            let mut database = SessionDatabase::open(&state).unwrap();
            let mut session = TestSession::new(MODEL, CWD);
            session.title = title.clone();
            database.save(&session, None).unwrap();
            session.id
        };
        // Only the version decides whether an open is a cutover, so winding it
        // back is enough to make the next one try to migrate.
        let stale = Connection::open(state.path().join(SESSIONS_DB_FILE)).unwrap();
        stale
            .pragma_update(None, "user_version", COMPRESSION_PREVIOUS_SCHEMA)
            .unwrap();
        drop(stale);
        // The lease is what an open session holds; the shared migration lock it
        // takes alongside is what the upgrade then cannot convert.
        let _lease = SessionLease::acquire(&state, id).unwrap();

        let error = SessionDatabase::open_state(&state).err().unwrap();

        let SessionError::MigrationBlocked {
            found,
            supported,
            holders,
        } = &error
        else {
            panic!("{BLOCKED_NAMES_SESSIONS}: {error}");
        };
        assert_eq!(*found, COMPRESSION_PREVIOUS_SCHEMA);
        assert_eq!(*supported, SCHEMA_VERSION);
        assert!(holders.contains(&id.to_string()), "{BLOCKED_NAMES_SESSIONS}");
        assert!(
            holders.contains(&format!("{}{TITLE_ELLIPSIS}", &title[..MAX_HOLDER_TITLE_BYTES])),
            "{BLOCKED_NAMES_SESSIONS}: {holders}"
        );
    }

    fn schema_objects(database: &SessionDatabase) -> Vec<(String, String, Option<String>)> {
        let mut statement = database
            .connection
            .prepare(
                "SELECT type, name, sql FROM sqlite_schema \
                 WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                let kind = row.get(0)?;
                let name: String = row.get(1)?;
                let sql = if name == "sessions" {
                    Some("<current sessions schema>".to_owned())
                } else {
                    row.get(2)?
                };
                Ok((kind, name, sql))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn migrating_a_v4_database_yields_the_fresh_schema() {
        let (_migrated_temp, migrated_dir) = state_dir();
        let (_fresh_temp, fresh_dir) = state_dir();
        seed_v4_database(&migrated_dir);

        let migrated = SessionDatabase::open(&migrated_dir).unwrap();
        let fresh = SessionDatabase::open(&fresh_dir).unwrap();

        assert_eq!(migrated.stats().unwrap().schema_version, SCHEMA_VERSION);
        assert_eq!(
            schema_objects(&migrated),
            schema_objects(&fresh),
            "{MIGRATED_MATCHES_FRESH}"
        );
        assert!(
            schema_objects(&fresh)
                .iter()
                .any(|(_, name, _)| name == WORKFLOW_RUNS_TABLE)
        );
    }

    #[test]
    fn migrating_a_v5_database_adds_the_workflow_timeline() {
        let (_migrated_temp, migrated_dir) = state_dir();
        let (_fresh_temp, fresh_dir) = state_dir();
        seed_v5_database(&migrated_dir);

        let migrated = SessionDatabase::open(&migrated_dir).unwrap();
        let fresh = SessionDatabase::open(&fresh_dir).unwrap();

        assert_eq!(migrated.stats().unwrap().schema_version, SCHEMA_VERSION);
        assert_eq!(
            schema_objects(&migrated),
            schema_objects(&fresh),
            "{MIGRATED_MATCHES_FRESH}"
        );
        assert!(
            schema_objects(&fresh)
                .iter()
                .any(|(_, name, _)| name == WORKFLOW_EVENTS_TABLE_NAME)
        );
        assert!(
            schema_objects(&migrated)
                .iter()
                .any(|(_, name, _)| name == TOOL_LEDGER_TABLE_NAME),
            "{MIGRATED_MATCHES_FRESH}"
        );
    }

    #[test]
    fn migrating_leaves_the_original_readable_at_its_own_version() {
        let (_temp, state_dir) = state_dir();
        let id = seed_v1_database(&state_dir);

        SessionDatabase::open(&state_dir).unwrap();

        let backup = state_dir.path().join(format!("{SESSIONS_DB_FILE}.v1.bak"));
        let connection = Connection::open(&backup).unwrap();
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let title: String = connection
            .query_row(
                "SELECT title FROM sessions WHERE id = ?1",
                params![id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(version, 1, "{BACKUP_KEEPS_ORIGIN}");
        assert_eq!(title, "kept", "{BACKUP_KEEPS_ORIGIN}");
    }

    #[test]
    fn a_failed_migration_step_leaves_the_version_it_started_from() {
        let (_temp, state_dir) = state_dir();
        seed_v1_database(&state_dir);
        let path = state_dir.path().join(SESSIONS_DB_FILE);
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(USAGE_LEDGER_TABLE).unwrap();
        drop(connection);

        let error = SessionDatabase::open(&state_dir).err().unwrap();

        let connection = Connection::open(&path).unwrap();
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert!(matches!(error, SessionError::Sqlite(_)), "{error}");
        assert_eq!(version, 1, "{PARTIAL_MIGRATION}");
    }

    #[test]
    fn a_foreign_database_is_not_migrated() {
        let (_temp, state_dir) = state_dir();
        seed_v1_database(&state_dir);
        let path = state_dir.path().join(SESSIONS_DB_FILE);
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "application_id", APPLICATION_ID + 1)
            .unwrap();
        drop(connection);

        let error = SessionDatabase::open(&state_dir).err().unwrap();

        assert!(matches!(
            error,
            SessionError::CorruptDatabaseValue {
                field: "PRAGMA application_id",
                ..
            }
        ));
    }

    #[test_case(OLDER_SCHEMA_VERSION; "older")]
    #[test_case(NEWER_SCHEMA_VERSION; "newer")]
    fn noncurrent_schema_requires_a_reset(version: i64) {
        let (_temp, state_dir) = state_dir();
        let path = state_dir.path().join(SESSIONS_DB_FILE);
        create_owner_only(&path).unwrap();
        let connection = Connection::open(path).unwrap();
        connection
            .pragma_update(None, "user_version", version)
            .unwrap();
        drop(connection);

        let error = SessionDatabase::open(&state_dir).err().unwrap();

        assert!(matches!(
            error,
            SessionError::UnsupportedSchemaVersion {
                found,
                supported: SCHEMA_VERSION
            } if found == version
        ));
    }

    #[test]
    fn mark_opened_records_activity_without_touching_versions() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = session_with_outputs();
        let cursor = database.save(&session, None).unwrap();
        let before = database.session_facts(None).unwrap();
        assert!(before[0].last_opened_at.is_none());

        database.mark_opened(session.id).unwrap();

        let after = database.session_facts(None).unwrap();
        assert!(after[0].last_opened_at.is_some());
        assert_eq!(after[0].updated_at, before[0].updated_at);
        assert_eq!(
            database.write_version(session.id).unwrap(),
            Some(cursor.write_version()),
            "{VERSION_UNCHANGED}"
        );
        session.set_title("still writable".into());
        database.save(&session, Some(&cursor)).unwrap();
    }

    #[test]
    fn trim_keeps_transcript_and_small_outputs_and_removes_artifacts() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let session = session_with_outputs();
        let cursor = database.save(&session, None).unwrap();
        let paths = seed_artifacts(&state_dir, session.id);
        let lease = SessionLease::acquire(&state_dir, session.id).unwrap();

        let report = database.trim(&lease).unwrap();

        assert_eq!(report.tool_output_rows, 1, "{TRIM_DROPS_LARGE}");
        assert!(report.tool_output_row_bytes > 0);
        assert!(report.artifact_bytes > 0);
        assert!(
            paths.iter().all(|path| !path.exists()),
            "{ARTIFACTS_REMOVED}"
        );
        let loaded = database
            .load::<TestMessage, Value, Value>(session.id)
            .unwrap();
        assert_eq!(loaded.messages(), session.messages());
        assert!(
            loaded.tool_outputs().contains_key(SMALL_OUTPUT_ID),
            "{TRIM_KEEPS_SMALL}"
        );
        assert!(
            !loaded.tool_outputs().contains_key(LARGE_OUTPUT_ID),
            "{TRIM_DROPS_LARGE}"
        );
        let facts = database.session_facts(None).unwrap();
        assert!(facts[0].is_trimmed());
        assert_eq!(database.stats().unwrap().trimmed_count, 1);
        assert_ne!(
            database.write_version(session.id).unwrap(),
            Some(cursor.write_version())
        );
        let jobs: i64 = database
            .connection
            .query_row("SELECT count(*) FROM cleanup_jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(jobs, 0);
    }

    #[test]
    fn trim_rejects_a_lease_for_another_session() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let session = session_with_outputs();
        database.save(&session, None).unwrap();
        let other = TestSession::new(MODEL, CWD);
        let lease = SessionLease::acquire(&state_dir, other.id).unwrap();

        assert!(matches!(
            database.trim(&lease),
            Err(SessionError::Storage(StorageError::NotFound(_)))
        ));
        assert_eq!(
            database
                .load::<TestMessage, Value, Value>(session.id)
                .unwrap()
                .tool_outputs()
                .len(),
            2
        );
    }

    #[test]
    fn trimmed_session_written_again_becomes_a_candidate_and_drops_stale_jobs() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = session_with_outputs();
        database.save(&session, None).unwrap();
        let lease = SessionLease::acquire(&state_dir, session.id).unwrap();
        database.trim(&lease).unwrap();
        drop(lease);
        let mut reopened = database
            .load::<TestMessage, Value, Value>(session.id)
            .unwrap();
        reopened.push_message(TestMessage("later".into()));
        reopened.updated_at += 1;
        database.save(&reopened, None).unwrap();
        session.updated_at = reopened.updated_at;
        let transaction = database
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        enqueue_cleanup_jobs(&transaction, session.id).unwrap();
        transaction.commit().unwrap();
        let paths = seed_artifacts(&state_dir, session.id);

        let completed = database.process_cleanup_jobs().unwrap();

        assert_eq!(completed, 0);
        assert!(paths.iter().all(|path| path.exists()));
        assert!(!database.session_facts(None).unwrap()[0].is_trimmed());
        let jobs: i64 = database
            .connection
            .query_row("SELECT count(*) FROM cleanup_jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(jobs, 0);
    }

    #[test]
    fn trimmed_session_cleanup_waits_while_the_session_is_open() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let session = session_with_outputs();
        database.save(&session, None).unwrap();
        let lease = SessionLease::acquire(&state_dir, session.id).unwrap();
        let transaction = database
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        transaction
            .execute(
                "UPDATE sessions SET trimmed_at = updated_at WHERE id = ?1",
                params![session.id.as_bytes().as_slice()],
            )
            .unwrap();
        enqueue_cleanup_jobs(&transaction, session.id).unwrap();
        transaction.commit().unwrap();
        let paths = seed_artifacts(&state_dir, session.id);

        let completed = database.process_cleanup_jobs().unwrap();
        assert_eq!(completed, 0);
        assert!(paths.iter().all(|path| path.exists()));
        let deferred: i64 = database
            .connection
            .query_row(
                "SELECT count(*) FROM cleanup_jobs WHERE last_error = ?1",
                params![SESSION_OPEN_ELSEWHERE],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(deferred, ARTIFACT_CLEANUP_KINDS.len() as i64);
        drop(lease);
        database
            .connection
            .execute("UPDATE cleanup_jobs SET next_attempt_ms = 0", [])
            .unwrap();

        let completed = database.process_cleanup_jobs().unwrap();

        assert_eq!(completed, ARTIFACT_CLEANUP_KINDS.len() as u64);
        assert!(
            paths.iter().all(|path| !path.exists()),
            "{ARTIFACTS_REMOVED}"
        );
    }

    #[test]
    fn pin_marks_facts_and_rejects_unknown_sessions() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let session = session_with_outputs();
        database.save(&session, None).unwrap();

        database.set_pinned(session.id, true).unwrap();
        assert!(database.session_facts(None).unwrap()[0].pinned);
        assert_eq!(database.stats().unwrap().pinned_count, 1);
        database.set_pinned(session.id, false).unwrap();
        assert!(!database.session_facts(None).unwrap()[0].pinned);
        assert!(matches!(
            database.set_pinned(CaudraId::generate(), true),
            Err(SessionError::Storage(StorageError::NotFound(_)))
        ));
    }

    #[test]
    fn session_facts_filter_by_directory_and_report_pending_revert() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut reverting = TestSession::new(MODEL, CWD);
        reverting.meta.pending_revert = Some(super::super::PendingConversationRevert {
            original_head: None,
            target_head: None,
            original_workspace_head: None,
            workspace_head: None,
            file_status: None,
            restore_operation: None,
        });
        database.save(&reverting, None).unwrap();
        database
            .save(&TestSession::new(MODEL, "/elsewhere"), None)
            .unwrap();

        let all = database.session_facts(None).unwrap();
        let here = database.session_facts(Some(CWD)).unwrap();

        assert_eq!(all.len(), 2);
        assert_eq!(here.len(), 1);
        assert_eq!(here[0].id, reverting.id);
        assert!(here[0].pending_revert);
    }
}
