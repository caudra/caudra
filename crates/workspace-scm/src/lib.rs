#![forbid(unsafe_code)]

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{File, Metadata},
    io::Read,
    mem::size_of,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use gix::objs::FindExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::{
    fs,
    io::AsyncReadExt,
    process::Command,
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workcell_host_contract::{
    ContractVersion, Cursor, MAX_SCM_COMMIT_BYTES, MAX_SCM_CONFIG_BYTES, MAX_SCM_DIFF_BYTES,
    MAX_SCM_DIFF_LINES, MAX_SCM_DIFF_PARSED_LINES, MAX_SCM_DIFF_SCAN_BYTES, MAX_SCM_LOG_COMMITS,
    MAX_SCM_LOG_ENTRIES, MAX_SCM_LOG_SCAN_BYTES, MAX_SCM_PATHS, MAX_SCM_SHALLOW_BYTES,
    MAX_SCM_SHALLOW_COMMITS, MAX_SCM_SIDE_BYTES, MAX_SCM_SIDE_LINES, MAX_SCM_STATUS_ENTRIES,
    ResourceId, Revision, ScmChangeKind, ScmCommit, ScmDiffLine, ScmDiffLineKind, ScmDiffRequest,
    ScmDiffResponse, ScmDiffTarget, ScmDiscoverRequest, ScmDiscoverResponse, ScmLogRequest,
    ScmLogResponse, ScmMutation, ScmMutationPreview, ScmMutationResponse, ScmReadSideRequest,
    ScmReadSideResponse, ScmRepository, ScmRepositoryRevisions, ScmSide, ScmStatusEntry,
    ScmStatusRequest, ScmStatusResponse, ScmText, WorkspacePath, WorkspaceRequestBinding,
};
use workcell_mcp_files::{FileToolGroup, WorkspaceError, WorkspaceRepositoryResource};

const MAX_REPOSITORIES: usize = 64;
const MAX_CURSORS: usize = 256;
const MAX_STATUS_OUTPUT_BYTES: usize = 8 * 1_024 * 1_024;
const MAX_INDEX_BYTES: usize = 64 * 1_024 * 1_024;
const MAX_WORKTREE_REVISION_BYTES: usize = 64 * 1_024 * 1_024;
const MAX_DIFF_SOURCE_BYTES: usize = 8 * 1_024 * 1_024;
const MAX_COMMIT_PARENTS: usize = 64;
const MAX_COMMIT_SUMMARY_BYTES: usize = 4_096;
/// A commit body is prose a person wrote, so it is allowed more room than the
/// subject line while still being bounded: a log page carries many of them.
const MAX_COMMIT_BODY_BYTES: usize = 16_384;
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_GIT_VERSION_BYTES: usize = 128;
const CONFIG_FILE_NAME: &str = "config";
const SHALLOW_FILE_NAME: &str = "shallow";
pub const MAX_CONCURRENT_SCM_OPERATIONS: usize = 4;

fn concurrency() -> &'static Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_SCM_OPERATIONS)))
}

async fn acquire(token: &CancellationToken) -> Result<OwnedSemaphorePermit, ScmError> {
    tokio::select! {
        biased;
        () = token.cancelled() => Err(ScmError::Cancelled),
        permit = concurrency().clone().acquire_owned() => {
            permit.map_err(|_| ScmError::OperationFailed)
        }
    }
}

struct ScmAdmission {
    _permit: OwnedSemaphorePermit,
    #[cfg(test)]
    _observation: Option<AdmissionObservation>,
}

#[cfg(test)]
struct AdmissionObserver {
    active: std::sync::atomic::AtomicUsize,
    maximum: std::sync::atomic::AtomicUsize,
    barrier: tokio::sync::Barrier,
}

#[cfg(test)]
struct AdmissionObservation {
    observer: Arc<AdmissionObserver>,
}

#[cfg(test)]
impl AdmissionObserver {
    fn new(parties: usize) -> Arc<Self> {
        Arc::new(Self {
            active: std::sync::atomic::AtomicUsize::new(0),
            maximum: std::sync::atomic::AtomicUsize::new(0),
            barrier: tokio::sync::Barrier::new(parties),
        })
    }

    async fn enter(self: &Arc<Self>) -> AdmissionObservation {
        let active = self
            .active
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .saturating_add(1);
        self.maximum
            .fetch_max(active, std::sync::atomic::Ordering::SeqCst);
        self.barrier.wait().await;
        AdmissionObservation {
            observer: self.clone(),
        }
    }
}

#[cfg(test)]
impl Drop for AdmissionObservation {
    fn drop(&mut self) {
        self.observer
            .active
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
fn concurrency_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[cfg(unix)]
type ConfigFileIdentity = (u64, u64, u64, i64, i64, i64, i64);

#[cfg(not(unix))]
type ConfigFileIdentity = (
    u64,
    Option<std::time::SystemTime>,
    Option<std::time::SystemTime>,
);

#[derive(Clone, Debug, Eq, PartialEq)]
enum RepositoryConfigSnapshot {
    Missing,
    Present {
        identity: ConfigFileIdentity,
        digest: [u8; 32],
    },
}

#[derive(Clone)]
pub struct ScmGroup {
    files: FileToolGroup,
    git_executable: Arc<PathBuf>,
    state: Arc<Mutex<ScmState>>,
    #[cfg(test)]
    admission_observer: Option<Arc<AdmissionObserver>>,
}

#[derive(Default)]
struct ScmState {
    repositories: HashMap<ResourceId, RepositoryBinding>,
    repository_order: VecDeque<ResourceId>,
    cursors: HashMap<Cursor, CursorBinding>,
    cursor_order: VecDeque<Cursor>,
}

#[derive(Clone, Debug)]
struct RepositoryBinding {
    resource: WorkspaceRepositoryResource,
    identity: Revision,
    binding: WorkspaceRequestBinding,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CursorKind {
    Status,
    Log,
    Diff,
}

struct CursorBinding {
    kind: CursorKind,
    binding: WorkspaceRequestBinding,
    request: Revision,
    result: Revision,
    offset: usize,
}

#[derive(Debug)]
pub struct PreparedScmMutation {
    repository: RepositoryBinding,
    mutation: ScmMutation,
    preview: ScmMutationPreview,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScmMutationFailure {
    error: ScmError,
    side_effects_possible: bool,
}

impl ScmMutationFailure {
    #[must_use]
    pub const fn error(self) -> ScmError {
        self.error
    }

    #[must_use]
    pub const fn side_effects_possible(self) -> bool {
        self.side_effects_possible
    }

    const fn clean(error: ScmError) -> Self {
        Self {
            error,
            side_effects_possible: false,
        }
    }

    const fn uncertain(error: ScmError) -> Self {
        Self {
            error,
            side_effects_possible: true,
        }
    }
}

impl PreparedScmMutation {
    /// Conservative retained bytes, excluding filesystem state shared by the repository binding.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.repository.retained_bytes())
            .saturating_add(scm_mutation_bytes(&self.mutation))
            .saturating_add(scm_preview_bytes(&self.preview))
    }

    #[must_use]
    pub fn repository_resource_id(&self) -> &ResourceId {
        self.repository.resource.resource_id()
    }

    pub fn path_resource_id(&self, path: &WorkspacePath) -> Result<ResourceId, ScmError> {
        self.repository
            .resource
            .path_resource_id(path)
            .map_err(map_workspace_error)
    }
    pub fn repository_resource_scope(&self) -> Result<Vec<ResourceId>, ScmError> {
        self.repository
            .resource
            .resource_scope()
            .map_err(map_workspace_error)
    }

    pub fn path_resource_scope(&self, path: &WorkspacePath) -> Result<Vec<ResourceId>, ScmError> {
        self.repository
            .resource
            .path_resource_scope(path)
            .map_err(map_workspace_error)
    }
}

impl RepositoryBinding {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.resource.retained_bytes())
            .saturating_add(self.identity.as_str().len().saturating_mul(2))
            .saturating_add(workspace_binding_retained_bytes(&self.binding))
    }
}

fn scm_mutation_bytes(mutation: &ScmMutation) -> usize {
    mutation.retained_bytes()
}

fn scm_preview_bytes(preview: &ScmMutationPreview) -> usize {
    size_of::<ScmMutationPreview>()
        .saturating_add(scm_mutation_bytes(&preview.mutation))
        .saturating_add(preview.repository_identity.as_str().len().saturating_mul(2))
        .saturating_add(
            [
                &preview.revisions.repository,
                &preview.revisions.head,
                &preview.revisions.index,
                &preview.revisions.worktree,
            ]
            .into_iter()
            .map(|revision| revision.retained_bytes())
            .fold(0, usize::saturating_add),
        )
        .saturating_add(
            preview
                .entries
                .capacity()
                .saturating_mul(size_of::<ScmStatusEntry>()),
        )
        .saturating_add(
            preview
                .entries
                .iter()
                .map(|entry| entry.path.retained_bytes())
                .fold(0, usize::saturating_add),
        )
}

fn workspace_binding_retained_bytes(binding: &WorkspaceRequestBinding) -> usize {
    size_of::<WorkspaceRequestBinding>()
        .saturating_add(binding.host.server_id.retained_bytes())
        .saturating_add(binding.host.instance_id.retained_bytes())
        .saturating_add(binding.host.workspace_id.retained_bytes())
        .saturating_add(binding.host.workspace_generation.retained_bytes())
        .saturating_add(binding.host.root_project_id.retained_bytes())
        .saturating_add(binding.host.principal_id.retained_bytes())
        .saturating_add(binding.host.cwd_handle.retained_bytes())
        .saturating_add(binding.host.catalog_revision.retained_bytes())
        .saturating_add(binding.host.policy_revision.retained_bytes())
        .saturating_add(binding.cwd_handle.retained_bytes())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ScmError {
    #[error("SCM request is invalid")]
    InvalidRequest,
    #[error("SCM repository handle is unknown or stale")]
    StaleRepository,
    #[error("SCM cursor is invalid")]
    InvalidCursor,
    #[error("SCM cursor is stale")]
    StaleCursor,
    #[error("SCM prepared operation is stale")]
    StalePreparedOperation,
    #[error("SCM repository is unavailable")]
    RepositoryUnavailable,
    #[error("Not a Git repository")]
    NotRepository,
    #[error("SCM repository layout is unsupported")]
    UnsupportedRepository,
    #[error("SCM data exceeds a configured limit")]
    LimitExceeded,
    #[error("SCM data is not valid UTF-8")]
    UnsupportedEncoding,
    #[error("SCM index is locked by another operation")]
    Locked,
    #[error("SCM operation was cancelled")]
    Cancelled,
    #[error("SCM operation timed out")]
    TimedOut,
    #[error("SCM operation failed")]
    OperationFailed,
}

impl ScmError {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::StaleRepository => "stale_repository",
            Self::InvalidCursor => "invalid_cursor",
            Self::StaleCursor => "stale_cursor",
            Self::StalePreparedOperation => "stale_prepared_operation",
            Self::RepositoryUnavailable => "repository_unavailable",
            Self::NotRepository => "not_repository",
            Self::UnsupportedRepository => "unsupported_repository",
            Self::LimitExceeded => "limit_exceeded",
            Self::UnsupportedEncoding => "unsupported_encoding",
            Self::Locked => "repository_locked",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::OperationFailed => "operation_failed",
        }
    }
}

impl ScmGroup {
    #[must_use]
    pub fn new(files: FileToolGroup) -> Self {
        Self::with_executable(files, PathBuf::from("git"))
    }

    pub async fn probe(files: FileToolGroup, executable: PathBuf) -> Option<Self> {
        let _permit = concurrency().clone().acquire_owned().await.ok()?;
        git_available(&executable)
            .await
            .then(|| Self::with_executable(files, executable))
    }

    fn with_executable(files: FileToolGroup, git_executable: PathBuf) -> Self {
        Self {
            files,
            git_executable: Arc::new(git_executable),
            state: Arc::new(Mutex::new(ScmState::default())),
            #[cfg(test)]
            admission_observer: None,
        }
    }

    async fn admit(&self, token: &CancellationToken) -> Result<ScmAdmission, ScmError> {
        let permit = acquire(token).await?;
        Ok(ScmAdmission {
            _permit: permit,
            #[cfg(test)]
            _observation: match &self.admission_observer {
                Some(observer) => Some(observer.enter().await),
                None => None,
            },
        })
    }

    pub async fn discover(
        &self,
        request: &ScmDiscoverRequest,
        token: &CancellationToken,
    ) -> Result<ScmDiscoverResponse, ScmError> {
        check_cancelled(token)?;
        let _admission = self.admit(token).await?;
        let resource = self
            .files
            .workspace_discover_repository(&request.binding.cwd_handle, &request.path)
            .await
            .map_err(map_workspace_error)?;
        verify_repository(&resource)?;
        let identity = repository_identity(&resource)?;
        let revisions = repository_revisions(&self.git_executable, &resource, token).await?;
        let handle = ResourceId::new(format!("scm_{}", Uuid::new_v4()))
            .map_err(|_| ScmError::OperationFailed)?;
        let repository = ScmRepository {
            handle: handle.clone(),
            resource_id: resource.resource_id().clone(),
            root: WorkspacePath::new(resource.relative_path())
                .map_err(|_| ScmError::OperationFailed)?,
            identity: identity.clone(),
            revisions,
        };
        let mut state = lock(&self.state);
        state.repositories.insert(
            handle.clone(),
            RepositoryBinding {
                resource,
                identity,
                binding: request.binding.clone(),
            },
        );
        state.repository_order.push_back(handle);
        while state.repository_order.len() > MAX_REPOSITORIES {
            if let Some(expired) = state.repository_order.pop_front() {
                state.repositories.remove(&expired);
            }
        }
        Ok(ScmDiscoverResponse {
            version: ContractVersion::V1,
            repository,
        })
    }

    pub async fn status(
        &self,
        request: &ScmStatusRequest,
        token: &CancellationToken,
    ) -> Result<ScmStatusResponse, ScmError> {
        validate_page_size(request.page_size, MAX_SCM_STATUS_ENTRIES)?;
        let _admission = self.admit(token).await?;
        let repository = self
            .repository(&request.repository_handle, &request.binding)
            .await?;
        let snapshot = status_snapshot(&self.git_executable, &repository.resource, token).await?;
        let revision = snapshot.revisions.repository.clone();
        let request_revision = digest(&(&request.repository_handle, "status"))?;
        let offset = self.cursor_offset(
            request.cursor.as_ref(),
            CursorKind::Status,
            &request.binding,
            &request_revision,
            &revision,
        )?;
        let page_size = request.page_size as usize;
        let end = offset.saturating_add(page_size).min(snapshot.entries.len());
        let next_cursor = (end < snapshot.entries.len())
            .then(|| {
                self.insert_cursor(
                    CursorKind::Status,
                    request.binding.clone(),
                    request_revision,
                    revision.clone(),
                    end,
                )
            })
            .transpose()?;
        Ok(ScmStatusResponse {
            version: ContractVersion::V1,
            revisions: snapshot.revisions,
            revision,
            entries: snapshot
                .entries
                .into_iter()
                .skip(offset)
                .take(page_size)
                .collect(),
            next_cursor,
        })
    }

    pub async fn log(
        &self,
        request: &ScmLogRequest,
        token: &CancellationToken,
    ) -> Result<ScmLogResponse, ScmError> {
        validate_page_size(request.page_size, MAX_SCM_LOG_ENTRIES)?;
        let _admission = self.admit(token).await?;
        let repository = self
            .repository(&request.repository_handle, &request.binding)
            .await?;
        check_cancelled(token)?;
        let repo = verify_repository(&repository.resource)?;
        let head_id = head_object_id(&repo)?;
        let head = revision_from_head(head_id.as_ref())?;
        let request_revision = digest(&(&request.repository_handle, "log"))?;
        let offset = self.cursor_offset(
            request.cursor.as_ref(),
            CursorKind::Log,
            &request.binding,
            &request_revision,
            &head,
        )?;
        let page_size = request.page_size as usize;
        if offset.saturating_add(page_size)
            > usize::try_from(MAX_SCM_LOG_COMMITS).unwrap_or(usize::MAX)
        {
            return Err(ScmError::LimitExceeded);
        }
        let page = match head_id {
            Some(head_id) => {
                let shallow_commits =
                    read_shallow_commits(&repo, repository.resource.git_dir(), token)?;
                collect_log_page(&repo, head_id, &shallow_commits, offset, page_size, token)?
            }
            None => LogPage::default(),
        };
        let has_more = page.has_more;
        let scan_limit_reached = page.scan_limit_reached;
        let page = page.commits;
        let next_offset = offset.saturating_add(page.len());
        let next_cursor = (has_more
            && !scan_limit_reached
            && next_offset < usize::try_from(MAX_SCM_LOG_COMMITS).unwrap_or(usize::MAX))
        .then(|| {
            self.insert_cursor(
                CursorKind::Log,
                request.binding.clone(),
                request_revision,
                head.clone(),
                next_offset,
            )
        })
        .transpose()?;
        Ok(ScmLogResponse {
            version: ContractVersion::V1,
            head_revision: head.clone(),
            revision: head,
            commits: page,
            truncated: has_more || scan_limit_reached,
            next_cursor,
        })
    }

    pub async fn diff(
        &self,
        request: &ScmDiffRequest,
        token: &CancellationToken,
    ) -> Result<ScmDiffResponse, ScmError> {
        validate_read_limits(
            request.max_lines,
            request.max_bytes,
            MAX_SCM_DIFF_LINES,
            MAX_SCM_DIFF_BYTES,
        )?;
        validate_diff_target(&request.target)?;
        let _admission = self.admit(token).await?;
        let repository = self
            .repository(&request.repository_handle, &request.binding)
            .await?;
        let revisions =
            repository_revisions(&self.git_executable, &repository.resource, token).await?;
        let repository_revision = revisions.repository.clone();
        let path = match &request.path {
            Some(path) => Some(
                repository
                    .resource
                    .resolve_path(path)
                    .await
                    .map_err(map_workspace_error)?
                    .relative_to_repository()
                    .clone(),
            ),
            None => None,
        };
        let collected = collect_diff(
            &self.git_executable,
            &repository.resource,
            &request.target,
            path.as_ref(),
            token,
        )
        .await?;
        let revision = digest(&(&collected.lines, collected.truncated))?;
        let request_revision = digest(&(
            &request.repository_handle,
            &request.target,
            &path,
            request.max_lines,
            request.max_bytes,
        ))?;
        let offset = self.cursor_offset(
            request.cursor.as_ref(),
            CursorKind::Diff,
            &request.binding,
            &request_revision,
            &revision,
        )?;
        let mut bytes = 0usize;
        let mut page = Vec::new();
        for line in collected.lines.iter().skip(offset) {
            if page.len() >= request.max_lines as usize {
                break;
            }
            let line_bytes = serde_json::to_vec(line)
                .map_err(|_| ScmError::OperationFailed)?
                .len();
            if bytes.saturating_add(line_bytes) > request.max_bytes as usize {
                if page.is_empty() {
                    return Err(ScmError::LimitExceeded);
                }
                break;
            }
            bytes = bytes.saturating_add(line_bytes);
            page.push(line.clone());
        }
        let end = offset.saturating_add(page.len());
        let retained_more = end < collected.lines.len();
        let truncated = retained_more || collected.truncated;
        let next_cursor = retained_more
            .then(|| {
                self.insert_cursor(
                    CursorKind::Diff,
                    request.binding.clone(),
                    request_revision,
                    revision.clone(),
                    end,
                )
            })
            .transpose()?;
        Ok(ScmDiffResponse {
            version: ContractVersion::V1,
            repository_revision,
            revision,
            lines: page,
            truncated,
            next_cursor,
        })
    }

    pub async fn read_side(
        &self,
        request: &ScmReadSideRequest,
        token: &CancellationToken,
    ) -> Result<ScmReadSideResponse, ScmError> {
        validate_read_limits(
            request.max_lines,
            request.max_bytes,
            MAX_SCM_SIDE_LINES,
            MAX_SCM_SIDE_BYTES,
        )?;
        if request.start_line == 0 {
            return Err(ScmError::InvalidRequest);
        }
        if let ScmSide::Commit { revision } = &request.side {
            validate_object_id(revision)?;
        }
        let _admission = self.admit(token).await?;
        let repository = self
            .repository(&request.repository_handle, &request.binding)
            .await?;
        let resolved = repository
            .resource
            .resolve_path(&request.path)
            .await
            .map_err(map_workspace_error)?;
        let path = resolved.relative_to_repository().clone();
        let bytes = match &request.side {
            ScmSide::Worktree => {
                read_file_bounded(resolved.path(), MAX_DIFF_SOURCE_BYTES, token).await?
            }
            ScmSide::Head => {
                git_blob(
                    &self.git_executable,
                    &repository.resource,
                    &format!("HEAD:{}", path.as_str()),
                    token,
                )
                .await?
            }
            ScmSide::Index => {
                git_blob(
                    &self.git_executable,
                    &repository.resource,
                    &format!(":{}", path.as_str()),
                    token,
                )
                .await?
            }
            ScmSide::Commit { revision } => {
                git_blob(
                    &self.git_executable,
                    &repository.resource,
                    &format!("{}:{}", revision.as_str(), path.as_str()),
                    token,
                )
                .await?
            }
        };
        let text = String::from_utf8(bytes.clone()).map_err(|_| ScmError::UnsupportedEncoding)?;
        let all_lines = split_lines(&text);
        let start = request.start_line as usize - 1;
        let mut content = String::new();
        let mut consumed = 0usize;
        for line in all_lines
            .iter()
            .skip(start)
            .take(request.max_lines as usize)
        {
            let separator = usize::from(consumed > 0);
            if content
                .len()
                .saturating_add(separator)
                .saturating_add(line.len())
                > request.max_bytes as usize
            {
                break;
            }
            if separator == 1 {
                content.push('\n');
            }
            content.push_str(line);
            consumed = consumed.saturating_add(1);
        }
        if consumed == 0 && start < all_lines.len() && !all_lines[start].is_empty() {
            return Err(ScmError::LimitExceeded);
        }
        let end = start.saturating_add(consumed);
        let truncated = end < all_lines.len();
        Ok(ScmReadSideResponse {
            version: ContractVersion::V1,
            repository_revision: repository_revisions(
                &self.git_executable,
                &repository.resource,
                token,
            )
            .await?
            .repository,
            resource_id: repository
                .resource
                .path_resource_id(&path)
                .map_err(map_workspace_error)?,
            revision: digest_bytes(&bytes)?,
            path,
            side: request.side.clone(),
            content: ScmText::new(content).map_err(|_| ScmError::OperationFailed)?,
            start_line: request.start_line,
            end_line: u32::try_from(end).unwrap_or(u32::MAX),
            total_lines: u32::try_from(all_lines.len()).unwrap_or(u32::MAX),
            truncated,
            next_start_line: truncated.then(|| u32::try_from(end + 1).unwrap_or(u32::MAX)),
        })
    }

    pub async fn prepare_mutation(
        &self,
        repository_handle: &ResourceId,
        binding: &WorkspaceRequestBinding,
        mutation: ScmMutation,
        token: &CancellationToken,
    ) -> Result<PreparedScmMutation, ScmError> {
        self.prepare_mutation_bounded(repository_handle, binding, mutation, usize::MAX, token)
            .await
    }

    pub async fn prepare_mutation_bounded(
        &self,
        repository_handle: &ResourceId,
        binding: &WorkspaceRequestBinding,
        mutation: ScmMutation,
        maximum_retained_bytes: usize,
        token: &CancellationToken,
    ) -> Result<PreparedScmMutation, ScmError> {
        let _admission = self.admit(token).await?;
        let repository = self.repository(repository_handle, binding).await?;
        if !repository.resource.allow_write() {
            return Err(ScmError::InvalidRequest);
        }
        let paths = validate_mutation_paths(&repository.resource, &mutation).await?;
        let mutation = match mutation {
            ScmMutation::Stage { .. } => ScmMutation::Stage {
                paths: paths.clone(),
            },
            ScmMutation::Unstage { .. } => ScmMutation::Unstage {
                paths: paths.clone(),
            },
            ScmMutation::Discard { .. } => ScmMutation::Discard {
                paths: paths.clone(),
            },
        };
        let snapshot = status_snapshot(&self.git_executable, &repository.resource, token).await?;
        let entries = mutation_entries(&snapshot.entries, &paths, &mutation)?;
        let preview = ScmMutationPreview {
            mutation: mutation.clone(),
            repository_identity: repository.identity.clone(),
            revisions: snapshot.revisions,
            entries,
        };
        let prepared = PreparedScmMutation {
            repository,
            mutation,
            preview,
        };
        if prepared.retained_bytes() > maximum_retained_bytes {
            return Err(ScmError::LimitExceeded);
        }
        Ok(prepared)
    }

    pub async fn execute_mutation(
        &self,
        prepared: PreparedScmMutation,
        token: &CancellationToken,
    ) -> Result<ScmMutationResponse, ScmError> {
        self.execute_mutation_tracked(prepared, token)
            .await
            .map_err(ScmMutationFailure::error)
    }

    pub async fn execute_mutation_tracked(
        &self,
        prepared: PreparedScmMutation,
        token: &CancellationToken,
    ) -> Result<ScmMutationResponse, ScmMutationFailure> {
        check_cancelled(token).map_err(ScmMutationFailure::clean)?;
        let _admission = self.admit(token).await.map_err(ScmMutationFailure::clean)?;
        let _guard = prepared
            .repository
            .resource
            .mutation_guard()
            .await
            .map_err(map_workspace_error)
            .map_err(ScmMutationFailure::clean)?;
        prepared
            .repository
            .resource
            .revalidate()
            .await
            .map_err(|_| ScmMutationFailure::clean(ScmError::StalePreparedOperation))?;
        if repository_identity(&prepared.repository.resource).map_err(ScmMutationFailure::clean)?
            != prepared.repository.identity
        {
            return Err(ScmMutationFailure::clean(ScmError::StalePreparedOperation));
        }
        let current =
            repository_revisions(&self.git_executable, &prepared.repository.resource, token)
                .await
                .map_err(ScmMutationFailure::clean)?;
        if current != prepared.preview.revisions {
            return Err(ScmMutationFailure::clean(ScmError::StalePreparedOperation));
        }
        if fs::try_exists(prepared.repository.resource.git_dir().join("index.lock"))
            .await
            .unwrap_or(true)
        {
            return Err(ScmMutationFailure::clean(ScmError::Locked));
        }
        let paths = prepared
            .mutation
            .paths()
            .iter()
            .map(|path| path.as_str())
            .collect::<Vec<_>>();
        let mut arguments = match &prepared.mutation {
            ScmMutation::Stage { .. } => vec!["add"],
            ScmMutation::Unstage { .. } => vec!["restore", "--staged"],
            ScmMutation::Discard { .. } => vec!["restore", "--worktree"],
        };
        arguments.push("--");
        arguments.extend(paths);
        git_mutation(
            &self.git_executable,
            &prepared.repository.resource,
            &arguments,
            token,
        )
        .await
        .map_err(ScmMutationFailure::uncertain)?;
        let revisions =
            repository_revisions(&self.git_executable, &prepared.repository.resource, token)
                .await
                .map_err(ScmMutationFailure::uncertain)?;
        Ok(ScmMutationResponse {
            version: ContractVersion::V1,
            mutation: prepared.mutation,
            revisions,
        })
    }

    #[must_use]
    pub const fn preview(prepared: &PreparedScmMutation) -> &ScmMutationPreview {
        &prepared.preview
    }

    async fn repository(
        &self,
        handle: &ResourceId,
        binding: &WorkspaceRequestBinding,
    ) -> Result<RepositoryBinding, ScmError> {
        let repository = lock(&self.state)
            .repositories
            .get(handle)
            .cloned()
            .ok_or(ScmError::StaleRepository)?;
        if !same_workspace_binding(&repository.binding, binding) {
            return Err(ScmError::StaleRepository);
        }
        repository
            .resource
            .revalidate()
            .await
            .map_err(|_| ScmError::StaleRepository)?;
        if repository_identity(&repository.resource)? != repository.identity {
            return Err(ScmError::StaleRepository);
        }
        verify_repository(&repository.resource)?;
        Ok(repository)
    }

    fn cursor_offset(
        &self,
        cursor: Option<&Cursor>,
        kind: CursorKind,
        workspace_binding: &WorkspaceRequestBinding,
        request: &Revision,
        result: &Revision,
    ) -> Result<usize, ScmError> {
        let Some(cursor) = cursor else {
            return Ok(0);
        };
        let state = lock(&self.state);
        let binding = state.cursors.get(cursor).ok_or(ScmError::InvalidCursor)?;
        if binding.kind != kind
            || !same_workspace_binding(&binding.binding, workspace_binding)
            || &binding.request != request
        {
            return Err(ScmError::InvalidCursor);
        }
        if &binding.result != result {
            return Err(ScmError::StaleCursor);
        }
        Ok(binding.offset)
    }

    fn insert_cursor(
        &self,
        kind: CursorKind,
        binding: WorkspaceRequestBinding,
        request: Revision,
        result: Revision,
        offset: usize,
    ) -> Result<Cursor, ScmError> {
        let cursor = Cursor::new(format!("scm_cursor_{}", Uuid::new_v4()))
            .map_err(|_| ScmError::OperationFailed)?;
        let mut state = lock(&self.state);
        state.cursors.insert(
            cursor.clone(),
            CursorBinding {
                kind,
                binding,
                request,
                result,
                offset,
            },
        );
        state.cursor_order.push_back(cursor.clone());
        while state.cursor_order.len() > MAX_CURSORS {
            if let Some(expired) = state.cursor_order.pop_front() {
                state.cursors.remove(&expired);
            }
        }
        Ok(cursor)
    }
}

fn same_workspace_binding(left: &WorkspaceRequestBinding, right: &WorkspaceRequestBinding) -> bool {
    left.cwd_handle == right.cwd_handle
        && left.host.server_id == right.host.server_id
        && left.host.workspace_id == right.host.workspace_id
        && left.host.workspace_generation == right.host.workspace_generation
        && left.host.root_project_id == right.host.root_project_id
        && left.host.principal_id == right.host.principal_id
        && left.host.cwd_handle == right.host.cwd_handle
        && left.host.catalog_revision == right.host.catalog_revision
        && left.host.policy_revision == right.host.policy_revision
}

struct StatusSnapshot {
    revisions: ScmRepositoryRevisions,
    entries: Vec<ScmStatusEntry>,
}

#[derive(Debug, Default)]
struct LogPage {
    commits: Vec<ScmCommit>,
    has_more: bool,
    scan_limit_reached: bool,
}

enum CommitDecode {
    Commit {
        commit: Option<ScmCommit>,
        parents: Vec<gix::ObjectId>,
    },
    ScanLimitReached,
}

trait CommitObjectStore {
    fn object_header(&self, id: &gix::ObjectId) -> Result<(gix::objs::Kind, u64), ScmError>;

    fn load_object<'a>(
        &self,
        id: &gix::ObjectId,
        buffer: &'a mut Vec<u8>,
    ) -> Result<gix::objs::Data<'a>, ScmError>;
}

impl CommitObjectStore for gix::Repository {
    fn object_header(&self, id: &gix::ObjectId) -> Result<(gix::objs::Kind, u64), ScmError> {
        let header = self
            .find_header(id.to_owned())
            .map_err(|_| ScmError::RepositoryUnavailable)?;
        Ok((header.kind(), header.size()))
    }

    fn load_object<'a>(
        &self,
        id: &gix::ObjectId,
        buffer: &'a mut Vec<u8>,
    ) -> Result<gix::objs::Data<'a>, ScmError> {
        self.objects
            .find(id, buffer)
            .map_err(|_| ScmError::RepositoryUnavailable)
    }
}

fn collect_log_page(
    objects: &impl CommitObjectStore,
    head: gix::ObjectId,
    shallow_commits: &HashSet<gix::ObjectId>,
    offset: usize,
    page_size: usize,
    token: &CancellationToken,
) -> Result<LogPage, ScmError> {
    let traversal_limit = usize::try_from(MAX_SCM_LOG_COMMITS).unwrap_or(usize::MAX);
    let traversal_target = offset
        .saturating_add(page_size)
        .saturating_add(1)
        .min(traversal_limit);
    let mut pending = VecDeque::from([head]);
    let mut seen = HashSet::from([head]);
    let mut buffer = Vec::new();
    let mut commits = Vec::with_capacity(page_size.saturating_add(1));
    let mut traversed = 0usize;
    let mut remaining_scan_bytes = MAX_SCM_LOG_SCAN_BYTES;
    let mut scan_limit_reached = false;

    while traversed < traversal_target {
        check_cancelled(token)?;
        let Some(id) = pending.pop_front() else {
            break;
        };
        let retain = traversed >= offset;
        let CommitDecode::Commit { commit, parents } =
            decode_bounded_commit(objects, id, &mut buffer, retain, &mut remaining_scan_bytes)?
        else {
            scan_limit_reached = true;
            break;
        };
        traversed = traversed.saturating_add(1);
        if let Some(commit) = commit {
            commits.push(commit);
        }
        if !shallow_commits.contains(&id) {
            for parent in parents {
                if seen.insert(parent) {
                    pending.push_back(parent);
                }
            }
        }
    }

    let has_more =
        commits.len() > page_size || (traversed == traversal_limit && !pending.is_empty());
    commits.truncate(page_size);
    Ok(LogPage {
        commits,
        has_more,
        scan_limit_reached,
    })
}

fn decode_bounded_commit(
    objects: &impl CommitObjectStore,
    id: gix::ObjectId,
    buffer: &mut Vec<u8>,
    retain: bool,
    remaining_scan_bytes: &mut u64,
) -> Result<CommitDecode, ScmError> {
    let (kind, size) = objects.object_header(&id)?;
    if kind != gix::objs::Kind::Commit {
        return Err(ScmError::RepositoryUnavailable);
    }
    if size > MAX_SCM_COMMIT_BYTES {
        return Err(ScmError::LimitExceeded);
    }
    let Some(remaining) = remaining_scan_bytes.checked_sub(size) else {
        return Ok(CommitDecode::ScanLimitReached);
    };
    *remaining_scan_bytes = remaining;
    let size = usize::try_from(size).map_err(|_| ScmError::LimitExceeded)?;
    buffer.clear();
    buffer
        .try_reserve_exact(size)
        .map_err(|_| ScmError::OperationFailed)?;
    let data = objects.load_object(&id, buffer)?;
    if data.kind != gix::objs::Kind::Commit {
        return Err(ScmError::RepositoryUnavailable);
    }
    if data.data.len() > usize::try_from(MAX_SCM_COMMIT_BYTES).unwrap_or(usize::MAX) {
        return Err(ScmError::LimitExceeded);
    }
    if data.data.len() != size {
        return Err(ScmError::RepositoryUnavailable);
    }
    let decoded = match data.decode().map_err(|_| ScmError::RepositoryUnavailable)? {
        gix::objs::ObjectRef::Commit(commit) => commit,
        _ => return Err(ScmError::RepositoryUnavailable),
    };
    if decoded.parents.len() > MAX_COMMIT_PARENTS {
        return Err(ScmError::LimitExceeded);
    }
    let parents = decoded.parents().collect();
    let commit = retain.then(|| commit_dto(id, &decoded)).transpose()?;
    Ok(CommitDecode::Commit { commit, parents })
}

async fn status_snapshot(
    git_executable: &Path,
    resource: &WorkspaceRepositoryResource,
    token: &CancellationToken,
) -> Result<StatusSnapshot, ScmError> {
    let output = git_output(
        git_executable,
        resource,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
        MAX_STATUS_OUTPUT_BYTES,
        token,
    )
    .await?;
    let mut entries = parse_status(&output)?;
    if entries.len()
        > usize::try_from(workcell_host_contract::MAX_SCM_STATUS_PATHS).unwrap_or(usize::MAX)
    {
        return Err(ScmError::LimitExceeded);
    }
    entries.sort_by(|left, right| left.path.as_str().cmp(right.path.as_str()));
    let mut worktree_digest = Sha256::new();
    worktree_digest.update(serde_json::to_vec(&entries).map_err(|_| ScmError::OperationFailed)?);
    let mut worktree_bytes = 0usize;
    for entry in &entries {
        let resolved = resource
            .resolve_path(&entry.path)
            .await
            .map_err(map_workspace_error)?;
        match fs::symlink_metadata(resolved.path()).await {
            Ok(metadata) if metadata.is_file() => {
                let bytes =
                    read_file_bounded(resolved.path(), MAX_DIFF_SOURCE_BYTES, token).await?;
                worktree_bytes = worktree_bytes.saturating_add(bytes.len());
                if worktree_bytes > MAX_WORKTREE_REVISION_BYTES {
                    return Err(ScmError::LimitExceeded);
                }
                worktree_digest.update(entry.path.as_str().as_bytes());
                worktree_digest.update(bytes.len().to_be_bytes());
                worktree_digest.update(bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                worktree_digest.update(entry.path.as_str().as_bytes());
                worktree_digest.update(b"missing");
            }
            Ok(_) => return Err(ScmError::UnsupportedRepository),
            Err(_) => return Err(ScmError::OperationFailed),
        }
    }
    let head = head_revision(&verify_repository(resource)?)?;
    let index = index_revision(resource, token).await?;
    let worktree = Revision::new(prefixed_digest(worktree_digest.finalize().as_slice()))
        .map_err(|_| ScmError::OperationFailed)?;
    let repository = digest(&(resource.resource_id(), &head, &index, &worktree))?;
    Ok(StatusSnapshot {
        revisions: ScmRepositoryRevisions {
            repository,
            head,
            index,
            worktree,
        },
        entries,
    })
}

async fn repository_revisions(
    git_executable: &Path,
    resource: &WorkspaceRepositoryResource,
    token: &CancellationToken,
) -> Result<ScmRepositoryRevisions, ScmError> {
    Ok(status_snapshot(git_executable, resource, token)
        .await?
        .revisions)
}

fn parse_status(output: &[u8]) -> Result<Vec<ScmStatusEntry>, ScmError> {
    let mut entries = Vec::new();
    for record in output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let text = std::str::from_utf8(record).map_err(|_| ScmError::UnsupportedEncoding)?;
        let entry = if let Some(path) = text.strip_prefix("? ") {
            ScmStatusEntry {
                path: scm_path(path)?,
                staged: None,
                unstaged: None,
                untracked: true,
                conflicted: false,
            }
        } else if text.starts_with("1 ") {
            let mut fields = text.splitn(9, ' ');
            if fields.next() != Some("1") {
                return Err(ScmError::OperationFailed);
            }
            let xy = fields.next().ok_or(ScmError::OperationFailed)?;
            for _ in 0..6 {
                fields.next().ok_or(ScmError::OperationFailed)?;
            }
            let path = fields.next().ok_or(ScmError::OperationFailed)?;
            parse_tracked_status(path, xy)?
        } else if text.starts_with("u ") {
            let mut fields = text.splitn(11, ' ');
            if fields.next() != Some("u") {
                return Err(ScmError::OperationFailed);
            }
            let xy = fields.next().ok_or(ScmError::OperationFailed)?;
            for _ in 0..8 {
                fields.next().ok_or(ScmError::OperationFailed)?;
            }
            let path = fields.next().ok_or(ScmError::OperationFailed)?;
            let mut entry = parse_tracked_status(path, xy)?;
            entry.conflicted = true;
            entry.staged = Some(ScmChangeKind::Unmerged);
            entry.unstaged = Some(ScmChangeKind::Unmerged);
            entry
        } else {
            return Err(ScmError::OperationFailed);
        };
        entries.push(entry);
    }
    Ok(entries)
}

fn parse_tracked_status(path: &str, xy: &str) -> Result<ScmStatusEntry, ScmError> {
    let bytes = xy.as_bytes();
    if bytes.len() != 2 {
        return Err(ScmError::OperationFailed);
    }
    let staged = change_kind(bytes[0])?;
    let unstaged = change_kind(bytes[1])?;
    Ok(ScmStatusEntry {
        path: scm_path(path)?,
        staged,
        unstaged,
        untracked: false,
        conflicted: matches!(bytes, [b'U', _] | [_, b'U']),
    })
}

fn change_kind(value: u8) -> Result<Option<ScmChangeKind>, ScmError> {
    Ok(match value {
        b'.' => None,
        b'A' => Some(ScmChangeKind::Added),
        b'M' => Some(ScmChangeKind::Modified),
        b'D' => Some(ScmChangeKind::Deleted),
        b'R' => Some(ScmChangeKind::Renamed),
        b'C' => Some(ScmChangeKind::Copied),
        b'T' => Some(ScmChangeKind::TypeChanged),
        b'U' => Some(ScmChangeKind::Unmerged),
        _ => return Err(ScmError::OperationFailed),
    })
}

struct DiffCollection {
    lines: Vec<ScmDiffLine>,
    truncated: bool,
}

struct ParsedPatch {
    lines: Vec<ScmDiffLine>,
    scanned_lines: usize,
    truncated: bool,
}

async fn collect_diff(
    git_executable: &Path,
    resource: &WorkspaceRepositoryResource,
    target: &ScmDiffTarget,
    requested_path: Option<&WorkspacePath>,
    token: &CancellationToken,
) -> Result<DiffCollection, ScmError> {
    let mut name_arguments = diff_arguments(target);
    name_arguments.extend(["--name-status", "-z", "--no-renames", "--"]);
    if let Some(path) = requested_path {
        name_arguments.push(path.as_str());
    }
    let names = git_output(
        git_executable,
        resource,
        &name_arguments,
        MAX_STATUS_OUTPUT_BYTES,
        token,
    )
    .await?;
    let paths = parse_name_status(&names)?;
    if paths.len()
        > usize::try_from(workcell_host_contract::MAX_SCM_DIFF_FILES).unwrap_or(usize::MAX)
    {
        return Err(ScmError::LimitExceeded);
    }
    let mut lines = Vec::new();
    let mut scanned_bytes = names.len();
    let mut parsed_lines = 0usize;
    let mut truncated = false;
    for (change, path) in paths {
        resource
            .resolve_path(&path)
            .await
            .map_err(map_workspace_error)?;
        let mut arguments = diff_arguments(target);
        arguments.extend([
            "--patch",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--unified=3",
            "--no-renames",
            "--",
            path.as_str(),
        ]);
        lines.push(ScmDiffLine {
            path: path.clone(),
            kind: ScmDiffLineKind::File,
            change: Some(change),
            old_line: None,
            new_line: None,
            text: ScmText::new("").map_err(|_| ScmError::OperationFailed)?,
        });
        let remaining_bytes = (MAX_SCM_DIFF_SCAN_BYTES as usize).saturating_sub(scanned_bytes);
        if remaining_bytes == 0 {
            truncated = true;
            break;
        }
        let patch = match git_output(
            git_executable,
            resource,
            &arguments,
            remaining_bytes.min(MAX_DIFF_SOURCE_BYTES),
            token,
        )
        .await
        {
            Ok(patch) => patch,
            Err(ScmError::LimitExceeded) => {
                truncated = true;
                break;
            }
            Err(error) => return Err(error),
        };
        scanned_bytes = scanned_bytes.saturating_add(patch.len());
        let parsed = parse_patch(
            &path,
            &patch,
            (MAX_SCM_DIFF_PARSED_LINES as usize).saturating_sub(parsed_lines),
        )?;
        parsed_lines = parsed_lines.saturating_add(parsed.scanned_lines);
        lines.extend(parsed.lines);
        if parsed.truncated {
            truncated = true;
            break;
        }
    }
    Ok(DiffCollection { lines, truncated })
}

fn diff_arguments(target: &ScmDiffTarget) -> Vec<&str> {
    let mut arguments = vec!["diff", "--no-ext-diff", "--no-textconv", "--no-color"];
    match target {
        ScmDiffTarget::Staged => arguments.push("--cached"),
        ScmDiffTarget::Unstaged => {}
        ScmDiffTarget::Tree { base, target } => {
            arguments.extend([base.as_str(), target.as_str()]);
        }
    }
    arguments
}

fn parse_name_status(output: &[u8]) -> Result<Vec<(ScmChangeKind, WorkspacePath)>, ScmError> {
    let mut fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut paths = Vec::new();
    while let Some(status) = fields.next() {
        let path = fields.next().ok_or(ScmError::OperationFailed)?;
        if paths.len() == workcell_host_contract::MAX_SCM_DIFF_FILES as usize {
            return Err(ScmError::LimitExceeded);
        }
        let status = std::str::from_utf8(status).map_err(|_| ScmError::UnsupportedEncoding)?;
        let change = match status {
            "A" => ScmChangeKind::Added,
            "M" => ScmChangeKind::Modified,
            "D" => ScmChangeKind::Deleted,
            "T" => ScmChangeKind::TypeChanged,
            "U" => ScmChangeKind::Unmerged,
            _ => return Err(ScmError::OperationFailed),
        };
        let path = std::str::from_utf8(path).map_err(|_| ScmError::UnsupportedEncoding)?;
        paths.push((change, scm_path(path)?));
    }
    paths.sort_by(|left, right| left.1.as_str().cmp(right.1.as_str()));
    paths.dedup();
    Ok(paths)
}

fn parse_patch(
    path: &WorkspacePath,
    output: &[u8],
    maximum_scanned_lines: usize,
) -> Result<ParsedPatch, ScmError> {
    let text = std::str::from_utf8(output).map_err(|_| ScmError::UnsupportedEncoding)?;
    if text.contains("GIT binary patch")
        || text.lines().any(|line| line.starts_with("Binary files "))
    {
        return Ok(ParsedPatch {
            lines: vec![ScmDiffLine {
                path: path.clone(),
                kind: ScmDiffLineKind::Binary,
                change: None,
                old_line: None,
                new_line: None,
                text: ScmText::new("").map_err(|_| ScmError::OperationFailed)?,
            }],
            scanned_lines: output
                .iter()
                .filter(|byte| **byte == b'\n')
                .count()
                .min(maximum_scanned_lines),
            truncated: output.iter().filter(|byte| **byte == b'\n').count() > maximum_scanned_lines,
        });
    }
    let mut result = Vec::new();
    let mut old_line = 0u32;
    let mut new_line = 0u32;
    let mut in_hunk = false;
    let mut scanned_lines = 0usize;
    for line in text.lines() {
        if scanned_lines == maximum_scanned_lines {
            return Ok(ParsedPatch {
                lines: result,
                scanned_lines,
                truncated: true,
            });
        }
        scanned_lines = scanned_lines.saturating_add(1);
        if line.starts_with("@@ ") {
            (old_line, new_line) = parse_hunk_header(line)?;
            in_hunk = true;
            continue;
        }
        if !in_hunk || line == "\\ No newline at end of file" {
            continue;
        }
        let (kind, old, new, content) = match line.as_bytes().first() {
            Some(b' ') => {
                let record = (
                    ScmDiffLineKind::Context,
                    Some(old_line),
                    Some(new_line),
                    &line[1..],
                );
                old_line = old_line.saturating_add(1);
                new_line = new_line.saturating_add(1);
                record
            }
            Some(b'+') => {
                let record = (ScmDiffLineKind::Addition, None, Some(new_line), &line[1..]);
                new_line = new_line.saturating_add(1);
                record
            }
            Some(b'-') => {
                let record = (ScmDiffLineKind::Deletion, Some(old_line), None, &line[1..]);
                old_line = old_line.saturating_add(1);
                record
            }
            _ => return Err(ScmError::OperationFailed),
        };
        result.push(ScmDiffLine {
            path: path.clone(),
            kind,
            change: None,
            old_line: old,
            new_line: new,
            text: ScmText::new(content).map_err(|_| ScmError::LimitExceeded)?,
        });
    }
    Ok(ParsedPatch {
        lines: result,
        scanned_lines,
        truncated: false,
    })
}

fn parse_hunk_header(line: &str) -> Result<(u32, u32), ScmError> {
    let rest = line.strip_prefix("@@ -").ok_or(ScmError::OperationFailed)?;
    let (old, rest) = rest.split_once(" +").ok_or(ScmError::OperationFailed)?;
    let (new, _) = rest.split_once(" @@").ok_or(ScmError::OperationFailed)?;
    let parse_start = |range: &str| {
        range
            .split(',')
            .next()
            .ok_or(ScmError::OperationFailed)?
            .parse::<u32>()
            .map_err(|_| ScmError::OperationFailed)
    };
    Ok((parse_start(old)?, parse_start(new)?))
}

async fn validate_mutation_paths(
    resource: &WorkspaceRepositoryResource,
    mutation: &ScmMutation,
) -> Result<Vec<WorkspacePath>, ScmError> {
    if mutation.paths().is_empty() || mutation.paths().len() > MAX_SCM_PATHS {
        return Err(ScmError::InvalidRequest);
    }
    let mut seen = HashSet::new();
    let mut paths = Vec::with_capacity(mutation.paths().len());
    for path in mutation.paths() {
        let resolved = resource
            .resolve_path(path)
            .await
            .map_err(map_workspace_error)?;
        let relative = resolved.relative_to_repository().clone();
        if !seen.insert(relative.clone()) {
            return Err(ScmError::InvalidRequest);
        }
        paths.push(relative);
    }
    paths.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    Ok(paths)
}

fn mutation_entries(
    status: &[ScmStatusEntry],
    paths: &[WorkspacePath],
    mutation: &ScmMutation,
) -> Result<Vec<ScmStatusEntry>, ScmError> {
    let mut entries = Vec::with_capacity(paths.len());
    for path in paths {
        let entry = status
            .iter()
            .find(|entry| &entry.path == path)
            .ok_or(ScmError::InvalidRequest)?;
        let allowed = match mutation {
            ScmMutation::Stage { .. } => true,
            ScmMutation::Unstage { .. } => entry.staged.is_some() && !entry.conflicted,
            ScmMutation::Discard { .. } => {
                !entry.untracked && entry.unstaged.is_some() && !entry.conflicted
            }
        };
        if !allowed {
            return Err(ScmError::InvalidRequest);
        }
        entries.push(entry.clone());
    }
    Ok(entries)
}

fn verify_repository(resource: &WorkspaceRepositoryResource) -> Result<gix::Repository, ScmError> {
    verify_repository_with(resource, || {
        gix::open_opts(
            resource.worktree(),
            gix::open::Options::isolated().strict_config(true),
        )
        .map_err(|_| ScmError::RepositoryUnavailable)
    })
}

fn verify_repository_with(
    resource: &WorkspaceRepositoryResource,
    open_repository: impl FnOnce() -> Result<gix::Repository, ScmError>,
) -> Result<gix::Repository, ScmError> {
    let config = repository_config_snapshot(resource.git_dir())?;
    open_repository_after_config_snapshot(resource, &config, open_repository)
}

fn open_repository_after_config_snapshot(
    resource: &WorkspaceRepositoryResource,
    config: &RepositoryConfigSnapshot,
    open_repository: impl FnOnce() -> Result<gix::Repository, ScmError>,
) -> Result<gix::Repository, ScmError> {
    revalidate_repository_config(resource.git_dir(), config)?;
    let repository = open_repository();
    revalidate_repository_config(resource.git_dir(), config)?;
    let repository = repository?;
    let worktree = repository
        .workdir()
        .ok_or(ScmError::UnsupportedRepository)?;
    let canonical_worktree =
        std::fs::canonicalize(worktree).map_err(|_| ScmError::RepositoryUnavailable)?;
    let canonical_git_dir =
        std::fs::canonicalize(repository.path()).map_err(|_| ScmError::RepositoryUnavailable)?;
    if canonical_worktree != resource.worktree() || canonical_git_dir != resource.git_dir() {
        return Err(ScmError::UnsupportedRepository);
    }
    let config = repository.config_snapshot();
    if config
        .raw_values_by("gitoxide", "core", "shallowFile")
        .is_ok()
        || repository.shallow_file() != canonical_git_dir.join(SHALLOW_FILE_NAME)
    {
        return Err(ScmError::UnsupportedRepository);
    }
    for section in ["diff", "filter", "include", "includeIf"] {
        if config
            .sections_by_name(section)
            .is_some_and(|mut sections| sections.next().is_some())
        {
            return Err(ScmError::UnsupportedRepository);
        }
    }
    Ok(repository)
}

fn revalidate_repository_config(
    git_dir: &Path,
    expected: &RepositoryConfigSnapshot,
) -> Result<(), ScmError> {
    let current = repository_config_snapshot(git_dir).map_err(|_| ScmError::StaleRepository)?;
    if &current != expected {
        return Err(ScmError::StaleRepository);
    }
    Ok(())
}

fn repository_config_snapshot(git_dir: &Path) -> Result<RepositoryConfigSnapshot, ScmError> {
    let Some(mut file) = open_repository_config(git_dir)? else {
        return Ok(RepositoryConfigSnapshot::Missing);
    };
    let before = file
        .metadata()
        .map_err(|_| ScmError::RepositoryUnavailable)?;
    if !before.is_file() {
        return Err(ScmError::UnsupportedRepository);
    }
    if before.len() > MAX_SCM_CONFIG_BYTES {
        return Err(ScmError::LimitExceeded);
    }
    let capacity =
        usize::try_from(before.len().saturating_add(1)).map_err(|_| ScmError::LimitExceeded)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| ScmError::OperationFailed)?;
    file.by_ref()
        .take(MAX_SCM_CONFIG_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| ScmError::RepositoryUnavailable)?;
    let bytes_len = u64::try_from(bytes.len()).map_err(|_| ScmError::LimitExceeded)?;
    if bytes_len > MAX_SCM_CONFIG_BYTES {
        return Err(ScmError::LimitExceeded);
    }
    let after = file
        .metadata()
        .map_err(|_| ScmError::RepositoryUnavailable)?;
    let before_identity = config_file_identity(&before);
    if !after.is_file()
        || bytes_len != after.len()
        || before_identity != config_file_identity(&after)
    {
        return Err(ScmError::StaleRepository);
    }
    Ok(RepositoryConfigSnapshot::Present {
        identity: before_identity,
        digest: Sha256::digest(&bytes).into(),
    })
}

#[cfg(unix)]
fn open_repository_config(git_dir: &Path) -> Result<Option<File>, ScmError> {
    use rustix::{
        fs::{Mode, OFlags, open, openat},
        io::Errno,
    };

    let directory_flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY | OFlags::NOFOLLOW;
    let mut directory =
        open("/", directory_flags, Mode::empty()).map_err(|_| ScmError::RepositoryUnavailable)?;
    let mut absolute = false;
    for component in git_dir.components() {
        match component {
            Component::RootDir => absolute = true,
            Component::Normal(name) if absolute => {
                directory = openat(&directory, name, directory_flags, Mode::empty())
                    .map_err(|_| ScmError::RepositoryUnavailable)?;
            }
            _ => return Err(ScmError::UnsupportedRepository),
        }
    }
    let file_flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    match openat(&directory, CONFIG_FILE_NAME, file_flags, Mode::empty()) {
        Ok(descriptor) => Ok(Some(File::from(descriptor))),
        Err(Errno::NOENT) => Ok(None),
        Err(Errno::LOOP) => Err(ScmError::UnsupportedRepository),
        Err(_) => Err(ScmError::RepositoryUnavailable),
    }
}

#[cfg(not(unix))]
fn open_repository_config(git_dir: &Path) -> Result<Option<File>, ScmError> {
    let path = git_dir.join(CONFIG_FILE_NAME);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ScmError::UnsupportedRepository)
        }
        Ok(_) => File::open(path)
            .map(Some)
            .map_err(|_| ScmError::RepositoryUnavailable),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(ScmError::RepositoryUnavailable),
    }
}

#[cfg(unix)]
fn config_file_identity(metadata: &Metadata) -> ConfigFileIdentity {
    use std::os::unix::fs::MetadataExt;

    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

#[cfg(not(unix))]
fn config_file_identity(metadata: &Metadata) -> ConfigFileIdentity {
    (
        metadata.len(),
        metadata.modified().ok(),
        metadata.created().ok(),
    )
}

fn read_shallow_commits(
    repository: &gix::Repository,
    git_dir: &Path,
    token: &CancellationToken,
) -> Result<HashSet<gix::ObjectId>, ScmError> {
    check_cancelled(token)?;
    let Some(mut file) = open_shallow_file(git_dir)? else {
        return Ok(HashSet::new());
    };
    let metadata = file
        .metadata()
        .map_err(|_| ScmError::RepositoryUnavailable)?;
    if !metadata.is_file() {
        return Err(ScmError::UnsupportedRepository);
    }
    if metadata.len() > u64::try_from(MAX_SCM_SHALLOW_BYTES).unwrap_or(u64::MAX) {
        return Err(ScmError::LimitExceeded);
    }
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| ScmError::LimitExceeded)?
        .saturating_add(1);
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| ScmError::OperationFailed)?;
    file.by_ref()
        .take(
            u64::try_from(MAX_SCM_SHALLOW_BYTES)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        )
        .read_to_end(&mut bytes)
        .map_err(|_| ScmError::RepositoryUnavailable)?;
    check_cancelled(token)?;
    if bytes.len() > MAX_SCM_SHALLOW_BYTES {
        return Err(ScmError::LimitExceeded);
    }
    parse_shallow_commits(repository, &bytes)
}

fn parse_shallow_commits(
    repository: &gix::Repository,
    bytes: &[u8],
) -> Result<HashSet<gix::ObjectId>, ScmError> {
    if bytes.is_empty() {
        return Ok(HashSet::new());
    }
    let content = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if content.is_empty() {
        return Err(ScmError::RepositoryUnavailable);
    }
    let commit_count = content
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        .saturating_add(1);
    if commit_count > MAX_SCM_SHALLOW_COMMITS {
        return Err(ScmError::LimitExceeded);
    }
    let mut commits = HashSet::new();
    commits
        .try_reserve(commit_count)
        .map_err(|_| ScmError::OperationFailed)?;
    for line in content.split(|byte| *byte == b'\n') {
        let id = gix::ObjectId::from_hex(line).map_err(|_| ScmError::RepositoryUnavailable)?;
        if id.kind() != repository.object_hash() {
            return Err(ScmError::RepositoryUnavailable);
        }
        commits.insert(id);
    }
    Ok(commits)
}

#[cfg(unix)]
fn open_shallow_file(git_dir: &Path) -> Result<Option<File>, ScmError> {
    use rustix::{
        fs::{Mode, OFlags, open, openat},
        io::Errno,
    };

    let path = git_dir.join(SHALLOW_FILE_NAME);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(ScmError::UnsupportedRepository);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ScmError::RepositoryUnavailable),
    }

    let directory_flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY | OFlags::NOFOLLOW;
    let mut directory =
        open("/", directory_flags, Mode::empty()).map_err(|_| ScmError::RepositoryUnavailable)?;
    let mut absolute = false;
    for component in git_dir.components() {
        match component {
            Component::RootDir => absolute = true,
            Component::Normal(name) if absolute => {
                directory = openat(&directory, name, directory_flags, Mode::empty())
                    .map_err(|_| ScmError::RepositoryUnavailable)?;
            }
            _ => return Err(ScmError::UnsupportedRepository),
        }
    }
    let file_flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    match openat(&directory, SHALLOW_FILE_NAME, file_flags, Mode::empty()) {
        Ok(descriptor) => Ok(Some(File::from(descriptor))),
        Err(Errno::NOENT) => Ok(None),
        Err(_) => Err(ScmError::RepositoryUnavailable),
    }
}

#[cfg(not(unix))]
fn open_shallow_file(git_dir: &Path) -> Result<Option<File>, ScmError> {
    let path = git_dir.join(SHALLOW_FILE_NAME);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ScmError::UnsupportedRepository)
        }
        Ok(_) => File::open(path)
            .map(Some)
            .map_err(|_| ScmError::RepositoryUnavailable),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(ScmError::RepositoryUnavailable),
    }
}

fn repository_identity(resource: &WorkspaceRepositoryResource) -> Result<Revision, ScmError> {
    digest(&(
        resource.resource_id(),
        resource.relative_path(),
        resource.git_dir().to_string_lossy(),
    ))
}

fn head_object_id(repository: &gix::Repository) -> Result<Option<gix::ObjectId>, ScmError> {
    let head = repository
        .head()
        .map_err(|_| ScmError::RepositoryUnavailable)?;
    Ok(head.id().map(gix::Id::detach))
}

fn head_revision(repository: &gix::Repository) -> Result<Revision, ScmError> {
    revision_from_head(head_object_id(repository)?.as_ref())
}

fn revision_from_head(head: Option<&gix::ObjectId>) -> Result<Revision, ScmError> {
    if let Some(head) = head {
        Revision::new(head.to_string()).map_err(|_| ScmError::OperationFailed)
    } else {
        Revision::new("unborn").map_err(|_| ScmError::OperationFailed)
    }
}

async fn index_revision(
    resource: &WorkspaceRepositoryResource,
    token: &CancellationToken,
) -> Result<Revision, ScmError> {
    let path = resource.git_dir().join("index");
    match read_file_bounded(&path, MAX_INDEX_BYTES, token).await {
        Ok(bytes) => digest_bytes(&bytes),
        Err(ScmError::OperationFailed) if !fs::try_exists(&path).await.unwrap_or(true) => {
            Revision::new("missing").map_err(|_| ScmError::OperationFailed)
        }
        Err(error) => Err(error),
    }
}

fn commit_dto(
    id: gix::ObjectId,
    decoded: &gix::objs::CommitRef<'_>,
) -> Result<ScmCommit, ScmError> {
    let author = decoded
        .author()
        .map_err(|_| ScmError::RepositoryUnavailable)?;
    let time = decoded
        .time()
        .map_err(|_| ScmError::RepositoryUnavailable)?;
    let message =
        std::str::from_utf8(decoded.message).map_err(|_| ScmError::UnsupportedEncoding)?;
    let summary = message.lines().next().unwrap_or_default();
    let summary = truncate_utf8(summary, MAX_COMMIT_SUMMARY_BYTES);
    let body = message
        .split_once('\n')
        .map(|(_, body)| body.trim())
        .unwrap_or_default();
    let body = truncate_utf8(body, MAX_COMMIT_BODY_BYTES);
    Ok(ScmCommit {
        id: Revision::new(id.to_string()).map_err(|_| ScmError::OperationFailed)?,
        parents: decoded
            .parents()
            .map(|parent| Revision::new(parent.to_string()).map_err(|_| ScmError::OperationFailed))
            .collect::<Result<Vec<_>, _>>()?,
        author_name: ScmText::new(
            std::str::from_utf8(author.name).map_err(|_| ScmError::UnsupportedEncoding)?,
        )
        .map_err(|_| ScmError::LimitExceeded)?,
        author_email: ScmText::new(
            std::str::from_utf8(author.email).map_err(|_| ScmError::UnsupportedEncoding)?,
        )
        .map_err(|_| ScmError::LimitExceeded)?,
        committed_unix_seconds: time.seconds,
        summary: ScmText::new(summary).map_err(|_| ScmError::LimitExceeded)?,
        body: Some(ScmText::new(body).map_err(|_| ScmError::LimitExceeded)?),
    })
}

async fn git_blob(
    git_executable: &Path,
    resource: &WorkspaceRepositoryResource,
    object: &str,
    token: &CancellationToken,
) -> Result<Vec<u8>, ScmError> {
    git_output(
        git_executable,
        resource,
        &["show", "--no-textconv", object],
        MAX_DIFF_SOURCE_BYTES,
        token,
    )
    .await
}

async fn git_available(executable: &Path) -> bool {
    let mut child = match git_command(executable)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let Some(stdout) = child.stdout.take() else {
        terminate_and_reap(&mut child).await;
        return false;
    };
    let mut output = Vec::new();
    let mut bounded = stdout.take(
        u64::try_from(MAX_GIT_VERSION_BYTES)
            .unwrap_or(u64::MAX)
            .saturating_add(1),
    );
    if !matches!(
        timeout(GIT_PROBE_TIMEOUT, bounded.read_to_end(&mut output)).await,
        Ok(Ok(_))
    ) || output.len() > MAX_GIT_VERSION_BYTES
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return false;
    }
    let status = match timeout(GIT_PROBE_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        _ => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return false;
        }
    };
    status.success()
        && std::str::from_utf8(&output).is_ok_and(|version| version.starts_with("git version "))
}

fn git_command(executable: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_DIFF_OPTS")
        .env_remove("GIT_EXEC_PATH")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_EXTERNAL_DIFF_TRUST_EXIT_CODE")
        .env_remove("GIT_PAGER")
        .env_remove("PAGER")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
    command
}

async fn git_output(
    git_executable: &Path,
    resource: &WorkspaceRepositoryResource,
    arguments: &[&str],
    maximum: usize,
    token: &CancellationToken,
) -> Result<Vec<u8>, ScmError> {
    check_cancelled(token)?;
    let mut child = git_command(git_executable)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .arg("-C")
        .arg(resource.worktree())
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ScmError::OperationFailed)?;
    let Some(stdout) = child.stdout.take() else {
        terminate_and_reap(&mut child).await;
        return Err(ScmError::OperationFailed);
    };
    let mut output = Vec::new();
    let mut bounded_stdout =
        stdout.take(u64::try_from(maximum).unwrap_or(u64::MAX).saturating_add(1));
    let read = bounded_stdout.read_to_end(&mut output);
    tokio::pin!(read);
    let read_result = tokio::select! {
        biased;
        () = token.cancelled() => Err(ScmError::Cancelled),
        result = timeout(GIT_TIMEOUT, &mut read) => {
            result
                .map_err(|_| ScmError::TimedOut)
                .and_then(|result| result.map_err(|_| ScmError::OperationFailed))
        }
    };
    if let Err(error) = read_result {
        terminate_and_reap(&mut child).await;
        return Err(error);
    }
    if output.len() > maximum {
        terminate_and_reap(&mut child).await;
        return Err(ScmError::LimitExceeded);
    }
    let status = tokio::select! {
        biased;
        () = token.cancelled() => Err(ScmError::Cancelled),
        status = timeout(GIT_TIMEOUT, child.wait()) => {
            status
                .map_err(|_| ScmError::TimedOut)
                .and_then(|status| status.map_err(|_| ScmError::OperationFailed))
        }
    };
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            terminate_and_reap(&mut child).await;
            return Err(error);
        }
    };
    if !status.success() {
        return Err(
            if fs::try_exists(resource.git_dir().join("index.lock"))
                .await
                .unwrap_or(true)
            {
                ScmError::Locked
            } else {
                ScmError::OperationFailed
            },
        );
    }
    Ok(output)
}

async fn git_mutation(
    git_executable: &Path,
    resource: &WorkspaceRepositoryResource,
    arguments: &[&str],
    token: &CancellationToken,
) -> Result<(), ScmError> {
    check_cancelled(token)?;
    let mut child = git_command(git_executable)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .arg("-C")
        .arg(resource.worktree())
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ScmError::OperationFailed)?;
    let status = tokio::select! {
        biased;
        () = token.cancelled() => Err(ScmError::Cancelled),
        status = timeout(GIT_TIMEOUT, child.wait()) => {
            status
                .map_err(|_| ScmError::TimedOut)
                .and_then(|status| status.map_err(|_| ScmError::OperationFailed))
        }
    };
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            terminate_and_reap(&mut child).await;
            return Err(error);
        }
    };
    if !status.success() {
        sleep(Duration::from_millis(1)).await;
        return Err(
            if fs::try_exists(resource.git_dir().join("index.lock"))
                .await
                .unwrap_or(true)
            {
                ScmError::Locked
            } else {
                ScmError::OperationFailed
            },
        );
    }
    Ok(())
}

async fn terminate_and_reap(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn read_file_bounded(
    path: &Path,
    maximum: usize,
    token: &CancellationToken,
) -> Result<Vec<u8>, ScmError> {
    check_cancelled(token)?;
    let file = fs::File::open(path)
        .await
        .map_err(|_| ScmError::OperationFailed)?;
    let mut bounded_file = file.take(u64::try_from(maximum).unwrap_or(u64::MAX).saturating_add(1));
    let mut bytes = Vec::new();
    tokio::select! {
        biased;
        () = token.cancelled() => return Err(ScmError::Cancelled),
        result = bounded_file.read_to_end(&mut bytes) => {
            result.map_err(|_| ScmError::OperationFailed)?;
        }
    }
    if bytes.len() > maximum {
        return Err(ScmError::LimitExceeded);
    }
    Ok(bytes)
}

fn validate_page_size(value: u32, maximum: u32) -> Result<(), ScmError> {
    if value == 0 || value > maximum {
        return Err(ScmError::InvalidRequest);
    }
    Ok(())
}

fn validate_read_limits(
    lines: u32,
    bytes: u32,
    maximum_lines: u32,
    maximum_bytes: u32,
) -> Result<(), ScmError> {
    if lines == 0 || lines > maximum_lines || bytes == 0 || bytes > maximum_bytes {
        return Err(ScmError::InvalidRequest);
    }
    Ok(())
}

fn validate_diff_target(target: &ScmDiffTarget) -> Result<(), ScmError> {
    if let ScmDiffTarget::Tree { base, target } = target {
        validate_object_id(base)?;
        validate_object_id(target)?;
    }
    Ok(())
}

fn validate_object_id(revision: &Revision) -> Result<(), ScmError> {
    let value = revision.as_str();
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ScmError::InvalidRequest);
    }
    Ok(())
}

fn scm_path(path: &str) -> Result<WorkspacePath, ScmError> {
    if path == "." || path.is_empty() {
        return Err(ScmError::OperationFailed);
    }
    WorkspacePath::new(path).map_err(|_| ScmError::OperationFailed)
}

fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split_terminator('\n').collect()
    }
}

fn truncate_utf8(value: &str, maximum: usize) -> &str {
    if value.len() <= maximum {
        return value;
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn check_cancelled(token: &CancellationToken) -> Result<(), ScmError> {
    if token.is_cancelled() {
        Err(ScmError::Cancelled)
    } else {
        Ok(())
    }
}

fn map_workspace_error(error: WorkspaceError) -> ScmError {
    match error {
        WorkspaceError::StaleCwd | WorkspaceError::StaleResource => ScmError::StaleRepository,
        WorkspaceError::RepositoryUnavailable => ScmError::RepositoryUnavailable,
        WorkspaceError::NotRepository => ScmError::NotRepository,
        WorkspaceError::FileTooLarge { .. } => ScmError::LimitExceeded,
        WorkspaceError::UnsupportedRepository => ScmError::UnsupportedRepository,
        WorkspaceError::InvalidRequest
        | WorkspaceError::InvalidCursor
        | WorkspaceError::StaleCursor
        | WorkspaceError::WatchUnavailable { .. }
        | WorkspaceError::RolledBack(_)
        | WorkspaceError::PartialFailure(_)
        | WorkspaceError::Filesystem(_) => ScmError::InvalidRequest,
    }
}

fn digest(value: &impl Serialize) -> Result<Revision, ScmError> {
    let bytes = serde_json::to_vec(value).map_err(|_| ScmError::OperationFailed)?;
    digest_bytes(&bytes)
}

fn digest_bytes(bytes: &[u8]) -> Result<Revision, ScmError> {
    Revision::new(hex_digest(bytes)).map_err(|_| ScmError::OperationFailed)
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut value = String::from("sha256:");
    for byte in Sha256::digest(bytes) {
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn prefixed_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut value = String::from("sha256:");
    for byte in bytes {
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, fs as std_fs, process::Command as StdCommand};

    use tempfile::TempDir;
    use workcell_host_contract::{
        HostBinding, Identifier, ScmDiscoverRequest, ScmLogRequest, ScmStatusRequest,
        WorkspaceRequestBinding,
    };

    use super::*;

    const NOT_REPOSITORY_CODE: &str = "not_repository";

    #[tokio::test]
    async fn discovery_distinguishes_absent_repositories_from_corrupt_or_missing_roots() {
        let parent = tempfile::tempdir().unwrap();
        init_repository(parent.path());
        let root = parent.path().join("workspace");
        std_fs::create_dir(&root).unwrap();
        std_fs::create_dir(root.join("nested")).unwrap();
        let files = FileToolGroup::new(&root, false, None).await.unwrap();
        let cwd = files.workspace_root().await.unwrap();
        let group = ScmGroup::new(files);
        let request = ScmDiscoverRequest {
            version: ContractVersion::V1,
            binding: WorkspaceRequestBinding {
                host: host_binding(cwd.handle.clone()),
                cwd_handle: cwd.handle,
            },
            path: path("nested"),
        };
        let error = group
            .discover(&request, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error, ScmError::NotRepository);
        assert_eq!(error.code(), NOT_REPOSITORY_CODE);
        std_fs::create_dir(root.join(".git")).unwrap();
        assert_eq!(
            group
                .discover(&request, &CancellationToken::new())
                .await
                .unwrap_err(),
            ScmError::RepositoryUnavailable
        );
        std_fs::remove_dir(root.join("nested")).unwrap();
        assert_eq!(
            group
                .discover(&request, &CancellationToken::new())
                .await
                .unwrap_err(),
            ScmError::RepositoryUnavailable
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inaccessible_repository_discovery_remains_a_failure_not_confirmed_absence() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let denied = root.path().join("denied");
        std_fs::create_dir(&denied).unwrap();
        let files = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let cwd = files.workspace_root().await.unwrap();
        let group = ScmGroup::new(files);
        let request = ScmDiscoverRequest {
            version: ContractVersion::V1,
            binding: WorkspaceRequestBinding {
                host: host_binding(cwd.handle.clone()),
                cwd_handle: cwd.handle,
            },
            path: path("denied"),
        };
        std_fs::set_permissions(&denied, std_fs::Permissions::from_mode(0o0)).unwrap();
        let result = group.discover(&request, &CancellationToken::new()).await;
        std_fs::set_permissions(&denied, std_fs::Permissions::from_mode(0o700)).unwrap();
        if !rustix::process::geteuid().is_root() {
            assert_eq!(result.unwrap_err(), ScmError::RepositoryUnavailable);
        }
    }

    struct Fixture {
        _root: TempDir,
        group: ScmGroup,
        binding: WorkspaceRequestBinding,
        repository: ScmRepository,
    }

    struct ObservedCommitStore<'repo> {
        repository: &'repo gix::Repository,
        header_reads: RefCell<Vec<gix::ObjectId>>,
        body_loads: RefCell<Vec<gix::ObjectId>>,
    }

    impl<'repo> ObservedCommitStore<'repo> {
        fn new(repository: &'repo gix::Repository) -> Self {
            Self {
                repository,
                header_reads: RefCell::new(Vec::new()),
                body_loads: RefCell::new(Vec::new()),
            }
        }
    }

    impl CommitObjectStore for ObservedCommitStore<'_> {
        fn object_header(&self, id: &gix::ObjectId) -> Result<(gix::objs::Kind, u64), ScmError> {
            self.header_reads.borrow_mut().push(id.to_owned());
            self.repository.object_header(id)
        }

        fn load_object<'a>(
            &self,
            id: &gix::ObjectId,
            buffer: &'a mut Vec<u8>,
        ) -> Result<gix::objs::Data<'a>, ScmError> {
            self.body_loads.borrow_mut().push(id.to_owned());
            self.repository.load_object(id, buffer)
        }
    }

    #[tokio::test]
    async fn status_log_diffs_sides_and_prepared_mutations_are_structured() {
        let fixture = fixture(true).await;
        let root = fixture._root.path();
        std_fs::create_dir(root.join("nested")).unwrap();
        let alias = fixture
            .group
            .discover(
                &ScmDiscoverRequest {
                    version: ContractVersion::V1,
                    binding: fixture.binding.clone(),
                    path: path("nested/.."),
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap()
            .repository;
        assert_ne!(alias.handle, fixture.repository.handle);
        assert_eq!(alias.resource_id, fixture.repository.resource_id);
        let first_commit = git_text(root, &["rev-parse", "HEAD"]);
        std_fs::write(root.join("tracked.txt"), "one\ntwo committed\nthree\n").unwrap();
        git(root, &["add", "--", "tracked.txt"]);
        git(root, &["commit", "-m", "second"]);
        let second_commit = git_text(root, &["rev-parse", "HEAD"]);
        std_fs::write(root.join("tracked.txt"), "one\ntwo changed\nthree\n").unwrap();
        std_fs::write(root.join("untracked.txt"), "new\n").unwrap();

        let first = fixture
            .group
            .status(
                &status_request(&fixture, 1, None),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(first.entries.len(), 1);
        assert!(first.next_cursor.is_some());
        let second = fixture
            .group
            .status(
                &status_request(&fixture, 1, first.next_cursor),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(second.entries.len(), 1);
        assert!(second.entries.iter().any(|entry| entry.untracked));

        let log = fixture
            .group
            .log(
                &ScmLogRequest {
                    version: ContractVersion::V1,
                    binding: fixture.binding.clone(),
                    repository_handle: fixture.repository.handle.clone(),
                    page_size: 1,
                    cursor: None,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(log.commits[0].summary.as_str(), "second");
        let older = fixture
            .group
            .log(
                &ScmLogRequest {
                    version: ContractVersion::V1,
                    binding: fixture.binding.clone(),
                    repository_handle: fixture.repository.handle.clone(),
                    page_size: 1,
                    cursor: log.next_cursor,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(older.commits[0].summary.as_str(), "initial");

        let diff = fixture
            .group
            .diff(
                &ScmDiffRequest {
                    version: ContractVersion::V1,
                    binding: fixture.binding.clone(),
                    repository_handle: fixture.repository.handle.clone(),
                    target: ScmDiffTarget::Unstaged,
                    path: Some(path("tracked.txt")),
                    max_lines: 100,
                    max_bytes: 32_768,
                    cursor: None,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(diff.lines.iter().any(|line| {
            line.kind == ScmDiffLineKind::Addition && line.text.as_str() == "two changed"
        }));
        let first_diff_page = fixture
            .group
            .diff(
                &ScmDiffRequest {
                    version: ContractVersion::V1,
                    binding: fixture.binding.clone(),
                    repository_handle: fixture.repository.handle.clone(),
                    target: ScmDiffTarget::Unstaged,
                    path: Some(path("tracked.txt")),
                    max_lines: 1,
                    max_bytes: 32_768,
                    cursor: None,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(first_diff_page.truncated);
        assert_eq!(first_diff_page.lines[0].kind, ScmDiffLineKind::File);
        let continued_diff = fixture
            .group
            .diff(
                &ScmDiffRequest {
                    version: ContractVersion::V1,
                    binding: fixture.binding.clone(),
                    repository_handle: fixture.repository.handle.clone(),
                    target: ScmDiffTarget::Unstaged,
                    path: Some(path("tracked.txt")),
                    max_lines: 1,
                    max_bytes: 32_768,
                    cursor: first_diff_page.next_cursor,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(continued_diff.lines.len(), 1);
        let tree_diff = fixture
            .group
            .diff(
                &ScmDiffRequest {
                    version: ContractVersion::V1,
                    binding: fixture.binding.clone(),
                    repository_handle: fixture.repository.handle.clone(),
                    target: ScmDiffTarget::Tree {
                        base: Revision::new(first_commit).unwrap(),
                        target: Revision::new(second_commit).unwrap(),
                    },
                    path: Some(path("tracked.txt")),
                    max_lines: 100,
                    max_bytes: 32_768,
                    cursor: None,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(tree_diff.lines.iter().any(|line| {
            line.kind == ScmDiffLineKind::Addition && line.text.as_str() == "two committed"
        }));

        let head = fixture
            .group
            .read_side(
                &side_request(&fixture, ScmSide::Head),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            head.resource_id,
            workcell_mcp_files::root_relative_resource_id(
                workcell_mcp_files::RootResourceKind::Path,
                "tracked.txt",
            )
            .unwrap()
        );
        let worktree = fixture
            .group
            .read_side(
                &side_request(&fixture, ScmSide::Worktree),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(head.content.as_str(), "one\ntwo committed\nthree");
        assert_eq!(worktree.content.as_str(), "one\ntwo changed\nthree");
        let mut side_page_request = side_request(&fixture, ScmSide::Worktree);
        side_page_request.max_lines = 1;
        let side_page = fixture
            .group
            .read_side(&side_page_request, &CancellationToken::new())
            .await
            .unwrap();
        assert!(side_page.truncated);
        side_page_request.start_line = side_page.next_start_line.unwrap();
        let side_continuation = fixture
            .group
            .read_side(&side_page_request, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(side_continuation.content.as_str(), "two changed");

        let staged = fixture
            .group
            .prepare_mutation(
                &fixture.repository.handle,
                &fixture.binding,
                ScmMutation::Stage {
                    paths: vec![path("tracked.txt"), path("untracked.txt")],
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(ScmGroup::preview(&staged).entries.len(), 2);
        assert!(
            staged.retained_bytes()
                >= serde_json::to_vec(ScmGroup::preview(&staged))
                    .unwrap()
                    .len()
        );
        fixture
            .group
            .execute_mutation(staged, &CancellationToken::new())
            .await
            .unwrap();
        let staged_status = current_status(&fixture).await;
        assert!(
            staged_status
                .entries
                .iter()
                .all(|entry| entry.staged.is_some())
        );

        let unstaged = fixture
            .group
            .prepare_mutation(
                &fixture.repository.handle,
                &fixture.binding,
                ScmMutation::Unstage {
                    paths: vec![path("tracked.txt"), path("untracked.txt")],
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        fixture
            .group
            .execute_mutation(unstaged, &CancellationToken::new())
            .await
            .unwrap();

        let discard = fixture
            .group
            .prepare_mutation(
                &fixture.repository.handle,
                &fixture.binding,
                ScmMutation::Discard {
                    paths: vec![path("tracked.txt")],
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        fixture
            .group
            .execute_mutation(discard, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            std_fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "one\ntwo committed\nthree\n"
        );
        assert!(root.join("untracked.txt").exists());
        assert_eq!(
            fixture
                .group
                .prepare_mutation(
                    &fixture.repository.handle,
                    &fixture.binding,
                    ScmMutation::Discard {
                        paths: vec![path("untracked.txt")],
                    },
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err(),
            ScmError::InvalidRequest
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn mixed_scm_requests_share_the_process_wide_concurrency_bound() {
        let _serial = concurrency_test_lock().lock().await;
        let mut fixture = fixture(true).await;
        std_fs::write(fixture._root.path().join("tracked.txt"), "changed\n").unwrap();
        let observer = AdmissionObserver::new(MAX_CONCURRENT_SCM_OPERATIONS);
        fixture.group.admission_observer = Some(observer.clone());
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..MAX_CONCURRENT_SCM_OPERATIONS * 10 {
            let group = fixture.group.clone();
            let status = status_request(&fixture, MAX_SCM_STATUS_ENTRIES, None);
            let log = log_request(&fixture, MAX_SCM_LOG_ENTRIES);
            let diff = ScmDiffRequest {
                version: ContractVersion::V1,
                binding: fixture.binding.clone(),
                repository_handle: fixture.repository.handle.clone(),
                target: ScmDiffTarget::Unstaged,
                path: Some(path("tracked.txt")),
                max_lines: MAX_SCM_DIFF_LINES,
                max_bytes: MAX_SCM_DIFF_BYTES,
                cursor: None,
            };
            let side = side_request(&fixture, ScmSide::Head);
            let repository_handle = fixture.repository.handle.clone();
            let binding = fixture.binding.clone();
            tasks.spawn(async move {
                match index % 5 {
                    0 => group
                        .status(&status, &CancellationToken::new())
                        .await
                        .map(|_| ()),
                    1 => group.log(&log, &CancellationToken::new()).await.map(|_| ()),
                    2 => group
                        .diff(&diff, &CancellationToken::new())
                        .await
                        .map(|_| ()),
                    3 => group
                        .read_side(&side, &CancellationToken::new())
                        .await
                        .map(|_| ()),
                    _ => group
                        .prepare_mutation(
                            &repository_handle,
                            &binding,
                            ScmMutation::Stage {
                                paths: vec![path("tracked.txt")],
                            },
                            &CancellationToken::new(),
                        )
                        .await
                        .map(|_| ()),
                }
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap().unwrap();
        }
        assert_eq!(
            observer.maximum.load(std::sync::atomic::Ordering::SeqCst),
            MAX_CONCURRENT_SCM_OPERATIONS
        );
        assert_eq!(observer.active.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancellation_while_queued_for_scm_admission_is_honored() {
        let _serial = concurrency_test_lock().lock().await;
        let fixture = fixture(false).await;
        let mut permits = Vec::new();
        for _ in 0..MAX_CONCURRENT_SCM_OPERATIONS {
            permits.push(concurrency().clone().acquire_owned().await.unwrap());
        }
        let group = fixture.group.clone();
        let request = status_request(&fixture, MAX_SCM_STATUS_ENTRIES, None);
        let token = CancellationToken::new();
        let task_token = token.clone();
        let task = tokio::spawn(async move { group.status(&request, &task_token).await });
        tokio::task::yield_now().await;
        token.cancel();
        assert_eq!(task.await.unwrap().unwrap_err(), ScmError::Cancelled);
        drop(permits);
    }

    #[tokio::test]
    async fn cursors_are_request_bound_and_become_stale_after_repository_changes() {
        let fixture = fixture(true).await;
        let mut wrong_generation = status_request(&fixture, 1, None);
        wrong_generation.binding.host.workspace_generation =
            Identifier::new("other-generation").unwrap();
        assert_eq!(
            fixture
                .group
                .status(&wrong_generation, &CancellationToken::new())
                .await
                .unwrap_err(),
            ScmError::StaleRepository
        );
        std_fs::write(fixture._root.path().join("a.txt"), "a").unwrap();
        std_fs::write(fixture._root.path().join("b.txt"), "b").unwrap();
        let first = fixture
            .group
            .status(
                &status_request(&fixture, 1, None),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let cursor = first.next_cursor.unwrap();
        let mut log_request = ScmLogRequest {
            version: ContractVersion::V1,
            binding: fixture.binding.clone(),
            repository_handle: fixture.repository.handle.clone(),
            page_size: 1,
            cursor: Some(cursor.clone()),
        };
        assert_eq!(
            fixture
                .group
                .log(&log_request, &CancellationToken::new())
                .await
                .unwrap_err(),
            ScmError::InvalidCursor
        );
        log_request.cursor = None;
        std_fs::write(fixture._root.path().join("c.txt"), "c").unwrap();
        assert_eq!(
            fixture
                .group
                .status(
                    &status_request(&fixture, 1, Some(cursor)),
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err(),
            ScmError::StaleCursor
        );
        assert_eq!(
            fixture
                .group
                .status(
                    &status_request(&fixture, 1, Some(Cursor::new("forged").unwrap()),),
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err(),
            ScmError::InvalidCursor
        );
    }

    #[tokio::test]
    async fn prepared_mutations_reject_stale_repositories_locks_and_cancellation() {
        let fixture = fixture(true).await;
        std_fs::write(fixture._root.path().join("tracked.txt"), "changed").unwrap();
        let prepared = fixture
            .group
            .prepare_mutation(
                &fixture.repository.handle,
                &fixture.binding,
                ScmMutation::Stage {
                    paths: vec![path("tracked.txt")],
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        std_fs::write(fixture._root.path().join("tracked.txt"), "changed again").unwrap();
        assert_eq!(
            fixture
                .group
                .execute_mutation(prepared, &CancellationToken::new())
                .await
                .unwrap_err(),
            ScmError::StalePreparedOperation
        );

        let prepared = fixture
            .group
            .prepare_mutation(
                &fixture.repository.handle,
                &fixture.binding,
                ScmMutation::Stage {
                    paths: vec![path("tracked.txt")],
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        std_fs::write(fixture._root.path().join(".git/index.lock"), "locked").unwrap();
        assert_eq!(
            fixture
                .group
                .execute_mutation(prepared, &CancellationToken::new())
                .await
                .unwrap_err(),
            ScmError::Locked
        );
        std_fs::remove_file(fixture._root.path().join(".git/index.lock")).unwrap();

        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            fixture
                .group
                .status(&status_request(&fixture, 10, None), &token)
                .await
                .unwrap_err(),
            ScmError::Cancelled
        );
    }

    #[tokio::test]
    async fn discovery_rejects_escape_linked_gitdirs_and_read_only_mutations() {
        let outside = tempfile::tempdir().unwrap();
        init_repository(outside.path());
        let root = tempfile::tempdir().unwrap();
        std_fs::write(
            root.path().join(".git"),
            format!("gitdir: {}", outside.path().join(".git").display()),
        )
        .unwrap();
        let files = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let cwd = files.workspace_root().await.unwrap();
        assert_eq!(
            files
                .workspace_discover_repository(&cwd.handle, &path("."))
                .await
                .unwrap_err()
                .code(),
            "unsupported_repository"
        );

        let submodule_root = tempfile::tempdir().unwrap();
        init_repository(submodule_root.path());
        std_fs::write(
            submodule_root.path().join(".gitmodules"),
            "[submodule \"nested\"]\npath = nested\nurl = ../nested\n",
        )
        .unwrap();
        let files = FileToolGroup::new(submodule_root.path(), false, None)
            .await
            .unwrap();
        let cwd = files.workspace_root().await.unwrap();
        assert_eq!(
            files
                .workspace_discover_repository(&cwd.handle, &path("."))
                .await
                .unwrap_err()
                .code(),
            "unsupported_repository"
        );

        let filter_root = tempfile::tempdir().unwrap();
        init_repository(filter_root.path());
        git(
            filter_root.path(),
            &["config", "filter.external.clean", "printf forbidden"],
        );
        let files = FileToolGroup::new(filter_root.path(), false, None)
            .await
            .unwrap();
        let cwd = files.workspace_root().await.unwrap();
        let group = ScmGroup::new(files);
        assert_eq!(
            group
                .discover(
                    &ScmDiscoverRequest {
                        version: ContractVersion::V1,
                        binding: WorkspaceRequestBinding {
                            host: host_binding(cwd.handle.clone()),
                            cwd_handle: cwd.handle,
                        },
                        path: path("."),
                    },
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err(),
            ScmError::UnsupportedRepository
        );

        let plain = tempfile::tempdir().unwrap();
        let files = FileToolGroup::new(plain.path(), false, None).await.unwrap();
        let cwd = files.workspace_root().await.unwrap();
        assert!(
            files
                .workspace_discover_repository(&cwd.handle, &path("../outside"))
                .await
                .is_err()
        );

        let fixture = fixture(false).await;
        std_fs::write(fixture._root.path().join("tracked.txt"), "changed").unwrap();
        assert_eq!(
            fixture
                .group
                .prepare_mutation(
                    &fixture.repository.handle,
                    &fixture.binding,
                    ScmMutation::Stage {
                        paths: vec![path("tracked.txt")],
                    },
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err(),
            ScmError::InvalidRequest
        );
    }

    #[tokio::test]
    async fn oversized_and_raced_configs_do_not_enter_the_gix_open_path() {
        let root = tempfile::tempdir().unwrap();
        init_repository(root.path());
        let files = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let cwd = files.workspace_root().await.unwrap();
        let resource = files
            .workspace_discover_repository(&cwd.handle, &path("."))
            .await
            .unwrap();
        let config_path = resource.git_dir().join(CONFIG_FILE_NAME);
        let original = std_fs::read(&config_path).unwrap();
        let mut at_limit = original.clone();
        at_limit.push(b'#');
        at_limit.resize(usize::try_from(MAX_SCM_CONFIG_BYTES).unwrap(), b'x');
        std_fs::write(&config_path, at_limit).unwrap();
        assert!(verify_repository(&resource).is_ok());

        let oversized = vec![b'x'; usize::try_from(MAX_SCM_CONFIG_BYTES).unwrap() + 1];
        std_fs::write(&config_path, &oversized).unwrap();
        let open_called = RefCell::new(false);
        assert_eq!(
            verify_repository_with(&resource, || {
                *open_called.borrow_mut() = true;
                Err(ScmError::OperationFailed)
            })
            .unwrap_err(),
            ScmError::LimitExceeded
        );
        assert!(!*open_called.borrow());

        std_fs::write(&config_path, &original).unwrap();
        let snapshot = repository_config_snapshot(resource.git_dir()).unwrap();
        std_fs::write(&config_path, oversized).unwrap();
        assert_eq!(
            open_repository_after_config_snapshot(&resource, &snapshot, || {
                *open_called.borrow_mut() = true;
                Err(ScmError::OperationFailed)
            })
            .unwrap_err(),
            ScmError::StaleRepository
        );
        assert!(!*open_called.borrow());

        std_fs::write(&config_path, &original).unwrap();
        let snapshot = repository_config_snapshot(resource.git_dir()).unwrap();
        let mut changed = original.clone();
        changed.extend_from_slice(b"# changed\n");
        assert_eq!(
            open_repository_after_config_snapshot(&resource, &snapshot, || {
                let repository = gix::open_opts(
                    resource.worktree(),
                    gix::open::Options::isolated().strict_config(true),
                )
                .map_err(|_| ScmError::RepositoryUnavailable);
                std_fs::write(&config_path, changed).unwrap();
                repository
            })
            .unwrap_err(),
            ScmError::StaleRepository
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let outside = tempfile::tempdir().unwrap();
            let outside_config = outside.path().join(CONFIG_FILE_NAME);
            std_fs::write(&outside_config, &original).unwrap();
            std_fs::remove_file(&config_path).unwrap();
            symlink(outside_config, &config_path).unwrap();
            assert_eq!(
                verify_repository_with(&resource, || {
                    *open_called.borrow_mut() = true;
                    Err(ScmError::OperationFailed)
                })
                .unwrap_err(),
                ScmError::UnsupportedRepository
            );
            assert!(!*open_called.borrow());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repository_diff_helpers_are_rejected_without_running_the_sentinel() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = fixture(true).await;
        let sentinel = fixture._root.path().join("helper-ran");
        let helper = fixture._root.path().join("diff-helper");
        std_fs::write(
            &helper,
            format!("#!/bin/sh\ntouch '{}'\n", sentinel.display()),
        )
        .unwrap();
        let mut permissions = std_fs::metadata(&helper).unwrap().permissions();
        permissions.set_mode(0o700);
        std_fs::set_permissions(&helper, permissions).unwrap();
        git(
            fixture._root.path(),
            &["config", "diff.external", helper.to_str().unwrap()],
        );
        std_fs::write(fixture._root.path().join("tracked.txt"), "changed\n").unwrap();

        let error = fixture
            .group
            .diff(
                &ScmDiffRequest {
                    version: ContractVersion::V1,
                    binding: fixture.binding.clone(),
                    repository_handle: fixture.repository.handle.clone(),
                    target: ScmDiffTarget::Unstaged,
                    path: None,
                    max_lines: 100,
                    max_bytes: 32_768,
                    cursor: None,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert_eq!(error, ScmError::UnsupportedRepository);
        assert!(!sentinel.exists());
    }

    #[tokio::test]
    async fn status_represents_unmerged_conflicts_without_flattening_them() {
        let fixture = fixture(true).await;
        let root = fixture._root.path();
        git(root, &["checkout", "-b", "other"]);
        std_fs::write(root.join("tracked.txt"), "other\n").unwrap();
        git(root, &["add", "--", "tracked.txt"]);
        git(root, &["commit", "-m", "other"]);
        git(root, &["checkout", "main"]);
        std_fs::write(root.join("tracked.txt"), "main\n").unwrap();
        git(root, &["add", "--", "tracked.txt"]);
        git(root, &["commit", "-m", "main"]);
        let status = StdCommand::new("git")
            .arg("-C")
            .arg(root)
            .args(["merge", "other"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success());
        let status = current_status(&fixture).await;
        let conflict = status
            .entries
            .iter()
            .find(|entry| entry.path.as_str() == "tracked.txt")
            .unwrap();
        assert!(conflict.conflicted);
        assert_eq!(conflict.staged, Some(ScmChangeKind::Unmerged));
        assert_eq!(conflict.unstaged, Some(ScmChangeKind::Unmerged));
    }

    #[tokio::test]
    async fn diff_caps_aggregate_scanned_bytes_before_materializing_every_file() {
        let fixture = fixture(true).await;
        let root = fixture._root.path();
        for index in 0..4 {
            let name = format!("large-{index}.txt");
            std_fs::write(root.join(&name), "before\n").unwrap();
            git(root, &["add", "--", &name]);
        }
        git(root, &["commit", "-m", "large-base"]);
        let line = "x".repeat(4_096);
        let content = format!("{line}\n").repeat(1_280);
        for index in 0..4 {
            std_fs::write(root.join(format!("large-{index}.txt")), &content).unwrap();
        }
        let resource = lock(&fixture.group.state)
            .repositories
            .get(&fixture.repository.handle)
            .unwrap()
            .resource
            .clone();

        let collected = collect_diff(
            fixture.group.git_executable.as_ref(),
            &resource,
            &ScmDiffTarget::Unstaged,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(collected.truncated);
        let retained_text_bytes = collected
            .lines
            .iter()
            .map(|line| line.text.as_str().len())
            .sum::<usize>();
        assert!(retained_text_bytes <= MAX_SCM_DIFF_SCAN_BYTES as usize);
    }

    #[tokio::test]
    async fn diff_caps_aggregate_parsed_lines_before_allocating_the_complete_patch() {
        let fixture = fixture(true).await;
        let root = fixture._root.path();
        std_fs::write(root.join("tracked.txt"), "changed\n".repeat(25_000)).unwrap();
        let resource = lock(&fixture.group.state)
            .repositories
            .get(&fixture.repository.handle)
            .unwrap()
            .resource
            .clone();

        let collected = collect_diff(
            fixture.group.git_executable.as_ref(),
            &resource,
            &ScmDiffTarget::Unstaged,
            Some(&path("tracked.txt")),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(collected.truncated);
        assert!(collected.lines.len() <= MAX_SCM_DIFF_PARSED_LINES as usize + 1);
    }

    #[tokio::test]
    async fn individually_valid_commits_stop_at_the_aggregate_budget_before_body_load() {
        let fixture = fixture(true).await;
        let root = fixture._root.path();
        commit_large_history(root);
        let binding = fixture
            .group
            .repository(&fixture.repository.handle, &fixture.binding)
            .await
            .unwrap();
        let repository = verify_repository(&binding.resource).unwrap();
        let head = head_object_id(&repository).unwrap().unwrap();
        let objects = ObservedCommitStore::new(&repository);

        let collected = collect_log_page(
            &objects,
            head,
            &HashSet::new(),
            0,
            MAX_SCM_LOG_ENTRIES as usize,
            &CancellationToken::new(),
        )
        .unwrap();

        assert!(collected.scan_limit_reached);
        let body_load_count = {
            let header_reads = objects.header_reads.borrow();
            let body_loads = objects.body_loads.borrow();
            assert_eq!(header_reads.len(), body_loads.len() + 1);
            assert_eq!(&header_reads[..body_loads.len()], body_loads.as_slice());
            let scanned_bytes = body_loads
                .iter()
                .map(|id| repository.object_header(id).unwrap().1)
                .sum::<u64>();
            let blocked_bytes = repository
                .object_header(header_reads.last().unwrap())
                .unwrap()
                .1;
            assert!(
                header_reads
                    .iter()
                    .all(|id| repository.object_header(id).unwrap().1 <= MAX_SCM_COMMIT_BYTES)
            );
            assert!(scanned_bytes <= MAX_SCM_LOG_SCAN_BYTES);
            assert!(scanned_bytes.saturating_add(blocked_bytes) > MAX_SCM_LOG_SCAN_BYTES);
            body_loads.len()
        };

        let response = fixture
            .group
            .log(
                &ScmLogRequest {
                    version: ContractVersion::V1,
                    binding: fixture.binding.clone(),
                    repository_handle: fixture.repository.handle.clone(),
                    page_size: MAX_SCM_LOG_ENTRIES,
                    cursor: None,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(response.commits.len(), body_load_count);
        assert!(response.truncated);
        assert!(response.next_cursor.is_none());
    }

    #[tokio::test]
    async fn deep_log_pages_charge_skipped_commits_and_withhold_an_unusable_cursor() {
        let fixture = fixture(true).await;
        commit_large_history(fixture._root.path());
        let binding = fixture
            .group
            .repository(&fixture.repository.handle, &fixture.binding)
            .await
            .unwrap();
        let repository = verify_repository(&binding.resource).unwrap();
        let head = head_object_id(&repository).unwrap().unwrap();
        let fitting_commits = collect_log_page(
            &repository,
            head,
            &HashSet::new(),
            0,
            MAX_SCM_LOG_ENTRIES as usize,
            &CancellationToken::new(),
        )
        .unwrap()
        .commits
        .len();
        let page_size = fitting_commits / 2 + 1;

        let first = fixture
            .group
            .log(
                &ScmLogRequest {
                    version: ContractVersion::V1,
                    binding: fixture.binding.clone(),
                    repository_handle: fixture.repository.handle.clone(),
                    page_size: u32::try_from(page_size).unwrap(),
                    cursor: None,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(first.commits.len(), page_size);
        assert!(first.truncated);
        assert!(first.next_cursor.is_some());

        let deep = fixture
            .group
            .log(
                &ScmLogRequest {
                    version: ContractVersion::V1,
                    binding: fixture.binding.clone(),
                    repository_handle: fixture.repository.handle.clone(),
                    page_size: u32::try_from(page_size).unwrap(),
                    cursor: first.next_cursor,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(deep.commits.len(), fitting_commits - page_size);
        assert!(deep.commits.len() < page_size);
        assert!(deep.truncated);
        assert!(deep.next_cursor.is_none());
    }

    #[tokio::test]
    async fn deep_log_pages_return_only_the_current_page_and_lookahead() {
        let fixture = fixture(true).await;
        let root = fixture._root.path();
        for index in 0..32 {
            git(
                root,
                &[
                    "commit",
                    "--allow-empty",
                    "-m",
                    &format!("commit-{index:02}"),
                ],
            );
        }
        let mut cursor = None;
        for _ in 0..30 {
            let page = fixture
                .group
                .log(
                    &ScmLogRequest {
                        version: ContractVersion::V1,
                        binding: fixture.binding.clone(),
                        repository_handle: fixture.repository.handle.clone(),
                        page_size: 1,
                        cursor,
                    },
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            assert_eq!(page.commits.len(), 1);
            assert!(page.truncated);
            cursor = page.next_cursor;
        }
        let deep = fixture
            .group
            .log(
                &ScmLogRequest {
                    version: ContractVersion::V1,
                    binding: fixture.binding.clone(),
                    repository_handle: fixture.repository.handle.clone(),
                    page_size: 2,
                    cursor,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(deep.commits.len(), 2);
        assert!(deep.commits[0].summary.as_str().starts_with("commit-"));
    }

    #[tokio::test]
    async fn log_stops_at_boundaries_from_the_fixed_shallow_file() {
        let fixture = fixture(true).await;
        let root = fixture._root.path();
        git(root, &["commit", "--allow-empty", "-m", "second"]);
        let boundary = git_text(root, &["rev-parse", "HEAD"]);
        git(root, &["commit", "--allow-empty", "-m", "third"]);
        std_fs::write(root.join(".git/shallow"), format!("{boundary}\n")).unwrap();

        let log = fixture
            .group
            .log(&log_request(&fixture, 10), &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(log.commits.len(), 2);
        assert_eq!(log.commits[0].summary.as_str(), "third");
        assert_eq!(log.commits[1].id.as_str(), boundary);
        assert!(!log.truncated);
    }

    #[tokio::test]
    async fn a_commit_body_is_the_trimmed_message_past_its_subject_and_is_empty_rather_than_absent()
    {
        let fixture = fixture(true).await;
        git(
            fixture._root.path(),
            &[
                "commit",
                "--allow-empty",
                "-m",
                "subject line",
                "-m",
                "first body line\nsecond body line",
            ],
        );

        let log = fixture
            .group
            .log(&log_request(&fixture, 10), &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(log.commits[0].summary.as_str(), "subject line");
        assert_eq!(
            log.commits[0].body.as_ref().unwrap().as_str(),
            "first body line\nsecond body line"
        );
        assert_eq!(log.commits[1].summary.as_str(), "initial");
        assert_eq!(log.commits[1].body.as_ref().unwrap().as_str(), "");
    }

    #[tokio::test]
    async fn log_rejects_oversized_and_malformed_shallow_files() {
        let fixture = fixture(true).await;
        let shallow = fixture._root.path().join(".git/shallow");
        std_fs::write(&shallow, vec![b'x'; MAX_SCM_SHALLOW_BYTES + 1]).unwrap();
        assert_eq!(
            fixture
                .group
                .log(&log_request(&fixture, 1), &CancellationToken::new())
                .await
                .unwrap_err(),
            ScmError::LimitExceeded
        );

        let head = git_text(fixture._root.path(), &["rev-parse", "HEAD"]);
        std_fs::write(
            &shallow,
            format!("{head}\n").repeat(MAX_SCM_SHALLOW_COMMITS + 1),
        )
        .unwrap();
        assert_eq!(
            fixture
                .group
                .log(&log_request(&fixture, 1), &CancellationToken::new())
                .await
                .unwrap_err(),
            ScmError::LimitExceeded
        );

        std_fs::write(&shallow, b"not-an-object-id\n").unwrap();
        assert_eq!(
            fixture
                .group
                .log(&log_request(&fixture, 1), &CancellationToken::new())
                .await
                .unwrap_err(),
            ScmError::RepositoryUnavailable
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shallow_reader_rejects_symlinks_and_special_files() {
        use std::{os::unix::fs::symlink, os::unix::net::UnixListener};

        let fixture = fixture(true).await;
        let binding = fixture
            .group
            .repository(&fixture.repository.handle, &fixture.binding)
            .await
            .unwrap();
        let repository = verify_repository(&binding.resource).unwrap();
        let shallow = binding.resource.git_dir().join(SHALLOW_FILE_NAME);
        let outside = tempfile::tempdir().unwrap();
        let outside_shallow = outside.path().join(SHALLOW_FILE_NAME);
        std_fs::write(
            &outside_shallow,
            format!("{}\n", head_revision(&repository).unwrap().as_str()),
        )
        .unwrap();
        symlink(&outside_shallow, &shallow).unwrap();
        assert_eq!(
            read_shallow_commits(
                &repository,
                binding.resource.git_dir(),
                &CancellationToken::new()
            )
            .unwrap_err(),
            ScmError::UnsupportedRepository
        );

        std_fs::remove_file(&shallow).unwrap();
        let _listener = UnixListener::bind(&shallow).unwrap();
        assert_eq!(
            read_shallow_commits(
                &repository,
                binding.resource.git_dir(),
                &CancellationToken::new()
            )
            .unwrap_err(),
            ScmError::UnsupportedRepository
        );
    }

    #[tokio::test]
    async fn discovery_rejects_out_of_root_shallow_path_overrides() {
        let outside = tempfile::tempdir().unwrap();
        let outside_shallow = outside.path().join(SHALLOW_FILE_NAME);
        std_fs::write(
            &outside_shallow,
            "0000000000000000000000000000000000000000\n",
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        init_repository(root.path());
        std_fs::write(root.path().join("tracked.txt"), "tracked\n").unwrap();
        git(root.path(), &["add", "--", "tracked.txt"]);
        git(root.path(), &["commit", "-m", "initial"]);
        git(
            root.path(),
            &[
                "config",
                "gitoxide.core.shallowFile",
                outside_shallow.to_str().unwrap(),
            ],
        );
        let files = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let cwd = files.workspace_root().await.unwrap();
        let group = ScmGroup::new(files);

        assert_eq!(
            group
                .discover(
                    &ScmDiscoverRequest {
                        version: ContractVersion::V1,
                        binding: WorkspaceRequestBinding {
                            host: host_binding(cwd.handle.clone()),
                            cwd_handle: cwd.handle,
                        },
                        path: path("."),
                    },
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err(),
            ScmError::UnsupportedRepository
        );
    }

    #[tokio::test]
    async fn an_oversized_head_is_rejected_before_the_body_loader_is_called() {
        let fixture = fixture(true).await;
        let root = fixture._root.path();
        commit_oversized_message(root);

        let binding = fixture
            .group
            .repository(&fixture.repository.handle, &fixture.binding)
            .await
            .unwrap();
        let repository = verify_repository(&binding.resource).unwrap();
        let head = head_object_id(&repository).unwrap().unwrap();
        let objects = ObservedCommitStore::new(&repository);
        assert_eq!(
            collect_log_page(
                &objects,
                head,
                &HashSet::new(),
                0,
                1,
                &CancellationToken::new(),
            )
            .unwrap_err(),
            ScmError::LimitExceeded
        );
        assert_eq!(objects.header_reads.borrow().as_slice(), &[head]);
        assert!(objects.body_loads.borrow().is_empty());

        assert_eq!(
            fixture
                .group
                .log(
                    &ScmLogRequest {
                        version: ContractVersion::V1,
                        binding: fixture.binding.clone(),
                        repository_handle: fixture.repository.handle.clone(),
                        page_size: 1,
                        cursor: None,
                    },
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err(),
            ScmError::LimitExceeded
        );
    }

    #[tokio::test]
    async fn an_oversized_ancestor_is_rejected_before_the_body_loader_is_called() {
        let fixture = fixture(true).await;
        let root = fixture._root.path();
        commit_oversized_message(root);
        let oversized = git_text(root, &["rev-parse", "HEAD"]);
        git(root, &["commit", "--allow-empty", "-m", "child"]);

        let binding = fixture
            .group
            .repository(&fixture.repository.handle, &fixture.binding)
            .await
            .unwrap();
        let repository = verify_repository(&binding.resource).unwrap();
        let head = head_object_id(&repository).unwrap().unwrap();
        let objects = ObservedCommitStore::new(&repository);
        assert_eq!(
            collect_log_page(
                &objects,
                head,
                &HashSet::new(),
                0,
                2,
                &CancellationToken::new(),
            )
            .unwrap_err(),
            ScmError::LimitExceeded
        );
        assert_eq!(
            objects
                .header_reads
                .borrow()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec![head.to_string(), oversized]
        );
        assert_eq!(objects.body_loads.borrow().as_slice(), &[head]);

        assert_eq!(
            fixture
                .group
                .log(
                    &ScmLogRequest {
                        version: ContractVersion::V1,
                        binding: fixture.binding.clone(),
                        repository_handle: fixture.repository.handle.clone(),
                        page_size: 2,
                        cursor: None,
                    },
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err(),
            ScmError::LimitExceeded
        );
    }

    fn commit_oversized_message(root: &Path) {
        let message = root.join("oversized-message");
        std_fs::write(
            &message,
            vec![b'x'; usize::try_from(MAX_SCM_COMMIT_BYTES).unwrap() + 1],
        )
        .unwrap();
        git(
            root,
            &["commit", "--allow-empty", "-F", "oversized-message"],
        );
    }

    fn commit_large_history(root: &Path) {
        let message_bytes = usize::try_from(MAX_SCM_COMMIT_BYTES / 2).unwrap();
        let commit_count =
            usize::try_from(MAX_SCM_LOG_SCAN_BYTES / (MAX_SCM_COMMIT_BYTES / 2)).unwrap() + 4;
        std_fs::write(root.join("large-message"), vec![b'x'; message_bytes]).unwrap();
        for _ in 0..commit_count {
            git(root, &["commit", "--allow-empty", "-F", "large-message"]);
        }
    }

    async fn fixture(allow_write: bool) -> Fixture {
        let root = tempfile::tempdir().unwrap();
        init_repository(root.path());
        std_fs::write(root.path().join("tracked.txt"), "one\ntwo\nthree\n").unwrap();
        git(root.path(), &["add", "--", "tracked.txt"]);
        git(root.path(), &["commit", "-m", "initial"]);
        let files = FileToolGroup::new(root.path(), allow_write, None)
            .await
            .unwrap();
        let cwd = files.workspace_root().await.unwrap();
        let binding = WorkspaceRequestBinding {
            host: host_binding(cwd.handle.clone()),
            cwd_handle: cwd.handle,
        };
        let group = ScmGroup::new(files);
        let repository = group
            .discover(
                &ScmDiscoverRequest {
                    version: ContractVersion::V1,
                    binding: binding.clone(),
                    path: path("."),
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap()
            .repository;
        Fixture {
            _root: root,
            group,
            binding,
            repository,
        }
    }

    fn init_repository(path: &Path) {
        git(path, &["init", "--quiet", "--initial-branch=main"]);
        git(path, &["config", "user.name", "Workcell Test"]);
        git(path, &["config", "user.email", "workcell@example.invalid"]);
    }

    fn git(path: &Path, arguments: &[&str]) {
        let status = StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git command failed: {arguments:?}");
    }

    fn git_text(path: &Path, arguments: &[&str]) -> String {
        let output = StdCommand::new("git")
            .arg("-C")
            .arg(path)
            .args(arguments)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "git command failed: {arguments:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn host_binding(cwd_handle: ResourceId) -> HostBinding {
        HostBinding {
            server_id: Identifier::new("server").unwrap(),
            instance_id: Identifier::new("instance").unwrap(),
            workspace_id: Identifier::new("workspace").unwrap(),
            workspace_generation: Identifier::new("generation").unwrap(),
            root_project_id: Identifier::new("project").unwrap(),
            principal_id: Identifier::new("principal").unwrap(),
            cwd_handle,
            catalog_revision: Revision::new("sha256:catalog").unwrap(),
            policy_revision: Revision::new("sha256:policy").unwrap(),
        }
    }

    fn path(value: &str) -> WorkspacePath {
        WorkspacePath::new(value).unwrap()
    }

    fn status_request(
        fixture: &Fixture,
        page_size: u32,
        cursor: Option<Cursor>,
    ) -> ScmStatusRequest {
        ScmStatusRequest {
            version: ContractVersion::V1,
            binding: fixture.binding.clone(),
            repository_handle: fixture.repository.handle.clone(),
            page_size,
            cursor,
        }
    }

    fn log_request(fixture: &Fixture, page_size: u32) -> ScmLogRequest {
        ScmLogRequest {
            version: ContractVersion::V1,
            binding: fixture.binding.clone(),
            repository_handle: fixture.repository.handle.clone(),
            page_size,
            cursor: None,
        }
    }

    fn side_request(fixture: &Fixture, side: ScmSide) -> ScmReadSideRequest {
        ScmReadSideRequest {
            version: ContractVersion::V1,
            binding: fixture.binding.clone(),
            repository_handle: fixture.repository.handle.clone(),
            path: path("tracked.txt"),
            side,
            start_line: 1,
            max_lines: 100,
            max_bytes: 32_768,
        }
    }

    async fn current_status(fixture: &Fixture) -> ScmStatusResponse {
        fixture
            .group
            .status(
                &status_request(fixture, MAX_SCM_STATUS_ENTRIES, None),
                &CancellationToken::new(),
            )
            .await
            .unwrap()
    }
}
