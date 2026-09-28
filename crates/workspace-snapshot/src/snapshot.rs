//! What a stored snapshot is to the contract: `snap_` and the id of its wrapper tree, prefixed git
//! object ids as revisions, and git's view of a file's mode, where only the executable bit counts.

use std::fs::Metadata;

use workcell_host_contract::{
    DisplayText, Revision, SnapshotEntryKind, SnapshotFile, SnapshotSkipReason, SnapshotSkipped,
    SnapshotSkippedEntry, SnapshotState, SnapshotSummary, WorkspacePath,
};
use workcell_snapshot_store::{
    Entry, EntryKind, Meta, ObjectId, SkipReason, Skipped, SnapshotId, StoreError,
};

use crate::{SnapshotError, identifier, path_resource_id, revision};

pub(crate) const SNAPSHOT_ID_PREFIX: &str = "snap_";
const TREE_REVISION_PREFIX: &str = "gitoid:tree:sha1:";
const BLOB_REVISION_PREFIX: &str = "gitoid:blob:sha1:";
pub(crate) const FILE_MODE: u32 = 0o644;
pub(crate) const EXECUTABLE_MODE: u32 = 0o755;
/// Link permission bits carry no meaning, so every link reports the same mode.
pub(crate) const SYMLINK_MODE: u32 = 0o777;
/// Git treats a file as executable when its owner may execute it.
pub(crate) const OWNER_EXECUTABLE: u32 = 0o100;
pub(crate) const PERMISSION_BITS: u32 = 0o777;

pub(crate) fn snapshot_identifier(id: &SnapshotId) -> String {
    format!("{SNAPSHOT_ID_PREFIX}{id}")
}

/// The stored snapshot a contract id names. A malformed id names nothing this store could hold.
pub(crate) fn parse_snapshot_id(value: &str) -> Result<SnapshotId, SnapshotError> {
    value
        .strip_prefix(SNAPSHOT_ID_PREFIX)
        .and_then(|hex| hex.parse().ok())
        .ok_or(SnapshotError::IntegrityFailure)
}

pub(crate) fn blob_revision(oid: &ObjectId) -> Result<Revision, SnapshotError> {
    revision(&format!("{BLOB_REVISION_PREFIX}{oid}"))
}

/// One of `snapshots` missing is not found; anything else missing or damaged fails integrity.
pub(crate) fn store_error(error: StoreError, snapshots: &[&SnapshotId]) -> SnapshotError {
    match error {
        StoreError::Missing(oid) if snapshots.iter().any(|id| id.oid() == oid) => {
            SnapshotError::NotFound
        }
        StoreError::Missing(_) | StoreError::Corrupt(_) | StoreError::Collision => {
            SnapshotError::IntegrityFailure
        }
        StoreError::DisjointScopes => SnapshotError::InvalidRequest,
        StoreError::Io(_) | StoreError::InvalidInput(_) => SnapshotError::OperationFailed,
    }
}

pub(crate) fn summary(
    id: &SnapshotId,
    meta: &Meta,
    checkpoint_id: Option<&str>,
    created_at_unix_ms: u64,
) -> Result<SnapshotSummary, SnapshotError> {
    Ok(SnapshotSummary {
        snapshot_id: identifier(&snapshot_identifier(id))?,
        checkpoint_id: checkpoint_id.map(identifier).transpose()?,
        state: SnapshotState::Complete,
        manifest_revision: revision(&format!("{TREE_REVISION_PREFIX}{id}"))?,
        scope: WorkspacePath::new(meta.scope.clone())
            .map_err(|_| SnapshotError::IntegrityFailure)?,
        file_count: u32::try_from(meta.file_count).unwrap_or(u32::MAX),
        total_bytes: meta.total_bytes,
        skipped: contract_skipped(&meta.skipped)?,
        created_at_unix_ms,
    })
}

pub(crate) fn file(entry: &Entry, size_bytes: u64) -> Result<SnapshotFile, SnapshotError> {
    let kind = entry.content.kind;
    Ok(SnapshotFile {
        path: WorkspacePath::new(entry.path.clone())
            .map_err(|_| SnapshotError::IntegrityFailure)?,
        resource_id: path_resource_id(&entry.path)?,
        kind: match kind {
            EntryKind::File | EntryKind::Executable => SnapshotEntryKind::File,
            EntryKind::Symlink => SnapshotEntryKind::Symlink,
        },
        digest: blob_revision(&entry.content.oid)?,
        mode: match kind {
            EntryKind::File => FILE_MODE,
            EntryKind::Executable => EXECUTABLE_MODE,
            EntryKind::Symlink => SYMLINK_MODE,
        },
        size_bytes,
    })
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

pub(crate) const fn skip_reason(reason: SnapshotSkipReason) -> SkipReason {
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

const fn contract_reason(reason: SkipReason) -> SnapshotSkipReason {
    match reason {
        SkipReason::NestedRepository => SnapshotSkipReason::NestedRepository,
        SkipReason::Mount => SnapshotSkipReason::Mount,
        SkipReason::Special => SnapshotSkipReason::Special,
        SkipReason::Oversized => SnapshotSkipReason::Oversized,
        SkipReason::Unreadable => SnapshotSkipReason::Unreadable,
        SkipReason::Unstable => SnapshotSkipReason::Unstable,
        SkipReason::Unrepresentable => SnapshotSkipReason::Unrepresentable,
    }
}

fn contract_skipped(skipped: &Skipped) -> Result<SnapshotSkipped, SnapshotError> {
    let count = |reason| skipped.counts.get(&reason).copied().unwrap_or(0);
    Ok(SnapshotSkipped {
        nested_repositories: count(SkipReason::NestedRepository),
        mounts: count(SkipReason::Mount),
        special_files: count(SkipReason::Special),
        oversized_files: count(SkipReason::Oversized),
        unreadable_entries: count(SkipReason::Unreadable),
        unstable_files: count(SkipReason::Unstable),
        unrepresentable_names: count(SkipReason::Unrepresentable),
        samples: skipped
            .samples
            .iter()
            .map(|sample| {
                Ok(SnapshotSkippedEntry {
                    path: DisplayText::new(sample.path.clone())
                        .map_err(|_| SnapshotError::IntegrityFailure)?,
                    reason: contract_reason(sample.reason),
                })
            })
            .collect::<Result<_, SnapshotError>>()?,
    })
}

/// A root-relative path of plain components, as captures record them.
pub(crate) fn valid_path(path: &str) -> bool {
    WorkspacePath::new(path).is_ok()
        && path
            .split('/')
            .all(|component| !matches!(component, "" | "." | ".."))
}
