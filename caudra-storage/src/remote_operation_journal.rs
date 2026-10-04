use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use caudra_workspace::{OperationId, WorkspacePath};
use rusqlite::limits::Limit;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::StateDir;
use crate::workspace_binding::StoredWorkspaceBinding;

mod admission;

use admission::JournalAdmission;

pub const REMOTE_OPERATION_JOURNAL_FILE: &str = "remote-operations.db";
const JOURNAL_DIR: &str = "recovery";
const APPLICATION_ID: i64 = i32::from_be_bytes(*b"CAUR") as i64;
const SCHEMA_VERSION: i64 = 5;
const OWNER_FILE_MODE: u32 = 0o600;
const OWNER_DIR_MODE: u32 = 0o700;
const PAGE_SIZE: i64 = 4096;
const MAX_PAGE_COUNT: i64 = 4096;
const MAX_DATABASE_BYTES: u64 = PAGE_SIZE as u64 * MAX_PAGE_COUNT as u64;
const MAX_ROWS: usize = 4096;
const MAX_GC_ROWS: usize = 256;
const MAX_SQLITE_VALUE_BYTES: i32 = 64 * 1024;
const MAX_OPERATION_KIND_BYTES: usize = 64;
const OPERATION_KIND_FIELD: &str = "operation kind";
const SHA256_DIGEST_BYTES: usize = 71;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SCHEMA_OBJECTS: i64 = 16;

const SCHEMA: &str = r#"
CREATE TABLE remote_operations (
    operation_id         TEXT PRIMARY KEY,
    invocation_id        TEXT NOT NULL UNIQUE,
    preparation_id       TEXT NOT NULL UNIQUE,
    publication_id       TEXT,
    publication_cwd      TEXT,
    binding              TEXT NOT NULL CHECK(json_valid(binding)),
    source                TEXT NOT NULL,
    server                TEXT NOT NULL,
    host_instance_id      TEXT NOT NULL,
    workspace             TEXT NOT NULL,
    workspace_generation  TEXT NOT NULL,
    resource_namespace    TEXT NOT NULL,
    principal             TEXT NOT NULL,
    project               TEXT NOT NULL,
    cwd_handle            TEXT NOT NULL,
    cursor_label          TEXT,
    operation_kind        TEXT NOT NULL,
    request_digest        TEXT NOT NULL,
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
    #[error(
        "remote operation journal at {} has schema version {found}, but this build requires {supported}; \
         it records only in-flight remote operations, so deleting the file restores service and loses \
         nothing beyond the ability to reconcile operations that were still unresolved",
        path.display()
    )]
    UnsupportedVersion {
        path: PathBuf,
        found: i64,
        supported: i64,
    },
    #[error("remote operation journal changed during admission; retry opening it")]
    AdmissionChanged,
    #[error("remote operation journal contains invalid data in {0}")]
    CorruptData(&'static str),
}

impl From<io::Error> for RemoteOperationJournalError {
    fn from(error: io::Error) -> Self {
        Self::PersistentStateUnavailable(error)
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
    pub publication_cwd: Option<WorkspacePath>,
    pub host_instance_id: String,
    pub publication_id: Option<OperationId>,
    pub operation_id: OperationId,
    pub invocation_id: OperationId,
    pub preparation_id: OperationId,
    pub binding: StoredWorkspaceBinding,
    pub operation_kind: String,
    pub request_digest: RequestDigest,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteOperationRecord {
    pub publication_cwd: Option<WorkspacePath>,
    pub publication_id: Option<OperationId>,
    pub host_instance_id: String,
    pub operation_id: OperationId,
    pub invocation_id: OperationId,
    pub preparation_id: OperationId,
    pub binding: StoredWorkspaceBinding,
    pub operation_kind: String,
    pub request_digest: RequestDigest,
    pub state: RemoteOperationState,
    pub created_at: u64,
    pub updated_at: u64,
    pub dispatched_at: Option<u64>,
    pub terminal_at: Option<u64>,
    pub acknowledged_at: Option<u64>,
    pub side_effects_possible: bool,
}

impl RemoteOperationRecord {
    /// False for an operation an earlier generation of the workspace recorded:
    /// the host that ran it is gone, so it can be acknowledged but never
    /// reconciled, and nothing done to the current workspace can affect it.
    pub fn reachable_from(&self, current: &StoredWorkspaceBinding) -> bool {
        self.binding.workspace_generation() == current.workspace_generation()
    }
}

pub struct RemoteOperationJournal {
    connection: Connection,
    path: PathBuf,
}

impl RemoteOperationJournal {
    pub fn open(state_dir: &StateDir) -> Result<Self, RemoteOperationJournalError> {
        let directory = state_dir.persistent_path().join(JOURNAL_DIR);
        verify_directory(&directory)?;
        let path = directory.join(REMOTE_OPERATION_JOURNAL_FILE);
        let admission = JournalAdmission::inspect(&path)?;
        let fresh = admission.is_new();
        // Cooperating openers serialize only after the original state is accepted.
        let _admission_lock = if fresh {
            admission.recheck(&path)?;
            ensure_private_directory(&directory)?;
            let file = create_owner_only(&path)?;
            file.lock()?;
            file
        } else {
            let file = admission.lock_existing(&path)?;
            ensure_private_directory(&directory)?;
            file
        };
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
        initialize(&connection, &path, fresh)?;
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
                 host_instance_id, workspace, workspace_generation, resource_namespace, principal, project,
                 cwd_handle, cursor_label, operation_kind, request_digest, state,
                 created_at, updated_at, side_effects_possible, publication_id, publication_cwd
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                       ?15, ?16, 'reserved', ?17, ?17, 0, ?18, ?19)",
            params![
                reservation.operation_id.as_str(),
                reservation.invocation_id.as_str(),
                reservation.preparation_id.as_str(),
                binding,
                reservation.binding.trust_anchor().as_str(),
                reservation.binding.server_id(),
                reservation.host_instance_id,
                reservation.binding.workspace_id(),
                reservation.binding.workspace_generation(),
                reservation.binding.resource_namespace_version(),
                reservation.binding.principal_id(),
                reservation.binding.project_key().as_str(),
                reservation.binding.cwd_handle().as_str(),
                reservation.binding.cursor_label(),
                reservation.operation_kind,
                reservation.request_digest.as_str(),
                created_at,
                reservation.publication_id.as_ref().map(OperationId::as_str),
                reservation.publication_cwd.as_ref().map(WorkspacePath::as_str),
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

    /// Every unresolved operation of this workspace, including those recorded
    /// against an earlier generation of it. Those can no longer be reconciled
    /// with the host that ran them, but they stay listed until acknowledged;
    /// callers tell them apart with [`RemoteOperationRecord::reachable_from`].
    pub fn list_pending(
        &self,
        binding: &StoredWorkspaceBinding,
    ) -> Result<Vec<RemoteOperationRecord>, RemoteOperationJournalError> {
        let mut statement = self.connection.prepare(
            "SELECT operation_id, invocation_id, preparation_id, binding, operation_kind,
                    request_digest, state, created_at, updated_at, dispatched_at,
                    terminal_at, acknowledged_at, side_effects_possible, publication_id, publication_cwd,
                    host_instance_id
             FROM remote_operations WHERE source = ?1 AND server = ?2 AND workspace = ?3
                   AND resource_namespace = ?4 AND principal = ?5 AND project = ?6
                   AND state IN ('reserved', 'dispatched', 'indeterminate')
                   AND acknowledged_at IS NULL
             ORDER BY created_at, operation_id LIMIT ?7",
        )?;
        let records = statement
            .query_map(
                params![
                    binding.trust_anchor().as_str(),
                    binding.server_id(),
                    binding.workspace_id(),
                    binding.resource_namespace_version(),
                    binding.principal_id(),
                    binding.project_key().as_str(),
                    MAX_ROWS as i64,
                ],
                record_from_row,
            )?
            .collect::<Result<Vec<_>, _>>()
            .map_err(RemoteOperationJournalError::from)?;
        if records
            .iter()
            .any(|record| !same_workspace_in_any_generation(&record.binding, binding))
        {
            return Err(RemoteOperationJournalError::CorruptData("binding"));
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

fn validate_schema(
    connection: &Connection,
    path: &Path,
) -> Result<(), RemoteOperationJournalError> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(RemoteOperationJournalError::UnsupportedVersion {
            path: path.to_path_buf(),
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    let application_id: i64 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    if application_id != APPLICATION_ID {
        return Err(RemoteOperationJournalError::UnsafeStorage(
            "unexpected SQLite application id".into(),
        ));
    }
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(SCHEMA)?;
    if schema_objects(connection)? != schema_objects(&expected)? {
        return Err(RemoteOperationJournalError::UnsafeStorage(
            "unexpected SQLite schema".into(),
        ));
    }
    Ok(())
}

fn schema_objects(connection: &Connection) -> Result<Vec<[String; 4]>, rusqlite::Error> {
    connection
        .prepare(
            "SELECT type, name, tbl_name, coalesce(sql, '') FROM sqlite_schema
             ORDER BY type, name LIMIT ?1",
        )?
        .query_map([MAX_SCHEMA_OBJECTS], |row| {
            Ok([row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?])
        })?
        .collect()
}

fn initialize(
    connection: &Connection,
    path: &Path,
    fresh: bool,
) -> Result<(), RemoteOperationJournalError> {
    if !fresh {
        validate_schema(connection, path)?;
    }
    connection.execute_batch(&format!(
        "PRAGMA page_size = {PAGE_SIZE};
         PRAGMA max_page_count = {MAX_PAGE_COUNT};
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA trusted_schema = OFF;"
    ))?;
    if fresh {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(SCHEMA)?;
        transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
    }
    Ok(())
}

fn verify_directory(path: &Path) -> Result<(), RemoteOperationJournalError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(RemoteOperationJournalError::UnsafeStorage(format!(
                "{} is not a regular directory",
                path.display()
            )));
        }
        Ok(metadata) =>
        {
            #[cfg(unix)]
            if metadata.uid() != rustix::process::geteuid().as_raw() {
                return Err(RemoteOperationJournalError::UnsafeStorage(
                    "journal directory must be owned by the current user".into(),
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(RemoteOperationJournalError::PersistentStateUnavailable(
                error,
            ));
        }
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), RemoteOperationJournalError> {
    verify_directory(path)?;
    fs::create_dir_all(path)?;
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

fn create_owner_only(path: &Path) -> Result<File, RemoteOperationJournalError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options
        .mode(OWNER_FILE_MODE)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    Ok(options.open(path)?)
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
    for suffix in ["-wal", "-shm", "-journal"] {
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
        OPERATION_KIND_FIELD,
        &reservation.operation_kind,
        MAX_OPERATION_KIND_BYTES,
    )
}

/// The row's own binding has to name the workspace its indexed columns
/// matched, or the record is not what the query selected.
fn same_workspace_in_any_generation(
    recorded: &StoredWorkspaceBinding,
    current: &StoredWorkspaceBinding,
) -> bool {
    recorded.trust_anchor() == current.trust_anchor()
        && recorded.server_id() == current.server_id()
        && recorded.workspace_id() == current.workspace_id()
        && recorded.resource_namespace_version() == current.resource_namespace_version()
        && recorded.principal_id() == current.principal_id()
        && recorded.project_key() == current.project_key()
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
    let state_name: String = row.get(6)?;
    let state = RemoteOperationState::from_storage_name(&state_name).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Text,
            Box::new(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid operation state",
            )),
        )
    })?;
    Ok(RemoteOperationRecord {
        publication_cwd: row
            .get::<_, Option<String>>(14)?
            .map(WorkspacePath::new)
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    14,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
        publication_id: row
            .get::<_, Option<String>>(13)?
            .map(|id| parse_operation_id(id, 13))
            .transpose()?,
        host_instance_id: row.get(15)?,
        operation_id,
        invocation_id,
        preparation_id,
        binding,
        operation_kind: parse_operation_kind(row.get(4)?, 4)?,
        request_digest: RequestDigest::sha256(row.get::<_, String>(5)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        state,
        created_at: from_i64(row.get(7)?, 7)?,
        updated_at: from_i64(row.get(8)?, 8)?,
        dispatched_at: optional_u64(row.get(9)?, 9)?,
        terminal_at: optional_u64(row.get(10)?, 10)?,
        acknowledged_at: optional_u64(row.get(11)?, 11)?,
        side_effects_possible: row.get(12)?,
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

/// Held to the bounds it was written with, since reports print it verbatim.
fn parse_operation_kind(value: String, index: usize) -> rusqlite::Result<String> {
    validate_text(OPERATION_KIND_FIELD, &value, MAX_OPERATION_KIND_BYTES).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })?;
    Ok(value)
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
    use std::{
        env,
        fs::Permissions,
        process::{self, Command},
        sync::{Arc, Barrier},
        thread,
        time::SystemTime,
    };
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const LABEL: &str = "workspace";
    const REQUEST_DIGEST: &str =
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const UNSUPPORTED_VERSION_EXPECTED: &str =
        "an unreadable journal must be refused with its path and the supported version";
    const FUTURE_SCHEMA_VERSION: i64 = SCHEMA_VERSION + 1;
    const SHELL_KIND: &str = "canonical:shell";
    const WRITE_KIND: &str = "canonical:file_write";
    const PREVIOUS_GENERATION: &str = "previous-generation";
    const CRASH_PATH_ENV: &str = "CAUDRA_TEST_JOURNAL_CRASH_PATH";
    const CRASH_SQL_ENV: &str = "CAUDRA_TEST_JOURNAL_CRASH_SQL";
    const HOT_JOURNAL_MAGIC: &[u8] = b"\xd9\xd5\x05\xf9\x20\xa1\x63\xd7";
    const SPILL_TRANSACTION: &str = "
        PRAGMA cache_size=1; BEGIN IMMEDIATE;
        UPDATE remote_operations SET updated_at=20;
        CREATE TABLE uncommitted_spill(value BLOB);
        WITH RECURSIVE seq(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM seq WHERE x<100)
        INSERT INTO uncommitted_spill SELECT zeroblob(4096) FROM seq;";

    #[derive(Debug, PartialEq, Eq)]
    struct JournalSnapshot {
        files: Vec<(PathBuf, Vec<u8>, Permissions, SystemTime)>,
        directory_permissions: Permissions,
    }

    fn journal_snapshot(path: &Path) -> JournalSnapshot {
        let directory = path.parent().unwrap();
        let mut files = fs::read_dir(directory)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let metadata = fs::symlink_metadata(&path).unwrap();
                (
                    path.clone(),
                    fs::read(&path).unwrap(),
                    metadata.permissions(),
                    metadata.modified().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        JournalSnapshot {
            files,
            directory_permissions: fs::metadata(directory).unwrap().permissions(),
        }
    }

    fn crash_journal(path: &Path, sql: &str) {
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "remote_operation_journal::tests::journal_crash_child",
                "--nocapture",
            ])
            .env(CRASH_PATH_ENV, path)
            .env(CRASH_SQL_ENV, sql)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    #[test]
    fn journal_crash_child() {
        let Some(path) = env::var_os(CRASH_PATH_ENV) else {
            return;
        };
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(&env::var(CRASH_SQL_ENV).unwrap())
            .unwrap();
        process::exit(0);
    }

    #[test_case("future_wal", Some(FUTURE_SCHEMA_VERSION); "newer_version_only_in_wal")]
    #[test_case("future_wal_without_shm", Some(FUTURE_SCHEMA_VERSION); "newer_version_only_in_wal_without_shm")]
    #[test_case("old_hot", Some(2); "old_schema_hot_rollback")]
    #[test_case("application_wal", None; "wrong_application_only_in_wal")]
    #[test_case("schema_wal", None; "wrong_schema_only_in_wal")]
    fn rejected_crash_state_is_not_recovered_or_chmodded(kind: &str, version: Option<i64>) {
        let temp = TempDir::new().unwrap();
        let state = StateDir::from_path(temp.path().join("state"));
        let mut journal = RemoteOperationJournal::open(&state).unwrap();
        journal
            .reserve_before_send(&reservation("retained", default_binding()))
            .unwrap();
        let path = journal.path().to_owned();
        drop(journal);
        let script = match kind {
            "future_wal" | "future_wal_without_shm" => {
                format!("PRAGMA wal_autocheckpoint=0; PRAGMA user_version={FUTURE_SCHEMA_VERSION};")
            }
            "application_wal" => "PRAGMA wal_autocheckpoint=0; PRAGMA application_id=0;".to_owned(),
            "schema_wal" => "PRAGMA wal_autocheckpoint=0; DROP TABLE remote_operations;".to_owned(),
            "old_hot" => format!(
                "PRAGMA journal_mode=DELETE;
                 ALTER TABLE remote_operations DROP COLUMN publication_id;
                 ALTER TABLE remote_operations DROP COLUMN publication_cwd;
                 PRAGMA user_version=2; {SPILL_TRANSACTION}"
            ),
            _ => unreachable!(),
        };
        crash_journal(&path, &script);
        if kind == "future_wal_without_shm" {
            fs::remove_file(sidecar(&path, "-shm")).unwrap();
        }
        let header = fs::read(&path).unwrap();
        let main_version = i64::from(u32::from_be_bytes(header[60..64].try_into().unwrap()));
        assert_eq!(
            main_version,
            if kind == "old_hot" { 2 } else { SCHEMA_VERSION }
        );
        let recovery = fs::read(sidecar(
            &path,
            if kind == "old_hot" {
                "-journal"
            } else {
                "-wal"
            },
        ))
        .unwrap();
        assert!(!recovery.is_empty());
        if kind == "old_hot" {
            assert_eq!(&recovery[..8], HOT_JOURNAL_MAGIC);
        }
        #[cfg(unix)]
        fs::set_permissions(path.parent().unwrap(), Permissions::from_mode(0o750)).unwrap();
        let before = journal_snapshot(&path);
        for _ in 0..2 {
            let result = RemoteOperationJournal::open(&state);
            match version {
                Some(expected) => {
                    let Err(RemoteOperationJournalError::UnsupportedVersion {
                        path: reported,
                        found,
                        supported,
                    }) = result
                    else {
                        panic!("{UNSUPPORTED_VERSION_EXPECTED}");
                    };
                    assert_eq!(found, expected);
                    assert_eq!(supported, SCHEMA_VERSION);
                    assert_eq!(reported, path);
                }
                None => assert!(matches!(
                    result,
                    Err(RemoteOperationJournalError::UnsafeStorage(_))
                )),
            }
            assert_eq!(journal_snapshot(&path), before);
        }
    }

    #[test_case(false; "existing_empty_file")]
    #[test_case(true; "unrelated_unversioned_database")]
    fn existing_version_zero_is_not_an_initialization_candidate(populated: bool) {
        let temp = TempDir::new().unwrap();
        let state = StateDir::from_path(temp.path().join("state"));
        let directory = state.persistent_path().join(JOURNAL_DIR);
        ensure_private_directory(&directory).unwrap();
        let path = directory.join(REMOTE_OPERATION_JOURNAL_FILE);
        create_owner_only(&path).unwrap();
        if populated {
            let connection = Connection::open(&path).unwrap();
            connection.execute_batch(
                "CREATE TABLE unrelated(value TEXT); INSERT INTO unrelated VALUES ('retained');"
            ).unwrap();
        }
        let before = journal_snapshot(&path);
        let Err(RemoteOperationJournalError::UnsupportedVersion {
            path: reported,
            found,
            supported,
        }) = RemoteOperationJournal::open(&state)
        else {
            panic!("{UNSUPPORTED_VERSION_EXPECTED}");
        };
        assert_eq!(found, 0);
        assert_eq!(supported, SCHEMA_VERSION);
        assert_eq!(reported, path);
        assert_eq!(journal_snapshot(&path), before);
    }

    #[test_case(false, false; "current_committed_wal")]
    #[test_case(false, true; "current_committed_wal_without_shm")]
    #[test_case(true, false; "current_hot_rollback")]
    fn current_crash_state_recovers_after_side_effect_free_preflight(
        rollback: bool,
        missing_shm: bool,
    ) {
        let temp = TempDir::new().unwrap();
        let state = StateDir::from_path(temp.path().join("state"));
        let mut journal = RemoteOperationJournal::open(&state).unwrap();
        journal
            .reserve_before_send(&reservation("retained", default_binding()))
            .unwrap();
        let path = journal.path().to_owned();
        drop(journal);
        let sql = if rollback {
            format!("PRAGMA journal_mode=DELETE; {SPILL_TRANSACTION}")
        } else {
            "PRAGMA wal_autocheckpoint=0; UPDATE remote_operations SET updated_at=20;".into()
        };
        crash_journal(&path, &sql);
        if missing_shm {
            fs::remove_file(sidecar(&path, "-shm")).unwrap();
        }
        let recovery =
            fs::read(sidecar(&path, if rollback { "-journal" } else { "-wal" })).unwrap();
        assert!(!recovery.is_empty());
        if rollback {
            assert_eq!(&recovery[..8], HOT_JOURNAL_MAGIC);
        }
        let before = journal_snapshot(&path);
        JournalAdmission::inspect(&path).unwrap();
        assert_eq!(journal_snapshot(&path), before);
        let journal = RemoteOperationJournal::open(&state).unwrap();
        let records = journal.list_pending(&default_binding()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].operation_id,
            OperationId::new("operation-retained").unwrap()
        );
        assert_eq!(records[0].updated_at, if rollback { 10 } else { 20 });
        validate_schema(&journal.connection, journal.path()).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            verify_owner_only(&fs::metadata(sidecar(&path, suffix)).unwrap()).unwrap();
        }
    }

    #[test]
    fn exclusive_creation_does_not_adopt_a_racing_empty_file() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join(REMOTE_OPERATION_JOURNAL_FILE);
        let admission = JournalAdmission::inspect(&path).unwrap();
        assert!(admission.is_new());
        create_owner_only(&path).unwrap();
        let before = journal_snapshot(&path);
        assert!(matches!(
            admission.recheck(&path),
            Err(RemoteOperationJournalError::AdmissionChanged)
        ));
        assert!(matches!(create_owner_only(&path),
            Err(RemoteOperationJournalError::PersistentStateUnavailable(error)) if error.kind() == io::ErrorKind::AlreadyExists));
        assert_eq!(journal_snapshot(&path), before);
    }

    /// The journal records operations and never arbitrates between them: two
    /// processes reserving at the same instant, one of them a shell, both get
    /// their row.
    #[test]
    fn reservations_from_independent_connections_never_block_each_other() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("state");
        RemoteOperationJournal::open(&StateDir::from_path(path.clone())).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let tasks = [("root", SHELL_KIND), ("nested", WRITE_KIND)]
            .into_iter()
            .map(|(cursor, kind)| {
                let path = path.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    let mut journal =
                        RemoteOperationJournal::open(&StateDir::from_path(path)).unwrap();
                    let mut reservation = reservation(
                        cursor,
                        binding("source", "authority", "principal", "project", cursor),
                    );
                    reservation.operation_kind = kind.into();
                    barrier.wait();
                    journal.reserve_before_send(&reservation).unwrap();
                })
            })
            .collect::<Vec<_>>();
        for task in tasks {
            task.join().unwrap();
        }
        let journal = RemoteOperationJournal::open(&StateDir::from_path(path)).unwrap();
        assert_eq!(journal.list_pending(&default_binding()).unwrap().len(), 2);
    }

    #[test]
    fn transfer_recovery_metadata_survives_reopen() {
        let temp = TempDir::new().unwrap();
        let state = StateDir::from_path(temp.path().join("state"));
        let mut journal = RemoteOperationJournal::open(&state).unwrap();
        let mut reserved = reservation("publication", default_binding());
        reserved.publication_id = Some(OperationId::new("publication").unwrap());
        reserved.publication_cwd = Some(WorkspacePath::new("nested").unwrap());
        journal.reserve_before_send(&reserved).unwrap();
        journal.mark_dispatched(&reserved.operation_id, 11).unwrap();
        drop(journal);
        let journal = RemoteOperationJournal::open(&state).unwrap();
        let pending = journal.list_pending(&default_binding()).unwrap();
        assert_eq!(pending[0].publication_id, reserved.publication_id);
        assert_eq!(pending[0].publication_cwd, reserved.publication_cwd);
        assert_eq!(pending[0].state, RemoteOperationState::Dispatched);
        assert_eq!(pending[0].dispatched_at, Some(11));
    }

    #[test_case(1; "v1")]
    #[test_case(2; "v2")]
    #[test_case(3; "v3")]
    #[test_case(4; "v4")]
    #[test_case(FUTURE_SCHEMA_VERSION; "future")]
    fn unsupported_journal_versions_are_rejected_without_rewriting(version: i64) {
        let temp = TempDir::new().unwrap();
        let state = StateDir::from_path(temp.path().join("state"));
        let mut journal = RemoteOperationJournal::open(&state).unwrap();
        journal
            .reserve_before_send(&reservation("retained", default_binding()))
            .unwrap();
        let path = journal.path().to_owned();
        journal
            .connection
            .execute_batch(
                "ALTER TABLE remote_operations DROP COLUMN publication_id;
             ALTER TABLE remote_operations DROP COLUMN publication_cwd;
             ALTER TABLE remote_operations DROP COLUMN host_instance_id;
             PRAGMA journal_mode = DELETE;",
            )
            .unwrap();
        journal
            .connection
            .pragma_update(None, "user_version", version)
            .unwrap();
        drop(journal);
        let before = fs::read(&path).unwrap();
        let Err(RemoteOperationJournalError::UnsupportedVersion {
            path: reported,
            found,
            supported,
        }) = RemoteOperationJournal::open(&state)
        else {
            panic!("{UNSUPPORTED_VERSION_EXPECTED}");
        };
        assert_eq!(found, version);
        assert_eq!(supported, SCHEMA_VERSION);
        assert_eq!(reported, path);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!sidecar(&path, "-wal").exists());
    }

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
            publication_cwd: None,
            host_instance_id: "test-instance".to_owned(),
            publication_id: None,
            operation_id: OperationId::new(format!("operation-{id}")).unwrap(),
            invocation_id: OperationId::new(format!("invocation-{id}")).unwrap(),
            preparation_id: OperationId::new(format!("preparation-{id}")).unwrap(),
            binding,
            operation_kind: "tool_mutation".into(),
            request_digest: RequestDigest::sha256(REQUEST_DIGEST).unwrap(),
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

    /// A recreated sandbox keeps its workspace identity under a new
    /// generation. What the old one left unresolved is still reported, with
    /// the generation that recorded it, and never stands in the new one's way.
    #[test]
    fn pending_operations_of_an_earlier_generation_are_listed_and_block_nothing() {
        let temp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let previous = binding_with_generation(
            "source",
            "authority",
            "principal",
            "project",
            "cursor",
            PREVIOUS_GENERATION,
        );
        let mut journal = RemoteOperationJournal::open(&state_dir).unwrap();
        let earlier = reservation("earlier", previous);
        journal.reserve_before_send(&earlier).unwrap();
        journal.mark_dispatched(&earlier.operation_id, 11).unwrap();
        let current = default_binding();
        let mut later = reservation("current", current.clone());
        later.created_at = earlier.created_at + 1;

        journal.reserve_before_send(&later).unwrap();

        let pending = journal.list_pending(&current).unwrap();
        let generations = pending
            .iter()
            .map(|record| {
                (
                    record.operation_id.as_str(),
                    record.binding.workspace_generation(),
                    record.reachable_from(&current),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            generations,
            [
                (earlier.operation_id.as_str(), PREVIOUS_GENERATION, false),
                (
                    later.operation_id.as_str(),
                    current.workspace_generation(),
                    true
                ),
            ]
        );
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

        let digest = RequestDigest::sha256(REQUEST_DIGEST).unwrap();
        assert!(!format!("{digest:?}").contains(REQUEST_DIGEST));
        assert!(RequestDigest::sha256("raw request content").is_err());

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
        oversized.operation_kind = "x".repeat(MAX_OPERATION_KIND_BYTES + 1);
        assert!(matches!(
            journal.reserve_before_send(&oversized),
            Err(RemoteOperationJournalError::LimitExceeded {
                field: "operation kind"
            })
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

    /// Reports print a stored kind as it is, so one edited outside Caudra to
    /// carry a terminal escape fails the read instead.
    #[test]
    fn a_stored_kind_is_held_to_its_bounds_on_read() {
        const ESCAPING_KIND: &str = "canonical:\u{1b}[2Jshell";
        const KIND_COLUMN: usize = 4;
        let temp = TempDir::new().unwrap();
        let state_dir = StateDir::from_path(temp.path().join("state"));
        let mut journal = RemoteOperationJournal::open(&state_dir).unwrap();
        journal
            .reserve_before_send(&reservation("escaping", default_binding()))
            .unwrap();
        journal
            .connection
            .execute(
                "UPDATE remote_operations SET operation_kind = ?1",
                [ESCAPING_KIND],
            )
            .unwrap();
        assert!(matches!(
            journal.list_pending(&default_binding()),
            Err(RemoteOperationJournalError::Sqlite(
                rusqlite::Error::FromSqlConversionFailure(KIND_COLUMN, _, _)
            ))
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
                     host_instance_id, workspace, workspace_generation, resource_namespace,
                     principal, project, cwd_handle, operation_kind, request_digest,
                     state, created_at, updated_at, side_effects_possible
                  ) SELECT 'operation-' || value, 'invocation-' || value,
                           'preparation-' || value, '{}', 'source', 'authority', 'instance',
                           'workspace', 'generation', 'namespace', 'principal', 'project',
                           'cursor', 'tool_mutation', ?2, 'failed', 1, 1, 0
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
