use caudra_storage::private_file::{FileRevision, PrivateFile};
use caudra_workspace::{
    DirectoryPublicationRequest, OperationId, PreparedDirectoryPublication, PreparedLocalDirectory,
    PreparedLocalTransfer, PreparedTransferPublication, RemoteTransferStage, ResourceRevision,
    TransferContent, TransferDigest, WorkspacePath,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

use super::{
    ApprovedParent, FileStamp, NodeKind, PlannedDirectory, PlannedFile, Side, TransferAction,
    TransferError, TransferPlan, TransferRoots, byte_digest, digest,
};

const JOURNAL_VERSION: u32 = 3;
const FILE_JOURNAL_VERSION: u32 = 2;
pub(super) const ROTATE_RECORDS: usize = 64;
const MAX_BASE_RECORDS: usize = 16_384;
const MAX_JOURNAL_BYTES: usize = 8 * 1024 * 1024;
const ARCHIVE_EXTENSION: &str = "archive";
pub(super) const MAX_RECORDS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalState {
    Reserved,
    Prepared,
    Dispatched,
    Confirmed,
    Failed,
    Cancelled,
    Unknown,
}

impl JournalState {
    pub fn blocks(&self) -> bool {
        matches!(
            self,
            Self::Reserved | Self::Prepared | Self::Dispatched | Self::Unknown
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    #[serde(default)]
    pub directory: Option<PlannedDirectory>,
    #[serde(default)]
    pub remote_directory: Option<PreparedDirectoryPublication>,
    #[serde(default)]
    pub local_directory: Option<PreparedLocalDirectory>,
    pub operation_id: OperationId,
    pub plan_digest: TransferDigest,
    pub filter_digest: TransferDigest,
    pub roots: TransferRoots,
    pub path: WorkspacePath,
    pub action: TransferAction,
    pub local: Option<FileStamp>,
    pub remote: Option<FileStamp>,
    pub state: JournalState,
    pub stage: Option<RemoteTransferStage>,
    pub remote_preparation: Option<PreparedTransferPublication>,
    pub local_preparation: Option<OperationId>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub local_review: Option<PreparedLocalTransfer>,
    pub created_directories: Vec<ApprovedParent>,
    pub cleanup_pending: bool,
}

impl JournalEntry {
    fn valid_effect(&self) -> bool {
        let Some(directory) = &self.directory else {
            return self.remote_directory.is_none()
                && self.local_directory.is_none()
                && self.source().is_ok_and(|source| {
                    source.node.kind == NodeKind::File && source.node.path == self.path
                });
        };
        let side = if self.action == TransferAction::Pull {
            Side::Local
        } else {
            Side::Remote
        };
        let valid_request = |request: &DirectoryPublicationRequest| {
            request.publication_id == self.operation_id
                && request.path == self.path
                && request
                    .create_directories
                    .iter()
                    .all(|path| directory.create_directories.contains(path))
        };
        directory.operation_id == self.operation_id
            && directory.path == self.path
            && directory.source.path == self.path
            && directory.source.kind == NodeKind::Directory
            && directory.directory_side == side
            && self.local.is_none()
            && self.remote.is_none()
            && self.stage.is_none()
            && self.remote_preparation.is_none()
            && self.local_preparation.is_none()
            && self.local_review.is_none()
            && self
                .local_directory
                .as_ref()
                .is_none_or(|prepared| side == Side::Local && valid_request(&prepared.request))
            && self.remote_directory.as_ref().is_none_or(|prepared| {
                side == Side::Remote
                    && valid_request(&prepared.request)
                    && prepared.binding == self.roots.remote.binding
                    && prepared.cursor == self.roots.remote.cursor
                    && prepared.cwd_path == self.roots.remote.cwd
            })
    }

    fn terminal(&self) -> bool {
        !self.state.blocks() && !self.cleanup_pending
    }

    pub(super) fn source(&self) -> Result<&FileStamp, TransferError> {
        match self.action {
            TransferAction::Seed | TransferAction::Push => self.local.as_ref(),
            TransferAction::Pull => self.remote.as_ref(),
        }
        .ok_or(TransferError::Journal)
    }

    fn overlaps(&self, other: &Self) -> bool {
        let local_path =
            |entry: &Self| entry.roots.local.canonical_path().join(entry.path.as_str());
        let remote_path = |entry: &Self| {
            let cwd = &entry.roots.remote.cwd;
            if cwd.is_root() {
                entry.path.to_string()
            } else {
                format!("{cwd}/{}", entry.path)
            }
        };
        let left = &self.roots.remote.binding;
        let right = &other.roots.remote.binding;
        local_path(self).starts_with(local_path(other))
            || local_path(other).starts_with(local_path(self))
            || (left.authority() == right.authority()
                && left.project() == right.project()
                && (PathBuf::from(remote_path(self)).starts_with(remote_path(other))
                    || PathBuf::from(remote_path(other)).starts_with(remote_path(self))))
    }
}

/// Last confirmed bytes on both sides, not a claim that either workspace is still unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseEntry {
    pub roots: TransferRoots,
    pub filter_digest: TransferDigest,
    pub path: WorkspacePath,
    pub content: TransferContent,
    pub local_revision: ResourceRevision,
    pub remote_revision: ResourceRevision,
    pub operation_id: OperationId,
}

/// Counts of archived terminal records. Unresolved and recent records remain in `entries`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalAudit {
    pub pages: u64,
    pub confirmed: u64,
    pub failed: u64,
    pub cancelled: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveHead {
    page: TransferDigest,
    audit: JournalAudit,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    plan_digest: TransferDigest,
    state: JournalState,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchivePage {
    version: u32,
    namespace: String,
    sequence: u64,
    previous: Option<TransferDigest>,
    receipts: BTreeMap<OperationId, Receipt>,
    base: BTreeMap<String, BaseEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalData {
    version: u32,
    entries: BTreeMap<OperationId, JournalEntry>,
    base: BTreeMap<String, BaseEntry>,
    archives: BTreeMap<String, ArchiveHead>,
}

impl Default for JournalData {
    fn default() -> Self {
        Self {
            version: JOURNAL_VERSION,
            entries: BTreeMap::new(),
            base: BTreeMap::new(),
            archives: BTreeMap::new(),
        }
    }
}

/// Client-owned, owner-only CAS storage. It contains identities and operation selectors, never
/// file bytes, previews, bearer credentials, or replayable HTTP requests. Each state/base update
/// is one durable atomic replacement. A failed durability acknowledgement remains blocked.
pub struct TransferJournal {
    storage: PrivateFile,
    #[cfg(test)]
    pub(super) rotation_records: usize,
}

impl TransferJournal {
    /// Does not create a file or directory. Keep this path outside the transferred workspace.
    pub fn new(path: PathBuf) -> Result<Self, TransferError> {
        Ok(Self {
            storage: PrivateFile::new(path, MAX_JOURNAL_BYTES)?,
            #[cfg(test)]
            rotation_records: ROTATE_RECORDS,
        })
    }

    fn rotation_records(&self) -> usize {
        #[cfg(test)]
        {
            self.rotation_records
        }
        #[cfg(not(test))]
        {
            ROTATE_RECORDS
        }
    }

    pub fn entries(&self) -> Result<Vec<JournalEntry>, TransferError> {
        let snapshot = self.storage.load()?;
        Ok(Self::decode(snapshot.data.as_deref())?
            .entries
            .into_values()
            .collect())
    }

    pub fn base(&self) -> Result<Vec<BaseEntry>, TransferError> {
        let snapshot = self.storage.load()?;
        let data = Self::decode(snapshot.data.as_deref())?;
        let mut base = BTreeMap::new();
        for (namespace, head) in &data.archives {
            base.extend(
                self.archive_page(namespace, &head.page, head.audit.pages)?
                    .base,
            );
        }
        merge_base(&mut base, data.base)?;
        Ok(base.into_values().collect())
    }

    pub fn audit(&self) -> Result<BTreeMap<String, JournalAudit>, TransferError> {
        let snapshot = self.storage.load()?;
        Ok(Self::decode(snapshot.data.as_deref())?
            .archives
            .into_iter()
            .map(|(namespace, head)| (namespace, head.audit))
            .collect())
    }

    fn decode(data: Option<&[u8]>) -> Result<JournalData, TransferError> {
        let data: JournalData = match data {
            Some(data) => serde_json::from_slice(data).map_err(|_| TransferError::Journal)?,
            None => JournalData::default(),
        };
        if !matches!(data.version, JOURNAL_VERSION | FILE_JOURNAL_VERSION)
            || (data.version == FILE_JOURNAL_VERSION
                && data.entries.values().any(|entry| {
                    entry.directory.is_some()
                        || entry.remote_directory.is_some()
                        || entry.local_directory.is_some()
                }))
            || data.entries.len() > MAX_RECORDS
            || data.base.len() > MAX_RECORDS
            || data
                .entries
                .iter()
                .any(|(id, entry)| id != &entry.operation_id || !entry.valid_effect())
        {
            return Err(TransferError::Journal);
        }
        Ok(data)
    }

    fn change<T>(
        &self,
        apply: impl FnOnce(&mut JournalData) -> Result<T, TransferError>,
    ) -> Result<T, TransferError> {
        let snapshot = self.storage.load()?;
        let mut data = Self::decode(snapshot.data.as_deref())?;
        data.version = JOURNAL_VERSION;
        let result = apply(&mut data)?;
        let mut bytes = serde_json::to_vec(&data).map_err(|_| TransferError::Journal)?;
        if bytes.len() > MAX_JOURNAL_BYTES {
            self.compact(&mut data)?;
            bytes = serde_json::to_vec(&data).map_err(|_| TransferError::Journal)?;
        }
        if bytes.len() > MAX_JOURNAL_BYTES {
            return Err(TransferError::Quota);
        }
        self.storage
            .compare_exchange(&snapshot.revision, Some(&bytes))?;
        Ok(result)
    }

    fn archive_file(
        &self,
        namespace: &str,
        page: &TransferDigest,
    ) -> Result<PrivateFile, TransferError> {
        let namespace =
            TransferDigest::new(namespace.to_owned()).map_err(|_| TransferError::Journal)?;
        Ok(PrivateFile::new(
            self.storage
                .path()
                .with_extension(ARCHIVE_EXTENSION)
                .join(namespace.as_str().trim_start_matches("sha256:"))
                .join(page.as_str().trim_start_matches("sha256:")),
            MAX_JOURNAL_BYTES,
        )?)
    }

    fn archive_page(
        &self,
        namespace: &str,
        id: &TransferDigest,
        sequence: u64,
    ) -> Result<ArchivePage, TransferError> {
        let bytes = self
            .archive_file(namespace, id)?
            .load()?
            .data
            .ok_or(TransferError::Journal)?;
        if byte_digest(&bytes)? != *id {
            return Err(TransferError::Journal);
        }
        let page: ArchivePage =
            serde_json::from_slice(&bytes).map_err(|_| TransferError::Journal)?;
        if !matches!(page.version, JOURNAL_VERSION | FILE_JOURNAL_VERSION)
            || page.namespace != namespace
            || sequence == 0
            || page.sequence != sequence
            || page.previous.is_some() != (sequence > 1)
            || page.receipts.len() > MAX_RECORDS
            || page.base.len() > MAX_BASE_RECORDS
            || page.receipts.values().any(|receipt| receipt.state.blocks())
        {
            return Err(TransferError::Journal);
        }
        for base in page.base.values() {
            if namespace_key(&base.roots)? != namespace {
                return Err(TransferError::Journal);
            }
        }
        Ok(page)
    }

    fn archived_receipt(
        &self,
        data: &JournalData,
        entry: &JournalEntry,
    ) -> Result<Option<Receipt>, TransferError> {
        let namespace = namespace_key(&entry.roots)?;
        let Some(head) = data.archives.get(&namespace) else {
            return Ok(None);
        };
        let mut next = Some(head.page.clone());
        let mut sequence = head.audit.pages;
        while let Some(id) = next {
            let mut page = self.archive_page(&namespace, &id, sequence)?;
            if let Some(receipt) = page.receipts.remove(&entry.operation_id) {
                return Ok(Some(receipt));
            }
            next = page.previous;
            sequence -= 1;
        }
        Ok(None)
    }

    fn compact(&self, data: &mut JournalData) -> Result<(), TransferError> {
        let mut groups: BTreeMap<String, Vec<OperationId>> = BTreeMap::new();
        for entry in data.entries.values().filter(|entry| entry.terminal()) {
            groups
                .entry(namespace_key(&entry.roots)?)
                .or_default()
                .push(entry.operation_id.clone());
        }
        let mut bases: BTreeMap<String, BTreeMap<String, BaseEntry>> = BTreeMap::new();
        for (key, base) in &data.base {
            let namespace = namespace_key(&base.roots)?;
            groups.entry(namespace.clone()).or_default();
            bases
                .entry(namespace)
                .or_default()
                .insert(key.clone(), base.clone());
        }
        for (namespace, ids) in groups {
            let head = data.archives.get(&namespace);
            let mut audit = head.map(|head| head.audit.clone()).unwrap_or_default();
            let mut base = match head {
                Some(head) => self.archive_page(&namespace, &head.page, audit.pages)?.base,
                None => BTreeMap::new(),
            };
            merge_base(&mut base, bases.remove(&namespace).unwrap_or_default())?;
            if base.len() > MAX_BASE_RECORDS {
                return Err(TransferError::Quota);
            }
            let mut receipts = BTreeMap::new();
            for id in ids {
                let entry = data.entries.remove(&id).ok_or(TransferError::Journal)?;
                let count = match entry.state {
                    JournalState::Confirmed => &mut audit.confirmed,
                    JournalState::Failed => &mut audit.failed,
                    JournalState::Cancelled => &mut audit.cancelled,
                    _ => return Err(TransferError::Journal),
                };
                *count = count.checked_add(1).ok_or(TransferError::Quota)?;
                receipts.insert(
                    id,
                    Receipt {
                        plan_digest: entry.plan_digest,
                        state: entry.state,
                    },
                );
            }
            audit.pages = audit.pages.checked_add(1).ok_or(TransferError::Quota)?;
            let page = ArchivePage {
                version: JOURNAL_VERSION,
                namespace: namespace.clone(),
                sequence: audit.pages,
                previous: head.map(|head| head.page.clone()),
                receipts,
                base,
            };
            let bytes = serde_json::to_vec(&page).map_err(|_| TransferError::Journal)?;
            if bytes.len() > MAX_JOURNAL_BYTES {
                return Err(TransferError::Quota);
            }
            let id = byte_digest(&bytes)?;
            let storage = self.archive_file(&namespace, &id)?;
            let snapshot = storage.load()?;
            if snapshot.revision == FileRevision::Missing {
                storage.compare_exchange(&snapshot.revision, Some(&bytes))?;
            } else if snapshot.data.as_deref() != Some(bytes.as_slice()) {
                return Err(TransferError::Journal);
            }
            // An archive becomes authoritative only with the hot journal's CAS. A lost race or
            // crash may leave an unreferenced page, but cannot remove a live entry or its replay ID.
            data.archives
                .insert(namespace, ArchiveHead { page: id, audit });
        }
        data.base.clear();
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn compact_before_commit(&self, before: impl FnOnce()) -> Result<(), TransferError> {
        self.change(|data| {
            self.compact(data)?;
            before();
            Ok(())
        })
    }

    pub(super) fn reserve(
        &mut self,
        plan: &TransferPlan,
        file: &PlannedFile,
    ) -> Result<bool, TransferError> {
        let entry = JournalEntry {
            directory: None,
            remote_directory: None,
            local_directory: None,
            operation_id: file.operation_id.clone(),
            plan_digest: plan.digest.clone(),
            filter_digest: plan.review.filter_digest.clone(),
            roots: plan.review.context.roots.clone(),
            path: file.path.clone(),
            action: plan.review.action.clone(),
            local: file.local.clone(),
            remote: file.remote.clone(),
            state: JournalState::Reserved,
            stage: None,
            remote_preparation: None,
            local_preparation: None,
            local_review: None,
            created_directories: Vec::new(),
            cleanup_pending: false,
        };
        self.reserve_entry(entry)
    }

    pub(super) fn reserve_directory(
        &mut self,
        plan: &TransferPlan,
        directory: &PlannedDirectory,
    ) -> Result<bool, TransferError> {
        self.reserve_entry(JournalEntry {
            directory: Some(directory.clone()),
            remote_directory: None,
            local_directory: None,
            operation_id: directory.operation_id.clone(),
            plan_digest: plan.digest.clone(),
            filter_digest: plan.review.filter_digest.clone(),
            roots: plan.review.context.roots.clone(),
            path: directory.path.clone(),
            action: plan.review.action.clone(),
            local: None,
            remote: None,
            state: JournalState::Reserved,
            stage: None,
            remote_preparation: None,
            local_preparation: None,
            local_review: None,
            created_directories: Vec::new(),
            cleanup_pending: false,
        })
    }

    fn reserve_entry(&mut self, entry: JournalEntry) -> Result<bool, TransferError> {
        let root = entry.roots.local.canonical_path();
        let archive_root = self.storage.path().with_extension(ARCHIVE_EXTENSION);
        if self.storage.path().starts_with(root)
            || archive_root.starts_with(root)
            || root.starts_with(&archive_root)
        {
            return Err(TransferError::Journal);
        }
        self.change(|data| {
            if let Some(previous) = data.entries.get(&entry.operation_id) {
                if previous.plan_digest == entry.plan_digest
                    && previous.state == JournalState::Confirmed
                {
                    return Ok(false);
                }
                return Err(if previous.state.blocks() {
                    TransferError::RecoveryRequired
                } else {
                    TransferError::ReviewConsumed
                });
            }
            if let Some(receipt) = self.archived_receipt(data, &entry)? {
                return if receipt.state == JournalState::Confirmed
                    && receipt.plan_digest == entry.plan_digest
                {
                    Ok(false)
                } else {
                    Err(TransferError::ReviewConsumed)
                };
            }
            if data
                .entries
                .values()
                .any(|previous| previous.state.blocks() && previous.overlaps(&entry))
            {
                return Err(TransferError::RecoveryRequired);
            }
            // Do not evict operation IDs and accidentally make an old plan replayable.
            if data.entries.len() >= self.rotation_records()
                || data.base.len() >= self.rotation_records()
            {
                self.compact(data)?;
            }
            if data.entries.len() >= MAX_RECORDS {
                return Err(TransferError::JournalQuota);
            }
            data.entries.insert(entry.operation_id.clone(), entry);
            Ok(true)
        })
    }

    pub(super) fn update(
        &mut self,
        id: &OperationId,
        apply: impl FnOnce(&mut JournalEntry) -> Result<(), TransferError>,
    ) -> Result<(), TransferError> {
        self.change(|data| apply(data.entries.get_mut(id).ok_or(TransferError::Journal)?))
    }

    pub(super) fn confirm(
        &mut self,
        id: &OperationId,
        destination_revision: ResourceRevision,
    ) -> Result<(), TransferError> {
        self.change(|data| {
            let entry = data.entries.get_mut(id).ok_or(TransferError::Journal)?;
            if entry.state == JournalState::Confirmed {
                return Ok(());
            }
            if !matches!(
                entry.state,
                JournalState::Dispatched | JournalState::Unknown
            ) {
                return Err(TransferError::Journal);
            }
            if entry.directory.is_some() {
                entry.state = JournalState::Confirmed;
                entry.cleanup_pending = true;
                return Ok(());
            }
            let source = entry.source()?;
            let (local_revision, remote_revision) = match entry.action {
                TransferAction::Seed | TransferAction::Push => {
                    (source.revision.clone(), destination_revision)
                }
                TransferAction::Pull => (destination_revision, source.revision.clone()),
            };
            let base = BaseEntry {
                roots: entry.roots.clone(),
                filter_digest: entry.filter_digest.clone(),
                path: entry.path.clone(),
                content: source.content.clone(),
                local_revision,
                remote_revision,
                operation_id: entry.operation_id.clone(),
            };
            let key = base_key(&base)?;
            merge_base(&mut data.base, BTreeMap::from([(key, base)]))?;
            entry.state = JournalState::Confirmed;
            entry.cleanup_pending = true;
            Ok(())
        })
    }
}

fn namespace_key(roots: &TransferRoots) -> Result<String, TransferError> {
    let remote = &roots.remote;
    Ok(digest(&(
        &roots.local,
        remote.binding.authority(),
        remote.binding.principal(),
        remote.binding.project(),
        &remote.cwd,
    ))?
    .as_str()
    .to_owned())
}

fn base_key(base: &BaseEntry) -> Result<String, TransferError> {
    Ok(digest(&(namespace_key(&base.roots)?, &base.path))?
        .as_str()
        .to_owned())
}

fn merge_base(
    base: &mut BTreeMap<String, BaseEntry>,
    updates: BTreeMap<String, BaseEntry>,
) -> Result<(), TransferError> {
    for (key, update) in updates {
        if key == base_key(&update)? {
            let namespace = namespace_key(&update.roots)?;
            let obsolete = base
                .iter()
                .filter(|(_, entry)| entry.path == update.path)
                .map(|(key, entry)| Ok((key.clone(), namespace_key(&entry.roots)? == namespace)))
                .collect::<Result<Vec<_>, TransferError>>()?;
            for (key, same) in obsolete {
                if same {
                    base.remove(&key);
                }
            }
        }
        base.insert(key, update);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{TransferRoots, namespace_key};
    use crate::workspace_transfer::{LocalRootIdentity, tests::remote_root};
    use caudra_workspace::{
        CwdHandle, ProjectIdentity, ProjectKey, SessionBindingId, SessionWorkspaceBinding,
        WorkspaceCursor, WorkspacePath,
    };
    use test_case::test_case;

    const GENERATION: &str = "generation-a";
    const PRINCIPAL: &str = "principal-a";
    const OTHER: &str = "other";

    #[test_case("local", false; "host_root")]
    #[test_case("generation", false; "remote_generation")]
    #[test_case("principal", false; "remote_principal")]
    #[test_case("project", false; "remote_project")]
    #[test_case("cwd", false; "remote_root")]
    #[test_case("session", true; "reconnect_keeps_namespace")]
    fn archive_namespace_tracks_authority_not_connection(variation: &str, same: bool) {
        let local = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let roots = TransferRoots {
            local: LocalRootIdentity::capture(local.path()).unwrap(),
            remote: remote_root(GENERATION, PRINCIPAL),
        };
        let mut changed = roots.clone();
        match variation {
            "local" => changed.local = LocalRootIdentity::capture(other.path()).unwrap(),
            "generation" => changed.remote = remote_root(OTHER, PRINCIPAL),
            "principal" => changed.remote = remote_root(GENERATION, OTHER),
            "cwd" => changed.remote.cwd = WorkspacePath::new(OTHER).unwrap(),
            "project" | "session" => {
                let remote = &mut changed.remote;
                remote.binding = SessionWorkspaceBinding::new(
                    SessionBindingId::new(OTHER).unwrap(),
                    remote.binding.authority().clone(),
                    remote.binding.principal().clone(),
                    if variation == "project" {
                        ProjectIdentity::new(
                            remote.binding.authority().clone(),
                            ProjectKey::new(OTHER).unwrap(),
                        )
                    } else {
                        remote.binding.project().clone()
                    },
                )
                .unwrap();
                remote.cursor = WorkspaceCursor::new(
                    &remote.binding,
                    remote.cursor.scope().clone(),
                    remote.cursor.generation() + 1,
                    CwdHandle::new(OTHER).unwrap(),
                );
            }
            _ => unreachable!(),
        }
        assert_eq!(
            namespace_key(&roots).unwrap() == namespace_key(&changed).unwrap(),
            same
        );
    }
}
