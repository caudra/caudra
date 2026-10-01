//! The change records of a local session directory: Workcell's engine run in
//! process over the file tools' own filesystem core, so a revert waits on the
//! same mutation lock as every write.

pub(crate) mod wire;

use std::collections::VecDeque;
use std::fs;
use std::future::Future;
use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use caudra_storage::StateDir;
use caudra_storage::id::CaudraId;
use caudra_storage::paths::ensure_private_dir;
use caudra_storage::projects::workspace_key;
use caudra_storage::sessions::WORKSPACE_CHANGES_DIR;
use caudra_storage::sessions::change_stores::{ChangeStores, StoreUsage};
use caudra_workspace::{
    CancellationResult, ChangeOperationPreview, ChangeOperationResult, CleanupPreview,
    CleanupSummary, HolderPage, HolderSummary, OpenRecord, OperationError, OperationHandle,
    OperationId, OperationPhase, OperationState, OperationStatus, PreparedChangeOperation,
    RecordHolder, RecordPage, RecordRequest, RecordSummary, RecordTicket, ReleaseResult,
    ReleaseSelection, ReleaseSummary, RevertStatus, SequenceMetadata, TransportErrorKind,
    WorkspaceChangeService, WorkspaceError, WorkspacePath,
};
use futures_lite::FutureExt;
use futures_lite::future::{self, Boxed};
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use workcell::host_contract::MAX_RECORD_PAGE_SIZE;
use workcell::snapshots::{
    ChangeStore, PreparedCleanup, PreparedRevert, SnapshotError, SnapshotManager,
};

use crate::{HostError, HostInner};

/// Prepared operations kept awaiting execution; past it the oldest is dropped.
const MAX_PREPARED: usize = 64;
/// What the prepared operations of one handle may retain together.
const MAX_PREPARED_BYTES: usize = 32 * 1024 * 1024;
/// Finished operations whose outcome stays readable.
const MAX_SETTLED: usize = 128;
const STORE_WORKER_THREADS: usize = 1;
const STORE_THREAD_NAME: &str = "caudra-changes";
/// Refusal codes for an open that fails before the engine runs.
const UNKEYED_WORKSPACE: &str = "workspace_key_unavailable";
const NO_FILE_TOOLS: &str = "file_tools_unavailable";

/// What one store holds and who holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeInventory {
    /// Everything the store occupies on disk.
    pub bytes: u64,
    pub objects: u64,
    pub records: u32,
    pub open_records: u32,
    pub pending_reverts: u32,
    pub holders: Vec<HolderSummary>,
}

/// The change records of the session directory `cwd`, kept beneath `state`.
/// The store opens on the first call that needs it, and the next call tries
/// again after a failed open. Until it opens, a failure that may clear holds
/// the call, and any other lets it run unrecorded, as on a remote host
/// without change records.
pub(crate) fn bound(
    host: Arc<HostInner>,
    cwd: &Path,
    state: &StateDir,
) -> Arc<dyn WorkspaceChangeService> {
    let opener = {
        let (host, cwd, state) = (Arc::clone(&host), cwd.to_path_buf(), state.clone());
        move || open(Arc::clone(&host), cwd.clone(), state.clone()).boxed()
    };
    Arc::new(LocalChanges::new(host, cwd.to_path_buf(), Box::new(opener)))
}

/// Why an attempt to open the store failed.
enum OpenFailure {
    Engine(SnapshotError),
    /// The runtime dropped the attempt.
    Interrupted,
    /// The session directory has no workspace key.
    Unkeyed,
    /// The project's file tools would not start.
    NoFileTools,
}

async fn open(
    host: Arc<HostInner>,
    cwd: PathBuf,
    state: StateDir,
) -> Result<SnapshotManager, OpenFailure> {
    let key = workspace_key(&cwd).map_err(|_| OpenFailure::Unkeyed)?;
    let private_root = private_root(&state);
    let exclusions = exclusions(&cwd, &state);
    let groups = Arc::clone(&host);
    host.runtime
        .spawn(async move {
            let files = groups
                .project_groups(cwd)
                .await
                .map_err(|_| OpenFailure::NoFileTools)?
                .files;
            SnapshotManager::open_bound(
                files.workspace_snapshot_access(),
                private_root,
                &exclusions,
                &key,
            )
            .await
            .map_err(OpenFailure::Engine)
        })
        .await
        .map_err(|_| OpenFailure::Interrupted)?
}

/// A failure that may clear by itself blocks the call, and the next call
/// opens again. Any other is a refusal: calls run unrecorded, as they do on a
/// remote host whose store cannot open.
fn open_error(failure: OpenFailure) -> WorkspaceError {
    match failure {
        OpenFailure::Engine(
            error @ (SnapshotError::Busy | SnapshotError::TimedOut | SnapshotError::Cancelled),
        ) => change_error(error),
        OpenFailure::Interrupted => WorkspaceError::Unavailable,
        OpenFailure::Engine(error) => refusal(error.code()),
        OpenFailure::Unkeyed => refusal(UNKEYED_WORKSPACE),
        OpenFailure::NoFileTools => refusal(NO_FILE_TOOLS),
    }
}

fn refusal(symbolic: &str) -> WorkspaceError {
    WorkspaceError::Refused {
        code: wire::REFUSAL_RPC_CODE,
        symbolic: symbolic.to_owned(),
    }
}

/// `<state>/workspace-changes`, created owner-only and spelled without the
/// links Workcell refuses in a private root.
fn private_root(state: &StateDir) -> PathBuf {
    let root = state.path().join(WORKSPACE_CHANGES_DIR);
    if let Err(error) = ensure_private_dir(&root) {
        warn!(root = %root.display(), %error, "cannot create the change record store root");
    }
    fs::canonicalize(&root).unwrap_or(root)
}

/// Caudra's persistent state when it lies inside the workspace, since it
/// holds credentials no record may copy. The volatile root holds the private
/// root, and Workcell refuses to record a workspace around that at all.
fn exclusions(cwd: &Path, state: &StateDir) -> Vec<PathBuf> {
    match (
        fs::canonicalize(cwd),
        fs::canonicalize(state.persistent_path()),
    ) {
        (Ok(workspace), Ok(persistent)) if persistent.starts_with(&workspace) => vec![persistent],
        _ => Vec::new(),
    }
}

/// What a remote session reads for the same engine error once the host has
/// sent it and the client has mapped it, so a recorder treats both alike.
fn change_error(error: SnapshotError) -> WorkspaceError {
    match error {
        SnapshotError::InvalidConfiguration
        | SnapshotError::UnhealthyStorage
        | SnapshotError::InvalidRequest
        | SnapshotError::NotFound
        | SnapshotError::IntegrityFailure
        | SnapshotError::UnsupportedPlatform => refusal(error.code()),
        SnapshotError::UnsupportedFile => WorkspaceError::UnsupportedEntry,
        SnapshotError::LimitExceeded { limit, maximum } => WorkspaceError::LimitExceeded {
            limit: Some(limit.as_str().to_owned()),
            maximum,
        },
        SnapshotError::QuotaExceeded { limit, maximum } => WorkspaceError::QuotaExceeded {
            limit: Some(limit.as_str().to_owned()),
            maximum,
        },
        SnapshotError::Busy => WorkspaceError::Busy,
        SnapshotError::TimedOut => WorkspaceError::Transport {
            kind: TransportErrorKind::Timeout,
        },
        SnapshotError::Conflict => WorkspaceError::Conflict,
        SnapshotError::Cancelled => WorkspaceError::Cancelled,
        SnapshotError::OperationFailed => WorkspaceError::Unavailable,
    }
}

/// Runs engine work on Tokio from any executor, and cancels it when the
/// caller stops waiting.
async fn on_runtime<T, Fut>(
    runtime: &Handle,
    work: impl FnOnce(CancellationToken) -> Fut,
) -> Result<T, WorkspaceError>
where
    T: Send + 'static,
    Fut: Future<Output = Result<T, SnapshotError>> + Send + 'static,
{
    let token = CancellationToken::new();
    let _cancel_on_drop = token.clone().drop_guard();
    runtime
        .spawn(work(token))
        .await
        .map_err(|_| WorkspaceError::Unavailable)?
        .map_err(change_error)
}

async fn holder_page(
    runtime: &Handle,
    store: ChangeStore,
    after: Option<&RecordHolder>,
    page_size: u32,
) -> Result<HolderPage, WorkspaceError> {
    let after = after.map(wire::holder).transpose()?;
    let page_size = page_size.min(MAX_RECORD_PAGE_SIZE);
    let (holders, next_after) = on_runtime(runtime, move |_| async move {
        store.holders(after.as_ref(), page_size).await
    })
    .await?;
    wire::holder_page(holders, next_after)
}

async fn release_records(
    runtime: &Handle,
    store: ChangeStore,
    holder: &RecordHolder,
    selection: &ReleaseSelection,
) -> Result<ReleaseSummary, WorkspaceError> {
    let holder = wire::holder(holder)?;
    let selection = wire::release_selection(selection);
    let summary = on_runtime(runtime, move |_| async move {
        store.release(&holder, &selection).await
    })
    .await?;
    Ok(wire::release_summary(summary))
}

async fn plan_cleanup(
    runtime: &Handle,
    store: ChangeStore,
    retention_bytes: u64,
) -> Result<(PreparedCleanup, CleanupPreview), WorkspaceError> {
    let (prepared, preview) = on_runtime(runtime, move |_| async move {
        store
            .prepare_cleanup(retention_bytes, MAX_PREPARED_BYTES)
            .await
    })
    .await?;
    Ok((prepared, wire::cleanup_preview(preview)))
}

/// One attempt to open the store.
type Opener = Box<dyn Fn() -> Boxed<Result<SnapshotManager, OpenFailure>> + Send + Sync>;

struct LocalChanges {
    host: Arc<HostInner>,
    cwd: PathBuf,
    open: Opener,
    engine: OnceCell<SnapshotManager>,
    failure_logged: AtomicBool,
    registry: Arc<Mutex<Registry>>,
}

impl LocalChanges {
    fn new(host: Arc<HostInner>, cwd: PathBuf, open: Opener) -> Self {
        Self {
            host,
            cwd,
            open,
            engine: OnceCell::new(),
            failure_logged: AtomicBool::new(false),
            registry: Arc::default(),
        }
    }

    /// The engine, opened by the first call that needs it. Concurrent calls
    /// share one attempt, a failed attempt leaves it for the next call, and
    /// only the first failure is logged.
    async fn engine(&self) -> Result<&SnapshotManager, WorkspaceError> {
        self.engine
            .get_or_try_init(|| async {
                (self.open)().await.map_err(|failure| {
                    let error = open_error(failure);
                    if !self.failure_logged.swap(true, Ordering::Relaxed) {
                        warn!(cwd = %self.cwd.display(), %error, "local change records are unavailable");
                    }
                    error
                })
            })
            .await
    }

    fn runtime(&self) -> &Handle {
        self.host.runtime.handle()
    }

    async fn run<T, Fut>(
        &self,
        work: impl FnOnce(SnapshotManager, CancellationToken) -> Fut,
    ) -> Result<T, WorkspaceError>
    where
        T: Send + 'static,
        Fut: Future<Output = Result<T, SnapshotError>> + Send + 'static,
    {
        let engine = self.engine().await?.clone();
        on_runtime(self.runtime(), |token| work(engine, token)).await
    }

    fn register(
        &self,
        prepared: Prepared,
        preview: ChangeOperationPreview,
    ) -> Result<PreparedChangeOperation, WorkspaceError> {
        let operation = OperationHandle {
            preparation_id: OperationId::new(CaudraId::generate().to_string())
                .map_err(|_| WorkspaceError::Unavailable)?,
            invocation_id: None,
            execution_id: None,
            expires_at_unix_ms: None,
        };
        lock(&self.registry).prepare(operation.preparation_id.clone(), prepared);
        Ok(PreparedChangeOperation { operation, preview })
    }
}

#[async_trait]
impl WorkspaceChangeService for LocalChanges {
    async fn begin(&self, request: &RecordRequest) -> Result<RecordTicket, WorkspaceError> {
        let request = wire::record_request(
            request,
            &WorkspacePath::root(),
            &SnapshotManager::capability().limits,
        )?;
        let ticket = self
            .run(|engine, token| async move { engine.begin_record(request, &token).await })
            .await?;
        wire::record_ticket(&ticket)
    }

    async fn finish(&self, ticket: &RecordTicket) -> Result<Option<RecordSummary>, WorkspaceError> {
        let ticket = wire::ticket(ticket)?;
        let summary = self
            .run(|engine, token| async move { engine.finish_record(&ticket, &token).await })
            .await?;
        Ok(summary.map(wire::record_summary))
    }

    async fn abandon(&self, ticket: &RecordTicket) -> Result<bool, WorkspaceError> {
        let ticket = wire::ticket(ticket)?;
        self.run(|engine, _| async move { engine.store().abandon_record(&ticket).await })
            .await
    }

    async fn open_records(&self, holder: &RecordHolder) -> Result<Vec<OpenRecord>, WorkspaceError> {
        let holder = wire::holder(holder)?;
        self.run(|engine, _| async move { engine.store().open_records(&holder).await })
            .await?
            .into_iter()
            .map(wire::open_record)
            .collect()
    }

    async fn abandon_open_records(&self, holder: &RecordHolder) -> Result<u32, WorkspaceError> {
        let holder = wire::holder(holder)?;
        self.run(|engine, _| async move { engine.store().abandon_open_records(&holder).await })
            .await
    }

    async fn records(
        &self,
        holder: &RecordHolder,
        after_seq: Option<u64>,
        page_size: u32,
    ) -> Result<RecordPage, WorkspaceError> {
        let holder = wire::holder(holder)?;
        let page_size = page_size.min(MAX_RECORD_PAGE_SIZE);
        let page = self
            .run(move |engine, _| async move {
                engine.store().records(&holder, after_seq, page_size).await
            })
            .await?;
        Ok(wire::record_page(page))
    }

    async fn holders(
        &self,
        after: Option<&RecordHolder>,
        page_size: u32,
    ) -> Result<HolderPage, WorkspaceError> {
        holder_page(
            self.runtime(),
            self.engine().await?.store().clone(),
            after,
            page_size,
        )
        .await
    }

    async fn hold(&self, from: &RecordHolder, to: &RecordHolder) -> Result<u32, WorkspaceError> {
        let (from, to) = (wire::holder(from)?, wire::holder(to)?);
        self.run(|engine, _| async move { engine.store().hold(&from, &to).await })
            .await
    }

    async fn release(
        &self,
        holder: &RecordHolder,
        selection: &ReleaseSelection,
    ) -> Result<ReleaseSummary, WorkspaceError> {
        release_records(
            self.runtime(),
            self.engine().await?.store().clone(),
            holder,
            selection,
        )
        .await
    }

    async fn prepare_revert(
        &self,
        holder: &RecordHolder,
        seqs: &[u64],
    ) -> Result<PreparedChangeOperation, WorkspaceError> {
        let holder = wire::holder(holder)?;
        let seqs = seqs.to_vec();
        let (prepared, preview) = self
            .run(|engine, token| async move {
                engine
                    .prepare_revert(&holder, &seqs, MAX_PREPARED_BYTES, &token)
                    .await
            })
            .await?;
        self.register(
            Prepared::Revert(prepared),
            ChangeOperationPreview::Revert(wire::revert_preview(preview)?),
        )
    }

    async fn prepare_unrevert(
        &self,
        holder: &RecordHolder,
    ) -> Result<PreparedChangeOperation, WorkspaceError> {
        let holder = wire::holder(holder)?;
        let (prepared, preview) = self
            .run(|engine, token| async move {
                engine
                    .prepare_unrevert(&holder, MAX_PREPARED_BYTES, &token)
                    .await
            })
            .await?;
        self.register(
            Prepared::Revert(prepared),
            ChangeOperationPreview::Revert(wire::revert_preview(preview)?),
        )
    }

    async fn acknowledge(&self, holder: &RecordHolder) -> Result<RevertStatus, WorkspaceError> {
        let holder = wire::holder(holder)?;
        wire::revert_status(
            self.run(|engine, _| async move { engine.store().acknowledge(&holder).await })
                .await?,
        )
    }

    async fn status(&self, holder: &RecordHolder) -> Result<RevertStatus, WorkspaceError> {
        let holder = wire::holder(holder)?;
        wire::revert_status(
            self.run(|engine, _| async move { engine.store().status(&holder).await })
                .await?,
        )
    }

    async fn prepare_cleanup(
        &self,
        retention_bytes: u64,
    ) -> Result<PreparedChangeOperation, WorkspaceError> {
        let (prepared, preview) = plan_cleanup(
            self.runtime(),
            self.engine().await?.store().clone(),
            retention_bytes,
        )
        .await?;
        self.register(
            Prepared::Cleanup(prepared),
            ChangeOperationPreview::Cleanup(preview),
        )
    }

    /// Runs to the end even when the caller stops waiting: interrupting a
    /// revert is what `cancel` is for.
    async fn execute(
        &self,
        prepared: &PreparedChangeOperation,
    ) -> Result<OperationStatus<ChangeOperationResult>, WorkspaceError> {
        let engine = self.engine().await?.clone();
        let handle = prepared.operation.clone();
        let token = CancellationToken::new();
        let started = lock(&self.registry).start(&handle.preparation_id, &token);
        let operation = match started {
            Ok(operation) => operation,
            Err(state) => return Ok(status(handle, state)),
        };
        let registry = Arc::clone(&self.registry);
        let id = handle.preparation_id.clone();
        let state = self
            .runtime()
            .spawn(async move {
                let state = operation.execute(&engine, &token).await;
                lock(&registry).settle(&id, state.clone());
                state
            })
            .await
            .map_err(|_| WorkspaceError::Unavailable)?;
        Ok(status(handle, state))
    }

    async fn operation_status(
        &self,
        operation: &OperationHandle,
    ) -> Result<OperationStatus<ChangeOperationResult>, WorkspaceError> {
        self.engine().await?;
        let state = lock(&self.registry).state(&operation.preparation_id);
        Ok(status(operation.clone(), state))
    }

    async fn cancel(
        &self,
        operation: &OperationHandle,
    ) -> Result<CancellationResult, WorkspaceError> {
        self.engine().await?;
        Ok(lock(&self.registry).cancel(&operation.preparation_id))
    }

    async fn release_prepared(
        &self,
        prepared: &PreparedChangeOperation,
    ) -> Result<ReleaseResult, WorkspaceError> {
        self.engine().await?;
        lock(&self.registry).release(&prepared.operation.preparation_id)
    }
}

enum Prepared {
    Revert(PreparedRevert),
    Cleanup(PreparedCleanup),
}

impl Prepared {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Revert(revert) => revert.retained_bytes(),
            Self::Cleanup(cleanup) => cleanup.retained_bytes(),
        }
    }

    /// Settled as a remote host settles the same outcome: a revert that
    /// failed may have published part of its plan, a cleanup only when it
    /// was interrupted or failed midway.
    async fn execute(
        self,
        engine: &SnapshotManager,
        token: &CancellationToken,
    ) -> OperationState<ChangeOperationResult> {
        match self {
            Self::Revert(revert) => settled(
                engine
                    .execute_revert(&revert, token)
                    .await
                    .map(|status| wire::revert_status(status).map(ChangeOperationResult::Revert)),
                |_| true,
            ),
            Self::Cleanup(cleanup) => settled(
                engine
                    .store()
                    .execute_cleanup(&cleanup, token)
                    .await
                    .map(|summary| {
                        Ok(ChangeOperationResult::Cleanup(wire::cleanup_summary(
                            summary,
                        )))
                    }),
                cleanup_may_have_deleted,
            ),
        }
    }
}

fn cleanup_may_have_deleted(error: SnapshotError) -> bool {
    matches!(
        error,
        SnapshotError::Cancelled | SnapshotError::OperationFailed
    )
}

/// A failure that may have changed something leaves the outcome open, as
/// does a result that ran but cannot be read: the holder's status then says
/// where it stopped.
fn settled(
    outcome: Result<Result<ChangeOperationResult, WorkspaceError>, SnapshotError>,
    side_effects: impl Fn(SnapshotError) -> bool,
) -> OperationState<ChangeOperationResult> {
    let indeterminate = OperationState::Indeterminate {
        side_effects_possible: true,
    };
    match outcome {
        Ok(Ok(result)) => OperationState::Completed {
            result,
            side_effects_possible: false,
        },
        Ok(Err(_)) => indeterminate,
        Err(error) if side_effects(error) => indeterminate,
        Err(error) => {
            OperationId::new(error.code()).map_or(indeterminate, |code| OperationState::Failed {
                error: OperationError {
                    code,
                    message: error.to_string(),
                },
                side_effects_possible: false,
            })
        }
    }
}

fn status(
    handle: OperationHandle,
    state: OperationState<ChangeOperationResult>,
) -> OperationStatus<ChangeOperationResult> {
    OperationStatus {
        handle,
        state,
        progress: Vec::new(),
        progress_metadata: SequenceMetadata {
            first_retained_sequence: None,
            next_sequence: 0,
            gap_before_first: false,
        },
    }
}

fn phase<T>(state: &OperationState<T>) -> OperationPhase {
    match state {
        OperationState::NeverSeen => OperationPhase::NeverSeen,
        OperationState::Prepared => OperationPhase::Prepared,
        OperationState::Running => OperationPhase::Running,
        OperationState::Completed { .. } => OperationPhase::Completed,
        OperationState::Failed { .. } => OperationPhase::Failed,
        OperationState::Cancelled { .. } => OperationPhase::Cancelled,
        OperationState::Forgotten => OperationPhase::Forgotten,
        OperationState::Indeterminate { .. } => OperationPhase::Indeterminate,
    }
}

fn lock(registry: &Mutex<Registry>) -> MutexGuard<'_, Registry> {
    registry.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The operations prepared through one handle, oldest first.
#[derive(Default)]
struct Registry(VecDeque<(OperationId, Operation)>);

enum Operation {
    Prepared(Prepared),
    Running(CancellationToken),
    Settled(OperationState<ChangeOperationResult>),
}

impl Operation {
    fn state(&self) -> OperationState<ChangeOperationResult> {
        match self {
            Self::Prepared(_) => OperationState::Prepared,
            Self::Running(_) => OperationState::Running,
            Self::Settled(state) => state.clone(),
        }
    }
}

impl Registry {
    fn prepare(&mut self, id: OperationId, prepared: Prepared) {
        let retained = prepared.retained_bytes();
        while self.prepared().count() >= MAX_PREPARED
            || self.prepared().map(Prepared::retained_bytes).sum::<usize>() + retained
                > MAX_PREPARED_BYTES
        {
            if !self.evict_oldest(|operation| matches!(operation, Operation::Prepared(_))) {
                break;
            }
        }
        self.0.push_back((id, Operation::Prepared(prepared)));
    }

    /// Takes a prepared operation to run, or says where it is instead.
    fn start(
        &mut self,
        id: &OperationId,
        token: &CancellationToken,
    ) -> Result<Prepared, OperationState<ChangeOperationResult>> {
        let Some(operation) = self.get_mut(id) else {
            return Err(OperationState::NeverSeen);
        };
        match std::mem::replace(operation, Operation::Running(token.clone())) {
            Operation::Prepared(prepared) => Ok(prepared),
            other => {
                let state = other.state();
                *operation = other;
                Err(state)
            }
        }
    }

    fn settle(&mut self, id: &OperationId, state: OperationState<ChangeOperationResult>) {
        self.0.retain(|(operation_id, _)| operation_id != id);
        self.0.push_back((id.clone(), Operation::Settled(state)));
        let settled = |operation: &Operation| matches!(operation, Operation::Settled(_));
        if self
            .0
            .iter()
            .filter(|(_, operation)| settled(operation))
            .count()
            > MAX_SETTLED
        {
            self.evict_oldest(settled);
        }
    }

    fn state(&self, id: &OperationId) -> OperationState<ChangeOperationResult> {
        self.get(id)
            .map_or(OperationState::NeverSeen, Operation::state)
    }

    fn cancel(&self, id: &OperationId) -> CancellationResult {
        let cancellation_requested = match self.get(id) {
            Some(Operation::Running(token)) => {
                token.cancel();
                true
            }
            _ => false,
        };
        CancellationResult {
            state: phase(&self.state(id)),
            cancellation_requested,
        }
    }

    fn release(&mut self, id: &OperationId) -> Result<ReleaseResult, WorkspaceError> {
        let state = phase(&self.state(id));
        let released = match self.get(id) {
            Some(Operation::Running(_)) => {
                return Err(WorkspaceError::Conflict);
            }
            Some(Operation::Prepared(_)) => {
                self.0.retain(|(operation_id, _)| operation_id != id);
                true
            }
            _ => false,
        };
        Ok(ReleaseResult { state, released })
    }

    fn prepared(&self) -> impl Iterator<Item = &Prepared> {
        self.0.iter().filter_map(|(_, operation)| match operation {
            Operation::Prepared(prepared) => Some(prepared),
            _ => None,
        })
    }

    fn evict_oldest(&mut self, kind: impl Fn(&Operation) -> bool) -> bool {
        self.0
            .iter()
            .position(|(_, operation)| kind(operation))
            .and_then(|index| self.0.remove(index))
            .is_some()
    }

    fn get(&self, id: &OperationId) -> Option<&Operation> {
        self.0
            .iter()
            .find(|(operation_id, _)| operation_id == id)
            .map(|(_, operation)| operation)
    }

    fn get_mut(&mut self, id: &OperationId) -> Option<&mut Operation> {
        self.0
            .iter_mut()
            .find(|(operation_id, _)| operation_id == id)
            .map(|(_, operation)| operation)
    }
}

/// The change stores beneath any state directory, opened without the
/// workspace each records: enough to list, release and clean up after it is
/// gone. Every store runs on its runtime, so a process builds one.
pub struct LocalChangeStores {
    runtime: Arc<Runtime>,
}

impl LocalChangeStores {
    pub fn new() -> Result<Self, HostError> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(STORE_WORKER_THREADS)
            .enable_all()
            .thread_name(STORE_THREAD_NAME)
            .build()
            .map_err(HostError::Runtime)?;
        Ok(Self {
            runtime: Arc::new(runtime),
        })
    }

    pub async fn open(
        &self,
        state: &StateDir,
        key: &str,
    ) -> Result<LocalChangeStore, WorkspaceError> {
        let private_root = state.path().join(WORKSPACE_CHANGES_DIR);
        let private_root = fs::canonicalize(&private_root).unwrap_or(private_root);
        let key = key.to_owned();
        let store = on_runtime(self.runtime.handle(), move |_| async move {
            ChangeStore::open(private_root, &key).await
        })
        .await?;
        Ok(LocalChangeStore {
            runtime: Arc::clone(&self.runtime),
            store,
        })
    }
}

/// Session storage reaches the stores through this to release a deleted or
/// trimmed session's records, sweep, and report.
impl ChangeStores for LocalChangeStores {
    /// The workspace key of every store present, in order.
    fn keys(&self, state: &StateDir) -> io::Result<Vec<String>> {
        let entries = match fs::read_dir(state.path().join(WORKSPACE_CHANGES_DIR)) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut keys = Vec::new();
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Ok(key) = entry.file_name().into_string()
            {
                keys.push(key);
            }
        }
        keys.sort_unstable();
        Ok(keys)
    }

    fn usage(&self, state: &StateDir, key: &str) -> io::Result<StoreUsage> {
        blocking(async {
            let inventory = self.open(state, key).await?.inventory().await?;
            Ok(StoreUsage {
                bytes: inventory.bytes,
                objects: inventory.objects,
                records: inventory.records,
                open_records: inventory.open_records,
                pending_reverts: inventory.pending_reverts,
            })
        })
    }

    fn holders(&self, state: &StateDir, key: &str) -> io::Result<Vec<HolderSummary>> {
        blocking(async {
            let store = self.open(state, key).await?;
            let mut holders = Vec::new();
            let mut after = None;
            loop {
                let page = store.holders(after.as_ref(), u32::MAX).await?;
                holders.extend(page.holders);
                after = page.next_after;
                if after.is_none() {
                    return Ok(holders);
                }
            }
        })
    }

    fn release(&self, state: &StateDir, key: &str, holder: &RecordHolder) -> io::Result<()> {
        blocking(async {
            let store = self.open(state, key).await?;
            store.release(holder, &ReleaseSelection::All).await?;
            Ok(())
        })
    }

    fn clean_up(
        &self,
        state: &StateDir,
        key: &str,
        retention: NonZeroU64,
        dry_run: bool,
    ) -> io::Result<u64> {
        blocking(async {
            let store = self.open(state, key).await?;
            let cleanup = store.prepare_cleanup(retention.get()).await?;
            let preview = &cleanup.preview;
            // Executing rewrites the store's state, which is how the sweep
            // tells a store in use from one left behind.
            let nothing_to_clean = preview.stale_open_records == 0
                && preview.evicted_records == 0
                && preview.reclaimable_bytes == 0;
            if dry_run || nothing_to_clean {
                return Ok(preview.reclaimable_bytes);
            }
            Ok(store.execute_cleanup(cleanup).await?.reclaimed_bytes)
        })
    }
}

/// Runs one store operation to completion on the calling thread, which never
/// is the store runtime's own.
fn blocking<T>(work: impl Future<Output = Result<T, WorkspaceError>>) -> io::Result<T> {
    future::block_on(work).map_err(io::Error::other)
}

/// One change store, opened without its workspace.
pub struct LocalChangeStore {
    runtime: Arc<Runtime>,
    store: ChangeStore,
}

/// A cleanup planned on one store, run by [`LocalChangeStore::execute_cleanup`].
pub struct PreparedStoreCleanup {
    prepared: PreparedCleanup,
    pub preview: CleanupPreview,
}

impl LocalChangeStore {
    pub async fn inventory(&self) -> Result<ChangeInventory, WorkspaceError> {
        let store = self.store.clone();
        let inventory = on_runtime(self.runtime.handle(), move |_| async move {
            store.inventory().await
        })
        .await?;
        Ok(ChangeInventory {
            bytes: inventory.bytes,
            objects: inventory.objects,
            records: inventory.records,
            open_records: inventory.open_records,
            pending_reverts: inventory.pending_reverts,
            holders: inventory
                .holders
                .into_iter()
                .map(wire::holder_summary)
                .collect::<Result<_, _>>()?,
        })
    }

    pub async fn holders(
        &self,
        after: Option<&RecordHolder>,
        page_size: u32,
    ) -> Result<HolderPage, WorkspaceError> {
        holder_page(self.runtime.handle(), self.store.clone(), after, page_size).await
    }

    pub async fn release(
        &self,
        holder: &RecordHolder,
        selection: &ReleaseSelection,
    ) -> Result<ReleaseSummary, WorkspaceError> {
        release_records(self.runtime.handle(), self.store.clone(), holder, selection).await
    }

    pub async fn prepare_cleanup(
        &self,
        retention_bytes: u64,
    ) -> Result<PreparedStoreCleanup, WorkspaceError> {
        let (prepared, preview) =
            plan_cleanup(self.runtime.handle(), self.store.clone(), retention_bytes).await?;
        Ok(PreparedStoreCleanup { prepared, preview })
    }

    pub async fn execute_cleanup(
        &self,
        cleanup: PreparedStoreCleanup,
    ) -> Result<CleanupSummary, WorkspaceError> {
        let store = self.store.clone();
        let summary = on_runtime(self.runtime.handle(), move |token| async move {
            store.execute_cleanup(&cleanup.prepared, &token).await
        })
        .await?;
        Ok(wire::cleanup_summary(summary))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::num::NonZeroU64;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, UNIX_EPOCH};

    use caudra_storage::StateDir;
    use caudra_storage::projects::workspace_key;
    use caudra_storage::sessions::WORKSPACE_CHANGES_DIR;
    use caudra_storage::sessions::change_stores::ChangeStores;
    use caudra_workspace::{
        CancellationResult, ChangeOperationPreview, ChangeOperationResult, OperationId,
        OperationPhase, OperationState, PreparedChangeOperation, RecordHolder, RecordLimits,
        RecordRequest, RecordScope, RecordState, RecordSummary, ReleaseResult, ReleaseSelection,
        ReleaseSummary, RevertConflictKind, RevertPreview, TransportErrorKind,
        WorkspaceChangeService, WorkspaceError, WorkspacePath,
    };
    use futures_lite::FutureExt;
    use futures_lite::future::{self, Boxed};
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;
    use tokio_util::sync::CancellationToken;
    use workcell::host_contract::SnapshotLimit;
    use workcell::snapshots::SnapshotError;

    use super::wire::REFUSAL_RPC_CODE;
    use super::{
        LocalChangeStores, LocalChanges, MAX_PREPARED, MAX_SETTLED, NO_FILE_TOOLS, OpenFailure,
        Operation, Registry, UNKEYED_WORKSPACE, change_error, cleanup_may_have_deleted, open,
        open_error, phase, settled,
    };
    use crate::WorkcellHost;

    const LIMITS: RecordLimits = RecordLimits {
        max_files: 1_000,
        max_file_bytes: 1024 * 1024,
        max_total_bytes: 16 * 1024 * 1024,
    };
    const HOLDER: &str = "session-holder";
    const FORK: &str = "fork-holder";
    const FILE: &str = "named.txt";
    /// Written beside the named file, outside the scope that names it.
    const UNNAMED: &str = "unnamed.txt";
    const CREATED: &str = "created.txt";
    const BEFORE: &str = "before the call\n";
    const AFTER: &str = "after the call\n";
    const LATER: &str = "changed after the call was recorded\n";
    const PATHS_CALL: &str = "paths-call";
    const WORKSPACE_CALL: &str = "workspace-call";
    const STATE_DIR: &str = "state";
    const SESSION_DIR: &str = "session";
    const CREDENTIALS: &str = "credentials.json";
    const OPERATION: &str = "operation";
    const PAGE_SIZE: u32 = 100;
    const OVERLAP_REFUSAL: &str = "invalid_configuration";
    const EXPECT_RECORDED: &str = "the call changed a recorded file";
    const EXPECT_REVERT: &str = "a revert previews what it publishes";
    const EXPECT_CLEANUP: &str = "a cleanup previews and completes as a cleanup";
    const RELEASED_ALL: &str = "releasing a holder drops every hold it has";
    const RELEASE_LEAVES_GARBAGE: &str = "released records leave objects for a cleanup";
    const CLEANED: &str = "a cleanup removes the objects no record names";
    const IDLE_CLEANUP_WRITES_NOTHING: &str =
        "a cleanup with nothing to do must leave the store's age alone";
    const STORE_STATE_FILE: &str = "state";
    const LONG_AGO: Duration = Duration::from_secs(1_000_000_000);
    const OPEN_OUTCOME: OperationState<ChangeOperationResult> = OperationState::Indeterminate {
        side_effects_possible: true,
    };

    /// A session directory, and Caudra's state apart from it.
    struct Session {
        workspace: TempDir,
        state: TempDir,
        changes: Arc<dyn WorkspaceChangeService>,
    }

    /// Two records of one holder: a write to a named file beside an unnamed
    /// one, then a file created anywhere in the directory.
    struct Records {
        named: RecordSummary,
        whole: RecordSummary,
    }

    impl Session {
        fn new() -> Self {
            let workspace = TempDir::new().unwrap();
            let state = TempDir::new().unwrap();
            let changes = bound(workspace.path(), &state_dir(state.path()));
            Self {
                workspace,
                state,
                changes,
            }
        }

        async fn record_two(&self) -> Records {
            self.write(FILE, BEFORE);
            Records {
                named: self.record(paths(FILE), PATHS_CALL, &[FILE, UNNAMED]).await,
                whole: self
                    .record(RecordScope::Workspace, WORKSPACE_CALL, &[CREATED])
                    .await,
            }
        }

        async fn record(&self, scope: RecordScope, call: &str, files: &[&str]) -> RecordSummary {
            let ticket = self.changes.begin(&request(scope, call)).await.unwrap();
            for file in files {
                self.write(file, AFTER);
            }
            self.changes
                .finish(&ticket)
                .await
                .unwrap()
                .expect(EXPECT_RECORDED)
        }

        fn write(&self, name: &str, content: &str) {
            fs::write(self.workspace.path().join(name), content).unwrap();
        }

        fn read(&self, name: &str) -> Option<String> {
            fs::read_to_string(self.workspace.path().join(name)).ok()
        }

        async fn listing(&self, name: &str) -> Vec<(u64, RecordState)> {
            self.changes
                .records(&holder(name), None, PAGE_SIZE)
                .await
                .unwrap()
                .records
                .into_iter()
                .map(|record| (record.seq, record.state))
                .collect()
        }

        async fn state_of(
            &self,
            prepared: &PreparedChangeOperation,
        ) -> OperationState<ChangeOperationResult> {
            self.changes
                .operation_status(&prepared.operation)
                .await
                .unwrap()
                .state
        }
    }

    fn bound(workspace: &Path, state: &StateDir) -> Arc<dyn WorkspaceChangeService> {
        WorkcellHost::new(workspace, None)
            .unwrap()
            .change_service(workspace, state)
    }

    /// A service over `workspace` whose every open first awaits `attempt`.
    /// The failure it returns stands in for the attempt's; without one the
    /// store opens for real.
    fn scripted(
        workspace: &Path,
        state: &Path,
        attempt: impl Fn() -> Boxed<Option<OpenFailure>> + Send + Sync + 'static,
    ) -> LocalChanges {
        let host = Arc::clone(&WorkcellHost::new(workspace, None).unwrap().inner);
        let (opener_host, cwd, state) =
            (Arc::clone(&host), workspace.to_path_buf(), state_dir(state));
        let opener = move || {
            let attempt = attempt();
            let opening = open(Arc::clone(&opener_host), cwd.clone(), state.clone());
            async move {
                match attempt.await {
                    Some(failure) => Err(failure),
                    None => opening.await,
                }
            }
            .boxed()
        };
        LocalChanges::new(host, workspace.to_path_buf(), Box::new(opener))
    }

    fn state_dir(path: &Path) -> StateDir {
        StateDir::from_path(path.to_path_buf())
    }

    fn holder(name: &str) -> RecordHolder {
        RecordHolder::new(name).unwrap()
    }

    fn paths(name: &str) -> RecordScope {
        RecordScope::Paths(BTreeSet::from([WorkspacePath::new(name).unwrap()]))
    }

    fn request(scope: RecordScope, call: &str) -> RecordRequest {
        RecordRequest {
            scope,
            holder: holder(HOLDER),
            client: json!({"call":call}),
            limits: LIMITS,
        }
    }

    fn refused(code: &str) -> WorkspaceError {
        WorkspaceError::Refused {
            code: REFUSAL_RPC_CODE,
            symbolic: code.to_owned(),
        }
    }

    fn revert_preview(prepared: &PreparedChangeOperation) -> &RevertPreview {
        match &prepared.preview {
            ChangeOperationPreview::Revert(preview) => preview,
            other => panic!("{EXPECT_REVERT}: {other:?}"),
        }
    }

    fn assert_completed(state: &OperationState<ChangeOperationResult>) {
        assert!(
            matches!(
                state,
                OperationState::Completed {
                    side_effects_possible: false,
                    ..
                }
            ),
            "{state:?}"
        );
    }

    #[test]
    fn records_list_each_call_in_order_with_its_client_metadata() {
        smol::block_on(async {
            let session = Session::new();
            let Records { named, whole } = session.record_two().await;
            assert_eq!((named.paths, whole.paths), (1, 1));
            assert!(named.seq < whole.seq);
            let page = session
                .changes
                .records(&holder(HOLDER), None, PAGE_SIZE)
                .await
                .unwrap();
            assert_eq!(
                page.records
                    .into_iter()
                    .map(|record| (record.seq, record.client))
                    .collect::<Vec<_>>(),
                [
                    (named.seq, json!({"call":PATHS_CALL})),
                    (whole.seq, json!({"call":WORKSPACE_CALL})),
                ]
            );
        });
    }

    #[test]
    fn an_unfinished_record_stays_open_until_abandoned() {
        smol::block_on(async {
            let session = Session::new();
            let holder = holder(HOLDER);
            let ticket = session
                .changes
                .begin(&request(RecordScope::Workspace, WORKSPACE_CALL))
                .await
                .unwrap();
            session
                .changes
                .begin(&request(paths(FILE), PATHS_CALL))
                .await
                .unwrap();
            let open = session.changes.open_records(&holder).await.unwrap();
            assert_eq!(open.len(), 2);
            assert!(
                open.iter().any(|record| record.ticket == ticket
                    && record.client == json!({"call":WORKSPACE_CALL})),
                "{open:?}"
            );
            assert!(session.changes.abandon(&ticket).await.unwrap());
            assert_eq!(
                session.changes.abandon_open_records(&holder).await.unwrap(),
                1
            );
            assert!(
                session
                    .changes
                    .open_records(&holder)
                    .await
                    .unwrap()
                    .is_empty()
            );
        });
    }

    #[test]
    fn a_revert_restores_the_recorded_files_and_an_unrevert_reapplies_them() {
        smol::block_on(async {
            let session = Session::new();
            let Records { named, whole } = session.record_two().await;
            let holder = holder(HOLDER);
            let revert = session
                .changes
                .prepare_revert(&holder, &[named.seq, whole.seq])
                .await
                .unwrap();
            assert_eq!(
                revert_preview(&revert)
                    .planned
                    .iter()
                    .map(|planned| planned.path.as_str())
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from([FILE, CREATED])
            );
            assert_completed(&session.changes.execute(&revert).await.unwrap().state);
            assert_eq!(session.read(FILE).as_deref(), Some(BEFORE));
            assert_eq!(session.read(CREATED), None);
            assert_eq!(session.read(UNNAMED).as_deref(), Some(AFTER));
            assert_eq!(
                session.changes.status(&holder).await.unwrap().pending.len(),
                1
            );

            let unrevert = session.changes.prepare_unrevert(&holder).await.unwrap();
            assert_completed(&session.changes.execute(&unrevert).await.unwrap().state);
            assert_eq!(session.read(FILE).as_deref(), Some(AFTER));
            assert_eq!(session.read(CREATED).as_deref(), Some(AFTER));
            assert!(
                session
                    .changes
                    .status(&holder)
                    .await
                    .unwrap()
                    .pending
                    .is_empty()
            );
        });
    }

    #[test]
    fn acknowledging_a_revert_forgets_the_records_it_took_back() {
        smol::block_on(async {
            let session = Session::new();
            let Records { named, whole } = session.record_two().await;
            let holder = holder(HOLDER);
            let revert = session
                .changes
                .prepare_revert(&holder, &[named.seq])
                .await
                .unwrap();
            assert_completed(&session.changes.execute(&revert).await.unwrap().state);
            assert_eq!(
                session.listing(HOLDER).await,
                [
                    (named.seq, RecordState::Reverted),
                    (whole.seq, RecordState::Applied)
                ]
            );
            assert!(
                session
                    .changes
                    .acknowledge(&holder)
                    .await
                    .unwrap()
                    .pending
                    .is_empty()
            );
            assert_eq!(
                session.listing(HOLDER).await,
                [(whole.seq, RecordState::Applied)]
            );
        });
    }

    /// The host settles a revert that failed as open, since a failure may
    /// come after part of it was published; the holder's status says none was.
    #[test]
    fn a_conflicting_revert_names_the_path_and_writes_nothing() {
        smol::block_on(async {
            let session = Session::new();
            let Records { named, .. } = session.record_two().await;
            let holder = holder(HOLDER);
            session.write(FILE, LATER);
            let revert = session
                .changes
                .prepare_revert(&holder, &[named.seq])
                .await
                .unwrap();
            assert_eq!(
                revert_preview(&revert)
                    .conflicts
                    .iter()
                    .map(|conflict| (conflict.path.as_str(), conflict.kind))
                    .collect::<Vec<_>>(),
                [(FILE, RevertConflictKind::ChangedSince)]
            );
            assert_eq!(
                session.changes.execute(&revert).await.unwrap().state,
                OPEN_OUTCOME
            );
            assert_eq!(session.read(FILE).as_deref(), Some(LATER));
            assert!(
                session
                    .changes
                    .status(&holder)
                    .await
                    .unwrap()
                    .pending
                    .is_empty()
            );
        });
    }

    #[test]
    fn records_held_twice_stay_until_both_holders_release_them() {
        smol::block_on(async {
            let session = Session::new();
            let Records { named, whole } = session.record_two().await;
            let (owner, fork) = (holder(HOLDER), holder(FORK));
            assert_eq!(session.changes.hold(&owner, &fork).await.unwrap(), 2);
            assert_eq!(
                session
                    .changes
                    .holders(None, PAGE_SIZE)
                    .await
                    .unwrap()
                    .holders
                    .into_iter()
                    .map(|summary| (summary.holder, summary.records))
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from([(owner.clone(), 2), (fork.clone(), 2)])
            );
            assert_eq!(
                session
                    .changes
                    .release(&owner, &ReleaseSelection::All)
                    .await
                    .unwrap(),
                ReleaseSummary {
                    released: 2,
                    deleted: 0
                }
            );
            assert!(session.listing(HOLDER).await.is_empty());
            assert_eq!(
                session
                    .changes
                    .release(&fork, &ReleaseSelection::Seqs(vec![named.seq]))
                    .await
                    .unwrap(),
                ReleaseSummary {
                    released: 1,
                    deleted: 1
                }
            );
            assert_eq!(
                session.listing(FORK).await,
                [(whole.seq, RecordState::Applied)]
            );
        });
    }

    #[test]
    fn a_cleanup_without_retention_evicts_every_record() {
        smol::block_on(async {
            let session = Session::new();
            session.record_two().await;
            let cleanup = session.changes.prepare_cleanup(0).await.unwrap();
            let ChangeOperationPreview::Cleanup(preview) = &cleanup.preview else {
                panic!("{EXPECT_CLEANUP}: {:?}", cleanup.preview)
            };
            assert_eq!(preview.evicted_records, 2);
            let state = session.changes.execute(&cleanup).await.unwrap().state;
            let OperationState::Completed {
                result: ChangeOperationResult::Cleanup(summary),
                side_effects_possible: false,
            } = state
            else {
                panic!("{EXPECT_CLEANUP}: {state:?}")
            };
            assert_eq!(summary.evicted_records, 2);
            let page = session
                .changes
                .records(&holder(HOLDER), None, PAGE_SIZE)
                .await
                .unwrap();
            assert!(page.records.is_empty());
            assert!(page.evicted_through.is_some());
        });
    }

    #[test]
    fn a_prepared_operation_runs_once_and_keeps_its_outcome() {
        smol::block_on(async {
            let session = Session::new();
            let cleanup = session.changes.prepare_cleanup(0).await.unwrap();
            assert_eq!(session.state_of(&cleanup).await, OperationState::Prepared);
            assert_eq!(
                session.changes.cancel(&cleanup.operation).await.unwrap(),
                CancellationResult {
                    state: OperationPhase::Prepared,
                    cancellation_requested: false,
                }
            );
            let first = session.changes.execute(&cleanup).await.unwrap();
            assert_completed(&first.state);
            assert_eq!(session.changes.execute(&cleanup).await.unwrap(), first);
            assert_eq!(session.state_of(&cleanup).await, first.state);
            assert_eq!(
                session.changes.release_prepared(&cleanup).await.unwrap(),
                ReleaseResult {
                    state: OperationPhase::Completed,
                    released: false,
                }
            );
        });
    }

    #[test]
    fn a_released_operation_never_runs() {
        smol::block_on(async {
            let session = Session::new();
            let cleanup = session.changes.prepare_cleanup(0).await.unwrap();
            assert_eq!(
                session.changes.release_prepared(&cleanup).await.unwrap(),
                ReleaseResult {
                    state: OperationPhase::Prepared,
                    released: true,
                }
            );
            assert_eq!(
                session.changes.execute(&cleanup).await.unwrap().state,
                OperationState::NeverSeen
            );
        });
    }

    #[test]
    fn the_oldest_prepared_operation_makes_room_for_a_new_one() {
        smol::block_on(async {
            let session = Session::new();
            let mut prepared = Vec::new();
            for _ in 0..=MAX_PREPARED {
                prepared.push(session.changes.prepare_cleanup(0).await.unwrap());
            }
            assert_eq!(
                session.state_of(&prepared[0]).await,
                OperationState::NeverSeen
            );
            assert_eq!(
                session.state_of(&prepared[1]).await,
                OperationState::Prepared
            );
        });
    }

    #[test]
    fn the_oldest_outcome_makes_room_for_a_new_one() {
        let mut registry = Registry::default();
        let ids = (0..=MAX_SETTLED)
            .map(|index| OperationId::new(format!("{OPERATION}-{index}")).unwrap())
            .collect::<Vec<_>>();
        for id in &ids {
            registry.settle(id, OPEN_OUTCOME);
        }
        assert_eq!(registry.state(&ids[0]), OperationState::NeverSeen);
        assert_eq!(registry.state(&ids[1]), OPEN_OUTCOME);
    }

    #[test]
    fn a_running_operation_can_be_cancelled_but_not_released() {
        let mut registry = Registry::default();
        let id = OperationId::new(OPERATION).unwrap();
        let token = CancellationToken::new();
        registry
            .0
            .push_back((id.clone(), Operation::Running(token.clone())));
        assert_eq!(
            registry.cancel(&id),
            CancellationResult {
                state: OperationPhase::Running,
                cancellation_requested: true,
            }
        );
        assert!(token.is_cancelled());
        assert_eq!(registry.release(&id), Err(WorkspaceError::Conflict));
    }

    #[test_case(SnapshotError::Cancelled, OperationPhase::Indeterminate ; "an_interrupted_cleanup")]
    #[test_case(SnapshotError::OperationFailed, OperationPhase::Indeterminate ; "a_cleanup_failing_midway")]
    #[test_case(SnapshotError::Busy, OperationPhase::Failed ; "a_refused_cleanup")]
    fn a_failed_cleanup_settles_as_a_remote_host_settles_it(
        error: SnapshotError,
        expected: OperationPhase,
    ) {
        assert_eq!(
            phase(&settled(Err(error), cleanup_may_have_deleted)),
            expected
        );
    }

    #[test]
    fn a_result_that_cannot_be_read_leaves_the_outcome_open() {
        assert_eq!(
            settled(Ok(Err(WorkspaceError::Unavailable)), |_| false),
            OPEN_OUTCOME
        );
    }

    #[test_case(SnapshotError::InvalidConfiguration, refused("invalid_configuration") ; "invalid_configuration")]
    #[test_case(SnapshotError::UnhealthyStorage, refused("unhealthy_storage") ; "unhealthy_storage")]
    #[test_case(SnapshotError::InvalidRequest, refused("invalid_request") ; "invalid_request")]
    #[test_case(SnapshotError::NotFound, refused("not_found") ; "not_found")]
    #[test_case(SnapshotError::IntegrityFailure, refused("integrity_failure") ; "integrity_failure")]
    #[test_case(SnapshotError::UnsupportedFile, WorkspaceError::UnsupportedEntry ; "unsupported_file")]
    #[test_case(SnapshotError::UnsupportedPlatform, refused("unsupported_platform") ; "unsupported_platform")]
    #[test_case(
        SnapshotError::LimitExceeded { limit: SnapshotLimit::Files, maximum: Some(1) },
        WorkspaceError::LimitExceeded { limit: Some("files".into()), maximum: Some(1) }
        ; "limit_exceeded"
    )]
    #[test_case(
        SnapshotError::QuotaExceeded { limit: SnapshotLimit::StorageBytes, maximum: None },
        WorkspaceError::QuotaExceeded { limit: Some("storageBytes".into()), maximum: None }
        ; "quota_exceeded"
    )]
    #[test_case(SnapshotError::Busy, WorkspaceError::Busy ; "busy")]
    #[test_case(
        SnapshotError::TimedOut,
        WorkspaceError::Transport { kind: TransportErrorKind::Timeout }
        ; "timed_out"
    )]
    #[test_case(SnapshotError::Conflict, WorkspaceError::Conflict ; "conflict")]
    #[test_case(SnapshotError::Cancelled, WorkspaceError::Cancelled ; "cancelled")]
    #[test_case(SnapshotError::OperationFailed, WorkspaceError::Unavailable ; "operation_failed")]
    fn an_engine_error_reads_as_a_remote_session_reads_it(
        error: SnapshotError,
        expected: WorkspaceError,
    ) {
        assert_eq!(change_error(error), expected);
    }

    #[test]
    fn a_workspace_around_the_private_root_is_refused() {
        smol::block_on(async {
            let workspace = TempDir::new().unwrap();
            let state = workspace.path().join(STATE_DIR);
            fs::create_dir(&state).unwrap();
            let changes = bound(workspace.path(), &state_dir(&state));
            assert_eq!(
                changes
                    .begin(&request(RecordScope::Workspace, WORKSPACE_CALL))
                    .await
                    .unwrap_err(),
                refused(OVERLAP_REFUSAL)
            );
            assert_eq!(
                changes.status(&holder(HOLDER)).await.unwrap_err(),
                refused(OVERLAP_REFUSAL)
            );
        });
    }

    #[test]
    fn caudra_state_inside_the_workspace_is_never_recorded() {
        smol::block_on(async {
            let workspace = TempDir::new().unwrap();
            let volatile = TempDir::new().unwrap();
            let persistent = workspace.path().join(STATE_DIR);
            fs::create_dir(&persistent).unwrap();
            let state = StateDir::split(volatile.path().to_path_buf(), persistent.clone());
            let changes = bound(workspace.path(), &state);
            let ticket = changes
                .begin(&request(RecordScope::Workspace, WORKSPACE_CALL))
                .await
                .unwrap();
            fs::write(persistent.join(CREDENTIALS), AFTER).unwrap();
            fs::write(workspace.path().join(CREATED), AFTER).unwrap();
            let summary = changes
                .finish(&ticket)
                .await
                .unwrap()
                .expect(EXPECT_RECORDED);
            assert_eq!((summary.paths, summary.unrecorded), (1, 0));
        });
    }

    #[test]
    fn the_store_alone_lists_releases_and_cleans_up() {
        smol::block_on(async {
            let session = Session::new();
            let Records { named, .. } = session.record_two().await;
            let stores = LocalChangeStores::new().unwrap();
            let state = state_dir(session.state.path());
            let key = workspace_key(session.workspace.path()).unwrap();
            assert_eq!(stores.keys(&state).unwrap(), [key.as_str()]);
            let store = stores.open(&state, &key).await.unwrap();
            let inventory = store.inventory().await.unwrap();
            assert_eq!(
                inventory
                    .holders
                    .iter()
                    .map(|summary| (summary.holder.as_str(), summary.records))
                    .collect::<Vec<_>>(),
                [(HOLDER, 2)]
            );
            assert_eq!(inventory.records, 2);
            assert_eq!(
                store.holders(None, PAGE_SIZE).await.unwrap().holders,
                inventory.holders
            );
            assert_eq!(
                store
                    .release(&holder(HOLDER), &ReleaseSelection::Seqs(vec![named.seq]))
                    .await
                    .unwrap(),
                ReleaseSummary {
                    released: 1,
                    deleted: 1
                }
            );
            let cleanup = store.prepare_cleanup(0).await.unwrap();
            assert_eq!(cleanup.preview.evicted_records, 1);
            assert_eq!(
                store
                    .execute_cleanup(cleanup)
                    .await
                    .unwrap()
                    .evicted_records,
                1
            );
            assert_eq!(store.inventory().await.unwrap().records, 0);
        });
    }

    /// A caller asks for every record without knowing the store's largest
    /// page, and gets that page.
    #[test]
    fn a_page_larger_than_the_store_serves_is_clamped() {
        smol::block_on(async {
            let session = Session::new();
            let Records { named, whole } = session.record_two().await;
            let records = session
                .changes
                .records(&holder(HOLDER), None, u32::MAX)
                .await
                .unwrap()
                .records;
            assert_eq!(
                records.iter().map(|record| record.seq).collect::<Vec<_>>(),
                [named.seq, whole.seq]
            );
            let holders = session.changes.holders(None, u32::MAX).await.unwrap();
            assert_eq!(
                holders
                    .holders
                    .iter()
                    .map(|summary| summary.holder.as_str())
                    .collect::<Vec<_>>(),
                [HOLDER]
            );
            let store = LocalChangeStores::new()
                .unwrap()
                .open(
                    &state_dir(session.state.path()),
                    &workspace_key(session.workspace.path()).unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                store.holders(None, u32::MAX).await.unwrap().holders,
                holders.holders
            );
        });
    }

    /// What session storage runs on a real store when a session goes, and
    /// when the sweep cleans up after it.
    #[test]
    fn a_released_holder_leaves_nothing_once_the_store_is_cleaned() {
        let session = Session::new();
        smol::block_on(session.record_two());
        let stores = LocalChangeStores::new().unwrap();
        let state = state_dir(session.state.path());
        let key = workspace_key(session.workspace.path()).unwrap();
        let holders = stores.holders(&state, &key).unwrap();
        assert_eq!(
            holders
                .iter()
                .map(|summary| (summary.holder.as_str(), summary.records))
                .collect::<Vec<_>>(),
            [(HOLDER, 2)]
        );

        stores.release(&state, &key, &holder(HOLDER)).unwrap();

        assert!(
            stores.holders(&state, &key).unwrap().is_empty(),
            "{RELEASED_ALL}"
        );
        let reclaimable = stores
            .clean_up(&state, &key, NonZeroU64::MIN, true)
            .unwrap();
        assert!(reclaimable > 0, "{RELEASE_LEAVES_GARBAGE}");
        assert_eq!(
            stores
                .clean_up(&state, &key, NonZeroU64::MIN, false)
                .unwrap(),
            reclaimable
        );
        let usage = stores.usage(&state, &key).unwrap();
        assert!(usage.keeps_nothing());
        assert_eq!(usage.objects, 0, "{CLEANED}");
        let store_state = session
            .state
            .path()
            .join(WORKSPACE_CHANGES_DIR)
            .join(&key)
            .join(STORE_STATE_FILE);
        let long_ago = UNIX_EPOCH + LONG_AGO;
        fs::File::open(&store_state)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        assert_eq!(
            stores
                .clean_up(&state, &key, NonZeroU64::MIN, false)
                .unwrap(),
            0
        );
        assert_eq!(
            fs::metadata(&store_state).unwrap().modified().unwrap(),
            long_ago,
            "{IDLE_CLEANUP_WRITES_NOTHING}"
        );
    }

    /// A limit raised past what the store accepts records within the store's
    /// own, rather than leaving the call unrecorded.
    #[test_case(RecordLimits { max_files: u32::MAX, ..LIMITS } ; "files")]
    #[test_case(RecordLimits { max_file_bytes: u64::MAX, ..LIMITS } ; "file_bytes")]
    #[test_case(RecordLimits { max_total_bytes: u64::MAX, ..LIMITS } ; "total_bytes")]
    fn a_limit_above_what_the_store_accepts_still_records(limits: RecordLimits) {
        smol::block_on(async {
            let session = Session::new();
            session.write(FILE, BEFORE);
            let ticket = session
                .changes
                .begin(&RecordRequest {
                    limits,
                    ..request(paths(FILE), PATHS_CALL)
                })
                .await
                .unwrap();
            session.write(FILE, AFTER);
            session
                .changes
                .finish(&ticket)
                .await
                .unwrap()
                .expect(EXPECT_RECORDED);
        });
    }

    #[test]
    fn a_store_busy_at_first_opens_for_a_later_call() {
        smol::block_on(async {
            let (workspace, state) = (TempDir::new().unwrap(), TempDir::new().unwrap());
            let busy = AtomicBool::new(true);
            let changes = scripted(workspace.path(), state.path(), move || {
                let failure = busy
                    .swap(false, Ordering::SeqCst)
                    .then_some(OpenFailure::Engine(SnapshotError::Busy));
                future::ready(failure).boxed()
            });
            let request = request(RecordScope::Workspace, WORKSPACE_CALL);
            assert_eq!(
                changes.begin(&request).await.unwrap_err(),
                WorkspaceError::Busy
            );
            changes.begin(&request).await.unwrap();
        });
    }

    #[test_case(OpenFailure::Engine(SnapshotError::Busy), WorkspaceError::Busy ; "busy")]
    #[test_case(
        OpenFailure::Engine(SnapshotError::TimedOut),
        WorkspaceError::Transport { kind: TransportErrorKind::Timeout }
        ; "timed_out"
    )]
    #[test_case(OpenFailure::Engine(SnapshotError::Cancelled), WorkspaceError::Cancelled ; "cancelled")]
    #[test_case(OpenFailure::Interrupted, WorkspaceError::Unavailable ; "interrupted")]
    #[test_case(OpenFailure::Engine(SnapshotError::OperationFailed), refused("operation_failed") ; "operation_failed")]
    #[test_case(OpenFailure::Engine(SnapshotError::Conflict), refused("conflict") ; "conflict")]
    #[test_case(OpenFailure::NoFileTools, refused(NO_FILE_TOOLS) ; "no_file_tools")]
    fn an_open_failure_holds_the_call_only_while_it_may_clear(
        failure: OpenFailure,
        expected: WorkspaceError,
    ) {
        assert_eq!(open_error(failure), expected);
    }

    #[test]
    fn a_missing_session_directory_lets_calls_run_unrecorded_until_it_exists() {
        smol::block_on(async {
            let (base, state) = (TempDir::new().unwrap(), TempDir::new().unwrap());
            let cwd = base.path().join(SESSION_DIR);
            let changes = WorkcellHost::new(base.path(), None)
                .unwrap()
                .change_service(&cwd, &state_dir(state.path()));
            let request = request(RecordScope::Workspace, WORKSPACE_CALL);
            assert_eq!(
                changes.begin(&request).await.unwrap_err(),
                refused(UNKEYED_WORKSPACE)
            );
            fs::create_dir(&cwd).unwrap();
            changes.begin(&request).await.unwrap();
        });
    }

    #[test]
    fn concurrent_first_calls_share_one_open() {
        smol::block_on(async {
            let (workspace, state) = (TempDir::new().unwrap(), TempDir::new().unwrap());
            let attempts = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&attempts);
            let (release, released) = smol::channel::bounded::<()>(1);
            let changes = scripted(workspace.path(), state.path(), move || {
                counted.fetch_add(1, Ordering::SeqCst);
                let released = released.clone();
                async move {
                    let _ = released.recv().await;
                    None
                }
                .boxed()
            });
            let holder = holder(HOLDER);
            let first_calls = future::zip(changes.status(&holder), changes.status(&holder));
            let ((first, second), ()) =
                future::zip(first_calls, async move { drop(release) }).await;
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
            first.unwrap();
            second.unwrap();
        });
    }
}
