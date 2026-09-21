use std::{
    collections::HashMap,
    fmt::Write as _,
    fs::File,
    future::Future,
    mem::size_of,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use tokio::sync::oneshot;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workcell_host_contract::{
    ContractVersion, HostBinding, Identifier, MAX_TRANSFER_CONCURRENCY, MAX_TRANSFER_JOURNAL_BYTES,
    MAX_TRANSFER_JOURNAL_STORAGE_BYTES, MAX_TRANSFER_JOURNALS, MAX_TRANSFER_RESERVED_BYTES,
    MAX_TRANSFER_STAGES, ResourceIntent, ReviewedTransferCapability, ReviewedTransferLimits,
    Revision, TRANSFER_IO_TIMEOUT_MS, TRANSFER_OUTCOME_RETENTION_MS, TRANSFER_STREAM_BUFFER_BYTES,
    TRANSFER_TTL_MS, TransferDownloadRequest, TransferDownloadResponse, TransferFile,
    TransferInventoryRequest, TransferInventoryResponse, TransferPrepareRequest,
    TransferPublicationState, TransferReleaseResponse, TransferSealResponse, TransferStageRequest,
    TransferStageResponse, TransferStageSelector, TransferStatusResponse, WorkspacePath,
    WorkspaceRequestBinding,
};
use workcell_mcp_files::{
    BinaryError, BinaryPublicationContent, FileToolGroup, PreparedBinaryPublication,
    VerifiedBinaryFile,
};

use super::journal::JournalStore;

const EXPIRY_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub(crate) enum TransferError {
    #[error("reviewed transfer binding is invalid")]
    Binding,
    #[error("reviewed transfer is missing, expired or released")]
    Missing,
    #[error("reviewed transfer quota or concurrency limit reached")]
    Limit,
    #[error("reviewed transfer state does not permit this request")]
    State,
    #[error("reviewed transfer digest or length does not match")]
    Integrity,
    #[error("publication ID was already reserved; query its durable status")]
    Replay,
    #[error("private transfer storage is unavailable or invalid")]
    Storage,
    #[error("reviewed transfer was cancelled or timed out")]
    Cancelled,
    #[error(transparent)]
    Binary(#[from] BinaryError),
}

impl TransferError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Binding => "transferBindingMismatch",
            Self::Missing => "transferMissing",
            Self::Limit => "transferLimitExceeded",
            Self::State => "transferInvalidState",
            Self::Integrity => "transferIntegrityFailure",
            Self::Replay => "transferPublicationReserved",
            Self::Storage => "transferStorageUnavailable",
            Self::Cancelled | Self::Binary(BinaryError::Cancelled) => "transferCancelled",
            Self::Binary(BinaryError::Indeterminate) => "transferIndeterminate",
            Self::Binary(_) => "transferConflict",
        }
    }
}

#[derive(Clone)]
pub(crate) struct ReviewedTransfers(Arc<Inner>);

impl std::fmt::Debug for ReviewedTransfers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReviewedTransfers")
            .finish_non_exhaustive()
    }
}

struct Inner {
    files: FileToolGroup,
    host: HostBinding,
    maximum: u64,
    registry: Mutex<HashMap<Identifier, Arc<Slot>>>,
    usage: Arc<Mutex<Usage>>,
    pub io: Arc<Semaphore>,
    journals: Mutex<JournalStore>,
}

#[derive(Default)]
struct Usage {
    count: u32,
    bytes: u64,
}

pub(super) struct Slot {
    pub binding: WorkspaceRequestBinding,
    expires: Instant,
    pub cancel: CancellationToken,
    pub size: u64,
    pub digest: Revision,
    pub data: Mutex<SlotData>,
    usage: Arc<Mutex<Usage>>,
}

pub(super) enum SlotData {
    Empty,
    Uploading,
    Uploaded {
        file: File,
        digest: Revision,
        size: u64,
    },
    Sealed(File),
    Claimed,
    Download {
        path: WorkspacePath,
        metadata: TransferFile,
    },
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.cancel.cancel();
        let mut usage = lock(&self.usage);
        usage.count -= 1;
        usage.bytes -= self.size;
    }
}

pub(crate) struct PreparedPublication {
    manager: ReviewedTransfers,
    pub publication_id: Identifier,
    slot: Arc<Slot>,
    source: Option<File>,
    binary: Option<PreparedBinaryPublication>,
    execution: Option<(Identifier, Identifier)>,
    #[cfg(test)]
    validation_gate: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
}

impl ReviewedTransfers {
    pub fn open(
        files: FileToolGroup,
        host: HostBinding,
        maximum: u64,
        root: &Path,
        namespace: &Identifier,
    ) -> Result<Self, TransferError> {
        if !files.allow_write() {
            return Err(TransferError::Binding);
        }
        let journals =
            JournalStore::open(root, files.workspace_snapshot_access().root(), namespace)?;
        let manager = Self(Arc::new(Inner {
            files,
            host,
            maximum: maximum.min(MAX_TRANSFER_RESERVED_BYTES),
            registry: Mutex::new(HashMap::new()),
            usage: Arc::new(Mutex::new(Usage::default())),
            io: Arc::new(Semaphore::new(MAX_TRANSFER_CONCURRENCY as usize)),
            journals: Mutex::new(journals),
        }));
        let weak = Arc::downgrade(&manager.0);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(EXPIRY_INTERVAL).await;
                let Some(inner) = weak.upgrade() else { break };
                expire_slots(&inner, Instant::now());
            }
        });
        Ok(manager)
    }

    pub fn capability(&self) -> ReviewedTransferCapability {
        ReviewedTransferCapability {
            version: ContractVersion::V1,
            private_staging: true,
            sealed_publication: true,
            conditional_download: true,
            single_range: true,
            durable_outcomes: true,
            creates_directories: true,
            safe_inventory: cfg!(target_os = "linux"),
            atomic_replace_against_external_writers: false,
            limits: ReviewedTransferLimits {
                max_file_bytes: self.0.maximum,
                max_stages: MAX_TRANSFER_STAGES,
                max_reserved_bytes: MAX_TRANSFER_RESERVED_BYTES,
                max_concurrent_io: MAX_TRANSFER_CONCURRENCY,
                stage_ttl_ms: TRANSFER_TTL_MS,
                io_timeout_ms: TRANSFER_IO_TIMEOUT_MS,
                max_journals: MAX_TRANSFER_JOURNALS,
                max_journal_bytes: MAX_TRANSFER_JOURNAL_BYTES,
                max_journal_storage_bytes: MAX_TRANSFER_JOURNAL_STORAGE_BYTES,
                outcome_retention_ms: TRANSFER_OUTCOME_RETENTION_MS,
                stream_buffer_bytes: TRANSFER_STREAM_BUFFER_BYTES as u32,
            },
        }
    }

    pub async fn validate(
        &self,
        binding: &WorkspaceRequestBinding,
    ) -> Result<String, TransferError> {
        if binding.host != self.0.host {
            return Err(TransferError::Binding);
        }
        self.0
            .files
            .workspace_directory_path(&binding.cwd_handle)
            .await
            .map_err(|_| TransferError::Binding)
    }

    pub async fn stage(
        &self,
        request: TransferStageRequest,
    ) -> Result<TransferStageResponse, TransferError> {
        self.validate(&request.binding).await?;
        if !valid_digest(&request.digest) {
            return Err(TransferError::Integrity);
        }
        let id = self.insert(
            request.binding,
            request.size_bytes,
            request.digest,
            SlotData::Empty,
        )?;
        Ok(TransferStageResponse {
            version: ContractVersion::V1,
            upload_path: format!("/files?reviewed=v1&stage={}", id.as_str()),
            stage_id: id,
            expires_at_unix_ms: unix_ms() + TRANSFER_TTL_MS,
        })
    }

    fn insert(
        &self,
        binding: WorkspaceRequestBinding,
        size: u64,
        digest: Revision,
        data: SlotData,
    ) -> Result<Identifier, TransferError> {
        let mut usage = lock(&self.0.usage);
        if size > self.0.maximum
            || usage.count >= MAX_TRANSFER_STAGES
            || usage.bytes.saturating_add(size) > MAX_TRANSFER_RESERVED_BYTES
        {
            return Err(TransferError::Limit);
        }
        usage.count += 1;
        usage.bytes += size;
        drop(usage);
        let id = Identifier::new(format!("transfer_{}", Uuid::new_v4()))
            .map_err(|_| TransferError::State)?;
        lock(&self.0.registry).insert(
            id.clone(),
            Arc::new(Slot {
                binding,
                expires: Instant::now() + Duration::from_millis(TRANSFER_TTL_MS),
                cancel: CancellationToken::new(),
                size,
                digest,
                data: Mutex::new(data),
                usage: self.0.usage.clone(),
            }),
        );
        Ok(id)
    }

    pub(super) async fn slot(&self, id: &Identifier) -> Result<Arc<Slot>, TransferError> {
        let slot = lock(&self.0.registry)
            .get(id)
            .cloned()
            .ok_or(TransferError::Missing)?;
        self.validate(&slot.binding).await?;
        if Instant::now() >= slot.expires || slot.cancel.is_cancelled() {
            return Err(TransferError::Missing);
        }
        Ok(slot)
    }

    pub async fn seal(
        &self,
        request: TransferStageSelector,
    ) -> Result<TransferSealResponse, TransferError> {
        self.validate(&request.binding).await?;
        let slot = self.slot(&request.stage_id).await?;
        if slot.binding != request.binding {
            return Err(TransferError::Binding);
        }
        let mut data = lock(&slot.data);
        match &*data {
            SlotData::Sealed(_) => (),
            SlotData::Uploaded { digest, size, .. }
                if *digest == slot.digest && *size == slot.size =>
            {
                let SlotData::Uploaded { file, .. } =
                    std::mem::replace(&mut *data, SlotData::Claimed)
                else {
                    return Err(TransferError::State);
                };
                *data = SlotData::Sealed(file);
            }
            SlotData::Uploaded { .. } => {
                *data = SlotData::Claimed;
                return Err(TransferError::Integrity);
            }
            _ => return Err(TransferError::State),
        }
        Ok(TransferSealResponse {
            version: ContractVersion::V1,
            stage_id: request.stage_id,
            digest: slot.digest.clone(),
            size_bytes: slot.size,
        })
    }

    pub async fn release(
        &self,
        request: TransferStageSelector,
    ) -> Result<TransferReleaseResponse, TransferError> {
        self.validate(&request.binding).await?;
        let mut registry = lock(&self.0.registry);
        if let Some(slot) = registry.get(&request.stage_id) {
            if slot.binding != request.binding {
                return Err(TransferError::Binding);
            }
            slot.cancel.cancel();
        }
        Ok(TransferReleaseResponse {
            version: ContractVersion::V1,
            released: registry.remove(&request.stage_id).is_some(),
        })
    }

    pub(super) fn permit(&self) -> Result<OwnedSemaphorePermit, TransferError> {
        self.0
            .io
            .clone()
            .try_acquire_owned()
            .map_err(|_| TransferError::Limit)
    }

    pub(super) fn private_file(&self) -> Result<File, TransferError> {
        tempfile::tempfile_in(&lock(&self.0.journals).root).map_err(|_| TransferError::Storage)
    }

    pub async fn stat(
        &self,
        binding: &WorkspaceRequestBinding,
        path: &WorkspacePath,
        token: &CancellationToken,
    ) -> Result<VerifiedBinaryFile, TransferError> {
        self.validate(binding).await?;
        let binding = binding.clone();
        let path = path.clone();
        self.bounded_io(token, move |manager, token| async move {
            manager
                .0
                .files
                .open_binary(&binding.cwd_handle, &path, manager.0.maximum, &token)
                .await
                .map_err(TransferError::from)
        })
        .await
    }

    async fn bounded_io<T, F, Fut>(
        &self,
        token: &CancellationToken,
        work: F,
    ) -> Result<T, TransferError>
    where
        T: Send + 'static,
        F: FnOnce(Self, CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, TransferError>> + Send + 'static,
    {
        let permit = self.permit()?;
        let manager = self.clone();
        let token = token.child_token();
        let _cancel = token.clone().drop_guard();
        // A disconnected request cancels the worker but cannot release its I/O admission while
        // spawn_blocking still owns filesystem descriptors or the shared mutation lock.
        tokio::spawn(async move {
            let _permit = permit;
            let work = work(manager, token.clone());
            tokio::pin!(work);
            tokio::select! {
                result = &mut work => result,
                () = tokio::time::sleep(Duration::from_millis(TRANSFER_IO_TIMEOUT_MS)) => {
                    token.cancel();
                    let _ = work.await;
                    Err(TransferError::Cancelled)
                }
            }
        })
        .await
        .map_err(|_| TransferError::State)?
    }

    pub async fn download(
        &self,
        request: TransferDownloadRequest,
        token: &CancellationToken,
    ) -> Result<TransferDownloadResponse, TransferError> {
        let source = self.stat(&request.binding, &request.path, token).await?;
        if source.metadata.revision != request.revision || source.metadata.digest != request.digest
        {
            return Err(BinaryError::Conflict.into());
        }
        let metadata = source.metadata;
        let id = self.insert(
            request.binding,
            0,
            request.digest,
            SlotData::Download {
                path: request.path,
                metadata: metadata.clone(),
            },
        )?;
        Ok(TransferDownloadResponse {
            version: ContractVersion::V1,
            download_path: format!("/files?reviewed=v1&download={}", id.as_str()),
            download_id: id,
            file: metadata,
            expires_at_unix_ms: unix_ms() + TRANSFER_TTL_MS,
        })
    }

    pub async fn inventory(
        &self,
        request: TransferInventoryRequest,
        token: &CancellationToken,
    ) -> Result<TransferInventoryResponse, TransferError> {
        self.validate(&request.binding).await?;
        self.bounded_io(token, move |manager, token| async move {
            manager
                .0
                .files
                .transfer_inventory(
                    &request.binding.cwd_handle,
                    request.inspect,
                    request.policy,
                    &token,
                )
                .await
                .map_err(TransferError::from)
        })
        .await
    }

    pub async fn prepare(
        &self,
        request: TransferPrepareRequest,
        request_digest: Revision,
        token: &CancellationToken,
    ) -> Result<PreparedPublication, TransferError> {
        let cwd = self.validate(&request.binding).await?;
        let slot = self.slot(&request.stage_id).await?;
        if slot.binding != request.binding {
            return Err(TransferError::Binding);
        }
        if slot.digest != request.digest || slot.size != request.size_bytes {
            return Err(TransferError::Integrity);
        }
        if !matches!(*lock(&slot.data), SlotData::Sealed(_)) {
            return Err(TransferError::State);
        }
        let publication_id = request.publication_id.clone();
        let binary = self
            .bounded_io(token, move |manager, token| async move {
                manager
                    .0
                    .files
                    .prepare_binary_publication_with_directories(
                        &request.binding.cwd_handle,
                        &request.path,
                        request.precondition,
                        BinaryPublicationContent {
                            digest: request.digest,
                            size_bytes: request.size_bytes,
                            mode: request.mode,
                        },
                        request.create_directories,
                        manager.0.maximum,
                        &token,
                    )
                    .await
                    .map_err(TransferError::from)
            })
            .await?;
        let mut data = lock(&slot.data);
        if !matches!(*data, SlotData::Sealed(_)) {
            return Err(TransferError::State);
        }
        lock(&self.0.journals).reserve(publication_id.clone(), cwd, request_digest)?;
        let SlotData::Sealed(source) = std::mem::replace(&mut *data, SlotData::Claimed) else {
            return Err(TransferError::State);
        };
        drop(data);
        Ok(PreparedPublication {
            manager: self.clone(),
            publication_id,
            slot,
            source: Some(source),
            binary: Some(binary),
            execution: None,
            #[cfg(test)]
            validation_gate: None,
        })
    }

    pub fn record_preparation(
        &self,
        publication: &Identifier,
        preparation: &Identifier,
        expires_at: u64,
    ) -> Result<(), TransferError> {
        let mut store = lock(&self.0.journals);
        let mut journal = store.get(publication).ok_or(TransferError::Missing)?;
        journal.status.preparation_id = Some(preparation.clone());
        journal.expires_at = expires_at;
        store.put(journal)
    }

    pub async fn status(
        &self,
        binding: &WorkspaceRequestBinding,
        id: &Identifier,
    ) -> Result<TransferStatusResponse, TransferError> {
        let cwd = self.validate(binding).await?;
        let mut store = lock(&self.0.journals);
        match store.get(id) {
            Some(mut journal) if journal.cwd == cwd => {
                if journal.status.state == TransferPublicationState::Prepared
                    && unix_ms() >= journal.expires_at
                {
                    journal.status.state = TransferPublicationState::Cancelled;
                    journal.updated_at = unix_ms();
                    store.put(journal.clone())?;
                }
                Ok(journal.status)
            }
            Some(_) => Err(TransferError::Binding),
            None => Ok(TransferStatusResponse {
                version: ContractVersion::V1,
                publication_id: id.clone(),
                state: TransferPublicationState::Unknown,
                preparation_id: None,
                invocation_id: None,
                request_digest: None,
                file: None,
            }),
        }
    }
}

impl PreparedPublication {
    pub fn retained_bytes(&self) -> usize {
        // The shared registry has its own fixed count/byte quota; charge our retained lease too.
        size_of::<Self>()
            + self
                .binary
                .as_ref()
                .map_or(0, PreparedBinaryPublication::retained_bytes)
            + self.publication_id.as_str().len() * 2
            + size_of::<Slot>()
            + serde_json::to_vec(&self.slot.binding).map_or(usize::MAX / 2, |bytes| bytes.len() * 2)
    }

    pub fn resources(&self) -> &[ResourceIntent] {
        self.binary
            .as_ref()
            .map_or(&[], PreparedBinaryPublication::resources)
    }

    pub fn bind_execution(&mut self, preparation: Identifier, invocation: Identifier) {
        self.execution = Some((preparation, invocation));
    }

    pub async fn execute(
        mut self,
        token: &CancellationToken,
    ) -> Result<TransferStatusResponse, TransferError> {
        let (preparation, invocation) = self.execution.take().ok_or(TransferError::State)?;
        #[cfg(test)]
        if let Some((entered, resume)) = self.validation_gate.take() {
            let _ = entered.send(());
            let _ = resume.await;
        }
        self.manager.validate(&self.slot.binding).await?;
        let _permit = self.manager.permit()?;
        // Admission, expiry and the durable publication fence must use the same current record.
        let mut journal = {
            let mut store = lock(&self.manager.0.journals);
            let mut journal = store
                .get(&self.publication_id)
                .ok_or(TransferError::State)?;
            if journal.status.state != TransferPublicationState::Prepared {
                return Err(TransferError::Replay);
            }
            let now = unix_ms();
            if now >= journal.expires_at
                || self.slot.cancel.is_cancelled()
                || Instant::now() >= self.slot.expires
                || token.is_cancelled()
            {
                return Err(TransferError::Cancelled);
            }
            journal.status.preparation_id = Some(preparation);
            journal.status.invocation_id = Some(invocation);
            journal.status.state = TransferPublicationState::Publishing;
            journal.updated_at = now;
            store.put(journal.clone())?;
            journal
        };
        let child = token.child_token();
        let monitor_token = child.clone();
        let slot_cancel = self.slot.cancel.clone();
        let monitor = tokio::spawn(async move {
            tokio::select! {
                () = slot_cancel.cancelled() => (),
                () = tokio::time::sleep(Duration::from_millis(TRANSFER_IO_TIMEOUT_MS)) => (),
            }
            monitor_token.cancel();
        });
        let result = self
            .manager
            .0
            .files
            .execute_binary_publication(
                self.binary.take().ok_or(TransferError::State)?,
                self.source.take().ok_or(TransferError::State)?,
                &child,
            )
            .await;
        monitor.abort();
        journal.updated_at = unix_ms();
        journal.status.state = match &result {
            Ok(file) => {
                journal.status.file = Some(file.clone());
                TransferPublicationState::Completed
            }
            Err(BinaryError::Indeterminate) => TransferPublicationState::Indeterminate,
            Err(BinaryError::Cancelled) => TransferPublicationState::Cancelled,
            Err(_) => TransferPublicationState::Failed,
        };
        lock(&self.manager.0.journals)
            .put(journal.clone())
            .map_err(|_| TransferError::Binary(BinaryError::Indeterminate))?;
        result?;
        Ok(journal.status)
    }
}

impl Drop for PreparedPublication {
    fn drop(&mut self) {
        let mut store = lock(&self.manager.0.journals);
        if let Some(mut journal) = store.get(&self.publication_id) {
            match journal.status.state {
                TransferPublicationState::Prepared => {
                    journal.status.state = TransferPublicationState::Cancelled
                }
                TransferPublicationState::Publishing => {
                    journal.status.state = TransferPublicationState::Indeterminate
                }
                _ => return,
            }
            journal.updated_at = unix_ms();
            let _ = store.put(journal);
        }
    }
}

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) fn valid_digest(digest: &Revision) -> bool {
    digest.as_str().strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

pub(super) fn hex_digest(bytes: impl IntoIterator<Item = u8>) -> String {
    let mut output = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

pub(super) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn expire_slots(inner: &Inner, now: Instant) {
    lock(&inner.registry).retain(|_, slot| {
        if now >= slot.expires {
            slot.cancel.cancel();
            false
        } else {
            true
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{ReviewedTransfers, TransferError, expire_slots, hex_digest, lock};
    use crate::{transfer::reviewed_http, transports::http::Authenticated};
    use axum::{
        body::{Body, Bytes, to_bytes},
        extract::Request,
        http::StatusCode,
    };
    use futures_util::{StreamExt, future::join_all, stream};
    use sha2::{Digest, Sha256};
    use std::{
        error::Error,
        fs,
        io::{self, Cursor},
        os::unix::fs::PermissionsExt,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
        time::{Duration, Instant},
    };
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncRead, ReadBuf},
        sync::{Notify, oneshot},
    };
    use tokio_util::sync::CancellationToken;
    use workcell_host_contract::{
        ContractVersion, HostBinding, Identifier, MAX_TRANSFER_CONCURRENCY,
        MAX_TRANSFER_RESERVED_BYTES, MAX_TRANSFER_STAGES, Revision, TRANSFER_IO_TIMEOUT_MS,
        TRANSFER_STREAM_BUFFER_BYTES, TRANSFER_TTL_MS, TransferDownloadRequest, TransferMode,
        TransferPrecondition, TransferPrepareRequest, TransferPublicationState,
        TransferStageRequest, TransferStageSelector, WorkspacePath, WorkspaceRequestBinding,
    };
    use workcell_mcp_files::FileToolGroup;

    const PAYLOAD: &[u8] = b"\xff\0binary transfer bytes";
    const TARGET: &str = "target.bin";
    const CWD: &str = "x-workcell-cwd";
    const RECLAIM_TIMEOUT: Duration = Duration::from_millis(1);
    const STREAM_PAYLOAD_BYTES: usize = TRANSFER_STREAM_BUFFER_BYTES * 4;

    #[derive(Clone, Default)]
    struct ReadProbe {
        bytes: Arc<AtomicUsize>,
        progress: Arc<Notify>,
        dropped: CancellationToken,
    }

    struct ObservedReader {
        data: Cursor<Vec<u8>>,
        probe: ReadProbe,
    }

    impl AsyncRead for ObservedReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let before = buffer.filled().len();
            let result = Pin::new(&mut self.data).poll_read(context, buffer);
            self.probe
                .bytes
                .fetch_add(buffer.filled().len() - before, Ordering::SeqCst);
            self.probe.progress.notify_one();
            result
        }
    }

    impl Drop for ObservedReader {
        fn drop(&mut self) {
            self.probe.dropped.cancel();
        }
    }

    enum DownloadStop {
        Deadline,
        Cancellation,
        Disconnect,
    }

    fn id(value: &str) -> Identifier {
        Identifier::new(value).unwrap()
    }
    fn digest(bytes: &[u8]) -> Revision {
        Revision::new(format!("sha256:{}", hex_digest(Sha256::digest(bytes)))).unwrap()
    }

    async fn fixture() -> (TempDir, TempDir, ReviewedTransfers, WorkspaceRequestBinding) {
        let root = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        fs::set_permissions(private.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let files = FileToolGroup::new(root.path(), true, None).await.unwrap();
        let cwd = files.workspace_root().await.unwrap().handle;
        let host = HostBinding {
            server_id: id("server"),
            instance_id: id("instance"),
            workspace_id: id("workspace"),
            workspace_generation: id("generation"),
            root_project_id: id("project"),
            principal_id: id("principal"),
            cwd_handle: cwd.clone(),
            catalog_revision: digest(b"catalog"),
            policy_revision: digest(b"policy"),
        };
        let binding = WorkspaceRequestBinding {
            host: host.clone(),
            cwd_handle: cwd,
        };
        let manager = ReviewedTransfers::open(
            files,
            host,
            MAX_TRANSFER_RESERVED_BYTES,
            private.path(),
            &id("namespace"),
        )
        .unwrap();
        (root, private, manager, binding)
    }

    async fn stage(
        manager: &ReviewedTransfers,
        binding: &WorkspaceRequestBinding,
        bytes: &[u8],
    ) -> TransferStageSelector {
        let stage = manager
            .stage(TransferStageRequest {
                version: ContractVersion::V1,
                binding: binding.clone(),
                size_bytes: bytes.len() as u64,
                digest: digest(bytes),
            })
            .await
            .unwrap();
        TransferStageSelector {
            version: ContractVersion::V1,
            binding: binding.clone(),
            stage_id: stage.stage_id,
        }
    }

    fn upload_request(selector: &TransferStageSelector, body: Body) -> Request {
        Request::post(format!(
            "/files?reviewed=v1&stage={}",
            selector.stage_id.as_str()
        ))
        .extension(Authenticated)
        .header(CWD, selector.binding.cwd_handle.as_str())
        .header("content-type", "application/octet-stream")
        .body(body)
        .unwrap()
    }

    fn prepare_request(
        selector: &TransferStageSelector,
        publication: &str,
    ) -> TransferPrepareRequest {
        TransferPrepareRequest {
            version: ContractVersion::V1,
            binding: selector.binding.clone(),
            publication_id: id(publication),
            stage_id: selector.stage_id.clone(),
            digest: digest(PAYLOAD),
            size_bytes: PAYLOAD.len() as u64,
            path: WorkspacePath::new(TARGET).unwrap(),
            create_directories: Vec::new(),
            precondition: TransferPrecondition::MustNotExist {},
            mode: TransferMode::Regular,
        }
    }

    async fn sealed(
        manager: &ReviewedTransfers,
        binding: &WorkspaceRequestBinding,
    ) -> TransferStageSelector {
        let selector = stage(manager, binding, PAYLOAD).await;
        assert_eq!(
            reviewed_http::upload(
                Some(manager),
                upload_request(&selector, Body::from(PAYLOAD))
            )
            .await
            .status(),
            StatusCode::OK
        );
        manager.seal(selector.clone()).await.unwrap();
        selector
    }

    #[tokio::test]
    async fn private_staging_requires_sealing_and_never_replays_a_publication() {
        let (root, _private, manager, binding) = fixture().await;
        let selector = stage(&manager, &binding, PAYLOAD).await;
        let request = prepare_request(&selector, "publication");
        assert!(matches!(
            manager
                .prepare(
                    request.clone(),
                    digest(b"request"),
                    &CancellationToken::new()
                )
                .await,
            Err(TransferError::State)
        ));
        for status in [StatusCode::OK, StatusCode::CONFLICT] {
            assert_eq!(
                reviewed_http::upload(
                    Some(&manager),
                    upload_request(&selector, Body::from(PAYLOAD))
                )
                .await
                .status(),
                status
            );
        }
        assert!(!root.path().join(TARGET).exists());
        assert!(matches!(
            manager
                .prepare(
                    request.clone(),
                    digest(b"request"),
                    &CancellationToken::new()
                )
                .await,
            Err(TransferError::State)
        ));
        let seal = manager.seal(selector.clone()).await.unwrap();
        assert_eq!(manager.seal(selector.clone()).await.unwrap(), seal);
        let mut wrong = request.clone();
        wrong.digest = digest(b"wrong");
        assert!(matches!(
            manager
                .prepare(wrong, digest(b"request"), &CancellationToken::new())
                .await,
            Err(TransferError::Integrity)
        ));
        let mut prepared = manager
            .prepare(request, digest(b"request"), &CancellationToken::new())
            .await
            .unwrap();
        assert!(!root.path().join(TARGET).exists());
        prepared.bind_execution(id("preparation"), id("invocation"));
        let outcome = prepared.execute(&CancellationToken::new()).await.unwrap();
        assert_eq!(outcome.state, TransferPublicationState::Completed);
        assert_eq!(fs::read(root.path().join(TARGET)).unwrap(), PAYLOAD);
        fs::remove_file(root.path().join(TARGET)).unwrap();
        let second = sealed(&manager, &binding).await;
        assert!(matches!(
            manager
                .prepare(
                    prepare_request(&second, "publication"),
                    digest(b"request"),
                    &CancellationToken::new()
                )
                .await,
            Err(TransferError::Replay)
        ));
        assert_eq!(
            manager.status(&binding, &id("publication")).await.unwrap(),
            outcome
        );
        assert!(!root.path().join(TARGET).exists());
    }

    #[tokio::test]
    async fn expiry_during_validation_cannot_publish_after_status_reports_cancelled() {
        let (root, _private, manager, binding) = fixture().await;
        let selector = sealed(&manager, &binding).await;
        let publication = id("expiry-race");
        let mut prepared = manager
            .prepare(
                prepare_request(&selector, publication.as_str()),
                digest(b"request"),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        prepared.bind_execution(id("preparation"), id("invocation"));
        let (entered, paused) = oneshot::channel();
        let (resume, resumed) = oneshot::channel();
        prepared.validation_gate = Some((entered, resumed));
        let task = tokio::spawn(async move { prepared.execute(&CancellationToken::new()).await });
        paused.await.unwrap();
        manager
            .record_preparation(&publication, &id("preparation"), 0)
            .unwrap();
        let cancelled = manager.status(&binding, &publication).await.unwrap();
        assert_eq!(cancelled.state, TransferPublicationState::Cancelled);
        resume.send(()).unwrap();
        let result = task.await.unwrap();
        assert!(!root.path().join(TARGET).exists());
        assert!(matches!(result, Err(TransferError::Replay)));
        assert_eq!(
            manager.status(&binding, &publication).await.unwrap(),
            cancelled
        );
    }

    #[tokio::test]
    async fn expiry_and_cancellation_during_validation_stop_before_the_publication_fence() {
        for cause in ["expiry", "release", "request"] {
            let (root, _private, manager, binding) = fixture().await;
            let selector = sealed(&manager, &binding).await;
            let publication = id(cause);
            let mut prepared = manager
                .prepare(
                    prepare_request(&selector, cause),
                    digest(b"request"),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            prepared.bind_execution(id("preparation"), id("invocation"));
            let (entered, paused) = oneshot::channel();
            let (resume, resumed) = oneshot::channel();
            prepared.validation_gate = Some((entered, resumed));
            let token = CancellationToken::new();
            let execution_token = token.clone();
            let task = tokio::spawn(async move { prepared.execute(&execution_token).await });
            paused.await.unwrap();
            match cause {
                "expiry" => manager
                    .record_preparation(&publication, &id("preparation"), 0)
                    .unwrap(),
                "release" => {
                    manager.release(selector).await.unwrap();
                }
                _ => token.cancel(),
            }
            resume.send(()).unwrap();
            assert!(
                matches!(task.await.unwrap(), Err(TransferError::Cancelled)),
                "{cause}"
            );
            let status = manager.status(&binding, &publication).await.unwrap();
            assert_eq!(status.state, TransferPublicationState::Cancelled, "{cause}");
            assert_eq!(status.invocation_id, None, "{cause}");
            assert!(!root.path().join(TARGET).exists(), "{cause}");
        }
    }

    async fn unpolled_downloads_reclaim_admission(stop: DownloadStop) {
        let (_root, _private, manager, binding) = fixture().await;
        let selector = stage(&manager, &binding, b"").await;
        let slot = manager.slot(&selector.stage_id).await.unwrap();
        tokio::time::pause();
        let (bodies, probes): (Vec<_>, Vec<_>) = (0..MAX_TRANSFER_CONCURRENCY)
            .map(|_| {
                let probe = ReadProbe::default();
                let body = reviewed_http::download_body(
                    ObservedReader {
                        data: Cursor::new(vec![0; STREAM_PAYLOAD_BYTES]),
                        probe: probe.clone(),
                    },
                    manager.permit().unwrap(),
                    slot.clone(),
                );
                (body, probe)
            })
            .unzip();
        for probe in &probes {
            tokio::time::timeout(RECLAIM_TIMEOUT, probe.progress.notified())
                .await
                .unwrap();
            // With an unpolled receiver only the queued and pending chunks can be read.
            assert_eq!(
                probe.bytes.load(Ordering::SeqCst),
                TRANSFER_STREAM_BUFFER_BYTES / 2
            );
            assert!(!probe.dropped.is_cancelled());
        }
        assert!(matches!(manager.permit(), Err(TransferError::Limit)));
        let bodies = match stop {
            DownloadStop::Cancellation => {
                slot.cancel.cancel();
                Some(bodies)
            }
            DownloadStop::Deadline => {
                tokio::time::advance(
                    Duration::from_millis(TRANSFER_IO_TIMEOUT_MS) - RECLAIM_TIMEOUT,
                )
                .await;
                assert!(matches!(manager.permit(), Err(TransferError::Limit)));
                tokio::time::advance(RECLAIM_TIMEOUT).await;
                Some(bodies)
            }
            DownloadStop::Disconnect => {
                drop(bodies);
                None
            }
        };
        let reclaimed = tokio::time::timeout(
            RECLAIM_TIMEOUT,
            manager.0.io.acquire_many(MAX_TRANSFER_CONCURRENCY),
        )
        .await
        .expect("unpolled download bodies retained I/O admission")
        .unwrap();
        for probe in probes {
            assert!(probe.dropped.is_cancelled());
            assert_eq!(
                probe.bytes.load(Ordering::SeqCst),
                TRANSFER_STREAM_BUFFER_BYTES / 2
            );
        }
        for body in bodies.into_iter().flatten() {
            let mut stream = body.into_data_stream();
            let mut buffered = 0;
            let error = loop {
                match stream
                    .next()
                    .await
                    .expect("interrupted download ended without an error")
                {
                    Ok(chunk) => buffered += chunk.len(),
                    Err(error) => break error,
                }
            };
            let mut cause: &(dyn Error + 'static) = &error;
            while let Some(source) = cause.source() {
                cause = source;
            }
            assert_eq!(buffered, TRANSFER_STREAM_BUFFER_BYTES / 4);
            assert_eq!(
                cause.downcast_ref::<io::Error>().unwrap().kind(),
                match stop {
                    DownloadStop::Deadline => io::ErrorKind::TimedOut,
                    _ => io::ErrorKind::Interrupted,
                }
            );
        }
        drop(reclaimed);
    }

    #[tokio::test]
    async fn unpolled_downloads_release_all_io_permits_at_the_deadline() {
        unpolled_downloads_reclaim_admission(DownloadStop::Deadline).await;
    }

    #[tokio::test]
    async fn unpolled_downloads_release_all_io_permits_on_cancellation() {
        unpolled_downloads_reclaim_admission(DownloadStop::Cancellation).await;
    }

    #[tokio::test]
    async fn dropping_saturated_download_bodies_shuts_down_the_producers() {
        unpolled_downloads_reclaim_admission(DownloadStop::Disconnect).await;
    }

    #[tokio::test]
    async fn download_producers_preserve_empty_and_multichunk_content_and_release_admission() {
        let (_root, _private, manager, binding) = fixture().await;
        let selector = stage(&manager, &binding, b"").await;
        let slot = manager.slot(&selector.stage_id).await.unwrap();
        tokio::time::pause();
        for bytes in [
            Vec::new(),
            PAYLOAD.to_vec(),
            PAYLOAD.repeat(TRANSFER_STREAM_BUFFER_BYTES),
        ] {
            let probe = ReadProbe::default();
            let body = reviewed_http::download_body(
                ObservedReader {
                    data: Cursor::new(bytes.clone()),
                    probe: probe.clone(),
                },
                manager.permit().unwrap(),
                slot.clone(),
            );
            assert_eq!(to_bytes(body, bytes.len()).await.unwrap().as_ref(), bytes);
            assert_eq!(
                manager.0.io.available_permits(),
                MAX_TRANSFER_CONCURRENCY as usize
            );
            assert!(probe.dropped.is_cancelled());
        }
    }

    #[tokio::test]
    async fn digest_mismatch_oversize_binding_changes_and_release_all_fail_closed() {
        let (root, _private, manager, binding) = fixture().await;
        let selector = stage(&manager, &binding, PAYLOAD).await;
        for change in ["principal", "generation", "instance", "cwd"] {
            let mut wrong = selector.clone();
            match change {
                "principal" => wrong.binding.host.principal_id = id("other"),
                "generation" => wrong.binding.host.workspace_generation = id("other"),
                "instance" => wrong.binding.host.instance_id = id("other"),
                _ => {
                    wrong.binding.cwd_handle =
                        workcell_host_contract::ResourceId::new("other").unwrap()
                }
            }
            assert!(matches!(
                manager.release(wrong.clone()).await,
                Err(TransferError::Binding)
            ));
            assert!(matches!(
                manager.seal(wrong).await,
                Err(TransferError::Binding)
            ));
        }
        let response = reviewed_http::upload(
            Some(&manager),
            upload_request(&selector, Body::from(vec![0u8; PAYLOAD.len()])),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(matches!(
            manager.seal(selector.clone()).await,
            Err(TransferError::Integrity)
        ));
        manager.release(selector).await.unwrap();
        let selector = stage(&manager, &binding, PAYLOAD).await;
        assert_eq!(
            reviewed_http::upload(
                Some(&manager),
                upload_request(&selector, Body::from(vec![0u8; PAYLOAD.len() + 1]))
            )
            .await
            .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert!(manager.seal(selector.clone()).await.is_err());
        manager.release(selector).await.unwrap();
        let selector = sealed(&manager, &binding).await;
        let mut prepared = manager
            .prepare(
                prepare_request(&selector, "cancelled"),
                digest(b"request"),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        prepared.bind_execution(id("prep"), id("invocation"));
        manager.release(selector).await.unwrap();
        assert!(matches!(
            prepared.execute(&CancellationToken::new()).await,
            Err(TransferError::Cancelled)
        ));
        assert_eq!(
            manager
                .status(&binding, &id("cancelled"))
                .await
                .unwrap()
                .state,
            TransferPublicationState::Cancelled
        );
        assert!(!root.path().join(TARGET).exists());
        assert_eq!(lock(&manager.0.usage).count, 0);
    }

    #[tokio::test]
    async fn concurrent_stage_admission_preserves_count_byte_and_io_quotas_until_leases_drop() {
        let (_root, _private, manager, binding) = fixture().await;
        let requests = (0..MAX_TRANSFER_STAGES * 2).map(|_| {
            manager.stage(TransferStageRequest {
                version: ContractVersion::V1,
                binding: binding.clone(),
                size_bytes: 0,
                digest: digest(b""),
            })
        });
        let admitted = join_all(requests)
            .await
            .into_iter()
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        assert_eq!(admitted.len(), MAX_TRANSFER_STAGES as usize);
        let retained = manager.slot(&admitted[0].stage_id).await.unwrap();
        for stage in admitted {
            manager
                .release(TransferStageSelector {
                    version: ContractVersion::V1,
                    binding: binding.clone(),
                    stage_id: stage.stage_id,
                })
                .await
                .unwrap();
        }
        assert_eq!(lock(&manager.0.usage).count, 1);
        drop(retained);
        let large = manager
            .stage(TransferStageRequest {
                version: ContractVersion::V1,
                binding: binding.clone(),
                size_bytes: MAX_TRANSFER_RESERVED_BYTES,
                digest: digest(PAYLOAD),
            })
            .await
            .unwrap();
        assert!(matches!(
            manager
                .stage(TransferStageRequest {
                    version: ContractVersion::V1,
                    binding: binding.clone(),
                    size_bytes: 1,
                    digest: digest(PAYLOAD)
                })
                .await,
            Err(TransferError::Limit)
        ));
        let retained = manager.slot(&large.stage_id).await.unwrap();
        expire_slots(
            &manager.0,
            Instant::now() + Duration::from_millis(TRANSFER_TTL_MS),
        );
        assert!(retained.cancel.is_cancelled());
        assert_eq!(lock(&manager.0.usage).bytes, MAX_TRANSFER_RESERVED_BYTES);
        drop(retained);
        assert_eq!(lock(&manager.0.usage).bytes, 0);
        let permits = (0..MAX_TRANSFER_CONCURRENCY)
            .map(|_| manager.permit().unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(manager.permit(), Err(TransferError::Limit)));
        drop(permits);
        assert!(manager.permit().is_ok());
    }

    #[tokio::test]
    async fn cancelling_an_active_upload_reclaims_anonymous_staging_without_workspace_effects() {
        let (root, _private, manager, binding) = fixture().await;
        let selector = stage(&manager, &binding, PAYLOAD).await;
        let (entered, receiver) = oneshot::channel();
        let body = Body::from_stream(stream::once(async move {
            entered.send(()).unwrap();
            std::future::pending::<Result<Bytes, io::Error>>().await
        }));
        let request = upload_request(&selector, body);
        let task_manager = manager.clone();
        let task =
            tokio::spawn(async move { reviewed_http::upload(Some(&task_manager), request).await });
        receiver.await.unwrap();
        manager.release(selector).await.unwrap();
        assert_eq!(task.await.unwrap().status(), StatusCode::GONE);
        assert_eq!(lock(&manager.0.usage).count, 0);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
        assert_eq!(
            fs::read_dir(&lock(&manager.0.journals).root)
                .unwrap()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn selected_downloads_enforce_if_match_single_ranges_and_source_revisions() {
        let (root, _private, manager, binding) = fixture().await;
        fs::write(root.path().join(TARGET), PAYLOAD).unwrap();
        let metadata = manager
            .stat(
                &binding,
                &WorkspacePath::new(TARGET).unwrap(),
                &CancellationToken::new(),
            )
            .await
            .unwrap()
            .metadata;
        let download = manager
            .download(
                TransferDownloadRequest {
                    version: ContractVersion::V1,
                    binding: binding.clone(),
                    path: WorkspacePath::new(TARGET).unwrap(),
                    revision: metadata.revision.clone(),
                    digest: metadata.digest.clone(),
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let etag = format!("\"{}\"", metadata.revision.as_str());
        for (range, expected, bytes) in [
            (None, StatusCode::OK, PAYLOAD),
            (
                Some("bytes=2-5"),
                StatusCode::PARTIAL_CONTENT,
                &PAYLOAD[2..6],
            ),
            (
                Some("bytes=-3"),
                StatusCode::PARTIAL_CONTENT,
                &PAYLOAD[PAYLOAD.len() - 3..],
            ),
            (Some("bytes=2-"), StatusCode::PARTIAL_CONTENT, &PAYLOAD[2..]),
            (Some("bytes=0-9999"), StatusCode::PARTIAL_CONTENT, PAYLOAD),
            (Some("bytes=99-100"), StatusCode::RANGE_NOT_SATISFIABLE, &[]),
            (
                Some("bytes=0-1,3-4"),
                StatusCode::RANGE_NOT_SATISFIABLE,
                &[],
            ),
            (Some("bytes=+1-2"), StatusCode::RANGE_NOT_SATISFIABLE, &[]),
        ] {
            let mut request = Request::get(&download.download_path)
                .extension(Authenticated)
                .header(CWD, binding.cwd_handle.as_str())
                .header("if-match", &etag);
            if let Some(range) = range {
                request = request.header("range", range);
            }
            let response =
                reviewed_http::download(Some(&manager), request.body(Body::empty()).unwrap()).await;
            assert_eq!(response.status(), expected, "{range:?}");
            if expected.is_success() {
                assert_eq!(
                    to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
                    bytes
                );
            } else {
                assert_eq!(
                    response.headers()["content-range"],
                    format!("bytes */{}", PAYLOAD.len())
                );
            }
        }
        for (if_match, expected) in [
            (None, StatusCode::PRECONDITION_REQUIRED),
            (Some(format!("W/{etag}")), StatusCode::PRECONDITION_FAILED),
            (Some(format!("\"other\", {etag}")), StatusCode::OK),
            (Some("*".to_owned()), StatusCode::OK),
        ] {
            let mut request = Request::get(&download.download_path)
                .extension(Authenticated)
                .header(CWD, binding.cwd_handle.as_str());
            if let Some(value) = if_match {
                request = request.header("if-match", value);
            }
            assert_eq!(
                reviewed_http::download(Some(&manager), request.body(Body::empty()).unwrap())
                    .await
                    .status(),
                expected
            );
        }
        for (if_range, expected, body) in [
            (etag.as_str(), StatusCode::PARTIAL_CONTENT, &PAYLOAD[..2]),
            ("\"other\"", StatusCode::OK, PAYLOAD),
        ] {
            let request = Request::get(&download.download_path)
                .extension(Authenticated)
                .header(CWD, binding.cwd_handle.as_str())
                .header("if-match", &etag)
                .header("range", "bytes=0-1")
                .header("if-range", if_range)
                .body(Body::empty())
                .unwrap();
            let response = reviewed_http::download(Some(&manager), request).await;
            assert_eq!(response.status(), expected);
            assert_eq!(
                to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
                body
            );
        }
        fs::write(root.path().join(TARGET), b"new revision").unwrap();
        let request = Request::get(&download.download_path)
            .extension(Authenticated)
            .header(CWD, binding.cwd_handle.as_str())
            .header("if-match", etag)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            reviewed_http::download(Some(&manager), request)
                .await
                .status(),
            StatusCode::PRECONDITION_FAILED
        );
    }
}
