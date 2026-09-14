use std::fmt;
use std::fs::{self, OpenOptions};
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use caudra_workspace::OperationId;
use rusqlite::limits::Limit;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::StateDir;
use crate::workspace_binding::StoredWorkspaceBinding;

pub const REMOTE_OPERATION_JOURNAL_FILE: &str = "remote-operations.sqlite3";
const JOURNAL_DIR: &str = "recovery";
const APPLICATION_ID: i64 = i32::from_be_bytes(*b"CAUR") as i64;
const SCHEMA_VERSION: i64 = 2;
const OWNER_FILE_MODE: u32 = 0o600;
const OWNER_DIR_MODE: u32 = 0o700;
const PAGE_SIZE: i64 = 4096;
const MAX_PAGE_COUNT: i64 = 4096;
const MAX_DATABASE_BYTES: u64 = PAGE_SIZE as u64 * MAX_PAGE_COUNT as u64;
const MAX_ROWS: usize = 4096;
const MAX_GC_ROWS: usize = 256;
const MAX_SQLITE_VALUE_BYTES: i32 = 64 * 1024;
const MAX_OPERATION_KIND_BYTES: usize = 64;
const SHA256_DIGEST_BYTES: usize = 71;
const MAX_LOCK_KEYS: usize = 64;
const MAX_LOCK_KEY_BYTES: usize = 512;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA: &str = r#"
CREATE TABLE remote_operations (
    operation_id         TEXT PRIMARY KEY,
    invocation_id        TEXT NOT NULL UNIQUE,
    preparation_id       TEXT NOT NULL UNIQUE,
    binding              TEXT NOT NULL CHECK(json_valid(binding)),
    source                TEXT NOT NULL,
    server                TEXT NOT NULL,
    workspace             TEXT NOT NULL,
    workspace_generation  TEXT NOT NULL,
    resource_namespace    TEXT NOT NULL,
    principal             TEXT NOT NULL,
    project               TEXT NOT NULL,
    cwd_handle            TEXT NOT NULL,
    cursor_label          TEXT,
    operation_kind        TEXT NOT NULL,
    request_digest        TEXT NOT NULL,
    lock_keys             TEXT NOT NULL CHECK(json_valid(lock_keys)),
    state                 TEXT NOT NULL CHECK(state IN (
                              'reserved', 'dispatched', 'succeeded', 'failed',
                              'cancelled', 'indeterminate'
                          )),
    created_at            INTEGER NOT NULL,
    updated_at            INTEGER NOT NULL,
    dispatched_at         INTEGER,
    terminal_at           INTEGER,
    acknowledged_at       INTEGER,
    side_effects_possible INTEGER NOT NULL CHECK(side_effects_possible IN (0, 1))
) STRICT;

CREATE INDEX remote_operations_binding_pending ON remote_operations(
    source, server, workspace, workspace_generation, resource_namespace,
    principal, project, state, updated_at
);
CREATE INDEX remote_operations_gc ON remote_operations(acknowledged_at, terminal_at, updated_at);
"#;

#[derive(Debug, thiserror::Error)]
pub enum RemoteOperationJournalError {
    #[error("persistent local state is unavailable: {0}")]
    PersistentStateUnavailable(#[source] io::Error),
    #[error("remote operation journal storage is unsafe: {0}")]
    UnsafeStorage(String),
    #[error("remote operation journal SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("remote operation journal serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("duplicate remote operation idempotency identity")]
    DuplicateId,
    #[error("remote operation {0} was not found")]
    NotFound(String),
    #[error("remote operation cannot transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: RemoteOperationState,
        to: RemoteOperationState,
    },
    #[error("{field} exceeds its storage bound")]
    LimitExceeded { field: &'static str },
    #[error("remote operation journal reached its row or byte bound")]
    JournalFull,
    #[error("remote operation journal schema version {0} is unsupported")]
    UnsupportedVersion(i64),
    #[error("remote operation journal contains invalid data in {0}")]
    CorruptData(&'static str),
    #[error("pending remote operation {0} belongs to a different workspace generation")]
    PendingBindingMismatch(String),
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct OpaqueLockKey(String);

impl OpaqueLockKey {
    pub fn new(value: impl Into<String>) -> Result<Self, RemoteOperationJournalError> {
        let value = value.into();
        validate_text("lock key", &value, MAX_LOCK_KEY_BYTES)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for OpaqueLockKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("OpaqueLockKey")
            .field(&"<opaque>")
            .finish()
    }
}

impl TryFrom<String> for OpaqueLockKey {
    type Error = RemoteOperationJournalError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<OpaqueLockKey> for String {
    fn from(value: OpaqueLockKey) -> Self {
        value.0
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RequestDigest(String);

impl RequestDigest {
    pub fn sha256(value: impl Into<String>) -> Result<Self, RemoteOperationJournalError> {
        let value = value.into();
        let Some(digest) = value.strip_prefix("sha256:") else {
            return Err(RemoteOperationJournalError::LimitExceeded {
                field: "request digest",
            });
        };
        if value.len() != SHA256_DIGEST_BYTES
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(RemoteOperationJournalError::LimitExceeded {
                field: "request digest",
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RequestDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("RequestDigest")
            .field(&"<opaque>")
            .finish()
    }
}

impl TryFrom<String> for RequestDigest {
    type Error = RemoteOperationJournalError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::sha256(value)
    }
}

impl From<RequestDigest> for String {
    fn from(value: RequestDigest) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteOperationState {
    Reserved,
    Dispatched,
    Succeeded,
    Failed,
    Cancelled,
    Indeterminate,
}

impl RemoteOperationState {
    fn storage_name(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Dispatched => "dispatched",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Indeterminate => "indeterminate",
        }
    }

    fn from_storage_name(value: &str) -> Option<Self> {
        match value {
            "reserved" => Some(Self::Reserved),
            "dispatched" => Some(Self::Dispatched),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "indeterminate" => Some(Self::Indeterminate),
            _ => None,
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone)]
pub struct RemoteOperationReservation {
    pub operation_id: OperationId,
    pub invocation_id: OperationId,
    pub preparation_id: OperationId,
    pub binding: StoredWorkspaceBinding,
    pub operation_kind: String,
    pub request_digest: RequestDigest,
    pub lock_keys: Vec<OpaqueLockKey>,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteOperationRecord {
    pub operation_id: OperationId,
    pub invocation_id: OperationId,
    pub preparation_id: OperationId,
    pub binding: StoredWorkspaceBinding,
    pub operation_kind: String,
    pub request_digest: RequestDigest,
    pub lock_keys: Vec<OpaqueLockKey>,
    pub state: RemoteOperationState,
    pub created_at: u64,
    pub updated_at: u64,
    pub dispatched_at: Option<u64>,
    pub terminal_at: Option<u64>,
    pub acknowledged_at: Option<u64>,
    pub side_effects_possible: bool,
}

pub struct RemoteOperationJournal {
    connection: Connection,
    path: PathBuf,
}

impl RemoteOperationJournal {
    pub fn open(state_dir: &StateDir) -> Result<Self, RemoteOperationJournalError> {
        let directory = state_dir.persistent_path().join(JOURNAL_DIR);
        ensure_private_directory(&directory)?;
        let path = directory.join(REMOTE_OPERATION_JOURNAL_FILE);
        create_owner_only(&path)?;
        verify_sidecars(&path)?;
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|error| {
            RemoteOperationJournalError::PersistentStateUnavailable(io::Error::other(error))
        })?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_SQLITE_VALUE_BYTES)?;
        initialize(&connection)?;
        verify_sidecars(&path)?;
        Ok(Self { connection, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn reserve_before_send(
        &mut self,
        reservation: &RemoteOperationReservation,
    ) -> Result<(), RemoteOperationJournalError> {
        validate_reservation(reservation)?;
        if database_bytes(&self.path) >= MAX_DATABASE_BYTES {
            return Err(RemoteOperationJournalError::JournalFull);
        }
        let binding = serde_json::to_string(&reservation.binding)?;
        let lock_keys = serde_json::to_string(&reservation.lock_keys)?;
        let created_at = to_i64(reservation.created_at, "created_at")?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count: i64 =
            transaction.query_row("SELECT count(*) FROM remote_operations", [], |row| {
                row.get(0)
            })?;
        if usize::try_from(count).unwrap_or(MAX_ROWS) >= MAX_ROWS {
            return Err(RemoteOperationJournalError::JournalFull);
        }
        let duplicate: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM remote_operations WHERE operation_id IN (?1, ?2, ?3) \
             OR invocation_id IN (?1, ?2, ?3) OR preparation_id IN (?1, ?2, ?3))",
            params![
                reservation.operation_id.as_str(),
                reservation.invocation_id.as_str(),
                reservation.preparation_id.as_str(),
            ],
            |row| row.get(0),
        )?;
        if duplicate {
            return Err(RemoteOperationJournalError::DuplicateId);
        }
        transaction.execute(
            "INSERT INTO remote_operations (
                 operation_id, invocation_id, preparation_id, binding, source, server,
                 workspace, workspace_generation, resource_namespace, principal, project,
                 cwd_handle, cursor_label, operation_kind, request_digest, lock_keys, state,
                 created_at, updated_at, side_effects_possible
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                       ?15, ?16, 'reserved', ?17, ?17, 0)",
            params![
                reservation.operation_id.as_str(),
                reservation.invocation_id.as_str(),
                reservation.preparation_id.as_str(),
                binding,
                reservation.binding.trust_anchor().as_str(),
                reservation.binding.server_id(),
                reservation.binding.workspace_id(),
                reservation.binding.workspace_generation(),
                reservation.binding.resource_namespace_version(),
                reservation.binding.principal_id(),
                reservation.binding.project_key().as_str(),
                reservation.binding.cwd_handle().as_str(),
                reservation.binding.cursor_label(),
                reservation.operation_kind,
                reservation.request_digest.as_str(),
                lock_keys,
                created_at,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn mark_dispatched(
        &mut self,
        operation_id: &OperationId,
        at: u64,
    ) -> Result<(), RemoteOperationJournalError> {
        self.transition(
            operation_id,
            &[RemoteOperationState::Reserved],
            RemoteOperationState::Dispatched,
            at,
            true,
        )
    }

    pub fn commit_terminal(
        &mut self,
        operation_id: &OperationId,
        status: RemoteOperationState,
        side_effects_possible: bool,
        at: u64,
    ) -> Result<(), RemoteOperationJournalError> {
        if !status.is_terminal() {
            return Err(RemoteOperationJournalError::InvalidTransition {
                from: self.state(operation_id)?,
                to: status,
            });
        }
        self.transition(
            operation_id,
            &[
                RemoteOperationState::Reserved,
                RemoteOperationState::Dispatched,
                RemoteOperationState::Indeterminate,
            ],
            status,
            at,
            side_effects_possible,
        )
    }

    pub fn mark_indeterminate(
        &mut self,
        operation_id: &OperationId,
        at: u64,
    ) -> Result<(), RemoteOperationJournalError> {
        self.transition(
            operation_id,
            &[
                RemoteOperationState::Reserved,
                RemoteOperationState::Dispatched,
            ],
            RemoteOperationState::Indeterminate,
            at,
            true,
        )
    }

    pub fn list_pending(
        &self,
        binding: &StoredWorkspaceBinding,
    ) -> Result<Vec<RemoteOperationRecord>, RemoteOperationJournalError> {
        let mismatched_generation = self
            .connection
            .query_row(
                "SELECT operation_id FROM remote_operations
             WHERE source = ?1 AND server = ?2 AND workspace = ?3 AND principal = ?4
                   AND project = ?5 AND resource_namespace = ?6 AND workspace_generation != ?7
                   AND state IN ('reserved', 'dispatched', 'indeterminate')
                   AND acknowledged_at IS NULL
             ORDER BY created_at, operation_id LIMIT 1",
                params![
                    binding.trust_anchor().as_str(),
                    binding.server_id(),
                    binding.workspace_id(),
                    binding.principal_id(),
                    binding.project_key().as_str(),
                    binding.resource_namespace_version(),
                    binding.workspace_generation(),
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(operation_id) = mismatched_generation {
            return Err(RemoteOperationJournalError::PendingBindingMismatch(
                operation_id,
            ));
        }
        let mut statement = self.connection.prepare(
            "SELECT operation_id, invocation_id, preparation_id, binding, operation_kind,
                    request_digest, lock_keys, state, created_at, updated_at, dispatched_at,
                    terminal_at, acknowledged_at, side_effects_possible
             FROM remote_operations WHERE source = ?1 AND server = ?2 AND workspace = ?3
                   AND workspace_generation = ?4 AND resource_namespace = ?5
                   AND principal = ?6 AND project = ?7
                   AND state IN ('reserved', 'dispatched', 'indeterminate')
                   AND acknowledged_at IS NULL
             ORDER BY created_at, operation_id LIMIT ?8",
        )?;
        let records = statement
            .query_map(
                params![
                    binding.trust_anchor().as_str(),
                    binding.server_id(),
                    binding.workspace_id(),
                    binding.workspace_generation(),
                    binding.resource_namespace_version(),
                    binding.principal_id(),
                    binding.project_key().as_str(),
                    MAX_ROWS as i64,
                ],
                record_from_row,
            )?
            .collect::<Result<Vec<_>, _>>()
            .map_err(RemoteOperationJournalError::from)?;
        if let Some(record) = records
            .iter()
            .find(|record| !record.binding.same_workspace_identity(binding))
        {
            return Err(RemoteOperationJournalError::PendingBindingMismatch(
                record.operation_id.as_str().to_owned(),
            ));
        }
        Ok(records)
    }

    pub fn acknowledge(
        &mut self,
        operation_id: &OperationId,
        at: u64,
    ) -> Result<(), RemoteOperationJournalError> {
        let changed = self.connection.execute(
            "UPDATE remote_operations SET acknowledged_at = ?1, updated_at = ?1
             WHERE operation_id = ?2",
            params![to_i64(at, "acknowledged_at")?, operation_id.as_str()],
        )?;
        require_changed(changed, operation_id)
    }

    pub fn reconcile(
        &mut self,
        operation_id: &OperationId,
        status: RemoteOperationState,
        side_effects_possible: bool,
        at: u64,
    ) -> Result<(), RemoteOperationJournalError> {
        self.commit_terminal(operation_id, status, side_effects_possible, at)?;
        self.acknowledge(operation_id, at)
    }

    pub fn gc(
        &mut self,
        updated_before: u64,
        limit: usize,
    ) -> Result<usize, RemoteOperationJournalError> {
        let limit = limit.min(MAX_GC_ROWS);
        if limit == 0 {
            return Ok(0);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let removed = transaction.execute(
            "DELETE FROM remote_operations WHERE operation_id IN (
                 SELECT operation_id FROM remote_operations
                 WHERE updated_at < ?1 AND (acknowledged_at IS NOT NULL OR terminal_at IS NOT NULL)
                 ORDER BY updated_at LIMIT ?2
             )",
            params![
                to_i64(updated_before, "updated_before")?,
                i64::try_from(limit).unwrap_or(MAX_GC_ROWS as i64),
            ],
        )?;
        transaction.commit()?;
        Ok(removed)
    }

    fn state(
        &self,
        operation_id: &OperationId,
    ) -> Result<RemoteOperationState, RemoteOperationJournalError> {
        let state = self
            .connection
            .query_row(
                "SELECT state FROM remote_operations WHERE operation_id = ?1",
                params![operation_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| RemoteOperationJournalError::NotFound(operation_id.as_str().into()))?;
        RemoteOperationState::from_storage_name(&state)
            .ok_or(RemoteOperationJournalError::CorruptData("state"))
    }

    fn transition(
        &mut self,
        operation_id: &OperationId,
        allowed: &[RemoteOperationState],
        target: RemoteOperationState,
        at: u64,
        side_effects_possible: bool,
    ) -> Result<(), RemoteOperationJournalError> {
        let current = self.state(operation_id)?;
        if !allowed.contains(&current) {
            return Err(RemoteOperationJournalError::InvalidTransition {
                from: current,
                to: target,
            });
        }
        let at = to_i64(at, "transition timestamp")?;
        let dispatched_at = (target == RemoteOperationState::Dispatched).then_some(at);
        let terminal_at = target.is_terminal().then_some(at);
        let changed = self.connection.execute(
            "UPDATE remote_operations SET state = ?1, updated_at = ?2,
                 dispatched_at = coalesce(?3, dispatched_at), terminal_at = coalesce(?4, terminal_at),
                 side_effects_possible = ?5 WHERE operation_id = ?6 AND state = ?7",
            params![
                target.storage_name(),
                at,
                dispatched_at,
                terminal_at,
                side_effects_possible,
                operation_id.as_str(),
                current.storage_name(),
            ],
        )?;
        if changed == 0 {
            return Err(RemoteOperationJournalError::InvalidTransition {
                from: self.state(operation_id)?,
                to: target,
            });
        }
        Ok(())
    }
}

fn initialize(connection: &Connection) -> Result<(), RemoteOperationJournalError> {
    connection.execute_batch(&format!(
        "PRAGMA page_size = {PAGE_SIZE};
         PRAGMA max_page_count = {MAX_PAGE_COUNT};
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA trusted_schema = OFF;"
    ))?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == 0 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(SCHEMA)?;
        transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
    } else if version != SCHEMA_VERSION {
        return Err(RemoteOperationJournalError::UnsupportedVersion(version));
    }
    let application_id: i64 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    if application_id != APPLICATION_ID {
        return Err(RemoteOperationJournalError::UnsafeStorage(
            "unexpected SQLite application id".into(),
        ));
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), RemoteOperationJournalError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(RemoteOperationJournalError::UnsafeStorage(format!(
                "{} is not a regular directory",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .map_err(RemoteOperationJournalError::PersistentStateUnavailable)?;
        }
        Err(error) => {
            return Err(RemoteOperationJournalError::PersistentStateUnavailable(
                error,
            ));
        }
    }
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(OWNER_DIR_MODE))
            .map_err(RemoteOperationJournalError::PersistentStateUnavailable)?;
        let metadata =
            fs::metadata(path).map_err(RemoteOperationJournalError::PersistentStateUnavailable)?;
        if metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(RemoteOperationJournalError::UnsafeStorage(
                "journal directory must be owner-only and owned by the current user".into(),
            ));
        }
    }
    Ok(())
}

fn create_owner_only(path: &Path) -> Result<(), RemoteOperationJournalError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(RemoteOperationJournalError::UnsafeStorage(format!(
                "{} is not a regular non-symlink file",
                path.display()
            )));
        }
        Ok(metadata) => verify_owner_only(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options
                .mode(OWNER_FILE_MODE)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
            options
                .open(path)
                .map_err(RemoteOperationJournalError::PersistentStateUnavailable)?;
        }
        Err(error) => {
            return Err(RemoteOperationJournalError::PersistentStateUnavailable(
                error,
            ));
        }
    }
    Ok(())
}

fn verify_owner_only(metadata: &fs::Metadata) -> Result<(), RemoteOperationJournalError> {
    #[cfg(unix)]
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(RemoteOperationJournalError::UnsafeStorage(
            "journal file must be owner-only and owned by the current user".into(),
        ));
    }
    Ok(())
}

fn verify_sidecars(path: &Path) -> Result<(), RemoteOperationJournalError> {
    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar(path, suffix);
        match fs::symlink_metadata(&sidecar) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(RemoteOperationJournalError::UnsafeStorage(format!(
                    "{} is not a regular non-symlink file",
                    sidecar.display()
                )));
            }
            Ok(metadata) => verify_owner_only(&metadata)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(RemoteOperationJournalError::PersistentStateUnavailable(
                    error,
                ));
            }
        }
    }
    Ok(())
}

fn validate_reservation(
    reservation: &RemoteOperationReservation,
) -> Result<(), RemoteOperationJournalError> {
    let ids = [
        reservation.operation_id.as_str(),
        reservation.invocation_id.as_str(),
        reservation.preparation_id.as_str(),
    ];
    if ids[0] == ids[1] || ids[0] == ids[2] || ids[1] == ids[2] {
        return Err(RemoteOperationJournalError::DuplicateId);
    }
    validate_text(
        "operation kind",
        &reservation.operation_kind,
        MAX_OPERATION_KIND_BYTES,
    )?;
    if reservation.lock_keys.len() > MAX_LOCK_KEYS {
        return Err(RemoteOperationJournalError::LimitExceeded { field: "lock keys" });
    }
    Ok(())
}

fn validate_text(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), RemoteOperationJournalError> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        return Err(RemoteOperationJournalError::LimitExceeded { field });
    }
    Ok(())
}

fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RemoteOperationRecord> {
    let operation_id = parse_operation_id(row.get(0)?, 0)?;
    let invocation_id = parse_operation_id(row.get(1)?, 1)?;
    let preparation_id = parse_operation_id(row.get(2)?, 2)?;
    let binding = serde_json::from_str(&row.get::<_, String>(3)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let lock_keys = serde_json::from_str(&row.get::<_, String>(6)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let state_name: String = row.get(7)?;
    let state = RemoteOperationState::from_storage_name(&state_name).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            7,
            rusqlite::types::Type::Text,
            Box::new(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid operation state",
            )),
        )
    })?;
    Ok(RemoteOperationRecord {
        operation_id,
        invocation_id,
        preparation_id,
        binding,
        operation_kind: row.get(4)?,
        request_digest: RequestDigest::sha256(row.get::<_, String>(5)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        lock_keys,
        state,
        created_at: from_i64(row.get(8)?, 8)?,
        updated_at: from_i64(row.get(9)?, 9)?,
        dispatched_at: optional_u64(row.get(10)?, 10)?,
        terminal_at: optional_u64(row.get(11)?, 11)?,
        acknowledged_at: optional_u64(row.get(12)?, 12)?,
        side_effects_possible: row.get(13)?,
    })
}

fn parse_operation_id(value: String, index: usize) -> rusqlite::Result<OperationId> {
    OperationId::new(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn from_i64(value: i64, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn optional_u64(value: Option<i64>, index: usize) -> rusqlite::Result<Option<u64>> {
    value.map(|value| from_i64(value, index)).transpose()
}

fn to_i64(value: u64, field: &'static str) -> Result<i64, RemoteOperationJournalError> {
    i64::try_from(value).map_err(|_| RemoteOperationJournalError::LimitExceeded { field })
}

fn require_changed(
    changed: usize,
    operation_id: &OperationId,
) -> Result<(), RemoteOperationJournalError> {
    if changed == 1 {
        Ok(())
    } else {
        Err(RemoteOperationJournalError::NotFound(
            operation_id.as_str().into(),
        ))
    }
}

fn database_bytes(path: &Path) -> u64 {
    [
        path.to_path_buf(),
        sidecar(path, "-wal"),
        sidecar(path, "-shm"),
    ]
    .iter()
    .filter_map(|path| fs::metadata(path).ok())
    .map(|metadata| metadata.len())
    .sum()
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(suffix);
    PathBuf::from(sidecar)
}

#[cfg(test)]
mod tests {
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CwdHandle, ProjectIdentity, ProjectKey,
        SessionBindingId, SessionWorkspaceBinding, SourceTrustAnchor,
    };
    use tempfile::TempDir;

    use super::*;

    const LABEL: &str = "workspace";
    const REQUEST_DIGEST: &str =
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const RAW_LOCK_KEY: &str = "opaque-lock-value";

    fn binding(
        source: &str,
        authority_id: &str,
        principal_id: &str,
        project_key: &str,
        cursor: &str,
    ) -> StoredWorkspaceBinding {
        binding_with_generation(
            source,
            authority_id,
            principal_id,
            project_key,
            cursor,
            "generation",
        )
    }

    fn binding_with_generation(
        source: &str,
        authority_id: &str,
        principal_id: &str,
        project_key: &str,
        cursor: &str,
        generation: &str,
    ) -> StoredWorkspaceBinding {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new(source).unwrap(),
            authority_id,
            "workspace",
            generation,
            "namespace",
        )
        .unwrap();
        let principal = AuthenticatedPrincipalId::new(authority.clone(), principal_id).unwrap();
        let project =
            ProjectIdentity::new(authority.clone(), ProjectKey::new(project_key).unwrap());
        let session_binding = SessionWorkspaceBinding::new(
            SessionBindingId::new(format!("binding-{cursor}")).unwrap(),
            authority,
            principal,
            project,
        )
        .unwrap();
        StoredWorkspaceBinding::new(
            session_binding,
            CwdHandle::new(cursor).unwrap(),
            Some(LABEL.into()),
        )
        .unwrap()
    }

    fn reservation(id: &str, binding: StoredWorkspaceBinding) -> RemoteOperationReservation {
        RemoteOperationReservation {
            operation_id: OperationId::new(format!("operation-{id}")).unwrap(),
            invocation_id: OperationId::new(format!("invocation-{id}")).unwrap(),
            preparation_id: OperationId::new(format!("preparation-{id}")).unwrap(),
            binding,
            operation_kind: "tool_mutation".into(),
            request_digest: RequestDigest::sha256(REQUEST_DIGEST).unwrap(),
            lock_keys: vec![OpaqueLockKey::new(RAW_LOCK_KEY).unwrap()],
            created_at: 10,
        }
    }

    fn default_binding() -> StoredWorkspaceBinding {
        binding("source", "authority", "principal", "project", "cursor")
    }

    #[test]
    fn ephemeral_transcripts_keep_the_recovery_journal_persistent() {
        let temp = TempDir::new().unwrap();
        let volatile = temp.path().join("volatile");
        let persistent = temp.path().join("persistent");
        let state_dir = StateDir::split(volatile.clone(), persistent.clone());

        let journal = RemoteOperationJournal::open(&state_dir).unwrap();

        assert!(journal.path().starts_with(&persistent));
        assert!(!journal.path().starts_with(&volatile));
        assert!(!persistent.join(crate::sessions::SESSIONS_DB_FILE).exists());
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(journal.path()).unwrap().permissions().mode() & 0o777,
            OWNER_FILE_MODE
        );
    }

    #[test]
    fn reservation_survives_reopen_and_duplicate_ids_fail() {
        let temp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let first_reservation = reservation("one", default_binding());
        {
            let mut journal = RemoteOperationJournal::open(&state_dir).unwrap();
            journal.reserve_before_send(&first_reservation).unwrap();
        }

        let mut journal = RemoteOperationJournal::open(&state_dir).unwrap();
        let rows = journal.list_pending(&first_reservation.binding).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, RemoteOperationState::Reserved);
        assert!(matches!(
            journal.reserve_before_send(&first_reservation),
            Err(RemoteOperationJournalError::DuplicateId)
        ));

        let mut colliding = reservation("two", default_binding());
        colliding.invocation_id = first_reservation.invocation_id.clone();
        assert!(matches!(
            journal.reserve_before_send(&colliding),
            Err(RemoteOperationJournalError::DuplicateId)
        ));
    }

    #[test]
    fn pending_lookup_is_project_wide_but_keeps_other_identity_dimensions_isolated() {
        let temp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let exact = default_binding();
        let mut journal = RemoteOperationJournal::open(&state_dir).unwrap();
        journal
            .reserve_before_send(&reservation("exact", exact.clone()))
            .unwrap();

        for other in [
            binding(
                "other-source",
                "authority",
                "principal",
                "project",
                "cursor",
            ),
            binding(
                "source",
                "other-authority",
                "principal",
                "project",
                "cursor",
            ),
            binding(
                "source",
                "authority",
                "other-principal",
                "project",
                "cursor",
            ),
            binding(
                "source",
                "authority",
                "principal",
                "other-project",
                "cursor",
            ),
        ] {
            assert!(journal.list_pending(&other).unwrap().is_empty());
        }
        let nested = binding(
            "source",
            "authority",
            "principal",
            "project",
            "nested-cursor",
        );
        let rows = journal.list_pending(&nested).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].binding.cwd_handle().as_str(), "cursor");
        assert_eq!(journal.list_pending(&exact).unwrap().len(), 1);
    }

    #[test]
    fn workspace_generation_mismatch_surfaces_pending_operation_without_querying_it() {
        let temp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let exact = default_binding();
        let reservation = reservation("old-generation", exact.clone());
        let mut journal = RemoteOperationJournal::open(&state_dir).unwrap();
        journal.reserve_before_send(&reservation).unwrap();
        let restarted = binding_with_generation(
            "source",
            "authority",
            "principal",
            "project",
            "cursor",
            "different-generation",
        );

        assert!(matches!(
            journal.list_pending(&restarted),
            Err(RemoteOperationJournalError::PendingBindingMismatch(operation_id))
                if operation_id == reservation.operation_id.as_str()
        ));
    }

    #[test]
    fn transitions_reconcile_and_bounded_gc() {
        let temp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let binding = default_binding();
        let first = reservation("first", binding.clone());
        let second = reservation("second", binding.clone());
        let mut journal = RemoteOperationJournal::open(&state_dir).unwrap();
        journal.reserve_before_send(&first).unwrap();
        journal.reserve_before_send(&second).unwrap();
        journal.mark_dispatched(&first.operation_id, 11).unwrap();
        journal.mark_indeterminate(&first.operation_id, 12).unwrap();
        journal
            .reconcile(
                &first.operation_id,
                RemoteOperationState::Succeeded,
                true,
                13,
            )
            .unwrap();
        journal
            .commit_terminal(
                &second.operation_id,
                RemoteOperationState::Failed,
                false,
                13,
            )
            .unwrap();

        assert!(journal.list_pending(&binding).unwrap().is_empty());
        assert_eq!(journal.gc(14, 1).unwrap(), 1);
        assert_eq!(journal.gc(14, usize::MAX).unwrap(), 1);
    }

    #[test]
    fn records_are_bounded_and_debug_redacts_opaque_values() {
        const RAW_WORKSPACE_PATH: &str = "/private/workspace/path";

        let lock_key = OpaqueLockKey::new(RAW_LOCK_KEY).unwrap();
        assert!(!format!("{lock_key:?}").contains(RAW_LOCK_KEY));
        let digest = RequestDigest::sha256(REQUEST_DIGEST).unwrap();
        assert!(!format!("{digest:?}").contains(REQUEST_DIGEST));
        assert!(RequestDigest::sha256("raw request content").is_err());
        assert!(matches!(
            OpaqueLockKey::new("x".repeat(MAX_LOCK_KEY_BYTES + 1)),
            Err(RemoteOperationJournalError::LimitExceeded { field: "lock key" })
        ));

        let temp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let mut journal = RemoteOperationJournal::open(&state_dir).unwrap();
        journal
            .reserve_before_send(&reservation(
                "redacted-path",
                StoredWorkspaceBinding::local_from_cwd(RAW_WORKSPACE_PATH),
            ))
            .unwrap();
        for path in [
            journal.path().to_path_buf(),
            sidecar(journal.path(), "-wal"),
            sidecar(journal.path(), "-shm"),
        ] {
            if let Ok(bytes) = fs::read(path) {
                assert!(
                    !bytes
                        .windows(RAW_WORKSPACE_PATH.len())
                        .any(|window| { window == RAW_WORKSPACE_PATH.as_bytes() })
                );
            }
        }
        let mut oversized = reservation("large", default_binding());
        oversized.lock_keys = (0..=MAX_LOCK_KEYS)
            .map(|index| OpaqueLockKey::new(format!("lock-{index}")).unwrap())
            .collect();
        assert!(matches!(
            journal.reserve_before_send(&oversized),
            Err(RemoteOperationJournalError::LimitExceeded { field: "lock keys" })
        ));

        let columns = journal
            .connection
            .prepare("SELECT name FROM pragma_table_info('remote_operations') ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        for forbidden in ["token", "endpoint", "query", "args", "content", "output"] {
            assert!(!columns.iter().any(|column| column.contains(forbidden)));
        }

        let mut duplicate_within_record = reservation("same", default_binding());
        duplicate_within_record.invocation_id = duplicate_within_record.operation_id.clone();
        assert!(matches!(
            journal.reserve_before_send(&duplicate_within_record),
            Err(RemoteOperationJournalError::DuplicateId)
        ));
    }

    #[test]
    fn journal_enforces_row_and_database_size_bounds() {
        let temp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let mut journal = RemoteOperationJournal::open(&state_dir).unwrap();
        journal
            .connection
            .execute(
                "WITH RECURSIVE seq(value) AS (
                     VALUES(1) UNION ALL SELECT value + 1 FROM seq WHERE value < ?1
                 )
                 INSERT INTO remote_operations (
                     operation_id, invocation_id, preparation_id, binding, source, server,
                     workspace, workspace_generation, resource_namespace, principal, project,
                     cwd_handle, operation_kind, request_digest, lock_keys, state, created_at,
                     updated_at, side_effects_possible
                  ) SELECT 'operation-' || value, 'invocation-' || value,
                           'preparation-' || value, '{}', 'source', 'authority', 'workspace',
                           'generation', 'namespace', 'principal', 'project', 'cursor',
                           'tool_mutation', ?2, '[]', 'failed', 1, 1, 0
                   FROM seq",
                params![MAX_ROWS as i64, REQUEST_DIGEST],
            )
            .unwrap();
        assert!(matches!(
            journal.reserve_before_send(&reservation("overflow", default_binding())),
            Err(RemoteOperationJournalError::JournalFull)
        ));

        let oversized = temp.path().join("oversized");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&oversized)
            .unwrap()
            .set_len(MAX_DATABASE_BYTES)
            .unwrap();
        journal.path = oversized;
        assert!(matches!(
            journal.reserve_before_send(&reservation("too-large", default_binding())),
            Err(RemoteOperationJournalError::JournalFull)
        ));
    }

    #[test]
    fn unavailable_or_symlinked_persistent_state_fails_closed() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("not-a-directory");
        fs::write(&file, "occupied").unwrap();
        let state_dir = StateDir::split(temp.path().join("volatile"), file);
        assert!(matches!(
            RemoteOperationJournal::open(&state_dir),
            Err(RemoteOperationJournalError::PersistentStateUnavailable(_))
        ));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let persistent = temp.path().join("persistent");
            let target = temp.path().join("target");
            fs::create_dir_all(&persistent).unwrap();
            fs::create_dir_all(&target).unwrap();
            symlink(&target, persistent.join(JOURNAL_DIR)).unwrap();
            let state_dir = StateDir::from_path(persistent);
            assert!(matches!(
                RemoteOperationJournal::open(&state_dir),
                Err(RemoteOperationJournalError::UnsafeStorage(_))
            ));
        }
    }
}
