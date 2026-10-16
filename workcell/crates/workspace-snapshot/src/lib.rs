#![forbid(unsafe_code)]

//! Workspace change records for Workcell hosts. A record captures one call's scope before and
//! after the call into a private git object store and keeps what changed; any of a holder's
//! records can then be reverted, journaled so a crash never leaves a revert unaccounted for.
//!
//! Several processes may share one store: every operation takes the store's lock for as long as
//! it runs, and never longer.

mod capture;
mod cleanup;
mod format;
mod holders;
mod recording;
mod revert;
mod snapshot;
mod store;

use std::{
    collections::BTreeSet,
    fmt::Write as _,
    future::{self, Future},
    mem::size_of,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;
use workcell_host_contract::{
    ChangeInventory, CleanupPreview, CleanupSummary, ContractVersion, HolderSummary, Identifier,
    MAX_OPEN_RECORDS, MAX_RECORD_CLIENT_BYTES, MAX_RECORD_HOLDER_BYTES, MAX_RECORD_PAGE_SIZE,
    MAX_RECORD_SCOPE_PATHS, MAX_REVERT_RECORDS, MAX_SNAPSHOT_CAPTURE_ENTRIES,
    MAX_SNAPSHOT_CAPTURE_PATH_BYTES, MAX_SNAPSHOT_FILE_BYTES, MAX_SNAPSHOT_FILES,
    MAX_SNAPSHOT_STORAGE_BYTES, MAX_SNAPSHOT_TOTAL_BYTES, OpenRecord, RecordHolder, RecordPage,
    RecordRequest, RecordSummary, ReleaseSelection, ReleaseSummary, RevertDirection, RevertPreview,
    RevertStatus, SnapshotLimit, WorkspaceChangesCapability, WorkspaceChangesLimits,
    WorkspaceChangesMethods, WorkspacePath,
};
use workcell_mcp_files::{SnapshotTreeError, SnapshotTreeLimit, WorkspaceSnapshotAccess};
use workcell_snapshot_store::within;

use crate::{
    cleanup::CleanupPlan,
    format::{StoredState, count},
    revert::RevertPlan,
    store::{DIGEST_PREFIX, OPEN, RECORDS, REVERTS, Store, probe_umask},
};

const ADMISSION_TIMEOUT: Duration = Duration::from_secs(30);
const CAPTURE_EXECUTION_BUDGET: Duration = Duration::from_secs(15 * 60);
const MAX_EXCLUSIONS: usize = 32;
const CLEANUP_SCOPE_PREFIX: &str = "workspace-changes:cleanup:";
const DEFAULT_EXCLUSIONS: &[&str] = &[
    ".git",
    ".ssh",
    ".workcell",
    ".env",
    ".npmrc",
    ".pypirc",
    ".netrc",
];
/// Records one store keeps before retention evicts the oldest, however small they are.
pub(crate) const MAX_STORE_RECORDS: usize = 10_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot configuration is invalid")]
    InvalidConfiguration,
    #[error("snapshot storage is unavailable or unhealthy")]
    UnhealthyStorage,
    #[error("snapshot request is invalid")]
    InvalidRequest,
    #[error("snapshot record was not found")]
    NotFound,
    #[error("snapshot data failed integrity verification")]
    IntegrityFailure,
    #[error("a recorded scope or a reverted path is not a plain workspace entry")]
    UnsupportedFile,
    #[error("workspace snapshots need descriptor-relative traversal this host does not support")]
    UnsupportedPlatform,
    /// The workspace is past a ceiling of the record or the host.
    #[error("snapshot {limit} limit was exceeded")]
    LimitExceeded {
        limit: SnapshotLimit,
        maximum: Option<u64>,
    },
    /// The store is full with nothing it may evict.
    #[error("snapshot {limit} quota was exceeded")]
    QuotaExceeded {
        limit: SnapshotLimit,
        maximum: Option<u64>,
    },
    #[error("snapshot storage is busy")]
    Busy,
    #[error("snapshot capture execution budget was exhausted")]
    TimedOut,
    #[error("the workspace or the store no longer matches what the snapshot operation expects")]
    Conflict,
    #[error("snapshot operation was cancelled")]
    Cancelled,
    #[error("snapshot operation failed")]
    OperationFailed,
}

impl SnapshotError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidConfiguration => "invalid_configuration",
            Self::UnhealthyStorage => "unhealthy_storage",
            Self::InvalidRequest => "invalid_request",
            Self::NotFound => "not_found",
            Self::IntegrityFailure => "integrity_failure",
            Self::UnsupportedFile => "unsupported_file",
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::LimitExceeded { .. } => "limit_exceeded",
            Self::QuotaExceeded { .. } => "quota_exceeded",
            Self::Busy => "busy",
            Self::TimedOut => "timed_out",
            Self::Conflict => "conflict",
            Self::Cancelled => "cancelled",
            Self::OperationFailed => "operation_failed",
        }
    }

    /// The limit or quota reached, with its maximum where it has one.
    #[must_use]
    pub const fn limit(self) -> Option<(SnapshotLimit, Option<u64>)> {
        match self {
            Self::LimitExceeded { limit, maximum } | Self::QuotaExceeded { limit, maximum } => {
                Some((limit, maximum))
            }
            _ => None,
        }
    }
}

/// A store of change records, opened without the workspace it records: enough to list, hold,
/// release, acknowledge and clean up, never to capture or revert.
#[derive(Clone)]
pub struct ChangeStore {
    shared: Arc<Shared>,
}

/// A store of change records bound to the workspace it records.
#[derive(Clone)]
pub struct SnapshotManager {
    store: ChangeStore,
}

pub struct PreparedRevert {
    store: ChangeStore,
    plan: Arc<RevertPlan>,
}

pub struct PreparedCleanup {
    store: ChangeStore,
    plan: Arc<CleanupPlan>,
    resource_scope: String,
}

struct Shared {
    inner: Inner,
    /// Orders this process's operations; the store lock orders them against other processes.
    gate: Arc<AsyncMutex<()>>,
}

pub(crate) struct Inner {
    store: Store,
    workspace: Option<Workspace>,
}

/// The workspace a manager records, as every capture and revert sees it.
pub(crate) struct Workspace {
    access: WorkspaceSnapshotAccess,
    /// Root-relative paths no record ever holds.
    exclusions: Vec<String>,
    umask: u32,
}

impl ChangeStore {
    /// Opens the existing store beneath `private_root` that belongs to one workspace binding.
    pub async fn open(
        private_root: impl AsRef<Path>,
        binding: &str,
    ) -> Result<Self, SnapshotError> {
        let private_root = private_root.as_ref().to_path_buf();
        let binding = binding.to_owned();
        let inner = tokio::task::spawn_blocking(move || {
            Inner::open(Store::open(&private_root, None, &binding, false)?, None)
        })
        .await
        .map_err(|_| SnapshotError::OperationFailed)??;
        Ok(Self::new(inner))
    }

    fn new(inner: Inner) -> Self {
        Self {
            shared: Arc::new(Shared {
                inner,
                gate: Arc::default(),
            }),
        }
    }

    /// What the store holds and who holds it.
    pub async fn inventory(&self) -> Result<ChangeInventory, SnapshotError> {
        self.run_store(|inner, _| inner.inventory()).await
    }

    /// One page of `holder`'s records in seq order, starting after `after_seq`.
    pub async fn records(
        &self,
        holder: &RecordHolder,
        after_seq: Option<u64>,
        page_size: u32,
    ) -> Result<RecordPage, SnapshotError> {
        let holder = holder.as_str().to_owned();
        self.run_store(move |inner, _| inner.records_of(&holder, after_seq, page_size))
            .await
    }

    /// One page of every holder, in holder order, with the `after` of the next page.
    pub async fn holders(
        &self,
        after: Option<&RecordHolder>,
        page_size: u32,
    ) -> Result<(Vec<HolderSummary>, Option<RecordHolder>), SnapshotError> {
        let after = after.map(|holder| holder.as_str().to_owned());
        self.run_store(move |inner, _| inner.holders_page(after.as_deref(), page_size))
            .await
    }

    /// Has `to` hold every record `from` holds, as a fork inherits its parent's history. Returns
    /// how many that is.
    pub async fn hold(&self, from: &RecordHolder, to: &RecordHolder) -> Result<u32, SnapshotError> {
        let (from, to) = (from.as_str().to_owned(), to.as_str().to_owned());
        self.run_store(move |inner, _| inner.hold(&from, &to)).await
    }

    /// Drops `holder`'s hold on the selected records. A record no holder holds is deleted.
    pub async fn release(
        &self,
        holder: &RecordHolder,
        selection: &ReleaseSelection,
    ) -> Result<ReleaseSummary, SnapshotError> {
        if matches!(selection, ReleaseSelection::Seqs(seqs) if seqs.len() > MAX_REVERT_RECORDS) {
            return Err(SnapshotError::InvalidRequest);
        }
        let holder = holder.as_str().to_owned();
        let selection = selection.clone();
        self.run_store(move |inner, _| inner.release(&holder, &selection))
            .await
    }

    pub async fn open_records(
        &self,
        holder: &RecordHolder,
    ) -> Result<Vec<OpenRecord>, SnapshotError> {
        let holder = holder.as_str().to_owned();
        self.run_store(move |inner, _| inner.open_records_of(&holder))
            .await
    }

    /// Deletes an open record, reporting whether there was one.
    pub async fn abandon_record(&self, ticket: &Identifier) -> Result<bool, SnapshotError> {
        let ticket = ticket.as_str().to_owned();
        self.run_store(move |inner, _| inner.abandon(&ticket)).await
    }

    /// Deletes every open record `holder` began, returning how many.
    pub async fn abandon_open_records(&self, holder: &RecordHolder) -> Result<u32, SnapshotError> {
        let holder = holder.as_str().to_owned();
        self.run_store(move |inner, _| inner.abandon_open_records_of(&holder))
            .await
    }

    /// Settles `holder`'s pending reverts. The records they reverted are deleted for every
    /// holder, since what those records changed is gone from the workspace.
    pub async fn acknowledge(&self, holder: &RecordHolder) -> Result<RevertStatus, SnapshotError> {
        let holder = holder.as_str().to_owned();
        self.run_store(move |inner, _| inner.acknowledge(&holder))
            .await
    }

    /// `holder`'s reverts awaiting acknowledgement.
    pub async fn status(&self, holder: &RecordHolder) -> Result<RevertStatus, SnapshotError> {
        let holder = holder.as_str().to_owned();
        self.run_store(move |inner, _| {
            inner.settle()?;
            inner.status(&holder)
        })
        .await
    }

    /// Plans abandoning open records older than any call may run and evicting the oldest records
    /// until the store is within `retention_bytes`, then collecting what nothing names.
    pub async fn prepare_cleanup(
        &self,
        retention_bytes: u64,
        maximum_retained_bytes: usize,
    ) -> Result<(PreparedCleanup, CleanupPreview), SnapshotError> {
        let plan = self
            .run_store(move |inner, _| inner.plan_cleanup(retention_bytes))
            .await?;
        let preview = plan.preview.clone();
        let prepared = PreparedCleanup {
            store: self.clone(),
            resource_scope: format!("{CLEANUP_SCOPE_PREFIX}{}", digest_serializable(&plan)?),
            plan: Arc::new(plan),
        };
        if prepared.retained_bytes() > maximum_retained_bytes {
            return Err(limit_error(
                SnapshotLimit::PreparedBytes,
                maximum_retained_bytes,
            ));
        }
        Ok((prepared, preview))
    }

    pub async fn execute_cleanup(
        &self,
        prepared: &PreparedCleanup,
        token: &CancellationToken,
    ) -> Result<CleanupSummary, SnapshotError> {
        if !Arc::ptr_eq(&self.shared, &prepared.store.shared) {
            return Err(SnapshotError::InvalidRequest);
        }
        let plan = Arc::clone(&prepared.plan);
        self.run(token, future::ready(Ok(())), move |inner, token| {
            inner.execute_cleanup(&plan, token)
        })
        .await
    }

    async fn run_store<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Inner, &CancellationToken) -> Result<T, SnapshotError> + Send + 'static,
    ) -> Result<T, SnapshotError> {
        self.run(&CancellationToken::new(), future::ready(Ok(())), work)
            .await
    }

    /// Runs `work` off the executor once this process's earlier operations, `guard` and the store
    /// lock admit it, all within one admission deadline. Everything it holds is released only
    /// when it finishes, even if the caller stops waiting, so nothing overlaps it.
    async fn run<G, T>(
        &self,
        token: &CancellationToken,
        guard: impl Future<Output = Result<G, SnapshotError>>,
        work: impl FnOnce(&Inner, &CancellationToken) -> Result<T, SnapshotError> + Send + 'static,
    ) -> Result<T, SnapshotError>
    where
        G: Send + 'static,
        T: Send + 'static,
    {
        let deadline = Instant::now() + self.shared.inner.store.admission(ADMISSION_TIMEOUT);
        let admission = async {
            let gate = Arc::clone(&self.shared.gate).lock_owned().await;
            Ok::<_, SnapshotError>((gate, guard.await?))
        };
        let guards = tokio::select! {
            biased;
            () = token.cancelled() => return Err(SnapshotError::Cancelled),
            admitted = tokio::time::timeout_at(deadline.into(), admission) => {
                admitted.map_err(|_| SnapshotError::Busy)??
            }
        };
        let shared = Arc::clone(&self.shared);
        let token = token.clone();
        tokio::task::spawn_blocking(move || {
            let _guards = guards;
            let _lock = shared.inner.store.lock(deadline, &token)?;
            work(&shared.inner, &token)
        })
        .await
        .map_err(|_| SnapshotError::OperationFailed)?
    }
}

impl SnapshotManager {
    /// Opens, creating it if need be, the store beneath `private_root` that belongs to one
    /// workspace binding. `private_root` must be an absolute private directory outside the
    /// workspace; `excluded_paths` are left out of every record besides the defaults.
    pub async fn open_bound(
        access: WorkspaceSnapshotAccess,
        private_root: impl AsRef<Path>,
        excluded_paths: &[PathBuf],
        binding: &str,
    ) -> Result<Self, SnapshotError> {
        if !access.allow_write() {
            return Err(SnapshotError::InvalidConfiguration);
        }
        let private_root = private_root.as_ref().to_path_buf();
        let excluded_paths = excluded_paths.to_vec();
        let binding = binding.to_owned();
        let inner = tokio::task::spawn_blocking(move || {
            let store = Store::open(&private_root, Some(access.root()), &binding, true)?;
            let exclusions = configured_exclusions(access.root(), &excluded_paths)?;
            Inner::open(store, Some((access, exclusions)))
        })
        .await
        .map_err(|_| SnapshotError::OperationFailed)??;
        Ok(Self {
            store: ChangeStore::new(inner),
        })
    }

    #[must_use]
    pub fn capability() -> WorkspaceChangesCapability {
        WorkspaceChangesCapability {
            version: ContractVersion::V1,
            methods: WorkspaceChangesMethods {
                begin_record: true,
                finish_record: true,
                abandon_record: true,
                open_records: true,
                abandon_open_records: true,
                records: true,
                holders: true,
                hold: true,
                release: true,
                prepare_revert: true,
                prepare_unrevert: true,
                acknowledge: true,
                status: true,
                prepare_cleanup: true,
            },
            limits: WorkspaceChangesLimits {
                max_files: count(MAX_SNAPSHOT_FILES),
                max_file_bytes: MAX_SNAPSHOT_FILE_BYTES,
                max_total_bytes: MAX_SNAPSHOT_TOTAL_BYTES,
                max_capture_entries: count(MAX_SNAPSHOT_CAPTURE_ENTRIES),
                max_capture_path_bytes: MAX_SNAPSHOT_CAPTURE_PATH_BYTES,
                max_storage_bytes: MAX_SNAPSHOT_STORAGE_BYTES,
                max_scope_paths: count(MAX_RECORD_SCOPE_PATHS),
                max_client_bytes: count(MAX_RECORD_CLIENT_BYTES),
                max_holder_bytes: count(MAX_RECORD_HOLDER_BYTES),
                max_page_size: MAX_RECORD_PAGE_SIZE,
                max_open_records: count(MAX_OPEN_RECORDS),
                max_revert_records: count(MAX_REVERT_RECORDS),
            },
        }
    }

    /// The store alone, for what needs no workspace.
    #[must_use]
    pub const fn store(&self) -> &ChangeStore {
        &self.store
    }

    /// Captures the request's scope before its call. The ticket finishes or abandons the record,
    /// and outlives this process.
    pub async fn begin_record(
        &self,
        request: RecordRequest,
        token: &CancellationToken,
    ) -> Result<Identifier, SnapshotError> {
        recording::validate(&request)?;
        let ticket = self
            .capture(token, move |inner, token| inner.begin(request, token))
            .await?;
        identifier(&ticket)
    }

    /// Captures the record's scope after its call and commits what the call changed, if
    /// anything did.
    pub async fn finish_record(
        &self,
        ticket: &Identifier,
        token: &CancellationToken,
    ) -> Result<Option<RecordSummary>, SnapshotError> {
        let ticket = ticket.as_str().to_owned();
        self.capture(token, move |inner, token| inner.finish(&ticket, token))
            .await
    }

    /// Plans taking `seqs` back out of the workspace. `holder` must hold each, and no pending
    /// revert may name any.
    pub async fn prepare_revert(
        &self,
        holder: &RecordHolder,
        seqs: &[u64],
        maximum_retained_bytes: usize,
        token: &CancellationToken,
    ) -> Result<(PreparedRevert, RevertPreview), SnapshotError> {
        if seqs.len() > MAX_REVERT_RECORDS {
            return Err(SnapshotError::InvalidRequest);
        }
        let access = self.access()?;
        let holder = holder.as_str().to_owned();
        let seqs = seqs.to_vec();
        let plan = self
            .store
            .run(
                token,
                async { Ok(access.capture_guard().await) },
                move |inner, token| inner.prepare_revert(&holder, &seqs, token),
            )
            .await?;
        self.prepared(plan, maximum_retained_bytes)
    }

    /// Plans re-applying every revert `holder` has pending.
    pub async fn prepare_unrevert(
        &self,
        holder: &RecordHolder,
        maximum_retained_bytes: usize,
        token: &CancellationToken,
    ) -> Result<(PreparedRevert, RevertPreview), SnapshotError> {
        let access = self.access()?;
        let holder = holder.as_str().to_owned();
        let plan = self
            .store
            .run(
                token,
                async { Ok(access.capture_guard().await) },
                move |inner, token| inner.prepare_unrevert(&holder, token),
            )
            .await?;
        self.prepared(plan, maximum_retained_bytes)
    }

    /// Publishes a prepared revert or unrevert whose records and stack are still as planned.
    pub async fn execute_revert(
        &self,
        prepared: &PreparedRevert,
        token: &CancellationToken,
    ) -> Result<RevertStatus, SnapshotError> {
        if !Arc::ptr_eq(&self.store.shared, &prepared.store.shared) {
            return Err(SnapshotError::InvalidRequest);
        }
        let access = self.access()?;
        let plan = Arc::clone(&prepared.plan);
        self.store
            .run(
                token,
                async {
                    access
                        .mutation_guard()
                        .await
                        .map_err(|_| SnapshotError::OperationFailed)
                },
                move |inner, token| inner.execute_revert(&plan, token),
            )
            .await
    }

    fn prepared(
        &self,
        plan: RevertPlan,
        maximum_retained_bytes: usize,
    ) -> Result<(PreparedRevert, RevertPreview), SnapshotError> {
        let preview = plan.preview.clone();
        let prepared = PreparedRevert {
            store: self.store.clone(),
            plan: Arc::new(plan),
        };
        if prepared.retained_bytes() > maximum_retained_bytes {
            return Err(limit_error(
                SnapshotLimit::PreparedBytes,
                maximum_retained_bytes,
            ));
        }
        Ok((prepared, preview))
    }

    /// Runs a capture within its execution budget, with workspace mutations held off while it
    /// reads.
    async fn capture<T: Send + 'static>(
        &self,
        token: &CancellationToken,
        work: impl FnOnce(&Inner, &CancellationToken) -> Result<T, SnapshotError> + Send + 'static,
    ) -> Result<T, SnapshotError> {
        let access = self.access()?;
        let cancellation = token.child_token();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let execution = self.store.run(
            &cancellation,
            async { Ok(access.capture_guard().await) },
            work,
        );
        tokio::pin!(execution);
        tokio::select! {
            biased;
            result = &mut execution => result,
            () = tokio::time::sleep(CAPTURE_EXECUTION_BUDGET) => {
                cancellation.cancel();
                match execution.await {
                    Err(SnapshotError::Cancelled) => Err(SnapshotError::TimedOut),
                    result => result,
                }
            }
        }
    }

    fn access(&self) -> Result<&WorkspaceSnapshotAccess, SnapshotError> {
        Ok(&self.store.shared.inner.workspace()?.access)
    }
}

impl PreparedRevert {
    /// Conservative retained bytes, excluding the store shared with the host.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>().saturating_add(self.plan.retained_bytes())
    }

    #[must_use]
    pub fn revert_id(&self) -> &str {
        &self.plan.revert_id
    }

    #[must_use]
    pub fn holder(&self) -> &str {
        &self.plan.holder
    }

    #[must_use]
    pub fn direction(&self) -> RevertDirection {
        self.plan.direction
    }

    #[must_use]
    pub fn seqs(&self) -> &[u64] {
        &self.plan.seqs
    }

    #[must_use]
    pub fn preview(&self) -> &RevertPreview {
        &self.plan.preview
    }

    /// Every path the revert writes, the directories it creates first.
    pub fn paths(&self) -> impl Iterator<Item = &WorkspacePath> {
        self.plan.paths()
    }
}

impl PreparedCleanup {
    /// Conservative retained bytes, excluding the store shared with the host.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.plan.retained_bytes())
            .saturating_add(self.resource_scope.capacity())
    }

    #[must_use]
    pub fn preview(&self) -> &CleanupPreview {
        &self.plan.preview
    }

    /// Names exactly what the cleanup would abandon and evict.
    #[must_use]
    pub fn resource_scope(&self) -> &str {
        &self.resource_scope
    }
}

impl Inner {
    /// Readies a store under its lock: removes what earlier formats and crashes left, creates the
    /// state of a new store, and reconciles any revert a crash interrupted.
    fn open(
        store: Store,
        workspace: Option<(WorkspaceSnapshotAccess, Vec<String>)>,
    ) -> Result<Self, SnapshotError> {
        let deadline = Instant::now() + store.admission(ADMISSION_TIMEOUT);
        let _lock = store.lock(deadline, &CancellationToken::new())?;
        let removed = store.remove_legacy()?;
        if removed > 0 {
            tracing::info!(
                removed,
                "removed workspace snapshot data an earlier store format left"
            );
        }
        store.remove_temporaries()?;
        let workspace = match workspace {
            Some((access, exclusions)) => Some(Workspace {
                umask: probe_umask(store.root())?,
                access,
                exclusions,
            }),
            None => None,
        };
        let inner = Self { store, workspace };
        inner.initialize()?;
        inner.settle()?;
        Ok(inner)
    }

    /// Writes the state of a store that has none. One that has records without a state has lost
    /// its seqs and is refused rather than reuse them.
    fn initialize(&self) -> Result<(), SnapshotError> {
        match self.store.state() {
            Err(SnapshotError::NotFound) => {}
            state => return state.map(drop),
        }
        for directory in [OPEN, RECORDS, REVERTS] {
            if !self.store.names(directory)?.is_empty() {
                return Err(SnapshotError::UnhealthyStorage);
            }
        }
        self.store.write_state(&StoredState::new())
    }

    fn workspace(&self) -> Result<&Workspace, SnapshotError> {
        self.workspace.as_ref().ok_or(SnapshotError::InvalidRequest)
    }
}

impl Workspace {
    /// Whether an exclusion covers `path`, which no record then holds.
    fn excluded(&self, path: &str) -> bool {
        self.exclusions
            .iter()
            .any(|exclusion| within(exclusion, path))
    }
}

fn configured_exclusions(
    workspace: &Path,
    paths: &[PathBuf],
) -> Result<Vec<String>, SnapshotError> {
    let mut exclusions = DEFAULT_EXCLUSIONS
        .iter()
        .map(|path| (*path).to_owned())
        .collect::<BTreeSet<_>>();
    for path in paths {
        exclusions.insert(configured_exclusion(workspace, path)?);
    }
    if exclusions.len() > MAX_EXCLUSIONS {
        return Err(SnapshotError::InvalidConfiguration);
    }
    Ok(exclusions.into_iter().collect())
}

/// The root-relative form of a configured exclusion, resolved through whatever part of it
/// exists so a later-created path stays excluded.
fn configured_exclusion(workspace: &Path, requested: &Path) -> Result<String, SnapshotError> {
    if requested.as_os_str().is_empty()
        || requested
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(SnapshotError::InvalidConfiguration);
    }
    let mut ancestor = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        workspace.join(requested)
    };
    let mut suffix = Vec::new();
    loop {
        match std::fs::symlink_metadata(&ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    ancestor
                        .file_name()
                        .ok_or(SnapshotError::InvalidConfiguration)?
                        .to_owned(),
                );
                if !ancestor.pop() {
                    return Err(SnapshotError::InvalidConfiguration);
                }
            }
            Err(_) => return Err(SnapshotError::InvalidConfiguration),
        }
    }
    let canonical = ancestor
        .canonicalize()
        .map_err(|_| SnapshotError::InvalidConfiguration)?;
    let mut relative = canonical
        .strip_prefix(workspace)
        .map_err(|_| SnapshotError::InvalidConfiguration)?
        .to_path_buf();
    for component in suffix.into_iter().rev() {
        relative.push(component);
    }
    let relative = relative
        .components()
        .map(|component| match component {
            Component::Normal(part) => part.to_str().ok_or(SnapshotError::InvalidConfiguration),
            _ => Err(SnapshotError::InvalidConfiguration),
        })
        .collect::<Result<Vec<_>, _>>()?
        .join("/");
    if relative.is_empty() || !snapshot::valid_path(&relative) {
        return Err(SnapshotError::InvalidConfiguration);
    }
    Ok(relative)
}

fn digest_serializable(value: &impl Serialize) -> Result<String, SnapshotError> {
    let bytes = serde_json::to_vec(value).map_err(|_| SnapshotError::OperationFailed)?;
    Ok(format!("{DIGEST_PREFIX}{}", hex_sha256(&bytes)))
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut output = String::new();
    for byte in Sha256::digest(bytes) {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn identifier(value: &str) -> Result<Identifier, SnapshotError> {
    Identifier::new(value.to_owned()).map_err(|_| SnapshotError::IntegrityFailure)
}

fn check_cancelled(token: &CancellationToken) -> Result<(), SnapshotError> {
    if token.is_cancelled() {
        Err(SnapshotError::Cancelled)
    } else {
        Ok(())
    }
}

fn limit_error(limit: SnapshotLimit, maximum: impl TryInto<u64>) -> SnapshotError {
    SnapshotError::LimitExceeded {
        limit,
        maximum: maximum.try_into().ok(),
    }
}

fn quota_error(limit: SnapshotLimit, maximum: impl TryInto<u64>) -> SnapshotError {
    SnapshotError::QuotaExceeded {
        limit,
        maximum: maximum.try_into().ok(),
    }
}

fn tree_error(error: SnapshotTreeError) -> SnapshotError {
    match error {
        SnapshotTreeError::LimitExceeded { limit, maximum } => limit_error(
            match limit {
                SnapshotTreeLimit::Entries => SnapshotLimit::CaptureEntries,
                SnapshotTreeLimit::PathBytes => SnapshotLimit::CapturePathBytes,
                SnapshotTreeLimit::Depth => SnapshotLimit::Depth,
            },
            maximum,
        ),
        SnapshotTreeError::IgnoreRulesExceeded => SnapshotError::LimitExceeded {
            limit: SnapshotLimit::IgnoreRules,
            maximum: None,
        },
        SnapshotTreeError::ScopeUnavailable
        | SnapshotTreeError::Protected
        | SnapshotTreeError::Blocked => SnapshotError::UnsupportedFile,
        SnapshotTreeError::Changed => SnapshotError::Conflict,
        SnapshotTreeError::Unsupported => SnapshotError::UnsupportedPlatform,
        SnapshotTreeError::Cancelled => SnapshotError::Cancelled,
        SnapshotTreeError::Failed(_) | SnapshotTreeError::Unsettled(_) => {
            SnapshotError::OperationFailed
        }
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        io::Write as _,
        os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink},
        sync::atomic::Ordering,
    };

    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;
    use workcell_host_contract::{
        RecordClientMetadata, RecordLimits, RecordListing, RecordScope, RecordState,
        RevertConflictKind, RevertState, UnrecordedReason,
    };
    use workcell_mcp_files::FileToolGroup;
    use workcell_snapshot_store::{ObjectId, blob_id};

    use super::*;
    use crate::{
        format::{StoredJournal, StoredRecord, encode, revert_id},
        snapshot::OWNER_EXECUTABLE,
        store::TestHooks,
    };

    const BINDING: &str = "0123456789abcdef";
    const ROOT: &str = ".";
    const HOLDER: &str = "session-a";
    const OTHER_HOLDER: &str = "session-b";
    const FILE: &str = "file.txt";
    const OTHER: &str = "other.txt";
    const THIRD: &str = "third.txt";
    const LINK: &str = "link";
    const DIRECTORY: &str = "dir";
    const NESTED: &str = "dir/nested.txt";
    const SECOND_NESTED: &str = "dir/second.txt";
    const DEEPER: &str = "dir/new/deeper.txt";
    const LINKED_DIRECTORY: &str = "linked";
    const THROUGH_LINKED_DIRECTORY: &str = "linked/nested.txt";
    const IGNORE_RULES: &str = "target/\n*.log\n";
    const IGNORED_FILE: &str = "build.log";
    const IGNORED_DIRECTORY: &str = "target";
    const BENEATH_IGNORED: &str = "target/debug";
    const BUILD_OUTPUT: &str = "target/debug/out.o";
    const NEW_BUILD_OUTPUT: &str = "target/debug/new.o";
    const INNER_REPOSITORY: &str = "target/inner";
    const INNER_REPOSITORY_MARKER: &str = "target/inner/.git";
    const INNER_OUTPUT: &str = "target/inner/out.o";
    const IGNORED_NESTED: &str = "dir/debug.log";
    const ONE_FILE: u32 = 1;
    const BEFORE: &str = "before";
    const AFTER: &str = "after";
    const LATER: &str = "later";
    const CANARY: &str = "written by someone else";
    const STAGED_SAVE: &str = "other.txt.save";
    const LARGE: &str = "larger than the record's file limit allows";
    const LARGER: &str = "larger still than the record's file limit allows";
    const GARBAGE: &str = "named by an abandoned record only";
    const SMALL_FILE_BYTES: u64 = 16;
    const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
    const PRIVATE_FILE_MODE: u32 = 0o600;
    const EXECUTABLE_MODE: u32 = 0o755;
    /// Older than any call may run.
    const STALE_AGE_MS: u64 = 13 * 60 * 60 * 1_000;
    const CONCURRENT_RECORDS: u64 = 24;
    const ADMISSION_MS: u64 = 50;
    const HELD_LOCK: Duration = Duration::from_secs(60);
    const LEGACY_DIRECTORIES: [&str; 2] = ["blobs", "manifests"];
    const LEGACY_FILES: [(&str, &str); 5] = [
        ("checkpoints", "workspace-snapshot-checkpoint.v1"),
        ("checkpoints", "workspace-snapshot-checkpoint.v2"),
        ("journals", "workspace-restore-journal.v1"),
        ("journals", "workspace-restore-journal.v2"),
        ("journals", "workspace-restore-journal.v3"),
    ];
    const LATER_CHECKPOINT_VERSION: &str = "workspace-snapshot-checkpoint.v3";
    const RECORDED: &str = "the call changed its scope, so it leaves a record";
    const NOTHING_WRITTEN: &str = "a refused revert must leave the workspace as it was";
    const LEFT_TO_WRITER: &str = "a revert must leave what another writer wrote while it ran";
    const KEPT: &str = "collection must keep what a record, an open record or a revert needs";

    struct Fixture {
        workspace: TempDir,
        storage: TempDir,
        manager: SnapshotManager,
    }

    #[derive(Clone, Copy, Debug)]
    enum Edit {
        Create,
        Modify,
        Delete,
        Relink,
        MakeExecutable,
        ChangeSubtree,
    }

    #[derive(Clone, Copy, Debug)]
    enum Unreadable {
        Record,
        OpenRecord,
        Journal,
    }

    /// A named path a write goes through to land on another one.
    #[derive(Clone, Copy, Debug)]
    enum Alias {
        Symlink,
        LinkedDirectory,
        HardLink,
    }

    /// Another writer acting on `OTHER` once a revert has published `FILE`.
    #[derive(Clone, Copy, Debug)]
    enum Race {
        /// Writes other bytes into the file `OTHER` shares with `FILE` through a hard link.
        Rewrite,
        /// Saves the bytes a separate `OTHER` already holds over it, as an editor saves.
        Resave,
    }

    impl Fixture {
        async fn new() -> Self {
            let workspace = tempfile::tempdir().unwrap();
            let storage = private_directory();
            let manager = open_manager(workspace.path(), storage.path())
                .await
                .unwrap();
            Self {
                workspace,
                storage,
                manager,
            }
        }

        /// Another process's view of the same store.
        async fn another(&self) -> SnapshotManager {
            open_manager(self.workspace.path(), self.storage.path())
                .await
                .unwrap()
        }

        async fn reopen(&mut self) {
            self.manager = self.another().await;
        }

        fn inner(&self) -> &Inner {
            &self.manager.store.shared.inner
        }

        fn hooks(&self) -> &TestHooks {
            &self.inner().store.hooks
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.workspace.path().join(relative)
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.path(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }

        fn read(&self, relative: &str) -> Option<String> {
            fs::read_to_string(self.path(relative)).ok()
        }

        /// Every file and link in the workspace with what a record keeps of it.
        fn tree(&self) -> BTreeMap<String, String> {
            let mut entries = BTreeMap::new();
            let mut pending = vec![PathBuf::new()];
            while let Some(relative) = pending.pop() {
                for entry in fs::read_dir(self.workspace.path().join(&relative)).unwrap() {
                    let entry = entry.unwrap();
                    let path = relative.join(entry.file_name());
                    let metadata = fs::symlink_metadata(entry.path()).unwrap();
                    let key = path.to_str().unwrap().to_owned();
                    if metadata.is_dir() {
                        pending.push(path);
                    } else if metadata.file_type().is_symlink() {
                        let target = fs::read_link(entry.path()).unwrap();
                        entries.insert(key, format!("link to {}", target.display()));
                    } else {
                        let executable = metadata.permissions().mode() & OWNER_EXECUTABLE != 0;
                        let content = fs::read_to_string(entry.path()).unwrap();
                        entries.insert(key, format!("{content} executable={executable}"));
                    }
                }
            }
            entries
        }

        async fn begin(&self, holder_id: &str, scope: RecordScope) -> Identifier {
            self.manager
                .begin_record(request(holder_id, scope), &token())
                .await
                .unwrap()
        }

        async fn finish(&self, ticket: &Identifier) -> Option<u64> {
            self.manager
                .finish_record(ticket, &token())
                .await
                .unwrap()
                .map(|summary| summary.seq)
        }

        /// Records `paths` around `change`, returning the record's seq if it left one.
        async fn record(&self, holder_id: &str, paths: &[&str], change: impl FnOnce(&Self)) -> u64 {
            let ticket = self.begin(holder_id, named(paths)).await;
            change(self);
            self.finish(&ticket).await.expect(RECORDED)
        }

        async fn prepare(
            &self,
            holder_id: &str,
            seqs: &[u64],
        ) -> Result<(PreparedRevert, RevertPreview), SnapshotError> {
            self.manager
                .prepare_revert(&holder(holder_id), seqs, usize::MAX, &token())
                .await
        }

        async fn execute(&self, prepared: &PreparedRevert) -> Result<RevertStatus, SnapshotError> {
            self.manager.execute_revert(prepared, &token()).await
        }

        async fn revert(&self, holder_id: &str, seqs: &[u64]) -> RevertStatus {
            let (prepared, _) = self.prepare(holder_id, seqs).await.unwrap();
            self.execute(&prepared).await.unwrap()
        }

        async fn unrevert(&self, holder_id: &str) -> RevertStatus {
            let (prepared, _) = self
                .manager
                .prepare_unrevert(&holder(holder_id), usize::MAX, &token())
                .await
                .unwrap();
            self.execute(&prepared).await.unwrap()
        }

        async fn listing(&self, holder_id: &str) -> Vec<RecordListing> {
            self.manager
                .store()
                .records(&holder(holder_id), None, MAX_RECORD_PAGE_SIZE)
                .await
                .unwrap()
                .records
        }

        async fn cleanup(&self, retention_bytes: u64) -> CleanupSummary {
            let (prepared, _) = self
                .manager
                .store()
                .prepare_cleanup(retention_bytes, usize::MAX)
                .await
                .unwrap();
            self.manager
                .store()
                .execute_cleanup(&prepared, &token())
                .await
                .unwrap()
        }

        fn stored(&self, seq: u64) -> Option<StoredRecord> {
            self.inner().store.record(seq).unwrap()
        }

        fn contains(&self, content: &str) -> bool {
            self.inner()
                .store
                .objects()
                .read_blob(&blob(content))
                .is_ok()
        }
    }

    impl Edit {
        fn named(self) -> &'static str {
            match self {
                Self::Relink => LINK,
                Self::ChangeSubtree => DIRECTORY,
                _ => FILE,
            }
        }

        fn prepare(self, fixture: &Fixture) {
            match self {
                Self::Create => {}
                Self::Modify | Self::Delete | Self::MakeExecutable => fixture.write(FILE, BEFORE),
                Self::Relink => symlink(BEFORE, fixture.path(LINK)).unwrap(),
                Self::ChangeSubtree => fixture.write(NESTED, BEFORE),
            }
        }

        fn apply(self, fixture: &Fixture) {
            match self {
                Self::Create | Self::Modify => fixture.write(FILE, AFTER),
                Self::Delete => fs::remove_file(fixture.path(FILE)).unwrap(),
                Self::Relink => {
                    fs::remove_file(fixture.path(LINK)).unwrap();
                    symlink(AFTER, fixture.path(LINK)).unwrap();
                }
                Self::MakeExecutable => set_mode(&fixture.path(FILE), EXECUTABLE_MODE),
                Self::ChangeSubtree => {
                    fixture.write(NESTED, AFTER);
                    fixture.write(DEEPER, AFTER);
                }
            }
        }
    }

    impl Alias {
        fn prepare(self, fixture: &Fixture) {
            fixture.write(self.real(), BEFORE);
            match self {
                Self::Symlink => symlink(FILE, fixture.path(LINK)).unwrap(),
                Self::LinkedDirectory => {
                    symlink(DIRECTORY, fixture.path(LINKED_DIRECTORY)).unwrap();
                }
                Self::HardLink => fs::hard_link(fixture.path(FILE), fixture.path(OTHER)).unwrap(),
            }
        }

        fn named(self) -> &'static str {
            match self {
                Self::Symlink => LINK,
                Self::LinkedDirectory => THROUGH_LINKED_DIRECTORY,
                Self::HardLink => OTHER,
            }
        }

        /// Where a write through the named path lands.
        fn real(self) -> &'static str {
            match self {
                Self::Symlink | Self::HardLink => FILE,
                Self::LinkedDirectory => NESTED,
            }
        }
    }

    impl Race {
        fn prepare(self, fixture: &Fixture) {
            fixture.write(FILE, BEFORE);
            match self {
                Self::Rewrite => fs::hard_link(fixture.path(FILE), fixture.path(OTHER)).unwrap(),
                Self::Resave => fixture.write(OTHER, BEFORE),
            }
        }

        fn written(self) -> &'static str {
            match self {
                Self::Rewrite => CANARY,
                Self::Resave => AFTER,
            }
        }

        fn run(self, other: &Path, staged: &Path) {
            match self {
                Self::Rewrite => fs::write(other, self.written()).unwrap(),
                Self::Resave => {
                    fs::write(staged, self.written()).unwrap();
                    fs::rename(staged, other).unwrap();
                }
            }
        }
    }

    fn private_directory() -> TempDir {
        let directory = tempfile::tempdir().unwrap();
        set_mode(directory.path(), PRIVATE_DIRECTORY_MODE);
        directory
    }

    fn private_subdirectory(path: &Path) {
        fs::create_dir(path).unwrap();
        set_mode(path, PRIVATE_DIRECTORY_MODE);
    }

    fn write_private(path: &Path, contents: &[u8]) {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .open(path)
            .unwrap()
            .write_all(contents)
            .unwrap();
    }

    fn versioned(version: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({ "version": version })).unwrap()
    }

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    async fn open_manager(
        workspace: &Path,
        storage: &Path,
    ) -> Result<SnapshotManager, SnapshotError> {
        let files = FileToolGroup::new(workspace, true, None).await.unwrap();
        SnapshotManager::open_bound(files.workspace_snapshot_access(), storage, &[], BINDING).await
    }

    fn token() -> CancellationToken {
        CancellationToken::new()
    }

    fn holder(value: &str) -> RecordHolder {
        RecordHolder::new(value).unwrap()
    }

    fn named(paths: &[&str]) -> RecordScope {
        RecordScope::Paths {
            paths: paths
                .iter()
                .map(|path| WorkspacePath::new(*path).unwrap())
                .collect(),
        }
    }

    fn whole(directory: &str) -> RecordScope {
        RecordScope::Workspace {
            directory: WorkspacePath::new(directory).unwrap(),
        }
    }

    fn client(value: serde_json::Value) -> RecordClientMetadata {
        RecordClientMetadata::new(value).unwrap()
    }

    fn request(holder_id: &str, scope: RecordScope) -> RecordRequest {
        RecordRequest {
            scope,
            holder: holder(holder_id),
            client: client(json!({ "holder": holder_id })),
            limits: RecordLimits {
                max_files: count(MAX_SNAPSHOT_FILES),
                max_file_bytes: MAX_SNAPSHOT_FILE_BYTES,
                max_total_bytes: MAX_SNAPSHOT_TOTAL_BYTES,
            },
        }
    }

    fn blob(content: &str) -> ObjectId {
        blob_id(content.as_bytes()).unwrap()
    }

    fn byte_len(content: &str) -> u64 {
        u64::try_from(content.len()).unwrap()
    }

    fn changed(record: &StoredRecord) -> Vec<&str> {
        record
            .changes
            .iter()
            .map(|change| change.path.as_str())
            .collect()
    }

    type Edge<'a> = (&'a str, Option<ObjectId>, Option<ObjectId>);

    fn edges(record: &StoredRecord) -> Vec<Edge<'_>> {
        record
            .changes
            .iter()
            .map(|change| {
                (
                    change.path.as_str(),
                    change.before.map(|content| content.0.oid),
                    change.after.map(|content| content.0.oid),
                )
            })
            .collect()
    }

    fn conflicts(preview: &RevertPreview) -> Vec<(&str, RevertConflictKind)> {
        preview
            .conflicts
            .iter()
            .map(|conflict| (conflict.path.as_str(), conflict.kind))
            .collect()
    }

    fn seqs(listings: &[RecordListing]) -> Vec<u64> {
        listings.iter().map(|listing| listing.seq).collect()
    }

    #[test_case(Edit::Create ; "creating a file")]
    #[test_case(Edit::Modify ; "modifying a file")]
    #[test_case(Edit::Delete ; "deleting a file")]
    #[test_case(Edit::Relink ; "retargeting a link")]
    #[test_case(Edit::MakeExecutable ; "making a file executable")]
    #[test_case(Edit::ChangeSubtree ; "changing a directory subtree")]
    #[tokio::test]
    async fn a_named_path_records_its_call_and_reverts_to_what_it_held(edit: Edit) {
        let fixture = Fixture::new().await;
        edit.prepare(&fixture);
        let before = fixture.tree();

        let seq = fixture
            .record(HOLDER, &[edit.named()], |fixture| edit.apply(fixture))
            .await;
        assert_ne!(fixture.tree(), before);
        let status = fixture.revert(HOLDER, &[seq]).await;

        assert_eq!(status.pending[0].state, RevertState::Completed);
        assert_eq!(fixture.tree(), before);
    }

    #[test_case(IGNORED_FILE, &[IGNORED_FILE, NESTED] ; "a named ignored file is recorded")]
    #[test_case(IGNORED_DIRECTORY, &[NESTED] ; "a named ignored directory holds nothing")]
    #[test_case(BENEATH_IGNORED, &[NESTED] ; "a named directory beneath an ignored one holds nothing")]
    #[test_case(INNER_REPOSITORY, &[NESTED, INNER_OUTPUT] ; "a named repository beneath an ignored directory answers to its own rules")]
    #[tokio::test]
    async fn ignore_rules_apply_beneath_a_named_path_as_git_applies_them(
        path: &str,
        recorded: &[&str],
    ) {
        let fixture = Fixture::new().await;
        fixture.write(".gitignore", IGNORE_RULES);
        fixture.write(BUILD_OUTPUT, BEFORE);
        fixture.write(INNER_REPOSITORY_MARKER, "");

        let seq = fixture
            .record(HOLDER, &[path, DIRECTORY], |fixture| {
                for written in [
                    IGNORED_FILE,
                    BUILD_OUTPUT,
                    NEW_BUILD_OUTPUT,
                    INNER_OUTPUT,
                    IGNORED_NESTED,
                    NESTED,
                ] {
                    fixture.write(written, AFTER);
                }
            })
            .await;

        assert_eq!(changed(&fixture.stored(seq).unwrap()), recorded);
    }

    #[test_case(Alias::Symlink ; "a named link to a file")]
    #[test_case(Alias::LinkedDirectory ; "a named path behind a linked directory")]
    #[test_case(Alias::HardLink ; "a named file with a second hard link")]
    #[tokio::test]
    async fn a_write_through_a_named_path_is_recorded_where_it_lands(alias: Alias) {
        let mut fixture = Fixture::new().await;
        alias.prepare(&fixture);
        let before = fixture.tree();

        let ticket = fixture.begin(HOLDER, named(&[alias.named()])).await;
        fixture.reopen().await;
        fs::write(fixture.path(alias.named()), AFTER).unwrap();
        let seq = fixture.finish(&ticket).await.expect(RECORDED);

        assert!(changed(&fixture.stored(seq).unwrap()).contains(&alias.real()));
        let status = fixture.revert(HOLDER, &[seq]).await;
        assert_eq!(status.pending[0].state, RevertState::Completed);
        assert_eq!(fixture.tree(), before);
    }

    #[test_case(Race::Rewrite ; "other bytes in a file the revert replaced under another name")]
    #[test_case(Race::Resave ; "the same bytes saved over a separate file")]
    #[tokio::test]
    async fn a_revert_stops_at_a_path_another_writer_changes_while_it_runs(race: Race) {
        let fixture = Fixture::new().await;
        race.prepare(&fixture);
        let seq = fixture
            .record(HOLDER, &[FILE, OTHER], |fixture| {
                fixture.write(FILE, AFTER);
                fixture.write(OTHER, AFTER);
            })
            .await;
        let (other, staged) = (fixture.path(OTHER), fixture.path(STAGED_SAVE));
        *fixture.hooks().publication.lock().unwrap() = Some(Box::new(move |published: &str| {
            if published == FILE {
                race.run(&other, &staged);
            }
        }));

        let status = fixture.revert(HOLDER, &[seq]).await;

        let pending = &status.pending[0];
        assert_eq!(
            (
                pending.state,
                pending.applied_files,
                pending.stopped_at.as_ref().map(WorkspacePath::as_str)
            ),
            (RevertState::Partial, 1, Some(OTHER))
        );
        assert_eq!(
            fs::read(fixture.path(OTHER)).unwrap(),
            race.written().as_bytes(),
            "{LEFT_TO_WRITER}"
        );
        assert_eq!(fixture.read(FILE).as_deref(), Some(BEFORE));
    }

    #[test_case(SnapshotLimit::Files ; "more files than it allows")]
    #[test_case(SnapshotLimit::TotalBytes ; "more bytes than it allows")]
    #[tokio::test]
    async fn a_named_directory_past_a_record_limit_is_refused_and_leaves_nothing_behind(
        limit: SnapshotLimit,
    ) {
        let fixture = Fixture::new().await;
        for path in [NESTED, SECOND_NESTED] {
            fixture.write(path, BEFORE);
        }
        let mut request = request(HOLDER, named(&[DIRECTORY]));
        let maximum = match limit {
            SnapshotLimit::Files => {
                request.limits.max_files = ONE_FILE;
                u64::from(ONE_FILE)
            }
            _ => {
                request.limits.max_total_bytes = byte_len(BEFORE);
                byte_len(BEFORE)
            }
        };
        let usage = fixture.inner().store.usage().unwrap();

        let refused = fixture
            .manager
            .begin_record(request, &token())
            .await
            .unwrap_err();

        assert_eq!(
            refused,
            SnapshotError::LimitExceeded {
                limit,
                maximum: Some(maximum)
            }
        );
        assert!(
            fixture
                .manager
                .store()
                .open_records(&holder(HOLDER))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(fixture.inner().store.usage().unwrap(), usage);
    }

    #[test_case(&[DIRECTORY, NESTED], &[NESTED, SECOND_NESTED] ; "a path named inside a named directory")]
    #[test_case(&[LINK], &[FILE, LINK, NESTED, SECOND_NESTED] ; "a named link the widened record walks again")]
    #[tokio::test]
    async fn a_path_captured_twice_counts_once_against_the_record_limits(
        paths: &[&str],
        held: &[&str],
    ) {
        let fixture = Fixture::new().await;
        for path in [FILE, NESTED, SECOND_NESTED] {
            fixture.write(path, BEFORE);
        }
        symlink(FILE, fixture.path(LINK)).unwrap();
        let mut request = request(HOLDER, named(paths));
        request.limits.max_files = u32::try_from(held.len()).unwrap();
        request.limits.max_total_bytes = held
            .iter()
            .map(|path| fs::symlink_metadata(fixture.path(path)).unwrap().len())
            .sum();

        fixture
            .manager
            .begin_record(request, &token())
            .await
            .unwrap();
    }

    #[test_case(NESTED, UnrecordedReason::Blocked ; "behind a directory the call replaced with a link")]
    #[test_case(FILE, UnrecordedReason::Oversized ; "over the file limit")]
    #[tokio::test]
    async fn a_named_path_a_record_cannot_store_is_unrecorded(
        path: &str,
        reason: UnrecordedReason,
    ) {
        let fixture = Fixture::new().await;
        fixture.write(NESTED, BEFORE);
        fixture.write(FILE, LARGE);
        let mut request = request(HOLDER, named(&[path]));
        request.limits.max_file_bytes = SMALL_FILE_BYTES;

        let ticket = fixture
            .manager
            .begin_record(request, &token())
            .await
            .unwrap();
        fs::rename(fixture.path(DIRECTORY), fixture.path(LINKED_DIRECTORY)).unwrap();
        symlink(LINKED_DIRECTORY, fixture.path(DIRECTORY)).unwrap();
        fixture.write(FILE, LARGER);
        let seq = fixture.finish(&ticket).await.expect(RECORDED);

        let record = fixture.stored(seq).unwrap();
        assert!(record.changes.is_empty());
        assert_eq!(
            record
                .unrecorded
                .iter()
                .map(|unrecorded| (unrecorded.path.as_str(), unrecorded.reason))
                .collect::<Vec<_>>(),
            [(path, reason)]
        );
    }

    #[test_case("../outside" ; "above the root")]
    #[test_case("dir/../../outside" ; "through a parent")]
    #[test_case("." ; "the root itself")]
    #[test_case("dir//file.txt" ; "with an empty component")]
    #[tokio::test]
    async fn a_named_path_that_is_not_plainly_inside_the_root_is_refused(path: &str) {
        let fixture = Fixture::new().await;

        assert_eq!(
            fixture
                .manager
                .begin_record(request(HOLDER, named(&[path])), &token())
                .await
                .unwrap_err(),
            SnapshotError::InvalidRequest
        );
    }

    #[tokio::test]
    async fn a_workspace_record_holds_only_what_changed() {
        let fixture = Fixture::new().await;
        for path in [FILE, OTHER, NESTED] {
            fixture.write(path, BEFORE);
        }

        let ticket = fixture.begin(HOLDER, whole(ROOT)).await;
        fixture.write(OTHER, AFTER);
        let seq = fixture.finish(&ticket).await.expect(RECORDED);

        assert_eq!(changed(&fixture.stored(seq).unwrap()), [OTHER]);
    }

    #[test_case(named(&[FILE]) ; "for named paths")]
    #[test_case(whole(ROOT) ; "for the workspace")]
    #[tokio::test]
    async fn a_call_that_changes_nothing_leaves_no_record(scope: RecordScope) {
        let fixture = Fixture::new().await;
        fixture.write(FILE, BEFORE);

        let ticket = fixture.begin(HOLDER, scope).await;

        assert_eq!(fixture.finish(&ticket).await, None);
        assert!(fixture.listing(HOLDER).await.is_empty());
        assert!(
            fixture
                .manager
                .store()
                .open_records(&holder(HOLDER))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn overlapping_records_that_saw_the_same_change_leave_it_with_one_owner() {
        let fixture = Fixture::new().await;
        fixture.write(FILE, BEFORE);
        let outer = fixture.begin(HOLDER, whole(ROOT)).await;
        let inner = fixture.begin(HOLDER, named(&[FILE])).await;

        fixture.write(FILE, AFTER);
        let seq = fixture.finish(&inner).await.expect(RECORDED);

        assert_eq!(fixture.finish(&outer).await, None);
        assert_eq!(changed(&fixture.stored(seq).unwrap()), [FILE]);
    }

    #[tokio::test]
    async fn overlapping_records_with_consecutive_changes_form_a_chain() {
        let fixture = Fixture::new().await;
        fixture.write(FILE, BEFORE);
        let outer = fixture.begin(HOLDER, named(&[FILE])).await;
        let inner = fixture.begin(HOLDER, named(&[FILE])).await;

        fixture.write(FILE, AFTER);
        let first = fixture.finish(&inner).await.expect(RECORDED);
        fixture.write(FILE, LATER);
        let second = fixture.finish(&outer).await.expect(RECORDED);

        assert_eq!(
            edges(&fixture.stored(first).unwrap()),
            [(FILE, Some(blob(BEFORE)), Some(blob(AFTER)))]
        );
        assert_eq!(
            edges(&fixture.stored(second).unwrap()),
            [(FILE, Some(blob(AFTER)), Some(blob(LATER)))]
        );
        fixture.revert(HOLDER, &[first, second]).await;
        assert_eq!(fixture.read(FILE).as_deref(), Some(BEFORE));
    }

    #[test_case(&[0, 1], BEFORE ; "both records")]
    #[test_case(&[1], AFTER ; "the newer record")]
    #[tokio::test]
    async fn reverting_records_composes_them_newest_first(selected: &[usize], expected: &str) {
        let fixture = Fixture::new().await;
        fixture.write(FILE, BEFORE);
        let records = [
            fixture
                .record(HOLDER, &[FILE], |fixture| fixture.write(FILE, AFTER))
                .await,
            fixture
                .record(HOLDER, &[FILE], |fixture| fixture.write(FILE, LATER))
                .await,
        ];
        let selected = selected
            .iter()
            .map(|index| records[*index])
            .collect::<Vec<_>>();

        let (prepared, preview) = fixture.prepare(HOLDER, &selected).await.unwrap();
        assert_eq!(preview.counts.replace, 1);
        fixture.execute(&prepared).await.unwrap();

        assert_eq!(fixture.read(FILE).as_deref(), Some(expected));
    }

    #[tokio::test]
    async fn a_canary_on_an_unrecorded_path_is_never_written() {
        let fixture = Fixture::new().await;
        fixture.write(FILE, BEFORE);
        fixture.write(OTHER, LARGE);
        let mut request = request(HOLDER, named(&[FILE, OTHER]));
        request.limits.max_file_bytes = SMALL_FILE_BYTES;
        let ticket = fixture
            .manager
            .begin_record(request, &token())
            .await
            .unwrap();
        fixture.write(FILE, AFTER);
        fixture.write(OTHER, LARGER);
        let seq = fixture.finish(&ticket).await.expect(RECORDED);
        fixture.write(OTHER, CANARY);

        let (prepared, preview) = fixture.prepare(HOLDER, &[seq]).await.unwrap();

        assert_eq!(
            conflicts(&preview),
            [(OTHER, RevertConflictKind::Unrecorded)]
        );
        assert_eq!(
            fixture.execute(&prepared).await.unwrap_err(),
            SnapshotError::Conflict
        );
        assert_eq!(fixture.read(OTHER).as_deref(), Some(CANARY));
        assert_eq!(fixture.read(FILE).as_deref(), Some(AFTER));
    }

    #[test_case(RevertConflictKind::ChangedSince ; "a path changed since")]
    #[test_case(RevertConflictKind::Interleaved ; "a path changed between records")]
    #[test_case(RevertConflictKind::Unrecorded ; "a path a record could not store")]
    #[tokio::test]
    async fn any_conflict_refuses_the_whole_revert_and_names_every_conflicting_path(
        kind: RevertConflictKind,
    ) {
        let fixture = Fixture::new().await;
        let troubled = [OTHER, THIRD];
        let initial = if kind == RevertConflictKind::Unrecorded {
            LARGE
        } else {
            BEFORE
        };
        fixture.write(FILE, BEFORE);
        for path in troubled {
            fixture.write(path, initial);
        }
        let mut request = request(HOLDER, named(&[FILE, OTHER, THIRD]));
        request.limits.max_file_bytes = SMALL_FILE_BYTES;
        let ticket = fixture
            .manager
            .begin_record(request, &token())
            .await
            .unwrap();
        fixture.write(FILE, AFTER);
        for path in troubled {
            fixture.write(
                path,
                if kind == RevertConflictKind::Unrecorded {
                    LARGER
                } else {
                    AFTER
                },
            );
        }
        let mut selected = vec![fixture.finish(&ticket).await.expect(RECORDED)];
        match kind {
            RevertConflictKind::ChangedSince => {
                for path in troubled {
                    fixture.write(path, CANARY);
                }
            }
            RevertConflictKind::Interleaved => {
                for path in troubled {
                    fixture.write(path, CANARY);
                }
                selected.push(
                    fixture
                        .record(HOLDER, &troubled, |fixture| {
                            for path in troubled {
                                fixture.write(path, LATER);
                            }
                        })
                        .await,
                );
            }
            RevertConflictKind::Unrecorded => {}
        }
        let before = fixture.tree();

        let (prepared, preview) = fixture.prepare(HOLDER, &selected).await.unwrap();

        assert_eq!(conflicts(&preview), [(OTHER, kind), (THIRD, kind)]);
        assert_eq!(preview.counts.conflicts, 2);
        assert_eq!(
            fixture.execute(&prepared).await.unwrap_err(),
            SnapshotError::Conflict
        );
        assert_eq!(fixture.tree(), before, "{NOTHING_WRITTEN}");
        assert!(
            fixture
                .manager
                .store()
                .status(&holder(HOLDER))
                .await
                .unwrap()
                .pending
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_path_already_back_where_it_started_is_left_unchanged() {
        let fixture = Fixture::new().await;
        fixture.write(FILE, BEFORE);
        let seq = fixture
            .record(HOLDER, &[FILE], |fixture| fixture.write(FILE, AFTER))
            .await;
        fixture.write(FILE, BEFORE);

        let (prepared, preview) = fixture.prepare(HOLDER, &[seq]).await.unwrap();
        let status = fixture.execute(&prepared).await.unwrap();

        assert_eq!(
            (
                preview.counts.unchanged,
                preview.planned.len(),
                preview.conflicts.len()
            ),
            (1, 0, 0)
        );
        assert_eq!(status.pending[0].state, RevertState::Completed);
        assert_eq!(fixture.read(FILE).as_deref(), Some(BEFORE));
    }

    #[tokio::test]
    async fn an_unrevert_reapplies_every_pending_revert() {
        let fixture = Fixture::new().await;
        fixture.write(FILE, BEFORE);
        fixture.write(OTHER, BEFORE);
        let first = fixture
            .record(HOLDER, &[FILE], |fixture| fixture.write(FILE, AFTER))
            .await;
        let second = fixture
            .record(HOLDER, &[OTHER], |fixture| fixture.write(OTHER, AFTER))
            .await;
        fixture.revert(HOLDER, &[second]).await;
        let stacked = fixture.revert(HOLDER, &[first]).await;
        assert_eq!(stacked.pending.len(), 2);
        assert_eq!(fixture.read(FILE).as_deref(), Some(BEFORE));

        let status = fixture.unrevert(HOLDER).await;

        assert!(status.pending.is_empty());
        assert_eq!(fixture.read(FILE).as_deref(), Some(AFTER));
        assert_eq!(fixture.read(OTHER).as_deref(), Some(AFTER));
        assert!(
            fixture
                .listing(HOLDER)
                .await
                .iter()
                .all(|listing| listing.state == RecordState::Applied)
        );
    }

    #[tokio::test]
    async fn acknowledging_deletes_the_reverted_records_for_every_holder() {
        let fixture = Fixture::new().await;
        fixture.write(FILE, BEFORE);
        let seq = fixture
            .record(HOLDER, &[FILE], |fixture| fixture.write(FILE, AFTER))
            .await;
        let store = fixture.manager.store();
        store
            .hold(&holder(HOLDER), &holder(OTHER_HOLDER))
            .await
            .unwrap();
        fixture.revert(HOLDER, &[seq]).await;

        let status = store.acknowledge(&holder(HOLDER)).await.unwrap();

        assert!(status.pending.is_empty());
        assert!(fixture.listing(HOLDER).await.is_empty());
        assert!(fixture.listing(OTHER_HOLDER).await.is_empty());
        assert!(fixture.stored(seq).is_none());
        assert_eq!(fixture.read(FILE).as_deref(), Some(BEFORE));
    }

    #[tokio::test]
    async fn a_pending_revert_keeps_its_records_from_every_other_holder() {
        let fixture = Fixture::new().await;
        fixture.write(FILE, BEFORE);
        let seq = fixture
            .record(HOLDER, &[FILE], |fixture| fixture.write(FILE, AFTER))
            .await;
        let store = fixture.manager.store();
        store
            .hold(&holder(HOLDER), &holder(OTHER_HOLDER))
            .await
            .unwrap();

        fixture.revert(HOLDER, &[seq]).await;

        assert_eq!(
            fixture.listing(OTHER_HOLDER).await[0].state,
            RecordState::Reverted
        );
        assert_eq!(
            fixture.prepare(OTHER_HOLDER, &[seq]).await.err(),
            Some(SnapshotError::Conflict)
        );
        assert_eq!(
            store
                .release(&holder(OTHER_HOLDER), &ReleaseSelection::Seqs(vec![seq]))
                .await
                .unwrap_err(),
            SnapshotError::Conflict
        );
    }

    #[test_case(&[], SnapshotError::InvalidRequest ; "naming nothing")]
    #[test_case(&[1, 2], SnapshotError::NotFound ; "naming a record that does not exist")]
    #[tokio::test]
    async fn a_revert_of_records_the_holder_does_not_hold_is_refused(
        extra: &[u64],
        expected: SnapshotError,
    ) {
        let fixture = Fixture::new().await;
        fixture.write(FILE, BEFORE);
        fixture
            .record(HOLDER, &[FILE], |fixture| fixture.write(FILE, AFTER))
            .await;

        let refused = fixture.prepare(OTHER_HOLDER, &[1]).await.err();
        let selected = fixture.prepare(HOLDER, extra).await.err();

        assert_eq!(refused, Some(SnapshotError::NotFound));
        assert_eq!(selected, Some(expected));
    }

    #[test_case(true, RevertState::Completed, 2 ; "after publishing everything")]
    #[test_case(false, RevertState::Partial, 1 ; "midway")]
    #[tokio::test]
    async fn an_interrupted_revert_is_reconciled_when_the_store_opens(
        published_everything: bool,
        state: RevertState,
        applied: u32,
    ) {
        let mut fixture = Fixture::new().await;
        fixture.write(FILE, BEFORE);
        fixture.write(OTHER, BEFORE);
        let seq = fixture
            .record(HOLDER, &[FILE, OTHER], |fixture| {
                fixture.write(FILE, AFTER);
                fixture.write(OTHER, AFTER);
            })
            .await;
        let journal = StoredJournal::new(
            revert_id(),
            HOLDER,
            0,
            RevertDirection::Revert,
            vec![seq],
            2,
            0,
        );
        fixture.inner().store.write_journal(&journal).unwrap();
        fixture.write(FILE, BEFORE);
        if published_everything {
            fixture.write(OTHER, BEFORE);
        }

        fixture.reopen().await;
        let status = fixture
            .manager
            .store()
            .status(&holder(HOLDER))
            .await
            .unwrap();

        let pending = &status.pending[0];
        assert_eq!(
            (
                pending.state,
                pending.applied_files,
                pending.reconciliation_required
            ),
            (state, applied, false)
        );
        assert!(fixture.unrevert(HOLDER).await.pending.is_empty());
        assert_eq!(fixture.read(FILE).as_deref(), Some(AFTER));
        assert_eq!(fixture.read(OTHER).as_deref(), Some(AFTER));
    }

    #[tokio::test]
    async fn an_open_record_survives_a_reopen_and_still_finishes() {
        let mut fixture = Fixture::new().await;
        fixture.write(FILE, BEFORE);
        let ticket = fixture.begin(HOLDER, named(&[FILE])).await;

        fixture.reopen().await;
        let open = fixture
            .manager
            .store()
            .open_records(&holder(HOLDER))
            .await
            .unwrap();
        fixture.write(FILE, AFTER);
        let seq = fixture.finish(&ticket).await.expect(RECORDED);

        assert_eq!(open.len(), 1);
        assert_eq!(open[0].ticket, ticket);
        fixture.revert(HOLDER, &[seq]).await;
        assert_eq!(fixture.read(FILE).as_deref(), Some(BEFORE));
    }

    #[tokio::test]
    async fn cleanup_abandons_open_records_older_than_any_call_runs() {
        let fixture = Fixture::new().await;
        let stale = fixture.begin(HOLDER, named(&[FILE])).await;
        let fresh = fixture.begin(HOLDER, named(&[OTHER])).await;
        let store = &fixture.inner().store;
        let mut open = store.open_record(stale.as_str()).unwrap();
        open.opened_at_unix_ms -= STALE_AGE_MS;
        store
            .write_atomic(
                &store.open_path(stale.as_str()).unwrap(),
                &encode(&open).unwrap(),
            )
            .unwrap();

        let (prepared, preview) = fixture
            .manager
            .store()
            .prepare_cleanup(u64::MAX, usize::MAX)
            .await
            .unwrap();
        let summary = fixture
            .manager
            .store()
            .execute_cleanup(&prepared, &token())
            .await
            .unwrap();

        assert_eq!(preview.stale_open_records, 1);
        assert_eq!(summary.abandoned_open_records, 1);
        let remaining = fixture
            .manager
            .store()
            .open_records(&holder(HOLDER))
            .await
            .unwrap();
        assert_eq!(
            remaining
                .iter()
                .map(|open| &open.ticket)
                .collect::<Vec<_>>(),
            [&fresh]
        );
    }

    #[tokio::test]
    async fn a_record_moves_between_holders_and_goes_with_its_last() {
        let fixture = Fixture::new().await;
        fixture.write(FILE, BEFORE);
        let seq = fixture
            .record(HOLDER, &[FILE], |fixture| fixture.write(FILE, AFTER))
            .await;
        let store = fixture.manager.store();

        let held = store
            .hold(&holder(HOLDER), &holder(OTHER_HOLDER))
            .await
            .unwrap();
        let first = store
            .release(&holder(HOLDER), &ReleaseSelection::All)
            .await
            .unwrap();
        assert!(fixture.listing(HOLDER).await.is_empty());
        assert_eq!(seqs(&fixture.listing(OTHER_HOLDER).await), [seq]);
        let last = store
            .release(&holder(OTHER_HOLDER), &ReleaseSelection::Seqs(vec![seq]))
            .await
            .unwrap();

        assert_eq!(held, 1);
        assert_eq!(
            first,
            ReleaseSummary {
                released: 1,
                deleted: 0
            }
        );
        assert_eq!(
            last,
            ReleaseSummary {
                released: 1,
                deleted: 1
            }
        );
        assert!(fixture.stored(seq).is_none());
    }

    #[tokio::test]
    async fn collection_keeps_what_open_finished_and_pending_records_need() {
        let fixture = Fixture::new().await;
        let content = |path: &str, state: &str| format!("{path} {state}");
        for path in [FILE, OTHER, THIRD] {
            fixture.write(path, &content(path, BEFORE));
        }
        fixture.write(NESTED, GARBAGE);
        let finished = fixture
            .record(HOLDER, &[FILE], |fixture| {
                fixture.write(FILE, &content(FILE, AFTER));
            })
            .await;
        let pending = fixture
            .record(HOLDER, &[OTHER], |fixture| {
                fixture.write(OTHER, &content(OTHER, AFTER));
            })
            .await;
        fixture.revert(HOLDER, &[pending]).await;
        let abandoned = fixture.begin(HOLDER, named(&[NESTED])).await;
        assert!(
            fixture
                .manager
                .store()
                .abandon_record(&abandoned)
                .await
                .unwrap()
        );
        fs::remove_file(fixture.path(NESTED)).unwrap();
        let open = fixture.begin(HOLDER, named(&[THIRD])).await;
        fixture.write(THIRD, &content(THIRD, AFTER));

        let collected = fixture.cleanup(u64::MAX).await;

        assert!(collected.deleted_objects > 0);
        assert!(!fixture.contains(GARBAGE));
        for (path, state) in [
            (FILE, BEFORE),
            (FILE, AFTER),
            (OTHER, BEFORE),
            (OTHER, AFTER),
            (THIRD, BEFORE),
        ] {
            assert!(
                fixture.contains(&content(path, state)),
                "{KEPT}: {path} {state}"
            );
        }
        let evicted = fixture.cleanup(0).await;
        assert_eq!(evicted.evicted_records, 1);
        assert!(fixture.stored(finished).is_none());
        for (path, state) in [(OTHER, BEFORE), (OTHER, AFTER), (THIRD, BEFORE)] {
            assert!(
                fixture.contains(&content(path, state)),
                "{KEPT}: {path} {state}"
            );
        }
        let third = fixture.finish(&open).await.expect(RECORDED);
        fixture
            .manager
            .store()
            .acknowledge(&holder(HOLDER))
            .await
            .unwrap();
        fixture.revert(HOLDER, &[third]).await;
        assert_eq!(fixture.read(THIRD), Some(content(THIRD, BEFORE)));
    }

    #[test_case(Unreadable::Record ; "a record")]
    #[test_case(Unreadable::OpenRecord ; "an open record")]
    #[test_case(Unreadable::Journal ; "a revert journal")]
    #[tokio::test]
    async fn collection_refuses_to_run_when_anything_it_keeps_cannot_be_read(
        unreadable: Unreadable,
    ) {
        let fixture = Fixture::new().await;
        fixture.write(FILE, BEFORE);
        let seq = fixture
            .record(HOLDER, &[FILE], |fixture| fixture.write(FILE, AFTER))
            .await;
        let store = &fixture.inner().store;
        let damaged = match unreadable {
            Unreadable::Record => store.record_path(seq),
            Unreadable::OpenRecord => {
                let ticket = fixture.begin(HOLDER, named(&[OTHER])).await;
                store.open_path(ticket.as_str()).unwrap()
            }
            Unreadable::Journal => {
                let journal = StoredJournal::new(
                    revert_id(),
                    HOLDER,
                    0,
                    RevertDirection::Revert,
                    vec![seq],
                    1,
                    0,
                );
                store.write_journal(&journal).unwrap();
                store.journal_path(&journal.revert_id).unwrap()
            }
        };
        fs::write(&damaged, "damaged").unwrap();
        let objects = store.object_count().unwrap();

        let refused = fixture
            .manager
            .store()
            .prepare_cleanup(0, usize::MAX)
            .await
            .err();

        assert_eq!(refused, Some(SnapshotError::UnhealthyStorage));
        assert_eq!(store.object_count().unwrap(), objects);
    }

    #[tokio::test]
    async fn retention_evicts_the_oldest_records_and_reports_the_newest_it_evicted() {
        let fixture = Fixture::new().await;
        fixture.write(FILE, BEFORE);
        let mut records = Vec::new();
        for call in 0..3 {
            let mut request = request(HOLDER, named(&[FILE]));
            request.client = client(json!({ "call": call }));
            let ticket = fixture
                .manager
                .begin_record(request, &token())
                .await
                .unwrap();
            fixture.write(FILE, &call.to_string());
            records.push(fixture.finish(&ticket).await.expect(RECORDED));
        }
        fixture.cleanup(u64::MAX).await;
        let usage = fixture.manager.store().inventory().await.unwrap().bytes;

        let summary = fixture.cleanup(usage - 1).await;
        let page = fixture
            .manager
            .store()
            .records(&holder(HOLDER), None, MAX_RECORD_PAGE_SIZE)
            .await
            .unwrap();

        assert_eq!(summary.evicted_records, 1);
        assert_eq!(seqs(&page.records), records[1..]);
        assert_eq!(page.evicted_through, Some(client(json!({ "call": 0 }))));
    }

    #[tokio::test]
    async fn retention_keeps_every_record_an_open_record_may_still_rebase_onto() {
        let fixture = Fixture::new().await;
        fixture.write(FILE, BEFORE);
        fixture.write(OTHER, BEFORE);
        let older = fixture
            .record(HOLDER, &[OTHER], |fixture| fixture.write(OTHER, AFTER))
            .await;
        let open = fixture.begin(HOLDER, named(&[FILE])).await;
        let inside = fixture
            .record(OTHER_HOLDER, &[FILE], |fixture| fixture.write(FILE, AFTER))
            .await;

        let summary = fixture.cleanup(0).await;

        assert_eq!(summary.evicted_records, 1);
        assert!(fixture.stored(older).is_none());
        assert!(fixture.stored(inside).is_some());
        assert_eq!(fixture.finish(&open).await, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_stores_on_one_binding_commit_unique_seqs_without_gaps() {
        let fixture = Fixture::new().await;
        let managers = [fixture.manager.clone(), fixture.another().await];
        let tasks = managers.into_iter().enumerate().map(|(process, manager)| {
            let workspace = fixture.workspace.path().to_path_buf();
            tokio::spawn(async move {
                let mut seqs = Vec::new();
                for call in 0..CONCURRENT_RECORDS {
                    let path = format!("{process}-{call}.txt");
                    let ticket = manager
                        .begin_record(request(HOLDER, named(&[&path])), &token())
                        .await
                        .unwrap();
                    fs::write(workspace.join(&path), &path).unwrap();
                    let summary = manager.finish_record(&ticket, &token()).await.unwrap();
                    seqs.push(summary.expect(RECORDED).seq);
                }
                seqs
            })
        });
        let mut committed = Vec::new();
        for task in tasks.collect::<Vec<_>>() {
            committed.extend(task.await.unwrap());
        }
        committed.sort_unstable();

        assert_eq!(committed, (1..=2 * CONCURRENT_RECORDS).collect::<Vec<_>>());
        assert_eq!(
            seqs(&fixture.listing(HOLDER).await).len(),
            usize::try_from(2 * CONCURRENT_RECORDS).unwrap()
        );
    }

    #[tokio::test]
    async fn a_store_another_process_holds_is_busy_once_admission_times_out() {
        let fixture = Fixture::new().await;
        let other = fixture.another().await;
        let _held = other
            .store
            .shared
            .inner
            .store
            .lock(Instant::now() + HELD_LOCK, &token())
            .unwrap();
        fixture
            .hooks()
            .admission_ms
            .store(ADMISSION_MS, Ordering::SeqCst);

        assert_eq!(
            fixture.manager.store().inventory().await.unwrap_err(),
            SnapshotError::Busy
        );
    }

    #[tokio::test]
    async fn listing_pages_through_records_in_seq_order() {
        let fixture = Fixture::new().await;
        let mut records = Vec::new();
        for path in [FILE, OTHER, THIRD] {
            records.push(
                fixture
                    .record(HOLDER, &[path], |fixture| fixture.write(path, AFTER))
                    .await,
            );
        }
        let store = fixture.manager.store();

        let first = store.records(&holder(HOLDER), None, 2).await.unwrap();
        let second = store
            .records(&holder(HOLDER), first.next_after_seq, 2)
            .await
            .unwrap();

        assert_eq!(seqs(&first.records), records[..2]);
        assert_eq!(first.next_after_seq, Some(records[1]));
        assert_eq!(seqs(&second.records), records[2..]);
        assert_eq!(second.next_after_seq, None);
    }

    #[tokio::test]
    async fn a_store_opens_without_its_workspace_to_list_and_release() {
        let fixture = Fixture::new().await;
        fixture.write(FILE, BEFORE);
        fixture
            .record(HOLDER, &[FILE], |fixture| fixture.write(FILE, AFTER))
            .await;

        let store = ChangeStore::open(fixture.storage.path(), BINDING)
            .await
            .unwrap();
        let inventory = store.inventory().await.unwrap();
        let released = store
            .release(&holder(HOLDER), &ReleaseSelection::All)
            .await
            .unwrap();

        assert_eq!((inventory.records, inventory.holders.len()), (1, 1));
        assert_eq!(
            released,
            ReleaseSummary {
                released: 1,
                deleted: 1
            }
        );
        assert_eq!(
            ChangeStore::open(fixture.storage.path(), "absent")
                .await
                .err(),
            Some(SnapshotError::NotFound)
        );
    }

    #[tokio::test]
    async fn what_earlier_formats_left_is_deleted_when_the_store_opens() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = private_directory();
        let root = storage.path().join(BINDING);
        private_subdirectory(&root);
        for directory in LEGACY_DIRECTORIES
            .into_iter()
            .chain(LEGACY_FILES.map(|(directory, _)| directory))
        {
            if !root.join(directory).exists() {
                private_subdirectory(&root.join(directory));
            }
        }
        for (index, (directory, version)) in LEGACY_FILES.into_iter().enumerate() {
            write_private(
                &root.join(directory).join(format!("{index}.json")),
                &versioned(version),
            );
        }

        open_manager(workspace.path(), storage.path())
            .await
            .unwrap();

        for (directory, _) in LEGACY_FILES {
            assert!(!root.join(directory).exists());
        }
        for directory in LEGACY_DIRECTORIES {
            assert!(!root.join(directory).exists());
        }
    }

    #[tokio::test]
    async fn a_store_holding_a_format_this_release_does_not_know_is_refused() {
        let workspace = tempfile::tempdir().unwrap();
        let storage = private_directory();
        let root = storage.path().join(BINDING);
        private_subdirectory(&root);
        private_subdirectory(&root.join("checkpoints"));
        let later = root.join("checkpoints").join("later.json");
        write_private(&later, &versioned(LATER_CHECKPOINT_VERSION));

        let refused = open_manager(workspace.path(), storage.path()).await.err();

        assert_eq!(refused, Some(SnapshotError::UnhealthyStorage));
        assert!(later.exists());
    }
}
