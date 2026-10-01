//! Git's view of what a record stores: object errors as the contract sees them, and a file's mode,
//! where only the executable bit counts.

use std::fs::Metadata;

use workcell_host_contract::{SnapshotSkipReason, UnrecordedReason, WorkspacePath};
use workcell_snapshot_store::{EntryKind, SkipReason, StoreError};

use crate::SnapshotError;

/// Git treats a file as executable when its owner may execute it.
pub(crate) const OWNER_EXECUTABLE: u32 = 0o100;
pub(crate) const PERMISSION_BITS: u32 = 0o777;
#[cfg(not(unix))]
const FILE_MODE: u32 = 0o644;

/// Anything missing or damaged in the object store fails integrity.
pub(crate) fn store_error(error: StoreError) -> SnapshotError {
    match error {
        StoreError::Missing(_) | StoreError::Corrupt(_) | StoreError::Collision => {
            SnapshotError::IntegrityFailure
        }
        StoreError::DisjointScopes => SnapshotError::InvalidRequest,
        StoreError::Io(_) | StoreError::InvalidInput(_) => SnapshotError::OperationFailed,
    }
}

#[cfg(unix)]
pub(crate) fn permission_bits(metadata: &Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & PERMISSION_BITS
}

#[cfg(not(unix))]
pub(crate) fn permission_bits(_metadata: &Metadata) -> u32 {
    FILE_MODE
}

pub(crate) fn file_kind(metadata: &Metadata) -> EntryKind {
    if permission_bits(metadata) & OWNER_EXECUTABLE == 0 {
        EntryKind::File
    } else {
        EntryKind::Executable
    }
}

/// How a snapshot tree marks a path a capture left out, so no diff ever reports it.
pub(crate) const fn walk_skip(reason: SnapshotSkipReason) -> SkipReason {
    match reason {
        SnapshotSkipReason::NestedRepository => SkipReason::NestedRepository,
        SnapshotSkipReason::Mount => SkipReason::Mount,
        SnapshotSkipReason::Special => SkipReason::Special,
        SnapshotSkipReason::Oversized => SkipReason::Oversized,
        SnapshotSkipReason::Unreadable => SkipReason::Unreadable,
        SnapshotSkipReason::Unstable => SkipReason::Unstable,
        SnapshotSkipReason::Unrepresentable => SkipReason::Unrepresentable,
    }
}

pub(crate) const fn blind_skip(reason: UnrecordedReason) -> SkipReason {
    match reason {
        UnrecordedReason::Oversized => SkipReason::Oversized,
        UnrecordedReason::Unstable => SkipReason::Unstable,
        UnrecordedReason::Special => SkipReason::Special,
        UnrecordedReason::Unreadable
        | UnrecordedReason::Blocked
        | UnrecordedReason::Interleaved => SkipReason::Unreadable,
    }
}

/// A root-relative path of plain components, as captures record them.
pub(crate) fn valid_path(path: &str) -> bool {
    WorkspacePath::new(path).is_ok()
        && path
            .split('/')
            .all(|component| !matches!(component, "" | "." | ".."))
}

pub(crate) fn workspace_path(path: &str) -> Result<WorkspacePath, SnapshotError> {
    WorkspacePath::new(path).map_err(|_| SnapshotError::IntegrityFailure)
}
