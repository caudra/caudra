use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::Metadata,
    io,
    mem::size_of,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU8, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    time::{Duration, Instant},
};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher, event::ModifyKind};
#[cfg(target_os = "linux")]
use rustix::fs::{ResolveFlags, openat2};
#[cfg(unix)]
use rustix::{
    fs::{Dir, Mode, OFlags, open},
    io::Errno,
};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::{ffi::OsStr, fs::File, os::unix::ffi::OsStrExt};
use tokio::sync::{Notify, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio::{fs, io::AsyncReadExt};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workcell_host_contract::{
    ContractVersion, Cursor, DirectoryNavigation, DiscoverProjectAssetsRequest,
    DiscoverProjectAssetsResponse, Identifier, ListRequest, ListResponse, MAX_PAGE_SIZE,
    MAX_PROJECT_ASSET_DISCOVERY_HASH_BYTES, MAX_PROJECT_ASSET_READ_BYTES, MAX_PROJECT_ASSETS,
    MAX_TEXT_READ_BYTES, MAX_WORKSPACE_LIST_ENTRIES, MAX_WORKSPACE_LIST_RETAINED_BYTES,
    PROJECT_ASSET_MANIFEST_VERSION, ProjectAsset, ProjectAssetContent, ProjectAssetEncoding,
    ProjectAssetKind, ProjectAssetManifest, ProjectAssetTrust, ReadProjectAssetRequest,
    ReadProjectAssetResponse, ReadTextRequest, ReadTextResponse, ResourceId, Revision,
    SearchTextRequest, SearchTextResponse, StatRequest, StatResponse, TextSearchMatch,
    WatchEventKind, WatchOpenRequest, WorkspaceDirectory, WorkspaceEntry, WorkspaceEntryKind,
    WorkspaceMutation, WorkspaceMutationKind, WorkspaceMutationResponse, WorkspaceMutationResult,
    WorkspacePath,
};

#[cfg(unix)]
use crate::binary::DIRECTORY_FLAGS;
use crate::{
    FileGrepInput, FileResource, FileResourceAccess, FileToolGroup, FilesystemError,
    SnapshotTreeStamp,
    operations::FilesystemCore,
    text::{
        FileVersion, check_cancelled, read_file_version_required, read_text_snapshot_required,
        split_text_lines, validate_snapshot,
    },
};

const MAX_CWD_HANDLES: usize = 256;
const MAX_CURSORS: usize = 256;
const LIST_REVISION_NAMESPACE: &str = "inventory:";
const LIST_CURSOR_PREFIX: &str = "list";
const MAX_LIST_WORKERS: usize = 4;
const MAX_LIST_INVENTORIES: usize = 16;
const MAX_LIST_INVENTORY_ENTRIES: usize = 200_000;
const MAX_LIST_INVENTORY_BYTES: usize = 128 * 1_024 * 1_024;
const LIST_INVENTORY_RESERVATION_BYTES: usize = 2 * MAX_WORKSPACE_LIST_RETAINED_BYTES as usize;
const LIST_INVENTORY_TTL: Duration = Duration::from_secs(30);
const LIST_CAPACITY_MESSAGE: &str =
    "Workspace listing inventory capacity is exhausted; retry after expiry";
const PROJECT_ASSET_HASH_BUFFER_BYTES: usize = 64 * 1_024;
#[cfg(unix)]
const WORKSPACE_METADATA_FLAGS: OFlags =
    OFlags::PATH.union(OFlags::NOFOLLOW).union(OFlags::CLOEXEC);
#[cfg(unix)]
const LIST_SEARCH_FLAGS: OFlags = WORKSPACE_METADATA_FLAGS.union(OFlags::DIRECTORY);
#[cfg(target_os = "linux")]
const LIST_RESOLVE_FLAGS: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_MAGICLINKS);
const MAX_GIT_METADATA_ENTRIES: usize = 200_000;
const WATCH_BACKEND_QUEUE: usize = 256;
const MAX_WATCH_BACKEND_PATHS: usize = 16;
const MAX_WATCH_BACKEND_EVENT_BYTES: usize = 64 * 1_024;
const WATCH_FAILURE_NONE: u8 = 0;
const WATCH_FAILURE_OVERFLOW: u8 = 1;
const WATCH_FAILURE_BACKEND: u8 = 2;
const INSTRUCTION_FILES: &[&str] = &[
    "AGENTS.md",
    "AGENTS.local.md",
    "CLAUDE.md",
    "COPILOT.md",
    ".cursorrules",
    ".windsurfrules",
    ".clinerules",
    "CONVENTIONS.md",
    "GEMINI.md",
    "CODING_AGENT.md",
];
const SKILL_ROOTS: &[&str] = &[".caudra", ".claude", ".opencode", ".agents"];
const COMMAND_PARENTS: &[(&str, &str)] = &[
    (".caudra", "commands"),
    (".claude", "commands"),
    (".opencode", "commands"),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootResourceKind {
    Path,
    Repository,
}

impl RootResourceKind {
    const fn namespace(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Repository => "repository",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("workspace request is invalid")]
    InvalidRequest,
    #[error("workspace cwd handle is unknown or stale")]
    StaleCwd,
    #[error("workspace cursor is invalid")]
    InvalidCursor,
    #[error("workspace cursor is stale")]
    StaleCursor,
    #[error("workspace resource revision is stale")]
    StaleResource,
    #[error("workspace watch backend is unavailable")]
    WatchUnavailable {
        phase: WorkspaceWatchPhase,
        kind: WorkspaceWatchErrorKind,
        io_kind: Option<io::ErrorKind>,
        raw_os_error: Option<i32>,
    },
    #[error("workspace path is not in a Git repository")]
    NotRepository,
    #[error("no supported repository is available in the workspace")]
    RepositoryUnavailable,
    #[error("workspace file exceeds the configured content size limit of {maximum} bytes")]
    FileTooLarge { maximum: usize },
    #[error("linked worktrees, submodules, and external git directories are unsupported")]
    UnsupportedRepository,
    #[error("workspace mutation was rolled back: {0}")]
    RolledBack(String),
    #[error("workspace mutation failed and rollback was incomplete: {0}")]
    PartialFailure(String),
    #[error(transparent)]
    Filesystem(#[from] FilesystemError),
}

#[derive(Clone, Debug)]
pub struct WorkspaceRepositoryResource {
    core: Arc<FilesystemCore>,
    worktree: PathBuf,
    git_dir: PathBuf,
    relative_path: String,
    resource_id: ResourceId,
}

#[derive(Clone, Debug)]
pub struct WorkspaceResolvedPath {
    path: PathBuf,
    relative_to_repository: WorkspacePath,
}

#[derive(Clone, Debug)]
pub struct WorkspaceSnapshotAccess {
    pub(crate) core: Arc<FilesystemCore>,
}

#[derive(Clone, Debug)]
pub struct WorkspaceSnapshotScope {
    pub(crate) path: String,
    pub(crate) revision: Revision,
}

impl WorkspaceSnapshotScope {
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>() + self.path.capacity() + self.revision.as_str().len()
    }
}

impl WorkspaceSnapshotAccess {
    pub async fn snapshot_scope(
        &self,
        path: &WorkspacePath,
    ) -> Result<WorkspaceSnapshotScope, WorkspaceError> {
        let resolved = self.resolve(path).await?;
        Ok(WorkspaceSnapshotScope {
            path: path.as_str().to_owned(),
            revision: directory_revision(&resolved).await?,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        self.core.root()
    }

    #[must_use]
    pub fn allow_write(&self) -> bool {
        self.core.allow_write
    }

    #[must_use]
    pub fn allows_canonical_entry(&self, path: &Path) -> bool {
        self.core.policy.authorize_canonical_entry(path)
    }

    pub async fn resolve(&self, path: &WorkspacePath) -> Result<PathBuf, WorkspaceError> {
        self.core
            .policy
            .resolve(path.as_str())
            .await
            .map_err(WorkspaceError::from)
    }

    pub async fn revalidate(&self, path: &Path) -> Result<PathBuf, WorkspaceError> {
        self.core
            .policy
            .revalidate(path)
            .await
            .map_err(WorkspaceError::from)
    }

    pub async fn mutation_guard(&self) -> Result<OwnedMutexGuard<()>, WorkspaceError> {
        self.core.require_write()?;
        Ok(self.core.mutation.clone().lock_owned().await)
    }

    pub async fn capture_guard(&self) -> OwnedMutexGuard<()> {
        self.core.mutation.clone().lock_owned().await
    }
}

impl WorkspaceRepositoryResource {
    /// Conservative retained bytes, excluding the shared filesystem core.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.worktree.capacity())
            .saturating_add(self.git_dir.capacity())
            .saturating_add(self.relative_path.capacity())
            .saturating_add(self.resource_id.as_str().len().saturating_mul(2))
    }

    #[must_use]
    pub fn worktree(&self) -> &Path {
        &self.worktree
    }

    #[must_use]
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    #[must_use]
    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }

    #[must_use]
    pub const fn resource_id(&self) -> &ResourceId {
        &self.resource_id
    }

    pub fn path_resource_id(&self, path: &WorkspacePath) -> Result<ResourceId, WorkspaceError> {
        let root_relative_path = match (self.relative_path.as_str(), path.as_str()) {
            (".", path) => path.to_owned(),
            (root, ".") => root.to_owned(),
            (root, path) => format!("{root}/{path}"),
        };
        root_relative_resource_id(RootResourceKind::Path, &root_relative_path)
    }

    pub fn resource_scope(&self) -> Result<Vec<ResourceId>, WorkspaceError> {
        let mut scope = root_relative_resource_scope(RootResourceKind::Path, &self.relative_path)?;
        if scope.len() >= workcell_host_contract::MAX_RESOURCE_SCOPE_DEPTH {
            return Err(WorkspaceError::InvalidRequest);
        }
        scope.push(self.resource_id.clone());
        Ok(scope)
    }

    pub fn path_resource_scope(
        &self,
        path: &WorkspacePath,
    ) -> Result<Vec<ResourceId>, WorkspaceError> {
        let root_relative_path = match (self.relative_path.as_str(), path.as_str()) {
            (".", path) => path.to_owned(),
            (root, ".") => root.to_owned(),
            (root, path) => format!("{root}/{path}"),
        };
        root_relative_resource_scope(RootResourceKind::Path, &root_relative_path)
    }

    #[must_use]
    pub fn allow_write(&self) -> bool {
        self.core.allow_write
    }

    pub async fn revalidate(&self) -> Result<(), WorkspaceError> {
        let worktree = self
            .core
            .policy
            .resolve(&self.relative_path)
            .await
            .map_err(|_| WorkspaceError::StaleResource)?;
        let git_dir = self
            .core
            .policy
            .resolve_internal_existing(&worktree.join(".git"))
            .await
            .map_err(|_| WorkspaceError::StaleResource)?;
        let metadata = fs::symlink_metadata(worktree.join(".git"))
            .await
            .map_err(|_| WorkspaceError::StaleResource)?;
        if worktree != self.worktree
            || git_dir != self.git_dir
            || !metadata.file_type().is_dir()
            || metadata.file_type().is_symlink()
        {
            return Err(WorkspaceError::StaleResource);
        }
        validate_repository_storage(&self.core, &worktree, &git_dir)
            .await
            .map_err(|_| WorkspaceError::StaleResource)?;
        Ok(())
    }

    pub async fn resolve_path(
        &self,
        requested: &WorkspacePath,
    ) -> Result<WorkspaceResolvedPath, WorkspaceError> {
        let joined = if self.relative_path == "." {
            requested.as_str().to_owned()
        } else {
            format!("{}/{}", self.relative_path, requested.as_str())
        };
        let path = self.core.policy.resolve(&joined).await?;
        let relative = path
            .strip_prefix(&self.worktree)
            .map_err(|_| WorkspaceError::InvalidRequest)?;
        if relative.as_os_str().is_empty() {
            return Err(WorkspaceError::InvalidRequest);
        }
        let relative_to_repository = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        Ok(WorkspaceResolvedPath {
            path,
            relative_to_repository: WorkspacePath::new(relative_to_repository)
                .map_err(|_| WorkspaceError::InvalidRequest)?,
        })
    }

    pub async fn mutation_guard(&self) -> Result<OwnedMutexGuard<()>, WorkspaceError> {
        if !self.core.allow_write {
            return Err(WorkspaceError::InvalidRequest);
        }
        Ok(self.core.mutation.clone().lock_owned().await)
    }
}

impl WorkspaceResolvedPath {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn relative_to_repository(&self) -> &WorkspacePath {
        &self.relative_to_repository
    }
}

fn reserve_path(paths: &mut HashSet<String>, path: &str) -> Result<(), WorkspaceError> {
    if path == "." || !paths.insert(path.to_owned()) {
        return Err(WorkspaceError::InvalidRequest);
    }
    Ok(())
}

fn workspace_path(path: String) -> Result<WorkspacePath, WorkspaceError> {
    WorkspacePath::new(path).map_err(|_| WorkspaceError::InvalidRequest)
}

fn file_resource(path: &Path, relative: &str, access: FileResourceAccess) -> FileResource {
    FileResource {
        requested_path: relative.to_owned(),
        path: path.to_path_buf(),
        root_relative_path: relative.to_owned(),
        access,
    }
}

async fn require_missing(path: &Path) -> Result<(), WorkspaceError> {
    match fs::symlink_metadata(path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(FilesystemError::io_path("Cannot inspect workspace path", path, error).into())
        }
        Ok(_) => Err(WorkspaceError::StaleResource),
    }
}

async fn require_parent(path: &Path) -> Result<(), WorkspaceError> {
    let parent = path.parent().ok_or(WorkspaceError::InvalidRequest)?;
    let metadata = fs::metadata(parent).await.map_err(|error| {
        FilesystemError::io_path("Cannot inspect workspace parent", parent, error)
    })?;
    if !metadata.is_dir() {
        return Err(WorkspaceError::InvalidRequest);
    }
    Ok(())
}

fn require_revision(version: &FileVersion, expected: &Revision) -> Result<(), WorkspaceError> {
    if version.revision() != expected.as_str() {
        return Err(WorkspaceError::StaleResource);
    }
    Ok(())
}

async fn validate_change(
    core: &FilesystemCore,
    change: &PreparedChange,
    token: &CancellationToken,
) -> Result<(), WorkspaceError> {
    check_cancelled(token)?;
    match change {
        PreparedChange::Create { path, relative, .. }
        | PreparedChange::Mkdir { path, relative } => {
            core.validate_prepared_path(relative.as_str(), path).await?;
            require_missing(path).await?;
            require_parent(path).await
        }
        PreparedChange::Write {
            path,
            relative,
            expected,
            ..
        }
        | PreparedChange::Delete {
            path,
            relative,
            expected,
        } => {
            core.validate_prepared_path(relative.as_str(), path).await?;
            validate_snapshot(path, expected, core.limits.max_file_bytes, token)
                .await
                .map_err(|_| WorkspaceError::StaleResource)
        }
        PreparedChange::Rename {
            source,
            destination,
            source_relative,
            destination_relative,
            expected,
        } => {
            core.validate_prepared_path(source_relative.as_str(), source)
                .await?;
            core.validate_prepared_path(destination_relative.as_str(), destination)
                .await?;
            validate_snapshot(source, expected, core.limits.max_file_bytes, token)
                .await
                .map_err(|_| WorkspaceError::StaleResource)?;
            require_missing(destination).await?;
            require_parent(destination).await
        }
    }
}

async fn apply_change(
    core: &FilesystemCore,
    change: &PreparedChange,
    token: &CancellationToken,
    rollback: &mut Vec<Rollback>,
    results: &mut Vec<WorkspaceMutationResult>,
) -> Result<(), WorkspaceError> {
    check_cancelled(token)?;
    match change {
        PreparedChange::Create {
            path,
            relative,
            content,
        } => {
            core.commit_write(relative.as_str(), path, content, token, true, None)
                .await?;
            rollback.push(Rollback::RemoveFile(path.clone()));
            results.push(mutation_result(
                WorkspaceMutationKind::Create,
                relative,
                None,
                Some(file_revision(path, core.limits.max_file_bytes, token).await?),
            ));
        }
        PreparedChange::Write {
            path,
            relative,
            content,
            old_content,
            expected,
        } => {
            core.commit_write(
                relative.as_str(),
                path,
                content,
                token,
                false,
                Some(expected),
            )
            .await?;
            rollback.push(Rollback::RestoreFile {
                path: path.clone(),
                content: old_content.clone(),
            });
            results.push(mutation_result(
                WorkspaceMutationKind::Write,
                relative,
                None,
                Some(file_revision(path, core.limits.max_file_bytes, token).await?),
            ));
        }
        PreparedChange::Mkdir { path, relative } => {
            fs::create_dir(path).await.map_err(|error| {
                FilesystemError::io_path("Cannot create workspace directory", path, error)
            })?;
            rollback.push(Rollback::RemoveDirectory(path.clone()));
            results.push(mutation_result(
                WorkspaceMutationKind::Mkdir,
                relative,
                None,
                Some(directory_revision(path).await?),
            ));
        }
        PreparedChange::Rename {
            source,
            destination,
            source_relative,
            destination_relative,
            ..
        } => {
            fs::rename(source, destination).await.map_err(|error| {
                FilesystemError::io_path("Cannot rename workspace file", source, error)
            })?;
            rollback.push(Rollback::Rename {
                from: destination.clone(),
                to: source.clone(),
            });
            results.push(mutation_result(
                WorkspaceMutationKind::Rename,
                source_relative,
                Some(destination_relative.clone()),
                Some(file_revision(destination, core.limits.max_file_bytes, token).await?),
            ));
        }
        PreparedChange::Delete { path, relative, .. } => {
            let parent = path.parent().ok_or(WorkspaceError::InvalidRequest)?;
            let staged = parent.join(format!(".workcell-delete-{}", Uuid::new_v4()));
            fs::rename(path, &staged).await.map_err(|error| {
                FilesystemError::io_path("Cannot stage workspace deletion", path, error)
            })?;
            rollback.push(Rollback::RestoreDelete {
                staged,
                path: path.clone(),
            });
            results.push(mutation_result(
                WorkspaceMutationKind::Delete,
                relative,
                None,
                None,
            ));
        }
    }
    Ok(())
}

fn mutation_result(
    kind: WorkspaceMutationKind,
    path: &WorkspacePath,
    destination: Option<WorkspacePath>,
    revision: Option<Revision>,
) -> WorkspaceMutationResult {
    WorkspaceMutationResult {
        kind,
        path: path.clone(),
        destination,
        revision,
    }
}

async fn rollback_after_error(
    core: &FilesystemCore,
    rollback: Vec<Rollback>,
    error: WorkspaceError,
) -> WorkspaceError {
    let mut failed = false;
    for action in rollback.into_iter().rev() {
        failed |= rollback_action(core, action).await.is_err();
    }
    if failed {
        WorkspaceError::PartialFailure(error.to_string())
    } else {
        WorkspaceError::RolledBack(error.to_string())
    }
}

async fn rollback_action(core: &FilesystemCore, action: Rollback) -> Result<(), WorkspaceError> {
    match action {
        Rollback::RemoveFile(path) => fs::remove_file(&path).await.map_err(|error| {
            FilesystemError::io_path("Cannot roll back created file", &path, error).into()
        }),
        Rollback::RestoreFile { path, content } => {
            let relative = core.policy.relative(&path)?;
            let current = read_text_snapshot_required(
                &path,
                core.limits.max_file_bytes,
                &CancellationToken::new(),
            )
            .await?;
            core.commit_write(
                &relative,
                &path,
                &content,
                &CancellationToken::new(),
                false,
                Some(&current.version),
            )
            .await?;
            Ok(())
        }
        Rollback::RemoveDirectory(path) => fs::remove_dir(&path).await.map_err(|error| {
            FilesystemError::io_path("Cannot roll back directory", &path, error).into()
        }),
        Rollback::Rename { from, to } => fs::rename(&from, &to).await.map_err(|error| {
            FilesystemError::io_path("Cannot roll back rename", &from, error).into()
        }),
        Rollback::RestoreDelete { staged, path } => {
            fs::rename(&staged, &path).await.map_err(|error| {
                FilesystemError::io_path("Cannot roll back deletion", &staged, error).into()
            })
        }
    }
}

impl WorkspaceError {
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::StaleCwd => "stale_cwd",
            Self::InvalidCursor => "invalid_cursor",
            Self::StaleCursor => "stale_cursor",
            Self::StaleResource => "stale_resource",
            Self::WatchUnavailable { .. } => "watch_unavailable",
            Self::NotRepository => "not_repository",
            Self::RepositoryUnavailable => "repository_unavailable",
            Self::FileTooLarge { .. } => "file_too_large",
            Self::UnsupportedRepository => "unsupported_repository",
            Self::RolledBack(_) => "rolled_back",
            Self::PartialFailure(_) => "partial_failure",
            Self::Filesystem(error) => error.code(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceWatchPhase {
    Initialize,
    Register,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceWatchErrorKind {
    Generic,
    Io,
    PathNotFound,
    WatchNotFound,
    InvalidConfig,
    MaxFilesWatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceWatchFailure {
    Overflow,
    Backend,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceWatchEvent {
    pub kind: WatchEventKind,
    pub path: WorkspacePath,
}

#[derive(Debug, Default)]
pub struct WorkspaceWatchBatch {
    pub events: Vec<WorkspaceWatchEvent>,
    pub failure: Option<WorkspaceWatchFailure>,
}

#[derive(Debug)]
struct WorkspaceListEntries {
    entries: Vec<WorkspaceEntry>,
    truncated: bool,
    incomplete: bool,
}

enum WorkspaceWatchSignal {
    Event(Event),
}

pub struct WorkspaceWatcher {
    _watcher: RecommendedWatcher,
    receiver: Mutex<Receiver<WorkspaceWatchSignal>>,
    notification: Arc<Notify>,
    failure: Arc<AtomicU8>,
    core: Arc<FilesystemCore>,
    scope: PathBuf,
    scope_relative: WorkspacePath,
}

impl std::fmt::Debug for WorkspaceWatcher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceWatcher")
            .field("scope_relative", &self.scope_relative)
            .finish_non_exhaustive()
    }
}

impl WorkspaceWatcher {
    pub async fn poll(&self, wait: Duration) -> WorkspaceWatchBatch {
        let notified = self.notification.notified();
        let batch = self.drain();
        if batch.failure.is_some() || !batch.events.is_empty() || wait.is_zero() {
            return batch;
        }
        let _ = tokio::time::timeout(wait, notified).await;
        self.drain()
    }

    fn drain(&self) -> WorkspaceWatchBatch {
        let failure = self.failure.swap(WATCH_FAILURE_NONE, Ordering::AcqRel);
        if failure != WATCH_FAILURE_NONE {
            return WorkspaceWatchBatch {
                events: Vec::new(),
                failure: Some(if failure == WATCH_FAILURE_OVERFLOW {
                    WorkspaceWatchFailure::Overflow
                } else {
                    WorkspaceWatchFailure::Backend
                }),
            };
        }
        let receiver = self
            .receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut events = Vec::new();
        while let Ok(WorkspaceWatchSignal::Event(event)) = receiver.try_recv() {
            match self.normalize(event) {
                Ok(mut normalized) => events.append(&mut normalized),
                Err(failure) => {
                    return WorkspaceWatchBatch {
                        events: Vec::new(),
                        failure: Some(failure),
                    };
                }
            }
        }
        WorkspaceWatchBatch {
            events,
            failure: None,
        }
    }

    fn normalize(&self, event: Event) -> Result<Vec<WorkspaceWatchEvent>, WorkspaceWatchFailure> {
        if event.need_rescan() {
            return Err(WorkspaceWatchFailure::Overflow);
        }
        let mut normalized = Vec::new();
        match event.kind {
            EventKind::Access(_) => {}
            EventKind::Create(_) => {
                self.push_paths(&mut normalized, WatchEventKind::Create, &event.paths)?
            }
            EventKind::Remove(_) => {
                self.push_paths(&mut normalized, WatchEventKind::Remove, &event.paths)?
            }
            EventKind::Modify(ModifyKind::Name(mode)) => {
                use notify::event::RenameMode;

                match mode {
                    RenameMode::Both if event.paths.len() >= 2 => {
                        self.push_paths(
                            &mut normalized,
                            WatchEventKind::Remove,
                            &event.paths[..1],
                        )?;
                        self.push_paths(
                            &mut normalized,
                            WatchEventKind::Create,
                            &event.paths[1..],
                        )?;
                    }
                    RenameMode::From => {
                        self.push_paths(&mut normalized, WatchEventKind::Remove, &event.paths)?
                    }
                    RenameMode::To => {
                        self.push_paths(&mut normalized, WatchEventKind::Create, &event.paths)?
                    }
                    RenameMode::Any | RenameMode::Other | RenameMode::Both => {
                        normalized.push(WorkspaceWatchEvent {
                            kind: WatchEventKind::Rescan,
                            path: self.scope_relative.clone(),
                        });
                    }
                }
            }
            EventKind::Modify(_) => {
                self.push_paths(&mut normalized, WatchEventKind::Modify, &event.paths)?
            }
            EventKind::Any | EventKind::Other => normalized.push(WorkspaceWatchEvent {
                kind: WatchEventKind::Rescan,
                path: self.scope_relative.clone(),
            }),
        }
        Ok(normalized)
    }

    fn push_paths(
        &self,
        events: &mut Vec<WorkspaceWatchEvent>,
        kind: WatchEventKind,
        paths: &[PathBuf],
    ) -> Result<(), WorkspaceWatchFailure> {
        if paths.is_empty() {
            events.push(WorkspaceWatchEvent {
                kind: WatchEventKind::Rescan,
                path: self.scope_relative.clone(),
            });
            return Ok(());
        }
        for path in paths {
            if !path.starts_with(&self.scope) || !path.starts_with(self.core.root()) {
                return Err(WorkspaceWatchFailure::Backend);
            }
            if !self.core.policy.authorize_canonical_entry(path) {
                continue;
            }
            let relative = self
                .core
                .policy
                .relative(path)
                .map_err(|_| WorkspaceWatchFailure::Backend)?;
            let path = WorkspacePath::new(relative).map_err(|_| WorkspaceWatchFailure::Backend)?;
            events.push(WorkspaceWatchEvent { kind, path });
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct WorkspaceState {
    inner: Mutex<WorkspaceStateInner>,
    list_workers: Arc<Semaphore>,
    list_slots: Arc<Semaphore>,
    list_entries: Arc<Semaphore>,
    list_bytes: Arc<Semaphore>,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self {
            inner: Mutex::default(),
            list_workers: Arc::new(Semaphore::new(MAX_LIST_WORKERS)),
            list_slots: Arc::new(Semaphore::new(MAX_LIST_INVENTORIES)),
            list_entries: Arc::new(Semaphore::new(MAX_LIST_INVENTORY_ENTRIES)),
            list_bytes: Arc::new(Semaphore::new(MAX_LIST_INVENTORY_BYTES)),
        }
    }
}

impl WorkspaceState {
    fn reserve_listing(&self) -> Result<ListingReservation, WorkspaceError> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .expire_listings(Instant::now());
        Ok(ListingReservation {
            _slot: self
                .list_slots
                .clone()
                .try_acquire_owned()
                .map_err(|_| listing_capacity())?,
            entries: self
                .list_entries
                .clone()
                .try_acquire_many_owned(MAX_WORKSPACE_LIST_ENTRIES)
                .map_err(|_| listing_capacity())?,
            bytes: self
                .list_bytes
                .clone()
                .try_acquire_many_owned(LIST_INVENTORY_RESERVATION_BYTES as u32)
                .map_err(|_| listing_capacity())?,
        })
    }

    fn listing_page(
        &self,
        cursor: &Cursor,
        request_digest: &Revision,
        scope_revision: &Revision,
    ) -> Result<ListResponse, WorkspaceError> {
        let mut parts = cursor.as_str().splitn(4, '_');
        if parts.next() != Some(LIST_CURSOR_PREFIX) {
            return Err(WorkspaceError::StaleCursor);
        }
        let id = parts
            .next()
            .and_then(|id| Uuid::parse_str(id).ok())
            .ok_or(WorkspaceError::StaleCursor)?;
        let offset = parts
            .next()
            .and_then(|offset| offset.parse::<usize>().ok())
            .ok_or(WorkspaceError::StaleCursor)?;
        let inventory = {
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.expire_listings(Instant::now());
            state
                .listings
                .get(&id)
                .ok_or(WorkspaceError::StaleCursor)?
                .inventory
                .clone()
        };
        if inventory.request_digest != *request_digest
            || offset == 0
            || offset >= inventory.listed.entries.len()
            || !offset.is_multiple_of(inventory.page_size)
            || inventory.cursor(offset)? != *cursor
        {
            return Err(WorkspaceError::InvalidCursor);
        }
        if inventory.scope_revision != *scope_revision {
            return Err(WorkspaceError::StaleCursor);
        }
        inventory.page(offset)
    }

    fn retain_listing(
        self: &Arc<Self>,
        inventory: Arc<ListingInventory>,
    ) -> Result<(), WorkspaceError> {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.expire_listings(Instant::now());
        if state.listings.contains_key(&inventory.id) {
            return Err(WorkspaceError::InvalidRequest);
        }
        let expiry = tokio::spawn(expire_listing(
            Arc::downgrade(self),
            inventory.id,
            inventory.expires_at,
        ));
        state
            .listings
            .insert(inventory.id, ListingRecord { inventory, expiry });
        Ok(())
    }
}

#[derive(Debug, Default)]
struct WorkspaceStateInner {
    directories: HashMap<ResourceId, DirectoryBinding>,
    cursors: HashMap<Cursor, CursorBinding>,
    cursor_order: VecDeque<Cursor>,
    listings: HashMap<Uuid, ListingRecord>,
}

impl WorkspaceStateInner {
    fn expire_listings(&mut self, now: Instant) {
        self.listings
            .retain(|_, record| record.inventory.expires_at > now);
    }
}

#[derive(Debug)]
struct ListingReservation {
    _slot: OwnedSemaphorePermit,
    entries: OwnedSemaphorePermit,
    bytes: OwnedSemaphorePermit,
}

#[derive(Debug)]
struct ListingInventory {
    listed: WorkspaceListEntries,
    id: Uuid,
    nonce: Uuid,
    request_digest: Revision,
    scope_revision: Revision,
    revision: Revision,
    page_size: usize,
    expires_at: Instant,
    _reservation: ListingReservation,
}

impl ListingInventory {
    fn cursor(&self, offset: usize) -> Result<Cursor, WorkspaceError> {
        let proof = digest_parts(&[
            self.nonce.as_simple().to_string().as_str(),
            &offset.to_string(),
        ])?;
        Cursor::new(format!(
            "{LIST_CURSOR_PREFIX}_{}_{offset}_{}",
            self.id.as_simple(),
            proof.as_str()
        ))
        .map_err(|_| WorkspaceError::InvalidRequest)
    }

    fn page(&self, offset: usize) -> Result<ListResponse, WorkspaceError> {
        if self.expires_at <= Instant::now() {
            return Err(WorkspaceError::StaleCursor);
        }
        let end = offset
            .saturating_add(self.page_size)
            .min(self.listed.entries.len());
        Ok(ListResponse {
            version: ContractVersion::V1,
            revision: self.revision.clone(),
            entries: self
                .listed
                .entries
                .get(offset..end)
                .ok_or(WorkspaceError::InvalidCursor)?
                .to_vec(),
            truncated: self.listed.truncated,
            incomplete: self.listed.incomplete,
            next_cursor: (end < self.listed.entries.len())
                .then(|| self.cursor(end))
                .transpose()?,
        })
    }
}

#[derive(Debug)]
struct ListingRecord {
    inventory: Arc<ListingInventory>,
    expiry: JoinHandle<()>,
}

impl Drop for ListingRecord {
    fn drop(&mut self) {
        self.expiry.abort();
    }
}

#[derive(Clone, Debug)]
struct DirectoryBinding {
    path: PathBuf,
    relative_path: String,
    revision: Revision,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CursorKind {
    Search,
}

#[derive(Debug)]
struct CursorBinding {
    kind: CursorKind,
    request_digest: Revision,
    result_revision: Revision,
    offset: usize,
}

pub struct PreparedWorkspaceMutation {
    core: Arc<FilesystemCore>,
    changes: Vec<PreparedChange>,
    resources: Vec<FileResource>,
    resource_revisions: Vec<Option<Revision>>,
}

impl PreparedWorkspaceMutation {
    #[must_use]
    pub fn resources(&self) -> &[FileResource] {
        &self.resources
    }

    #[must_use]
    pub fn resource_revisions(&self) -> &[Option<Revision>] {
        &self.resource_revisions
    }

    /// Conservative retained bytes, excluding the shared filesystem core.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(
                self.changes
                    .capacity()
                    .saturating_mul(size_of::<PreparedChange>()),
            )
            .saturating_add(
                self.changes
                    .iter()
                    .map(PreparedChange::retained_bytes)
                    .fold(0, usize::saturating_add),
            )
            .saturating_add(
                self.resources
                    .capacity()
                    .saturating_mul(size_of::<FileResource>()),
            )
            .saturating_add(
                self.resources
                    .iter()
                    .map(FileResource::retained_bytes)
                    .fold(0, usize::saturating_add),
            )
            .saturating_add(
                self.resource_revisions
                    .capacity()
                    .saturating_mul(size_of::<Option<Revision>>()),
            )
            .saturating_add(
                self.resource_revisions
                    .iter()
                    .flatten()
                    .map(Revision::retained_bytes)
                    .fold(0, usize::saturating_add),
            )
    }
}

enum PreparedChange {
    Create {
        path: PathBuf,
        relative: WorkspacePath,
        content: String,
    },
    Write {
        path: PathBuf,
        relative: WorkspacePath,
        content: String,
        old_content: String,
        expected: crate::text::FileVersion,
    },
    Mkdir {
        path: PathBuf,
        relative: WorkspacePath,
    },
    Rename {
        source: PathBuf,
        destination: PathBuf,
        source_relative: WorkspacePath,
        destination_relative: WorkspacePath,
        expected: crate::text::FileVersion,
    },
    Delete {
        path: PathBuf,
        relative: WorkspacePath,
        expected: crate::text::FileVersion,
    },
}

impl PreparedChange {
    fn retained_bytes(&self) -> usize {
        let workspace_path = WorkspacePath::retained_bytes;
        match self {
            Self::Create {
                path,
                relative,
                content,
            } => size_of::<Self>()
                .saturating_add(path.capacity())
                .saturating_add(workspace_path(relative))
                .saturating_add(content.capacity()),
            Self::Write {
                path,
                relative,
                content,
                old_content,
                ..
            } => size_of::<Self>()
                .saturating_add(path.capacity())
                .saturating_add(workspace_path(relative))
                .saturating_add(content.capacity())
                .saturating_add(old_content.capacity()),
            Self::Mkdir { path, relative } | Self::Delete { path, relative, .. } => {
                size_of::<Self>()
                    .saturating_add(path.capacity())
                    .saturating_add(workspace_path(relative))
            }
            Self::Rename {
                source,
                destination,
                source_relative,
                destination_relative,
                ..
            } => size_of::<Self>()
                .saturating_add(source.capacity())
                .saturating_add(destination.capacity())
                .saturating_add(workspace_path(source_relative))
                .saturating_add(workspace_path(destination_relative)),
        }
    }
}

fn prepared_workspace_parts_bytes(
    changes: &[PreparedChange],
    resources: &[FileResource],
    revisions: &[Option<Revision>],
) -> usize {
    changes
        .iter()
        .map(PreparedChange::retained_bytes)
        .chain(resources.iter().map(FileResource::retained_bytes))
        .chain(revisions.iter().flatten().map(Revision::retained_bytes))
        .fold(0, usize::saturating_add)
        .saturating_add(changes.len().saturating_mul(size_of::<PreparedChange>()))
        .saturating_add(resources.len().saturating_mul(size_of::<FileResource>()))
        .saturating_add(
            revisions
                .len()
                .saturating_mul(size_of::<Option<Revision>>()),
        )
}

fn enforce_preparation_bytes(bytes: usize, maximum: usize) -> Result<(), WorkspaceError> {
    if bytes > maximum {
        return Err(FilesystemError::message(format!(
            "Prepared workspace mutation exceeds maximum retained size of {maximum} bytes"
        ))
        .into());
    }
    Ok(())
}

enum Rollback {
    RemoveFile(PathBuf),
    RestoreFile { path: PathBuf, content: String },
    RemoveDirectory(PathBuf),
    Rename { from: PathBuf, to: PathBuf },
    RestoreDelete { staged: PathBuf, path: PathBuf },
}

impl FileToolGroup {
    #[must_use]
    pub fn workspace_snapshot_access(&self) -> WorkspaceSnapshotAccess {
        WorkspaceSnapshotAccess {
            core: self.core.clone(),
        }
    }

    pub async fn workspace_root(&self) -> Result<WorkspaceDirectory, WorkspaceError> {
        self.insert_directory(self.core.root().to_path_buf(), ".".to_owned())
            .await
    }

    pub async fn workspace_resolve_directory(
        &self,
        cwd: &ResourceId,
        path: &DirectoryNavigation,
    ) -> Result<WorkspaceDirectory, WorkspaceError> {
        let binding = self.validate_directory(cwd).await?;
        let mut components: Vec<&str> = if binding.relative_path == "." {
            Vec::new()
        } else {
            binding.relative_path.split('/').collect()
        };
        for component in path.as_str().split('/') {
            match component {
                "." => {}
                ".." => {
                    components.pop().ok_or(WorkspaceError::InvalidRequest)?;
                }
                _ => components.push(component),
            }
        }
        let joined = if components.is_empty() {
            ".".to_owned()
        } else {
            components.join("/")
        };
        let resolved = self.core.policy.resolve(&joined).await?;
        let relative = self.core.policy.relative(&resolved)?;
        let metadata = fs::metadata(&resolved).await.map_err(|error| {
            FilesystemError::io_path("Cannot inspect workspace directory", &resolved, error)
        })?;
        if !metadata.is_dir() {
            return Err(WorkspaceError::InvalidRequest);
        }
        self.insert_directory(resolved, relative).await
    }

    pub async fn workspace_directory_path(
        &self,
        cwd: &ResourceId,
    ) -> Result<String, WorkspaceError> {
        Ok(self.validate_directory(cwd).await?.relative_path)
    }

    pub async fn workspace_snapshot_scope(
        &self,
        cwd: &ResourceId,
    ) -> Result<WorkspaceSnapshotScope, WorkspaceError> {
        let binding = self.validate_directory(cwd).await?;
        Ok(WorkspaceSnapshotScope {
            path: binding.relative_path,
            revision: binding.revision,
        })
    }

    pub async fn workspace_discover_repository(
        &self,
        cwd: &ResourceId,
        path: &WorkspacePath,
    ) -> Result<WorkspaceRepositoryResource, WorkspaceError> {
        let (_, resolved, _) = self.resolve_workspace_path(cwd, path).await?;
        let metadata = fs::metadata(&resolved)
            .await
            .map_err(|_| WorkspaceError::RepositoryUnavailable)?;
        let mut current = if metadata.is_dir() {
            resolved
        } else {
            resolved
                .parent()
                .ok_or(WorkspaceError::RepositoryUnavailable)?
                .to_path_buf()
        };
        loop {
            let dot_git = current.join(".git");
            match fs::symlink_metadata(&dot_git).await {
                Ok(metadata) => {
                    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                        return Err(WorkspaceError::UnsupportedRepository);
                    }
                    let worktree = self.core.policy.resolve_internal_existing(&current).await?;
                    let git_dir = self
                        .core
                        .policy
                        .resolve_internal_existing(&dot_git)
                        .await
                        .map_err(|_| WorkspaceError::UnsupportedRepository)?;
                    if git_dir != worktree.join(".git") {
                        return Err(WorkspaceError::UnsupportedRepository);
                    }
                    validate_repository_storage(&self.core, &worktree, &git_dir).await?;
                    let relative_path = self.core.policy.relative(&worktree)?;
                    return Ok(WorkspaceRepositoryResource {
                        core: self.core.clone(),
                        resource_id: root_relative_resource_id(
                            RootResourceKind::Repository,
                            &relative_path,
                        )?,
                        worktree,
                        git_dir,
                        relative_path,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if !fs::metadata(&current)
                        .await
                        .map_err(|_| WorkspaceError::RepositoryUnavailable)?
                        .is_dir()
                    {
                        return Err(WorkspaceError::RepositoryUnavailable);
                    }
                }
                Err(_) => return Err(WorkspaceError::RepositoryUnavailable),
            }
            if current == self.core.root() {
                break;
            }
            current = current
                .parent()
                .filter(|parent| parent.starts_with(self.core.root()))
                .ok_or(WorkspaceError::RepositoryUnavailable)?
                .to_path_buf();
        }
        Err(WorkspaceError::NotRepository)
    }

    pub async fn workspace_stat(
        &self,
        request: &StatRequest,
    ) -> Result<StatResponse, WorkspaceError> {
        let (_, path, relative) = self
            .resolve_workspace_path(&request.binding.cwd_handle, &request.path)
            .await?;
        Ok(StatResponse {
            version: ContractVersion::V1,
            entry: self.workspace_entry(&path, relative).await?,
        })
    }

    pub async fn workspace_list(
        &self,
        request: &ListRequest,
        token: &CancellationToken,
    ) -> Result<ListResponse, WorkspaceError> {
        validate_page_size(request.page_size)?;
        check_cancelled(token)?;
        let permit = self
            .workspace
            .list_workers
            .clone()
            .try_acquire_owned()
            .map_err(|_| listing_capacity())?;
        let (binding, root, _) = self
            .resolve_workspace_path(&request.binding.cwd_handle, &request.path)
            .await?;
        #[cfg(test)]
        tests::run_workspace_hook(tests::WorkspaceHookPhase::CwdResolved, &binding.path, token);
        let group = self.clone();
        let request = request.clone();
        let token = token.child_token();
        let _cancel_on_drop = token.clone().drop_guard();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            group.workspace_list_blocking(&binding, &root, &request, &token)
        })
        .await
        .map_err(|_| WorkspaceError::InvalidRequest)?
    }

    pub async fn workspace_read_text(
        &self,
        request: &ReadTextRequest,
        token: &CancellationToken,
    ) -> Result<ReadTextResponse, WorkspaceError> {
        if request.max_bytes == 0 || request.max_bytes > MAX_TEXT_READ_BYTES {
            return Err(WorkspaceError::InvalidRequest);
        }
        let (_, path, relative) = self
            .resolve_workspace_path(&request.binding.cwd_handle, &request.path)
            .await?;
        let snapshot =
            read_text_snapshot_required(&path, self.core.limits.max_file_bytes, token).await?;
        let lines = split_text_lines(&snapshot.content);
        let start = request.range.as_ref().map_or(1, |range| range.start_line);
        let requested_end = request.range.as_ref().and_then(|range| range.end_line);
        let end = requested_end.unwrap_or_else(|| u32::try_from(lines.len()).unwrap_or(u32::MAX));
        if start == 0 || requested_end.is_some_and(|end| end < start) {
            return Err(WorkspaceError::InvalidRequest);
        }
        let start_index = usize::try_from(start - 1).map_err(|_| WorkspaceError::InvalidRequest)?;
        let end_index = usize::try_from(end).unwrap_or(usize::MAX).min(lines.len());
        let selected = if start_index >= lines.len() {
            String::new()
        } else {
            lines[start_index..end_index].join("\n")
        };
        let byte_offset =
            usize::try_from(request.byte_offset).map_err(|_| WorkspaceError::InvalidRequest)?;
        if byte_offset > selected.len() || !selected.is_char_boundary(byte_offset) {
            return Err(WorkspaceError::InvalidRequest);
        }
        let maximum = request.max_bytes as usize;
        let mut end_byte = byte_offset.saturating_add(maximum).min(selected.len());
        while !selected.is_char_boundary(end_byte) {
            end_byte -= 1;
        }
        if end_byte == byte_offset && byte_offset < selected.len() {
            return Err(WorkspaceError::InvalidRequest);
        }
        let text = selected[byte_offset..end_byte].to_owned();
        let next_byte_offset =
            (end_byte < selected.len()).then(|| u64::try_from(end_byte).unwrap_or(u64::MAX));
        let returned_start_line = if text.is_empty() {
            0
        } else {
            start.saturating_add(
                u32::try_from(
                    selected[..byte_offset]
                        .bytes()
                        .filter(|byte| *byte == b'\n')
                        .count(),
                )
                .unwrap_or(u32::MAX),
            )
        };
        let complete_lines = text.bytes().filter(|byte| *byte == b'\n').count()
            + usize::from(!text.is_empty() && end_byte == selected.len());
        let returned_end_line = if complete_lines == 0 {
            0
        } else {
            returned_start_line
                .saturating_add(u32::try_from(complete_lines.saturating_sub(1)).unwrap_or(u32::MAX))
        };
        Ok(ReadTextResponse {
            version: ContractVersion::V1,
            resource_id: root_relative_resource_id(RootResourceKind::Path, &relative)?,
            revision: Revision::new(snapshot.version.revision())
                .map_err(|_| WorkspaceError::InvalidRequest)?,
            path: WorkspacePath::new(relative).map_err(|_| WorkspaceError::InvalidRequest)?,
            text,
            start_line: returned_start_line,
            end_line: returned_end_line,
            total_lines: u32::try_from(lines.len()).unwrap_or(u32::MAX),
            start_byte: request.byte_offset,
            end_byte: u64::try_from(end_byte).unwrap_or(u64::MAX),
            truncated: next_byte_offset.is_some(),
            next_byte_offset,
        })
    }

    pub async fn workspace_search_text(
        &self,
        request: &SearchTextRequest,
        token: &CancellationToken,
    ) -> Result<SearchTextResponse, WorkspaceError> {
        validate_page_size(request.page_size)?;
        let (_, scope, relative_scope) = self
            .resolve_workspace_path(&request.binding.cwd_handle, &request.path)
            .await?;
        let grep = self
            .file_grep(
                FileGrepInput {
                    pattern: request.pattern.as_str().to_owned(),
                    path: Some(scope.to_string_lossy().into_owned()),
                    include: request
                        .include
                        .as_ref()
                        .map(|value| value.as_str().to_owned()),
                    context_after: None,
                    context_before: None,
                    context: None,
                    head_limit: None,
                },
                token,
            )
            .await?;
        let files_scanned = u32::try_from(grep.files_scanned).unwrap_or(u32::MAX);
        let files_listed = u32::try_from(grep.files_listed).unwrap_or(u32::MAX);
        let truncated = grep.truncated;
        let mut matches = Vec::with_capacity(grep.rows.len());
        let mut validated = HashSet::new();
        for row in grep.rows {
            check_cancelled(token)?;
            let relative = self.core.policy.relative(Path::new(&row.path))?;
            let version = *grep
                .revisions
                .get(&row.path)
                .ok_or(WorkspaceError::StaleResource)?;
            if !validated.contains(&row.path) {
                validate_snapshot(
                    Path::new(&row.path),
                    &version,
                    self.core.limits.max_file_bytes,
                    token,
                )
                .await?;
                validated.insert(row.path.clone());
            }
            matches.push(TextSearchMatch {
                path: WorkspacePath::new(relative.clone())
                    .map_err(|_| WorkspaceError::InvalidRequest)?,
                resource_id: root_relative_resource_id(RootResourceKind::Path, &relative)?,
                revision: Revision::new(version.revision())
                    .map_err(|_| WorkspaceError::InvalidRequest)?,
                line: u32::try_from(row.line).unwrap_or(u32::MAX),
                text: row.text,
            });
        }
        matches.sort_by(|left, right| {
            left.path
                .as_str()
                .cmp(right.path.as_str())
                .then(left.line.cmp(&right.line))
        });
        let revision = digest_serializable(&(&matches, files_scanned, files_listed, truncated))?;
        let request_digest = digest_parts(&[
            request.binding.cwd_handle.as_str(),
            &relative_scope,
            request.pattern.as_str(),
            request.include.as_ref().map_or("", |value| value.as_str()),
        ])?;
        let offset = self.cursor_offset(
            request.cursor.as_ref(),
            CursorKind::Search,
            &request_digest,
            &revision,
        )?;
        let page_size = request.page_size as usize;
        let end = offset.saturating_add(page_size).min(matches.len());
        let next_cursor = (end < matches.len())
            .then(|| self.insert_cursor(CursorKind::Search, request_digest, revision.clone(), end))
            .transpose()?;
        Ok(SearchTextResponse {
            version: ContractVersion::V1,
            revision,
            matches: matches.into_iter().skip(offset).take(page_size).collect(),
            files_scanned,
            files_listed,
            truncated,
            next_cursor,
        })
    }

    pub async fn workspace_open_watch(
        &self,
        request: &WatchOpenRequest,
    ) -> Result<WorkspaceWatcher, WorkspaceError> {
        let (_, scope, relative) = self
            .resolve_workspace_path(&request.binding.cwd_handle, &request.path)
            .await?;
        let metadata = fs::metadata(&scope).await.map_err(|error| {
            FilesystemError::io_path("Cannot inspect workspace watch root", &scope, error)
        })?;
        if !metadata.is_dir() {
            return Err(WorkspaceError::InvalidRequest);
        }
        let (sender, receiver) = sync_channel(WATCH_BACKEND_QUEUE);
        let notification = Arc::new(Notify::new());
        let failure = Arc::new(AtomicU8::new(WATCH_FAILURE_NONE));
        let mut watcher = notify::recommended_watcher(watch_handler(
            sender,
            notification.clone(),
            failure.clone(),
        ))
        .map_err(|error| watch_unavailable(WorkspaceWatchPhase::Initialize, error))?;
        watcher
            .watch(
                &scope,
                if request.recursive {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                },
            )
            .map_err(|error| watch_unavailable(WorkspaceWatchPhase::Register, error))?;
        Ok(WorkspaceWatcher {
            _watcher: watcher,
            receiver: Mutex::new(receiver),
            notification,
            failure,
            core: self.core.clone(),
            scope,
            scope_relative: WorkspacePath::new(relative)
                .map_err(|_| WorkspaceError::InvalidRequest)?,
        })
    }

    pub async fn workspace_discover_project_assets(
        &self,
        request: &DiscoverProjectAssetsRequest,
        token: &CancellationToken,
    ) -> Result<DiscoverProjectAssetsResponse, WorkspaceError> {
        let directory = self.validate_directory(&request.binding.cwd_handle).await?;
        let (mut assets, mut unreadable) = self
            .walk_project_assets(
                &directory.path,
                MAX_PROJECT_ASSET_DISCOVERY_HASH_BYTES,
                token,
            )
            .await?;
        assets.sort_by(|left, right| left.path.as_str().cmp(right.path.as_str()));
        unreadable.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        let revision = digest_serializable(&(&assets, &unreadable))?;
        Ok(DiscoverProjectAssetsResponse {
            version: ContractVersion::V1,
            manifest: ProjectAssetManifest {
                version: Identifier::new(PROJECT_ASSET_MANIFEST_VERSION)
                    .map_err(|_| WorkspaceError::InvalidRequest)?,
                revision,
                assets,
                unreadable,
            },
        })
    }

    /// Assets are named by path, so the walk only reads the files that match
    /// one. A directory, entry or asset it cannot read is reported instead of
    /// failing discovery: one bad mount must not keep a session from starting.
    async fn walk_project_assets(
        &self,
        root: &Path,
        maximum_hash_bytes: u64,
        token: &CancellationToken,
    ) -> Result<(Vec<ProjectAsset>, Vec<WorkspacePath>), WorkspaceError> {
        let allows_protected = self.core.policy.traversal_allows_protected(root);
        let mut assets = Vec::new();
        let mut unreadable = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        let mut visited = 0usize;
        let mut remaining_hash_bytes = maximum_hash_bytes;
        while let Some(directory) = stack.pop() {
            check_cancelled(token)?;
            let listing_failed = |error| {
                FilesystemError::io_path("Cannot list workspace directory", &directory, error)
            };
            let mut children = Vec::new();
            let listed = match fs::read_dir(&directory).await {
                Ok(mut reader) => loop {
                    match reader.next_entry().await {
                        Ok(Some(entry)) => {
                            visited = visited.saturating_add(1);
                            if visited > self.core.limits.max_traversal_entries
                                || visited > MAX_WORKSPACE_LIST_ENTRIES as usize
                            {
                                return Err(WorkspaceError::InvalidRequest);
                            }
                            children.push(entry);
                        }
                        Ok(None) => break Ok(()),
                        Err(error) => break Err(listing_failed(error)),
                    }
                },
                Err(error) => Err(listing_failed(error)),
            };
            if let Err(error) = listed {
                if directory == root {
                    return Err(error.into());
                }
                skip_unreadable(
                    &mut unreadable,
                    self.core.policy.relative(&directory)?,
                    error,
                )?;
                continue;
            }
            children.sort_by_cached_key(fs::DirEntry::path);
            let mut directories = Vec::new();
            for entry in children {
                let path = entry.path();
                let file_type = match entry.file_type().await {
                    Ok(file_type) => file_type,
                    Err(error) => {
                        let error = FilesystemError::io_path(
                            "Cannot inspect workspace entry",
                            &path,
                            error,
                        );
                        skip_unreadable(&mut unreadable, self.core.policy.relative(&path)?, error)?;
                        continue;
                    }
                };
                if file_type.is_symlink()
                    || !self
                        .core
                        .policy
                        .traversal_entry_allowed(allows_protected, &path)
                    || !self.core.policy.authorize_canonical_entry(&path)
                {
                    continue;
                }
                if file_type.is_dir() {
                    directories.push(path);
                    continue;
                }
                let relative = self.core.policy.relative(&path)?;
                let Some((kind, trust)) = project_asset_kind(&relative) else {
                    continue;
                };
                let (revision, size_bytes) = match self
                    .read_project_asset_revision(&path, &mut remaining_hash_bytes, token)
                    .await
                {
                    Ok(snapshot) => snapshot,
                    Err(
                        error @ (WorkspaceError::InvalidRequest
                        | WorkspaceError::Filesystem(FilesystemError::Aborted)),
                    ) => return Err(error),
                    Err(error) => {
                        skip_unreadable(&mut unreadable, relative, error)?;
                        continue;
                    }
                };
                assets.push(ProjectAsset {
                    path: workspace_path(relative.clone())?,
                    resource_id: root_relative_resource_id(RootResourceKind::Path, &relative)?,
                    revision,
                    kind,
                    trust,
                    size_bytes,
                });
                if assets.len() > MAX_PROJECT_ASSETS {
                    return Err(WorkspaceError::InvalidRequest);
                }
            }
            stack.extend(directories.into_iter().rev());
        }
        Ok((assets, unreadable))
    }

    async fn read_project_asset_revision(
        &self,
        path: &Path,
        remaining_hash_bytes: &mut u64,
        token: &CancellationToken,
    ) -> Result<(Revision, u64), WorkspaceError> {
        check_cancelled(token)?;
        let failed = |error| FilesystemError::io("Cannot read project asset", error);
        let before = fs::metadata(path).await.map_err(failed)?;
        if !before.is_file() {
            return Err(WorkspaceError::StaleResource);
        }
        if before.len() > self.core.limits.max_file_bytes as u64 {
            return Err(WorkspaceError::FileTooLarge {
                maximum: self.core.limits.max_file_bytes,
            });
        }
        *remaining_hash_bytes = remaining_hash_bytes
            .checked_sub(before.len())
            .ok_or(WorkspaceError::InvalidRequest)?;
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags((OFlags::NONBLOCK | OFlags::NOFOLLOW).bits() as i32);
        let mut file = options.open(path).await.map_err(failed)?;
        if SnapshotTreeStamp::of(&before)
            != SnapshotTreeStamp::of(&file.metadata().await.map_err(failed)?)
        {
            return Err(WorkspaceError::StaleResource);
        }
        #[cfg(test)]
        tests::run_workspace_hook(tests::WorkspaceHookPhase::BeforeRead, path, token);
        let mut remaining = before.len();
        let mut buffer = [0u8; PROJECT_ASSET_HASH_BUFFER_BYTES];
        let mut digest = Sha256::new();
        while remaining > 0 {
            check_cancelled(token)?;
            let capacity = remaining.min(buffer.len() as u64) as usize;
            let count = file.read(&mut buffer[..capacity]).await.map_err(failed)?;
            if count == 0 {
                return Err(WorkspaceError::StaleResource);
            }
            remaining -= count as u64;
            digest.update(&buffer[..count]);
        }
        check_cancelled(token)?;
        if SnapshotTreeStamp::of(&before)
            != SnapshotTreeStamp::of(&file.metadata().await.map_err(failed)?)
        {
            return Err(WorkspaceError::StaleResource);
        }
        let revision = Revision::new(encode_digest(digest.finalize()))
            .map_err(|_| WorkspaceError::InvalidRequest)?;
        Ok((revision, before.len()))
    }

    pub async fn workspace_read_project_asset(
        &self,
        request: &ReadProjectAssetRequest,
        token: &CancellationToken,
    ) -> Result<ReadProjectAssetResponse, WorkspaceError> {
        if request.max_bytes == 0 || request.max_bytes > MAX_PROJECT_ASSET_READ_BYTES {
            return Err(WorkspaceError::InvalidRequest);
        }
        let Some((kind, trust)) = project_asset_kind(request.path.as_str()) else {
            return Err(WorkspaceError::InvalidRequest);
        };
        let stat = self
            .workspace_stat(&StatRequest {
                version: ContractVersion::V1,
                binding: request.binding.clone(),
                path: request.path.clone(),
            })
            .await?;
        if stat.entry.kind != WorkspaceEntryKind::File
            || stat.entry.path != request.path
            || stat.entry.revision.as_ref() != Some(&request.expected_revision)
        {
            return Err(WorkspaceError::StaleResource);
        }
        let read = self
            .workspace_read_text(
                &ReadTextRequest {
                    version: ContractVersion::V1,
                    binding: request.binding.clone(),
                    path: request.path.clone(),
                    range: None,
                    byte_offset: 0,
                    max_bytes: request.max_bytes,
                },
                token,
            )
            .await?;
        if read.path != request.path || Some(&read.revision) != stat.entry.revision.as_ref() {
            return Err(WorkspaceError::StaleResource);
        }
        Ok(ReadProjectAssetResponse {
            version: ContractVersion::V1,
            asset: ProjectAsset {
                path: read.path,
                resource_id: read.resource_id,
                revision: read.revision,
                kind,
                trust,
                size_bytes: stat.entry.size_bytes.ok_or(WorkspaceError::StaleResource)?,
            },
            encoding: ProjectAssetEncoding::Utf8,
            content: ProjectAssetContent::new(read.text)
                .map_err(|_| WorkspaceError::InvalidRequest)?,
            truncated: read.truncated,
        })
    }

    async fn insert_directory(
        &self,
        path: PathBuf,
        relative_path: String,
    ) -> Result<WorkspaceDirectory, WorkspaceError> {
        let revision = directory_revision(&path).await?;
        let handle = ResourceId::new(format!("cwd_{}", Uuid::new_v4()))
            .map_err(|_| WorkspaceError::InvalidRequest)?;
        let directory = WorkspaceDirectory {
            handle: handle.clone(),
            resource_id: root_relative_resource_id(RootResourceKind::Path, &relative_path)?,
            revision: revision.clone(),
            display_path: WorkspacePath::new(relative_path.clone())
                .map_err(|_| WorkspaceError::InvalidRequest)?,
        };
        let mut state = self
            .workspace
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((handle, _)) = state
            .directories
            .iter()
            .find(|(_, entry)| entry.path == path && entry.revision == revision)
        {
            return Ok(WorkspaceDirectory {
                handle: handle.clone(),
                ..directory
            });
        }
        if state.directories.len() >= MAX_CWD_HANDLES {
            return Err(WorkspaceError::InvalidRequest);
        }
        state.directories.insert(
            handle.clone(),
            DirectoryBinding {
                path,
                relative_path,
                revision,
            },
        );
        Ok(directory)
    }

    async fn validate_directory(
        &self,
        handle: &ResourceId,
    ) -> Result<DirectoryBinding, WorkspaceError> {
        let binding = self
            .workspace
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .directories
            .get(handle)
            .cloned()
            .ok_or(WorkspaceError::StaleCwd)?;
        let current = self
            .core
            .policy
            .resolve(&binding.relative_path)
            .await
            .map_err(|_| WorkspaceError::StaleCwd)?;
        let revision = directory_revision(&current)
            .await
            .map_err(|_| WorkspaceError::StaleCwd)?;
        if current != binding.path || revision != binding.revision {
            return Err(WorkspaceError::StaleCwd);
        }
        Ok(binding)
    }

    async fn resolve_workspace_path(
        &self,
        cwd: &ResourceId,
        requested: &WorkspacePath,
    ) -> Result<(DirectoryBinding, PathBuf, String), WorkspaceError> {
        let binding = self.validate_directory(cwd).await?;
        let joined = if binding.relative_path == "." {
            requested.as_str().to_owned()
        } else {
            format!("{}/{}", binding.relative_path, requested.as_str())
        };
        let path = self.core.policy.resolve(&joined).await?;
        let relative = self.core.policy.relative(&path)?;
        Ok((binding, path, relative))
    }

    async fn workspace_entry(
        &self,
        path: &Path,
        relative: String,
    ) -> Result<WorkspaceEntry, WorkspaceError> {
        let metadata = fs::metadata(path).await.map_err(|error| {
            FilesystemError::io_path("Cannot inspect workspace path", path, error)
        })?;
        let (kind, size_bytes, revision) = if metadata.is_file() {
            if metadata.len() > self.core.limits.max_file_bytes as u64 {
                return Err(WorkspaceError::FileTooLarge {
                    maximum: self.core.limits.max_file_bytes,
                });
            }
            (
                WorkspaceEntryKind::File,
                Some(metadata.len()),
                file_revision(
                    path,
                    self.core.limits.max_file_bytes,
                    &CancellationToken::new(),
                )
                .await?,
            )
        } else if metadata.is_dir() {
            (
                WorkspaceEntryKind::Directory,
                None,
                directory_revision(path).await?,
            )
        } else {
            return Err(WorkspaceError::InvalidRequest);
        };
        Ok(WorkspaceEntry {
            path: WorkspacePath::new(relative.clone())
                .map_err(|_| WorkspaceError::InvalidRequest)?,
            resource_id: root_relative_resource_id(RootResourceKind::Path, &relative)?,
            revision: Some(revision),
            kind,
            size_bytes,
        })
    }

    #[cfg(not(unix))]
    fn workspace_list_blocking(
        &self,
        _binding: &DirectoryBinding,
        _root: &Path,
        _request: &ListRequest,
        _token: &CancellationToken,
    ) -> Result<ListResponse, WorkspaceError> {
        Err(FilesystemError::message("Descriptor-relative workspace listing is unsupported").into())
    }

    #[cfg(unix)]
    fn workspace_list_blocking(
        &self,
        binding: &DirectoryBinding,
        root: &Path,
        request: &ListRequest,
        token: &CancellationToken,
    ) -> Result<ListResponse, WorkspaceError> {
        let scope = self.open_workspace_listing_scope(binding, root, token)?;
        let metadata = scope
            .metadata()
            .map_err(|error| FilesystemError::io("Cannot inspect workspace list root", error))?;
        let scope_revision = directory_revision_from_metadata(root, &metadata)?;
        let request_digest = digest_serializable(&(
            &request.binding.cwd_handle,
            &binding.revision,
            self.core.policy.relative(root)?,
            request.recursive,
            request.page_size,
        ))?;
        check_cancelled(token)?;
        if let Some(cursor) = &request.cursor {
            return self
                .workspace
                .listing_page(cursor, &request_digest, &scope_revision);
        }
        let mut reservation = self.workspace.reserve_listing()?;
        let mut listed = self.list_entries_blocking(&scope, root, request.recursive, token)?;
        listed
            .entries
            .sort_by(|left, right| left.path.as_str().cmp(right.path.as_str()));
        check_cancelled(token)?;
        let id = Uuid::new_v4();
        let revision = Revision::new(format!("{LIST_REVISION_NAMESPACE}{id}"))
            .map_err(|_| WorkspaceError::InvalidRequest)?;
        let bytes = size_of::<ListingInventory>() * 2
            + size_of::<ListingRecord>() * 2
            + request_digest.retained_bytes()
            + scope_revision.retained_bytes()
            + revision.retained_bytes()
            + listed.entries.capacity() * size_of::<WorkspaceEntry>()
            + listed
                .entries
                .iter()
                .map(workspace_entry_retained_bytes)
                .sum::<usize>();
        if bytes > reservation.bytes.num_permits() {
            return Err(listing_capacity());
        }
        drop(
            reservation
                .bytes
                .split(reservation.bytes.num_permits() - bytes),
        );
        drop(
            reservation
                .entries
                .split(reservation.entries.num_permits() - listed.entries.len()),
        );
        let inventory = Arc::new(ListingInventory {
            listed,
            id,
            nonce: Uuid::new_v4(),
            request_digest,
            scope_revision,
            revision,
            page_size: request.page_size as usize,
            expires_at: Instant::now() + LIST_INVENTORY_TTL,
            _reservation: reservation,
        });
        let page = inventory.page(0)?;
        check_cancelled(token)?;
        if page.next_cursor.is_some() {
            self.workspace.retain_listing(inventory)?;
        }
        Ok(page)
    }

    #[cfg(unix)]
    fn open_workspace_listing_scope(
        &self,
        binding: &DirectoryBinding,
        root: &Path,
        token: &CancellationToken,
    ) -> Result<File, WorkspaceError> {
        check_cancelled(token)?;
        let anchor = open_listing_root(self.core.root())
            .map_err(|error| FilesystemError::io("Cannot open workspace root", error.into()))?;
        let cwd = open_listing_child(&anchor, &binding.relative_path, LIST_SEARCH_FLAGS)
            .map_err(|_| WorkspaceError::StaleCwd)?;
        let metadata = cwd.metadata().map_err(|_| WorkspaceError::StaleCwd)?;
        if directory_revision_from_metadata(&binding.path, &metadata)? != binding.revision {
            return Err(WorkspaceError::StaleCwd);
        }
        drop(anchor);
        #[cfg(test)]
        tests::run_workspace_hook(tests::WorkspaceHookPhase::CwdOpened, &binding.path, token);
        check_cancelled(token)?;
        let scope = open_listing_child(&cwd, scope_relative(&binding.path, root)?, DIRECTORY_FLAGS)
            .map_err(|error| match error {
                Errno::NOTDIR => WorkspaceError::InvalidRequest,
                error => {
                    FilesystemError::io("Cannot open workspace list root", error.into()).into()
                }
            })?;
        Ok(scope)
    }

    #[cfg(unix)]
    fn list_entries_blocking(
        &self,
        scope: &File,
        root: &Path,
        recursive: bool,
        token: &CancellationToken,
    ) -> Result<WorkspaceListEntries, WorkspaceError> {
        let mut entries = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        let allows_protected = self.core.policy.traversal_allows_protected(root);
        let mut visited = 0usize;
        let mut retained_bytes = root.as_os_str().len();
        let mut truncated = false;
        let mut incomplete = false;
        'traversal: while let Some(directory) = stack.pop() {
            check_cancelled(token)?;
            let opened =
                open_listing_child(scope, scope_relative(root, &directory)?, DIRECTORY_FLAGS)
                    .and_then(|directory| Dir::read_from(&directory));
            let mut reader = match opened {
                Ok(reader) => reader,
                Err(error) if directory == root => {
                    return Err(FilesystemError::io(
                        "Cannot list workspace directory",
                        error.into(),
                    )
                    .into());
                }
                Err(_) => {
                    incomplete = true;
                    continue;
                }
            };
            let mut children = Vec::new();
            let mut stop_after_directory = false;
            loop {
                check_cancelled(token)?;
                let entry = match reader.next() {
                    Some(Ok(entry)) => entry,
                    None => break,
                    Some(Err(error)) if directory == root => {
                        return Err(FilesystemError::io_path(
                            "Cannot list workspace directory",
                            &directory,
                            error.into(),
                        )
                        .into());
                    }
                    Some(Err(_)) => {
                        incomplete = true;
                        break;
                    }
                };
                let name = entry.file_name().to_bytes();
                if matches!(name, b"." | b"..") {
                    continue;
                }
                visited = visited.saturating_add(1);
                if visited > self.core.limits.max_traversal_entries
                    || visited > MAX_WORKSPACE_LIST_ENTRIES as usize
                {
                    truncated = true;
                    stop_after_directory = true;
                    break;
                }
                let path = directory.join(OsStr::from_bytes(name));
                if !self
                    .core
                    .policy
                    .traversal_entry_allowed(allows_protected, &path)
                    || !self.core.policy.authorize_canonical_entry(&path)
                {
                    continue;
                }
                let prospective = retained_bytes
                    .saturating_add(path.as_os_str().len())
                    .saturating_add(size_of::<PathBuf>());
                if prospective > MAX_WORKSPACE_LIST_RETAINED_BYTES as usize {
                    truncated = true;
                    stop_after_directory = true;
                    break;
                }
                retained_bytes = prospective;
                children.push(path);
            }
            children.sort();
            #[cfg(test)]
            tests::run_workspace_hook(tests::WorkspaceHookPhase::BeforeRead, &directory, token);
            for path in &children {
                check_cancelled(token)?;
                let entry = match self.workspace_metadata_entry(scope, root, path) {
                    Ok(Some(entry)) => entry,
                    Ok(None) | Err(_) => {
                        incomplete = true;
                        continue;
                    }
                };
                let directory = entry.kind == WorkspaceEntryKind::Directory;
                let entry_bytes = workspace_entry_retained_bytes(&entry).saturating_add(
                    if recursive && directory {
                        path.as_os_str().len() + size_of::<PathBuf>()
                    } else {
                        0
                    },
                );
                if retained_bytes.saturating_add(entry_bytes)
                    > MAX_WORKSPACE_LIST_RETAINED_BYTES as usize
                {
                    truncated = true;
                    break 'traversal;
                }
                retained_bytes = retained_bytes.saturating_add(entry_bytes);
                entries.push(entry);
                if recursive && directory {
                    stack.push(path.clone());
                }
            }
            if stop_after_directory {
                break 'traversal;
            }
        }
        check_cancelled(token)?;
        Ok(WorkspaceListEntries {
            entries,
            truncated,
            incomplete,
        })
    }

    #[cfg(unix)]
    fn workspace_metadata_entry(
        &self,
        scope: &File,
        root: &Path,
        path: &Path,
    ) -> Result<Option<WorkspaceEntry>, WorkspaceError> {
        if path.to_str().is_none() {
            return Ok(None);
        }
        let node = open_listing_child(scope, scope_relative(root, path)?, WORKSPACE_METADATA_FLAGS)
            .map_err(|error| {
                FilesystemError::io("Cannot open workspace entry metadata", error.into())
            })?;
        let metadata = node
            .metadata()
            .map_err(|error| FilesystemError::io("Cannot inspect workspace entry", error))?;
        let kind = if metadata.is_file() {
            WorkspaceEntryKind::File
        } else if metadata.is_dir() {
            WorkspaceEntryKind::Directory
        } else {
            return Ok(None);
        };
        let relative = self.core.policy.relative(path)?;
        Ok(Some(WorkspaceEntry {
            path: workspace_path(relative.clone())?,
            resource_id: root_relative_resource_id(RootResourceKind::Path, &relative)?,
            revision: None,
            kind,
            size_bytes: metadata.is_file().then_some(metadata.len()),
        }))
    }

    fn cursor_offset(
        &self,
        cursor: Option<&Cursor>,
        kind: CursorKind,
        request_digest: &Revision,
        result_revision: &Revision,
    ) -> Result<usize, WorkspaceError> {
        let Some(cursor) = cursor else { return Ok(0) };
        let state = self
            .workspace
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let binding = state
            .cursors
            .get(cursor)
            .ok_or(WorkspaceError::InvalidCursor)?;
        if binding.kind != kind || binding.request_digest != *request_digest {
            return Err(WorkspaceError::InvalidCursor);
        }
        if binding.result_revision != *result_revision {
            return Err(WorkspaceError::StaleCursor);
        }
        Ok(binding.offset)
    }

    fn insert_cursor(
        &self,
        kind: CursorKind,
        request_digest: Revision,
        result_revision: Revision,
        offset: usize,
    ) -> Result<Cursor, WorkspaceError> {
        let cursor = Cursor::new(format!("cursor_{}", Uuid::new_v4()))
            .map_err(|_| WorkspaceError::InvalidRequest)?;
        let mut state = self
            .workspace
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cursors.insert(
            cursor.clone(),
            CursorBinding {
                kind,
                request_digest,
                result_revision,
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

    pub async fn prepare_workspace_mutation(
        &self,
        cwd: &ResourceId,
        mutations: Vec<WorkspaceMutation>,
        token: &CancellationToken,
    ) -> Result<PreparedWorkspaceMutation, WorkspaceError> {
        self.prepare_workspace_mutation_bounded(cwd, mutations, usize::MAX, token)
            .await
    }

    /// Prepare while bounding the aggregate old and new content retained by the plan.
    pub async fn prepare_workspace_mutation_bounded(
        &self,
        cwd: &ResourceId,
        mutations: Vec<WorkspaceMutation>,
        maximum_retained_bytes: usize,
        token: &CancellationToken,
    ) -> Result<PreparedWorkspaceMutation, WorkspaceError> {
        self.core.require_write()?;
        if mutations.is_empty() || mutations.len() > workcell_host_contract::MAX_MUTATIONS {
            return Err(WorkspaceError::InvalidRequest);
        }
        let mut paths = HashSet::new();
        let mut changes = Vec::with_capacity(mutations.len());
        let mut resources = Vec::with_capacity(mutations.len().saturating_mul(2));
        let mut resource_revisions = Vec::with_capacity(mutations.len().saturating_mul(2));
        for mutation in mutations {
            check_cancelled(token)?;
            match mutation {
                WorkspaceMutation::Create { path, content } => {
                    let (_, resolved, relative) = self.resolve_workspace_path(cwd, &path).await?;
                    reserve_path(&mut paths, &relative)?;
                    require_missing(&resolved).await?;
                    require_parent(&resolved).await?;
                    resources.push(file_resource(
                        &resolved,
                        &relative,
                        FileResourceAccess::Write,
                    ));
                    resource_revisions.push(None);
                    changes.push(PreparedChange::Create {
                        path: resolved,
                        relative: workspace_path(relative)?,
                        content: content.as_str().to_owned(),
                    });
                }
                WorkspaceMutation::Write {
                    path,
                    content,
                    expected_revision,
                } => {
                    let (_, resolved, relative) = self.resolve_workspace_path(cwd, &path).await?;
                    reserve_path(&mut paths, &relative)?;
                    let retained_before_read =
                        prepared_workspace_parts_bytes(&changes, &resources, &resource_revisions)
                            .saturating_add(self.core.limits.max_file_bytes)
                            .saturating_add(content.as_str().len())
                            .saturating_add(resolved.capacity())
                            .saturating_add(relative.len().saturating_mul(4));
                    enforce_preparation_bytes(retained_before_read, maximum_retained_bytes)?;
                    let snapshot = read_text_snapshot_required(
                        &resolved,
                        self.core.limits.max_file_bytes,
                        token,
                    )
                    .await?;
                    require_revision(&snapshot.version, &expected_revision)?;
                    resources.push(file_resource(
                        &resolved,
                        &relative,
                        FileResourceAccess::ReadWrite,
                    ));
                    resource_revisions.push(Some(expected_revision));
                    changes.push(PreparedChange::Write {
                        path: resolved,
                        relative: workspace_path(relative)?,
                        content: content.as_str().to_owned(),
                        old_content: snapshot.content,
                        expected: snapshot.version,
                    });
                }
                WorkspaceMutation::Mkdir { path } => {
                    let (_, resolved, relative) = self.resolve_workspace_path(cwd, &path).await?;
                    reserve_path(&mut paths, &relative)?;
                    require_missing(&resolved).await?;
                    require_parent(&resolved).await?;
                    resources.push(file_resource(
                        &resolved,
                        &relative,
                        FileResourceAccess::Write,
                    ));
                    resource_revisions.push(None);
                    changes.push(PreparedChange::Mkdir {
                        path: resolved,
                        relative: workspace_path(relative)?,
                    });
                }
                WorkspaceMutation::Rename {
                    from,
                    to,
                    expected_revision,
                } => {
                    let (_, source, source_relative) =
                        self.resolve_workspace_path(cwd, &from).await?;
                    let (_, destination, destination_relative) =
                        self.resolve_workspace_path(cwd, &to).await?;
                    reserve_path(&mut paths, &source_relative)?;
                    reserve_path(&mut paths, &destination_relative)?;
                    let version =
                        read_file_version_required(&source, self.core.limits.max_file_bytes, token)
                            .await?;
                    require_revision(&version, &expected_revision)?;
                    require_missing(&destination).await?;
                    require_parent(&destination).await?;
                    resources.push(file_resource(
                        &source,
                        &source_relative,
                        FileResourceAccess::Delete,
                    ));
                    resource_revisions.push(Some(expected_revision));
                    resources.push(file_resource(
                        &destination,
                        &destination_relative,
                        FileResourceAccess::Write,
                    ));
                    resource_revisions.push(None);
                    changes.push(PreparedChange::Rename {
                        source,
                        destination,
                        source_relative: workspace_path(source_relative)?,
                        destination_relative: workspace_path(destination_relative)?,
                        expected: version,
                    });
                }
                WorkspaceMutation::Delete {
                    path,
                    expected_revision,
                } => {
                    let (_, resolved, relative) = self.resolve_workspace_path(cwd, &path).await?;
                    reserve_path(&mut paths, &relative)?;
                    let version = read_file_version_required(
                        &resolved,
                        self.core.limits.max_file_bytes,
                        token,
                    )
                    .await?;
                    require_revision(&version, &expected_revision)?;
                    resources.push(file_resource(
                        &resolved,
                        &relative,
                        FileResourceAccess::Delete,
                    ));
                    resource_revisions.push(Some(expected_revision));
                    changes.push(PreparedChange::Delete {
                        path: resolved,
                        relative: workspace_path(relative)?,
                        expected: version,
                    });
                }
            }
        }
        let prepared = PreparedWorkspaceMutation {
            core: self.core.clone(),
            changes,
            resources,
            resource_revisions,
        };
        enforce_preparation_bytes(prepared.retained_bytes(), maximum_retained_bytes)?;
        Ok(prepared)
    }

    pub async fn execute_prepared_workspace_mutation(
        &self,
        prepared: PreparedWorkspaceMutation,
        token: &CancellationToken,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        if !Arc::ptr_eq(&self.core, &prepared.core) {
            return Err(WorkspaceError::InvalidRequest);
        }
        self.core.require_write()?;
        let _mutation = self.core.mutation.lock().await;
        for change in &prepared.changes {
            validate_change(&self.core, change, token).await?;
        }
        let mut rollback = Vec::with_capacity(prepared.changes.len());
        let mut results = Vec::with_capacity(prepared.changes.len());
        for change in &prepared.changes {
            if let Err(error) =
                apply_change(&self.core, change, token, &mut rollback, &mut results).await
            {
                return Err(rollback_after_error(&self.core, rollback, error).await);
            }
        }
        for action in rollback {
            if let Rollback::RestoreDelete { staged, .. } = action {
                let _ = fs::remove_file(staged).await;
            }
        }
        Ok(WorkspaceMutationResponse {
            version: ContractVersion::V1,
            committed: true,
            rolled_back: false,
            atomic_across_files: false,
            results,
        })
    }
}

fn listing_capacity() -> WorkspaceError {
    FilesystemError::message(LIST_CAPACITY_MESSAGE).into()
}

async fn expire_listing(workspace: Weak<WorkspaceState>, id: Uuid, deadline: Instant) {
    tokio::time::sleep_until(deadline.into()).await;
    if let Some(workspace) = workspace.upgrade() {
        workspace
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .listings
            .remove(&id);
    }
}

#[cfg(unix)]
fn open_listing_root(root: &Path) -> Result<File, Errno> {
    let relative = root.strip_prefix("/").map_err(|_| Errno::INVAL)?;
    let anchor = File::from(open("/", LIST_SEARCH_FLAGS, Mode::empty())?);
    open_listing_child(
        &anchor,
        if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative
        },
        LIST_SEARCH_FLAGS,
    )
}

#[cfg(unix)]
fn open_listing_child(parent: &File, path: impl AsRef<Path>, flags: OFlags) -> Result<File, Errno> {
    #[cfg(target_os = "linux")]
    {
        openat2(
            parent,
            path.as_ref(),
            flags,
            Mode::empty(),
            LIST_RESOLVE_FLAGS,
        )
        .map(File::from)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (parent, path, flags);
        Err(Errno::NOSYS)
    }
}

#[cfg(unix)]
fn scope_relative<'a>(scope: &Path, path: &'a Path) -> Result<&'a str, WorkspaceError> {
    let relative = path
        .strip_prefix(scope)
        .map_err(|_| WorkspaceError::InvalidRequest)?;
    if relative.as_os_str().is_empty() {
        Ok(".")
    } else {
        relative.to_str().ok_or(WorkspaceError::InvalidRequest)
    }
}

fn watch_unavailable(phase: WorkspaceWatchPhase, error: notify::Error) -> WorkspaceError {
    let (kind, io_kind, raw_os_error) = match error.kind {
        notify::ErrorKind::Generic(_) => (WorkspaceWatchErrorKind::Generic, None, None),
        notify::ErrorKind::Io(error) => (
            WorkspaceWatchErrorKind::Io,
            Some(error.kind()),
            error.raw_os_error(),
        ),
        notify::ErrorKind::PathNotFound => (WorkspaceWatchErrorKind::PathNotFound, None, None),
        notify::ErrorKind::WatchNotFound => (WorkspaceWatchErrorKind::WatchNotFound, None, None),
        notify::ErrorKind::InvalidConfig(_) => (WorkspaceWatchErrorKind::InvalidConfig, None, None),
        notify::ErrorKind::MaxFilesWatch => (WorkspaceWatchErrorKind::MaxFilesWatch, None, None),
    };
    WorkspaceError::WatchUnavailable {
        phase,
        kind,
        io_kind,
        raw_os_error,
    }
}

fn watch_handler(
    sender: SyncSender<WorkspaceWatchSignal>,
    notification: Arc<Notify>,
    failure: Arc<AtomicU8>,
) -> impl FnMut(notify::Result<Event>) + Send + 'static {
    move |result| {
        let signal = match result {
            Ok(event)
                if event.paths.len() <= MAX_WATCH_BACKEND_PATHS
                    && event
                        .paths
                        .iter()
                        .map(|path| path.as_os_str().len())
                        .sum::<usize>()
                        <= MAX_WATCH_BACKEND_EVENT_BYTES =>
            {
                WorkspaceWatchSignal::Event(event)
            }
            Ok(_) => {
                failure.store(WATCH_FAILURE_OVERFLOW, Ordering::Release);
                notification.notify_one();
                return;
            }
            Err(_) => {
                failure.store(WATCH_FAILURE_BACKEND, Ordering::Release);
                notification.notify_one();
                return;
            }
        };
        if sender.try_send(signal).is_err() {
            failure.store(WATCH_FAILURE_OVERFLOW, Ordering::Release);
        }
        notification.notify_one();
    }
}

fn project_asset_kind(path: &str) -> Option<(ProjectAssetKind, ProjectAssetTrust)> {
    let parts = path.split('/').collect::<Vec<_>>();
    let basename = parts.last().copied()?;
    if INSTRUCTION_FILES.contains(&basename)
        || parts.as_slice() == [".github", "copilot-instructions.md"]
        || parts.as_slice() == [".caudra", "instructions"]
    {
        return Some((
            ProjectAssetKind::Instructions,
            ProjectAssetTrust::Declarative,
        ));
    }
    if parts.len() == 4
        && SKILL_ROOTS.contains(&parts[0])
        && parts[1] == "skills"
        && !parts[2].is_empty()
        && parts[3] == "SKILL.md"
    {
        return Some((ProjectAssetKind::Skill, ProjectAssetTrust::Declarative));
    }
    if parts.len() == 3
        && COMMAND_PARENTS.contains(&(parts[0], parts[1]))
        && basename
            .strip_suffix(".md")
            .is_some_and(|stem| !stem.is_empty())
    {
        return Some((ProjectAssetKind::Command, ProjectAssetTrust::Declarative));
    }
    if parts.len() == 3
        && parts[0] == ".caudra"
        && parts[1] == "workflows"
        && !parts[2].starts_with('.')
        && parts[2].ends_with(".rhai")
    {
        return Some((
            ProjectAssetKind::Workflow,
            ProjectAssetTrust::ClientApprovalRequired,
        ));
    }
    if parts.as_slice() == [".caudra", "permissions.toml"] {
        return Some((
            ProjectAssetKind::Permissions,
            ProjectAssetTrust::MixedReviewRequired,
        ));
    }
    None
}

/// Names a path discovery could not read, unless it could hide the project's
/// permission policy: a session running without the project's restrictions is
/// weaker than one that does not start.
fn skip_unreadable(
    unreadable: &mut Vec<WorkspacePath>,
    relative: String,
    error: impl Into<WorkspaceError>,
) -> Result<(), WorkspaceError> {
    if matches!(relative.as_str(), ".caudra" | ".caudra/permissions.toml") {
        return Err(error.into());
    }
    if unreadable.len() < MAX_PROJECT_ASSETS {
        unreadable.push(WorkspacePath::new(relative).map_err(|_| WorkspaceError::InvalidRequest)?);
    }
    Ok(())
}

async fn validate_repository_storage(
    core: &FilesystemCore,
    worktree: &Path,
    git_dir: &Path,
) -> Result<(), WorkspaceError> {
    for forbidden in [
        worktree.join(".gitmodules"),
        git_dir.join("commondir"),
        git_dir.join("modules"),
        git_dir.join("objects/info/alternates"),
    ] {
        match fs::symlink_metadata(forbidden).await {
            Ok(_) => return Err(WorkspaceError::UnsupportedRepository),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(WorkspaceError::UnsupportedRepository),
        }
    }
    for confined in [
        git_dir.join("objects"),
        git_dir.join("refs"),
        git_dir.join("HEAD"),
        git_dir.join("config"),
        git_dir.join("index"),
        git_dir.join("packed-refs"),
    ] {
        match fs::symlink_metadata(&confined).await {
            Ok(metadata) => {
                if metadata.file_type().is_symlink()
                    || core
                        .policy
                        .resolve_internal_existing(&confined)
                        .await
                        .is_err()
                {
                    return Err(WorkspaceError::UnsupportedRepository);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(WorkspaceError::UnsupportedRepository),
        }
    }
    let mut stack = vec![git_dir.to_path_buf()];
    let mut visited = 0usize;
    while let Some(directory) = stack.pop() {
        let mut entries = fs::read_dir(&directory)
            .await
            .map_err(|_| WorkspaceError::UnsupportedRepository)?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|_| WorkspaceError::UnsupportedRepository)?
        {
            visited = visited.saturating_add(1);
            if visited > MAX_GIT_METADATA_ENTRIES {
                return Err(WorkspaceError::UnsupportedRepository);
            }
            let metadata = entry
                .file_type()
                .await
                .map_err(|_| WorkspaceError::UnsupportedRepository)?;
            if metadata.is_symlink() {
                return Err(WorkspaceError::UnsupportedRepository);
            }
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if !metadata.is_file() {
                return Err(WorkspaceError::UnsupportedRepository);
            }
        }
    }
    Ok(())
}

fn validate_page_size(page_size: u32) -> Result<(), WorkspaceError> {
    if page_size == 0 || page_size > MAX_PAGE_SIZE {
        return Err(WorkspaceError::InvalidRequest);
    }
    Ok(())
}

async fn file_revision(
    path: &Path,
    maximum: usize,
    token: &CancellationToken,
) -> Result<Revision, WorkspaceError> {
    let version = read_file_version_required(path, maximum, token).await?;
    Revision::new(version.revision()).map_err(|_| WorkspaceError::InvalidRequest)
}

async fn directory_revision(path: &Path) -> Result<Revision, WorkspaceError> {
    let metadata = fs::metadata(path).await.map_err(|error| {
        FilesystemError::io_path("Cannot inspect workspace directory", path, error)
    })?;
    directory_revision_from_metadata(path, &metadata)
}

pub(crate) fn directory_revision_from_metadata(
    path: &Path,
    metadata: &Metadata,
) -> Result<Revision, WorkspaceError> {
    if !metadata.is_dir() {
        return Err(WorkspaceError::InvalidRequest);
    }
    let identity = directory_identity(metadata);
    digest_parts(&[&path.to_string_lossy(), &identity])
}

#[cfg(unix)]
fn directory_identity(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}:{}", metadata.dev(), metadata.ino())
}

#[cfg(windows)]
fn directory_identity(metadata: &std::fs::Metadata) -> String {
    use std::os::windows::fs::MetadataExt;
    format!(
        "{:?}:{:?}",
        metadata.volume_serial_number(),
        metadata.file_index()
    )
}

#[cfg(not(any(unix, windows)))]
fn directory_identity(metadata: &std::fs::Metadata) -> String {
    format!("{}:{:?}", metadata.len(), metadata.modified().ok())
}

pub fn root_relative_resource_scope(
    kind: RootResourceKind,
    path: &str,
) -> Result<Vec<ResourceId>, WorkspaceError> {
    let leaf = root_relative_resource_id(kind, path)?;
    let mut scope = Vec::new();
    if path != "." {
        scope.push(root_relative_resource_id(RootResourceKind::Path, ".")?);
        for (index, _) in path.match_indices('/') {
            if scope.len() >= workcell_host_contract::MAX_RESOURCE_SCOPE_DEPTH - 1 {
                return Err(WorkspaceError::InvalidRequest);
            }
            scope.push(root_relative_resource_id(
                RootResourceKind::Path,
                &path[..index],
            )?);
        }
    }
    scope.push(leaf);
    Ok(scope)
}

pub fn root_relative_resource_id(
    kind: RootResourceKind,
    canonical_root_relative_path: &str,
) -> Result<ResourceId, WorkspaceError> {
    let canonical = canonical_root_relative_path == "."
        || (!canonical_root_relative_path.is_empty()
            && !canonical_root_relative_path.starts_with('/')
            && !canonical_root_relative_path.ends_with('/')
            && canonical_root_relative_path
                .split('/')
                .all(|component| !matches!(component, "" | "." | "..")));
    if !canonical || canonical_root_relative_path.contains('\\') {
        return Err(WorkspaceError::InvalidRequest);
    }
    ResourceId::new(hex_digest(
        format!(
            "workcell-root-resource-v1\0{}\0{canonical_root_relative_path}",
            kind.namespace()
        )
        .as_bytes(),
    ))
    .map_err(|_| WorkspaceError::InvalidRequest)
}

fn workspace_entry_retained_bytes(entry: &WorkspaceEntry) -> usize {
    size_of::<WorkspaceEntry>()
        .saturating_add(entry.path.retained_bytes())
        .saturating_add(entry.resource_id.retained_bytes())
        .saturating_add(entry.revision.as_ref().map_or(0, Revision::retained_bytes))
}

fn digest_parts(parts: &[&str]) -> Result<Revision, WorkspaceError> {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(part.len().to_be_bytes());
        digest.update(part.as_bytes());
    }
    Revision::new(hex_digest(&digest.finalize())).map_err(|_| WorkspaceError::InvalidRequest)
}

fn digest_serializable(value: &impl serde::Serialize) -> Result<Revision, WorkspaceError> {
    let bytes = serde_json::to_vec(value).map_err(|_| WorkspaceError::InvalidRequest)?;
    Revision::new(hex_digest(&bytes)).map_err(|_| WorkspaceError::InvalidRequest)
}

fn hex_digest(bytes: &[u8]) -> String {
    encode_digest(Sha256::digest(bytes))
}

fn encode_digest(digest: impl IntoIterator<Item = u8>) -> String {
    let mut value = String::from("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

#[cfg(test)]
mod tests {
    #[test]
    fn resource_ancestry_includes_each_canonical_parent_once() {
        let scope =
            super::root_relative_resource_scope(super::RootResourceKind::Path, "a/b/file").unwrap();
        let expected = [".", "a", "a/b", "a/b/file"]
            .into_iter()
            .map(|path| {
                super::root_relative_resource_id(super::RootResourceKind::Path, path).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(scope, expected);
        assert!(
            super::root_relative_resource_scope(super::RootResourceKind::Path, "a/../file")
                .is_err()
        );
        let deep = vec!["a"; workcell_host_contract::MAX_RESOURCE_SCOPE_DEPTH + 1].join("/");
        assert!(super::root_relative_resource_scope(super::RootResourceKind::Path, &deep).is_err());
    }

    use std::{fs as std_fs, sync::OnceLock};

    use tempfile::tempdir;
    use workcell_host_contract::{
        HostBinding, Identifier, IncludePattern, ListRequest, MutationContent,
        PrepareMutationRequest, ReadTextRequest, Revision, SearchPattern, SearchTextRequest,
        StatRequest, TextRange, WorkspaceMutation, WorkspacePath, WorkspaceRequestBinding,
    };

    use super::*;
    use crate::FileReadInput;
    use crate::text::install_snapshot_read_hook;

    const PERMISSION_DENIED_CODE: &str = "filesystem_permission_denied";
    const STALE_RESOURCE_CODE: &str = "stale_resource";
    const WATCH_UNAVAILABLE_CODE: &str = "watch_unavailable";
    const PRIVATE_DIAGNOSTIC: &str = "/private/root/token-secret";
    #[derive(Eq, Hash, PartialEq)]
    pub(super) enum WorkspaceHookPhase {
        CwdResolved,
        CwdOpened,
        BeforeRead,
    }
    type ListHook = Box<dyn FnOnce(&CancellationToken) + Send>;
    static LIST_HOOKS: OnceLock<Mutex<HashMap<(WorkspaceHookPhase, PathBuf), ListHook>>> =
        OnceLock::new();

    fn install_workspace_hook(
        phase: WorkspaceHookPhase,
        path: &Path,
        hook: impl FnOnce(&CancellationToken) + Send + 'static,
    ) {
        LIST_HOOKS
            .get_or_init(Mutex::default)
            .lock()
            .unwrap()
            .insert((phase, path.to_path_buf()), Box::new(hook));
    }

    pub(super) fn run_workspace_hook(
        phase: WorkspaceHookPhase,
        path: &Path,
        token: &CancellationToken,
    ) {
        let hook = LIST_HOOKS
            .get_or_init(Mutex::default)
            .lock()
            .unwrap()
            .remove(&(phase, path.to_path_buf()));
        if let Some(hook) = hook {
            hook(token);
        }
    }

    async fn list_request(group: &FileToolGroup) -> ListRequest {
        ListRequest {
            version: ContractVersion::V1,
            binding: request_binding(group.workspace_root().await.unwrap().handle),
            path: workspace_path(".").unwrap(),
            recursive: true,
            page_size: MAX_PAGE_SIZE,
            cursor: None,
        }
    }

    #[tokio::test]
    async fn inventory_pages_and_retries_never_rewalk_or_observe_post_capture_changes() {
        use std::sync::atomic::AtomicUsize;

        const ORIGINAL: &str = "old";
        let root = tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std_fs::write(root.path().join(name), ORIGINAL).unwrap();
        }
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let mut request = list_request(&group).await;
        request.page_size = 1;
        let first = group
            .workspace_list(&request, &CancellationToken::new())
            .await
            .unwrap();
        let rewalks = Arc::new(AtomicUsize::new(0));
        let observed = rewalks.clone();
        install_workspace_hook(WorkspaceHookPhase::BeforeRead, root.path(), move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        });
        std_fs::write(root.path().join("b.txt"), "changed after capture").unwrap();
        std_fs::remove_file(root.path().join("c.txt")).unwrap();
        std_fs::write(root.path().join("d.txt"), "added").unwrap();
        request.cursor = first.next_cursor;
        let second = group
            .workspace_list(&request, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(second.entries[0].path.as_str(), "b.txt");
        assert_eq!(second.entries[0].size_bytes, Some(ORIGINAL.len() as u64));
        request.cursor = second.next_cursor.clone();
        let last = group
            .workspace_list(&request, &CancellationToken::new())
            .await
            .unwrap();
        let retry = group
            .workspace_list(&request, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(last.entries[0].path.as_str(), "c.txt");
        assert!(last.next_cursor.is_none());
        assert_eq!(
            serde_json::to_value(&last).unwrap(),
            serde_json::to_value(retry).unwrap()
        );
        assert_eq!(last.revision, second.revision);
        assert_eq!(rewalks.load(Ordering::SeqCst), 0);
        request.cursor = None;
        let fresh = group
            .workspace_list(&request, &CancellationToken::new())
            .await
            .unwrap();
        assert_ne!(fresh.revision, last.revision);
        assert_eq!(rewalks.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn empty_scopes_have_distinct_inventory_generations_without_retained_receipts() {
        let root = tempdir().unwrap();
        for scope in ["first", "second"] {
            std_fs::create_dir(root.path().join(scope)).unwrap();
        }
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let mut request = list_request(&group).await;
        let mut revisions = HashSet::new();
        for scope in ["first", "second", "first"] {
            request.path = workspace_path(scope).unwrap();
            let page = group
                .workspace_list(&request, &CancellationToken::new())
                .await
                .unwrap();
            assert!(page.entries.is_empty());
            assert!(page.next_cursor.is_none());
            assert!(revisions.insert(page.revision));
        }
        assert!(group.workspace.inner.lock().unwrap().listings.is_empty());
        assert_eq!(
            group.workspace.list_slots.available_permits(),
            MAX_LIST_INVENTORIES
        );
        assert_eq!(
            group.workspace.list_entries.available_permits(),
            MAX_LIST_INVENTORY_ENTRIES
        );
        assert_eq!(
            group.workspace.list_bytes.available_permits(),
            MAX_LIST_INVENTORY_BYTES
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cached_pages_do_not_bypass_revoked_scope_read_permission() {
        use std::os::unix::fs::PermissionsExt;

        if rustix::process::geteuid().is_root() {
            return;
        }
        let root = tempdir().unwrap();
        let scope = root.path().join("scope");
        std_fs::create_dir(&scope).unwrap();
        for name in ["a", "b"] {
            std_fs::write(scope.join(name), name).unwrap();
        }
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let mut request = list_request(&group).await;
        request.path = workspace_path("scope").unwrap();
        request.page_size = 1;
        request.cursor = group
            .workspace_list(&request, &CancellationToken::new())
            .await
            .unwrap()
            .next_cursor;
        std_fs::set_permissions(&scope, std_fs::Permissions::from_mode(0o0)).unwrap();
        let refused = group
            .workspace_list(&request, &CancellationToken::new())
            .await;
        std_fs::set_permissions(&scope, std_fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(refused.unwrap_err().code(), PERMISSION_DENIED_CODE);
    }

    #[tokio::test]
    async fn inventory_expiry_tasks_do_not_keep_the_workspace_alive_after_owner_drop() {
        let root = tempdir().unwrap();
        for name in ["a", "b"] {
            std_fs::write(root.path().join(name), name).unwrap();
        }
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let mut request = list_request(&group).await;
        request.page_size = 1;
        group
            .workspace_list(&request, &CancellationToken::new())
            .await
            .unwrap();
        let weak = Arc::downgrade(&group.workspace);
        assert_eq!(group.workspace.inner.lock().unwrap().listings.len(), 1);
        drop(group);
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn inventory_cursors_reject_wrong_scope_cwd_page_bounds_and_forged_offsets() {
        let root = tempdir().unwrap();
        std_fs::create_dir(root.path().join("scope")).unwrap();
        for name in ["a", "b", "c"] {
            std_fs::write(root.path().join("scope").join(name), name).unwrap();
        }
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let mut request = list_request(&group).await;
        request.path = workspace_path("scope").unwrap();
        request.page_size = 1;
        request.cursor = group
            .workspace_list(&request, &CancellationToken::new())
            .await
            .unwrap()
            .next_cursor;
        let mut wrong_scope = request.clone();
        wrong_scope.path = workspace_path(".").unwrap();
        let mut wrong_page = request.clone();
        wrong_page.page_size += 1;
        let mut wrong_recursion = request.clone();
        wrong_recursion.recursive = !request.recursive;
        let mut wrong_cwd = request.clone();
        let cwd = group
            .workspace_resolve_directory(
                &request.binding.cwd_handle,
                &DirectoryNavigation::new("scope").unwrap(),
            )
            .await
            .unwrap();
        wrong_cwd.binding = request_binding(cwd.handle);
        wrong_cwd.path = workspace_path(".").unwrap();
        let mut forged = request.clone();
        forged.cursor = Some(
            Cursor::new(
                request
                    .cursor
                    .as_ref()
                    .unwrap()
                    .as_str()
                    .replacen("_1_", "_2_", 1),
            )
            .unwrap(),
        );
        for wrong in [wrong_scope, wrong_page, wrong_recursion, wrong_cwd, forged] {
            assert!(matches!(
                group
                    .workspace_list(&wrong, &CancellationToken::new())
                    .await
                    .unwrap_err(),
                WorkspaceError::InvalidCursor
            ));
        }
        request.cursor = Some(Cursor::new("unknown-inventory").unwrap());
        assert!(matches!(
            group
                .workspace_list(&request, &CancellationToken::new())
                .await
                .unwrap_err(),
            WorkspaceError::StaleCursor
        ));
    }

    #[tokio::test]
    async fn cached_pages_still_refuse_replaced_scope_and_cwd_descriptors() {
        for replace_cwd in [false, true] {
            let root = tempdir().unwrap();
            let public = root.path().join("public");
            let scope = public.join("scope");
            std_fs::create_dir_all(&scope).unwrap();
            for name in ["a", "b"] {
                std_fs::write(scope.join(name), name).unwrap();
            }
            let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
            let mut request = list_request(&group).await;
            if replace_cwd {
                let cwd = group
                    .workspace_resolve_directory(
                        &request.binding.cwd_handle,
                        &DirectoryNavigation::new("public").unwrap(),
                    )
                    .await
                    .unwrap();
                request.binding = request_binding(cwd.handle);
            }
            request.path =
                workspace_path(if replace_cwd { "scope" } else { "public/scope" }).unwrap();
            request.page_size = 1;
            request.cursor = group
                .workspace_list(&request, &CancellationToken::new())
                .await
                .unwrap()
                .next_cursor;
            std_fs::rename(
                if replace_cwd { &public } else { &scope },
                root.path().join("old"),
            )
            .unwrap();
            std_fs::create_dir_all(&scope).unwrap();
            let error = group
                .workspace_list(&request, &CancellationToken::new())
                .await
                .unwrap_err();
            if replace_cwd {
                assert!(matches!(error, WorkspaceError::StaleCwd));
            } else {
                assert!(matches!(error, WorkspaceError::StaleCursor));
            }
        }
    }

    #[tokio::test]
    async fn inventories_expire_without_requests_and_never_restart_unknown_cursors() {
        let root = tempdir().unwrap();
        for name in ["a", "b"] {
            std_fs::write(root.path().join(name), name).unwrap();
        }
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let mut request = list_request(&group).await;
        request.page_size = 1;
        request.cursor = group
            .workspace_list(&request, &CancellationToken::new())
            .await
            .unwrap()
            .next_cursor;
        let id = *group
            .workspace
            .inner
            .lock()
            .unwrap()
            .listings
            .keys()
            .next()
            .unwrap();
        expire_listing(Arc::downgrade(&group.workspace), id, Instant::now()).await;
        assert!(group.workspace.inner.lock().unwrap().listings.is_empty());
        assert_eq!(
            group.workspace.list_slots.available_permits(),
            MAX_LIST_INVENTORIES
        );
        assert!(matches!(
            group
                .workspace_list(&request, &CancellationToken::new())
                .await
                .unwrap_err(),
            WorkspaceError::StaleCursor
        ));
    }

    #[tokio::test]
    async fn inventory_capacity_never_evicts_live_receipts_and_arc_leases_remain_charged() {
        let root = tempdir().unwrap();
        for name in ["a", "b"] {
            std_fs::write(root.path().join(name), name).unwrap();
        }
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let mut request = list_request(&group).await;
        request.page_size = 1;
        let mut first = None;
        for _ in 0..MAX_LIST_INVENTORIES {
            let page = group
                .workspace_list(&request, &CancellationToken::new())
                .await
                .unwrap();
            first = first.or(page.next_cursor);
        }
        assert_eq!(
            group
                .workspace_list(&request, &CancellationToken::new())
                .await
                .unwrap_err()
                .code(),
            listing_capacity().code()
        );
        request.cursor = first;
        assert!(
            group
                .workspace_list(&request, &CancellationToken::new())
                .await
                .unwrap()
                .next_cursor
                .is_none()
        );
        let held = {
            let mut state = group.workspace.inner.lock().unwrap();
            let held = state.listings.values().next().unwrap().inventory.clone();
            state.expire_listings(Instant::now() + LIST_INVENTORY_TTL);
            held
        };
        assert_eq!(
            held._reservation.entries.num_permits(),
            held.listed.entries.len()
        );
        assert!(
            held._reservation.bytes.num_permits()
                >= held.listed.entries.capacity() * size_of::<WorkspaceEntry>()
                    + held
                        .listed
                        .entries
                        .iter()
                        .map(workspace_entry_retained_bytes)
                        .sum::<usize>()
        );
        assert_eq!(
            group.workspace.list_slots.available_permits(),
            MAX_LIST_INVENTORIES - 1
        );
        assert_eq!(
            group.workspace.list_entries.available_permits(),
            MAX_LIST_INVENTORY_ENTRIES - held._reservation.entries.num_permits()
        );
        assert_eq!(
            group.workspace.list_bytes.available_permits(),
            MAX_LIST_INVENTORY_BYTES - held._reservation.bytes.num_permits()
        );
        assert!(matches!(
            group
                .workspace_list(&request, &CancellationToken::new())
                .await
                .unwrap_err(),
            WorkspaceError::StaleCursor
        ));
        drop(held);
        assert_eq!(
            group.workspace.list_slots.available_permits(),
            MAX_LIST_INVENTORIES
        );
        assert_eq!(
            group.workspace.list_entries.available_permits(),
            MAX_LIST_INVENTORY_ENTRIES
        );
        assert_eq!(
            group.workspace.list_bytes.available_permits(),
            MAX_LIST_INVENTORY_BYTES
        );
    }

    #[test]
    fn inventory_admission_reserves_entries_and_bytes_and_rolls_back_partial_reservations() {
        let state = WorkspaceState::default();
        for gate in [&state.list_entries, &state.list_bytes] {
            let held = gate
                .clone()
                .try_acquire_many_owned(gate.available_permits() as u32)
                .unwrap();
            assert!(state.reserve_listing().is_err());
            assert_eq!(state.list_slots.available_permits(), MAX_LIST_INVENTORIES);
            drop(held);
            assert_eq!(
                state.list_entries.available_permits(),
                MAX_LIST_INVENTORY_ENTRIES
            );
            assert_eq!(
                state.list_bytes.available_permits(),
                MAX_LIST_INVENTORY_BYTES
            );
        }
        drop(state.reserve_listing().unwrap());
        assert_eq!(state.list_slots.available_permits(), MAX_LIST_INVENTORIES);
        assert_eq!(
            state.list_entries.available_permits(),
            MAX_LIST_INVENTORY_ENTRIES
        );
        assert_eq!(
            state.list_bytes.available_permits(),
            MAX_LIST_INVENTORY_BYTES
        );
    }

    #[tokio::test]
    async fn worker_admission_is_bounded_and_cancelled_requests_never_queue_or_capture() {
        let root = tempdir().unwrap();
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let request = list_request(&group).await;
        let _workers = group
            .workspace
            .list_workers
            .clone()
            .try_acquire_many_owned(MAX_LIST_WORKERS as u32)
            .unwrap();
        assert_eq!(
            group
                .workspace_list(&request, &CancellationToken::new())
                .await
                .unwrap_err()
                .code(),
            listing_capacity().code()
        );
        let token = CancellationToken::new();
        token.cancel();
        assert!(matches!(
            group.workspace_list(&request, &token).await.unwrap_err(),
            WorkspaceError::Filesystem(FilesystemError::Aborted)
        ));
        assert!(group.workspace.inner.lock().unwrap().listings.is_empty());
    }

    #[tokio::test]
    async fn listing_refuses_a_real_cwd_replacement_between_resolution_and_descriptor_open() {
        for scope in [".", "child"] {
            let root = tempdir().unwrap();
            let public = root.path().join("public");
            std_fs::create_dir_all(public.join("child")).unwrap();
            let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
            let mut request = list_request(&group).await;
            let directory = group
                .workspace_resolve_directory(
                    &request.binding.cwd_handle,
                    &DirectoryNavigation::new("public").unwrap(),
                )
                .await
                .unwrap();
            request.binding = request_binding(directory.handle);
            request.path = workspace_path(scope).unwrap();
            let original = public.clone();
            let moved = root.path().join("public-old");
            install_workspace_hook(WorkspaceHookPhase::CwdResolved, &public, move |_| {
                std_fs::rename(&original, moved).unwrap();
                std_fs::create_dir_all(original.join("child")).unwrap();
                std_fs::write(
                    original.join("child/replacement.txt"),
                    "replacement metadata",
                )
                .unwrap();
            });
            let error = group
                .workspace_list(&request, &CancellationToken::new())
                .await
                .unwrap_err();
            assert!(matches!(error, WorkspaceError::StaleCwd));
        }
    }

    #[tokio::test]
    async fn adding_children_does_not_invalidate_the_opened_cwd_identity() {
        use std::time::SystemTime;

        let root = tempdir().unwrap();
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let request = list_request(&group).await;
        let directory = root.path().to_path_buf();
        install_workspace_hook(WorkspaceHookPhase::CwdResolved, root.path(), move |_| {
            std_fs::write(directory.join("new.txt"), "new").unwrap();
            std_fs::File::open(directory)
                .unwrap()
                .set_times(std_fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
                .unwrap();
        });
        let response = group
            .workspace_list(&request, &CancellationToken::new())
            .await
            .unwrap();
        assert!(!response.incomplete);
        assert_eq!(response.entries.len(), 1);
        assert_eq!(response.entries[0].path.as_str(), "new.txt");
    }

    #[tokio::test]
    async fn child_scope_resolution_stays_on_the_verified_cwd_descriptor_after_rename() {
        let root = tempdir().unwrap();
        let public = root.path().join("public");
        std_fs::create_dir_all(public.join("child")).unwrap();
        std_fs::write(public.join("child/original.txt"), "original").unwrap();
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let mut request = list_request(&group).await;
        let directory = group
            .workspace_resolve_directory(
                &request.binding.cwd_handle,
                &DirectoryNavigation::new("public").unwrap(),
            )
            .await
            .unwrap();
        request.binding = request_binding(directory.handle);
        request.path = workspace_path("child").unwrap();
        let original = public.clone();
        let moved = root.path().join("public-old");
        install_workspace_hook(WorkspaceHookPhase::CwdOpened, &public, move |_| {
            std_fs::rename(&original, moved).unwrap();
            std_fs::create_dir_all(original.join("child")).unwrap();
            std_fs::write(
                original.join("child/replacement.txt"),
                "replacement metadata",
            )
            .unwrap();
        });
        let response = group
            .workspace_list(&request, &CancellationToken::new())
            .await
            .unwrap();
        assert!(!response.incomplete);
        assert_eq!(response.entries.len(), 1);
        assert_eq!(
            response.entries[0].path.as_str(),
            "public/child/original.txt"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn ordinary_listings_cross_existing_mounts_without_weakening_snapshot_or_transfer_policy()
    {
        use crate::binary::open_child;

        let root = Path::new("/dev");
        let anchor = open_listing_root(root).unwrap();
        assert_eq!(
            open_child(&anchor, "pts", DIRECTORY_FLAGS).unwrap_err(),
            Errno::XDEV
        );
        let group = FileToolGroup::new(root, false, None).await.unwrap();
        let mut request = list_request(&group).await;
        request.recursive = false;
        let response = group
            .workspace_list(&request, &CancellationToken::new())
            .await
            .unwrap();
        assert!(response.entries.iter().any(
            |entry| entry.path.as_str() == "pts" && entry.kind == WorkspaceEntryKind::Directory
        ));
        request.path = workspace_path("pts").unwrap();
        group
            .workspace_list(&request, &CancellationToken::new())
            .await
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn listing_requires_read_permission_only_on_directories_it_enumerates() {
        use std::os::unix::fs::PermissionsExt;

        if rustix::process::geteuid().is_root() {
            return;
        }
        for search_only_cwd in [false, true] {
            let fixture = tempdir().unwrap();
            let parent = fixture.path().join("search-only-parent");
            let project = parent.join("project");
            let public = project.join("public");
            std_fs::create_dir_all(public.join("child")).unwrap();
            std_fs::write(public.join("child/file.txt"), "metadata").unwrap();
            std_fs::set_permissions(&parent, std_fs::Permissions::from_mode(0o111)).unwrap();
            assert_eq!(
                std_fs::read_dir(&parent).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            if search_only_cwd {
                for path in [&project, &public] {
                    std_fs::set_permissions(path, std_fs::Permissions::from_mode(0o111)).unwrap();
                }
            }
            let result = async {
                let group = FileToolGroup::new(&project, false, None).await?;
                let directory = group.workspace_root().await?;
                let cwd = if search_only_cwd {
                    group
                        .workspace_resolve_directory(
                            &directory.handle,
                            &DirectoryNavigation::new("public").unwrap(),
                        )
                        .await?
                        .handle
                } else {
                    directory.handle
                };
                group
                    .workspace_list(
                        &ListRequest {
                            version: ContractVersion::V1,
                            binding: request_binding(cwd),
                            path: workspace_path(if search_only_cwd { "child" } else { "." })
                                .unwrap(),
                            recursive: false,
                            page_size: MAX_PAGE_SIZE,
                            cursor: None,
                        },
                        &CancellationToken::new(),
                    )
                    .await
            }
            .await;
            for path in [&parent, &project, &public] {
                std_fs::set_permissions(path, std_fs::Permissions::from_mode(0o700)).unwrap();
            }
            let response = result.unwrap();
            assert!(!response.incomplete);
            assert_eq!(response.entries.len(), 1);
            assert_eq!(
                response.entries[0].path.as_str(),
                if search_only_cwd {
                    "public/child/file.txt"
                } else {
                    "public"
                }
            );
        }
    }

    #[tokio::test]
    async fn vanished_descendants_leave_readable_siblings_and_an_incomplete_inventory() {
        let root = tempdir().unwrap();
        let missing = root.path().join("missing.txt");
        std_fs::write(&missing, "missing").unwrap();
        std_fs::write(root.path().join("sibling.txt"), "sibling").unwrap();
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        install_workspace_hook(WorkspaceHookPhase::BeforeRead, root.path(), move |_| {
            std_fs::remove_file(missing).unwrap()
        });
        let response = group
            .workspace_list(&list_request(&group).await, &CancellationToken::new())
            .await
            .unwrap();
        assert!(response.incomplete);
        assert!(!response.truncated);
        assert_eq!(response.entries.len(), 1);
        assert_eq!(response.entries[0].path.as_str(), "sibling.txt");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn listing_never_resolves_replaced_ancestors_into_protected_or_outside_metadata() {
        use std::os::unix::fs::symlink;

        const PUBLIC: &str = "public";
        const FORBIDDEN: &str = "forbidden metadata and content";
        for protected in [false, true] {
            let parent = tempdir().unwrap();
            let root = parent.path().join("root");
            let public = root.join("public");
            let target = if protected {
                root.join(".git")
            } else {
                parent.path().join("outside")
            };
            std_fs::create_dir_all(&public).unwrap();
            std_fs::create_dir_all(&target).unwrap();
            std_fs::write(public.join("config"), PUBLIC).unwrap();
            std_fs::write(target.join("config"), FORBIDDEN).unwrap();
            std_fs::write(root.join("sibling.txt"), PUBLIC).unwrap();
            let group = FileToolGroup::new(&root, false, None).await.unwrap();
            let moved = root.join("public-old");
            let raced = public.clone();
            install_workspace_hook(WorkspaceHookPhase::BeforeRead, &public, move |_| {
                std_fs::rename(&raced, moved).unwrap();
                symlink(target, raced).unwrap();
            });
            let response = group
                .workspace_list(&list_request(&group).await, &CancellationToken::new())
                .await
                .unwrap();
            assert!(response.incomplete);
            assert!(!response.truncated);
            assert_eq!(
                response
                    .entries
                    .iter()
                    .map(|entry| entry.path.as_str())
                    .collect::<Vec<_>>(),
                ["public", "sibling.txt"]
            );
            assert!(
                response
                    .entries
                    .iter()
                    .all(|entry| entry.size_bytes != Some(FORBIDDEN.len() as u64))
            );
        }
    }

    #[tokio::test]
    async fn cancellation_during_descriptor_enumeration_stops_before_child_metadata() {
        let root = tempdir().unwrap();
        std_fs::write(root.path().join("sibling.txt"), "sibling").unwrap();
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let token = CancellationToken::new();
        let cancelled = token.clone();
        install_workspace_hook(WorkspaceHookPhase::BeforeRead, root.path(), move |_| {
            cancelled.cancel()
        });
        let error = group
            .workspace_list(&list_request(&group).await, &token)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            WorkspaceError::Filesystem(FilesystemError::Aborted)
        ));
    }

    #[tokio::test]
    async fn dropping_a_listing_cancels_its_blocking_descriptor_worker() {
        use tokio::sync::oneshot;

        let root = tempdir().unwrap();
        std_fs::write(root.path().join("sibling.txt"), "sibling").unwrap();
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let request = list_request(&group).await;
        let workspace = group.workspace.clone();
        let (entered, observed) = oneshot::channel();
        let (resume, paused) = sync_channel(0);
        install_workspace_hook(WorkspaceHookPhase::BeforeRead, root.path(), move |token| {
            entered.send(token.clone()).unwrap();
            paused.recv().unwrap();
        });
        let listing = tokio::spawn(async move {
            group
                .workspace_list(&request, &CancellationToken::new())
                .await
        });
        let token = observed.await.unwrap();
        assert_eq!(
            workspace.list_workers.available_permits(),
            MAX_LIST_WORKERS - 1
        );
        assert_eq!(
            workspace.list_entries.available_permits(),
            MAX_LIST_INVENTORY_ENTRIES - MAX_WORKSPACE_LIST_ENTRIES as usize
        );
        assert_eq!(
            workspace.list_bytes.available_permits(),
            MAX_LIST_INVENTORY_BYTES - LIST_INVENTORY_RESERVATION_BYTES
        );
        listing.abort();
        assert!(listing.await.unwrap_err().is_cancelled());
        assert_eq!(
            workspace.list_workers.available_permits(),
            MAX_LIST_WORKERS - 1
        );
        let cancelled = token.is_cancelled();
        resume.send(()).unwrap();
        assert!(cancelled);
        let _workers = workspace
            .list_workers
            .clone()
            .acquire_many_owned(MAX_LIST_WORKERS as u32)
            .await
            .unwrap();
        assert_eq!(
            workspace.list_slots.available_permits(),
            MAX_LIST_INVENTORIES
        );
        assert_eq!(
            workspace.list_entries.available_permits(),
            MAX_LIST_INVENTORY_ENTRIES
        );
        assert_eq!(
            workspace.list_bytes.available_permits(),
            MAX_LIST_INVENTORY_BYTES
        );
        assert!(workspace.inner.lock().unwrap().listings.is_empty());
    }

    #[tokio::test]
    async fn asset_discovery_charges_its_own_budget_before_reading_each_candidate() {
        use std::sync::atomic::AtomicUsize;

        const FILE_BYTES: usize = 5;
        const HASH_BUDGET: u64 = 64;
        const FILE_COUNT: usize = 13;
        let root = tempdir().unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        for index in 0..FILE_COUNT {
            let directory = root.path().join(format!("asset-{index:02}"));
            std_fs::create_dir(&directory).unwrap();
            let path = directory.join("AGENTS.md");
            std_fs::write(&path, vec![b'x'; FILE_BYTES]).unwrap();
            let reads = reads.clone();
            install_workspace_hook(WorkspaceHookPhase::BeforeRead, &path, move |_| {
                reads.fetch_add(1, Ordering::SeqCst);
            });
        }
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let error = group
            .walk_project_assets(root.path(), HASH_BUDGET, &CancellationToken::new())
            .await
            .unwrap_err();
        LIST_HOOKS
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .retain(|(_, path), _| !path.starts_with(root.path()));
        assert!(matches!(error, WorkspaceError::InvalidRequest));
        assert_eq!(
            reads.load(Ordering::SeqCst),
            HASH_BUDGET as usize / FILE_BYTES
        );
    }

    #[tokio::test]
    async fn asset_growth_or_read_failure_cannot_refund_or_overrun_reserved_hash_bytes() {
        const ORIGINAL: &str = "small";
        const GROWN: &str = "larger than the entire discovery allowance";
        for replacement in ["", GROWN] {
            let root = tempdir().unwrap();
            let path = root.path().join("AGENTS.md");
            std_fs::write(&path, ORIGINAL).unwrap();
            let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
            let raced = path.clone();
            install_workspace_hook(WorkspaceHookPhase::BeforeRead, &path, move |_| {
                std_fs::write(raced, replacement).unwrap()
            });
            let mut budget = ORIGINAL.len() as u64;
            let error = group
                .read_project_asset_revision(&path, &mut budget, &CancellationToken::new())
                .await
                .unwrap_err();
            assert!(matches!(error, WorkspaceError::StaleResource));
            assert_eq!(budget, 0);
            std_fs::write(&path, ORIGINAL).unwrap();
            let error = group
                .read_project_asset_revision(&path, &mut budget, &CancellationToken::new())
                .await
                .unwrap_err();
            assert!(matches!(error, WorkspaceError::InvalidRequest));
        }
    }

    #[tokio::test]
    async fn cancelled_asset_hashing_does_not_return_a_partial_manifest() {
        const HASH_BUDGET: u64 = 64;
        let root = tempdir().unwrap();
        let path = root.path().join("AGENTS.md");
        std_fs::write(&path, "asset").unwrap();
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let token = CancellationToken::new();
        let cancelled = token.clone();
        install_workspace_hook(WorkspaceHookPhase::BeforeRead, &path, move |_| {
            cancelled.cancel()
        });
        let error = group
            .walk_project_assets(root.path(), HASH_BUDGET, &token)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            WorkspaceError::Filesystem(FilesystemError::Aborted)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn special_files_are_skipped_without_opening_or_blocking() {
        use rustix::fs::{CWD, FileType, Mode, mknodat};
        use std::os::unix::{fs::symlink, net::UnixListener};

        let root = tempdir().unwrap();
        mknodat(
            CWD,
            root.path().join("fifo"),
            FileType::Fifo,
            Mode::RUSR | Mode::WUSR,
            0,
        )
        .unwrap();
        let _socket = UnixListener::bind(root.path().join("socket")).unwrap();
        symlink("absent", root.path().join("dangling")).unwrap();
        std_fs::write(root.path().join("sibling.txt"), "sibling").unwrap();
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let request = list_request(&group).await;
        let response = group
            .workspace_list(&request, &CancellationToken::new())
            .await
            .unwrap();
        assert!(response.incomplete);
        assert!(!response.truncated);
        assert_eq!(response.entries.len(), 1);
        assert_eq!(response.entries[0].path.as_str(), "sibling.txt");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_descendants_do_not_hide_siblings_but_a_denied_root_is_an_error() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let denied = root.path().join("denied");
        let unreadable = root.path().join("unreadable.txt");
        std_fs::create_dir(&denied).unwrap();
        std_fs::write(&unreadable, "unreadable").unwrap();
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let request = list_request(&group).await;
        std_fs::set_permissions(&denied, std_fs::Permissions::from_mode(0o0)).unwrap();
        std_fs::set_permissions(&unreadable, std_fs::Permissions::from_mode(0o0)).unwrap();
        let response = group
            .workspace_list(&request, &CancellationToken::new())
            .await
            .unwrap();
        let refused = group
            .workspace_list(
                &ListRequest {
                    path: workspace_path("denied").unwrap(),
                    ..request.clone()
                },
                &CancellationToken::new(),
            )
            .await;
        let stat = group
            .workspace_stat(&StatRequest {
                version: ContractVersion::V1,
                binding: request.binding,
                path: workspace_path("unreadable.txt").unwrap(),
            })
            .await;
        std_fs::set_permissions(&denied, std_fs::Permissions::from_mode(0o700)).unwrap();
        std_fs::set_permissions(&unreadable, std_fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(response.entries.len(), 2);
        assert!(!response.truncated);
        if !rustix::process::geteuid().is_root() {
            assert!(response.incomplete);
            assert_eq!(refused.unwrap_err().code(), PERMISSION_DENIED_CODE);
            assert_eq!(stat.unwrap_err().code(), PERMISSION_DENIED_CODE);
        }
    }

    #[tokio::test]
    async fn metadata_inventory_revisions_cannot_authorize_mutations_or_hide_same_size_changes() {
        const ORIGINAL: &str = "before";
        const REPLACEMENT: &str = "edited";
        let root = tempdir().unwrap();
        let path = root.path().join("file.txt");
        std_fs::write(&path, ORIGINAL).unwrap();
        let times = std_fs::FileTimes::new()
            .set_modified(std_fs::metadata(&path).unwrap().modified().unwrap());
        let group = FileToolGroup::new(root.path(), true, None).await.unwrap();
        let request = list_request(&group).await;
        let listed = group
            .workspace_list(&request, &CancellationToken::new())
            .await
            .unwrap();
        let stat_request = StatRequest {
            version: ContractVersion::V1,
            binding: request.binding.clone(),
            path: workspace_path("file.txt").unwrap(),
        };
        let revision = group
            .workspace_stat(&stat_request)
            .await
            .unwrap()
            .entry
            .revision
            .unwrap();
        for expected_revision in [listed.revision, revision] {
            std_fs::write(&path, REPLACEMENT).unwrap();
            std_fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(times)
                .unwrap();
            let result = group
                .prepare_workspace_mutation(
                    &request.binding.cwd_handle,
                    vec![WorkspaceMutation::Delete {
                        path: stat_request.path.clone(),
                        expected_revision,
                    }],
                    &CancellationToken::new(),
                )
                .await;
            assert_eq!(result.err().unwrap().code(), STALE_RESOURCE_CODE);
        }
        assert_eq!(std_fs::read_to_string(path).unwrap(), REPLACEMENT);
    }

    #[test]
    fn native_watch_diagnostics_keep_only_phase_kind_and_os_classification() {
        for phase in [
            WorkspaceWatchPhase::Initialize,
            WorkspaceWatchPhase::Register,
        ] {
            let native = notify::Error::io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                PRIVATE_DIAGNOSTIC,
            ))
            .add_path(PathBuf::from(PRIVATE_DIAGNOSTIC));
            let error = watch_unavailable(phase.clone(), native);
            assert_eq!(error.code(), WATCH_UNAVAILABLE_CODE);
            assert!(!format!("{error:?} {error}").contains(PRIVATE_DIAGNOSTIC));
            assert!(
                matches!(error, WorkspaceError::WatchUnavailable { phase: actual, kind: WorkspaceWatchErrorKind::Io, io_kind: Some(io::ErrorKind::PermissionDenied), raw_os_error: None } if actual == phase)
            );
        }
        let errno = rustix::io::Errno::NOSPC.raw_os_error();
        let error = watch_unavailable(
            WorkspaceWatchPhase::Register,
            notify::Error::io(io::Error::from_raw_os_error(errno)),
        );
        assert!(
            matches!(error, WorkspaceError::WatchUnavailable { raw_os_error: Some(actual), .. } if actual == errno)
        );
        let error = watch_unavailable(
            WorkspaceWatchPhase::Initialize,
            notify::Error::generic(PRIVATE_DIAGNOSTIC),
        );
        assert!(!format!("{error:?} {error}").contains(PRIVATE_DIAGNOSTIC));
        assert!(matches!(
            error,
            WorkspaceError::WatchUnavailable {
                kind: WorkspaceWatchErrorKind::Generic,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn workspace_reads_searches_and_pages_with_inventory_bound_list_cursors() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("b.txt"), "needle b\n")
            .await
            .unwrap();
        fs::write(root.path().join("a.txt"), "needle a\n")
            .await
            .unwrap();
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let directory = group.workspace_root().await.unwrap();
        let binding = request_binding(directory.handle.clone());
        let request = ListRequest {
            version: ContractVersion::V1,
            binding: binding.clone(),
            path: workspace_path(".").unwrap(),
            recursive: false,
            page_size: 1,
            cursor: None,
        };
        let first = group
            .workspace_list(&request, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(first.entries[0].path.as_str(), "a.txt");
        let cursor = first.next_cursor.clone().unwrap();
        let second = group
            .workspace_list(
                &ListRequest {
                    cursor: Some(cursor.clone()),
                    ..request.clone()
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(second.entries[0].path.as_str(), "b.txt");
        assert_eq!(first.revision, second.revision);
        assert!(first.revision.as_str().starts_with(LIST_REVISION_NAMESPACE));
        assert!(!first.incomplete);

        let tampered = group
            .workspace_list(
                &ListRequest {
                    cursor: Some(Cursor::new(format!("{}x", cursor.as_str())).unwrap()),
                    ..request.clone()
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(tampered.code(), "invalid_cursor");
        fs::write(root.path().join("b.txt"), "changed\n")
            .await
            .unwrap();
        let replayed = group
            .workspace_list(
                &ListRequest {
                    cursor: Some(cursor),
                    ..request
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(replayed).unwrap(),
            serde_json::to_value(second).unwrap()
        );

        let read = group
            .workspace_read_text(
                &ReadTextRequest {
                    version: ContractVersion::V1,
                    binding: binding.clone(),
                    path: workspace_path("a.txt").unwrap(),
                    range: None,
                    byte_offset: 0,
                    max_bytes: 64,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(read.text, "needle a\n");
        assert!(read.revision.as_str().starts_with("sha256:"));

        fs::create_dir(root.path().join("nested")).await.unwrap();
        let direct = group
            .prepare_read(
                FileReadInput {
                    file_path: "a.txt".to_owned(),
                    offset: None,
                    limit: None,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let alias = group
            .prepare_read(
                FileReadInput {
                    file_path: "nested/../a.txt".to_owned(),
                    offset: None,
                    limit: None,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(direct.resource().resource_id().unwrap(), read.resource_id);
        assert_eq!(alias.resource().resource_id().unwrap(), read.resource_id);
        assert_eq!(alias.resource().root_relative_path, "a.txt");
        let expected_scope =
            super::root_relative_resource_scope(RootResourceKind::Path, "a.txt").unwrap();
        assert_eq!(direct.resource().resource_scope().unwrap(), expected_scope);
        assert_eq!(alias.resource().resource_scope().unwrap(), expected_scope);

        let search = group
            .workspace_search_text(
                &SearchTextRequest {
                    version: ContractVersion::V1,
                    binding,
                    path: workspace_path(".").unwrap(),
                    pattern: SearchPattern::new("needle").unwrap(),
                    include: Some(IncludePattern::new("*.txt").unwrap()),
                    page_size: 2,
                    cursor: None,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(search.matches.len(), 1);
        assert_eq!(search.matches[0].path.as_str(), "a.txt");
        assert_eq!(search.files_scanned, 2);
        assert_eq!(search.files_listed, 2);
        assert!(!search.truncated);
    }

    #[test]
    fn root_relative_resource_ids_separate_repository_and_path_kinds() {
        let path = root_relative_resource_id(RootResourceKind::Path, "src/lib.rs").unwrap();
        assert_eq!(
            path,
            root_relative_resource_id(RootResourceKind::Path, "src/lib.rs").unwrap()
        );
        assert_ne!(
            path,
            root_relative_resource_id(RootResourceKind::Repository, "src/lib.rs").unwrap()
        );
        assert!(root_relative_resource_id(RootResourceKind::Path, "src/../lib.rs").is_err());
    }

    #[tokio::test]
    async fn workspace_search_never_pairs_pre_replacement_text_with_post_replacement_revision() {
        let root = tempdir().unwrap();
        let path = root.path().join("raced.txt");
        let replacement = root.path().join("replacement.txt");
        fs::write(&path, "needle old\n").await.unwrap();
        fs::write(&replacement, "needle new\n").await.unwrap();
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let directory = group.workspace_root().await.unwrap();
        let request = SearchTextRequest {
            version: ContractVersion::V1,
            binding: request_binding(directory.handle),
            path: workspace_path(".").unwrap(),
            pattern: SearchPattern::new("needle").unwrap(),
            include: Some(IncludePattern::new("raced.txt").unwrap()),
            page_size: 10,
            cursor: None,
        };
        let replaced_path = path.clone();
        install_snapshot_read_hook(path, move || {
            std_fs::rename(&replacement, &replaced_path).unwrap();
        });

        let raced = group
            .workspace_search_text(&request, &CancellationToken::new())
            .await
            .unwrap();
        assert!(raced.matches.is_empty());

        let stable = group
            .workspace_search_text(&request, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(stable.matches.len(), 1);
        assert_eq!(stable.matches[0].text, "needle new");
        assert_eq!(
            stable.matches[0].revision.as_str(),
            hex_digest(b"needle new\n")
        );
    }

    #[tokio::test]
    async fn workspace_list_materialization_is_bounded_independently_of_page_size() {
        let root = tempdir().unwrap();
        for index in 0..32 {
            fs::write(root.path().join(format!("file-{index:02}.txt")), "x")
                .await
                .unwrap();
        }
        let limits = crate::FilesystemLimits {
            max_traversal_entries: 8,
            ..crate::FilesystemLimits::default()
        };
        let group = FileToolGroup::new(root.path(), false, Some(limits))
            .await
            .unwrap();
        let directory = group.workspace_root().await.unwrap();
        let listed = group
            .workspace_list(
                &ListRequest {
                    version: ContractVersion::V1,
                    binding: request_binding(directory.handle),
                    path: workspace_path(".").unwrap(),
                    recursive: true,
                    page_size: MAX_PAGE_SIZE,
                    cursor: None,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(listed.truncated);
        assert!(!listed.incomplete);
        assert_eq!(listed.entries.len(), 8);
        assert!(listed.next_cursor.is_none());
    }

    #[tokio::test]
    async fn workspace_list_never_reads_content_or_applies_content_size_limits() {
        let root = tempdir().unwrap();
        const LARGE_BYTES: u64 = 65 * 1_024 * 1_024;
        const CONTENT: &str = "small text\n";
        for name in ["oversized.txt", "ckeditor.js.map", "binary.bin"] {
            let file = std_fs::File::create(root.path().join(name)).unwrap();
            file.set_len(LARGE_BYTES).unwrap();
        }
        fs::write(root.path().join("small.txt"), CONTENT)
            .await
            .unwrap();
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let directory = group.workspace_root().await.unwrap();
        let binding = request_binding(directory.handle);
        let listed = group
            .workspace_list(
                &ListRequest {
                    version: ContractVersion::V1,
                    binding: binding.clone(),
                    path: workspace_path(".").unwrap(),
                    recursive: true,
                    page_size: MAX_PAGE_SIZE,
                    cursor: None,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(!listed.truncated);
        assert!(!listed.incomplete);
        assert_eq!(listed.entries.len(), 4);
        assert!(listed.entries.iter().all(|entry| entry.revision.is_none()));
        assert!(listed.next_cursor.is_none());
        let error = group
            .workspace_stat(&StatRequest {
                version: ContractVersion::V1,
                binding: binding.clone(),
                path: workspace_path("ckeditor.js.map").unwrap(),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, WorkspaceError::FileTooLarge { .. }));
        let read = group
            .workspace_read_text(
                &ReadTextRequest {
                    version: ContractVersion::V1,
                    binding,
                    path: workspace_path("small.txt").unwrap(),
                    range: None,
                    byte_offset: 0,
                    max_bytes: MAX_TEXT_READ_BYTES,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(read.text, CONTENT);
    }

    #[tokio::test]
    async fn workspace_text_reads_continue_by_byte_without_skipping_oversized_lines() {
        let root = tempdir().unwrap();
        let content = "abcdefghij\nsecond\n";
        fs::write(root.path().join("large.txt"), content)
            .await
            .unwrap();
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let directory = group.workspace_root().await.unwrap();
        let binding = request_binding(directory.handle);
        let mut byte_offset = 0;
        let mut reconstructed = String::new();
        let mut first = true;
        loop {
            let read = group
                .workspace_read_text(
                    &ReadTextRequest {
                        version: ContractVersion::V1,
                        binding: binding.clone(),
                        path: workspace_path("large.txt").unwrap(),
                        range: None,
                        byte_offset,
                        max_bytes: 4,
                    },
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            if first {
                assert_eq!(read.text, "abcd");
                assert_eq!(read.start_line, 1);
                assert_eq!(read.end_line, 0);
                assert_eq!(read.start_byte, 0);
                assert_eq!(read.end_byte, 4);
                first = false;
            }
            reconstructed.push_str(&read.text);
            let Some(next) = read.next_byte_offset else {
                assert!(!read.truncated);
                break;
            };
            assert!(read.truncated);
            assert!(next > byte_offset);
            byte_offset = next;
        }
        assert_eq!(reconstructed, content);

        let beyond_eof = group
            .workspace_read_text(
                &ReadTextRequest {
                    version: ContractVersion::V1,
                    binding,
                    path: workspace_path("large.txt").unwrap(),
                    range: Some(TextRange {
                        start_line: 99,
                        end_line: None,
                    }),
                    byte_offset: 0,
                    max_bytes: 4,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(beyond_eof.text.is_empty());
        assert_eq!(beyond_eof.start_line, 0);
        assert_eq!(beyond_eof.end_line, 0);
        assert_eq!(beyond_eof.start_byte, 0);
        assert_eq!(beyond_eof.end_byte, 0);
        assert_eq!(beyond_eof.next_byte_offset, None);
        assert!(!beyond_eof.truncated);
    }

    #[tokio::test]
    async fn workspace_search_reports_incomplete_upstream_results_without_a_cursor() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("many.txt"), "needle\n".repeat(501))
            .await
            .unwrap();
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let directory = group.workspace_root().await.unwrap();
        let search = group
            .workspace_search_text(
                &SearchTextRequest {
                    version: ContractVersion::V1,
                    binding: request_binding(directory.handle),
                    path: workspace_path(".").unwrap(),
                    pattern: SearchPattern::new("needle").unwrap(),
                    include: None,
                    page_size: MAX_PAGE_SIZE,
                    cursor: None,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(search.matches.len(), 500);
        assert_eq!(search.files_scanned, 1);
        assert_eq!(search.files_listed, 1);
        assert!(search.truncated);
        assert_eq!(search.next_cursor, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cwd_handles_reject_rebinding_and_the_resolver_rejects_project_escape() {
        use std::os::unix::fs::symlink;

        let parent = tempdir().unwrap();
        let root = parent.path().join("root");
        let outside = parent.path().join("outside");
        fs::create_dir(&root).await.unwrap();
        fs::create_dir(&outside).await.unwrap();
        fs::create_dir(root.join("sub")).await.unwrap();
        fs::write(root.join("visible.txt"), "visible")
            .await
            .unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        let group = FileToolGroup::new(&root, false, None).await.unwrap();
        let initial = group.workspace_root().await.unwrap();
        let sub = group
            .workspace_resolve_directory(&initial.handle, &DirectoryNavigation::new("sub").unwrap())
            .await
            .unwrap();
        let parent_cursor = group
            .workspace_resolve_directory(&sub.handle, &DirectoryNavigation::new("..").unwrap())
            .await
            .unwrap();
        assert_eq!(parent_cursor.handle, initial.handle);
        assert!(
            group
                .workspace_resolve_directory(
                    &initial.handle,
                    &DirectoryNavigation::new("..").unwrap()
                )
                .await
                .is_err()
        );
        assert!(
            group
                .workspace_resolve_directory(
                    &sub.handle,
                    &DirectoryNavigation::new("../../root").unwrap()
                )
                .await
                .is_err()
        );
        assert!(
            group
                .workspace_resolve_directory(
                    &sub.handle,
                    &DirectoryNavigation::new("../escape").unwrap()
                )
                .await
                .is_err()
        );
        let stat = group
            .workspace_stat(&StatRequest {
                version: ContractVersion::V1,
                binding: request_binding(sub.handle.clone()),
                path: workspace_path("../visible.txt").unwrap(),
            })
            .await
            .unwrap();
        assert_eq!(stat.entry.path.as_str(), "visible.txt");
        assert!(
            group
                .workspace_stat(&StatRequest {
                    version: ContractVersion::V1,
                    binding: request_binding(initial.handle.clone()),
                    path: workspace_path("escape/file").unwrap(),
                })
                .await
                .is_err()
        );
        assert!(
            group
                .workspace_open_watch(&WatchOpenRequest {
                    version: ContractVersion::V1,
                    binding: request_binding(initial.handle.clone()),
                    path: workspace_path("escape").unwrap(),
                    recursive: true,
                })
                .await
                .is_err()
        );
        fs::rename(root.join("sub"), root.join("moved"))
            .await
            .unwrap();
        let stale = group
            .workspace_directory_path(&sub.handle)
            .await
            .unwrap_err();
        assert_eq!(stale.code(), "stale_cwd");
    }

    #[tokio::test]
    async fn cwd_admission_never_evicts_live_directories() {
        let root = tempdir().unwrap();
        let group = FileToolGroup::new(root.path(), false, None).await.unwrap();
        let initial = group.workspace_root().await.unwrap();
        let mut first = None;
        for index in 1..=MAX_CWD_HANDLES {
            let name = format!("directory-{index}");
            fs::create_dir(root.path().join(&name)).await.unwrap();
            let result = group
                .workspace_resolve_directory(
                    &initial.handle,
                    &DirectoryNavigation::new(name).unwrap(),
                )
                .await;
            if index == 1 {
                first = Some(result.as_ref().unwrap().handle.clone());
            }
            assert_eq!(result.is_ok(), index < MAX_CWD_HANDLES);
        }
        assert_eq!(
            group
                .workspace_directory_path(&initial.handle)
                .await
                .unwrap(),
            "."
        );
        assert_eq!(
            group
                .workspace_directory_path(&first.unwrap())
                .await
                .unwrap(),
            "directory-1"
        );
        assert_eq!(group.workspace_root().await.unwrap().handle, initial.handle);
    }

    #[tokio::test]
    async fn mutation_batches_validate_every_resource_before_publication() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("existing.txt"), "before")
            .await
            .unwrap();
        let group = FileToolGroup::new(root.path(), true, None).await.unwrap();
        let directory = group.workspace_root().await.unwrap();
        let binding = request_binding(directory.handle.clone());
        let read = group
            .workspace_read_text(
                &ReadTextRequest {
                    version: ContractVersion::V1,
                    binding: binding.clone(),
                    path: workspace_path("existing.txt").unwrap(),
                    range: None,
                    byte_offset: 0,
                    max_bytes: 64,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let request = PrepareMutationRequest {
            version: ContractVersion::V1,
            binding,
            mutations: vec![
                WorkspaceMutation::Write {
                    path: workspace_path("existing.txt").unwrap(),
                    content: MutationContent::new("after").unwrap(),
                    expected_revision: read.revision,
                },
                WorkspaceMutation::Create {
                    path: workspace_path("new.txt").unwrap(),
                    content: MutationContent::new("new").unwrap(),
                },
            ],
        };
        request.validate().unwrap();
        let prepared = group
            .prepare_workspace_mutation(
                &directory.handle,
                request.mutations,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(prepared.retained_bytes() >= "before".len() + "after".len() + "new".len());
        fs::write(root.path().join("new.txt"), "racing value")
            .await
            .unwrap();
        let error = group
            .execute_prepared_workspace_mutation(prepared, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.code(), "stale_resource");
        assert_eq!(
            fs::read_to_string(root.path().join("existing.txt"))
                .await
                .unwrap(),
            "before"
        );
    }

    #[tokio::test]
    async fn bounded_mutation_rejects_many_large_old_files_before_the_aggregate_can_escape() {
        const FILE_BYTES: usize = 5 * 1_024 * 1_024;
        const PREPARATION_BYTES: usize = 30 * 1_024 * 1_024;

        let root = tempdir().unwrap();
        let group = FileToolGroup::new(root.path(), true, None).await.unwrap();
        let directory = group.workspace_root().await.unwrap();
        let binding = request_binding(directory.handle.clone());
        let mut mutations = Vec::new();
        for index in 0..7 {
            let name = format!("large-{index}.txt");
            fs::write(root.path().join(&name), vec![b'a'; FILE_BYTES])
                .await
                .unwrap();
            let stat = group
                .workspace_stat(&StatRequest {
                    version: ContractVersion::V1,
                    binding: binding.clone(),
                    path: workspace_path(&name).unwrap(),
                })
                .await
                .unwrap();
            mutations.push(WorkspaceMutation::Write {
                path: workspace_path(&name).unwrap(),
                content: MutationContent::new("x").unwrap(),
                expected_revision: stat.entry.revision.unwrap(),
            });
        }

        let result = group
            .prepare_workspace_mutation_bounded(
                &directory.handle,
                mutations,
                PREPARATION_BYTES,
                &CancellationToken::new(),
            )
            .await;
        let Err(error) = result else {
            panic!("large aggregate preparation unexpectedly succeeded");
        };

        assert!(error.to_string().contains("maximum retained size"));
        assert_eq!(
            fs::metadata(root.path().join("large-6.txt"))
                .await
                .unwrap()
                .len(),
            FILE_BYTES as u64
        );
    }

    #[tokio::test]
    async fn workspace_rename_and_delete_accept_revision_bound_binary_files() {
        let root = tempdir().unwrap();
        let binary = [0, 159, 146, 150, 255];
        fs::write(root.path().join("binary.dat"), binary)
            .await
            .unwrap();
        let group = FileToolGroup::new(root.path(), true, None).await.unwrap();
        let directory = group.workspace_root().await.unwrap();
        let binding = request_binding(directory.handle.clone());
        let initial = group
            .workspace_stat(&StatRequest {
                version: ContractVersion::V1,
                binding: binding.clone(),
                path: workspace_path("binary.dat").unwrap(),
            })
            .await
            .unwrap();
        let rename = group
            .prepare_workspace_mutation(
                &directory.handle,
                vec![WorkspaceMutation::Rename {
                    from: workspace_path("binary.dat").unwrap(),
                    to: workspace_path("renamed.dat").unwrap(),
                    expected_revision: initial.entry.revision.unwrap(),
                }],
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        group
            .execute_prepared_workspace_mutation(rename, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            fs::read(root.path().join("renamed.dat")).await.unwrap(),
            binary
        );

        let renamed = group
            .workspace_stat(&StatRequest {
                version: ContractVersion::V1,
                binding,
                path: workspace_path("renamed.dat").unwrap(),
            })
            .await
            .unwrap();
        let delete = group
            .prepare_workspace_mutation(
                &directory.handle,
                vec![WorkspaceMutation::Delete {
                    path: workspace_path("renamed.dat").unwrap(),
                    expected_revision: renamed.entry.revision.unwrap(),
                }],
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        group
            .execute_prepared_workspace_mutation(delete, &CancellationToken::new())
            .await
            .unwrap();
        assert!(!root.path().join("renamed.dat").exists());
    }

    #[tokio::test]
    async fn a_publication_failure_rolls_back_earlier_batch_changes() {
        let root = tempdir().unwrap();
        let group = FileToolGroup::new(root.path(), true, None).await.unwrap();
        let directory = group.workspace_root().await.unwrap();
        let prepared = group
            .prepare_workspace_mutation(
                &directory.handle,
                vec![
                    WorkspaceMutation::Create {
                        path: workspace_path("rolled-back.txt").unwrap(),
                        content: MutationContent::new("temporary").unwrap(),
                    },
                    WorkspaceMutation::Mkdir {
                        path: workspace_path("second-directory").unwrap(),
                    },
                ],
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let token = CancellationToken::new();
        let watcher_token = token.clone();
        let published = root.path().join("rolled-back.txt");
        let watcher = tokio::spawn(async move {
            loop {
                if fs::metadata(&published).await.is_ok() {
                    watcher_token.cancel();
                    return;
                }
                tokio::task::yield_now().await;
            }
        });
        let error = group
            .execute_prepared_workspace_mutation(prepared, &token)
            .await
            .unwrap_err();
        watcher.await.unwrap();
        assert_eq!(error.code(), "rolled_back");
        assert!(!root.path().join("rolled-back.txt").exists());
    }

    #[tokio::test]
    async fn native_watch_reports_ordered_external_and_prepared_mutation_events() {
        let root = tempdir().unwrap();
        let group = FileToolGroup::new(root.path(), true, None).await.unwrap();
        let directory = group.workspace_root().await.unwrap();
        let binding = request_binding(directory.handle.clone());
        let watcher = group
            .workspace_open_watch(&WatchOpenRequest {
                version: ContractVersion::V1,
                binding,
                path: workspace_path(".").unwrap(),
                recursive: true,
            })
            .await
            .unwrap();

        fs::write(root.path().join("external.txt"), "one")
            .await
            .unwrap();
        fs::write(root.path().join("external.txt"), "two")
            .await
            .unwrap();
        fs::rename(
            root.path().join("external.txt"),
            root.path().join("renamed.txt"),
        )
        .await
        .unwrap();
        fs::remove_file(root.path().join("renamed.txt"))
            .await
            .unwrap();
        let prepared = group
            .prepare_workspace_mutation(
                &directory.handle,
                vec![WorkspaceMutation::Create {
                    path: workspace_path("mediated.txt").unwrap(),
                    content: MutationContent::new("created").unwrap(),
                }],
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        group
            .execute_prepared_workspace_mutation(prepared, &CancellationToken::new())
            .await
            .unwrap();

        let events = tokio::time::timeout(Duration::from_secs(5), async {
            let mut events = Vec::new();
            loop {
                let batch = watcher.poll(Duration::from_millis(500)).await;
                assert_eq!(batch.failure, None);
                events.extend(batch.events);
                let saw_create = events.iter().any(|event| {
                    event.path.as_str() == "external.txt" && event.kind == WatchEventKind::Create
                });
                let saw_modify = events.iter().any(|event| {
                    event.path.as_str() == "external.txt" && event.kind == WatchEventKind::Modify
                });
                let saw_rename = events.iter().any(|event| {
                    event.path.as_str() == "renamed.txt" || event.kind == WatchEventKind::Rescan
                });
                let saw_mediated = events
                    .iter()
                    .any(|event| event.path.as_str() == "mediated.txt");
                let saw_delete = events.iter().any(|event| {
                    event.path.as_str() == "renamed.txt" && event.kind == WatchEventKind::Remove
                });
                if saw_create && saw_modify && saw_rename && saw_delete && saw_mediated {
                    break events;
                }
            }
        })
        .await
        .expect("bounded native watch wait");
        assert!(
            events
                .iter()
                .all(|event| !event.path.as_str().starts_with('/'))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn project_assets_are_fixed_allowlisted_revision_bound_and_symlink_confined() {
        use std::os::unix::fs::symlink;

        let parent = tempdir().unwrap();
        let root = parent.path().join("root");
        fs::create_dir_all(root.join(".caudra/skills/release"))
            .await
            .unwrap();
        fs::create_dir_all(root.join(".caudra/workflows"))
            .await
            .unwrap();
        fs::create_dir_all(root.join(".caudra/commands/nested"))
            .await
            .unwrap();
        fs::create_dir_all(root.join(".claude/commands"))
            .await
            .unwrap();
        fs::create_dir_all(root.join(".opencode/commands"))
            .await
            .unwrap();
        fs::create_dir_all(root.join(".agents/commands"))
            .await
            .unwrap();
        fs::create_dir_all(root.join(".config/opencode/commands"))
            .await
            .unwrap();
        fs::create_dir_all(root.join(".caudra/plugins"))
            .await
            .unwrap();
        fs::create_dir_all(root.join("nested")).await.unwrap();
        fs::write(root.join("AGENTS.md"), "instructions")
            .await
            .unwrap();
        fs::write(root.join(".caudra/skills/release/SKILL.md"), "skill")
            .await
            .unwrap();
        fs::write(root.join(".caudra/workflows/review.rhai"), "workflow")
            .await
            .unwrap();
        fs::write(root.join(".caudra/commands/review.md"), "command")
            .await
            .unwrap();
        fs::write(root.join(".claude/commands/compat.md"), "command")
            .await
            .unwrap();
        fs::write(root.join(".opencode/commands/build.md"), "command")
            .await
            .unwrap();
        fs::write(
            root.join(".caudra/permissions.toml"),
            "[shell]\ndeny = [\"rm *\"]\nallow = [\"cargo test\"]\n",
        )
        .await
        .unwrap();
        for excluded in [
            ".env",
            ".caudra/init.lua",
            ".caudra/mcp.toml",
            ".caudra/plugins/remote.lua",
            ".caudra/plugins.toml",
            ".caudra/config.toml",
            ".caudra/commands/run.sh",
            ".caudra/commands/nested/deep.md",
            ".caudra/workflows/unsafe.sh",
            ".agents/commands/not-project-compatible.md",
            ".config/opencode/commands/global-only.md",
        ] {
            fs::write(root.join(excluded), "excluded").await.unwrap();
        }
        let outside = parent.path().join("outside.md");
        fs::write(&outside, "outside").await.unwrap();
        symlink(&outside, root.join("nested/AGENTS.md")).unwrap();
        symlink(&outside, root.join(".caudra/commands/outside.md")).unwrap();

        let group = FileToolGroup::new(&root, false, None).await.unwrap();
        let directory = group.workspace_root().await.unwrap();
        let binding = request_binding(directory.handle);
        let discovered = group
            .workspace_discover_project_assets(
                &DiscoverProjectAssetsRequest {
                    version: ContractVersion::V1,
                    binding: binding.clone(),
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let paths = discovered
            .manifest
            .assets
            .iter()
            .map(|asset| asset.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                ".caudra/commands/review.md",
                ".caudra/permissions.toml",
                ".caudra/skills/release/SKILL.md",
                ".caudra/workflows/review.rhai",
                ".claude/commands/compat.md",
                ".opencode/commands/build.md",
                "AGENTS.md",
            ]
        );
        for asset in &discovered.manifest.assets {
            let expected = match asset.kind {
                ProjectAssetKind::Instructions
                | ProjectAssetKind::Skill
                | ProjectAssetKind::Command => ProjectAssetTrust::Declarative,
                ProjectAssetKind::Workflow => ProjectAssetTrust::ClientApprovalRequired,
                ProjectAssetKind::Permissions => ProjectAssetTrust::MixedReviewRequired,
            };
            assert_eq!(asset.trust, expected);
        }
        let workflow = discovered
            .manifest
            .assets
            .iter()
            .find(|asset| asset.kind == ProjectAssetKind::Workflow)
            .unwrap();
        assert_eq!(workflow.trust, ProjectAssetTrust::ClientApprovalRequired);
        let read = group
            .workspace_read_project_asset(
                &ReadProjectAssetRequest {
                    version: ContractVersion::V1,
                    binding: binding.clone(),
                    path: workflow.path.clone(),
                    expected_revision: workflow.revision.clone(),
                    max_bytes: MAX_PROJECT_ASSET_READ_BYTES,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(read.content.as_str(), "workflow");
        assert_eq!(read.encoding, ProjectAssetEncoding::Utf8);

        let permissions = discovered
            .manifest
            .assets
            .iter()
            .find(|asset| asset.kind == ProjectAssetKind::Permissions)
            .unwrap();
        fs::write(
            root.join(".caudra/permissions.toml"),
            "[shell]\ndeny = true\n",
        )
        .await
        .unwrap();
        let stale = group
            .workspace_read_project_asset(
                &ReadProjectAssetRequest {
                    version: ContractVersion::V1,
                    binding: binding.clone(),
                    path: permissions.path.clone(),
                    expected_revision: permissions.revision.clone(),
                    max_bytes: MAX_PROJECT_ASSET_READ_BYTES,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(stale.code(), "stale_resource");
        let changed = group
            .workspace_discover_project_assets(
                &DiscoverProjectAssetsRequest {
                    version: ContractVersion::V1,
                    binding: binding.clone(),
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_ne!(changed.manifest.revision, discovered.manifest.revision);
        assert_ne!(
            changed
                .manifest
                .assets
                .iter()
                .find(|asset| asset.kind == ProjectAssetKind::Permissions)
                .unwrap()
                .revision,
            permissions.revision
        );
        assert!(
            group
                .workspace_read_project_asset(
                    &ReadProjectAssetRequest {
                        version: ContractVersion::V1,
                        binding: binding.clone(),
                        path: workspace_path(".caudra/commands/run.sh").unwrap(),
                        expected_revision: Revision::new("sha256:script").unwrap(),
                        max_bytes: MAX_PROJECT_ASSET_READ_BYTES,
                    },
                    &CancellationToken::new(),
                )
                .await
                .is_err()
        );
        assert!(
            group
                .workspace_read_project_asset(
                    &ReadProjectAssetRequest {
                        version: ContractVersion::V1,
                        binding,
                        path: workspace_path("nested/AGENTS.md").unwrap(),
                        expected_revision: Revision::new("sha256:outside").unwrap(),
                        max_bytes: MAX_PROJECT_ASSET_READ_BYTES,
                    },
                    &CancellationToken::new(),
                )
                .await
                .is_err()
        );
        assert!(
            group
                .workspace_read_project_asset(
                    &ReadProjectAssetRequest {
                        version: ContractVersion::V1,
                        binding: request_binding(group.workspace_root().await.unwrap().handle),
                        path: workspace_path("../AGENTS.md").unwrap(),
                        expected_revision: Revision::new("sha256:outside").unwrap(),
                        max_bytes: MAX_PROJECT_ASSET_READ_BYTES,
                    },
                    &CancellationToken::new(),
                )
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn discovery_names_what_it_cannot_read_and_fails_closed_on_the_permission_policy() {
        use std::os::unix::fs::PermissionsExt;

        let parent = tempdir().unwrap();
        let root = parent.path().join("root");
        for directory in ["locked", "notes", ".caudra"] {
            fs::create_dir_all(root.join(directory)).await.unwrap();
        }
        for file in [
            "AGENTS.md",
            "locked/AGENTS.md",
            "notes/AGENTS.md",
            ".caudra/permissions.toml",
            "private.bin",
        ] {
            fs::write(root.join(file), "[shell]\n").await.unwrap();
        }
        let set_mode = |path: &str, mode: u32| {
            std::fs::set_permissions(root.join(path), std::fs::Permissions::from_mode(mode))
                .unwrap();
        };
        for path in ["locked", "notes/AGENTS.md", "private.bin"] {
            set_mode(path, 0o000);
        }
        if std::fs::read_dir(root.join("locked")).is_ok() {
            // Permission bits do not bind this process, so nothing is unreadable.
            set_mode("locked", 0o755);
            return;
        }
        let group = FileToolGroup::new(&root, false, None).await.unwrap();
        let request = DiscoverProjectAssetsRequest {
            version: ContractVersion::V1,
            binding: request_binding(group.workspace_root().await.unwrap().handle),
        };

        let discovered = group
            .workspace_discover_project_assets(&request, &CancellationToken::new())
            .await
            .unwrap();
        let assets = discovered
            .manifest
            .assets
            .iter()
            .map(|asset| asset.path.as_str())
            .collect::<Vec<_>>();
        let unreadable = discovered
            .manifest
            .unreadable
            .iter()
            .map(WorkspacePath::as_str)
            .collect::<Vec<_>>();
        assert_eq!(assets, [".caudra/permissions.toml", "AGENTS.md"]);
        assert_eq!(unreadable, ["locked", "notes/AGENTS.md"]);

        for (path, locked, restored) in [
            (".caudra/permissions.toml", 0o000, 0o644),
            (".caudra", 0o000, 0o755),
        ] {
            set_mode(path, locked);
            let refused = group
                .workspace_discover_project_assets(&request, &CancellationToken::new())
                .await
                .unwrap_err();
            set_mode(path, restored);
            assert_eq!(refused.code(), "filesystem_permission_denied", "{path}");
        }
        set_mode("locked", 0o755);
    }

    fn workspace_path(value: &str) -> Result<WorkspacePath, WorkspaceError> {
        WorkspacePath::new(value).map_err(|_| WorkspaceError::InvalidRequest)
    }

    fn request_binding(cwd_handle: ResourceId) -> WorkspaceRequestBinding {
        WorkspaceRequestBinding {
            host: HostBinding {
                server_id: Identifier::new("server").unwrap(),
                instance_id: Identifier::new("instance").unwrap(),
                workspace_id: Identifier::new("workspace").unwrap(),
                workspace_generation: Identifier::new("generation").unwrap(),
                root_project_id: Identifier::new("project").unwrap(),
                principal_id: Identifier::new("principal").unwrap(),
                cwd_handle: ResourceId::new("root-cwd").unwrap(),
                catalog_revision: Revision::new("catalog").unwrap(),
                policy_revision: Revision::new("policy").unwrap(),
            },
            cwd_handle,
        }
    }
}
