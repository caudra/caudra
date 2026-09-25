use std::{
    collections::{HashMap, VecDeque},
    io::Write,
    mem::size_of,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tokio::{
    sync::{Notify, Semaphore},
    task::AbortHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workcell_host_contract::{
    ContractVersion, Cursor, ExecuteRequest, HostBinding, Identifier, MAX_ID_BYTES,
    MAX_WATCH_LIFETIME_EVENTS, MAX_WATCH_RETAINED_BYTES, MAX_WATCH_RETAINED_EVENTS,
    MAX_WATCH_SUBSCRIPTIONS, OperationBinding, OperationIntent, OperationState, OutcomeKind,
    PrepareResponse, ProgressChunkText, ProgressEvent, ProgressMetadata, ReleaseResponse,
    RemoteHostDescriptor, RemoteOperationLimits, StatusResponse, StructuredOutcome,
    WATCH_SUBSCRIPTION_TTL_MS, WatchEvent, WatchOpenResponse, WatchPollResponse, WatchResyncReason,
    WatchState, WorkspaceRequestBinding,
};

use workcell_host_contract::SNAPSHOT_CAPTURE_CONTRACT_ID;
pub use workcell_host_contract::{
    CANCEL_METHOD, CancelRequest, CancelResponse, DISCOVER_PROJECT_ASSETS_METHOD,
    DiscoverProjectAssetsRequest, EXECUTE_METHOD, EXTENSION_ID, LIST_METHOD, ListRequest,
    PREPARE_EXEC_METHOD, PREPARE_METHOD, PREPARE_MUTATION_METHOD, PrepareExecRequest,
    PrepareMutationRequest, PrepareRequest, READ_PROJECT_ASSET_METHOD, READ_TEXT_METHOD,
    RELEASE_METHOD, RESOLVE_DIRECTORY_METHOD, ReadProjectAssetRequest, ReadTextRequest,
    ReleaseRequest, RemoteHostConfiguration, ResolveDirectoryRequest, SEARCH_TEXT_METHOD,
    STAT_METHOD, STATUS_METHOD, SearchTextRequest, StatRequest, StatusRequest, WATCH_CLOSE_METHOD,
    WATCH_OPEN_METHOD, WATCH_POLL_METHOD, WatchCloseRequest, WatchCloseResponse, WatchOpenRequest,
    WatchPollRequest,
};
use workcell_mcp_code::PreparedCode;
use workcell_mcp_code_graph::{
    PreparedCodeContext, PreparedCodeExpand, PreparedCodeImpact, PreparedCodeMap, PreparedCodeRefs,
};
use workcell_mcp_files::{
    PreparedFileEdit, PreparedFileGlob, PreparedFileGrep, PreparedFileIndex, PreparedFilePatch,
    PreparedFileRead, PreparedFileWrite, PreparedWorkspaceMutation, WorkspaceWatchBatch,
    WorkspaceWatchFailure, WorkspaceWatcher,
};
use workcell_mcp_shell::PreparedShell;
use workcell_mcp_web::{PreparedWebfetchOperation, PreparedWebsearchOperation};
use workcell_workspace_scm::PreparedScmMutation;
use workcell_workspace_snapshot::{
    PreparedSnapshotCapture, PreparedSnapshotCleanup, PreparedSnapshotRestore,
};

use crate::execution_environment::{
    PreparedExecutionEnvironment, TOOL_NAME as EXECUTION_ENVIRONMENT_TOOL,
};

const PREPARATION_TTL: Duration = Duration::from_secs(120);
const PREPARATION_ADMISSION_TIMEOUT: Duration = Duration::from_secs(30);
const RETENTION_TTL: Duration = Duration::from_secs(600);
const MAX_PREPARATIONS: usize = 64;
const MAX_OPERATIONS: usize = 128;
const MAX_LEDGER_BYTES: usize = 32 * 1_024 * 1_024;
pub(crate) const MAX_PREPARED_OPERATION_BYTES: usize = MAX_LEDGER_BYTES - 2 * 1_024 * 1_024;
pub(crate) const LARGE_PREPARATION_RESERVATION_BYTES: usize =
    MAX_LEDGER_BYTES - MAX_TOMBSTONE_LEDGER_BYTES - PREPARATION_BOOKKEEPING_BYTES;
pub(crate) const MEDIUM_PREPARATION_RESERVATION_BYTES: usize = 16 * 1_024 * 1_024;
pub(crate) const SMALL_PREPARATION_RESERVATION_BYTES: usize = 4 * 1_024 * 1_024;
const MAX_PROGRESS_BYTES: usize = 64 * 1_024;
const MAX_TOMBSTONES: usize = 256;
// Match refresh_bytes: each tombstone is charged both inline and in deque capacity.
const MAX_TOMBSTONE_LEDGER_BYTES: usize =
    MAX_TOMBSTONES * (2 * size_of::<Tombstone>() + 2 * (size_of::<Identifier>() + MAX_ID_BYTES));
const PREPARATION_BOOKKEEPING_BYTES: usize = 64 * 1_024;
const MAX_WATCH_TOMBSTONES: usize = 32;
const WATCH_TOMBSTONE_TTL: Duration = Duration::from_secs(600);
const EXECUTION_LEASE_OVERHEAD_BYTES: usize = size_of::<ExecutionLease>()
    + 2 * (size_of::<Identifier>() + workcell_host_contract::MAX_ID_BYTES * 2);

#[derive(Clone)]
pub(crate) struct RemoteHostState {
    pub descriptor: Arc<RemoteHostDescriptor>,
    pub binding: HostBinding,
    ledger: Arc<Mutex<Ledger>>,
    preparation_changed: Arc<Notify>,
    preparation_waiters: Arc<Semaphore>,
    watches: Arc<Mutex<WatchRegistry>>,
}

#[derive(Default)]
struct WatchRegistry {
    subscriptions: HashMap<Identifier, WatchSubscription>,
    tombstones: HashMap<Identifier, WatchTombstone>,
    tombstone_order: VecDeque<Identifier>,
}

struct WatchSubscription {
    binding: WorkspaceRequestBinding,
    watcher: Arc<WorkspaceWatcher>,
    poll_lock: Arc<tokio::sync::Mutex<()>>,
    expires_at: Instant,
    expires_at_unix_ms: u64,
    events: VecDeque<RetainedWatchEvent>,
    retained_bytes: usize,
    cursor_sequences: HashMap<Cursor, u64>,
    next_sequence: u64,
    expiry_task: AbortHandle,
}

struct RetainedWatchEvent {
    event: WatchEvent,
    cursor: Cursor,
    bytes: usize,
}

struct WatchTombstone {
    binding: WorkspaceRequestBinding,
    reason: WatchResyncReason,
    retained_until: Instant,
}

pub(crate) enum PreparedRemoteOperation {
    FileRead(PreparedFileRead),
    FileGlob(PreparedFileGlob),
    FileGrep(PreparedFileGrep),
    FileWrite(PreparedFileWrite),
    FileEdit(PreparedFileEdit),
    FileApplyPatch(PreparedFilePatch),
    FileIndex(PreparedFileIndex),
    CodeMap(PreparedCodeMap),
    CodeContext(PreparedCodeContext),
    CodeRefs(PreparedCodeRefs),
    CodeImpact(PreparedCodeImpact),
    CodeExpand(PreparedCodeExpand),
    Websearch(PreparedWebsearchOperation),
    Webfetch(PreparedWebfetchOperation),
    Shell(PreparedShell),
    PythonExecution(PreparedCode),
    ExecutionEnvironment(PreparedExecutionEnvironment),
    WorkspaceMutation(PreparedWorkspaceMutation),
    #[cfg(unix)]
    TransferPublication(crate::transfer::reviewed::PreparedPublication),
    ScmMutation(PreparedScmMutation),
    SnapshotCapture(PreparedSnapshotCapture),
    SnapshotRestore(PreparedSnapshotRestore),
    SnapshotUnrevert(PreparedSnapshotRestore),
    SnapshotCleanup(PreparedSnapshotCleanup),
    #[cfg(test)]
    Test(Vec<u8>),
}

impl PreparedRemoteOperation {
    pub fn is_detached(&self) -> bool {
        matches!(self, Self::SnapshotCapture(_))
    }

    pub fn supports(name: &str) -> bool {
        matches!(
            name,
            "file_read"
                | "file_glob"
                | "file_grep"
                | "file_write"
                | "file_edit"
                | "file_apply_patch"
                | "file_index"
                | "code_map"
                | "code_context"
                | "code_refs"
                | "code_impact"
                | "code_expand"
                | "websearch"
                | "webfetch"
                | "shell"
                | "python_execution"
                | EXECUTION_ENVIRONMENT_TOOL
        )
    }

    /// Conservative bytes exclusively retained by the prepared value.
    pub fn retained_bytes(&self) -> usize {
        match self {
            Self::FileRead(prepared) => prepared.retained_bytes(),
            Self::FileGlob(prepared) => prepared.retained_bytes(),
            Self::FileGrep(prepared) => prepared.retained_bytes(),
            Self::FileWrite(prepared) => prepared.retained_bytes(),
            Self::FileEdit(prepared) => prepared.retained_bytes(),
            Self::FileApplyPatch(prepared) => prepared.retained_bytes(),
            Self::FileIndex(prepared) => prepared.retained_bytes(),
            Self::CodeMap(prepared) => prepared.retained_bytes(),
            Self::CodeContext(prepared) => prepared.retained_bytes(),
            Self::CodeRefs(prepared) => prepared.retained_bytes(),
            Self::CodeImpact(prepared) => prepared.retained_bytes(),
            Self::CodeExpand(prepared) => prepared.retained_bytes(),
            Self::Websearch(prepared) => prepared.retained_bytes(),
            Self::Webfetch(prepared) => prepared.retained_bytes(),
            Self::Shell(prepared) => prepared.retained_bytes(),
            Self::PythonExecution(prepared) => prepared.retained_bytes(),
            Self::ExecutionEnvironment(prepared) => prepared.retained_bytes(),
            Self::WorkspaceMutation(prepared) => prepared.retained_bytes(),
            #[cfg(unix)]
            Self::TransferPublication(prepared) => prepared.retained_bytes(),
            Self::ScmMutation(prepared) => prepared.retained_bytes(),
            Self::SnapshotCapture(prepared) => prepared.retained_bytes(),
            Self::SnapshotRestore(prepared) | Self::SnapshotUnrevert(prepared) => {
                prepared.retained_bytes()
            }
            Self::SnapshotCleanup(prepared) => prepared.retained_bytes(),
            #[cfg(test)]
            Self::Test(bytes) => size_of::<Self>().saturating_add(bytes.capacity()),
        }
    }
}

impl RemoteHostState {
    pub fn new(descriptor: RemoteHostDescriptor, binding: HostBinding) -> Self {
        Self {
            descriptor: Arc::new(descriptor),
            binding,
            ledger: Arc::new(Mutex::new(Ledger::default())),
            preparation_changed: Arc::new(Notify::new()),
            preparation_waiters: Arc::new(Semaphore::new(MAX_PREPARATIONS)),
            watches: Arc::new(Mutex::new(WatchRegistry::default())),
        }
    }

    #[must_use]
    pub fn limits() -> RemoteOperationLimits {
        RemoteOperationLimits {
            preparation_ttl_ms: duration_ms(PREPARATION_TTL),
            max_preparations: u32::try_from(MAX_PREPARATIONS).unwrap_or(u32::MAX),
            max_operations: u32::try_from(MAX_OPERATIONS).unwrap_or(u32::MAX),
            max_ledger_bytes: u64::try_from(MAX_LEDGER_BYTES).unwrap_or(u64::MAX),
            max_argument_bytes: u64::try_from(workcell_host_contract::MAX_ARGUMENT_BYTES)
                .unwrap_or(u64::MAX),
            max_resource_intents: u32::try_from(workcell_host_contract::MAX_RESOURCE_INTENTS)
                .unwrap_or(u32::MAX),
            max_progress_events: u32::try_from(workcell_host_contract::MAX_PROGRESS_EVENTS)
                .unwrap_or(u32::MAX),
            max_progress_bytes: u64::try_from(MAX_PROGRESS_BYTES).unwrap_or(u64::MAX),
        }
    }

    pub fn validate_host(&self, host: &HostBinding) -> Result<(), RemoteOperationError> {
        if !same_remote_identity(host, &self.binding) {
            return Err(RemoteOperationError::BindingMismatch);
        }
        if host.instance_id != self.binding.instance_id {
            return Err(RemoteOperationError::InstanceMismatch);
        }
        if host != &self.binding {
            return Err(RemoteOperationError::BindingMismatch);
        }
        Ok(())
    }

    pub fn validate_workspace_host(&self, host: &HostBinding) -> Result<(), RemoteOperationError> {
        if same_host_binding_except_instance(host, &self.binding) {
            Ok(())
        } else {
            Err(RemoteOperationError::BindingMismatch)
        }
    }

    #[cfg(test)]
    fn prepare(
        &self,
        operation: PreparedRemoteOperation,
        prepared_bytes: usize,
        binding: OperationBinding,
        intent: OperationIntent,
    ) -> Result<PrepareResponse, RemoteOperationError> {
        let reserved_bytes = prepared_bytes.saturating_add(encoded_len(&binding)?);
        let reservation = self.reserve_preparation(reserved_bytes)?;
        self.prepare_reserved(reservation, operation, binding, intent)
    }

    pub fn reserve_preparation(
        &self,
        bytes: usize,
    ) -> Result<PreparationReservation, RemoteOperationError> {
        let mut ledger = self
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ledger.reserve(bytes, Instant::now())?;
        Ok(PreparationReservation {
            ledger: self.ledger.clone(),
            changed: self.preparation_changed.clone(),
            bytes,
            active: true,
        })
    }

    pub async fn reserve_preparation_wait(
        &self,
        bytes: usize,
        token: &CancellationToken,
    ) -> Result<PreparationReservation, RemoteOperationError> {
        let _waiter = self
            .preparation_waiters
            .try_acquire()
            .map_err(|_| RemoteOperationError::QuotaExceeded)?;
        let admission = async {
            loop {
                let changed = self.preparation_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let mut ledger = self
                        .ledger
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match ledger.reserve(bytes, Instant::now()) {
                        Ok(()) => {
                            return Ok(PreparationReservation {
                                ledger: self.ledger.clone(),
                                changed: self.preparation_changed.clone(),
                                bytes,
                                active: true,
                            });
                        }
                        // Only in-progress inspections release their pessimistic reservation
                        // automatically. Retained operations still require client release.
                        Err(RemoteOperationError::QuotaExceeded) if ledger.reservations > 0 => {}
                        Err(error) => return Err(error),
                    }
                }
                changed.await;
            }
        };
        tokio::select! {
            biased;
            () = token.cancelled() => Err(RemoteOperationError::Cancelled),
            result = tokio::time::timeout(PREPARATION_ADMISSION_TIMEOUT, admission) => {
                result.unwrap_or(Err(RemoteOperationError::QuotaExceeded))
            }
        }
    }

    pub fn prepare_reserved(
        &self,
        mut reservation: PreparationReservation,
        operation: PreparedRemoteOperation,
        binding: OperationBinding,
        intent: OperationIntent,
    ) -> Result<PrepareResponse, RemoteOperationError> {
        intent
            .validate()
            .map_err(|_| RemoteOperationError::ResourceLimit)?;
        let now = Instant::now();
        let expires_at = now + PREPARATION_TTL;
        let expires_at_unix_ms = unix_ms().saturating_add(duration_ms(PREPARATION_TTL));
        let preparation_id = Identifier::new(format!("prep_{}", Uuid::new_v4()))
            .map_err(|_| RemoteOperationError::Internal)?;
        let prepared_bytes = operation.retained_bytes();
        if prepared_bytes > MAX_PREPARED_OPERATION_BYTES {
            return Err(RemoteOperationError::QuotaExceeded);
        }
        let record = Record {
            preparation_id: preparation_id.clone(),
            operation: Some(operation),
            prepared_bytes,
            binding: binding.clone(),
            created_at: now,
            expires_at,
            expires_at_unix_ms,
            invocation_id: None,
            execution_id: None,
            state: OperationState::Prepared,
            cancellation: None,
            outcome: None,
            terminal_at: None,
            progress: VecDeque::new(),
            next_progress_sequence: 1,
            progress_gap: false,
        };
        reservation.resize(record.reservation_bytes())?;
        reservation.commit(record, now)?;
        Ok(PrepareResponse {
            version: ContractVersion::V1,
            preparation_id,
            expires_at_unix_ms,
            binding,
            intent,
        })
    }

    pub fn begin(
        &self,
        request: &ExecuteRequest,
        cancellation: CancellationToken,
    ) -> Result<BeginExecution, RemoteOperationError> {
        self.validate_host(&request.host)?;
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .begin(request, cancellation, self.ledger.clone(), Instant::now())
    }

    pub fn cancel_captures(&self) {
        let mut ledger = self
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ledger.captures_closed = true;
        for record in ledger.records.values() {
            if record.binding.contract.id.as_str() == SNAPSHOT_CAPTURE_CONTRACT_ID
                && let Some(token) = &record.cancellation
            {
                token.cancel();
            }
        }
    }

    pub async fn drain_captures(&self) {
        let changed = self
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .capture_finished
            .clone();
        loop {
            let notified = changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .running_captures
                == 0
            {
                return;
            }
            notified.await;
        }
    }

    pub fn finish(
        &self,
        preparation_id: &Identifier,
        invocation_id: &Identifier,
        outcome: StructuredOutcome,
    ) {
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .finish(preparation_id, invocation_id, outcome, Instant::now());
    }

    pub fn finish_indeterminate(&self, preparation_id: &Identifier, invocation_id: &Identifier) {
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .finish_indeterminate(preparation_id, invocation_id, Instant::now());
    }

    pub fn status(
        &self,
        preparation_id: &Identifier,
        invocation_id: Option<&Identifier>,
        host: &HostBinding,
    ) -> Result<StatusResponse, RemoteOperationError> {
        if !same_remote_identity(host, &self.binding) {
            return Err(RemoteOperationError::BindingMismatch);
        }
        if host.instance_id != self.binding.instance_id {
            return Ok(unknown_status(
                OperationState::Indeterminate,
                preparation_id.clone(),
                invocation_id.cloned(),
            ));
        }
        self.validate_host(host)?;
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status(preparation_id, invocation_id, Instant::now())
    }

    pub fn status_after(
        &self,
        request: &StatusRequest,
    ) -> Result<StatusResponse, RemoteOperationError> {
        let mut status = self.status(
            &request.selector.preparation_id,
            request.selector.invocation_id.as_ref(),
            &request.selector.host,
        )?;
        if let Some(after) = request.after_sequence {
            if after == u64::MAX
                || (after >= status.progress_metadata.next_sequence
                    && request.selector.host.instance_id == self.binding.instance_id)
            {
                return Err(RemoteOperationError::InvalidRequest);
            }
            status.progress.retain(|event| event.sequence > after);
        }
        Ok(status)
    }

    pub fn release(
        &self,
        preparation_id: &Identifier,
        invocation_id: Option<&Identifier>,
        host: &HostBinding,
    ) -> Result<ReleaseResponse, RemoteOperationError> {
        self.validate_host(host)?;
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .release(preparation_id, invocation_id, Instant::now())
    }

    pub fn cancel(&self, request: &CancelRequest) -> Result<CancelResponse, RemoteOperationError> {
        self.validate_host(&request.host)?;
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancel(request, Instant::now())
    }

    pub fn open_watch(
        &self,
        request: &WatchOpenRequest,
        watcher: WorkspaceWatcher,
    ) -> Result<WatchOpenResponse, RemoteOperationError> {
        self.validate_host(&request.binding.host)?;
        let now = Instant::now();
        let expires_at = now + Duration::from_millis(WATCH_SUBSCRIPTION_TTL_MS);
        let expires_at_unix_ms = unix_ms().saturating_add(WATCH_SUBSCRIPTION_TTL_MS);
        let subscription_id = Identifier::new(format!("watch_{}", Uuid::new_v4()))
            .map_err(|_| RemoteOperationError::Internal)?;
        let cursor = Cursor::new(format!("watch_cursor_{}", Uuid::new_v4()))
            .map_err(|_| RemoteOperationError::Internal)?;
        {
            let mut registry = self
                .watches
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.expire(now);
            if registry.subscriptions.len() >= MAX_WATCH_SUBSCRIPTIONS {
                return Err(RemoteOperationError::QuotaExceeded);
            }
            let expiry_task = spawn_watch_expiry(
                Arc::downgrade(&self.watches),
                subscription_id.clone(),
                expires_at,
            );
            registry.subscriptions.insert(
                subscription_id.clone(),
                WatchSubscription {
                    binding: request.binding.clone(),
                    watcher: Arc::new(watcher),
                    poll_lock: Arc::new(tokio::sync::Mutex::new(())),
                    expires_at,
                    expires_at_unix_ms,
                    events: VecDeque::new(),
                    retained_bytes: 0,
                    cursor_sequences: HashMap::from([(cursor.clone(), 0)]),
                    next_sequence: 1,
                    expiry_task,
                },
            );
        }
        Ok(WatchOpenResponse {
            version: ContractVersion::V1,
            subscription_id,
            state: WatchState::Current,
            cursor,
            expires_at_unix_ms,
        })
    }

    pub async fn poll_watch(
        &self,
        request: &WatchPollRequest,
    ) -> Result<WatchPollResponse, RemoteOperationError> {
        request
            .validate()
            .map_err(|_| RemoteOperationError::InvalidRequest)?;
        if request.binding.host.instance_id != self.binding.instance_id {
            if same_remote_identity(&request.binding.host, &self.binding) {
                return Ok(watch_resync(
                    request.subscription_id.clone(),
                    WatchResyncReason::InstanceChanged,
                ));
            }
            return Err(RemoteOperationError::BindingMismatch);
        }
        self.validate_host(&request.binding.host)?;
        let (watcher, poll_lock) = {
            let mut registry = self
                .watches
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.expire(Instant::now());
            let Some(subscription) = registry.subscriptions.get(&request.subscription_id) else {
                return registry.resync_for(request);
            };
            validate_watch_binding(&subscription.binding, &request.binding)?;
            (subscription.watcher.clone(), subscription.poll_lock.clone())
        };
        let _poll = poll_lock.lock().await;
        let batch = watcher.poll(Duration::from_millis(request.wait_ms)).await;
        self.finish_watch_poll(request, batch)
    }

    fn finish_watch_poll(
        &self,
        request: &WatchPollRequest,
        batch: WorkspaceWatchBatch,
    ) -> Result<WatchPollResponse, RemoteOperationError> {
        let mut registry = self
            .watches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.expire(Instant::now());
        let Some(subscription) = registry.subscriptions.get(&request.subscription_id) else {
            return registry.resync_for(request);
        };
        validate_watch_binding(&subscription.binding, &request.binding)?;
        if let Some(reason) = watch_failure_reason(batch.failure) {
            registry.remove(&request.subscription_id, reason, Instant::now());
            return Ok(watch_resync(request.subscription_id.clone(), reason));
        }
        if registry
            .append_events(&request.subscription_id, batch)
            .is_err()
        {
            registry.remove(
                &request.subscription_id,
                WatchResyncReason::Overflow,
                Instant::now(),
            );
            return Ok(watch_resync(
                request.subscription_id.clone(),
                WatchResyncReason::Overflow,
            ));
        }
        registry.poll_response(request)
    }

    pub fn close_watch(
        &self,
        request: &WatchCloseRequest,
    ) -> Result<WatchCloseResponse, RemoteOperationError> {
        self.validate_host(&request.binding.host)?;
        let mut registry = self
            .watches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.expire(Instant::now());
        if let Some(subscription) = registry.subscriptions.get(&request.subscription_id) {
            validate_watch_binding(&subscription.binding, &request.binding)?;
            registry.remove(
                &request.subscription_id,
                WatchResyncReason::SubscriptionClosed,
                Instant::now(),
            );
            return Ok(WatchCloseResponse {
                version: ContractVersion::V1,
                subscription_id: request.subscription_id.clone(),
                closed: true,
            });
        }
        if let Some(tombstone) = registry.tombstones.get(&request.subscription_id) {
            validate_watch_binding(&tombstone.binding, &request.binding)?;
        }
        Ok(WatchCloseResponse {
            version: ContractVersion::V1,
            subscription_id: request.subscription_id.clone(),
            closed: false,
        })
    }

    pub fn capture_progress(
        &self,
        preparation_id: &Identifier,
        invocation_id: &Identifier,
        sequence: u64,
        kind: &str,
        message: String,
    ) {
        let Ok(kind) = Identifier::new(kind) else {
            return;
        };
        let Ok(chunk) = ProgressChunkText::new(message) else {
            return;
        };
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .capture_progress(preparation_id, invocation_id, sequence, kind, chunk);
    }
}

impl WatchRegistry {
    fn append_events(
        &mut self,
        subscription_id: &Identifier,
        batch: WorkspaceWatchBatch,
    ) -> Result<(), ()> {
        let subscription = self.subscriptions.get_mut(subscription_id).ok_or(())?;
        for pending in batch.events {
            if subscription.next_sequence > MAX_WATCH_LIFETIME_EVENTS as u64 {
                return Err(());
            }
            let event = WatchEvent {
                sequence: subscription.next_sequence,
                kind: pending.kind,
                path: pending.path,
            };
            let bytes = encoded_len(&event).map_err(|_| ())?;
            if bytes > MAX_WATCH_RETAINED_BYTES {
                return Err(());
            }
            let cursor = Cursor::new(format!("watch_cursor_{}", Uuid::new_v4())).map_err(|_| ())?;
            subscription
                .cursor_sequences
                .insert(cursor.clone(), event.sequence);
            subscription.next_sequence = subscription.next_sequence.saturating_add(1);
            subscription.retained_bytes = subscription.retained_bytes.saturating_add(bytes);
            subscription.events.push_back(RetainedWatchEvent {
                event,
                cursor,
                bytes,
            });
            while subscription.events.len() > MAX_WATCH_RETAINED_EVENTS
                || subscription.retained_bytes > MAX_WATCH_RETAINED_BYTES
            {
                let Some(removed) = subscription.events.pop_front() else {
                    break;
                };
                subscription.retained_bytes =
                    subscription.retained_bytes.saturating_sub(removed.bytes);
            }
        }
        Ok(())
    }

    fn poll_response(
        &mut self,
        request: &WatchPollRequest,
    ) -> Result<WatchPollResponse, RemoteOperationError> {
        let subscription = self
            .subscriptions
            .get(&request.subscription_id)
            .ok_or(RemoteOperationError::NotFound)?;
        let Some(after_sequence) = subscription.cursor_sequences.get(&request.cursor).copied()
        else {
            self.remove(
                &request.subscription_id,
                WatchResyncReason::CursorInvalid,
                Instant::now(),
            );
            return Ok(watch_resync(
                request.subscription_id.clone(),
                WatchResyncReason::CursorInvalid,
            ));
        };
        let first_retained = subscription
            .events
            .front()
            .map(|record| record.event.sequence);
        if first_retained.is_some_and(|first| after_sequence.saturating_add(1) < first) {
            self.remove(
                &request.subscription_id,
                WatchResyncReason::RetentionLost,
                Instant::now(),
            );
            return Ok(watch_resync(
                request.subscription_id.clone(),
                WatchResyncReason::RetentionLost,
            ));
        }
        let mut bytes = 0usize;
        let mut events = Vec::new();
        let mut next_cursor = request.cursor.clone();
        for record in subscription
            .events
            .iter()
            .filter(|record| record.event.sequence > after_sequence)
        {
            if events.len() >= request.max_events as usize
                || bytes.saturating_add(record.bytes) > request.max_bytes as usize
            {
                break;
            }
            bytes = bytes.saturating_add(record.bytes);
            events.push(record.event.clone());
            next_cursor = record.cursor.clone();
        }
        Ok(WatchPollResponse {
            version: ContractVersion::V1,
            subscription_id: request.subscription_id.clone(),
            state: WatchState::Current,
            resync_reason: None,
            first_retained_sequence: first_retained,
            next_sequence: subscription.next_sequence,
            events,
            next_cursor: Some(next_cursor),
            expires_at_unix_ms: Some(subscription.expires_at_unix_ms),
        })
    }

    fn resync_for(
        &self,
        request: &WatchPollRequest,
    ) -> Result<WatchPollResponse, RemoteOperationError> {
        if let Some(tombstone) = self.tombstones.get(&request.subscription_id) {
            validate_watch_binding(&tombstone.binding, &request.binding)?;
            return Ok(watch_resync(
                request.subscription_id.clone(),
                tombstone.reason,
            ));
        }
        Ok(watch_resync(
            request.subscription_id.clone(),
            WatchResyncReason::CursorInvalid,
        ))
    }

    fn expire(&mut self, now: Instant) {
        let expired = self
            .subscriptions
            .iter()
            .filter_map(|(id, subscription)| (subscription.expires_at <= now).then_some(id.clone()))
            .collect::<Vec<_>>();
        for id in expired {
            self.remove(&id, WatchResyncReason::SubscriptionExpired, now);
        }
        while self
            .tombstone_order
            .front()
            .and_then(|id| self.tombstones.get(id))
            .is_some_and(|tombstone| tombstone.retained_until <= now)
        {
            if let Some(id) = self.tombstone_order.pop_front() {
                self.tombstones.remove(&id);
            }
        }
    }

    fn remove(&mut self, id: &Identifier, reason: WatchResyncReason, now: Instant) {
        let Some(subscription) = self.subscriptions.remove(id) else {
            return;
        };
        self.tombstones.insert(
            id.clone(),
            WatchTombstone {
                binding: subscription.binding.clone(),
                reason,
                retained_until: now + WATCH_TOMBSTONE_TTL,
            },
        );
        self.tombstone_order.push_back(id.clone());
        while self.tombstone_order.len() > MAX_WATCH_TOMBSTONES {
            if let Some(expired) = self.tombstone_order.pop_front() {
                self.tombstones.remove(&expired);
            }
        }
    }
}

impl Drop for WatchSubscription {
    fn drop(&mut self) {
        self.expiry_task.abort();
    }
}

fn spawn_watch_expiry(
    registry: Weak<Mutex<WatchRegistry>>,
    subscription_id: Identifier,
    expires_at: Instant,
) -> AbortHandle {
    tokio::spawn(async move {
        tokio::time::sleep_until(tokio::time::Instant::from_std(expires_at)).await;
        let Some(registry) = registry.upgrade() else {
            return;
        };
        registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(
                &subscription_id,
                WatchResyncReason::SubscriptionExpired,
                Instant::now(),
            );
    })
    .abort_handle()
}

fn watch_failure_reason(failure: Option<WorkspaceWatchFailure>) -> Option<WatchResyncReason> {
    failure.map(|failure| match failure {
        WorkspaceWatchFailure::Overflow => WatchResyncReason::Overflow,
        WorkspaceWatchFailure::Backend => WatchResyncReason::BackendError,
    })
}

fn validate_watch_binding(
    expected: &WorkspaceRequestBinding,
    actual: &WorkspaceRequestBinding,
) -> Result<(), RemoteOperationError> {
    if expected == actual {
        Ok(())
    } else {
        Err(RemoteOperationError::BindingMismatch)
    }
}

fn same_remote_identity(left: &HostBinding, right: &HostBinding) -> bool {
    left.server_id == right.server_id
        && left.workspace_id == right.workspace_id
        && left.workspace_generation == right.workspace_generation
        && left.root_project_id == right.root_project_id
        && left.principal_id == right.principal_id
}

fn same_host_binding_except_instance(left: &HostBinding, right: &HostBinding) -> bool {
    same_remote_identity(left, right)
        && left.cwd_handle == right.cwd_handle
        && left.catalog_revision == right.catalog_revision
        && left.policy_revision == right.policy_revision
}

fn watch_resync(subscription_id: Identifier, reason: WatchResyncReason) -> WatchPollResponse {
    WatchPollResponse {
        version: ContractVersion::V1,
        subscription_id,
        state: WatchState::FullResync,
        resync_reason: Some(reason),
        first_retained_sequence: None,
        next_sequence: 1,
        events: Vec::new(),
        next_cursor: None,
        expires_at_unix_ms: None,
    }
}

fn unknown_status(
    state: OperationState,
    preparation_id: Identifier,
    invocation_id: Option<Identifier>,
) -> StatusResponse {
    StatusResponse {
        version: ContractVersion::V1,
        state,
        preparation_id,
        invocation_id,
        execution_id: None,
        expires_at_unix_ms: None,
        binding: None,
        outcome: None,
        tombstones_evicted_through_unix_ms: None,
        progress_metadata: ProgressMetadata {
            first_retained_sequence: None,
            next_sequence: 1,
            gap_before_first: false,
        },
        progress: Vec::new(),
    }
}

pub(crate) enum BeginExecution {
    Start {
        operation: Box<PreparedRemoteOperation>,
        cancellation: CancellationToken,
        lease: ExecutionLease,
    },
    Running(Box<StatusResponse>),
    Terminal(Box<StatusResponse>),
}

pub(crate) struct ExecutionLease {
    ledger: Arc<Mutex<Ledger>>,
    bytes: usize,
    preparation_id: Identifier,
    invocation_id: Identifier,
    capture: bool,
}

impl Drop for ExecutionLease {
    fn drop(&mut self) {
        let mut ledger = self
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ledger
            .records
            .get(&self.preparation_id)
            .is_some_and(|record| record.state == OperationState::Running)
        {
            ledger.finish_indeterminate(&self.preparation_id, &self.invocation_id, Instant::now());
        }
        ledger.running_bytes = ledger.running_bytes.saturating_sub(self.bytes);
        if self.capture {
            ledger.running_captures = ledger.running_captures.saturating_sub(1);
            ledger.capture_finished.notify_waiters();
        }
    }
}

impl std::fmt::Debug for BeginExecution {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start { .. } => formatter.write_str("Start { .. }"),
            Self::Running(status) => formatter.debug_tuple("Running").field(status).finish(),
            Self::Terminal(status) => formatter.debug_tuple("Terminal").field(status).finish(),
        }
    }
}

pub(crate) struct PreparationReservation {
    ledger: Arc<Mutex<Ledger>>,
    changed: Arc<Notify>,
    bytes: usize,
    active: bool,
}

impl PreparationReservation {
    fn resize(&mut self, bytes: usize) -> Result<(), RemoteOperationError> {
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .resize_reservation(self.bytes, bytes, Instant::now())?;
        self.bytes = bytes;
        Ok(())
    }

    fn commit(mut self, record: Record, now: Instant) -> Result<(), RemoteOperationError> {
        let result = self
            .ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert_reserved(record, self.bytes, now);
        self.active = false;
        result
    }
}

impl Drop for PreparationReservation {
    fn drop(&mut self) {
        if self.active {
            self.ledger
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .rollback_reservation(self.bytes);
        }
        self.changed.notify_waiters();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemoteOperationError {
    BindingMismatch,
    InstanceMismatch,
    ContractMismatch,
    InvocationMismatch,
    NotFound,
    Forgotten,
    Expired,
    Running,
    Cancelled,
    QuotaExceeded,
    ResourceLimit,
    InvalidRequest,
    Internal,
}

impl RemoteOperationError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::BindingMismatch => "binding_mismatch",
            Self::InstanceMismatch => "instance_mismatch",
            Self::ContractMismatch => "contract_mismatch",
            Self::InvocationMismatch => "invocation_mismatch",
            Self::NotFound => "not_found",
            Self::Forgotten => "forgotten",
            Self::Expired => "expired",
            Self::Running => "running",
            Self::Cancelled => "cancelled",
            Self::QuotaExceeded => "quota_exceeded",
            Self::ResourceLimit => "resource_limit",
            Self::InvalidRequest => "invalid_request",
            Self::Internal => "internal",
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::BindingMismatch => "operation binding does not match this remote host",
            Self::InstanceMismatch => "server instance does not match this process",
            Self::ContractMismatch => "tool contract does not match the configured catalog",
            Self::InvocationMismatch => "preparation is bound to a different invocation",
            Self::NotFound => "preparation was never seen by this server instance",
            Self::Forgotten => "preparation is no longer retained",
            Self::Expired => "preparation expired before execution",
            Self::Running => "operation is already running",
            Self::Cancelled => "preparation admission was cancelled",
            Self::QuotaExceeded => "remote operation ledger quota is exhausted",
            Self::ResourceLimit => "operation intents exceed the configured limit",
            Self::InvalidRequest => "remote operation request is invalid",
            Self::Internal => "remote operation state is unavailable",
        }
    }
}

#[derive(Default)]
struct Ledger {
    records: HashMap<Identifier, Record>,
    tombstones: VecDeque<Tombstone>,
    bytes: usize,
    reserved_bytes: usize,
    running_bytes: usize,
    running_captures: usize,
    captures_closed: bool,
    capture_finished: Arc<Notify>,
    reservations: usize,
    /// Wall clock of the newest tombstone this ledger has evicted. Eviction is
    /// oldest first, so every dropped tombstone was forgotten at or before this
    /// instant and every surviving one after it. Reporting the boundary instead
    /// of a latched flag keeps `NeverSeen` meaningful: a caller can still prove
    /// its own operation was too recent to have been dropped.
    evicted_through_unix_ms: Option<u64>,
}

struct Record {
    preparation_id: Identifier,
    operation: Option<PreparedRemoteOperation>,
    prepared_bytes: usize,
    binding: OperationBinding,
    created_at: Instant,
    expires_at: Instant,
    expires_at_unix_ms: u64,
    invocation_id: Option<Identifier>,
    execution_id: Option<Identifier>,
    state: OperationState,
    cancellation: Option<CancellationToken>,
    outcome: Option<StructuredOutcome>,
    terminal_at: Option<Instant>,
    progress: VecDeque<ProgressEvent>,
    next_progress_sequence: u64,
    progress_gap: bool,
}

struct Tombstone {
    preparation_id: Identifier,
    invocation_id: Option<Identifier>,
    state: OperationState,
    progress_metadata: ProgressMetadata,
    forgotten_at: Instant,
    forgotten_at_unix_ms: u64,
}

impl Ledger {
    fn reserve(&mut self, bytes: usize, now: Instant) -> Result<(), RemoteOperationError> {
        self.prune(now);
        let prepared = self
            .records
            .values()
            .filter(|record| record.state == OperationState::Prepared)
            .count();
        if prepared.saturating_add(self.reservations) >= MAX_PREPARATIONS
            || self.records.len().saturating_add(self.reservations) >= MAX_OPERATIONS
            || self
                .bytes
                .saturating_add(self.reserved_bytes)
                .saturating_add(self.running_bytes)
                .saturating_add(bytes)
                .saturating_add(
                    self.reservations
                        .saturating_add(1)
                        .saturating_mul(size_of::<PreparationReservation>()),
                )
                > MAX_LEDGER_BYTES
        {
            return Err(RemoteOperationError::QuotaExceeded);
        }
        self.reservations = self.reservations.saturating_add(1);
        self.reserved_bytes = self.reserved_bytes.saturating_add(bytes);
        Ok(())
    }

    fn rollback_reservation(&mut self, bytes: usize) {
        self.reservations = self.reservations.saturating_sub(1);
        self.reserved_bytes = self.reserved_bytes.saturating_sub(bytes);
    }

    fn resize_reservation(
        &mut self,
        previous: usize,
        bytes: usize,
        now: Instant,
    ) -> Result<(), RemoteOperationError> {
        self.prune(now);
        let prospective = self
            .bytes
            .saturating_add(self.reserved_bytes.saturating_sub(previous))
            .saturating_add(self.running_bytes)
            .saturating_add(bytes)
            .saturating_add(
                self.reservations
                    .saturating_mul(size_of::<PreparationReservation>()),
            );
        if prospective > MAX_LEDGER_BYTES {
            return Err(RemoteOperationError::QuotaExceeded);
        }
        self.reserved_bytes = self
            .reserved_bytes
            .saturating_sub(previous)
            .saturating_add(bytes);
        Ok(())
    }

    fn insert_reserved(
        &mut self,
        record: Record,
        reserved_bytes: usize,
        now: Instant,
    ) -> Result<(), RemoteOperationError> {
        self.prune(now);
        if self.records.len() >= MAX_OPERATIONS || self.total_bytes() > MAX_LEDGER_BYTES {
            self.rollback_reservation(reserved_bytes);
            return Err(RemoteOperationError::QuotaExceeded);
        }
        let preparation_id = record.preparation_id.clone();
        self.records.insert(preparation_id.clone(), record);
        self.rollback_reservation(reserved_bytes);
        self.refresh_bytes();
        if self.total_bytes() > MAX_LEDGER_BYTES {
            self.records.remove(&preparation_id);
            self.records.shrink_to_fit();
            self.refresh_bytes();
            return Err(RemoteOperationError::QuotaExceeded);
        }
        Ok(())
    }

    fn begin(
        &mut self,
        request: &ExecuteRequest,
        cancellation: CancellationToken,
        ledger: Arc<Mutex<Ledger>>,
        now: Instant,
    ) -> Result<BeginExecution, RemoteOperationError> {
        self.prune(now);
        let missing = self.missing_for_execution(&request.preparation_id, &request.invocation_id);
        let Some(record) = self.records.get_mut(&request.preparation_id) else {
            return Err(missing);
        };
        if record.binding.host != request.host {
            return Err(RemoteOperationError::BindingMismatch);
        }
        if let Some(invocation_id) = &record.invocation_id {
            if invocation_id != &request.invocation_id {
                return Err(RemoteOperationError::InvocationMismatch);
            }
            return if record.state == OperationState::Running {
                Ok(BeginExecution::Running(Box::new(record.status())))
            } else {
                Ok(BeginExecution::Terminal(Box::new(record.status())))
            };
        }
        if now >= record.expires_at {
            return Err(RemoteOperationError::Expired);
        }
        let capture = record
            .operation
            .as_ref()
            .is_some_and(PreparedRemoteOperation::is_detached);
        if capture && self.captures_closed {
            return Err(RemoteOperationError::Cancelled);
        }
        let Some(operation) = record.operation.take() else {
            return Err(RemoteOperationError::Internal);
        };
        let cancellation = if operation.is_detached() {
            CancellationToken::new()
        } else {
            cancellation
        };
        record.invocation_id = Some(request.invocation_id.clone());
        record.execution_id = Some(request.invocation_id.clone());
        record.state = OperationState::Running;
        record.cancellation = Some(cancellation.clone());
        let lease_bytes = record
            .prepared_bytes
            .saturating_add(EXECUTION_LEASE_OVERHEAD_BYTES);
        self.running_bytes = self.running_bytes.saturating_add(lease_bytes);
        self.running_captures += usize::from(capture);
        let start = BeginExecution::Start {
            operation: Box::new(operation),
            cancellation,
            lease: ExecutionLease {
                ledger,
                capture,
                bytes: lease_bytes,
                preparation_id: request.preparation_id.clone(),
                invocation_id: request.invocation_id.clone(),
            },
        };
        self.refresh_bytes();
        Ok(start)
    }

    fn finish(
        &mut self,
        preparation_id: &Identifier,
        invocation_id: &Identifier,
        outcome: StructuredOutcome,
        now: Instant,
    ) {
        let Some(record) = self.records.get_mut(preparation_id) else {
            return;
        };
        if record.invocation_id.as_ref() != Some(invocation_id)
            || record.state != OperationState::Running
        {
            return;
        }
        record.state = if outcome.side_effects_possible {
            OperationState::Indeterminate
        } else {
            match outcome.kind {
                OutcomeKind::Completed => OperationState::Completed,
                OutcomeKind::Failed => OperationState::Failed,
                OutcomeKind::Cancelled => OperationState::Cancelled,
            }
        };
        record.cancellation = None;
        record.outcome = Some(outcome);
        record.terminal_at = Some(now);
        self.refresh_bytes();
        self.enforce_ceiling(preparation_id, now);
    }

    fn finish_indeterminate(
        &mut self,
        preparation_id: &Identifier,
        invocation_id: &Identifier,
        now: Instant,
    ) {
        let Some(record) = self.records.get_mut(preparation_id) else {
            return;
        };
        if record.invocation_id.as_ref() != Some(invocation_id)
            || record.state != OperationState::Running
        {
            return;
        }
        record.state = OperationState::Indeterminate;
        record.cancellation = None;
        record.outcome = None;
        record.terminal_at = Some(now);
        self.refresh_bytes();
        self.enforce_ceiling(preparation_id, now);
    }

    fn status(
        &mut self,
        preparation_id: &Identifier,
        invocation_id: Option<&Identifier>,
        now: Instant,
    ) -> Result<StatusResponse, RemoteOperationError> {
        self.prune(now);
        let evicted_through = self.evicted_through_unix_ms;
        if let Some(record) = self.records.get(preparation_id) {
            if let Some(invocation_id) = invocation_id
                && record.invocation_id.as_ref() != Some(invocation_id)
            {
                return Err(RemoteOperationError::InvocationMismatch);
            }
            let mut response = record.status();
            response.tombstones_evicted_through_unix_ms = evicted_through;
            return Ok(response);
        }
        let mut response = if let Some(tombstone) = self.tombstone(preparation_id) {
            tombstone.status(invocation_id)?
        } else {
            unknown_status(
                OperationState::NeverSeen,
                preparation_id.clone(),
                invocation_id.cloned(),
            )
        };
        response.tombstones_evicted_through_unix_ms = evicted_through;
        Ok(response)
    }

    fn release(
        &mut self,
        preparation_id: &Identifier,
        invocation_id: Option<&Identifier>,
        now: Instant,
    ) -> Result<ReleaseResponse, RemoteOperationError> {
        self.prune(now);
        let evicted_through = self.evicted_through_unix_ms;
        let Some(record) = self.records.get(preparation_id) else {
            if let Some(tombstone) = self.tombstone(preparation_id) {
                tombstone.validate_invocation(invocation_id)?;
                return Ok(ReleaseResponse {
                    version: ContractVersion::V1,
                    state: tombstone.state,
                    released: false,
                    tombstones_evicted_through_unix_ms: evicted_through,
                });
            }
            return Ok(ReleaseResponse {
                version: ContractVersion::V1,
                state: OperationState::NeverSeen,
                released: false,
                tombstones_evicted_through_unix_ms: evicted_through,
            });
        };
        if let Some(invocation_id) = invocation_id
            && record.invocation_id.as_ref() != Some(invocation_id)
        {
            return Err(RemoteOperationError::InvocationMismatch);
        }
        if record.state == OperationState::Running {
            return Err(RemoteOperationError::Running);
        }
        let state = record.state;
        self.forget(preparation_id, OperationState::Forgotten, now);
        Ok(ReleaseResponse {
            version: ContractVersion::V1,
            state,
            released: true,
            tombstones_evicted_through_unix_ms: evicted_through,
        })
    }

    fn cancel(
        &mut self,
        request: &CancelRequest,
        now: Instant,
    ) -> Result<CancelResponse, RemoteOperationError> {
        self.prune(now);
        let Some(record) = self.records.get(&request.preparation_id) else {
            if let Some(tombstone) = self.tombstone(&request.preparation_id) {
                tombstone.validate_invocation(Some(&request.invocation_id))?;
                return Ok(CancelResponse {
                    version: ContractVersion::V1,
                    state: tombstone.state,
                    cancellation_requested: false,
                });
            }
            return Err(RemoteOperationError::NotFound);
        };
        if record.invocation_id.as_ref() != Some(&request.invocation_id) {
            return Err(RemoteOperationError::InvocationMismatch);
        }
        let requested = record.cancellation.as_ref().is_some_and(|cancellation| {
            cancellation.cancel();
            true
        });
        Ok(CancelResponse {
            version: ContractVersion::V1,
            state: record.state,
            cancellation_requested: requested,
        })
    }

    fn capture_progress(
        &mut self,
        preparation_id: &Identifier,
        invocation_id: &Identifier,
        sequence: u64,
        kind: Identifier,
        chunk: ProgressChunkText,
    ) {
        let Some(record) = self.records.get_mut(preparation_id) else {
            return;
        };
        if record.invocation_id.as_ref() != Some(invocation_id) {
            return;
        }
        if record.state != OperationState::Running {
            return;
        }
        if sequence < record.next_progress_sequence || sequence == u64::MAX {
            return;
        }
        if sequence != record.next_progress_sequence {
            record.progress_gap = true;
        }
        record.next_progress_sequence = record
            .next_progress_sequence
            .max(sequence.saturating_add(1));
        record.progress.push_back(ProgressEvent {
            execution_id: invocation_id.clone(),
            sequence,
            kind,
            chunk,
        });
        while record.progress.len() > workcell_host_contract::MAX_PROGRESS_EVENTS
            || record.progress_payload_bytes() > MAX_PROGRESS_BYTES
        {
            record.progress.pop_front();
            record.progress_gap = true;
        }
        self.refresh_bytes();
        while self.total_bytes() > MAX_LEDGER_BYTES && self.evict_oldest_progress() {
            self.refresh_bytes();
        }
    }

    fn prune(&mut self, now: Instant) {
        let forgotten = self
            .records
            .iter()
            .filter_map(|(id, record)| {
                ((record.state == OperationState::Prepared && now >= record.expires_at)
                    || (record.is_terminal()
                        && record.terminal_at.is_some_and(|terminal_at| {
                            now.duration_since(terminal_at) >= RETENTION_TTL
                        })))
                .then_some(id.clone())
            })
            .collect::<Vec<_>>();
        for id in forgotten {
            self.forget(&id, OperationState::Forgotten, now);
        }
        while self
            .tombstones
            .front()
            .is_some_and(|entry| now.duration_since(entry.forgotten_at) >= RETENTION_TTL)
            || self.tombstones.len() > MAX_TOMBSTONES
        {
            let Some(evicted) = self.tombstones.pop_front() else {
                break;
            };
            self.note_eviction(&evicted);
        }
        self.refresh_bytes();
        while self.total_bytes() > MAX_LEDGER_BYTES {
            let Some(evicted) = self.tombstones.pop_front() else {
                break;
            };
            self.note_eviction(&evicted);
            self.refresh_bytes();
        }
    }

    fn note_eviction(&mut self, evicted: &Tombstone) {
        self.evicted_through_unix_ms = Some(
            self.evicted_through_unix_ms
                .map_or(evicted.forgotten_at_unix_ms, |through| {
                    through.max(evicted.forgotten_at_unix_ms)
                }),
        );
    }

    fn forget(&mut self, preparation_id: &Identifier, state: OperationState, now: Instant) {
        if let Some(record) = self.records.remove(preparation_id) {
            let mut progress_metadata = record.progress_metadata();
            progress_metadata.first_retained_sequence = None;
            if progress_metadata.next_sequence != 1 {
                progress_metadata.gap_before_first = true;
            }
            if self.tombstones.len() == MAX_TOMBSTONES
                && let Some(evicted) = self.tombstones.pop_front()
            {
                self.note_eviction(&evicted);
            }
            self.tombstones.push_back(Tombstone {
                preparation_id: preparation_id.clone(),
                invocation_id: record.invocation_id,
                state,
                progress_metadata,
                forgotten_at: now,
                forgotten_at_unix_ms: unix_ms(),
            });
        }
        self.records.shrink_to_fit();
        self.refresh_bytes();
    }

    fn missing_for_execution(
        &self,
        preparation_id: &Identifier,
        invocation_id: &Identifier,
    ) -> RemoteOperationError {
        if let Some(tombstone) = self.tombstone(preparation_id) {
            if tombstone
                .invocation_id
                .as_ref()
                .is_some_and(|bound| bound != invocation_id)
            {
                RemoteOperationError::InvocationMismatch
            } else {
                RemoteOperationError::Forgotten
            }
        } else {
            RemoteOperationError::NotFound
        }
    }

    fn tombstone(&self, preparation_id: &Identifier) -> Option<&Tombstone> {
        self.tombstones
            .iter()
            .find(|entry| &entry.preparation_id == preparation_id)
    }

    fn total_bytes(&self) -> usize {
        self.bytes
            .saturating_add(self.reserved_bytes)
            .saturating_add(self.running_bytes)
            .saturating_add(
                self.reservations
                    .saturating_mul(size_of::<PreparationReservation>()),
            )
    }

    fn refresh_bytes(&mut self) {
        self.bytes = self
            .records
            .values()
            .map(Record::stored_bytes)
            .chain(self.tombstones.iter().map(Tombstone::stored_bytes))
            .fold(0, usize::saturating_add)
            .saturating_add(
                self.records
                    .capacity()
                    .saturating_mul(size_of::<(Identifier, Record)>().saturating_add(1)),
            )
            .saturating_add(
                self.records
                    .keys()
                    .map(Identifier::retained_bytes)
                    .fold(0, usize::saturating_add),
            )
            .saturating_add(
                self.tombstones
                    .capacity()
                    .saturating_mul(size_of::<Tombstone>()),
            );
    }

    fn evict_oldest_progress(&mut self) -> bool {
        let candidate = self
            .records
            .iter()
            .filter(|(_, record)| !record.progress.is_empty())
            .min_by_key(|(_, record)| record.created_at)
            .map(|(id, _)| id.clone());
        let Some(record) = candidate.and_then(|id| self.records.get_mut(&id)) else {
            return false;
        };
        record.progress.pop_front();
        record.progress_gap = true;
        true
    }

    fn enforce_ceiling(&mut self, protected: &Identifier, now: Instant) {
        while self.total_bytes() > MAX_LEDGER_BYTES && self.evict_oldest_progress() {
            self.refresh_bytes();
        }
        while self.total_bytes() > MAX_LEDGER_BYTES {
            let candidate = self
                .records
                .iter()
                .filter(|(id, record)| *id != protected && record.is_terminal())
                .min_by_key(|(_, record)| record.terminal_at)
                .map(|(id, _)| id.clone());
            let Some(candidate) = candidate else { break };
            self.forget(&candidate, OperationState::Indeterminate, now);
        }
        if self.total_bytes() > MAX_LEDGER_BYTES
            && let Some(record) = self.records.get_mut(protected)
        {
            record.state = OperationState::Indeterminate;
            record.outcome = None;
            if !record.progress.is_empty() {
                record.progress.clear();
                record.progress_gap = true;
            }
            self.refresh_bytes();
        }
        while self.total_bytes() > MAX_LEDGER_BYTES && self.tombstones.pop_front().is_some() {
            self.refresh_bytes();
        }
        debug_assert!(self.total_bytes() <= MAX_LEDGER_BYTES);
    }
}

impl Record {
    fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            OperationState::Completed
                | OperationState::Failed
                | OperationState::Cancelled
                | OperationState::Indeterminate
        )
    }

    fn progress_payload_bytes(&self) -> usize {
        self.progress
            .iter()
            .map(|event| encoded_len(event).unwrap_or(MAX_PROGRESS_BYTES))
            .fold(0, usize::saturating_add)
    }

    fn progress_bytes(&self) -> usize {
        self.progress
            .iter()
            .map(ProgressEvent::retained_bytes)
            .fold(0, usize::saturating_add)
            .saturating_add(
                self.progress
                    .capacity()
                    .saturating_mul(size_of::<ProgressEvent>()),
            )
    }

    fn stored_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.preparation_id.retained_bytes())
            .saturating_add(operation_binding_bytes(&self.binding))
            .saturating_add(
                self.invocation_id
                    .as_ref()
                    .map_or(0, Identifier::retained_bytes),
            )
            .saturating_add(
                self.execution_id
                    .as_ref()
                    .map_or(0, Identifier::retained_bytes),
            )
            .saturating_add(
                self.outcome
                    .as_ref()
                    .map_or(0, StructuredOutcome::retained_bytes),
            )
            .saturating_add(self.operation.as_ref().map_or(0, |_| {
                self.prepared_bytes
                    .saturating_add(EXECUTION_LEASE_OVERHEAD_BYTES)
            }))
            .saturating_add(self.progress_bytes())
    }

    fn reservation_bytes(&self) -> usize {
        const HASH_TABLE_GROWTH_ALLOWANCE: usize = 256 * 1_024;

        self.stored_bytes()
            .saturating_add(HASH_TABLE_GROWTH_ALLOWANCE)
    }

    fn status(&self) -> StatusResponse {
        StatusResponse {
            version: ContractVersion::V1,
            state: self.state,
            preparation_id: self.preparation_id.clone(),
            invocation_id: self.invocation_id.clone(),
            execution_id: self.execution_id.clone(),
            expires_at_unix_ms: Some(self.expires_at_unix_ms),
            binding: Some(self.binding.clone()),
            outcome: self.outcome.clone(),
            tombstones_evicted_through_unix_ms: None,
            progress_metadata: self.progress_metadata(),
            progress: self.progress.iter().cloned().collect(),
        }
    }

    fn progress_metadata(&self) -> ProgressMetadata {
        ProgressMetadata {
            first_retained_sequence: self.progress.front().map(|event| event.sequence),
            next_sequence: self.next_progress_sequence,
            gap_before_first: self.progress_gap,
        }
    }
}

impl Tombstone {
    fn validate_invocation(
        &self,
        invocation_id: Option<&Identifier>,
    ) -> Result<(), RemoteOperationError> {
        if let Some(invocation_id) = invocation_id
            && self.invocation_id.as_ref() != Some(invocation_id)
        {
            return Err(RemoteOperationError::InvocationMismatch);
        }
        Ok(())
    }

    fn status(
        &self,
        invocation_id: Option<&Identifier>,
    ) -> Result<StatusResponse, RemoteOperationError> {
        self.validate_invocation(invocation_id)?;
        let mut status = unknown_status(
            self.state,
            self.preparation_id.clone(),
            self.invocation_id.clone(),
        );
        status.progress_metadata = self.progress_metadata.clone();
        Ok(status)
    }

    fn stored_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.preparation_id.retained_bytes())
            .saturating_add(
                self.invocation_id
                    .as_ref()
                    .map_or(0, Identifier::retained_bytes),
            )
    }
}

fn operation_binding_bytes(binding: &OperationBinding) -> usize {
    size_of::<OperationBinding>()
        .saturating_add(binding.host.server_id.retained_bytes())
        .saturating_add(binding.host.instance_id.retained_bytes())
        .saturating_add(binding.host.workspace_id.retained_bytes())
        .saturating_add(binding.host.workspace_generation.retained_bytes())
        .saturating_add(binding.host.root_project_id.retained_bytes())
        .saturating_add(binding.host.principal_id.retained_bytes())
        .saturating_add(binding.host.cwd_handle.retained_bytes())
        .saturating_add(binding.host.catalog_revision.retained_bytes())
        .saturating_add(binding.host.policy_revision.retained_bytes())
        .saturating_add(binding.contract.id.retained_bytes())
        .saturating_add(binding.contract.version.retained_bytes())
        .saturating_add(binding.contract.result_version.retained_bytes())
        .saturating_add(binding.argument_digest.retained_bytes())
}

fn encoded_len(value: &impl Serialize) -> Result<usize, RemoteOperationError> {
    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, value)
        .map(|()| counter.bytes)
        .map_err(|_| RemoteOperationError::InvalidRequest)
}

#[derive(Default)]
struct ByteCounter {
    bytes: usize,
}

impl Write for ByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, duration_ms)
}

#[cfg(test)]
mod tests {
    use std::sync::{Barrier, mpsc};

    use tempfile::TempDir;
    use workcell_host_contract::{
        ContractBinding, DisplayText, OperationKind, ResourceAccess, ResourceId, ResourceIntent,
        Revision, ToolResultEnvelope, WatchCloseRequest, WatchEventKind, WatchResyncReason,
        WatchState, WorkspacePath,
    };
    use workcell_mcp_files::{FileToolGroup, WorkspaceWatchEvent};

    use super::*;

    #[test]
    fn prepared_operations_can_move_into_owned_tasks() {
        fn assert_send_static<T: Send + 'static>() {}

        assert_send_static::<PreparedRemoteOperation>();
    }

    #[tokio::test]
    async fn closing_capture_admission_does_not_cancel_or_drain_other_operations() {
        let state = state();
        let prepared = prepare(&state).unwrap();
        let request = ExecuteRequest {
            version: ContractVersion::V1,
            preparation_id: prepared.preparation_id,
            invocation_id: Identifier::new("non-capture").unwrap(),
            host: state.binding.clone(),
        };
        let parent = CancellationToken::new();
        let BeginExecution::Start {
            cancellation,
            lease,
            ..
        } = state.begin(&request, parent.child_token()).unwrap()
        else {
            panic!("expected start")
        };
        state.cancel_captures();
        assert!(!cancellation.is_cancelled());
        let mut drain = Box::pin(state.drain_captures());
        std::future::poll_fn(|cx| {
            assert!(drain.as_mut().poll(cx).is_ready());
            std::task::Poll::Ready(())
        })
        .await;
        parent.cancel();
        assert!(cancellation.is_cancelled());
        drop(lease);
    }

    #[test]
    fn ledger_enforces_single_invocation_and_replays_terminal_outcome() {
        let state = state();
        let prepared = state
            .prepare(
                PreparedRemoteOperation::Test(Vec::new()),
                2,
                operation_binding(&state),
                OperationIntent {
                    kind: OperationKind::Mutate,
                    mutating: true,
                    resources: Vec::new(),
                },
            )
            .unwrap();
        let invocation_id = Identifier::new("invocation").unwrap();
        let request = ExecuteRequest {
            version: ContractVersion::V1,
            preparation_id: prepared.preparation_id.clone(),
            invocation_id: invocation_id.clone(),
            host: state.binding.clone(),
        };
        let BeginExecution::Start { lease, .. } =
            state.begin(&request, CancellationToken::new()).unwrap()
        else {
            panic!("execution did not start")
        };
        assert!(
            state
                .ledger
                .lock()
                .unwrap()
                .records
                .get(&prepared.preparation_id)
                .unwrap()
                .operation
                .is_none(),
            "the ledger must drop its prepared value when execution starts"
        );
        state.finish(
            &prepared.preparation_id,
            &invocation_id,
            StructuredOutcome {
                kind: OutcomeKind::Completed,
                side_effects_possible: false,
                result: Some(
                    ToolResultEnvelope::new(
                        Vec::new(),
                        Some(serde_json::json!({"ok": true})),
                        false,
                    )
                    .unwrap(),
                ),
                error: None,
            },
        );
        drop(lease);
        assert!(matches!(
            state.begin(&request, CancellationToken::new()).unwrap(),
            BeginExecution::Terminal(_)
        ));
        let mut other = request;
        other.invocation_id = Identifier::new("other").unwrap();
        assert_eq!(
            state.begin(&other, CancellationToken::new()).unwrap_err(),
            RemoteOperationError::InvocationMismatch
        );
    }

    #[test]
    fn release_leaves_a_forgotten_tombstone_and_instance_changes_are_indeterminate() {
        let state = state();
        let prepared = state
            .prepare(
                PreparedRemoteOperation::Test(Vec::new()),
                2,
                operation_binding(&state),
                OperationIntent {
                    kind: OperationKind::Read,
                    mutating: false,
                    resources: Vec::new(),
                },
            )
            .unwrap();
        let release = state
            .release(&prepared.preparation_id, None, &state.binding)
            .unwrap();
        assert!(release.released);
        assert_eq!(
            state
                .status(&prepared.preparation_id, None, &state.binding)
                .unwrap()
                .state,
            OperationState::Forgotten
        );
        let repeated = state
            .release(&prepared.preparation_id, None, &state.binding)
            .unwrap();
        assert!(!repeated.released);
        assert_eq!(repeated.state, OperationState::Forgotten);
        assert_eq!(
            state
                .release(
                    &prepared.preparation_id,
                    Some(&Identifier::new("wrong").unwrap()),
                    &state.binding,
                )
                .unwrap_err(),
            RemoteOperationError::InvocationMismatch
        );
        let mut restarted = state.binding.clone();
        restarted.instance_id = Identifier::new("other-instance").unwrap();
        assert_eq!(
            state
                .status(&prepared.preparation_id, None, &restarted)
                .unwrap()
                .state,
            OperationState::Indeterminate
        );
        restarted.workspace_generation = Identifier::new("other-generation").unwrap();
        assert_eq!(
            state
                .status(&prepared.preparation_id, None, &restarted)
                .unwrap_err(),
            RemoteOperationError::BindingMismatch
        );
    }

    #[test]
    fn durable_workspace_binding_ignores_only_the_process_instance() {
        let state = state();
        let mut restarted = state.binding.clone();
        restarted.instance_id = Identifier::new("other-instance").unwrap();

        assert!(state.validate_workspace_host(&restarted).is_ok());
        assert_eq!(
            state.validate_host(&restarted).unwrap_err(),
            RemoteOperationError::InstanceMismatch
        );
        restarted.workspace_generation = Identifier::new("other-generation").unwrap();
        assert_eq!(
            state.validate_workspace_host(&restarted).unwrap_err(),
            RemoteOperationError::BindingMismatch
        );
    }

    #[test]
    fn preparation_count_is_bounded() {
        let state = state();
        for _ in 0..MAX_PREPARATIONS {
            prepare(&state).unwrap();
        }
        assert_eq!(
            prepare(&state).unwrap_err(),
            RemoteOperationError::QuotaExceeded
        );
    }

    #[test]
    fn expiry_prevents_replay_and_leaves_a_forgotten_status() {
        let state = state();
        let prepared = prepare(&state).unwrap();
        state
            .ledger
            .lock()
            .unwrap()
            .records
            .get_mut(&prepared.preparation_id)
            .unwrap()
            .expires_at = Instant::now() - Duration::from_millis(1);
        assert_eq!(
            state
                .status(&prepared.preparation_id, None, &state.binding)
                .unwrap()
                .state,
            OperationState::Forgotten
        );
        let request = ExecuteRequest {
            version: ContractVersion::V1,
            preparation_id: prepared.preparation_id,
            invocation_id: Identifier::new("late").unwrap(),
            host: state.binding.clone(),
        };
        assert_eq!(
            state.begin(&request, CancellationToken::new()).unwrap_err(),
            RemoteOperationError::Forgotten
        );
    }

    #[test]
    fn cancellation_reaches_a_running_execution_and_progress_replays_in_order() {
        let state = state();
        let prepared = prepare(&state).unwrap();
        let invocation_id = Identifier::new("active").unwrap();
        let request = ExecuteRequest {
            version: ContractVersion::V1,
            preparation_id: prepared.preparation_id.clone(),
            invocation_id: invocation_id.clone(),
            host: state.binding.clone(),
        };
        let BeginExecution::Start {
            cancellation,
            lease,
            ..
        } = state.begin(&request, CancellationToken::new()).unwrap()
        else {
            panic!("execution did not start");
        };
        assert_eq!(
            state
                .status(
                    &prepared.preparation_id,
                    Some(&invocation_id),
                    &state.binding,
                )
                .unwrap()
                .state,
            OperationState::Running
        );
        for sequence in 1..=300 {
            state.capture_progress(
                &prepared.preparation_id,
                &invocation_id,
                sequence,
                "stdout",
                sequence.to_string(),
            );
        }
        let cancelled = state
            .cancel(&CancelRequest {
                version: ContractVersion::V1,
                preparation_id: prepared.preparation_id.clone(),
                invocation_id: invocation_id.clone(),
                host: state.binding.clone(),
            })
            .unwrap();
        assert!(cancelled.cancellation_requested);
        assert!(cancellation.is_cancelled());
        state.finish(
            &prepared.preparation_id,
            &invocation_id,
            StructuredOutcome {
                kind: OutcomeKind::Cancelled,
                side_effects_possible: false,
                result: None,
                error: None,
            },
        );
        drop(lease);
        let status = state
            .status(
                &prepared.preparation_id,
                Some(&invocation_id),
                &state.binding,
            )
            .unwrap();
        assert_eq!(status.state, OperationState::Cancelled);
        assert_eq!(
            status.progress.len(),
            workcell_host_contract::MAX_PROGRESS_EVENTS
        );
        assert_eq!(status.progress.first().unwrap().sequence, 45);
        assert_eq!(status.progress.last().unwrap().sequence, 300);
        assert_eq!(status.progress_metadata.first_retained_sequence, Some(45));
        assert_eq!(status.progress_metadata.next_sequence, 301);
        assert!(status.progress_metadata.gap_before_first);
        for after in [0, 44, 45, 299, 300] {
            let replay = state
                .status_after(&StatusRequest {
                    version: ContractVersion::V1,
                    selector: workcell_host_contract::OperationSelector {
                        preparation_id: prepared.preparation_id.clone(),
                        invocation_id: Some(invocation_id.clone()),
                        host: state.binding.clone(),
                    },
                    after_sequence: Some(after),
                })
                .unwrap();
            assert_eq!(replay.progress_metadata, status.progress_metadata);
            assert_eq!(
                replay.progress,
                status
                    .progress
                    .iter()
                    .filter(|event| event.sequence > after)
                    .cloned()
                    .collect::<Vec<_>>()
            );
            replay.validate().unwrap();
        }
    }

    #[test]
    fn producer_sized_progress_with_newlines_and_controls_survives_status_serialization() {
        let state = state();
        let prepared = prepare(&state).unwrap();
        let invocation_id = Identifier::new("active").unwrap();
        let request = ExecuteRequest {
            version: ContractVersion::V1,
            preparation_id: prepared.preparation_id.clone(),
            invocation_id: invocation_id.clone(),
            host: state.binding.clone(),
        };
        let BeginExecution::Start { lease, .. } =
            state.begin(&request, CancellationToken::new()).unwrap()
        else {
            panic!("execution did not start")
        };
        let prefix = "line one\n\0\u{1b}";
        let chunk = format!(
            "{prefix}{}",
            "x".repeat(workcell_host_contract::MAX_PROGRESS_CHUNK_BYTES - prefix.len())
        );

        state.capture_progress(
            &prepared.preparation_id,
            &invocation_id,
            1,
            "stdout",
            chunk.clone(),
        );
        let encoded = serde_json::to_value(
            state
                .status(
                    &prepared.preparation_id,
                    Some(&invocation_id),
                    &state.binding,
                )
                .unwrap(),
        )
        .unwrap();
        let decoded: StatusResponse = serde_json::from_value(encoded).unwrap();

        assert_eq!(decoded.progress.len(), 1);
        assert_eq!(decoded.progress[0].chunk.as_str(), chunk);
        assert_eq!(decoded.progress_metadata.first_retained_sequence, Some(1));
        assert_eq!(decoded.progress_metadata.next_sequence, 2);
        assert!(!decoded.progress_metadata.gap_before_first);
        let replay = state
            .status_after(&StatusRequest {
                version: ContractVersion::V1,
                selector: workcell_host_contract::OperationSelector {
                    preparation_id: prepared.preparation_id.clone(),
                    invocation_id: Some(invocation_id.clone()),
                    host: state.binding.clone(),
                },
                after_sequence: Some(1),
            })
            .unwrap();
        assert!(replay.progress.is_empty());
        assert_eq!(replay.progress_metadata, decoded.progress_metadata);
        replay.validate().unwrap();
        drop(lease);
    }

    #[test]
    fn cancellation_after_possible_effects_is_indeterminate_not_cleanly_cancelled() {
        let state = state();
        let prepared = prepare(&state).unwrap();
        let invocation_id = Identifier::new("effectful-cancel").unwrap();
        let BeginExecution::Start { lease, .. } = state
            .begin(
                &ExecuteRequest {
                    version: ContractVersion::V1,
                    preparation_id: prepared.preparation_id.clone(),
                    invocation_id: invocation_id.clone(),
                    host: state.binding.clone(),
                },
                CancellationToken::new(),
            )
            .unwrap()
        else {
            panic!("execution did not start")
        };
        state.finish(
            &prepared.preparation_id,
            &invocation_id,
            StructuredOutcome {
                kind: OutcomeKind::Cancelled,
                side_effects_possible: true,
                result: None,
                error: None,
            },
        );
        drop(lease);

        let status = state
            .status(
                &prepared.preparation_id,
                Some(&invocation_id),
                &state.binding,
            )
            .unwrap();
        assert_eq!(status.state, OperationState::Indeterminate);
        assert!(status.outcome.unwrap().side_effects_possible);
    }

    #[test]
    fn concurrent_duplicate_execution_starts_exactly_once() {
        let state = state();
        let prepared = prepare(&state).unwrap();
        let request = ExecuteRequest {
            version: ContractVersion::V1,
            preparation_id: prepared.preparation_id,
            invocation_id: Identifier::new("concurrent").unwrap(),
            host: state.binding.clone(),
        };
        let barrier = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let state = state.clone();
            let request = request.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                state.begin(&request, CancellationToken::new()).unwrap()
            }));
        }
        barrier.wait();
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, BeginExecution::Start { .. }))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, BeginExecution::Running(_)))
                .count(),
            1
        );
    }

    #[test]
    fn preparation_reservations_bound_inspection_and_roll_back_every_failure() {
        let state = state();
        let reservations = (0..MAX_PREPARATIONS)
            .map(|_| state.reserve_preparation(1).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            state.reserve_preparation(1).err(),
            Some(RemoteOperationError::QuotaExceeded)
        );
        drop(reservations);
        {
            let ledger = state.ledger.lock().unwrap();
            assert_eq!(ledger.reservations, 0);
            assert_eq!(ledger.reserved_bytes, 0);
        }

        let reservation = state.reserve_preparation(1).unwrap();
        let resource = ResourceIntent {
            scope: vec![ResourceId::new("resource").unwrap()],
            resource_id: ResourceId::new("resource").unwrap(),
            display: DisplayText::new("resource").unwrap(),
            access: ResourceAccess::Read,
            revision: None,
        };
        assert_eq!(
            state
                .prepare_reserved(
                    reservation,
                    PreparedRemoteOperation::Test(Vec::new()),
                    operation_binding(&state),
                    OperationIntent {
                        kind: OperationKind::Read,
                        mutating: false,
                        resources: vec![resource; workcell_host_contract::MAX_RESOURCE_INTENTS + 1],
                    },
                )
                .unwrap_err(),
            RemoteOperationError::ResourceLimit
        );
        let ledger = state.ledger.lock().unwrap();
        assert_eq!(ledger.reservations, 0);
        assert_eq!(ledger.reserved_bytes, 0);
        assert!(ledger.records.is_empty());
    }

    #[tokio::test]
    async fn overlapping_shell_and_write_preparations_wait_for_inspection_not_execution() {
        let state = state();
        let first = state
            .reserve_preparation(LARGE_PREPARATION_RESERVATION_BYTES)
            .unwrap();
        // These are the actual shell and file_write reservation sizes on a fresh ledger.
        for bytes in [
            LARGE_PREPARATION_RESERVATION_BYTES,
            MEDIUM_PREPARATION_RESERVATION_BYTES,
        ] {
            assert_eq!(
                state.reserve_preparation(bytes).err(),
                Some(RemoteOperationError::QuotaExceeded)
            );
        }
        let token = CancellationToken::new();
        let shell = state.reserve_preparation_wait(LARGE_PREPARATION_RESERVATION_BYTES, &token);
        let write = state.reserve_preparation_wait(MEDIUM_PREPARATION_RESERVATION_BYTES, &token);
        tokio::pin!(shell, write);
        std::future::poll_fn(|cx| {
            assert!(shell.as_mut().poll(cx).is_pending());
            assert!(write.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let first = state
            .prepare_reserved(
                first,
                PreparedRemoteOperation::Test(Vec::new()),
                operation_binding(&state),
                OperationIntent {
                    kind: OperationKind::Read,
                    mutating: false,
                    resources: Vec::new(),
                },
            )
            .unwrap();
        tokio::join!(
            async {
                drop(shell.await.unwrap());
            },
            async {
                drop(write.await.unwrap());
            }
        );
        let ledger = state.ledger.lock().unwrap();
        assert_eq!(ledger.reservations, 0);
        assert_eq!(ledger.reserved_bytes, 0);
        assert!(ledger.total_bytes() <= MAX_LEDGER_BYTES);
        assert_eq!(
            ledger.records[&first.preparation_id].state,
            OperationState::Prepared
        );
    }

    #[test]
    fn releasing_preparations_reclaims_table_capacity_for_shell_inspection() {
        let state = state();
        let prepared = (0..MAX_PREPARATIONS)
            .map(|_| prepare(&state).unwrap())
            .collect::<Vec<_>>();
        for prepared in prepared {
            state
                .release(&prepared.preparation_id, None, &state.binding)
                .unwrap();
        }
        let ledger = state.ledger.lock().unwrap();
        let retained = ledger.total_bytes();
        let capacity = ledger.records.capacity();
        drop(ledger);
        assert!(
            state
                .reserve_preparation(LARGE_PREPARATION_RESERVATION_BYTES)
                .is_ok(),
            "empty operation table retains {retained} bytes with capacity {capacity}"
        );
    }

    #[test]
    fn executed_operation_churn_preserves_replay_and_large_preparation_capacity() {
        let state = state();
        let mut requests = Vec::new();
        for index in 0..MAX_TOMBSTONES * 2 {
            let prepared = prepare(&state).unwrap();
            let invocation = format!(
                "invocation-{index:0width$}",
                width = MAX_ID_BYTES - "invocation-".len()
            );
            let request = ExecuteRequest {
                version: ContractVersion::V1,
                preparation_id: prepared.preparation_id,
                invocation_id: Identifier::new(invocation).unwrap(),
                host: state.binding.clone(),
            };
            let BeginExecution::Start { lease, .. } =
                state.begin(&request, CancellationToken::new()).unwrap()
            else {
                panic!("execution did not start");
            };
            state.finish(
                &request.preparation_id,
                &request.invocation_id,
                successful_outcome(0),
            );
            drop(lease);
            state
                .release(
                    &request.preparation_id,
                    Some(&request.invocation_id),
                    &state.binding,
                )
                .unwrap();
            requests.push(request);
            if requests.len() >= MAX_TOMBSTONES {
                let reservation = state.reserve_preparation(LARGE_PREPARATION_RESERVATION_BYTES);
                let ledger = state.ledger.lock().unwrap();
                assert!(
                    reservation.is_ok(),
                    "idle replay ledger retains {} bytes, {} tombstones, deque capacity {}",
                    ledger.total_bytes(),
                    ledger.tombstones.len(),
                    ledger.tombstones.capacity()
                );
                assert!(ledger.total_bytes() <= MAX_LEDGER_BYTES);
                assert!(ledger.bytes <= MAX_TOMBSTONE_LEDGER_BYTES);
                assert_eq!(ledger.tombstones.len(), MAX_TOMBSTONES);
                assert!(ledger.tombstones.capacity() <= MAX_TOMBSTONES);
            }
        }
        assert_eq!(
            state
                .begin(&requests[0], CancellationToken::new())
                .unwrap_err(),
            RemoteOperationError::NotFound
        );
        for request in &requests[MAX_TOMBSTONES..] {
            assert_eq!(
                state.begin(request, CancellationToken::new()).unwrap_err(),
                RemoteOperationError::Forgotten
            );
            let mut other = request.clone();
            other.invocation_id = Identifier::new("different-invocation").unwrap();
            assert_eq!(
                state.begin(&other, CancellationToken::new()).unwrap_err(),
                RemoteOperationError::InvocationMismatch
            );
        }
        let reservation = state
            .reserve_preparation(LARGE_PREPARATION_RESERVATION_BYTES)
            .unwrap();
        state
            .prepare_reserved(
                reservation,
                PreparedRemoteOperation::Test(vec![
                    0;
                    MAX_PREPARED_OPERATION_BYTES
                        - size_of::<PreparedRemoteOperation>()
                ]),
                operation_binding(&state),
                OperationIntent {
                    kind: OperationKind::Read,
                    mutating: false,
                    resources: Vec::new(),
                },
            )
            .unwrap();
        assert!(state.ledger.lock().unwrap().total_bytes() <= MAX_LEDGER_BYTES);
    }

    #[tokio::test(start_paused = true)]
    async fn preparation_admission_cancels_times_out_and_preserves_retained_quota() {
        let state = state();
        let first = state
            .reserve_preparation(LARGE_PREPARATION_RESERVATION_BYTES)
            .unwrap();
        for cancel in [true, false] {
            let token = CancellationToken::new();
            let pending =
                state.reserve_preparation_wait(LARGE_PREPARATION_RESERVATION_BYTES, &token);
            tokio::pin!(pending);
            std::future::poll_fn(|cx| {
                assert!(pending.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            let expected = if cancel {
                token.cancel();
                RemoteOperationError::Cancelled
            } else {
                tokio::time::advance(PREPARATION_ADMISSION_TIMEOUT).await;
                RemoteOperationError::QuotaExceeded
            };
            assert_eq!(pending.await.err(), Some(expected));
            assert_eq!(state.ledger.lock().unwrap().reservations, 1);
            assert_eq!(
                state.preparation_waiters.available_permits(),
                MAX_PREPARATIONS
            );
        }
        state
            .prepare_reserved(
                first,
                PreparedRemoteOperation::Test(vec![0; MEDIUM_PREPARATION_RESERVATION_BYTES]),
                operation_binding(&state),
                OperationIntent {
                    kind: OperationKind::Read,
                    mutating: false,
                    resources: Vec::new(),
                },
            )
            .unwrap();
        assert_eq!(
            state
                .reserve_preparation_wait(
                    LARGE_PREPARATION_RESERVATION_BYTES,
                    &CancellationToken::new()
                )
                .await
                .err(),
            Some(RemoteOperationError::QuotaExceeded)
        );
        assert_eq!(state.ledger.lock().unwrap().records.len(), 1);
    }

    #[test]
    fn concurrent_preparation_reservations_never_overcommit_the_global_bound() {
        const CONTENDERS: usize = 12;

        let state = state();
        let start = Arc::new(Barrier::new(CONTENDERS + 1));
        let release = Arc::new(Barrier::new(CONTENDERS + 1));
        let (sender, receiver) = mpsc::channel();
        let mut threads = Vec::new();
        for _ in 0..CONTENDERS {
            let state = state.clone();
            let start = start.clone();
            let release = release.clone();
            let sender = sender.clone();
            threads.push(std::thread::spawn(move || {
                start.wait();
                let reservation = state.reserve_preparation(SMALL_PREPARATION_RESERVATION_BYTES);
                sender.send(reservation.is_ok()).unwrap();
                release.wait();
                drop(reservation);
            }));
        }
        drop(sender);
        start.wait();
        let accepted = (0..CONTENDERS)
            .map(|_| receiver.recv().unwrap())
            .filter(|accepted| *accepted)
            .count();
        {
            let ledger = state.ledger.lock().unwrap();
            assert!(ledger.total_bytes() <= MAX_LEDGER_BYTES);
            assert!(accepted > 0 && accepted < CONTENDERS);
            assert_eq!(ledger.reservations, accepted);
        }
        release.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        let ledger = state.ledger.lock().unwrap();
        assert_eq!(ledger.total_bytes(), ledger.bytes);
        assert!(ledger.total_bytes() <= MAX_LEDGER_BYTES);
    }

    #[test]
    fn multiple_near_limit_running_operations_stay_charged_while_owned_tasks_wait() {
        const OPERATION_BYTES: usize = 10 * 1_024 * 1_024;

        let state = state();
        let mut executions = Vec::new();
        let mut expected_running_bytes = 0usize;
        for index in 0..2 {
            let prepared = state
                .prepare(
                    PreparedRemoteOperation::Test(vec![0; OPERATION_BYTES]),
                    OPERATION_BYTES,
                    operation_binding(&state),
                    OperationIntent {
                        kind: OperationKind::Mutate,
                        mutating: true,
                        resources: Vec::new(),
                    },
                )
                .unwrap();
            let invocation_id = Identifier::new(format!("waiting-{index}")).unwrap();
            expected_running_bytes = expected_running_bytes.saturating_add(
                state
                    .ledger
                    .lock()
                    .unwrap()
                    .records
                    .get(&prepared.preparation_id)
                    .unwrap()
                    .prepared_bytes
                    .saturating_add(EXECUTION_LEASE_OVERHEAD_BYTES),
            );
            let execution = state
                .begin(
                    &ExecuteRequest {
                        version: ContractVersion::V1,
                        preparation_id: prepared.preparation_id,
                        invocation_id,
                        host: state.binding.clone(),
                    },
                    CancellationToken::new(),
                )
                .unwrap();
            executions.push(execution);
        }

        {
            let ledger = state.ledger.lock().unwrap();
            assert_eq!(ledger.running_bytes, expected_running_bytes);
            assert!(ledger.total_bytes() <= MAX_LEDGER_BYTES);
        }
        assert_eq!(
            state.reserve_preparation(16 * 1_024 * 1_024).err(),
            Some(RemoteOperationError::QuotaExceeded)
        );

        drop(executions);
        assert_eq!(state.ledger.lock().unwrap().running_bytes, 0);
    }

    #[test]
    fn terminal_outcomes_never_exceed_the_global_ledger_ceiling() {
        const OUTCOME_BYTES: usize = 12 * 1_024 * 1_024;

        let state = state();
        let mut executions = Vec::new();
        for index in 0..3 {
            let prepared = prepare(&state).unwrap();
            let invocation_id = Identifier::new(format!("large-{index}")).unwrap();
            let request = ExecuteRequest {
                version: ContractVersion::V1,
                preparation_id: prepared.preparation_id.clone(),
                invocation_id: invocation_id.clone(),
                host: state.binding.clone(),
            };
            let BeginExecution::Start { lease, .. } =
                state.begin(&request, CancellationToken::new()).unwrap()
            else {
                panic!("execution did not start")
            };
            state.finish(
                &prepared.preparation_id,
                &invocation_id,
                successful_outcome(OUTCOME_BYTES),
            );
            drop(lease);
            let ledger = state.ledger.lock().unwrap();
            assert!(ledger.total_bytes() <= MAX_LEDGER_BYTES);
            drop(ledger);
            executions.push((prepared.preparation_id, invocation_id));
        }

        let oldest = state
            .status(&executions[0].0, Some(&executions[0].1), &state.binding)
            .unwrap();
        assert_eq!(oldest.state, OperationState::Indeterminate);
        assert!(oldest.outcome.is_none());
        assert_eq!(
            state
                .begin(
                    &ExecuteRequest {
                        version: ContractVersion::V1,
                        preparation_id: executions[0].0.clone(),
                        invocation_id: executions[0].1.clone(),
                        host: state.binding.clone(),
                    },
                    CancellationToken::new(),
                )
                .unwrap_err(),
            RemoteOperationError::Forgotten
        );
    }

    #[test]
    fn terminal_retention_starts_when_a_long_execution_finishes() {
        let state = state();
        let prepared = prepare(&state).unwrap();
        let invocation_id = Identifier::new("long-running").unwrap();
        let request = ExecuteRequest {
            version: ContractVersion::V1,
            preparation_id: prepared.preparation_id.clone(),
            invocation_id: invocation_id.clone(),
            host: state.binding.clone(),
        };
        let BeginExecution::Start { lease, .. } =
            state.begin(&request, CancellationToken::new()).unwrap()
        else {
            panic!("execution did not start")
        };
        let completion = Instant::now();
        let mut ledger = state.ledger.lock().unwrap();
        ledger
            .records
            .get_mut(&prepared.preparation_id)
            .unwrap()
            .created_at = completion - RETENTION_TTL - Duration::from_secs(1);
        ledger.finish(
            &prepared.preparation_id,
            &invocation_id,
            successful_outcome(1),
            completion,
        );
        drop(ledger);
        drop(lease);
        let mut ledger = state.ledger.lock().unwrap();
        ledger.prune(completion + RETENTION_TTL - Duration::from_millis(1));
        assert!(ledger.records.contains_key(&prepared.preparation_id));
        ledger.prune(completion + RETENTION_TTL);
        assert!(!ledger.records.contains_key(&prepared.preparation_id));
    }

    #[test]
    fn producer_sequences_and_explicit_gap_metadata_survive_drops() {
        let state = state();
        let prepared = prepare(&state).unwrap();
        let invocation_id = Identifier::new("gapped").unwrap();
        let request = ExecuteRequest {
            version: ContractVersion::V1,
            preparation_id: prepared.preparation_id.clone(),
            invocation_id: invocation_id.clone(),
            host: state.binding.clone(),
        };
        let BeginExecution::Start { lease, .. } =
            state.begin(&request, CancellationToken::new()).unwrap()
        else {
            panic!("execution did not start")
        };
        state.capture_progress(
            &prepared.preparation_id,
            &invocation_id,
            7,
            "stdout",
            "a".into(),
        );
        state.capture_progress(
            &prepared.preparation_id,
            &invocation_id,
            9,
            "stderr",
            "b".into(),
        );

        let status = state
            .status(
                &prepared.preparation_id,
                Some(&invocation_id),
                &state.binding,
            )
            .unwrap();
        assert_eq!(
            status
                .progress
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            [7, 9]
        );
        assert_eq!(status.progress_metadata.first_retained_sequence, Some(7));
        assert_eq!(status.progress_metadata.next_sequence, 10);
        assert!(status.progress_metadata.gap_before_first);
        state.capture_progress(
            &prepared.preparation_id,
            &invocation_id,
            9,
            "stdout",
            "duplicate".into(),
        );
        state.capture_progress(
            &prepared.preparation_id,
            &invocation_id,
            8,
            "stdout",
            "late".into(),
        );
        let mut request = StatusRequest {
            version: ContractVersion::V1,
            selector: workcell_host_contract::OperationSelector {
                preparation_id: prepared.preparation_id.clone(),
                invocation_id: Some(invocation_id.clone()),
                host: state.binding.clone(),
            },
            after_sequence: None,
        };
        assert_eq!(state.status_after(&request).unwrap(), status);
        let wire = serde_json::to_value(&request).unwrap();
        assert!(wire.get("afterSequence").is_none());
        assert_eq!(
            serde_json::from_value::<StatusRequest>(wire.clone()).unwrap(),
            request
        );
        let mut unknown = wire;
        unknown["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<StatusRequest>(unknown).is_err());
        for after in [0, 6, 7, 8, 9] {
            request.after_sequence = Some(after);
            assert_eq!(
                serde_json::from_value::<StatusRequest>(serde_json::to_value(&request).unwrap())
                    .unwrap(),
                request
            );
            let replay = state.status_after(&request).unwrap();
            assert_eq!(replay.progress_metadata, status.progress_metadata);
            assert_eq!(
                replay.progress,
                status
                    .progress
                    .iter()
                    .filter(|event| event.sequence > after)
                    .cloned()
                    .collect::<Vec<_>>()
            );
            replay.validate().unwrap();
        }
        request.after_sequence = Some(10);
        assert!(matches!(
            state.status_after(&request),
            Err(RemoteOperationError::InvalidRequest)
        ));
        let mut wire = serde_json::to_value(&request).unwrap();
        for cursor in [
            serde_json::json!(-1),
            serde_json::json!(u64::MAX),
            serde_json::json!(1.5),
        ] {
            wire["afterSequence"] = cursor;
            assert!(serde_json::from_value::<StatusRequest>(wire.clone()).is_err());
        }

        state.finish(
            &prepared.preparation_id,
            &invocation_id,
            successful_outcome(1),
        );
        drop(lease);
        state
            .release(
                &prepared.preparation_id,
                Some(&invocation_id),
                &state.binding,
            )
            .unwrap();
        let forgotten = state
            .status(
                &prepared.preparation_id,
                Some(&invocation_id),
                &state.binding,
            )
            .unwrap();
        assert!(forgotten.progress.is_empty());
        assert_eq!(forgotten.progress_metadata.first_retained_sequence, None);
        assert_eq!(forgotten.progress_metadata.next_sequence, 10);
        assert!(forgotten.progress_metadata.gap_before_first);
        forgotten.validate().unwrap();
    }

    #[test]
    fn late_cancellation_cannot_replace_a_successful_terminal_outcome() {
        let state = state();
        let prepared = prepare(&state).unwrap();
        let invocation_id = Identifier::new("completed").unwrap();
        let request = ExecuteRequest {
            version: ContractVersion::V1,
            preparation_id: prepared.preparation_id.clone(),
            invocation_id: invocation_id.clone(),
            host: state.binding.clone(),
        };
        let BeginExecution::Start { lease, .. } =
            state.begin(&request, CancellationToken::new()).unwrap()
        else {
            panic!("execution did not start")
        };
        state.finish(
            &prepared.preparation_id,
            &invocation_id,
            successful_outcome(1),
        );
        drop(lease);

        for _ in 0..2 {
            let cancelled = state
                .cancel(&CancelRequest {
                    version: ContractVersion::V1,
                    preparation_id: prepared.preparation_id.clone(),
                    invocation_id: invocation_id.clone(),
                    host: state.binding.clone(),
                })
                .unwrap();
            assert_eq!(cancelled.state, OperationState::Completed);
            assert!(!cancelled.cancellation_requested);
        }
    }

    #[test]
    fn standard_parent_cancellation_reaches_the_operation_token() {
        let state = state();
        let prepared = prepare(&state).unwrap();
        let parent = CancellationToken::new();
        let request = ExecuteRequest {
            version: ContractVersion::V1,
            preparation_id: prepared.preparation_id,
            invocation_id: Identifier::new("standard-cancel").unwrap(),
            host: state.binding.clone(),
        };
        let BeginExecution::Start {
            cancellation,
            lease,
            ..
        } = state.begin(&request, parent.child_token()).unwrap()
        else {
            panic!("execution did not start");
        };
        parent.cancel();
        assert!(cancellation.is_cancelled());
        drop(lease);
    }

    #[tokio::test]
    async fn watch_events_are_ordered_replayable_and_cursor_tamper_requires_resync() {
        let state = state();
        let (_root, request, opened) = watch_fixture(&state).await;
        let poll = watch_poll(&request, &opened);
        let first = state
            .finish_watch_poll(
                &poll,
                WorkspaceWatchBatch {
                    events: watch_events(3),
                    failure: None,
                },
            )
            .unwrap();
        assert_eq!(
            first
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        let replayed = state
            .finish_watch_poll(&poll, WorkspaceWatchBatch::default())
            .unwrap();
        assert_eq!(replayed.events, first.events);

        let mut tampered = poll;
        tampered.cursor = Cursor::new("watch_cursor_tampered").unwrap();
        let resync = state
            .finish_watch_poll(&tampered, WorkspaceWatchBatch::default())
            .unwrap();
        assert_eq!(resync.state, WatchState::FullResync);
        assert_eq!(resync.resync_reason, Some(WatchResyncReason::CursorInvalid));
        assert!(resync.next_cursor.is_none());
    }

    #[tokio::test]
    async fn watch_overflow_retention_loss_expiry_and_instance_change_are_explicit_resyncs() {
        let state = state();
        let (_overflow_root, overflow_request, overflow_opened) = watch_fixture(&state).await;
        let overflow_expiry = watch_expiry(&state, &overflow_opened.subscription_id);
        let overflow = state
            .finish_watch_poll(
                &watch_poll(&overflow_request, &overflow_opened),
                WorkspaceWatchBatch {
                    events: Vec::new(),
                    failure: Some(WorkspaceWatchFailure::Overflow),
                },
            )
            .unwrap();
        assert_eq!(overflow.resync_reason, Some(WatchResyncReason::Overflow));

        let (_backend_root, backend_request, backend_opened) = watch_fixture(&state).await;
        let backend_expiry = watch_expiry(&state, &backend_opened.subscription_id);
        let backend = state
            .finish_watch_poll(
                &watch_poll(&backend_request, &backend_opened),
                WorkspaceWatchBatch {
                    events: Vec::new(),
                    failure: Some(WorkspaceWatchFailure::Backend),
                },
            )
            .unwrap();
        assert_eq!(backend.resync_reason, Some(WatchResyncReason::BackendError));

        let (_retention_root, retention_request, retention_opened) = watch_fixture(&state).await;
        let retention_expiry = watch_expiry(&state, &retention_opened.subscription_id);
        let retention = state
            .finish_watch_poll(
                &watch_poll(&retention_request, &retention_opened),
                WorkspaceWatchBatch {
                    events: watch_events(MAX_WATCH_RETAINED_EVENTS + 1),
                    failure: None,
                },
            )
            .unwrap();
        assert_eq!(
            retention.resync_reason,
            Some(WatchResyncReason::RetentionLost)
        );

        let (_expired_root, expired_request, expired_opened) = watch_fixture(&state).await;
        let expired_expiry = watch_expiry(&state, &expired_opened.subscription_id);
        state
            .watches
            .lock()
            .unwrap()
            .subscriptions
            .get_mut(&expired_opened.subscription_id)
            .unwrap()
            .expires_at = Instant::now();
        let expired = state
            .finish_watch_poll(
                &watch_poll(&expired_request, &expired_opened),
                WorkspaceWatchBatch::default(),
            )
            .unwrap();
        assert_eq!(
            expired.resync_reason,
            Some(WatchResyncReason::SubscriptionExpired)
        );
        assert!(
            !state
                .watches
                .lock()
                .unwrap()
                .subscriptions
                .contains_key(&expired_opened.subscription_id)
        );
        tokio::task::yield_now().await;
        assert!(overflow_expiry.is_finished());
        assert!(backend_expiry.is_finished());
        assert!(retention_expiry.is_finished());
        assert!(expired_expiry.is_finished());

        let (_instance_root, instance_request, instance_opened) = watch_fixture(&state).await;
        let mut changed_instance = watch_poll(&instance_request, &instance_opened);
        changed_instance.binding.host.instance_id =
            Identifier::new("replacement-instance").unwrap();
        let changed = state.poll_watch(&changed_instance).await.unwrap();
        assert_eq!(
            changed.resync_reason,
            Some(WatchResyncReason::InstanceChanged)
        );
        changed_instance.binding.host.workspace_generation =
            Identifier::new("other-generation").unwrap();
        assert_eq!(
            state.poll_watch(&changed_instance).await.unwrap_err(),
            RemoteOperationError::BindingMismatch
        );
    }

    #[tokio::test]
    async fn watch_subscriptions_enforce_cwd_quota_close_and_cleanup() {
        let state = state();
        let root = tempfile::tempdir().unwrap();
        let files = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let directory = files.workspace_root().await.unwrap();
        let request = WatchOpenRequest {
            version: ContractVersion::V1,
            binding: WorkspaceRequestBinding {
                host: state.binding.clone(),
                cwd_handle: directory.handle,
            },
            path: WorkspacePath::new(".").unwrap(),
            recursive: true,
        };
        let mut opened = Vec::new();
        let mut expiry_tasks = Vec::new();
        for _ in 0..MAX_WATCH_SUBSCRIPTIONS {
            let watcher = files.workspace_open_watch(&request).await.unwrap();
            let subscription = state.open_watch(&request, watcher).unwrap();
            expiry_tasks.push(
                state
                    .watches
                    .lock()
                    .unwrap()
                    .subscriptions
                    .get(&subscription.subscription_id)
                    .unwrap()
                    .expiry_task
                    .clone(),
            );
            opened.push(subscription);
        }
        let excess = files.workspace_open_watch(&request).await.unwrap();
        assert_eq!(
            state.open_watch(&request, excess).unwrap_err(),
            RemoteOperationError::QuotaExceeded
        );

        let mut wrong_cwd = watch_poll(&request, &opened[0]);
        wrong_cwd.binding.cwd_handle = ResourceId::new("different-cwd").unwrap();
        assert_eq!(
            state.poll_watch(&wrong_cwd).await.unwrap_err(),
            RemoteOperationError::BindingMismatch
        );
        for subscription in opened {
            let closed = state
                .close_watch(&WatchCloseRequest {
                    version: ContractVersion::V1,
                    binding: request.binding.clone(),
                    subscription_id: subscription.subscription_id,
                })
                .unwrap();
            assert!(closed.closed);
        }
        assert!(state.watches.lock().unwrap().subscriptions.is_empty());
        tokio::task::yield_now().await;
        assert!(expiry_tasks.iter().all(AbortHandle::is_finished));

        let churn = MAX_WATCH_SUBSCRIPTIONS * 4;
        for _ in 0..churn {
            let watcher = files.workspace_open_watch(&request).await.unwrap();
            let subscription = state.open_watch(&request, watcher).unwrap();
            let expiry_task = state
                .watches
                .lock()
                .unwrap()
                .subscriptions
                .get(&subscription.subscription_id)
                .unwrap()
                .expiry_task
                .clone();
            assert_eq!(state.watches.lock().unwrap().subscriptions.len(), 1);
            state
                .close_watch(&WatchCloseRequest {
                    version: ContractVersion::V1,
                    binding: request.binding.clone(),
                    subscription_id: subscription.subscription_id,
                })
                .unwrap();
            tokio::task::yield_now().await;
            assert!(expiry_task.is_finished());
        }
        assert!(state.watches.lock().unwrap().subscriptions.is_empty());

        let watcher = files.workspace_open_watch(&request).await.unwrap();
        let subscription = state.open_watch(&request, watcher).unwrap();
        let expiry_task = state
            .watches
            .lock()
            .unwrap()
            .subscriptions
            .get(&subscription.subscription_id)
            .unwrap()
            .expiry_task
            .clone();
        drop(state);
        tokio::task::yield_now().await;
        assert!(expiry_task.is_finished());
    }

    fn watch_expiry(state: &RemoteHostState, subscription_id: &Identifier) -> AbortHandle {
        state
            .watches
            .lock()
            .unwrap()
            .subscriptions
            .get(subscription_id)
            .unwrap()
            .expiry_task
            .clone()
    }

    fn state() -> RemoteHostState {
        let binding = HostBinding {
            server_id: Identifier::new("server").unwrap(),
            instance_id: Identifier::new("instance").unwrap(),
            workspace_id: Identifier::new("workspace").unwrap(),
            workspace_generation: Identifier::new("generation").unwrap(),
            root_project_id: Identifier::new("project").unwrap(),
            principal_id: Identifier::new("principal").unwrap(),
            cwd_handle: ResourceId::new("sha256:cwd").unwrap(),
            catalog_revision: Revision::new("sha256:catalog").unwrap(),
            policy_revision: Revision::new("sha256:policy").unwrap(),
        };
        let descriptor = RemoteHostDescriptor {
            version: ContractVersion::V1,
            server_id: binding.server_id.clone(),
            workspace_id: binding.workspace_id.clone(),
            workspace_generation: binding.workspace_generation.clone(),
            root_project_id: binding.root_project_id.clone(),
            principal_id: binding.principal_id.clone(),
            instance_id: binding.instance_id.clone(),
            resource_namespace_version: Identifier::new("v1").unwrap(),
            path_style: Identifier::new("root-relative-posix").unwrap(),
            revisions: workcell_host_contract::RemoteHostRevisions {
                execution_environment: Revision::new("sha256:environment").unwrap(),
                catalog: binding.catalog_revision.clone(),
                policy: binding.policy_revision.clone(),
            },
            cwd: workcell_host_contract::RemoteHostCwd {
                handle: binding.cwd_handle.clone(),
                display_path: DisplayText::new(".").unwrap(),
            },
            capabilities: workcell_host_contract::RemoteHostCapabilities {
                tool_catalog: workcell_host_contract::RemoteHostToolCapability {
                    version: ContractVersion::V1,
                    limits: workcell_host_contract::RemoteHostToolLimits {
                        max_request_bytes: 1,
                    },
                },
                tool_execution: workcell_host_contract::RemoteHostToolCapability {
                    version: ContractVersion::V1,
                    limits: workcell_host_contract::RemoteHostToolLimits {
                        max_request_bytes: 1,
                    },
                },
                execution_environment: None,
                reviewed_transfer: None,
                operations: None,
                workspace: None,
                watch: None,
                project_assets: None,
                workspace_mutation: None,
                direct_exec: None,
                scm: None,
                snapshots: None,
                control_plane: false,
                control_plane_missing: Vec::new(),
            },
        };
        RemoteHostState::new(descriptor, binding)
    }

    async fn watch_fixture(
        state: &RemoteHostState,
    ) -> (TempDir, WatchOpenRequest, WatchOpenResponse) {
        let root = tempfile::tempdir().unwrap();
        let files = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let directory = files.workspace_root().await.unwrap();
        let request = WatchOpenRequest {
            version: ContractVersion::V1,
            binding: WorkspaceRequestBinding {
                host: state.binding.clone(),
                cwd_handle: directory.handle,
            },
            path: WorkspacePath::new(".").unwrap(),
            recursive: true,
        };
        let watcher = files.workspace_open_watch(&request).await.unwrap();
        let opened = state.open_watch(&request, watcher).unwrap();
        (root, request, opened)
    }

    fn watch_poll(request: &WatchOpenRequest, opened: &WatchOpenResponse) -> WatchPollRequest {
        WatchPollRequest {
            version: ContractVersion::V1,
            binding: request.binding.clone(),
            subscription_id: opened.subscription_id.clone(),
            cursor: opened.cursor.clone(),
            max_events: workcell_host_contract::MAX_WATCH_POLL_EVENTS,
            max_bytes: workcell_host_contract::MAX_WATCH_POLL_BYTES,
            wait_ms: 0,
        }
    }

    fn watch_events(count: usize) -> Vec<WorkspaceWatchEvent> {
        (0..count)
            .map(|index| WorkspaceWatchEvent {
                kind: WatchEventKind::Modify,
                path: WorkspacePath::new(format!("event-{index}.txt")).unwrap(),
            })
            .collect()
    }

    fn operation_binding(state: &RemoteHostState) -> OperationBinding {
        OperationBinding {
            host: state.binding.clone(),
            contract: ContractBinding {
                id: Identifier::new("tool.v1").unwrap(),
                version: Identifier::new("v1").unwrap(),
                result_version: Identifier::new("v1").unwrap(),
            },
            argument_digest: Revision::new("sha256:arguments").unwrap(),
        }
    }

    fn prepare(state: &RemoteHostState) -> Result<PrepareResponse, RemoteOperationError> {
        state.prepare(
            PreparedRemoteOperation::Test(Vec::new()),
            2,
            operation_binding(state),
            OperationIntent {
                kind: OperationKind::Read,
                mutating: false,
                resources: Vec::new(),
            },
        )
    }

    fn successful_outcome(bytes: usize) -> StructuredOutcome {
        StructuredOutcome {
            kind: OutcomeKind::Completed,
            side_effects_possible: false,
            result: Some(
                ToolResultEnvelope::new(
                    Vec::new(),
                    Some(serde_json::json!({"value": "x".repeat(bytes)})),
                    false,
                )
                .unwrap(),
            ),
            error: None,
        }
    }
}
