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
use std::ffi::OsStr;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use rusqlite::backup::Backup;
use rusqlite::limits::Limit;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tempfile::NamedTempFile;
use tracing::warn;

use super::lease::SessionLease;
use super::{
    SESSION_VERSION, Session, SessionError, SessionSummary, StoredSubagent, StoredSubagentOutcome,
    StoredSubagentTaskSpec, StoredTokenUsage, next_epoch,
};
use crate::id::CaudraId;
use crate::retention::SessionFacts;
use crate::tool_outputs::{TOOL_OUTPUT_DIR, delete_session_outputs};
use crate::{
    StateDir, StorageError, atomic_write_permissions, exclusive_state_lock, lock_session_artifacts,
    shared_existing_state_lock, shared_state_lock,
};

pub const SESSIONS_DB_FILE: &str = "sessions.sqlite3";
pub const SESSIONS_DB_LOCK_FILE: &str = "sessions.sqlite3.lock";
const SESSIONS_DB_MIGRATION_FILE: &str = "sessions.sqlite3.migrating";
const SESSIONS_DB_CUTOVER_PENDING_FILE: &str = "sessions.sqlite3.cutover-pending";

const SCHEMA_VERSION: i64 = 1;
const APPLICATION_ID: i64 = i32::from_be_bytes(*b"CAUD") as i64;
const INCREMENTAL_AUTO_VACUUM: i64 = 2;
const PAGE_SIZE: i64 = 4096;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const WAL_AUTO_CHECKPOINT_PAGES: i64 = 1000;
const JOURNAL_SIZE_LIMIT_BYTES: i64 = 64 * 1024 * 1024;
const MAX_PAYLOAD_BYTES: usize = 32 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_PATH_BYTES: usize = 32 * 1024;
const MAX_JSON_DEPTH: usize = 64;
const MAX_IMAGES_PER_ITEM: usize = 16;
const MAX_DECODED_IMAGE_BYTES: usize = 24 * 1024 * 1024;
const MAX_SQLITE_VALUE_BYTES: i32 = 40 * 1024 * 1024;
const MAX_EAGER_LOAD_BYTES: usize = 512 * 1024 * 1024;
const OWNER_FILE_MODE: u32 = 0o600;
pub(super) const SESSION_SNAPSHOT_DIR: &str = "session-snapshots";
const CLEANUP_RETRY_DELAY_MS: i64 = 60_000;
const PENDING_ARCHIVE_ORPHAN_GRACE: Duration = Duration::from_secs(60 * 60);
const ARTIFACT_CLEANUP_KINDS: [&str; 3] = ["tool_output", "archive", "snapshot"];
const STATE_SCOPE_GLOBAL: &str = "global";
/// Rich tool output rows at or below this size survive a trim so old
/// transcripts keep their todo panels and other small structured records.
const TRIM_KEEP_OUTPUT_BYTES: i64 = 4096;
const SESSION_OPEN_ELSEWHERE: &str = "session is open in another Caudra instance";
const UNKNOWN_CLEANUP_KIND: &str = "unknown cleanup job kind";

// This schema stores canonical state only. Adding any second durable
// representation requires bounded retention and explicit recovery semantics.
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
    last_opened_at       INTEGER,
    pinned               INTEGER NOT NULL DEFAULT 0 CHECK(pinned IN (0, 1)),
    trimmed_at           INTEGER
) STRICT;

CREATE INDEX sessions_cwd_updated
    ON sessions(cwd, updated_at DESC, id DESC);

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
    payload     TEXT NOT NULL CHECK(json_valid(payload)),
    byte_count  INTEGER NOT NULL CHECK(byte_count = length(CAST(payload AS BLOB))),
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
    session_id     BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    model          TEXT NOT NULL,
    input_tokens   INTEGER NOT NULL,
    output_tokens  INTEGER NOT NULL,
    cache_creation INTEGER NOT NULL,
    cache_read     INTEGER NOT NULL,
    cost           REAL,
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

/// Holds source and target exclusion while XDG migration moves the remaining
/// state. Dropping before `finish` removes the staged target and preserves the
/// source; `finish` atomically publishes a durable retirement marker.
pub struct SessionMigration {
    source: StateDir,
    target: StateDir,
    _source_lock: File,
    _target_lock: File,
    temporary_path: PathBuf,
    pending_path: PathBuf,
    copied_database: bool,
    finished: bool,
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
}

impl SessionDatabase {
    pub fn open(state_dir: &StateDir) -> Result<Self, SessionError> {
        reject_retired_state_dir(state_dir)?;
        let migration_lock = shared_state_lock(
            &state_dir.path().join(SESSIONS_DB_LOCK_FILE),
            OWNER_FILE_MODE,
        )?;
        reject_retired_state_dir(state_dir)?;
        let cutover_pending = state_dir.path().join(SESSIONS_DB_CUTOVER_PENDING_FILE);
        if cutover_pending.exists() && state_dir.path().join(SESSIONS_DB_FILE).exists() {
            let _ = fs::remove_file(&cutover_pending);
            crate::sync_parent_dir(&cutover_pending);
        }
        let connection = open_writable_connection(state_dir)?;
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
        reject_retired_state_dir(state_dir)?;
        let migration_lock = shared_state_lock(
            &state_dir.path().join(SESSIONS_DB_LOCK_FILE),
            OWNER_FILE_MODE,
        )?;
        reject_retired_state_dir(state_dir)?;
        let connection = open_writable_connection(state_dir)?;
        Ok(Self {
            connection,
            state_dir: state_dir.clone(),
            _migration_lock: migration_lock,
        })
    }

    pub fn migrate(source: &StateDir, target: &StateDir) -> Result<SessionMigration, SessionError> {
        reject_retired_state_dir(source)?;
        // Every repository holds its shared lock for the connection lifetime.
        // Exclusive cutover therefore drains writers and blocks new ones until
        // target verification and durable source retirement are complete.
        let source_lock =
            exclusive_state_lock(&source.path().join(SESSIONS_DB_LOCK_FILE), OWNER_FILE_MODE)?;
        let target_lock =
            exclusive_state_lock(&target.path().join(SESSIONS_DB_LOCK_FILE), OWNER_FILE_MODE)?;
        // The first check can precede a long wait behind another migrator.
        // Recheck under both exclusive locks so a completed cutover cannot be
        // republished from its retained rollback source.
        reject_retired_state_dir(source)?;
        let source_path = source.path().join(SESSIONS_DB_FILE);
        let target_path = target.path().join(SESSIONS_DB_FILE);
        let temporary_path = target.path().join(SESSIONS_DB_MIGRATION_FILE);
        let pending_path = target.path().join(SESSIONS_DB_CUTOVER_PENDING_FILE);
        if pending_path.exists() {
            remove_database_files(&target_path)?;
            fs::remove_file(&pending_path).map_err(StorageError::from)?;
        }
        remove_database_files(&temporary_path)?;
        // A target database can contain canonical rows or tombstones. Refuse
        // the whole cutover rather than silently replacing either generation.
        if target_path.exists() {
            return Err(StorageError::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "session database already exists at {}",
                    target_path.display()
                ),
            ))
            .into());
        }
        if !source_path.exists() {
            return Ok(SessionMigration {
                source: source.clone(),
                target: target.clone(),
                _source_lock: source_lock,
                _target_lock: target_lock,
                temporary_path,
                pending_path,
                copied_database: false,
                finished: false,
            });
        }
        fs::create_dir_all(target.path()).map_err(StorageError::from)?;
        // Copying only the main file can omit committed WAL frames. The online
        // backup API reads one consistent logical database instead.
        let backup_result = (|| -> Result<(), SessionError> {
            let source_connection = open_migration_source_connection(source)?;
            create_owner_only(&temporary_path)?;
            let mut destination = Connection::open_with_flags(
                &temporary_path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?;
            let backup = Backup::new(&source_connection, &mut destination)?;
            backup.run_to_completion(256, Duration::from_millis(10), None)?;
            drop(backup);
            drop(destination);
            drop(source_connection);
            let migrated = Connection::open_with_flags(
                &temporary_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?;
            quick_check_on(&migrated)
        })();
        if let Err(error) = backup_result {
            let _ = remove_database_files(&temporary_path);
            return Err(error);
        }
        crate::sync_parent_dir_durable(&temporary_path)?;
        Ok(SessionMigration {
            source: source.clone(),
            target: target.clone(),
            _source_lock: source_lock,
            _target_lock: target_lock,
            temporary_path,
            pending_path,
            copied_database: true,
            finished: false,
        })
    }

    pub fn path(&self) -> PathBuf {
        self.state_dir.path().join(SESSIONS_DB_FILE)
    }

    pub fn open_read_only(state_dir: &StateDir) -> Result<Self, SessionError> {
        let path = state_dir.path().join(SESSIONS_DB_FILE);
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
        let wal = database_sidecar(&path, "-wal");
        let shm = database_sidecar(&path, "-shm");
        if wal.exists() && !shm.exists() {
            // SQLite must create shared memory to inspect this WAL. Refusing is
            // the only way a diagnostic open can remain genuinely read-only.
            return Err(StorageError::Io(io::Error::new(
                io::ErrorKind::WouldBlock,
                "read-only inspection requires an existing SQLite SHM sidecar",
            ))
            .into());
        }
        // Inspection must not create files, change journal mode, or initialize
        // a schema. Normal writable initialization creates this lock file.
        let migration_lock =
            shared_existing_state_lock(&state_dir.path().join(SESSIONS_DB_LOCK_FILE))?;
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_SQLITE_VALUE_BYTES)?;
        verify_current_schema(&connection)?;
        connection.execute_batch("PRAGMA query_only = ON; PRAGMA trusted_schema = OFF;")?;
        Ok(Self {
            connection,
            state_dir: state_dir.clone(),
            _migration_lock: migration_lock,
        })
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

    /// Scalar facts for every session, or for one working directory. Payload
    /// tables are never joined to plan retention.
    pub fn session_facts(&self, cwd: Option<&str>) -> Result<Vec<SessionFacts>, SessionError> {
        let mut statement = self.connection.prepare(
            "SELECT id, title, cwd, created_at, updated_at, last_opened_at, pinned, trimmed_at,\
                    logical_bytes, json_extract(metadata, '$.pending_revert') IS NOT NULL \
             FROM sessions WHERE ?1 IS NULL OR cwd = ?1 \
             ORDER BY max(updated_at, coalesce(last_opened_at, 0)) DESC, id DESC",
        )?;
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
            artifact_bytes,
        })
    }

    /// Bytes under every external artifact directory of one session.
    pub fn artifact_bytes(&self, id: CaudraId) -> u64 {
        let state_path = self.state_dir.path();
        let name = id.to_string();
        [
            state_path.join(TOOL_OUTPUT_DIR).join(&name),
            state_path.join(SESSION_SNAPSHOT_DIR).join(&name),
            state_path
                .join(super::SESSIONS_DIR)
                .join(super::ARCHIVE_DIR)
                .join(&name),
        ]
        .iter()
        .map(|path| directory_bytes(path))
        .sum()
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
        let mut freed = 0;
        while rows.next()?.is_some() {
            freed += 1;
        }
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
        let serialized = SerializedSession::new(session)?;
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
            "UPDATE sessions SET write_version = ?2 WHERE id = ?1",
            params![session.id.as_bytes().as_slice(), write_version],
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
        let actual = transaction
            .query_row(
                "SELECT write_version FROM sessions WHERE id = ?1",
                params![id.as_bytes().as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
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
                let payload: String = row.get(0)?;
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
        let serialized = SerializedSession::new(session)?;
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
        let root = SerializedRoot::new(session)?;
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
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
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

impl SessionMigration {
    pub fn copied_database(&self) -> bool {
        self.copied_database
    }

    pub fn finish(mut self) -> Result<bool, SessionError> {
        let target_path = self.target.path().join(SESSIONS_DB_FILE);
        if self.copied_database {
            atomic_write_permissions(&self.pending_path, b"pending", OWNER_FILE_MODE)?;
            crate::durable_rename(&self.temporary_path, &target_path)
                .map_err(StorageError::from)?;
            crate::sync_parent_dir_durable(&target_path)?;
        }
        // The marker is the durable cutover point. Cached legacy StateDirs
        // reject future opens once it exists, even after these locks are gone.
        if let Err(error) = write_cutover_marker(&self.source, &self.target) {
            // Once marker publication is visible, dropping the migration must
            // not remove its target and leave the process pointed at nothing.
            self.finished = self
                .source
                .path()
                .join(crate::paths::XDG_MIGRATED_MARKER)
                .exists();
            return Err(error);
        }
        self.finished = true;
        if self.copied_database {
            // The retired source remains intact as rollback data. The durable
            // marker prevents it from becoming canonical again.
            let _ = fs::remove_file(&self.pending_path);
            crate::sync_parent_dir(&self.pending_path);
        }
        Ok(self.copied_database)
    }
}

impl Drop for SessionMigration {
    fn drop(&mut self) {
        if self.copied_database && !self.finished {
            let _ = remove_database_files(&self.temporary_path);
            if self.pending_path.exists() {
                let target_path = self.target.path().join(SESSIONS_DB_FILE);
                let _ = remove_database_files(&target_path);
                let _ = fs::remove_file(&self.pending_path);
            }
            crate::sync_parent_dir(&self.temporary_path);
        }
    }
}

impl Drop for PreparedArchive {
    fn drop(&mut self) {
        if self.cleanup_on_drop && fs::remove_file(&self.pending_path).is_ok() {
            crate::sync_parent_dir(&self.pending_path);
        }
    }
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
    let meta = deserialize_json(&root.metadata, "session metadata")?;
    let write_version = Arc::new(std::sync::atomic::AtomicI64::new(root.write_version));
    let session = Session {
        version: root.format_version,
        id,
        title: root.title,
        cwd: root.cwd,
        model: root.model,
        messages: Arc::new(messages),
        token_usage,
        tool_outputs,
        subagent_messages,
        subagent_task_specs,
        subagents,
        usage_by_model,
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
        root.token_usage.len() + root.metadata.len(),
        task_spec_bytes,
    );
    Ok((session, cursor))
}

fn root_on(connection: &Connection, id: CaudraId) -> Result<RootRow, SessionError> {
    connection
        .query_row(
            "SELECT format_version, title, cwd, model, created_at, updated_at, \
                        write_version, logical_bytes, history_item_count, tool_output_count,\
                        subagent_item_count, token_usage, metadata \
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
    fn new<M, U, T>(session: &Session<M, U, T>) -> Result<Self, SessionError>
    where
        U: Serialize,
    {
        Ok(Self {
            token_usage: serialize_json(&session.token_usage, "token usage", MAX_PAYLOAD_BYTES)?,
            metadata: serialize_json(&session.meta, "session metadata", MAX_METADATA_BYTES)?,
        })
    }

    fn bytes(&self) -> usize {
        self.token_usage.len() + self.metadata.len()
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
        Ok(metadata) => {
            #[cfg(unix)]
            if metadata.uid() != rustix::process::geteuid().as_raw()
                || metadata.permissions().mode() & 0o077 != 0
            {
                return Err(StorageError::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "session database {} must be owned by the current user and inaccessible to other users",
                        path.display()
                    ),
                ))
                .into());
            }
            return Ok(());
        }
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

fn reject_retired_state_dir(state_dir: &StateDir) -> Result<(), SessionError> {
    let marker = state_dir.path().join(crate::paths::XDG_MIGRATED_MARKER);
    if !marker.is_file() {
        return Ok(());
    }
    let target = fs::read_to_string(&marker).unwrap_or_else(|_| "the XDG state directory".into());
    Err(StorageError::Io(io::Error::other(format!(
        "state directory {} was retired; use {}",
        state_dir.path().display(),
        target.trim()
    )))
    .into())
}

fn write_cutover_marker(source: &StateDir, target: &StateDir) -> Result<(), SessionError> {
    let marker = source.path().join(crate::paths::XDG_MIGRATED_MARKER);
    atomic_write_permissions(
        &marker,
        target.path().as_os_str().as_encoded_bytes(),
        OWNER_FILE_MODE,
    )?;
    crate::sync_parent_dir_durable(&marker)?;
    Ok(())
}

fn open_writable_connection(state_dir: &StateDir) -> Result<Connection, SessionError> {
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
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_SQLITE_VALUE_BYTES)?;
    initialize(&mut connection, state_dir)?;
    configure(&connection)?;
    Ok(connection)
}

fn open_migration_source_connection(state_dir: &StateDir) -> Result<Connection, SessionError> {
    let path = state_dir.path().join(SESSIONS_DB_FILE);
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_SQLITE_VALUE_BYTES)?;
    verify_current_schema(&connection)?;
    Ok(connection)
}

fn verify_current_schema(connection: &Connection) -> Result<(), SessionError> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(SessionError::UnsupportedSchemaVersion {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
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

fn remove_database_files(path: &Path) -> Result<(), SessionError> {
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    let mut shm = path.as_os_str().to_os_string();
    shm.push("-shm");
    for path in [path.to_path_buf(), wal.into(), shm.into()] {
        if let Err(error) = fs::remove_file(&path) {
            if error.kind() == io::ErrorKind::NotFound {
                continue;
            }
            return Err(StorageError::from(error).into());
        }
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
    // size limit trims WAL retention after checkpoints; it is not an active-WAL
    // cap and must never justify deleting live sidecars.
    connection.execute_batch(&format!(
        "PRAGMA foreign_keys = ON;\
         PRAGMA synchronous = FULL;\
         PRAGMA wal_autocheckpoint = {WAL_AUTO_CHECKPOINT_PAGES};\
         PRAGMA journal_size_limit = {JOURNAL_SIZE_LIMIT_BYTES};\
         PRAGMA trusted_schema = OFF;"
    ))?;
    Ok(())
}

fn initialize(connection: &mut Connection, state_dir: &StateDir) -> Result<(), SessionError> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 0 && version != SCHEMA_VERSION {
        return Err(SessionError::UnsupportedSchemaVersion {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version == SCHEMA_VERSION {
        return configure_current_schema(connection);
    }

    // Caudra initializers serialize on the existing artifact lock, while the
    // SQLite exclusive mode below also excludes non-cooperating connections.
    let _initialization_lock = lock_session_artifacts(state_dir)?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == SCHEMA_VERSION {
        return configure_current_schema(connection);
    }
    if version != 0 {
        return Err(SessionError::UnsupportedSchemaVersion {
            found: version,
            supported: SCHEMA_VERSION,
        });
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
        transaction.execute_batch(SCHEMA)?;
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
             token_usage, metadata\
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9, ?10, ?11, ?12, ?13)",
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
    let changed = transaction.execute(
        "UPDATE sessions SET \
             format_version = ?1, title = ?2, cwd = ?3, model = ?4,\
             created_at = ?5, updated_at = ?6, write_version = write_version + 1,\
             logical_bytes = ?7, history_item_count = ?8, tool_output_count = ?9,\
             subagent_item_count = ?10, token_usage = ?11, metadata = ?12 \
         WHERE id = ?13 AND write_version = ?14",
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
            payload,
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
            payload,
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
            payload,
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
                 session_id, model, input_tokens, output_tokens, cache_creation, cache_read, cost\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session.id.as_bytes().as_slice(),
                model,
                i64::from(usage.input),
                i64::from(usage.output),
                i64::from(usage.cache_creation),
                i64::from(usage.cache_read),
                usage.cost,
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
        let payload: String = row.get(0)?;
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
        let payload: String = row.get(1)?;
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
        let payload: String = row.get(0)?;
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
        "SELECT model, input_tokens, output_tokens, cache_creation, cache_read, cost \
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
            },
        );
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

fn to_i64(value: impl TryInto<i64>, field: &'static str) -> Result<i64, SessionError> {
    value
        .try_into()
        .map_err(|_| SessionError::CorruptDatabaseValue {
            field,
            reason: "value exceeds SQLite integer range".into(),
        })
}

fn from_i64(value: i64, field: &'static str) -> Result<u64, SessionError> {
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
    use std::fs;
    use std::sync::Barrier;

    use super::*;
    use crate::sessions::{Session, StoredSubagentOutcome, TitleSource};
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use test_case::test_case;

    const CWD: &str = "/project";
    const MODEL: &str = "test/model";
    const ARTIFACT_NAME: &str = "artifact";
    const LARGE_OUTPUT_ID: &str = "large";
    const FOREIGN_APPLICATION_ID: i64 = 1;
    const INITIALIZER_COUNT: usize = 2;
    const SMALL_OUTPUT_ID: &str = "small";
    const TOMBSTONES_TABLE: &str = "session_tombstones";
    const OLDER_SCHEMA_VERSION: i64 = -1;
    const NEWER_SCHEMA_VERSION: i64 = SCHEMA_VERSION + 1;
    const TRIM_KEEPS_SMALL: &str = "trim must keep rich outputs at or below the threshold";
    const TRIM_DROPS_LARGE: &str = "trim must drop rich outputs above the threshold";
    const ARTIFACTS_REMOVED: &str = "trim must remove every artifact directory";
    const VERSION_UNCHANGED: &str = "mark_opened must not bump write_version";

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
        let mut session = TestSession::new(MODEL, CWD);
        session.push_message(TestMessage("legacy".into()));
        session.save_to(&sessions_dir).unwrap();
        let source = sessions_dir.join(format!("{}.jsonl", session.id));

        assert!(source.exists());
        assert!(matches!(
            TestSession::load(session.id, &state_dir),
            Err(SessionError::Storage(StorageError::NotFound(_)))
        ));
        assert!(TestSession::list(CWD, &state_dir).unwrap().is_empty());
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
    fn database_migration_backs_up_wal_state() {
        let temp = TempDir::new().unwrap();
        let source = StateDir::from_path(temp.path().join("source"));
        let target = StateDir::from_path(temp.path().join("target"));
        let mut database = SessionDatabase::open(&source).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        session.push_message(TestMessage("in wal".into()));
        database.save(&session, None).unwrap();
        drop(database);

        assert!(
            SessionDatabase::migrate(&source, &target)
                .unwrap()
                .finish()
                .unwrap()
        );

        assert!(source.path().join(SESSIONS_DB_FILE).exists());
        assert!(
            source
                .path()
                .join(crate::paths::XDG_MIGRATED_MARKER)
                .exists()
        );
        assert!(SessionDatabase::open(&source).is_err());
        let migrated = SessionDatabase::open(&target).unwrap();
        assert_eq!(
            migrated
                .load::<TestMessage, Value, Value>(session.id)
                .unwrap()
                .messages(),
            session.messages()
        );
    }

    #[test]
    fn database_migration_never_overwrites_existing_target() {
        let temp = TempDir::new().unwrap();
        let source = StateDir::from_path(temp.path().join("source"));
        let target = StateDir::from_path(temp.path().join("target"));
        let source_database = SessionDatabase::open(&source).unwrap();
        let target_database = SessionDatabase::open(&target).unwrap();
        drop((source_database, target_database));

        let Err(error) = SessionDatabase::migrate(&source, &target) else {
            panic!("migration unexpectedly overwrote the target");
        };

        assert!(matches!(error, SessionError::Storage(StorageError::Io(_))));
        assert!(source.path().join(SESSIONS_DB_FILE).exists());
        assert!(target.path().join(SESSIONS_DB_FILE).exists());
    }

    #[test]
    fn database_migration_rejects_an_incompatible_v1_source() {
        let temp = TempDir::new().unwrap();
        let source = StateDir::from_path(temp.path().join("source"));
        let target = StateDir::from_path(temp.path().join("target"));
        fs::create_dir_all(source.path()).unwrap();
        let source_path = source.path().join(SESSIONS_DB_FILE);
        create_owner_only(&source_path).unwrap();
        let connection = Connection::open(source_path).unwrap();
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION)
            .unwrap();
        drop(connection);

        let error = SessionDatabase::migrate(&source, &target).err().unwrap();

        assert!(matches!(
            error,
            SessionError::CorruptDatabaseValue {
                field: "PRAGMA application_id",
                ..
            }
        ));
        assert!(!target.path().join(SESSIONS_DB_FILE).exists());
        assert!(
            !source
                .path()
                .join(crate::paths::XDG_MIGRATED_MARKER)
                .exists()
        );
    }

    #[test]
    fn database_absent_migration_rejects_existing_target_database() {
        let temp = TempDir::new().unwrap();
        let source = StateDir::from_path(temp.path().join("source"));
        let target = StateDir::from_path(temp.path().join("target"));
        fs::create_dir_all(source.path().join(super::super::SESSIONS_DIR)).unwrap();
        drop(SessionDatabase::open(&target).unwrap());

        assert!(SessionDatabase::migrate(&source, &target).is_err());
        assert!(
            !source
                .path()
                .join(crate::paths::XDG_MIGRATED_MARKER)
                .exists()
        );
        assert!(target.path().join(SESSIONS_DB_FILE).exists());
    }

    #[test]
    fn abandoned_database_migration_preserves_source_and_removes_partial_target() {
        let temp = TempDir::new().unwrap();
        let source = StateDir::from_path(temp.path().join("source"));
        let target = StateDir::from_path(temp.path().join("target"));
        drop(SessionDatabase::open(&source).unwrap());

        let migration = SessionDatabase::migrate(&source, &target).unwrap();
        assert!(source.path().join(SESSIONS_DB_FILE).exists());
        assert!(!target.path().join(SESSIONS_DB_FILE).exists());
        assert!(target.path().join(SESSIONS_DB_MIGRATION_FILE).exists());
        drop(migration);

        assert!(source.path().join(SESSIONS_DB_FILE).exists());
        assert!(!target.path().join(SESSIONS_DB_FILE).exists());
        assert!(!target.path().join(SESSIONS_DB_MIGRATION_FILE).exists());
        assert!(
            !source
                .path()
                .join(crate::paths::XDG_MIGRATED_MARKER)
                .exists()
        );
    }

    #[test]
    fn interrupted_cutover_is_rebuilt_from_the_retained_source() {
        let temp = TempDir::new().unwrap();
        let source = StateDir::from_path(temp.path().join("source"));
        let target = StateDir::from_path(temp.path().join("target"));
        let mut database = SessionDatabase::open(&source).unwrap();
        let session = TestSession::new(MODEL, CWD);
        database.save(&session, None).unwrap();
        drop(database);
        fs::create_dir_all(target.path()).unwrap();
        fs::write(target.path().join(SESSIONS_DB_FILE), b"partial target").unwrap();
        fs::write(
            target.path().join(SESSIONS_DB_CUTOVER_PENDING_FILE),
            b"pending",
        )
        .unwrap();

        SessionDatabase::migrate(&source, &target)
            .unwrap()
            .finish()
            .unwrap();

        assert!(
            SessionDatabase::open(&target)
                .unwrap()
                .load::<TestMessage, Value, Value>(session.id)
                .is_ok()
        );
    }

    #[test]
    fn cutover_marker_retires_legacy_state_without_a_database() {
        let temp = TempDir::new().unwrap();
        let source = StateDir::from_path(temp.path().join("source"));
        let target = StateDir::from_path(temp.path().join("target"));

        assert!(
            !SessionDatabase::migrate(&source, &target)
                .unwrap()
                .finish()
                .unwrap()
        );

        assert!(
            source
                .path()
                .join(crate::paths::XDG_MIGRATED_MARKER)
                .exists()
        );
        assert!(SessionDatabase::open(&source).is_err());
    }

    #[test]
    fn database_migration_waits_for_open_repositories() {
        let temp = TempDir::new().unwrap();
        let source = StateDir::from_path(temp.path().join("source"));
        let target = StateDir::from_path(temp.path().join("target"));
        let database = SessionDatabase::open(&source).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let migrate_source = source.clone();
        let migrate_target = target.clone();
        let handle = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx
                .send(
                    SessionDatabase::migrate(&migrate_source, &migrate_target)
                        .and_then(SessionMigration::finish),
                )
                .unwrap();
        });
        started_rx.recv().unwrap();

        assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(database);
        assert!(
            done_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
        );
        handle.join().unwrap();
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

    #[test]
    fn incremental_vacuum_frees_every_requested_page() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        let mut session = TestSession::new(MODEL, CWD);
        for index in 0..64 {
            session
                .insert_tool_output(format!("output-{index}"), json!({"text": "x".repeat(8192)}));
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

    #[test]
    fn read_only_open_never_creates_missing_wal_shared_memory() {
        let (_temp, state_dir) = state_dir();
        let mut database = SessionDatabase::open(&state_dir).unwrap();
        database.save(&TestSession::new(MODEL, CWD), None).unwrap();
        let path = database.path();
        let wal = database_sidecar(&path, "-wal");
        let shm = database_sidecar(&path, "-shm");
        assert!(wal.exists());
        fs::remove_file(&shm).unwrap();

        assert!(SessionDatabase::open_read_only(&state_dir).is_err());
        assert!(!shm.exists());
    }

    fn artifact_paths(state_dir: &StateDir, id: CaudraId) -> [PathBuf; 3] {
        let name = id.to_string();
        [
            state_dir.path().join(TOOL_OUTPUT_DIR).join(&name),
            state_dir
                .path()
                .join(super::super::SESSIONS_DIR)
                .join(super::super::ARCHIVE_DIR)
                .join(&name),
            state_dir.path().join(SESSION_SNAPSHOT_DIR).join(&name),
        ]
    }

    fn seed_artifacts(state_dir: &StateDir, id: CaudraId) -> [PathBuf; 3] {
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
    fn fresh_database_uses_the_single_current_schema() {
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
        assert_eq!(SCHEMA_VERSION, 1);
        let application_id: i64 = database
            .connection
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .unwrap();
        assert_eq!(application_id, APPLICATION_ID);
        assert!(table_exists(TOMBSTONES_TABLE));
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
        assert_eq!(deferred, 3);
        drop(lease);
        database
            .connection
            .execute("UPDATE cleanup_jobs SET next_attempt_ms = 0", [])
            .unwrap();

        let completed = database.process_cleanup_jobs().unwrap();

        assert_eq!(completed, 3);
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
