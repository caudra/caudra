//! Snapshot manifests. Captures write v2. A v1 manifest from an earlier host stays readable, so its
//! checkpoints remain restorable; it is verified against the exact v1 encoding its id hashes.

use std::{cmp::Ordering, collections::BTreeSet};

use serde::{Deserialize, Serialize};
use workcell_host_contract::{
    MAX_SNAPSHOT_CAPTURE_ENTRIES, MAX_SNAPSHOT_FILE_BYTES, MAX_SNAPSHOT_FILES,
    MAX_SNAPSHOT_TOTAL_BYTES, SnapshotEntryKind, SnapshotFile, SnapshotSkipReason, SnapshotSkipped,
    SnapshotState, SnapshotSummary, WorkspacePath,
};

use crate::{
    MAX_EXCLUSIONS, SnapshotError, hex_sha256, identifier, path_resource_id, revision,
    store::DIGEST_PREFIX, valid_hex_digest,
};

pub(crate) const MANIFEST_VERSION: &str = "workspace-snapshot.v2";
const MANIFEST_VERSION_V1: &str = "workspace-snapshot.v1";
pub(crate) const ROOT_SCOPE: &str = ".";
/// Link permission bits carry no meaning, so every captured link records the same mode.
pub(crate) const SYMLINK_MODE: u32 = 0o777;
pub(crate) const PERMISSION_BITS: u32 = 0o777;
pub(crate) const MAX_MANIFEST_BYTES: u64 = 32 * 1_024 * 1_024;
pub(crate) const SNAPSHOT_ID_PREFIX: &str = "snap_";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StoredEntryKind {
    #[default]
    File,
    /// The link itself: its blob is the raw target, never followed.
    Symlink,
}

impl StoredEntryKind {
    fn is_file(&self) -> bool {
        *self == Self::File
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredEntry {
    pub(crate) path: String,
    #[serde(default, skip_serializing_if = "StoredEntryKind::is_file")]
    pub(crate) kind: StoredEntryKind,
    pub(crate) digest: String,
    pub(crate) mode: u32,
    pub(crate) size_bytes: u64,
}

/// A path a capture saw but did not record. Nothing at or beneath it is ever restored or removed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrunedEntry {
    pub(crate) path: String,
    pub(crate) reason: SnapshotSkipReason,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestContent {
    pub(crate) version: String,
    pub(crate) scope: String,
    pub(crate) entries: Vec<StoredEntry>,
    pub(crate) pruned: Vec<PrunedEntry>,
    pub(crate) skipped: SnapshotSkipped,
    pub(crate) exclusions: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile<C> {
    snapshot_id: String,
    created_at_unix_ms: u64,
    content: C,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestContentV1 {
    version: String,
    files: Vec<StoredFileV1>,
    exclusions: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredFileV1 {
    path: String,
    resource_id: String,
    identity: String,
    revision: String,
    digest: String,
    mode: u32,
    size_bytes: u64,
}

/// Reads only the version, skipping everything else without retaining it.
#[derive(Deserialize)]
struct ManifestProbe {
    content: VersionProbe,
}

#[derive(Deserialize)]
struct VersionProbe {
    version: String,
}

pub(crate) struct Manifest {
    pub(crate) snapshot_id: String,
    pub(crate) created_at_unix_ms: u64,
    pub(crate) revision: String,
    pub(crate) content: ManifestContent,
}

/// One path whose entry differs between two captures that both cover it.
pub(crate) struct Difference<'a> {
    pub(crate) path: &'a str,
    pub(crate) source: Option<&'a StoredEntry>,
    pub(crate) target: Option<&'a StoredEntry>,
}

struct Coverage<'a> {
    scope: &'a str,
    uncovered: BTreeSet<&'a str>,
}

impl StoredEntry {
    pub(crate) fn same_content(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.digest == other.digest
            && (self.kind == StoredEntryKind::Symlink || self.mode == other.mode)
    }

    pub(crate) fn contract(&self) -> Result<SnapshotFile, SnapshotError> {
        Ok(SnapshotFile {
            path: WorkspacePath::new(self.path.clone())
                .map_err(|_| SnapshotError::IntegrityFailure)?,
            resource_id: path_resource_id(&self.path)?,
            kind: match self.kind {
                StoredEntryKind::File => SnapshotEntryKind::File,
                StoredEntryKind::Symlink => SnapshotEntryKind::Symlink,
            },
            digest: revision(&self.digest)?,
            mode: self.mode,
            size_bytes: self.size_bytes,
        })
    }
}

impl Manifest {
    /// Names new content by its digest and returns the bytes to store.
    pub(crate) fn encode(
        content: ManifestContent,
        created_at_unix_ms: u64,
    ) -> Result<(Self, Vec<u8>), SnapshotError> {
        let digest = content_digest(&content)?;
        let snapshot_id = format!("{SNAPSHOT_ID_PREFIX}{digest}");
        let bytes = serde_json::to_vec(&ManifestFile {
            snapshot_id: snapshot_id.clone(),
            created_at_unix_ms,
            content: &content,
        })
        .map_err(|_| SnapshotError::OperationFailed)?;
        Ok((
            Self {
                snapshot_id,
                created_at_unix_ms,
                revision: format!("{DIGEST_PREFIX}{digest}"),
                content,
            },
            bytes,
        ))
    }

    pub(crate) fn decode(snapshot_id: &str, bytes: &[u8]) -> Result<Self, SnapshotError> {
        let probe: ManifestProbe =
            serde_json::from_slice(bytes).map_err(|_| SnapshotError::IntegrityFailure)?;
        let (stored_id, created_at_unix_ms, digest, content) = match probe.content.version.as_str()
        {
            MANIFEST_VERSION => {
                let file: ManifestFile<ManifestContent> =
                    serde_json::from_slice(bytes).map_err(|_| SnapshotError::IntegrityFailure)?;
                let digest = content_digest(&file.content)?;
                (
                    file.snapshot_id,
                    file.created_at_unix_ms,
                    digest,
                    file.content,
                )
            }
            MANIFEST_VERSION_V1 => {
                let file: ManifestFile<ManifestContentV1> =
                    serde_json::from_slice(bytes).map_err(|_| SnapshotError::IntegrityFailure)?;
                let digest = content_digest(&file.content)?;
                (
                    file.snapshot_id,
                    file.created_at_unix_ms,
                    digest,
                    upgrade(file.content)?,
                )
            }
            _ => return Err(SnapshotError::IntegrityFailure),
        };
        if stored_id != snapshot_id || stored_id != format!("{SNAPSHOT_ID_PREFIX}{digest}") {
            return Err(SnapshotError::IntegrityFailure);
        }
        validate(&content)?;
        Ok(Self {
            snapshot_id: stored_id,
            created_at_unix_ms,
            revision: format!("{DIGEST_PREFIX}{digest}"),
            content,
        })
    }

    pub(crate) fn total_bytes(&self) -> u64 {
        self.content
            .entries
            .iter()
            .map(|entry| entry.size_bytes)
            .fold(0, u64::saturating_add)
    }

    pub(crate) fn digests(&self) -> impl Iterator<Item = &str> {
        self.content
            .entries
            .iter()
            .map(|entry| entry.digest.as_str())
    }

    pub(crate) fn summary(
        &self,
        checkpoint_id: Option<&str>,
    ) -> Result<SnapshotSummary, SnapshotError> {
        Ok(SnapshotSummary {
            snapshot_id: identifier(&self.snapshot_id)?,
            checkpoint_id: checkpoint_id.map(identifier).transpose()?,
            state: SnapshotState::Complete,
            manifest_revision: revision(&self.revision)?,
            scope: WorkspacePath::new(self.content.scope.clone())
                .map_err(|_| SnapshotError::IntegrityFailure)?,
            file_count: u32::try_from(self.content.entries.len()).unwrap_or(u32::MAX),
            total_bytes: self.total_bytes(),
            skipped: self.content.skipped.clone(),
            created_at_unix_ms: self.created_at_unix_ms,
        })
    }

    fn coverage(&self) -> Coverage<'_> {
        Coverage {
            scope: &self.content.scope,
            uncovered: self
                .content
                .pruned
                .iter()
                .map(|pruned| pruned.path.as_str())
                .chain(self.content.exclusions.iter().map(String::as_str))
                .collect(),
        }
    }
}

impl Coverage<'_> {
    fn covers(&self, path: &str) -> bool {
        within(self.scope, path)
            && !self.uncovered.contains(path)
            && !ancestors(path).any(|ancestor| self.uncovered.contains(ancestor))
    }
}

/// The deeper of two nested scopes, where both captures looked, and every path both cover whose
/// entry differs between them, in path order. Captures of disjoint scopes share nothing.
pub(crate) fn differences<'a>(
    source: &'a Manifest,
    target: &'a Manifest,
) -> Result<(&'a str, Vec<Difference<'a>>), SnapshotError> {
    let (source_scope, target_scope) = (&source.content.scope, &target.content.scope);
    let scope = if within(source_scope, target_scope) {
        target_scope
    } else if within(target_scope, source_scope) {
        source_scope
    } else {
        return Err(SnapshotError::InvalidRequest);
    };
    let (source_coverage, target_coverage) = (source.coverage(), target.coverage());
    let mut sources = source.content.entries.iter().peekable();
    let mut targets = target.content.entries.iter().peekable();
    let mut differences = Vec::new();
    loop {
        let (source, target) = match (sources.peek(), targets.peek()) {
            (None, None) => break,
            (Some(_), None) => (sources.next(), None),
            (None, Some(_)) => (None, targets.next()),
            (Some(left), Some(right)) => match left.path.cmp(&right.path) {
                Ordering::Less => (sources.next(), None),
                Ordering::Greater => (None, targets.next()),
                Ordering::Equal => (sources.next(), targets.next()),
            },
        };
        let Some(path) = source.or(target).map(|entry| entry.path.as_str()) else {
            break;
        };
        if source
            .zip(target)
            .is_some_and(|(source, target)| source.same_content(target))
            || !source_coverage.covers(path)
            || !target_coverage.covers(path)
        {
            continue;
        }
        differences.push(Difference {
            path,
            source,
            target,
        });
    }
    Ok((scope, differences))
}

/// Whether `path` is `scope` or beneath it.
pub(crate) fn within(scope: &str, path: &str) -> bool {
    scope == ROOT_SCOPE
        || path
            .strip_prefix(scope)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Proper ancestors of a relative path, shallowest first.
pub(crate) fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(index, _)| &path[..index])
}

fn content_digest(content: &impl Serialize) -> Result<String, SnapshotError> {
    Ok(hex_sha256(
        &serde_json::to_vec(content).map_err(|_| SnapshotError::OperationFailed)?,
    ))
}

fn upgrade(content: ManifestContentV1) -> Result<ManifestContent, SnapshotError> {
    let entries = content
        .files
        .into_iter()
        .map(|file| {
            if path_resource_id(&file.path)?.as_str() != file.resource_id {
                return Err(SnapshotError::IntegrityFailure);
            }
            Ok(StoredEntry {
                path: file.path,
                kind: StoredEntryKind::File,
                digest: file.digest,
                mode: file.mode,
                size_bytes: file.size_bytes,
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(ManifestContent {
        version: MANIFEST_VERSION_V1.to_owned(),
        scope: ROOT_SCOPE.to_owned(),
        entries,
        pruned: Vec::new(),
        skipped: SnapshotSkipped::default(),
        exclusions: content.exclusions,
    })
}

fn validate(content: &ManifestContent) -> Result<(), SnapshotError> {
    if content.entries.len() > MAX_SNAPSHOT_FILES
        || content.pruned.len() > MAX_SNAPSHOT_CAPTURE_ENTRIES
        || content.exclusions.len() > MAX_EXCLUSIONS
        || !valid_scope(&content.scope)
    {
        return Err(SnapshotError::IntegrityFailure);
    }
    let mut total = 0_u64;
    let mut previous: Option<&str> = None;
    for entry in &content.entries {
        total = total.saturating_add(entry.size_bytes);
        if previous.is_some_and(|previous| previous >= entry.path.as_str())
            || !valid_path(&entry.path)
            || !within(&content.scope, &entry.path)
            || entry.size_bytes > MAX_SNAPSHOT_FILE_BYTES
            || total > MAX_SNAPSHOT_TOTAL_BYTES
            || entry.mode & !PERMISSION_BITS != 0
            || (entry.kind == StoredEntryKind::Symlink && entry.mode != SYMLINK_MODE)
            || !entry
                .digest
                .strip_prefix(DIGEST_PREFIX)
                .is_some_and(valid_hex_digest)
        {
            return Err(SnapshotError::IntegrityFailure);
        }
        previous = Some(&entry.path);
    }
    if content.pruned.iter().any(|pruned| {
        pruned.reason == SnapshotSkipReason::Unrepresentable || !valid_path(&pruned.path)
    }) || !content.exclusions.iter().all(|path| valid_path(path))
    {
        return Err(SnapshotError::IntegrityFailure);
    }
    Ok(())
}

fn valid_scope(scope: &str) -> bool {
    scope == ROOT_SCOPE || valid_path(scope)
}

/// A root-relative path of plain components, as captures record them.
pub(crate) fn valid_path(path: &str) -> bool {
    WorkspacePath::new(path).is_ok()
        && path
            .split('/')
            .all(|component| !matches!(component, "" | "." | ".."))
}
