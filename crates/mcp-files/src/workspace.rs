use std::{
    collections::{HashMap, HashSet, VecDeque},
    mem::size_of,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    time::Duration,
};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher, event::ModifyKind};
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::sync::{Notify, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workcell_host_contract::{
    ContractVersion, Cursor, DirectoryNavigation, DiscoverProjectAssetsRequest,
    DiscoverProjectAssetsResponse, Identifier, ListRequest, ListResponse, MAX_PAGE_SIZE,
    MAX_PROJECT_ASSET_READ_BYTES, MAX_PROJECT_ASSETS, MAX_TEXT_READ_BYTES,
    MAX_WORKSPACE_LIST_ENTRIES, MAX_WORKSPACE_LIST_HASH_BYTES, MAX_WORKSPACE_LIST_RETAINED_BYTES,
    PROJECT_ASSET_MANIFEST_VERSION, ProjectAsset, ProjectAssetContent, ProjectAssetEncoding,
    ProjectAssetKind, ProjectAssetManifest, ProjectAssetTrust, ReadProjectAssetRequest,
    ReadProjectAssetResponse, ReadTextRequest, ReadTextResponse, ResourceId, Revision,
    SearchTextRequest, SearchTextResponse, StatRequest, StatResponse, TextSearchMatch,
    WatchEventKind, WatchOpenRequest, WorkspaceDirectory, WorkspaceEntry, WorkspaceEntryKind,
    WorkspaceMutation, WorkspaceMutationKind, WorkspaceMutationResponse, WorkspaceMutationResult,
    WorkspacePath,
};

use crate::{
    FileGrepInput, FileResource, FileResourceAccess, FileToolGroup, FilesystemError,
    operations::FilesystemCore,
    text::{
        FileVersion, check_cancelled, read_bounded, read_file_version_required,
        read_text_snapshot_required, split_text_lines, validate_snapshot,
    },
};

const MAX_CWD_HANDLES: usize = 256;
const MAX_CURSORS: usize = 256;
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
    WatchUnavailable,
    #[error("no supported repository is available in the workspace")]
    RepositoryUnavailable,
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
    core: Arc<FilesystemCore>,
}

impl WorkspaceSnapshotAccess {
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
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::StaleCwd => "stale_cwd",
            Self::InvalidCursor => "invalid_cursor",
            Self::StaleCursor => "stale_cursor",
            Self::StaleResource => "stale_resource",
            Self::WatchUnavailable => "watch_unavailable",
            Self::RepositoryUnavailable => "repository_unavailable",
            Self::UnsupportedRepository => "unsupported_repository",
            Self::RolledBack(_) => "rolled_back",
            Self::PartialFailure(_) => "partial_failure",
            Self::Filesystem(_) => "filesystem_error",
        }
    }
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

struct WorkspaceListEntries {
    entries: Vec<WorkspaceEntry>,
    truncated: bool,
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

#[derive(Debug, Default)]
pub(crate) struct WorkspaceState {
    inner: Mutex<WorkspaceStateInner>,
}

#[derive(Debug, Default)]
struct WorkspaceStateInner {
    directories: HashMap<ResourceId, DirectoryBinding>,
    cursors: HashMap<Cursor, CursorBinding>,
    cursor_order: VecDeque<Cursor>,
}

#[derive(Clone, Debug)]
struct DirectoryBinding {
    path: PathBuf,
    relative_path: String,
    revision: Revision,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CursorKind {
    List,
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
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
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
        Err(WorkspaceError::RepositoryUnavailable)
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
        let (_, root, _) = self
            .resolve_workspace_path(&request.binding.cwd_handle, &request.path)
            .await?;
        if !fs::metadata(&root)
            .await
            .map_err(|error| {
                FilesystemError::io_path("Cannot inspect workspace list root", &root, error)
            })?
            .is_dir()
        {
            return Err(WorkspaceError::InvalidRequest);
        }
        let listed = self.list_entries(&root, request.recursive, token).await?;
        let mut entries = listed.entries;
        entries.sort_by(|left, right| left.path.as_str().cmp(right.path.as_str()));
        let revision = digest_serializable(&(&entries, listed.truncated))?;
        let request_digest = digest_parts(&[
            request.binding.cwd_handle.as_str(),
            request.path.as_str(),
            if request.recursive {
                "recursive"
            } else {
                "direct"
            },
        ])?;
        let offset = self.cursor_offset(
            request.cursor.as_ref(),
            CursorKind::List,
            &request_digest,
            &revision,
        )?;
        let page_size = request.page_size as usize;
        let end = offset.saturating_add(page_size).min(entries.len());
        let next_cursor = (end < entries.len())
            .then(|| self.insert_cursor(CursorKind::List, request_digest, revision.clone(), end))
            .transpose()?;
        Ok(ListResponse {
            version: ContractVersion::V1,
            revision,
            entries: entries.into_iter().skip(offset).take(page_size).collect(),
            truncated: listed.truncated,
            next_cursor,
        })
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
        .map_err(|_| WorkspaceError::WatchUnavailable)?;
        watcher
            .watch(
                &scope,
                if request.recursive {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                },
            )
            .map_err(|_| WorkspaceError::WatchUnavailable)?;
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
        let entries = self.list_entries(&directory.path, true, token).await?;
        if entries.truncated {
            return Err(WorkspaceError::InvalidRequest);
        }
        let mut assets = Vec::new();
        for entry in entries.entries {
            if entry.kind != WorkspaceEntryKind::File {
                continue;
            }
            let Some((kind, trust)) = project_asset_kind(entry.path.as_str()) else {
                continue;
            };
            assets.push(ProjectAsset {
                path: entry.path,
                resource_id: entry.resource_id,
                revision: entry.revision,
                kind,
                trust,
                size_bytes: entry.size_bytes.unwrap_or(0),
            });
            if assets.len() > MAX_PROJECT_ASSETS {
                return Err(WorkspaceError::InvalidRequest);
            }
        }
        assets.sort_by(|left, right| left.path.as_str().cmp(right.path.as_str()));
        let revision = digest_serializable(&assets)?;
        Ok(DiscoverProjectAssetsResponse {
            version: ContractVersion::V1,
            manifest: ProjectAssetManifest {
                version: Identifier::new(PROJECT_ASSET_MANIFEST_VERSION)
                    .map_err(|_| WorkspaceError::InvalidRequest)?,
                revision,
                assets,
            },
        })
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
            || stat.entry.revision != request.expected_revision
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
        if read.path != request.path || read.revision != stat.entry.revision {
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
            revision,
            kind,
            size_bytes,
        })
    }

    async fn list_entries(
        &self,
        root: &Path,
        recursive: bool,
        token: &CancellationToken,
    ) -> Result<WorkspaceListEntries, WorkspaceError> {
        let mut entries = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        let allows_protected = self.core.policy.traversal_allows_protected(root);
        let mut visited = 0usize;
        let mut retained_bytes = root.as_os_str().len();
        let mut hashed_bytes = 0u64;
        let mut truncated = false;
        'traversal: while let Some(directory) = stack.pop() {
            check_cancelled(token)?;
            let mut reader = fs::read_dir(&directory).await.map_err(|error| {
                FilesystemError::io_path("Cannot list workspace directory", &directory, error)
            })?;
            let mut children = Vec::new();
            let mut stop_after_directory = false;
            while let Some(entry) = reader.next_entry().await.map_err(|error| {
                FilesystemError::io_path("Cannot list workspace directory", &directory, error)
            })? {
                visited = visited.saturating_add(1);
                if visited > self.core.limits.max_traversal_entries
                    || visited > MAX_WORKSPACE_LIST_ENTRIES as usize
                {
                    truncated = true;
                    stop_after_directory = true;
                    break;
                }
                let path = entry.path();
                let file_type = entry.file_type().await.map_err(|error| {
                    FilesystemError::io_path("Cannot inspect workspace entry", &path, error)
                })?;
                if file_type.is_symlink()
                    || !self
                        .core
                        .policy
                        .traversal_entry_allowed(allows_protected, &path)
                    || !self.core.policy.authorize_canonical_entry(&path)
                {
                    continue;
                }
                let prospective = retained_bytes.saturating_add(path.as_os_str().len());
                if prospective > MAX_WORKSPACE_LIST_RETAINED_BYTES as usize {
                    truncated = true;
                    stop_after_directory = true;
                    break;
                }
                retained_bytes = prospective;
                children.push((path, file_type.is_dir()));
            }
            children.sort_by(|left, right| left.0.cmp(&right.0));
            for (path, directory) in &children {
                if !directory {
                    let size = fs::metadata(path)
                        .await
                        .map_err(|error| FilesystemError::io_path("Cannot inspect", path, error))?
                        .len();
                    if hashed_bytes.saturating_add(size) > MAX_WORKSPACE_LIST_HASH_BYTES {
                        truncated = true;
                        break 'traversal;
                    }
                    hashed_bytes = hashed_bytes.saturating_add(size);
                }
                let relative = self.core.policy.relative(path)?;
                let entry = self.workspace_entry(path, relative).await?;
                let entry_bytes = workspace_entry_retained_bytes(&entry);
                if retained_bytes.saturating_add(entry_bytes)
                    > MAX_WORKSPACE_LIST_RETAINED_BYTES as usize
                {
                    truncated = true;
                    break 'traversal;
                }
                retained_bytes = retained_bytes.saturating_add(entry_bytes);
                entries.push(entry);
                if recursive && *directory {
                    stack.push(path.clone());
                }
            }
            if stop_after_directory {
                break 'traversal;
            }
        }
        Ok(WorkspaceListEntries { entries, truncated })
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
    let bytes = read_bounded(path, maximum, token).await?;
    Revision::new(hex_digest(&bytes)).map_err(|_| WorkspaceError::InvalidRequest)
}

async fn directory_revision(path: &Path) -> Result<Revision, WorkspaceError> {
    let metadata = fs::metadata(path).await.map_err(|error| {
        FilesystemError::io_path("Cannot inspect workspace directory", path, error)
    })?;
    if !metadata.is_dir() {
        return Err(WorkspaceError::InvalidRequest);
    }
    let identity = directory_identity(&metadata);
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
        .saturating_add(entry.revision.retained_bytes())
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
    let digest = Sha256::digest(bytes);
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

    use std::fs as std_fs;

    use tempfile::tempdir;
    use workcell_host_contract::{
        HostBinding, Identifier, IncludePattern, ListRequest, MutationContent,
        PrepareMutationRequest, ReadTextRequest, Revision, SearchPattern, SearchTextRequest,
        StatRequest, TextRange, WorkspaceMutation, WorkspacePath, WorkspaceRequestBinding,
    };

    use super::*;
    use crate::FileReadInput;
    use crate::text::install_snapshot_read_hook;

    #[tokio::test]
    async fn workspace_reads_searches_and_pages_with_revision_bound_cursors() {
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
        let stale = group
            .workspace_list(
                &ListRequest {
                    cursor: Some(cursor),
                    ..request
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(stale.code(), "stale_cursor");

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
        assert_eq!(listed.entries.len(), 8);
        assert!(listed.next_cursor.is_none());
    }

    #[tokio::test]
    async fn workspace_list_stops_before_hashing_past_its_aggregate_byte_budget() {
        let root = tempdir().unwrap();
        let file = std_fs::File::create(root.path().join("oversized.txt")).unwrap();
        file.set_len(MAX_WORKSPACE_LIST_HASH_BYTES + 1).unwrap();
        let limits = crate::FilesystemLimits {
            max_file_bytes: (MAX_WORKSPACE_LIST_HASH_BYTES + 1) as usize,
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
                    page_size: 1,
                    cursor: None,
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(listed.truncated);
        assert!(listed.entries.is_empty());
        assert!(listed.next_cursor.is_none());
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
                expected_revision: stat.entry.revision,
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
                    expected_revision: initial.entry.revision,
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
                    expected_revision: renamed.entry.revision,
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
