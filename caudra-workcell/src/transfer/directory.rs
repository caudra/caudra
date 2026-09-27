use std::{
    collections::BTreeMap,
    fs::{self, File},
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::Mutex,
    time::Instant,
};

use caudra_agent::workspace_transfer::LocalRootIdentity;
use caudra_storage::private_file::PrivateFile;
use caudra_workspace::{
    DirectoryPublicationRequest, DirectoryPublicationStatus, OperationId, PreparedLocalDirectory,
    PublishedTransferDirectory, ResourceId, TransferPublicationState, WorkspaceError,
    WorkspacePath,
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use workcell::{
    files::{BinaryError, DirectoryPublicationStaging, PreparedDirectoryPublication},
    host_contract as contract,
};

use super::{
    IO_TIMEOUT, LOCAL_TTL, LocalTransferPublisher, MAX_LOCAL_JOURNAL_BYTES, MAX_LOCAL_OUTCOMES,
    MAX_STAGES, binary_error,
};

const DIRECTORY_JOURNAL_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryOutcome {
    prepared: PreparedLocalDirectory,
    status: DirectoryPublicationStatus,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryJournal {
    version: u32,
    root: LocalRootIdentity,
    records: BTreeMap<OperationId, DirectoryOutcome>,
}

pub(super) struct DirectoryOutcomes {
    staging: DirectoryPublicationStaging,
    storage: PrivateFile,
    root: LocalRootIdentity,
    lock: Mutex<()>,
    _lease: File,
}

impl DirectoryOutcomes {
    pub(super) fn open(root: LocalRootIdentity, path: PathBuf) -> Result<Self, WorkspaceError> {
        if path.starts_with(root.canonical_path()) || root.canonical_path().starts_with(&path) {
            return Err(WorkspaceError::PermissionDenied);
        }
        let lease = PrivateFile::new(path.with_extension("directory-owner"), 0)
            .and_then(|file| file.try_lease(true))
            .map_err(|_| WorkspaceError::PermissionDenied)?;
        let staging_root = path
            .parent()
            .ok_or(WorkspaceError::PermissionDenied)?
            .canonicalize()
            .map_err(|_| WorkspaceError::PermissionDenied)?;
        if staging_root.starts_with(root.canonical_path())
            || root.canonical_path().starts_with(&staging_root)
        {
            return Err(WorkspaceError::PermissionDenied);
        }
        if fs::metadata(&staging_root)
            .map_err(|_| WorkspaceError::PermissionDenied)?
            .dev()
            != fs::metadata(root.canonical_path())
                .map_err(|_| WorkspaceError::PermissionDenied)?
                .dev()
        {
            return Err(WorkspaceError::UnsupportedEntry);
        }
        let store = Self {
            staging: DirectoryPublicationStaging::open(&staging_root).map_err(binary_error)?,
            storage: PrivateFile::new(path, MAX_LOCAL_JOURNAL_BYTES)
                .map_err(|_| WorkspaceError::PermissionDenied)?,
            root,
            lock: Mutex::new(()),
            _lease: lease,
        };
        store.change(|records| {
            for entry in records.values_mut() {
                entry.status.state = match entry.status.state {
                    TransferPublicationState::Prepared => TransferPublicationState::Cancelled,
                    TransferPublicationState::Publishing => TransferPublicationState::Indeterminate,
                    ref other => other.clone(),
                };
            }
            Ok(())
        })?;
        Ok(store)
    }

    fn change<T>(
        &self,
        apply: impl FnOnce(&mut BTreeMap<OperationId, DirectoryOutcome>) -> Result<T, WorkspaceError>,
    ) -> Result<T, WorkspaceError> {
        let _guard = self.lock.lock().map_err(|_| WorkspaceError::Unavailable)?;
        let snapshot = self
            .storage
            .load()
            .map_err(|_| WorkspaceError::Unavailable)?;
        let mut journal = match &snapshot.data {
            Some(bytes) => serde_json::from_slice::<DirectoryJournal>(bytes)
                .map_err(|_| WorkspaceError::Unavailable)?,
            None => DirectoryJournal {
                version: DIRECTORY_JOURNAL_VERSION,
                root: self.root.clone(),
                records: BTreeMap::new(),
            },
        };
        if journal.version != DIRECTORY_JOURNAL_VERSION
            || journal.root != self.root
            || journal.records.len() > MAX_LOCAL_OUTCOMES
            || journal.records.iter().any(|(id, record)| {
                id != &record.prepared.request.publication_id
                    || id != &record.status.publication_id
                    || ((record.status.state == TransferPublicationState::Completed)
                        != record.status.directory.is_some())
            })
        {
            return Err(WorkspaceError::Unavailable);
        }
        let result = apply(&mut journal.records)?;
        let bytes = serde_json::to_vec(&journal).map_err(|_| WorkspaceError::Unavailable)?;
        self.storage
            .compare_exchange(&snapshot.revision, Some(&bytes))
            .map_err(|_| WorkspaceError::IndeterminateOutcome)?;
        Ok(result)
    }

    fn status(
        &self,
        prepared: &PreparedLocalDirectory,
    ) -> Result<DirectoryPublicationStatus, WorkspaceError> {
        self.change(
            |records| match records.get(&prepared.request.publication_id) {
                Some(record) if record.prepared == *prepared => Ok(record.status.clone()),
                Some(_) => Err(WorkspaceError::Conflict),
                None => Ok(DirectoryPublicationStatus {
                    publication_id: prepared.request.publication_id.clone(),
                    state: TransferPublicationState::Unknown,
                    directory: None,
                }),
            },
        )
    }
}

pub(super) struct LocalDirectoryPreparation {
    prepared: PreparedLocalDirectory,
    native: PreparedDirectoryPublication,
    expires: Instant,
}

impl LocalTransferPublisher {
    fn validate_directory_root(&self) -> Result<(), WorkspaceError> {
        if LocalRootIdentity::capture(self.root.canonical_path())
            .map_err(|_| WorkspaceError::Conflict)?
            != self.root
        {
            return Err(WorkspaceError::Conflict);
        }
        Ok(())
    }

    pub(super) async fn prepare_local_directory(
        &self,
        request: &DirectoryPublicationRequest,
    ) -> Result<PreparedLocalDirectory, WorkspaceError> {
        self.validate_directory_root()?;
        let outcomes = self
            .directory_outcomes
            .as_ref()
            .ok_or(WorkspaceError::Unavailable)?;
        self.authorization.authorize_directory(request).await?;
        let permit = self.staging.io()?;
        let files = self.files.clone();
        let cwd = self.cwd.clone();
        let path = contract::WorkspacePath::new(request.path.as_str())
            .map_err(|_| WorkspaceError::PermissionDenied)?;
        let ancestors = request
            .create_directories
            .iter()
            .map(|path| {
                contract::WorkspacePath::new(path.as_str())
                    .map_err(|_| WorkspaceError::PermissionDenied)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let cancellation = CancellationToken::new();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let directory_outcomes = outcomes.clone();
        let native = self
            .runtime
            .spawn(async move {
                let _permit = permit;
                files
                    .prepare_directory_publication(
                        &directory_outcomes.staging,
                        &cwd,
                        &path,
                        contract::TransferPrecondition::MustNotExist {},
                        ancestors,
                        &cancellation,
                    )
                    .await
            })?
            .await
            .map_err(|_| WorkspaceError::Unavailable)?
            .map_err(binary_error)?;
        let prepared = PreparedLocalDirectory {
            request: request.clone(),
        };
        let mut entries = self
            .directory_preparations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?;
        entries.retain(|_, entry| entry.expires > Instant::now());
        if entries.len() >= MAX_STAGES as usize {
            return Err(WorkspaceError::TransferQuota);
        }
        outcomes.change(|records| {
            if records.contains_key(&request.publication_id) {
                return Err(WorkspaceError::Conflict);
            }
            if records.len() >= MAX_LOCAL_OUTCOMES {
                return Err(WorkspaceError::TransferQuota);
            }
            records.insert(
                request.publication_id.clone(),
                DirectoryOutcome {
                    prepared: prepared.clone(),
                    status: DirectoryPublicationStatus {
                        publication_id: request.publication_id.clone(),
                        state: TransferPublicationState::Prepared,
                        directory: None,
                    },
                },
            );
            Ok(())
        })?;
        entries.insert(
            request.publication_id.clone(),
            LocalDirectoryPreparation {
                prepared: prepared.clone(),
                native,
                expires: Instant::now() + LOCAL_TTL,
            },
        );
        Ok(prepared)
    }

    pub(super) async fn execute_local_directory(
        &self,
        prepared: &PreparedLocalDirectory,
    ) -> Result<DirectoryPublicationStatus, WorkspaceError> {
        self.validate_directory_root()?;
        self.authorization
            .authorize_directory(&prepared.request)
            .await?;
        let outcomes = self
            .directory_outcomes
            .clone()
            .ok_or(WorkspaceError::Unavailable)?;
        let permit = self.staging.io()?;
        let entry = {
            let mut entries = self
                .directory_preparations
                .lock()
                .map_err(|_| WorkspaceError::Unavailable)?;
            if entries
                .get(&prepared.request.publication_id)
                .is_none_or(|entry| entry.prepared != *prepared || entry.expires <= Instant::now())
            {
                return Err(WorkspaceError::Conflict);
            }
            entries
                .remove(&prepared.request.publication_id)
                .ok_or(WorkspaceError::Conflict)?
        };
        outcomes.change(|records| {
            let record = records
                .get_mut(&prepared.request.publication_id)
                .ok_or(WorkspaceError::Conflict)?;
            if record.prepared != *prepared
                || record.status.state != TransferPublicationState::Prepared
            {
                return Err(WorkspaceError::Conflict);
            }
            record.status.state = TransferPublicationState::Publishing;
            Ok(())
        })?;
        let files = self.files.clone();
        let cancellation = CancellationToken::new();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let prepared = prepared.clone();
        self.runtime
            .spawn(async move {
                let _permit = permit;
                let deadline = cancellation.clone();
                let monitor = tokio::spawn(async move {
                    tokio::time::sleep(IO_TIMEOUT).await;
                    deadline.cancel();
                });
                let result = files
                    .execute_directory_publication(entry.native, &cancellation)
                    .await;
                monitor.abort();
                let status = match result {
                    Ok(directory) => DirectoryPublicationStatus {
                        publication_id: prepared.request.publication_id.clone(),
                        state: TransferPublicationState::Completed,
                        directory: Some(convert_directory(directory)?),
                    },
                    Err(error) => DirectoryPublicationStatus {
                        publication_id: prepared.request.publication_id.clone(),
                        state: match error {
                            BinaryError::Indeterminate => TransferPublicationState::Indeterminate,
                            BinaryError::Cancelled => TransferPublicationState::Cancelled,
                            _ => TransferPublicationState::Failed,
                        },
                        directory: None,
                    },
                };
                outcomes.change(|records| {
                    records
                        .get_mut(&prepared.request.publication_id)
                        .ok_or(WorkspaceError::IndeterminateOutcome)?
                        .status = status.clone();
                    Ok(())
                })?;
                Ok(status)
            })?
            .await
            .map_err(|_| WorkspaceError::IndeterminateOutcome)?
    }

    pub(super) async fn local_directory_status(
        &self,
        prepared: &PreparedLocalDirectory,
    ) -> Result<DirectoryPublicationStatus, WorkspaceError> {
        self.validate_directory_root()?;
        let mut status = self
            .directory_outcomes
            .as_ref()
            .ok_or(WorkspaceError::Unavailable)?
            .status(prepared)?;
        if let Some(directory) = &status.directory {
            let files = self.files.clone();
            let cwd = self.cwd.clone();
            let directory = directory.clone();
            let valid = self
                .runtime
                .spawn(async move {
                    for (path, id) in directory
                        .created_directories
                        .iter()
                        .map(|(path, id)| (path, id))
                        .chain([(&directory.path, &directory.resource_id)])
                    {
                        let path = contract::WorkspacePath::new(path.as_str())
                            .map_err(|_| WorkspaceError::Conflict)?;
                        let snapshot = files
                            .transfer_inventory(
                                &cwd,
                                Some(path),
                                contract::TransferInventoryPolicy::default(),
                                &CancellationToken::new(),
                            )
                            .await
                            .map_err(binary_error)?;
                        if snapshot
                            .inspection
                            .and_then(|inspection| inspection.node)
                            .is_none_or(|node| {
                                node.kind != contract::TransferNodeKind::Directory
                                    || node.resource_id.as_str() != id.as_str()
                            })
                        {
                            return Ok::<_, WorkspaceError>(false);
                        }
                    }
                    Ok(true)
                })?
                .await
                .map_err(|_| WorkspaceError::Unavailable)??;
            if !valid {
                status.state = TransferPublicationState::Indeterminate;
                status.directory = None;
            }
        }
        Ok(status)
    }

    pub(super) fn release_local_directory(
        &self,
        prepared: &PreparedLocalDirectory,
    ) -> Result<(), WorkspaceError> {
        let outcomes = self
            .directory_outcomes
            .as_ref()
            .ok_or(WorkspaceError::Unavailable)?;
        let mut entries = self
            .directory_preparations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?;
        if entries
            .get(&prepared.request.publication_id)
            .is_some_and(|entry| entry.prepared != *prepared)
        {
            return Err(WorkspaceError::Conflict);
        }
        entries.remove(&prepared.request.publication_id);
        outcomes.change(|records| {
            let record = records
                .get_mut(&prepared.request.publication_id)
                .ok_or(WorkspaceError::Conflict)?;
            if record.prepared != *prepared {
                return Err(WorkspaceError::Conflict);
            }
            if record.status.state == TransferPublicationState::Prepared {
                record.status.state = TransferPublicationState::Cancelled;
            }
            Ok(())
        })
    }
}

fn convert_directory(
    directory: contract::TransferDirectory,
) -> Result<PublishedTransferDirectory, WorkspaceError> {
    Ok(PublishedTransferDirectory {
        path: WorkspacePath::new(directory.path.as_str())
            .map_err(|_| WorkspaceError::IndeterminateOutcome)?,
        resource_id: ResourceId::new(directory.resource_id.as_str())
            .map_err(|_| WorkspaceError::IndeterminateOutcome)?,
        created_directories: directory
            .created_directories
            .into_iter()
            .map(|(path, id)| {
                Ok((
                    WorkspacePath::new(path.as_str())
                        .map_err(|_| WorkspaceError::IndeterminateOutcome)?,
                    ResourceId::new(id.as_str())
                        .map_err(|_| WorkspaceError::IndeterminateOutcome)?,
                ))
            })
            .collect::<Result<_, WorkspaceError>>()?,
    })
}
