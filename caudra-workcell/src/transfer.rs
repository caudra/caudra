#[cfg(unix)]
use std::{
    collections::{BTreeMap, HashMap},
    fs::Permissions,
    future::Future,
    os::unix::fs::PermissionsExt,
    time::Instant,
};
use std::{
    fmt::Write as _,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
#[cfg(unix)]
use caudra_agent::workspace_transfer::LocalRootIdentity;
#[cfg(unix)]
use caudra_storage::{id::CaudraId, private_file::PrivateFile};
#[cfg(unix)]
use caudra_workspace::{
    DirectoryPublicationRequest, DirectoryPublicationStatus, LocalTransferCondition,
    LocalTransferReview, OperationId, PreparedLocalDirectory, ResourceRevision,
};
use caudra_workspace::{
    LocalPublicationState, LocalTransferAuthorization, LocalTransferDestination, LocalTransferPath,
    LocalTransferRevision, LocalTransferService, LocalTransferSource, PreparedLocalTransfer,
    ResourceId, TransferContent, TransferDigest, TransferLimits, TransferMode, WorkspaceError,
    WorkspacePath,
};
use futures_lite::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use smol::Unblock;
#[cfg(unix)]
use tokio::runtime::{Builder, Runtime};
#[cfg(unix)]
use tokio::task::JoinHandle;
#[cfg(unix)]
use tokio_util::sync::CancellationToken;
#[cfg(unix)]
use workcell::files::{
    BinaryError, BinaryPublicationContent, FileToolGroup, PreparedBinaryPublication,
};
use workcell::host_contract as contract;

#[cfg(unix)]
mod directory;
#[cfg(unix)]
use directory::{DirectoryOutcomes, LocalDirectoryPreparation};

pub(crate) const STREAM_BUFFER_BYTES: usize = 64 * 1024;
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_RESERVED_BYTES: u64 = 512 * 1024 * 1024;
const MAX_STAGES: u32 = 32;
const MAX_IO: u32 = 4;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;
// Copy/hash, async file bridge, HTTP upload/download bridge, and transport buffering.
const IO_BUFFER_RESERVATION: u64 = 4 * STREAM_BUFFER_BYTES as u64;
#[cfg(unix)]
const LOCAL_TTL: Duration = Duration::from_secs(600);
pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(unix)]
const MAX_LOCAL_OUTCOMES: usize = 512;
#[cfg(unix)]
const MAX_LOCAL_JOURNAL_BYTES: usize = 8 * 1024 * 1024;

#[cfg(unix)]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalOutcome {
    root: LocalRootIdentity,
    prepared: PreparedLocalTransfer,
    state: LocalPublicationState,
    created_directories: Vec<(WorkspacePath, ResourceId)>,
}

#[cfg(unix)]
struct LocalOutcomes {
    storage: PrivateFile,
    root: LocalRootIdentity,
    _lease: File,
    lock: Mutex<()>,
}

#[cfg(unix)]
impl LocalOutcomes {
    fn open(root: LocalRootIdentity, path: PathBuf) -> Result<Self, WorkspaceError> {
        if path.starts_with(root.canonical_path()) || root.canonical_path().starts_with(&path) {
            return Err(WorkspaceError::PermissionDenied);
        }
        let lease = PrivateFile::new(path.with_extension("publisher-owner"), 0)
            .and_then(|file| file.try_lease(true))
            .map_err(|_| WorkspaceError::PermissionDenied)?;
        let store = Self {
            storage: PrivateFile::new(path, MAX_LOCAL_JOURNAL_BYTES)
                .map_err(|_| WorkspaceError::PermissionDenied)?,
            root,
            _lease: lease,
            lock: Mutex::new(()),
        };
        store.change(|records| {
            for entry in records.values_mut() {
                if entry.root != store.root {
                    return Err(WorkspaceError::Conflict);
                }
                entry.state = match entry.state {
                    LocalPublicationState::Publishing => LocalPublicationState::Indeterminate,
                    LocalPublicationState::Prepared => LocalPublicationState::NotPublished,
                    ref other => other.clone(),
                };
            }
            Ok(())
        })?;
        Ok(store)
    }

    fn change<T>(
        &self,
        apply: impl FnOnce(&mut BTreeMap<OperationId, LocalOutcome>) -> Result<T, WorkspaceError>,
    ) -> Result<T, WorkspaceError> {
        let _guard = self.lock.lock().map_err(|_| WorkspaceError::Unavailable)?;
        let snapshot = self
            .storage
            .load()
            .map_err(|_| WorkspaceError::Unavailable)?;
        let mut records: BTreeMap<OperationId, LocalOutcome> = match snapshot.data {
            Some(ref bytes) => {
                serde_json::from_slice(bytes).map_err(|_| WorkspaceError::Unavailable)?
            }
            None => BTreeMap::new(),
        };
        if records.len() > MAX_LOCAL_OUTCOMES
            || records.iter().any(|(id, entry)| id != &entry.prepared.id)
        {
            return Err(WorkspaceError::Unavailable);
        }
        let result = apply(&mut records)?;
        let bytes = serde_json::to_vec(&records).map_err(|_| WorkspaceError::Unavailable)?;
        self.storage
            .compare_exchange(&snapshot.revision, Some(&bytes))
            .map_err(|_| WorkspaceError::IndeterminateOutcome)?;
        Ok(result)
    }

    fn record(
        &self,
        prepared: &PreparedLocalTransfer,
        state: LocalPublicationState,
    ) -> Result<(), WorkspaceError> {
        self.change(|records| {
            if let Some(previous) = records.get(&prepared.id) {
                if previous.root != self.root || previous.prepared != *prepared {
                    return Err(WorkspaceError::Conflict);
                }
                if state == LocalPublicationState::Publishing
                    && previous.state != LocalPublicationState::Prepared
                {
                    return Err(WorkspaceError::Conflict);
                }
            } else if state != LocalPublicationState::Prepared
                || records.len() >= MAX_LOCAL_OUTCOMES
            {
                return Err(WorkspaceError::TransferQuota);
            }
            let created_directories = records
                .get(&prepared.id)
                .map(|entry| entry.created_directories.clone())
                .unwrap_or_default();
            records.insert(
                prepared.id.clone(),
                LocalOutcome {
                    root: self.root.clone(),
                    prepared: prepared.clone(),
                    state,
                    created_directories,
                },
            );
            Ok(())
        })
    }

    fn status(
        &self,
        prepared: &PreparedLocalTransfer,
    ) -> Result<LocalPublicationState, WorkspaceError> {
        self.change(|records| match records.get(&prepared.id) {
            Some(entry) if entry.root == self.root && entry.prepared == *prepared => {
                Ok(entry.state.clone())
            }
            Some(_) => Err(WorkspaceError::Conflict),
            None => Ok(LocalPublicationState::Unknown),
        })
    }
}

#[derive(Default)]
struct Usage {
    stages: u32,
    bytes: u64,
    io: u32,
}

#[derive(Clone)]
pub(crate) struct PrivateStaging {
    usage: Arc<Mutex<Usage>>,
    limits: TransferLimits,
}

impl Default for PrivateStaging {
    fn default() -> Self {
        Self::new(Self::limits())
    }
}

pub(crate) struct Reservation {
    usage: Arc<Mutex<Usage>>,
    bytes: u64,
    io: bool,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Ok(mut usage) = self.usage.lock() {
            usage.bytes -= self.bytes;
            if self.io {
                usage.io -= 1;
            } else {
                usage.stages -= 1;
            }
        }
    }
}

pub(crate) struct StagedFile {
    pub file: File,
    pub _lease: Reservation,
}

impl Read for StagedFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Write for StagedFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.file.write(buffer)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl PrivateStaging {
    pub fn new(limits: TransferLimits) -> Self {
        Self {
            usage: Arc::new(Mutex::new(Usage::default())),
            limits,
        }
    }

    pub fn limits() -> TransferLimits {
        TransferLimits {
            max_file_bytes: MAX_FILE_BYTES - IO_BUFFER_RESERVATION - STREAM_BUFFER_BYTES as u64,
            max_stages: MAX_STAGES,
            max_reserved_bytes: MAX_RESERVED_BYTES,
            max_concurrent_io: MAX_IO,
            stream_buffer_bytes: STREAM_BUFFER_BYTES as u32,
            atomic_replace_against_external_writers: false,
        }
    }

    fn reserve(&self, bytes: u64, io: bool) -> Result<Reservation, WorkspaceError> {
        let mut usage = self.usage.lock().map_err(|_| WorkspaceError::Unavailable)?;
        if usage.bytes.saturating_add(bytes) > self.limits.max_reserved_bytes
            || (io && usage.io >= self.limits.max_concurrent_io)
            || (!io && usage.stages >= self.limits.max_stages)
        {
            return Err(WorkspaceError::TransferQuota);
        }
        usage.bytes += bytes;
        if io {
            usage.io += 1;
        } else {
            usage.stages += 1;
        }
        Ok(Reservation {
            usage: self.usage.clone(),
            bytes,
            io,
        })
    }

    pub fn io(&self) -> Result<Reservation, WorkspaceError> {
        self.reserve(IO_BUFFER_RESERVATION, true)
    }

    pub async fn receive(
        &self,
        reader: &mut (impl AsyncRead + Unpin + Send + ?Sized),
        size: u64,
        expected_digest: Option<&TransferDigest>,
    ) -> Result<(StagedFile, TransferDigest), WorkspaceError> {
        if size > self.limits.max_file_bytes {
            return Err(WorkspaceError::TransferQuota);
        }
        let lease = self.reserve(
            size.checked_add(STREAM_BUFFER_BYTES as u64)
                .ok_or(WorkspaceError::TransferQuota)?,
            false,
        )?;
        let file = smol::unblock(|| {
            let file = tempfile::tempfile()?;
            #[cfg(unix)]
            file.set_permissions(Permissions::from_mode(PRIVATE_FILE_MODE))?;
            Ok::<_, io::Error>(file)
        })
        .await
        .map_err(|_| WorkspaceError::Unavailable)?;
        let mut writer = Unblock::with_capacity(
            STREAM_BUFFER_BYTES,
            StagedFile {
                file,
                _lease: lease,
            },
        );
        let mut buffer = vec![0; STREAM_BUFFER_BYTES].into_boxed_slice();
        let mut digest = Sha256::new();
        let mut received = 0u64;
        loop {
            let count = reader
                .read(&mut buffer)
                .await
                .map_err(|_| WorkspaceError::Unavailable)?;
            if count == 0 {
                break;
            }
            let Some(total) = received
                .checked_add(count as u64)
                .filter(|value| *value <= size)
            else {
                drop(writer.into_inner().await);
                return Err(WorkspaceError::TransferIntegrity);
            };
            received = total;
            digest.update(&buffer[..count]);
            writer
                .write_all(&buffer[..count])
                .await
                .map_err(|_| WorkspaceError::Unavailable)?;
        }
        let digest = encode_digest(digest.finalize())?;
        if received != size || expected_digest.is_some_and(|expected| expected != &digest) {
            drop(writer.into_inner().await);
            return Err(WorkspaceError::TransferIntegrity);
        }
        writer
            .flush()
            .await
            .map_err(|_| WorkspaceError::Unavailable)?;
        let mut staged = writer.into_inner().await;
        let staged = smol::unblock(move || {
            staged.file.sync_all()?;
            staged.file.seek(SeekFrom::Start(0))?;
            Ok::<_, io::Error>(staged)
        })
        .await
        .map_err(|_| WorkspaceError::Unavailable)?;
        Ok((staged, digest))
    }
}

pub(crate) fn encode_digest(
    bytes: impl IntoIterator<Item = u8>,
) -> Result<TransferDigest, WorkspaceError> {
    let mut value = String::from("sha256:");
    for byte in bytes {
        write!(value, "{byte:02x}").map_err(|_| WorkspaceError::Unavailable)?;
    }
    TransferDigest::new(value)
}

pub(crate) fn content(file: &contract::TransferFile) -> Result<TransferContent, WorkspaceError> {
    Ok(TransferContent {
        digest: TransferDigest::new(file.digest.as_str())?,
        size_bytes: file.size_bytes,
        mode: match file.mode {
            contract::TransferMode::Regular => TransferMode::Regular,
            contract::TransferMode::Executable => TransferMode::Executable,
        },
    })
}

pub(crate) fn mode(mode: &TransferMode) -> contract::TransferMode {
    match mode {
        TransferMode::Regular => contract::TransferMode::Regular,
        TransferMode::Executable => contract::TransferMode::Executable,
    }
}

#[cfg(unix)]
struct LocalPreparation {
    prepared: PreparedLocalTransfer,
    binary: PreparedBinaryPublication,
    source: StagedFile,
    expires: Instant,
}

#[cfg(unix)]
pub(crate) struct BinaryRuntime(Option<Runtime>);

#[cfg(unix)]
impl BinaryRuntime {
    pub(crate) fn spawn<F>(&self, future: F) -> Result<JoinHandle<F::Output>, WorkspaceError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        Ok(self
            .0
            .as_ref()
            .ok_or(WorkspaceError::Unavailable)?
            .spawn(future))
    }
}

#[cfg(unix)]
impl Drop for BinaryRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

/// Rooted Pull publication. The host must supply its own authorization policy.
/// Replacement is checked twice but is not an atomic CAS against external writers.
#[cfg(unix)]
pub struct LocalTransferPublisher {
    pub(crate) runtime: BinaryRuntime,
    pub(crate) files: FileToolGroup,
    pub(crate) cwd: contract::ResourceId,
    authorization: Arc<dyn LocalTransferAuthorization>,
    pub(crate) staging: PrivateStaging,
    pub(crate) root: LocalRootIdentity,
    maximum: u64,
    preparations: Mutex<HashMap<OperationId, LocalPreparation>>,
    outcomes: Option<Arc<LocalOutcomes>>,
    directory_outcomes: Option<Arc<DirectoryOutcomes>>,
    directory_preparations: Mutex<HashMap<OperationId, LocalDirectoryPreparation>>,
}

#[cfg(unix)]
impl LocalTransferPublisher {
    pub async fn new(
        root: PathBuf,
        authorization: Arc<dyn LocalTransferAuthorization>,
    ) -> Result<Self, WorkspaceError> {
        let identity =
            LocalRootIdentity::capture(&root).map_err(|_| WorkspaceError::PermissionDenied)?;
        let runtime = BinaryRuntime(Some(
            Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|_| WorkspaceError::Unavailable)?,
        ));
        let (files, cwd) = runtime
            .spawn(async move {
                let files = FileToolGroup::new(&root, true, None)
                    .await
                    .map_err(|_| WorkspaceError::PermissionDenied)?;
                let cwd = files
                    .workspace_root()
                    .await
                    .map_err(|_| WorkspaceError::PermissionDenied)?;
                Ok::<_, WorkspaceError>((files, cwd.handle))
            })?
            .await
            .map_err(|_| WorkspaceError::Unavailable)??;
        Ok(Self {
            runtime,
            files,
            cwd,
            authorization,
            staging: PrivateStaging::default(),
            preparations: Mutex::new(HashMap::new()),
            outcomes: None,
            directory_outcomes: None,
            directory_preparations: Mutex::new(HashMap::new()),
            root: identity,
            maximum: MAX_FILE_BYTES,
        })
    }

    pub fn with_max_file_bytes(mut self, maximum: u64) -> Result<Self, WorkspaceError> {
        if maximum == 0 || maximum > MAX_FILE_BYTES {
            return Err(WorkspaceError::TransferQuota);
        }
        self.maximum = maximum;
        Ok(self)
    }

    /// The status path is private client state outside the transferred tree. It has a
    /// single-writer lifetime lease; unresolved records are bounded and never evicted.
    pub async fn new_durable(
        root: PathBuf,
        status_path: PathBuf,
        authorization: Arc<dyn LocalTransferAuthorization>,
    ) -> Result<Self, WorkspaceError> {
        let identity =
            LocalRootIdentity::capture(&root).map_err(|_| WorkspaceError::PermissionDenied)?;
        let directory_outcomes = if cfg!(target_os = "linux") {
            match DirectoryOutcomes::open(
                identity.clone(),
                status_path.with_extension("directories.json"),
            ) {
                Ok(outcomes) => Some(Arc::new(outcomes)),
                Err(WorkspaceError::UnsupportedEntry) => None,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let outcomes = Arc::new(LocalOutcomes::open(identity.clone(), status_path)?);
        let mut publisher =
            Self::new(identity.canonical_path().to_path_buf(), authorization).await?;
        publisher.outcomes = Some(outcomes);
        publisher.directory_outcomes = directory_outcomes;
        Ok(publisher)
    }

    fn take(&self, prepared: &PreparedLocalTransfer) -> Result<LocalPreparation, WorkspaceError> {
        let mut entries = self
            .preparations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?;
        entries.retain(|_, entry| entry.expires > Instant::now());
        if entries
            .get(&prepared.id)
            .is_none_or(|entry| entry.prepared != *prepared)
        {
            return Err(WorkspaceError::Conflict);
        }
        entries.remove(&prepared.id).ok_or(WorkspaceError::Conflict)
    }
}

#[cfg(unix)]
#[async_trait]
impl LocalTransferService for LocalTransferPublisher {
    fn supports_directory_publication(&self) -> bool {
        cfg!(target_os = "linux") && self.directory_outcomes.is_some()
    }

    async fn prepare_directory(
        &self,
        request: &DirectoryPublicationRequest,
    ) -> Result<PreparedLocalDirectory, WorkspaceError> {
        self.prepare_local_directory(request).await
    }

    async fn execute_directory(
        &self,
        prepared: &PreparedLocalDirectory,
    ) -> Result<DirectoryPublicationStatus, WorkspaceError> {
        self.execute_local_directory(prepared).await
    }

    async fn directory_status(
        &self,
        prepared: &PreparedLocalDirectory,
    ) -> Result<DirectoryPublicationStatus, WorkspaceError> {
        self.local_directory_status(prepared).await
    }

    async fn release_directory(
        &self,
        prepared: &PreparedLocalDirectory,
    ) -> Result<(), WorkspaceError> {
        self.release_local_directory(prepared)
    }

    async fn created_directories(
        &self,
        prepared: &PreparedLocalTransfer,
    ) -> Result<Vec<(WorkspacePath, ResourceId)>, WorkspaceError> {
        self.outcomes
            .as_ref()
            .ok_or(WorkspaceError::Unavailable)?
            .change(|records| {
                let entry = records
                    .get(&prepared.id)
                    .ok_or(WorkspaceError::Unavailable)?;
                if entry.root != self.root || entry.prepared != *prepared {
                    return Err(WorkspaceError::Conflict);
                }
                Ok(entry.created_directories.clone())
            })
    }
    async fn publication_status(
        &self,
        prepared: &PreparedLocalTransfer,
    ) -> Result<LocalPublicationState, WorkspaceError> {
        let Some(outcomes) = &self.outcomes else {
            return Ok(LocalPublicationState::Unknown);
        };
        let root = LocalRootIdentity::capture(outcomes.root.canonical_path())
            .map_err(|_| WorkspaceError::Conflict)?;
        if root != outcomes.root {
            return Err(WorkspaceError::Conflict);
        }
        let state = outcomes.status(prepared)?;
        if matches!(
            state,
            LocalPublicationState::Completed(_) | LocalPublicationState::Indeterminate
        ) {
            match self.stat(&prepared.review.destination.path).await {
                Ok((revision, content)) if content == prepared.review.content => {
                    let completed = LocalPublicationState::Completed(revision);
                    outcomes.record(prepared, completed.clone())?;
                    return Ok(completed);
                }
                _ => return Ok(LocalPublicationState::Indeterminate),
            }
        }
        Ok(state)
    }
    async fn stat(
        &self,
        path: &LocalTransferPath,
    ) -> Result<(LocalTransferRevision, TransferContent), WorkspaceError> {
        let permit = self.staging.io()?;
        let cancellation = CancellationToken::new();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let files = self.files.clone();
        let cwd = self.cwd.clone();
        let path = contract::WorkspacePath::new(path.as_str())
            .map_err(|_| WorkspaceError::PermissionDenied)?;
        let maximum = self.maximum;
        let file = self
            .runtime
            .spawn(async move {
                let _permit = permit;
                files.open_binary(&cwd, &path, maximum, &cancellation).await
            })?
            .await
            .map_err(|_| WorkspaceError::Unavailable)?
            .map_err(binary_error)?;
        Ok((local_revision(&file.metadata)?, content(&file.metadata)?))
    }

    async fn prepare(
        &self,
        source: LocalTransferSource,
        destination: LocalTransferDestination,
        expected: TransferContent,
    ) -> Result<PreparedLocalTransfer, WorkspaceError> {
        let review = LocalTransferReview {
            destination,
            content: expected,
            atomic_replace_against_external_writers: false,
        };
        if review.content.size_bytes > self.maximum {
            return Err(WorkspaceError::TransferQuota);
        }
        self.authorization.authorize(&review).await?;
        self.preparations
            .lock()
            .map_err(|_| WorkspaceError::Unavailable)?
            .retain(|_, entry| entry.expires > Instant::now());
        let permit = self.staging.io()?;
        let cancellation = CancellationToken::new();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let work = async {
            let (source, _) = self
                .staging
                .receive(
                    &mut source.into_reader(),
                    review.content.size_bytes,
                    Some(&review.content.digest),
                )
                .await?;
            let files = self.files.clone();
            let cwd = self.cwd.clone();
            let path = contract::WorkspacePath::new(review.destination.path.as_str())
                .map_err(|_| WorkspaceError::PermissionDenied)?;
            let condition = match &review.destination.condition {
                LocalTransferCondition::MustNotExist => {
                    contract::TransferPrecondition::MustNotExist {}
                }
                LocalTransferCondition::Matches(revision) => {
                    contract::TransferPrecondition::Revision {
                        revision: contract::Revision::new(revision.0.as_str())
                            .map_err(|_| WorkspaceError::Conflict)?,
                    }
                }
            };
            let content = BinaryPublicationContent {
                digest: contract::Revision::new(review.content.digest.as_str())
                    .map_err(|_| WorkspaceError::TransferIntegrity)?,
                size_bytes: review.content.size_bytes,
                mode: mode(&review.content.mode),
            };
            let maximum = self.maximum;
            let directories = review
                .destination
                .create_directories
                .iter()
                .map(|path| {
                    contract::WorkspacePath::new(path.as_str())
                        .map_err(|_| WorkspaceError::PermissionDenied)
                })
                .collect::<Result<Vec<_>, _>>()?;
            if !directories.is_empty() && self.outcomes.is_none() {
                return Err(WorkspaceError::Unavailable);
            }
            let binary = self
                .runtime
                .spawn(async move {
                    let _permit = permit;
                    files
                        .prepare_binary_publication_with_directories(
                            &cwd,
                            &path,
                            condition,
                            content,
                            directories,
                            maximum,
                            &cancellation,
                        )
                        .await
                })?
                .await
                .map_err(|_| WorkspaceError::Unavailable)?
                .map_err(binary_error)?;
            let prepared = PreparedLocalTransfer {
                id: OperationId::new(format!("local-transfer-{}", CaudraId::generate()))
                    .map_err(|_| WorkspaceError::Unavailable)?,
                review,
            };
            if let Some(outcomes) = &self.outcomes {
                outcomes.record(&prepared, LocalPublicationState::Prepared)?;
            }
            let mut entries = self
                .preparations
                .lock()
                .map_err(|_| WorkspaceError::Unavailable)?;
            entries.retain(|_, entry| entry.expires > Instant::now());
            entries.insert(
                prepared.id.clone(),
                LocalPreparation {
                    prepared: prepared.clone(),
                    binary,
                    source,
                    expires: Instant::now() + LOCAL_TTL,
                },
            );
            Ok(prepared)
        };
        futures_lite::future::race(work, async {
            smol::Timer::after(IO_TIMEOUT).await;
            Err(WorkspaceError::Cancelled)
        })
        .await
    }

    async fn execute(
        &self,
        prepared: &PreparedLocalTransfer,
    ) -> Result<LocalTransferRevision, WorkspaceError> {
        self.authorization.authorize(&prepared.review).await?;
        let permit = self.staging.io()?;
        let entry = self.take(prepared)?;
        let outcomes = self.outcomes.clone();
        if let Some(outcomes) = &outcomes {
            outcomes.record(prepared, LocalPublicationState::Publishing)?;
        }
        let prepared = prepared.clone();
        let files = self.files.clone();
        let cancellation = CancellationToken::new();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let file = self
            .runtime
            .spawn(async move {
                let _permit = permit;
                let _lease = entry.source._lease;
                let deadline = cancellation.clone();
                let monitor = tokio::spawn(async move {
                    tokio::time::sleep(IO_TIMEOUT).await;
                    deadline.cancel();
                });
                let result = files
                    .execute_binary_publication(entry.binary, entry.source.file, &cancellation)
                    .await;
                monitor.abort();
                if let Some(outcomes) = outcomes {
                    if let Ok(file) = &result {
                        let directories = file
                            .created_directories
                            .iter()
                            .map(|(path, id)| {
                                Ok((
                                    WorkspacePath::new(path.as_str())
                                        .map_err(|_| WorkspaceError::IndeterminateOutcome)?,
                                    ResourceId::new(id.as_str())
                                        .map_err(|_| WorkspaceError::IndeterminateOutcome)?,
                                ))
                            })
                            .collect::<Result<Vec<_>, WorkspaceError>>()?;
                        outcomes.change(|records| {
                            records
                                .get_mut(&prepared.id)
                                .ok_or(WorkspaceError::IndeterminateOutcome)?
                                .created_directories = directories;
                            Ok(())
                        })?;
                    }
                    let state = match &result {
                        Ok(file) => LocalPublicationState::Completed(local_revision(file)?),
                        Err(BinaryError::Indeterminate) => LocalPublicationState::Indeterminate,
                        Err(_) => LocalPublicationState::NotPublished,
                    };
                    outcomes.record(&prepared, state)?;
                }
                result.map_err(binary_error)
            })?
            .await
            .map_err(|_| WorkspaceError::IndeterminateOutcome)??;
        local_revision(&file)
    }

    async fn release(&self, prepared: &PreparedLocalTransfer) -> Result<(), WorkspaceError> {
        self.take(prepared)?;
        if let Some(outcomes) = &self.outcomes {
            outcomes.record(prepared, LocalPublicationState::NotPublished)?;
        }
        Ok(())
    }
}

#[cfg(unix)]
pub(crate) fn local_revision(
    file: &contract::TransferFile,
) -> Result<LocalTransferRevision, WorkspaceError> {
    ResourceRevision::new(file.revision.as_str())
        .map(LocalTransferRevision)
        .map_err(|_| WorkspaceError::Unavailable)
}

#[cfg(unix)]
pub(crate) fn binary_error(error: BinaryError) -> WorkspaceError {
    match error {
        BinaryError::Inaccessible => WorkspaceError::PermissionDenied,
        BinaryError::Conflict => WorkspaceError::Conflict,
        BinaryError::Integrity => WorkspaceError::TransferIntegrity,
        BinaryError::Cancelled => WorkspaceError::Cancelled,
        BinaryError::Indeterminate => WorkspaceError::IndeterminateOutcome,
    }
}

#[cfg(not(unix))]
pub struct LocalTransferPublisher {
    _unsupported: (),
}

#[cfg(not(unix))]
impl LocalTransferPublisher {
    pub async fn new(
        _root: PathBuf,
        _authorization: Arc<dyn LocalTransferAuthorization>,
    ) -> Result<Self, WorkspaceError> {
        Err(WorkspaceError::UnsupportedEntry)
    }

    pub async fn new_durable(
        _root: PathBuf,
        _status_path: PathBuf,
        _authorization: Arc<dyn LocalTransferAuthorization>,
    ) -> Result<Self, WorkspaceError> {
        Err(WorkspaceError::UnsupportedEntry)
    }

    pub fn with_max_file_bytes(self, _maximum: u64) -> Result<Self, WorkspaceError> {
        Err(WorkspaceError::UnsupportedEntry)
    }
}

#[cfg(not(unix))]
#[async_trait]
impl LocalTransferService for LocalTransferPublisher {
    async fn created_directories(
        &self,
        _prepared: &PreparedLocalTransfer,
    ) -> Result<Vec<(WorkspacePath, ResourceId)>, WorkspaceError> {
        Err(WorkspaceError::UnsupportedEntry)
    }

    async fn publication_status(
        &self,
        _prepared: &PreparedLocalTransfer,
    ) -> Result<LocalPublicationState, WorkspaceError> {
        Err(WorkspaceError::UnsupportedEntry)
    }

    async fn stat(
        &self,
        _path: &LocalTransferPath,
    ) -> Result<(LocalTransferRevision, TransferContent), WorkspaceError> {
        Err(WorkspaceError::UnsupportedEntry)
    }

    async fn prepare(
        &self,
        _source: LocalTransferSource,
        _destination: LocalTransferDestination,
        _expected: TransferContent,
    ) -> Result<PreparedLocalTransfer, WorkspaceError> {
        Err(WorkspaceError::UnsupportedEntry)
    }

    async fn execute(
        &self,
        _prepared: &PreparedLocalTransfer,
    ) -> Result<LocalTransferRevision, WorkspaceError> {
        Err(WorkspaceError::UnsupportedEntry)
    }

    async fn release(&self, _prepared: &PreparedLocalTransfer) -> Result<(), WorkspaceError> {
        Err(WorkspaceError::UnsupportedEntry)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IO_BUFFER_RESERVATION, LocalTransferPublisher, PrivateStaging, STREAM_BUFFER_BYTES,
        encode_digest,
    };
    use async_trait::async_trait;
    #[cfg(unix)]
    use caudra_workspace::{
        DirectoryPublicationRequest, LocalPublicationState, OperationId, TransferPublicationState,
        WorkspacePath,
    };
    use caudra_workspace::{
        LocalTransferAuthorization, LocalTransferCondition, LocalTransferDestination,
        LocalTransferPath, LocalTransferReview, LocalTransferService, LocalTransferSource,
        TransferContent, TransferMode, WorkspaceError,
    };
    use futures_lite::io::Cursor;
    #[cfg(unix)]
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use test_case::test_case;
    #[cfg(unix)]
    use tokio::runtime::Builder;

    const BYTES: &[u8] = b"\xff\x00private binary content";
    const CANARY: &[u8] = b"local canary must not change";
    #[cfg(unix)]
    const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
    #[cfg(unix)]
    const NESTED_DESTINATION: &str = "new/deep/file";

    #[cfg(target_os = "linux")]
    #[test_case(0o040; "group_readable")]
    #[test_case(0o020; "group_writable")]
    #[test_case(0o010; "group_searchable")]
    #[test_case(0o004; "world_readable")]
    #[test_case(0o002; "world_writable")]
    #[test_case(0o001; "world_searchable")]
    fn durable_publisher_rejects_nonprivate_staging_parent_without_destination_effects(
        extra_permissions: u32,
    ) {
        const CANARY_PATH: &str = "canary";
        smol::block_on(async {
            let root = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            let canary = root.path().join(CANARY_PATH);
            fs::write(&canary, CANARY).unwrap();
            let identity = fs::metadata(&canary).unwrap().ino();
            fs::set_permissions(
                state.path(),
                fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE | extra_permissions),
            )
            .unwrap();

            let result = LocalTransferPublisher::new_durable(
                root.path().into(),
                state.path().join("status.json"),
                Arc::new(Authorization(AtomicBool::new(true))),
            )
            .await;

            assert!(matches!(result, Err(WorkspaceError::PermissionDenied)));
            assert_eq!(fs::read(&canary).unwrap(), CANARY);
            assert_eq!(fs::metadata(&canary).unwrap().ino(), identity);
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
            assert!(!root.path().join(NESTED_DESTINATION).exists());
        });
    }

    #[cfg(unix)]
    #[test_case(false, false; "durable_success")]
    #[test_case(true, false; "racing_destination")]
    #[test_case(false, true; "denied_execute")]
    fn standalone_directory_publication_is_reviewed_durable_and_not_replayed(
        race: bool,
        deny: bool,
    ) {
        smol::block_on(async {
            let root = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            fs::set_permissions(
                state.path(),
                fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
            )
            .unwrap();
            let status = state.path().join("status.json");
            let auth = Arc::new(Authorization(AtomicBool::new(true)));
            let publisher = LocalTransferPublisher::new_durable(
                root.path().into(),
                status.clone(),
                auth.clone(),
            )
            .await
            .unwrap();
            let request = DirectoryPublicationRequest {
                publication_id: OperationId::new("directory-publication").unwrap(),
                path: WorkspacePath::new(NESTED_DESTINATION).unwrap(),
                create_directories: ["new", "new/deep"]
                    .into_iter()
                    .map(|path| WorkspacePath::new(path).unwrap())
                    .collect(),
            };
            let prepared = publisher.prepare_directory(&request).await.unwrap();
            assert!(!root.path().join("new").exists());
            if race {
                fs::create_dir_all(root.path().join(NESTED_DESTINATION)).unwrap();
            }
            if deny {
                auth.0.store(false, Ordering::Release);
            }
            let result = publisher.execute_directory(&prepared).await;
            if deny {
                assert!(matches!(result, Err(WorkspaceError::PermissionDenied)));
                assert!(!root.path().join("new").exists());
                return;
            }
            let result = result.unwrap();
            if race {
                assert_ne!(result.state, TransferPublicationState::Completed);
                return;
            }
            assert_eq!(result.state, TransferPublicationState::Completed);
            assert!(root.path().join(NESTED_DESTINATION).is_dir());
            publisher.release_directory(&prepared).await.unwrap();
            drop(publisher);
            let publisher = LocalTransferPublisher::new_durable(root.path().into(), status, auth)
                .await
                .unwrap();
            assert_eq!(publisher.directory_status(&prepared).await.unwrap(), result);
            assert!(publisher.execute_directory(&prepared).await.is_err());
            assert!(publisher.prepare_directory(&request).await.is_err());
            fs::rename(root.path().join("new"), root.path().join("old")).unwrap();
            fs::create_dir_all(root.path().join(NESTED_DESTINATION)).unwrap();
            assert_eq!(
                publisher.directory_status(&prepared).await.unwrap().state,
                TransferPublicationState::Indeterminate
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn missing_local_directory_recovery_metadata_is_rejected_without_rewriting() {
        smol::block_on(async {
            let root = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            fs::set_permissions(
                state.path(),
                fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
            )
            .unwrap();
            let status = state.path().join("status.json");
            let auth = Arc::new(Authorization(AtomicBool::new(true)));
            let publisher = LocalTransferPublisher::new_durable(
                root.path().into(),
                status.clone(),
                auth.clone(),
            )
            .await
            .unwrap();
            let prepared = publisher
                .prepare(
                    source(),
                    destination("file", LocalTransferCondition::MustNotExist),
                    expected(),
                )
                .await
                .unwrap();
            drop(publisher);
            let mut records: Value = serde_json::from_slice(&fs::read(&status).unwrap()).unwrap();
            records[prepared.id.as_str()]
                .as_object_mut()
                .unwrap()
                .remove("created_directories");
            let before = serde_json::to_vec(&records).unwrap();
            fs::write(&status, &before).unwrap();
            assert!(matches!(
                LocalTransferPublisher::new_durable(root.path().into(), status.clone(), auth).await,
                Err(WorkspaceError::Unavailable)
            ));
            assert_eq!(fs::read(&status).unwrap(), before);
            assert!(!root.path().join("file").exists());
        });
    }

    #[cfg(unix)]
    #[test_case(false, false; "reviewed_creation_is_durable")]
    #[test_case(true, false; "racing_directory_is_not_adopted")]
    #[test_case(false, true; "symlink_is_not_followed")]
    fn reviewed_local_ancestors_are_conditional_journaled_and_never_created_during_prepare(
        race: bool,
        link: bool,
    ) {
        smol::block_on(async {
            let root = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            fs::set_permissions(
                state.path(),
                fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
            )
            .unwrap();
            let status = state.path().join("status.json");
            let auth = Arc::new(Authorization(AtomicBool::new(true)));
            let publisher = LocalTransferPublisher::new_durable(
                root.path().into(),
                status.clone(),
                auth.clone(),
            )
            .await
            .unwrap();
            let mut destination =
                destination(NESTED_DESTINATION, LocalTransferCondition::MustNotExist);
            destination.create_directories = ["new", "new/deep"]
                .into_iter()
                .map(|path| WorkspacePath::new(path).unwrap())
                .collect();
            let prepared = publisher
                .prepare(source(), destination, expected())
                .await
                .unwrap();
            assert!(!root.path().join("new").exists());
            if race {
                fs::create_dir(root.path().join("new")).unwrap();
            }
            if link {
                symlink(state.path(), root.path().join("new")).unwrap();
            }
            let result = publisher.execute(&prepared).await;
            assert_eq!(result.is_err(), race || link);
            if race || link {
                assert!(!root.path().join(NESTED_DESTINATION).exists());
                return;
            }
            let directories = publisher.created_directories(&prepared).await.unwrap();
            assert_eq!(directories.len(), 2);
            drop(publisher);
            let publisher = LocalTransferPublisher::new_durable(root.path().into(), status, auth)
                .await
                .unwrap();
            assert_eq!(
                publisher.created_directories(&prepared).await.unwrap(),
                directories
            );
            assert!(matches!(
                publisher.publication_status(&prepared).await.unwrap(),
                LocalPublicationState::Completed(_)
            ));
            assert!(publisher.execute(&prepared).await.is_err());
            assert_eq!(
                fs::read(root.path().join(NESTED_DESTINATION)).unwrap(),
                BYTES
            );
        });
    }

    #[cfg(unix)]
    #[test_case(false, false; "confirmed_after_restart")]
    #[test_case(true, false; "interrupted_after_publish_reconciles_actual_digest")]
    #[test_case(true, true; "external_change_remains_indeterminate")]
    fn durable_publication_recovery_never_replays_or_trusts_stale_content(
        interrupted: bool,
        changed: bool,
    ) {
        smol::block_on(async {
            let root = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            fs::set_permissions(
                state.path(),
                fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
            )
            .unwrap();
            let status = state.path().join("local-status.json");
            let auth = Arc::new(Authorization(AtomicBool::new(true)));
            let publisher = LocalTransferPublisher::new_durable(
                root.path().into(),
                status.clone(),
                auth.clone(),
            )
            .await
            .unwrap();
            let prepared = publisher
                .prepare(
                    source(),
                    destination("target", LocalTransferCondition::MustNotExist),
                    expected(),
                )
                .await
                .unwrap();
            assert!(
                LocalTransferPublisher::new_durable(
                    root.path().into(),
                    status.clone(),
                    auth.clone()
                )
                .await
                .is_err()
            );
            publisher.execute(&prepared).await.unwrap();
            if interrupted {
                // Model a crash after publication but before the terminal journal replacement.
                publisher
                    .outcomes
                    .as_ref()
                    .unwrap()
                    .change(|records| {
                        records.get_mut(&prepared.id).unwrap().state =
                            LocalPublicationState::Publishing;
                        Ok(())
                    })
                    .unwrap();
            }
            drop(publisher);
            if changed {
                fs::write(root.path().join("target"), CANARY).unwrap();
            }
            let reopened = LocalTransferPublisher::new_durable(root.path().into(), status, auth)
                .await
                .unwrap();
            let outcome = reopened.publication_status(&prepared).await.unwrap();
            if changed {
                assert_eq!(outcome, LocalPublicationState::Indeterminate);
                assert_eq!(fs::read(root.path().join("target")).unwrap(), CANARY);
            } else {
                assert!(matches!(outcome, LocalPublicationState::Completed(_)));
                assert_eq!(fs::read(root.path().join("target")).unwrap(), BYTES);
            }
            assert!(reopened.execute(&prepared).await.is_err());
        });
    }

    #[cfg(unix)]
    #[test_case(false; "publisher_drop_in_tokio")]
    #[test_case(true; "initialization_failure_in_tokio")]
    fn local_publisher_lifecycle_does_not_require_a_particular_caller_runtime(fail: bool) {
        let root = tempfile::tempdir().unwrap();
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let path = if fail {
                root.path().join("missing")
            } else {
                root.path().to_owned()
            };
            let result =
                LocalTransferPublisher::new(path, Arc::new(Authorization(AtomicBool::new(true))))
                    .await;
            assert_eq!(result.is_err(), fail);
        });
    }

    struct Authorization(AtomicBool);

    #[async_trait]
    impl LocalTransferAuthorization for Authorization {
        #[cfg(unix)]
        async fn authorize_directory(
            &self,
            _: &DirectoryPublicationRequest,
        ) -> Result<(), WorkspaceError> {
            if self.0.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err(WorkspaceError::PermissionDenied)
            }
        }
        async fn authorize(&self, _: &LocalTransferReview) -> Result<(), WorkspaceError> {
            if self.0.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err(WorkspaceError::PermissionDenied)
            }
        }
    }

    fn expected() -> TransferContent {
        TransferContent {
            digest: encode_digest(Sha256::digest(BYTES)).unwrap(),
            size_bytes: BYTES.len() as u64,
            mode: TransferMode::Regular,
        }
    }

    fn source() -> LocalTransferSource {
        LocalTransferSource::new(Cursor::new(BYTES))
    }
    fn destination(path: &str, condition: LocalTransferCondition) -> LocalTransferDestination {
        LocalTransferDestination {
            create_directories: Vec::new(),
            path: LocalTransferPath::new(path).unwrap(),
            condition,
        }
    }

    #[cfg(not(unix))]
    #[test_case(false; "ephemeral")]
    #[test_case(true; "durable")]
    fn unsupported_local_publisher_has_no_filesystem_effects(durable: bool) {
        smol::block_on(async {
            let root = tempfile::tempdir().unwrap();
            let authorization = Arc::new(Authorization(AtomicBool::new(true)));
            let result = if durable {
                LocalTransferPublisher::new_durable(
                    root.path().into(),
                    root.path().join("status.json"),
                    authorization,
                )
                .await
            } else {
                LocalTransferPublisher::new(root.path().into(), authorization).await
            };
            assert!(matches!(result, Err(WorkspaceError::UnsupportedEntry)));
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
            let publisher = LocalTransferPublisher { _unsupported: () };
            assert!(!publisher.supports_directory_publication());
            assert_eq!(
                publisher
                    .stat(&LocalTransferPath::new("target").unwrap())
                    .await,
                Err(WorkspaceError::UnsupportedEntry)
            );
            assert_eq!(
                publisher
                    .prepare(
                        source(),
                        destination("target", LocalTransferCondition::MustNotExist),
                        expected(),
                    )
                    .await,
                Err(WorkspaceError::UnsupportedEntry)
            );
        });
    }

    #[cfg(unix)]
    #[test_case(false; "no_replace_race")]
    #[test_case(true; "stale_conditional_replace")]
    fn local_preconditions_are_checked_again_after_review(replace: bool) {
        smol::block_on(async {
            let root = tempfile::tempdir().unwrap();
            let publisher = LocalTransferPublisher::new(
                root.path().to_owned(),
                Arc::new(Authorization(AtomicBool::new(true))),
            )
            .await
            .unwrap();
            let condition = if replace {
                fs::write(root.path().join("target"), BYTES).unwrap();
                LocalTransferCondition::Matches(
                    publisher
                        .stat(&LocalTransferPath::new("target").unwrap())
                        .await
                        .unwrap()
                        .0,
                )
            } else {
                LocalTransferCondition::MustNotExist
            };
            let prepared = publisher
                .prepare(source(), destination("target", condition), expected())
                .await
                .unwrap();
            fs::write(root.path().join("target"), CANARY).unwrap();
            assert_eq!(
                publisher.execute(&prepared).await,
                Err(WorkspaceError::Conflict)
            );
            assert_eq!(fs::read(root.path().join("target")).unwrap(), CANARY);
        });
    }

    #[cfg(unix)]
    #[test_case(false; "create_binary")]
    #[test_case(true; "replace_binary")]
    fn local_publication_requires_independent_host_authorization(replace: bool) {
        smol::block_on(async {
            let root = tempfile::tempdir().unwrap();
            let authorization = Arc::new(Authorization(AtomicBool::new(true)));
            let publisher =
                LocalTransferPublisher::new(root.path().to_owned(), authorization.clone())
                    .await
                    .unwrap();
            let condition = if replace {
                fs::write(root.path().join("target"), CANARY).unwrap();
                LocalTransferCondition::Matches(
                    publisher
                        .stat(&LocalTransferPath::new("target").unwrap())
                        .await
                        .unwrap()
                        .0,
                )
            } else {
                LocalTransferCondition::MustNotExist
            };
            let prepared = publisher
                .prepare(source(), destination("target", condition), expected())
                .await
                .unwrap();
            authorization.0.store(false, Ordering::Release);
            assert_eq!(
                publisher.execute(&prepared).await,
                Err(WorkspaceError::PermissionDenied)
            );
            assert_eq!(root.path().join("target").exists(), replace);
            authorization.0.store(true, Ordering::Release);
            publisher.execute(&prepared).await.unwrap();
            assert_eq!(fs::read(root.path().join("target")).unwrap(), BYTES);
            assert_eq!(
                publisher.execute(&prepared).await,
                Err(WorkspaceError::Conflict)
            );
        });
    }

    #[cfg(unix)]
    #[test_case(false; "symlink_at_prepare")]
    #[test_case(true; "ancestor_swapped_after_review")]
    fn local_publication_never_follows_remote_or_swapped_paths(swapped: bool) {
        smol::block_on(async {
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("canary"), CANARY).unwrap();
            let publisher = LocalTransferPublisher::new(
                root.path().to_owned(),
                Arc::new(Authorization(AtomicBool::new(true))),
            )
            .await
            .unwrap();
            if swapped {
                fs::create_dir(root.path().join("parent")).unwrap();
            } else {
                symlink(outside.path(), root.path().join("parent")).unwrap();
            }
            let result = publisher
                .prepare(
                    source(),
                    destination("parent/canary", LocalTransferCondition::MustNotExist),
                    expected(),
                )
                .await;
            if swapped {
                let prepared = result.unwrap();
                fs::rename(root.path().join("parent"), root.path().join("old-parent")).unwrap();
                symlink(outside.path(), root.path().join("parent")).unwrap();
                assert!(publisher.execute(&prepared).await.is_err());
            } else {
                assert!(result.is_err());
            }
            assert_eq!(fs::read(outside.path().join("canary")).unwrap(), CANARY);
        });
    }

    #[test_case(false; "digest_mismatch")]
    #[test_case(true; "size_mismatch")]
    fn private_staging_is_bounded_and_cleans_failed_integrity(size_mismatch: bool) {
        smol::block_on(async {
            let staging = PrivateStaging::default();
            let _io = staging.io().unwrap();
            let mut expected = expected();
            if size_mismatch {
                expected.size_bytes -= 1;
            } else {
                expected.digest = encode_digest(Sha256::digest(CANARY)).unwrap();
            }
            let result = staging
                .receive(
                    &mut Cursor::new(BYTES),
                    expected.size_bytes,
                    Some(&expected.digest),
                )
                .await;
            assert!(matches!(result, Err(WorkspaceError::TransferIntegrity)));
            let usage = staging.usage.lock().unwrap();
            assert_eq!(usage.stages, 0);
            assert_eq!(usage.bytes, IO_BUFFER_RESERVATION);
        });
    }

    #[test_case(1; "one_io")]
    #[test_case(2; "two_io")]
    fn private_stages_and_io_buffers_share_quota_and_release_on_drop(io_limit: u32) {
        smol::block_on(async {
            let mut limits = PrivateStaging::limits();
            limits.max_concurrent_io = io_limit;
            limits.max_reserved_bytes = IO_BUFFER_RESERVATION * u64::from(io_limit)
                + STREAM_BUFFER_BYTES as u64
                + BYTES.len() as u64;
            let staging = PrivateStaging::new(limits);
            let permits = (0..io_limit)
                .map(|_| staging.io().unwrap())
                .collect::<Vec<_>>();
            assert!(matches!(staging.io(), Err(WorkspaceError::TransferQuota)));
            let (file, _) = staging
                .receive(
                    &mut Cursor::new(BYTES),
                    BYTES.len() as u64,
                    Some(&expected().digest),
                )
                .await
                .unwrap();
            #[cfg(unix)]
            {
                let metadata = file.file.metadata().unwrap();
                assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
                assert_eq!(metadata.nlink(), 0);
            }
            assert!(matches!(
                staging
                    .receive(&mut Cursor::new(BYTES), BYTES.len() as u64, None)
                    .await,
                Err(WorkspaceError::TransferQuota)
            ));
            drop(file);
            drop(permits);
            assert_eq!(staging.usage.lock().unwrap().bytes, 0);
        });
    }

    #[cfg(unix)]
    #[test_case(false; "released")]
    #[test_case(true; "abandoned")]
    fn local_preparations_do_not_publish_without_execute(abandoned: bool) {
        smol::block_on(async {
            let root = tempfile::tempdir().unwrap();
            let publisher = LocalTransferPublisher::new(
                root.path().to_owned(),
                Arc::new(Authorization(AtomicBool::new(true))),
            )
            .await
            .unwrap();
            let prepared = publisher
                .prepare(
                    source(),
                    destination("target", LocalTransferCondition::MustNotExist),
                    expected(),
                )
                .await
                .unwrap();
            if !abandoned {
                publisher.release(&prepared).await.unwrap();
            }
            drop(publisher);
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
        });
    }
}
