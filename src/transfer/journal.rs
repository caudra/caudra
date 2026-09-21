use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

use rustix::fs::{
    AtFlags, FlockOperation, Mode, OFlags, flock, mkdirat, open, openat, renameat, unlinkat,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use workcell_host_contract::{
    ContractVersion, Identifier, MAX_TRANSFER_JOURNAL_BYTES as MAX_JOURNAL_BYTES,
    MAX_TRANSFER_JOURNAL_STORAGE_BYTES as MAX_JOURNAL_STORAGE_BYTES, MAX_TRANSFER_JOURNALS,
    Revision, TRANSFER_OUTCOME_RETENTION_MS, TransferPublicationState, TransferStatusResponse,
};

use super::reviewed::{TransferError, hex_digest, unix_ms};

const PRIVATE_MODE: Mode = Mode::from_raw_mode(0o600);

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct Journal {
    pub status: TransferStatusResponse,
    pub cwd: String,
    pub updated_at: u64,
    pub expires_at: u64,
}

pub(super) struct JournalStore {
    pub root: PathBuf,
    directory: File,
    _lease: File,
    records: HashMap<Identifier, Journal>,
}

impl JournalStore {
    pub fn open(
        root: &Path,
        workspace: &Path,
        namespace: &Identifier,
    ) -> Result<Self, TransferError> {
        if !root.is_absolute() {
            return Err(TransferError::Storage);
        }
        let mut component_path = PathBuf::new();
        for component in root.components() {
            if matches!(component, Component::ParentDir) {
                return Err(TransferError::Storage);
            }
            component_path.push(component);
            if fs::symlink_metadata(&component_path)
                .map_err(|_| TransferError::Storage)?
                .file_type()
                .is_symlink()
            {
                return Err(TransferError::Storage);
            }
        }
        let root = root.canonicalize().map_err(|_| TransferError::Storage)?;
        if root.starts_with(workspace) || workspace.starts_with(&root) {
            return Err(TransferError::Storage);
        }
        let parent = File::from(
            open(
                &root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| TransferError::Storage)?,
        );
        validate_private(&parent, true)?;
        let name = format!(
            "transfers-{}",
            hex_digest(Sha256::digest(namespace.as_str()))
        );
        let created = match mkdirat(&parent, &name, Mode::from_raw_mode(0o700)) {
            Ok(()) => true,
            Err(rustix::io::Errno::EXIST) => false,
            Err(_) => return Err(TransferError::Storage),
        };
        let directory = File::from(
            openat(
                &parent,
                &name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| TransferError::Storage)?,
        );
        validate_private(&directory, true)?;
        parent.sync_all().map_err(|_| TransferError::Storage)?;
        let lease = File::from(
            openat(
                &directory,
                ".lock",
                (if created {
                    OFlags::CREATE | OFlags::EXCL
                } else {
                    OFlags::empty()
                }) | OFlags::RDWR
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK
                    | OFlags::CLOEXEC,
                PRIVATE_MODE,
            )
            .map_err(|_| TransferError::Storage)?,
        );
        validate_private(&lease, false)?;
        flock(&lease, FlockOperation::NonBlockingLockExclusive)
            .map_err(|_| TransferError::Storage)?;
        let mut store = Self {
            root: root.join(name),
            directory,
            _lease: lease,
            records: HashMap::new(),
        };
        let mut count = 0;
        let mut journals = Vec::new();
        let mut pending = Vec::new();
        for entry in fs::read_dir(&store.root).map_err(|_| TransferError::Storage)? {
            let entry = entry.map_err(|_| TransferError::Storage)?;
            count += 1;
            if count > MAX_TRANSFER_JOURNALS as usize * 2 + 1 {
                return Err(TransferError::Limit);
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| TransferError::Storage)?;
            if name == ".lock" {
                continue;
            }
            let is_pending = name
                .strip_prefix("pending-")
                .is_some_and(|id| Uuid::parse_str(id).is_ok());
            if !is_pending && (name.len() != 69 || !name.ends_with(".json")) {
                return Err(TransferError::Storage);
            }
            let mut file = File::from(
                openat(
                    &store.directory,
                    &name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| TransferError::Storage)?,
            );
            validate_private(&file, false)?;
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(MAX_JOURNAL_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| TransferError::Storage)?;
            if bytes.len() as u64 > MAX_JOURNAL_BYTES {
                return Err(TransferError::Limit);
            }
            let journal: Journal =
                serde_json::from_slice(&bytes).map_err(|_| TransferError::Storage)?;
            if (!is_pending && journal_name(&journal.status.publication_id) != name)
                || journal.cwd.len() > workcell_host_contract::MAX_WORKSPACE_PATH_BYTES
            {
                return Err(TransferError::Storage);
            }
            if is_pending {
                pending.push(name);
            } else {
                journals.push(journal);
            }
        }
        if journals.len() > MAX_TRANSFER_JOURNALS as usize {
            return Err(TransferError::Limit);
        }
        // Validate the entire store before recovery or cleanup can alter any existing artifact.
        for name in pending {
            unlinkat(&store.directory, &name, AtFlags::empty())
                .map_err(|_| TransferError::Storage)?;
        }
        for mut journal in journals {
            // A matching destination digest cannot prove that our rename happened. Never infer
            // success or replay a publication interrupted between the effect and its durable reply.
            if journal.status.state == TransferPublicationState::Publishing {
                journal.status.state = TransferPublicationState::Indeterminate;
            } else if journal.status.state == TransferPublicationState::Prepared {
                journal.status.state = TransferPublicationState::Cancelled;
            }
            store.put(journal)?;
        }
        store.prune()?;
        Ok(store)
    }

    pub fn reserve(
        &mut self,
        id: Identifier,
        cwd: String,
        digest: Revision,
    ) -> Result<(), TransferError> {
        self.prune()?;
        if self.records.contains_key(&id) {
            return Err(TransferError::Replay);
        }
        if self.records.len() >= MAX_TRANSFER_JOURNALS as usize {
            return Err(TransferError::Limit);
        }
        self.put(Journal {
            cwd,
            updated_at: unix_ms(),
            expires_at: unix_ms() + workcell_host_contract::TRANSFER_TTL_MS,
            status: TransferStatusResponse {
                version: ContractVersion::V1,
                publication_id: id,
                state: TransferPublicationState::Prepared,
                preparation_id: None,
                invocation_id: None,
                request_digest: Some(digest),
                file: None,
            },
        })
    }

    pub fn get(&self, id: &Identifier) -> Option<Journal> {
        self.records.get(id).cloned()
    }

    pub fn put(&mut self, journal: Journal) -> Result<(), TransferError> {
        let bytes = serde_json::to_vec(&journal).map_err(|_| TransferError::Storage)?;
        if bytes.len() as u64 > MAX_JOURNAL_BYTES
            || (!self.records.contains_key(&journal.status.publication_id)
                && (self.records.len() + 1) * MAX_JOURNAL_BYTES as usize
                    > MAX_JOURNAL_STORAGE_BYTES as usize)
        {
            return Err(TransferError::Limit);
        }
        let pending = format!("pending-{}", Uuid::new_v4());
        let mut file = File::from(
            openat(
                &self.directory,
                &pending,
                OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                PRIVATE_MODE,
            )
            .map_err(|_| TransferError::Storage)?,
        );
        let result = (|| {
            file.write_all(&bytes).map_err(|_| TransferError::Storage)?;
            file.sync_all().map_err(|_| TransferError::Storage)?;
            renameat(
                &self.directory,
                &pending,
                &self.directory,
                journal_name(&journal.status.publication_id),
            )
            .map_err(|_| TransferError::Storage)?;
            self.directory
                .sync_all()
                .map_err(|_| TransferError::Storage)
        })();
        let _ = unlinkat(&self.directory, &pending, AtFlags::empty());
        result?;
        self.records
            .insert(journal.status.publication_id.clone(), journal);
        Ok(())
    }

    fn prune(&mut self) -> Result<(), TransferError> {
        let now = unix_ms();
        let expired = self
            .records
            .iter()
            .filter(|(_, record)| {
                matches!(
                    record.status.state,
                    TransferPublicationState::Completed
                        | TransferPublicationState::Failed
                        | TransferPublicationState::Cancelled
                ) && now.saturating_sub(record.updated_at) > TRANSFER_OUTCOME_RETENTION_MS
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in expired {
            unlinkat(&self.directory, journal_name(&id), AtFlags::empty())
                .map_err(|_| TransferError::Storage)?;
            self.records.remove(&id);
        }
        self.directory
            .sync_all()
            .map_err(|_| TransferError::Storage)
    }
}

fn journal_name(id: &Identifier) -> String {
    format!("{}.json", hex_digest(Sha256::digest(id.as_str())))
}

fn validate_private(file: &File, directory: bool) -> Result<(), TransferError> {
    let metadata = file.metadata().map_err(|_| TransferError::Storage)?;
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || (directory && !metadata.is_dir())
        || (!directory && (!metadata.is_file() || metadata.nlink() != 1))
    {
        return Err(TransferError::Storage);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{JournalStore, TransferError, journal_name, unix_ms};
    use std::{fs, os::unix::fs::PermissionsExt};
    use workcell_host_contract::{
        Identifier, MAX_TRANSFER_JOURNALS, Revision, TRANSFER_OUTCOME_RETENTION_MS,
        TransferPublicationState,
    };

    #[test]
    fn incompatible_private_records_are_refused_without_recovery_or_cleanup_effects() {
        for incompatibility in [
            "version",
            "missingExpiry",
            "unknownField",
            "missingRequestDigest",
            "missingDirectoryIdentities",
        ] {
            for pending in [false, true] {
                let workspace = tempfile::tempdir().unwrap();
                let private = tempfile::tempdir().unwrap();
                fs::set_permissions(private.path(), fs::Permissions::from_mode(0o700)).unwrap();
                let namespace = Identifier::new("workspace").unwrap();
                let id = Identifier::new("publication").unwrap();
                let mut store =
                    JournalStore::open(private.path(), workspace.path(), &namespace).unwrap();
                store
                    .reserve(
                        id.clone(),
                        ".".into(),
                        Revision::new("sha256:request").unwrap(),
                    )
                    .unwrap();
                let mut incompatible = serde_json::to_value(store.get(&id).unwrap()).unwrap();
                match incompatibility {
                    "version" => incompatible["status"]["version"] = serde_json::json!("v0"),
                    "missingExpiry" => {
                        incompatible.as_object_mut().unwrap().remove("expiresAt");
                    }
                    "unknownField" => incompatible["legacy"] = serde_json::json!(true),
                    "missingRequestDigest" => {
                        incompatible["status"]
                            .as_object_mut()
                            .unwrap()
                            .remove("requestDigest");
                    }
                    "missingDirectoryIdentities" => {
                        incompatible["status"]["file"] = serde_json::json!({
                            "path":"file.bin", "resourceId":"file-identity", "revision":"file-revision",
                            "digest":"sha256:content", "sizeBytes":0, "mode":"regular"
                        })
                    }
                    _ => unreachable!(),
                }
                let retained = Identifier::new("retained").unwrap();
                store
                    .reserve(
                        retained.clone(),
                        ".".into(),
                        Revision::new("sha256:request").unwrap(),
                    )
                    .unwrap();
                let mut expired = store.get(&retained).unwrap();
                expired.status.state = TransferPublicationState::Completed;
                expired.updated_at = unix_ms() - TRANSFER_OUTCOME_RETENTION_MS - 1;
                store.put(expired).unwrap();
                let name = if pending {
                    format!("pending-{}", uuid::Uuid::new_v4())
                } else {
                    journal_name(&id)
                };
                let path = store.root.join(name);
                fs::write(&path, serde_json::to_vec(&incompatible).unwrap()).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                let root = store.root.clone();
                let snapshot = || {
                    let mut entries = fs::read_dir(&root)
                        .unwrap()
                        .map(|entry| {
                            let entry = entry.unwrap();
                            (
                                entry.file_name(),
                                fs::read(entry.path()).unwrap(),
                                entry.metadata().unwrap().modified().unwrap(),
                            )
                        })
                        .collect::<Vec<_>>();
                    entries.sort();
                    entries
                };
                let before = snapshot();
                drop(store);
                assert!(
                    matches!(
                        JournalStore::open(private.path(), workspace.path(), &namespace),
                        Err(TransferError::Storage)
                    ),
                    "{incompatibility}, pending={pending}"
                );
                assert_eq!(snapshot(), before);
            }
        }
    }

    #[test]
    fn crash_recovery_never_infers_success_from_matching_destination_bytes() {
        for applied in [false, true] {
            let workspace = tempfile::tempdir().unwrap();
            let private = tempfile::tempdir().unwrap();
            fs::set_permissions(private.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let namespace = Identifier::new("workspace-generation-principal").unwrap();
            let id = Identifier::new("publication").unwrap();
            let digest = Revision::new("sha256:request").unwrap();
            let mut store =
                JournalStore::open(private.path(), workspace.path(), &namespace).unwrap();
            assert!(matches!(
                JournalStore::open(private.path(), workspace.path(), &namespace),
                Err(TransferError::Storage)
            ));
            store
                .reserve(id.clone(), ".".into(), digest.clone())
                .unwrap();
            let mut pending = store.get(&id).unwrap();
            pending.status.state = TransferPublicationState::Publishing;
            store.put(pending).unwrap();
            if applied {
                fs::write(workspace.path().join("target"), b"sealed bytes").unwrap();
            }
            drop(store);
            let mut recovered =
                JournalStore::open(private.path(), workspace.path(), &namespace).unwrap();
            assert_eq!(
                recovered.get(&id).unwrap().status.state,
                TransferPublicationState::Indeterminate
            );
            assert!(matches!(
                recovered.reserve(id.clone(), ".".into(), digest),
                Err(TransferError::Replay)
            ));
            assert_eq!(workspace.path().join("target").exists(), applied);
        }
    }

    #[test]
    fn journal_cleanup_is_bounded_and_never_discards_unresolved_publications() {
        let workspace = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        fs::set_permissions(private.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let namespace = Identifier::new("workspace").unwrap();
        let digest = Revision::new("sha256:request").unwrap();
        let mut store = JournalStore::open(private.path(), workspace.path(), &namespace).unwrap();
        for index in 0..MAX_TRANSFER_JOURNALS {
            let id = Identifier::new(format!("publication-{index}")).unwrap();
            store
                .reserve(id.clone(), ".".into(), digest.clone())
                .unwrap();
            let mut record = store.get(&id).unwrap();
            record.status.state = TransferPublicationState::Indeterminate;
            record.updated_at = unix_ms() - TRANSFER_OUTCOME_RETENTION_MS - 1;
            store.put(record).unwrap();
        }
        let extra = Identifier::new("extra").unwrap();
        assert!(matches!(
            store.reserve(extra.clone(), ".".into(), digest.clone()),
            Err(TransferError::Limit)
        ));
        let old = Identifier::new("publication-0").unwrap();
        let mut record = store.get(&old).unwrap();
        record.status.state = TransferPublicationState::Completed;
        store.put(record).unwrap();
        store.reserve(extra, ".".into(), digest).unwrap();
        assert!(store.get(&old).is_none());
        assert_eq!(store.records.len(), MAX_TRANSFER_JOURNALS as usize);
    }
}
