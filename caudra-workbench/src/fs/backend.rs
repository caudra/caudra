//! Backend-neutral filesystem operations used by the workbench.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::SystemTime;

use async_trait::async_trait;
use caudra_workspace::{
    ContinuationToken, ListRequest, Mutation, MutationCondition, MutationRequest, OperationState,
    ReadBytesRequest, ResourceId, ResourceKind, ResourceRevision, ResourceSelector, SearchRequest,
    WatchCursor, WatchEventPage, WatchOpenRequest, WatchPollRequest, WatchPollState,
    WatchSubscriptionId, WorkspaceError, WorkspaceEvent, WorkspacePath, WorkspaceResource,
    WorkspaceSession, WriteContent,
};
use ignore::WalkBuilder;

use super::read::{self, LineEnding, ReadOnly};

const PAGE_SIZE: u32 = 256;
const WATCH_EVENTS: u32 = 256;
const WATCH_BYTES: u32 = 256 * 1024;
const WATCH_WAIT_MS: u64 = 250;
const MAX_MUTATION_STATUS_POLLS: usize = 8;
const MAX_EDITABLE_BYTES: u64 = 2 * 1024 * 1024;
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
static LOCAL_BACKEND_CALLS: AtomicUsize = AtomicUsize::new(0);

pub type RequestId = u64;
pub type MutationGateFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

#[derive(Clone)]
pub struct MutationGate(Arc<dyn Fn() -> MutationGateFuture + Send + Sync>);

impl MutationGate {
    pub fn new(ensure: impl Fn() -> MutationGateFuture + Send + Sync + 'static) -> Self {
        Self(Arc::new(ensure))
    }

    pub async fn ensure(&self) -> Result<(), String> {
        (self.0)().await
    }

    pub fn allow() -> Self {
        Self::new(|| Box::pin(std::future::ready(Ok(()))))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WorkbenchPath {
    Local(PathBuf),
    Remote(WorkspacePath),
}

impl WorkbenchPath {
    pub fn display(&self) -> String {
        match self {
            Self::Local(path) => path.display().to_string(),
            Self::Remote(path) => path.to_string(),
        }
    }

    pub fn local(&self) -> Option<&Path> {
        match self {
            Self::Local(path) => Some(path),
            Self::Remote(_) => None,
        }
    }

    pub fn remote(&self) -> Option<&WorkspacePath> {
        match self {
            Self::Local(_) => None,
            Self::Remote(path) => Some(path),
        }
    }

    pub fn file_name(&self) -> String {
        match self {
            Self::Local(path) => path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            Self::Remote(path) => path.file_name().to_owned(),
        }
    }

    pub fn parent(&self) -> Option<Self> {
        match self {
            Self::Local(path) => path.parent().map(|path| Self::Local(path.to_path_buf())),
            Self::Remote(path) => path.parent().map(Self::Remote),
        }
    }

    pub fn join(&self, name: &str) -> Result<Self, BackendError> {
        match self {
            Self::Local(path) => Ok(Self::Local(path.join(name))),
            Self::Remote(path) => {
                let joined = if path.is_root() {
                    name.to_owned()
                } else {
                    format!("{path}/{name}")
                };
                WorkspacePath::new(joined)
                    .map(Self::Remote)
                    .map_err(|error| BackendError::Local(error.to_string()))
            }
        }
    }

    pub fn starts_with(&self, parent: &Self) -> bool {
        path_contains(parent, self)
    }

    pub fn ends_with(&self, suffix: impl AsRef<Path>) -> bool {
        self.local().is_some_and(|path| path.ends_with(suffix))
    }

    pub fn display_relative(&self, root: &Self) -> String {
        match (self, root) {
            (Self::Local(path), Self::Local(root)) => path
                .strip_prefix(root)
                .unwrap_or(path)
                .display()
                .to_string(),
            (Self::Remote(path), Self::Remote(root)) if root.is_root() => path.to_string(),
            (Self::Remote(path), Self::Remote(root)) => path
                .as_str()
                .strip_prefix(root.as_str())
                .and_then(|path| path.strip_prefix('/'))
                .unwrap_or(path.as_str())
                .to_owned(),
            _ => self.display(),
        }
    }
}

impl From<PathBuf> for WorkbenchPath {
    fn from(path: PathBuf) -> Self {
        Self::Local(path)
    }
}

impl PartialEq<PathBuf> for WorkbenchPath {
    fn eq(&self, other: &PathBuf) -> bool {
        self.local() == Some(other.as_path())
    }
}

impl PartialEq<WorkbenchPath> for PathBuf {
    fn eq(&self, other: &WorkbenchPath) -> bool {
        other == self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendRevision {
    Local(Option<SystemTime>),
    Remote(ResourceRevision),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceEntry {
    pub path: WorkbenchPath,
    pub resource_id: Option<ResourceId>,
    pub revision: Option<BackendRevision>,
    pub kind: ResourceKind,
    pub size_bytes: Option<u64>,
}

impl ResourceEntry {
    pub fn remote(resource: WorkspaceResource) -> Result<Self, BackendError> {
        let path = resource.path.ok_or(BackendError::InvalidResponse)?;
        Ok(Self {
            path: WorkbenchPath::Remote(path),
            resource_id: Some(resource.scope.resource_id().clone()),
            revision: resource.revision.map(BackendRevision::Remote),
            kind: resource.kind,
            size_bytes: resource.size_bytes,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListResult {
    pub entries: Vec<ResourceEntry>,
    pub continuation: Option<ContinuationToken>,
    pub incomplete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedFile {
    pub entry: ResourceEntry,
    pub lines: Vec<String>,
    pub line_ending: LineEnding,
    pub trailing_newline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub entry: ResourceEntry,
    pub line: u32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub hits: Vec<SearchMatch>,
    pub continuation: Option<ContinuationToken>,
    pub truncated: bool,
    pub incomplete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchHandle {
    pub subscription_id: WatchSubscriptionId,
    pub cursor: WatchCursor,
    next_sequence: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchUpdate {
    Events(Vec<WorkspaceEvent>),
    Resync,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchResult {
    pub handle: WatchHandle,
    pub update: WatchUpdate,
}

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("filesystem operation is for the wrong workspace backend")]
    WrongBackend,
    #[error("resource is not a file")]
    NotFile,
    #[error("binary files cannot be edited")]
    Binary,
    #[error("file exceeds the editable size limit")]
    TooLarge,
    #[error("file is not valid UTF-8")]
    NotUtf8,
    #[error("workspace returned truncated file content")]
    Truncated,
    #[error("resource has no revision for a conditional operation")]
    MissingRevision,
    #[error("workspace response was invalid")]
    InvalidResponse,
    #[error("file changed since it was opened")]
    Conflict,
    #[error("workspace mutation outcome is indeterminate; refresh before editing again")]
    Indeterminate,
    #[error("workspace mutation blocked: {0}")]
    MutationBlocked(String),
    #[error("{0}")]
    Local(String),
    #[error("{0}")]
    Workspace(WorkspaceError),
}

impl From<WorkspaceError> for BackendError {
    fn from(error: WorkspaceError) -> Self {
        match error {
            WorkspaceError::Conflict | WorkspaceError::StaleResource { .. } => Self::Conflict,
            WorkspaceError::IndeterminateOutcome => Self::Indeterminate,
            error => Self::Workspace(error),
        }
    }
}

#[async_trait]
pub trait WorkbenchFilesystem: Send + Sync {
    fn is_remote(&self) -> bool;

    async fn list(
        &self,
        parent: &WorkbenchPath,
        recursive: bool,
        continuation: Option<ContinuationToken>,
    ) -> Result<ListResult, BackendError>;

    async fn read(&self, entry: &ResourceEntry) -> Result<LoadedFile, BackendError>;

    async fn write(
        &self,
        entry: &ResourceEntry,
        contents: String,
    ) -> Result<ResourceEntry, BackendError>;

    async fn create_file(&self, path: &WorkbenchPath) -> Result<ResourceEntry, BackendError>;
    async fn create_dir(&self, path: &WorkbenchPath) -> Result<ResourceEntry, BackendError>;
    async fn rename(
        &self,
        entry: &ResourceEntry,
        destination: &WorkbenchPath,
    ) -> Result<ResourceEntry, BackendError>;
    async fn delete(&self, entry: &ResourceEntry) -> Result<(), BackendError>;
    async fn count(&self, parent: &WorkbenchPath) -> Result<usize, BackendError>;

    async fn search(
        &self,
        query: String,
        include: Option<String>,
        continuation: Option<ContinuationToken>,
    ) -> Result<SearchResult, BackendError>;

    async fn watch_open(&self) -> Result<Option<WatchHandle>, BackendError>;
    async fn watch_poll(&self, handle: WatchHandle) -> Result<WatchResult, BackendError>;
    async fn watch_close(&self, handle: WatchHandle) -> Result<(), BackendError>;
}

#[derive(Clone)]
pub enum WorkbenchBackend {
    Local(Arc<LocalFilesystem>),
    Workspace(Arc<WorkspaceFilesystem>),
    Custom(Arc<dyn WorkbenchFilesystem>),
}

impl WorkbenchBackend {
    pub fn local(root: PathBuf, show_hidden: bool) -> Self {
        Self::Local(Arc::new(LocalFilesystem::new(root, show_hidden)))
    }

    pub fn workspace(session: WorkspaceSession) -> Result<Self, BackendError> {
        Self::workspace_with_gate(session, MutationGate::allow())
    }

    pub fn workspace_with_gate(
        session: WorkspaceSession,
        gate: MutationGate,
    ) -> Result<Self, BackendError> {
        WorkspaceFilesystem::new_with_gate(session, gate)
            .map(|backend| Self::Workspace(Arc::new(backend)))
    }

    pub fn custom(backend: Arc<dyn WorkbenchFilesystem>) -> Self {
        Self::Custom(backend)
    }

    pub fn filesystem(&self) -> Arc<dyn WorkbenchFilesystem> {
        match self {
            Self::Local(backend) => backend.clone(),
            Self::Workspace(backend) => backend.clone(),
            Self::Custom(backend) => backend.clone(),
        }
    }
}

#[derive(Debug)]
pub enum BackendEvent {
    Listed {
        request: RequestId,
        parent: WorkbenchPath,
        result: Result<ListResult, BackendError>,
    },
    Opened {
        request: RequestId,
        result: Result<LoadedFile, BackendError>,
    },
    Saved {
        request: RequestId,
        result: Result<ResourceEntry, BackendError>,
    },
    Created {
        request: RequestId,
        result: Result<ResourceEntry, BackendError>,
    },
    Renamed {
        request: RequestId,
        source: WorkbenchPath,
        result: Result<ResourceEntry, BackendError>,
    },
    Deleted {
        request: RequestId,
        path: WorkbenchPath,
        result: Result<(), BackendError>,
    },
    Counted {
        request: RequestId,
        result: Result<usize, BackendError>,
    },
    SearchPage {
        request: RequestId,
        result: Result<SearchResult, BackendError>,
    },
    WatchOpened(Result<Option<WatchHandle>, BackendError>),
    WatchPolled(Result<WatchResult, BackendError>),
}

struct Envelope {
    generation: u64,
    event: BackendEvent,
}

/// Runs backend futures away from rendering and admits only responses from the
/// current binding and request generation.
pub struct BackendDriver {
    backend: Arc<dyn WorkbenchFilesystem>,
    root: WorkbenchPath,
    generation: u64,
    next_request: RequestId,
    active_search: RequestId,
    search_task: Option<smol::Task<()>>,
    cancelled: HashSet<RequestId>,
    resources: HashMap<WorkbenchPath, ResourceEntry>,
    sender: flume::Sender<Envelope>,
    events: flume::Receiver<Envelope>,
    watch: Option<WatchHandle>,
    watch_cancel: Arc<AtomicBool>,
    stale: bool,
}

impl BackendDriver {
    pub fn new(backend: WorkbenchBackend, root: WorkbenchPath) -> Self {
        let (sender, events) = flume::unbounded();
        Self {
            backend: backend.filesystem(),
            root,
            generation: 0,
            next_request: 1,
            active_search: 0,
            search_task: None,
            cancelled: HashSet::new(),
            resources: HashMap::new(),
            sender,
            events,
            watch: None,
            watch_cancel: Arc::new(AtomicBool::new(false)),
            stale: false,
        }
    }

    pub fn root(&self) -> &WorkbenchPath {
        &self.root
    }

    pub fn is_remote(&self) -> bool {
        self.backend.is_remote()
    }

    pub fn is_stale(&self) -> bool {
        self.stale
    }

    pub fn resource(&self, path: &WorkbenchPath) -> Option<&ResourceEntry> {
        self.resources.get(path)
    }

    pub fn clear_stale(&mut self) {
        self.stale = false;
    }

    pub fn rebind(&mut self, backend: WorkbenchBackend, root: WorkbenchPath) {
        self.search_task = None;
        self.close_watch();
        self.watch_cancel = Arc::new(AtomicBool::new(false));
        self.generation = self.generation.wrapping_add(1);
        self.backend = backend.filesystem();
        self.root = root;
        self.cancelled.clear();
        self.resources.clear();
        self.active_search = 0;
        self.stale = true;
    }

    pub fn suspend(&mut self) {
        self.search_task = None;
        self.close_watch();
        self.generation = self.generation.wrapping_add(1);
        self.cancelled.clear();
        self.resources.clear();
        self.active_search = 0;
        self.stale = true;
    }

    pub fn list(&mut self, parent: WorkbenchPath, recursive: bool) -> RequestId {
        let request = self.request_id();
        let backend = Arc::clone(&self.backend);
        let sender = self.sender.clone();
        let generation = self.generation;
        smol::spawn(async move {
            let mut continuation = None;
            let mut seen = HashSet::new();
            let mut combined = ListResult {
                entries: Vec::new(),
                continuation: None,
                incomplete: false,
            };
            let result = loop {
                match backend.list(&parent, recursive, continuation).await {
                    Ok(page) => {
                        combined.entries.extend(page.entries);
                        combined.incomplete |= page.incomplete;
                        let Some(next) = page.continuation else {
                            break Ok(combined);
                        };
                        if !seen.insert(next.clone()) {
                            break Err(BackendError::InvalidResponse);
                        }
                        continuation = Some(next);
                    }
                    Err(error) => break Err(error),
                }
            };
            let _ = sender.send(Envelope {
                generation,
                event: BackendEvent::Listed {
                    request,
                    parent,
                    result,
                },
            });
        })
        .detach();
        request
    }

    pub fn open(&mut self, entry: ResourceEntry) -> RequestId {
        let request = self.request_id();
        let backend = Arc::clone(&self.backend);
        self.spawn(request, async move {
            BackendEvent::Opened {
                request,
                result: backend.read(&entry).await,
            }
        });
        request
    }

    pub fn save(&mut self, entry: ResourceEntry, contents: String) -> RequestId {
        let request = self.request_id();
        let backend = Arc::clone(&self.backend);
        self.spawn(request, async move {
            BackendEvent::Saved {
                request,
                result: backend.write(&entry, contents).await,
            }
        });
        request
    }

    pub fn create_file(&mut self, path: WorkbenchPath) -> RequestId {
        self.create(path, false)
    }

    pub fn create_dir(&mut self, path: WorkbenchPath) -> RequestId {
        self.create(path, true)
    }

    fn create(&mut self, path: WorkbenchPath, directory: bool) -> RequestId {
        let request = self.request_id();
        let backend = Arc::clone(&self.backend);
        self.spawn(request, async move {
            let result = if directory {
                backend.create_dir(&path).await
            } else {
                backend.create_file(&path).await
            };
            BackendEvent::Created { request, result }
        });
        request
    }

    pub fn rename(&mut self, entry: ResourceEntry, destination: WorkbenchPath) -> RequestId {
        let request = self.request_id();
        let source = entry.path.clone();
        let backend = Arc::clone(&self.backend);
        self.spawn(request, async move {
            BackendEvent::Renamed {
                request,
                source,
                result: backend.rename(&entry, &destination).await,
            }
        });
        request
    }

    pub fn delete(&mut self, entry: ResourceEntry) -> RequestId {
        let request = self.request_id();
        let path = entry.path.clone();
        let backend = Arc::clone(&self.backend);
        self.spawn(request, async move {
            BackendEvent::Deleted {
                request,
                path,
                result: backend.delete(&entry).await,
            }
        });
        request
    }

    pub fn count(&mut self, parent: WorkbenchPath) -> RequestId {
        let request = self.request_id();
        let backend = Arc::clone(&self.backend);
        self.spawn(request, async move {
            BackendEvent::Counted {
                request,
                result: backend.count(&parent).await,
            }
        });
        request
    }

    pub fn search(&mut self, query: String, include: Option<String>) -> RequestId {
        self.search_task = None;
        let request = self.request_id();
        self.active_search = request;
        let backend = Arc::clone(&self.backend);
        let sender = self.sender.clone();
        let generation = self.generation;
        self.search_task = Some(smol::spawn(async move {
            let mut continuation = None;
            let mut seen = HashSet::new();
            let mut combined = SearchResult {
                hits: Vec::new(),
                continuation: None,
                truncated: false,
                incomplete: false,
            };
            let event = loop {
                match backend
                    .search(query.clone(), include.clone(), continuation)
                    .await
                {
                    Ok(page) => {
                        combined.hits.extend(page.hits);
                        combined.truncated |= page.truncated;
                        combined.incomplete |= page.incomplete;
                        let Some(next) = page.continuation else {
                            break BackendEvent::SearchPage {
                                request,
                                result: Ok(combined),
                            };
                        };
                        if !seen.insert(next.clone()) {
                            break BackendEvent::SearchPage {
                                request,
                                result: Err(BackendError::InvalidResponse),
                            };
                        }
                        continuation = Some(next);
                    }
                    Err(error) => {
                        break BackendEvent::SearchPage {
                            request,
                            result: Err(error),
                        };
                    }
                }
            };
            let _ = sender.send(Envelope { generation, event });
        }));
        request
    }

    pub fn cancel(&mut self, request: RequestId) {
        if self.active_search == request {
            self.search_task = None;
            self.active_search = 0;
            return;
        }
        self.cancelled.insert(request);
    }

    pub fn open_watch(&mut self) {
        if self.watch_cancel.load(Ordering::Acquire) {
            self.watch_cancel = Arc::new(AtomicBool::new(false));
        }
        let backend = Arc::clone(&self.backend);
        let sender = self.sender.clone();
        let generation = self.generation;
        let cancel = Arc::clone(&self.watch_cancel);
        smol::spawn(async move {
            let result = backend.watch_open().await;
            if cancel.load(Ordering::Acquire) {
                if let Ok(Some(handle)) = result {
                    let _ = backend.watch_close(handle).await;
                }
                return;
            }
            let _ = sender.send(Envelope {
                generation,
                event: BackendEvent::WatchOpened(result),
            });
        })
        .detach();
    }

    pub fn close_watch(&mut self) {
        self.watch_cancel.store(true, Ordering::Release);
        let Some(handle) = self.watch.take() else {
            return;
        };
        let backend = Arc::clone(&self.backend);
        smol::spawn(async move {
            let _ = backend.watch_close(handle).await;
        })
        .detach();
    }

    pub fn drain(&mut self) -> Vec<BackendEvent> {
        let mut admitted = Vec::new();
        let mut refresh = false;
        let mut reopen_watch = false;
        for envelope in self.events.try_iter() {
            if envelope.generation != self.generation {
                continue;
            }
            if matches!(&envelope.event, BackendEvent::SearchPage { request, .. } if *request != self.active_search)
            {
                continue;
            }
            let request = event_request(&envelope.event);
            if request.is_some_and(|request| self.cancelled.remove(&request)) {
                continue;
            }
            match &envelope.event {
                BackendEvent::Listed {
                    result: Ok(page), ..
                } => {
                    for entry in &page.entries {
                        self.resources.insert(entry.path.clone(), entry.clone());
                    }
                    self.stale = false;
                }
                BackendEvent::Opened {
                    result: Ok(LoadedFile { entry: file, .. }),
                    ..
                }
                | BackendEvent::Saved {
                    result: Ok(file), ..
                }
                | BackendEvent::Created {
                    result: Ok(file), ..
                } => {
                    self.resources.insert(file.path.clone(), file.clone());
                }
                BackendEvent::Renamed {
                    source,
                    result: Ok(entry),
                    ..
                } => {
                    self.resources.remove(source);
                    self.resources.insert(entry.path.clone(), entry.clone());
                }
                BackendEvent::Deleted {
                    path,
                    result: Ok(()),
                    ..
                } => {
                    self.resources
                        .retain(|candidate, _| !path_contains(path, candidate));
                }
                BackendEvent::WatchOpened(Ok(Some(handle))) => {
                    self.watch = Some(handle.clone());
                    self.poll_watch(handle.clone());
                }
                BackendEvent::WatchPolled(Ok(result)) => {
                    if matches!(result.update, WatchUpdate::Resync) {
                        self.watch = None;
                        self.resources.clear();
                        self.stale = true;
                        refresh = true;
                        reopen_watch = true;
                    } else {
                        self.watch = Some(result.handle.clone());
                        if matches!(&result.update, WatchUpdate::Events(events) if !events.is_empty())
                        {
                            self.resources.clear();
                            self.stale = true;
                            refresh = true;
                        }
                        self.poll_watch(result.handle.clone());
                    }
                }
                BackendEvent::WatchPolled(Err(_)) => {
                    self.watch = None;
                    self.resources.clear();
                    self.stale = true;
                    refresh = true;
                    reopen_watch = true;
                }
                _ => {}
            }
            admitted.push(envelope.event);
        }
        if refresh {
            self.list(self.root.clone(), true);
        }
        if reopen_watch {
            self.watch_cancel = Arc::new(AtomicBool::new(false));
            self.open_watch();
        }
        admitted
    }

    fn poll_watch(&self, handle: WatchHandle) {
        if self.watch_cancel.load(Ordering::Acquire) {
            return;
        }
        let backend = Arc::clone(&self.backend);
        let sender = self.sender.clone();
        let generation = self.generation;
        let cancel = Arc::clone(&self.watch_cancel);
        smol::spawn(async move {
            let result = backend.watch_poll(handle).await;
            if !cancel.load(Ordering::Acquire) {
                let _ = sender.send(Envelope {
                    generation,
                    event: BackendEvent::WatchPolled(result),
                });
            }
        })
        .detach();
    }

    fn request_id(&mut self) -> RequestId {
        let request = self.next_request;
        self.next_request = self.next_request.wrapping_add(1).max(1);
        request
    }

    fn spawn<F>(&self, _request: RequestId, future: F)
    where
        F: Future<Output = BackendEvent> + Send + 'static,
    {
        let sender = self.sender.clone();
        let generation = self.generation;
        smol::spawn(async move {
            let event = future.await;
            let _ = sender.send(Envelope { generation, event });
        })
        .detach();
    }
}

impl Drop for BackendDriver {
    fn drop(&mut self) {
        self.close_watch();
    }
}

#[derive(Debug, Clone)]
pub struct LocalFilesystem {
    root: PathBuf,
    show_hidden: bool,
}

impl LocalFilesystem {
    pub fn new(root: PathBuf, show_hidden: bool) -> Self {
        Self { root, show_hidden }
    }

    pub fn call_count() -> usize {
        LOCAL_BACKEND_CALLS.load(Ordering::Acquire)
    }

    pub fn reset_call_count() {
        LOCAL_BACKEND_CALLS.store(0, Ordering::Release);
    }

    fn record_call() {
        LOCAL_BACKEND_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    fn local<'a>(&self, path: &'a WorkbenchPath) -> Result<&'a Path, BackendError> {
        let path = path.local().ok_or(BackendError::WrongBackend)?;
        path.starts_with(&self.root)
            .then_some(path)
            .ok_or(BackendError::WrongBackend)
    }

    fn entry(&self, path: PathBuf) -> Result<ResourceEntry, BackendError> {
        let metadata = fs::metadata(&path).map_err(local_error)?;
        let kind = if metadata.is_dir() {
            ResourceKind::Directory
        } else if metadata.is_file() {
            ResourceKind::File
        } else if metadata.file_type().is_symlink() {
            ResourceKind::Symlink
        } else {
            ResourceKind::Other
        };
        Ok(ResourceEntry {
            path: WorkbenchPath::Local(path),
            resource_id: None,
            revision: Some(BackendRevision::Local(metadata.modified().ok())),
            kind,
            size_bytes: Some(metadata.len()),
        })
    }
}

#[async_trait]
impl WorkbenchFilesystem for LocalFilesystem {
    fn is_remote(&self) -> bool {
        false
    }

    async fn list(
        &self,
        parent: &WorkbenchPath,
        recursive: bool,
        _continuation: Option<ContinuationToken>,
    ) -> Result<ListResult, BackendError> {
        Self::record_call();
        let parent = self.local(parent)?;
        let depth = (!recursive).then_some(1);
        let mut entries = WalkBuilder::new(parent)
            .max_depth(depth)
            .hidden(!self.show_hidden)
            .filter_entry(|entry| entry.file_name() != ".git")
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.path() != parent)
            .map(|entry| self.entry(entry.into_path()))
            .collect::<Result<Vec<_>, _>>()?;
        sort_entries(&mut entries);
        Ok(ListResult {
            entries,
            continuation: None,
            incomplete: false,
        })
    }

    async fn read(&self, entry: &ResourceEntry) -> Result<LoadedFile, BackendError> {
        Self::record_call();
        let path = self.local(&entry.path)?;
        if entry.kind != ResourceKind::File {
            return Err(BackendError::NotFile);
        }
        let loaded = read::load(path).map_err(|error| BackendError::Local(error.to_string()))?;
        if let Some(notice) = loaded.read_only {
            return Err(match notice {
                ReadOnly::Binary => BackendError::Binary,
                ReadOnly::TooLarge => BackendError::TooLarge,
                ReadOnly::NotUtf8 => BackendError::NotUtf8,
            });
        }
        Ok(LoadedFile {
            entry: self.entry(path.to_path_buf())?,
            lines: loaded.lines,
            line_ending: loaded.line_ending,
            trailing_newline: loaded.trailing_newline,
        })
    }

    async fn write(
        &self,
        entry: &ResourceEntry,
        contents: String,
    ) -> Result<ResourceEntry, BackendError> {
        Self::record_call();
        let path = self.local(&entry.path)?;
        let expected = match &entry.revision {
            Some(BackendRevision::Local(modified)) => *modified,
            _ => return Err(BackendError::MissingRevision),
        };
        read::save(path, &contents, expected).map_err(|error| match error {
            read::SaveError::Stale(_) => BackendError::Conflict,
            read::SaveError::Unconfirmed { .. } => BackendError::Indeterminate,
            error => BackendError::Local(error.to_string()),
        })?;
        self.entry(path.to_path_buf())
    }

    async fn create_file(&self, path: &WorkbenchPath) -> Result<ResourceEntry, BackendError> {
        Self::record_call();
        let path = self.local(path)?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(local_error)?;
        self.entry(path.to_path_buf())
    }

    async fn create_dir(&self, path: &WorkbenchPath) -> Result<ResourceEntry, BackendError> {
        Self::record_call();
        let path = self.local(path)?;
        fs::create_dir(path).map_err(local_error)?;
        self.entry(path.to_path_buf())
    }

    async fn rename(
        &self,
        entry: &ResourceEntry,
        destination: &WorkbenchPath,
    ) -> Result<ResourceEntry, BackendError> {
        Self::record_call();
        let source = self.local(&entry.path)?;
        let destination = self.local(destination)?;
        fs::rename(source, destination).map_err(local_error)?;
        self.entry(destination.to_path_buf())
    }

    async fn delete(&self, entry: &ResourceEntry) -> Result<(), BackendError> {
        Self::record_call();
        let path = self.local(&entry.path)?;
        if entry.kind == ResourceKind::Directory {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        }
        .map_err(local_error)
    }

    async fn count(&self, parent: &WorkbenchPath) -> Result<usize, BackendError> {
        Self::record_call();
        Ok(self.list(parent, true, None).await?.entries.len())
    }

    async fn search(
        &self,
        query: String,
        include: Option<String>,
        _continuation: Option<ContinuationToken>,
    ) -> Result<SearchResult, BackendError> {
        Self::record_call();
        let mut hits = Vec::new();
        for entry in self
            .list(&WorkbenchPath::Local(self.root.clone()), true, None)
            .await?
            .entries
        {
            if entry.kind != ResourceKind::File
                || include.as_ref().is_some_and(|glob| {
                    !entry
                        .display_relative(&self.root)
                        .contains(glob.trim_matches('*'))
                })
            {
                continue;
            }
            let Ok(loaded) = self.read(&entry).await else {
                continue;
            };
            for (index, line) in loaded.lines.iter().enumerate() {
                if line.contains(&query) {
                    hits.push(SearchMatch {
                        entry: entry.clone(),
                        line: u32::try_from(index + 1).unwrap_or(u32::MAX),
                        text: line.clone(),
                    });
                }
            }
        }
        Ok(SearchResult {
            hits,
            continuation: None,
            truncated: false,
            incomplete: false,
        })
    }

    async fn watch_open(&self) -> Result<Option<WatchHandle>, BackendError> {
        Self::record_call();
        Ok(None)
    }

    async fn watch_poll(&self, _handle: WatchHandle) -> Result<WatchResult, BackendError> {
        Self::record_call();
        Err(BackendError::WrongBackend)
    }

    async fn watch_close(&self, _handle: WatchHandle) -> Result<(), BackendError> {
        Self::record_call();
        Ok(())
    }
}

impl ResourceEntry {
    fn display_relative(&self, root: &Path) -> String {
        match &self.path {
            WorkbenchPath::Local(path) => path
                .strip_prefix(root)
                .unwrap_or(path)
                .display()
                .to_string(),
            WorkbenchPath::Remote(path) => path.to_string(),
        }
    }
}

#[derive(Clone)]
pub struct WorkspaceFilesystem {
    session: WorkspaceSession,
    gate: MutationGate,
}

impl WorkspaceFilesystem {
    pub fn new(session: WorkspaceSession) -> Result<Self, BackendError> {
        Self::new_with_gate(session, MutationGate::allow())
    }

    pub fn new_with_gate(
        session: WorkspaceSession,
        gate: MutationGate,
    ) -> Result<Self, BackendError> {
        let services = session.workspace().services();
        if services.read.is_none() || services.mutation.is_none() || services.search.is_none() {
            return Err(BackendError::Workspace(WorkspaceError::Unavailable));
        }
        Ok(Self { session, gate })
    }

    fn read_service(
        &self,
    ) -> Result<&Arc<dyn caudra_workspace::WorkspaceReadService>, BackendError> {
        self.session
            .workspace()
            .services()
            .read
            .as_ref()
            .ok_or(BackendError::Workspace(WorkspaceError::Unavailable))
    }

    fn mutation_service(
        &self,
    ) -> Result<&Arc<dyn caudra_workspace::WorkspaceMutationService>, BackendError> {
        self.session
            .workspace()
            .services()
            .mutation
            .as_ref()
            .ok_or(BackendError::Workspace(WorkspaceError::Unavailable))
    }

    fn remote<'a>(&self, path: &'a WorkbenchPath) -> Result<&'a WorkspacePath, BackendError> {
        path.remote().ok_or(BackendError::WrongBackend)
    }

    async fn resolve_entry(&self, path: &WorkspacePath) -> Result<ResourceEntry, BackendError> {
        let resource = self
            .read_service()?
            .resolve(self.session.binding(), self.session.cursor(), path)
            .await?;
        ResourceEntry::remote(resource)
    }

    async fn mutate(&self, mutation: Mutation) -> Result<Option<ResourceRevision>, BackendError> {
        self.gate
            .ensure()
            .await
            .map_err(BackendError::MutationBlocked)?;
        let service = Arc::clone(self.mutation_service()?);
        let request = MutationRequest {
            mutations: vec![mutation],
        };
        let mut status = service
            .execute(self.session.binding(), self.session.cursor(), &request)
            .await?;
        for _ in 0..MAX_MUTATION_STATUS_POLLS {
            match status.state {
                OperationState::Completed { result, .. } => {
                    if !result.committed || result.results.len() != 1 {
                        return Err(BackendError::InvalidResponse);
                    }
                    return Ok(result
                        .results
                        .into_iter()
                        .next()
                        .and_then(|entry| entry.revision));
                }
                OperationState::Failed { .. } => return Err(BackendError::Conflict),
                OperationState::Cancelled { .. } => {
                    return Err(BackendError::Workspace(WorkspaceError::Cancelled));
                }
                OperationState::Indeterminate { .. } => return Err(BackendError::Indeterminate),
                OperationState::Forgotten | OperationState::NeverSeen => {
                    return Err(BackendError::InvalidResponse);
                }
                OperationState::Prepared | OperationState::Running => {
                    smol::future::yield_now().await;
                    status = service
                        .status(
                            self.session.binding(),
                            self.session.cursor(),
                            &status.handle,
                        )
                        .await?;
                }
            }
        }
        Err(BackendError::Indeterminate)
    }
}

#[async_trait]
impl WorkbenchFilesystem for WorkspaceFilesystem {
    fn is_remote(&self) -> bool {
        true
    }

    async fn list(
        &self,
        parent: &WorkbenchPath,
        recursive: bool,
        continuation: Option<ContinuationToken>,
    ) -> Result<ListResult, BackendError> {
        let page = self
            .read_service()?
            .list(
                self.session.binding(),
                self.session.cursor(),
                &ListRequest {
                    parent: ResourceSelector::Path(self.remote(parent)?.clone()),
                    recursive,
                    continuation,
                    limit: PAGE_SIZE,
                },
            )
            .await?;
        let mut entries = page
            .resources
            .into_iter()
            .map(ResourceEntry::remote)
            .collect::<Result<Vec<_>, _>>()?;
        sort_entries(&mut entries);
        Ok(ListResult {
            entries,
            continuation: page.continuation,
            incomplete: page.incomplete,
        })
    }

    /// Reads what the resource says now. A listing is a snapshot of a workspace
    /// another writer is still working in, so conditioning the read on the
    /// revision it recorded refuses the open of every file that moved since,
    /// and the tab that would carry the current revision never exists to
    /// recover with. Identity stays pinned to the resource id, and the revision
    /// the read reports is what a later save is conditional on.
    async fn read(&self, entry: &ResourceEntry) -> Result<LoadedFile, BackendError> {
        if entry.kind != ResourceKind::File {
            return Err(BackendError::NotFile);
        }
        if entry
            .size_bytes
            .is_some_and(|size| size > MAX_EDITABLE_BYTES)
        {
            return Err(BackendError::TooLarge);
        }
        let resource_id = entry
            .resource_id
            .clone()
            .ok_or(BackendError::InvalidResponse)?;
        let content = self
            .read_service()?
            .read_bytes(
                self.session.binding(),
                self.session.cursor(),
                &ReadBytesRequest {
                    resource: ResourceSelector::Id(resource_id.clone()),
                    byte_offset: 0,
                    max_bytes: MAX_EDITABLE_BYTES + 1,
                    if_revision: None,
                },
            )
            .await?;
        if content.resource_id != resource_id {
            return Err(BackendError::InvalidResponse);
        }
        if content.range.start != 0
            || content.range.end_exclusive != content.bytes.len() as u64
            || content
                .total_bytes
                .is_some_and(|total| total != content.bytes.len() as u64)
        {
            return Err(BackendError::InvalidResponse);
        }
        if content.truncated || content.next_byte_offset.is_some() {
            return Err(BackendError::Truncated);
        }
        if content.bytes.len() as u64 > MAX_EDITABLE_BYTES {
            return Err(BackendError::TooLarge);
        }
        if content.bytes[..content.bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
            return Err(BackendError::Binary);
        }
        let text = String::from_utf8(content.bytes).map_err(|_| BackendError::NotUtf8)?;
        let (lines, line_ending, trailing_newline) = decode_text(&text);
        Ok(LoadedFile {
            entry: ResourceEntry {
                path: entry.path.clone(),
                resource_id: Some(content.resource_id),
                revision: Some(BackendRevision::Remote(content.revision)),
                kind: ResourceKind::File,
                size_bytes: content.total_bytes,
            },
            lines,
            line_ending,
            trailing_newline,
        })
    }

    async fn write(
        &self,
        entry: &ResourceEntry,
        contents: String,
    ) -> Result<ResourceEntry, BackendError> {
        let revision = match &entry.revision {
            Some(BackendRevision::Remote(revision)) => revision.clone(),
            _ => return Err(BackendError::MissingRevision),
        };
        let path = self.remote(&entry.path)?.clone();
        let next = self
            .mutate(Mutation::Write {
                path: path.clone(),
                content: WriteContent::Text(contents),
                condition: MutationCondition::Matches(revision),
            })
            .await?
            .ok_or(BackendError::InvalidResponse)?;
        Ok(ResourceEntry {
            revision: Some(BackendRevision::Remote(next)),
            ..entry.clone()
        })
    }

    async fn create_file(&self, path: &WorkbenchPath) -> Result<ResourceEntry, BackendError> {
        let path = self.remote(path)?.clone();
        self.mutate(Mutation::Write {
            path: path.clone(),
            content: WriteContent::Text(String::new()),
            condition: MutationCondition::MustNotExist,
        })
        .await?;
        self.resolve_entry(&path).await
    }

    async fn create_dir(&self, path: &WorkbenchPath) -> Result<ResourceEntry, BackendError> {
        let path = self.remote(path)?.clone();
        self.mutate(Mutation::CreateDirectory { path: path.clone() })
            .await?;
        self.resolve_entry(&path).await
    }

    async fn rename(
        &self,
        entry: &ResourceEntry,
        destination: &WorkbenchPath,
    ) -> Result<ResourceEntry, BackendError> {
        let expected_revision = remote_revision(entry)?;
        let source = self.remote(&entry.path)?.clone();
        let destination = self.remote(destination)?.clone();
        self.mutate(Mutation::Move {
            source,
            destination: destination.clone(),
            expected_revision,
        })
        .await?;
        self.resolve_entry(&destination).await
    }

    async fn delete(&self, entry: &ResourceEntry) -> Result<(), BackendError> {
        self.mutate(Mutation::Remove {
            path: self.remote(&entry.path)?.clone(),
            expected_revision: remote_revision(entry)?,
        })
        .await?;
        Ok(())
    }

    async fn count(&self, parent: &WorkbenchPath) -> Result<usize, BackendError> {
        let mut continuation = None;
        let mut count = 0;
        let mut seen = HashSet::new();
        loop {
            let page = self.list(parent, true, continuation).await?;
            count += page.entries.len();
            let Some(next) = page.continuation else {
                return Ok(count);
            };
            if !seen.insert(next.clone()) {
                return Err(BackendError::InvalidResponse);
            }
            continuation = Some(next);
        }
    }

    async fn search(
        &self,
        query: String,
        include: Option<String>,
        continuation: Option<ContinuationToken>,
    ) -> Result<SearchResult, BackendError> {
        let service = self
            .session
            .workspace()
            .services()
            .search
            .as_ref()
            .ok_or(BackendError::Workspace(WorkspaceError::Unavailable))?;
        let page = service
            .search(
                self.session.binding(),
                self.session.cursor(),
                &SearchRequest {
                    query,
                    include,
                    root: ResourceSelector::Current,
                    max_results: PAGE_SIZE,
                    continuation,
                },
            )
            .await?;
        Ok(SearchResult {
            hits: page
                .hits
                .into_iter()
                .map(|hit| {
                    Ok(SearchMatch {
                        entry: ResourceEntry::remote(hit.resource)?,
                        line: hit.line,
                        text: hit.text,
                    })
                })
                .collect::<Result<Vec<_>, BackendError>>()?,
            continuation: page.continuation,
            truncated: page.truncated,
            incomplete: page.incomplete,
        })
    }

    async fn watch_open(&self) -> Result<Option<WatchHandle>, BackendError> {
        let Some(service) = self.session.workspace().services().watch.as_ref() else {
            return Ok(None);
        };
        let subscription = service
            .open(
                self.session.binding(),
                self.session.cursor(),
                &WatchOpenRequest {
                    root: ResourceSelector::Current,
                    recursive: true,
                },
            )
            .await?;
        Ok(Some(WatchHandle {
            subscription_id: subscription.subscription_id,
            cursor: subscription.cursor,
            next_sequence: None,
        }))
    }

    async fn watch_poll(&self, handle: WatchHandle) -> Result<WatchResult, BackendError> {
        let service = self
            .session
            .workspace()
            .services()
            .watch
            .as_ref()
            .ok_or(BackendError::Workspace(WorkspaceError::Unavailable))?;
        let page = service
            .poll(
                self.session.binding(),
                self.session.cursor(),
                &WatchPollRequest {
                    subscription_id: handle.subscription_id.clone(),
                    cursor: handle.cursor.clone(),
                    max_events: WATCH_EVENTS,
                    max_bytes: WATCH_BYTES,
                    wait_ms: WATCH_WAIT_MS,
                },
            )
            .await?;
        validate_watch(&handle, &page)?;
        let WatchEventPage {
            subscription_id,
            state,
            sequence,
            events,
        } = page;
        match state {
            WatchPollState::Current { .. } if sequence.gap_before_first => Ok(WatchResult {
                handle,
                update: WatchUpdate::Resync,
            }),
            WatchPollState::Current { next_cursor, .. } => Ok(WatchResult {
                handle: WatchHandle {
                    subscription_id,
                    cursor: next_cursor,
                    next_sequence: Some(sequence.next_sequence),
                },
                update: WatchUpdate::Events(events),
            }),
            WatchPollState::FullResync { .. } => Ok(WatchResult {
                handle,
                update: WatchUpdate::Resync,
            }),
        }
    }

    async fn watch_close(&self, handle: WatchHandle) -> Result<(), BackendError> {
        let Some(service) = self.session.workspace().services().watch.as_ref() else {
            return Ok(());
        };
        service
            .close(
                self.session.binding(),
                self.session.cursor(),
                &handle.subscription_id,
            )
            .await?;
        Ok(())
    }
}

fn remote_revision(entry: &ResourceEntry) -> Result<ResourceRevision, BackendError> {
    match &entry.revision {
        Some(BackendRevision::Remote(revision)) => Ok(revision.clone()),
        _ => Err(BackendError::MissingRevision),
    }
}

fn event_request(event: &BackendEvent) -> Option<RequestId> {
    match event {
        BackendEvent::Listed { request, .. }
        | BackendEvent::Opened { request, .. }
        | BackendEvent::Saved { request, .. }
        | BackendEvent::Created { request, .. }
        | BackendEvent::Renamed { request, .. }
        | BackendEvent::Deleted { request, .. }
        | BackendEvent::Counted { request, .. }
        | BackendEvent::SearchPage { request, .. } => Some(*request),
        BackendEvent::WatchOpened(_) | BackendEvent::WatchPolled(_) => None,
    }
}

fn path_contains(parent: &WorkbenchPath, candidate: &WorkbenchPath) -> bool {
    match (parent, candidate) {
        (WorkbenchPath::Local(parent), WorkbenchPath::Local(candidate)) => {
            candidate.starts_with(parent)
        }
        (WorkbenchPath::Remote(parent), WorkbenchPath::Remote(candidate)) => {
            parent.is_root()
                || candidate == parent
                || candidate
                    .as_str()
                    .strip_prefix(parent.as_str())
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }
        _ => false,
    }
}

fn validate_watch(handle: &WatchHandle, page: &WatchEventPage) -> Result<(), BackendError> {
    if page.subscription_id != handle.subscription_id {
        return Err(BackendError::InvalidResponse);
    }
    if matches!(page.state, WatchPollState::FullResync { .. }) || page.sequence.gap_before_first {
        return Ok(());
    }
    let first = page.events.first().map(|event| event.sequence);
    if handle.next_sequence.is_some() && first.is_some() && first != handle.next_sequence {
        return Err(BackendError::InvalidResponse);
    }
    if !page
        .events
        .windows(2)
        .all(|events| events[1].sequence == events[0].sequence + 1)
    {
        return Err(BackendError::InvalidResponse);
    }
    let expected_next = page
        .events
        .last()
        .map_or(handle.next_sequence, |event| Some(event.sequence + 1));
    if expected_next.is_some() && expected_next != Some(page.sequence.next_sequence) {
        return Err(BackendError::InvalidResponse);
    }
    Ok(())
}

fn decode_text(text: &str) -> (Vec<String>, LineEnding, bool) {
    let line_ending = if text.contains("\r\n") {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    };
    let trailing_newline = text.ends_with('\n');
    let normalized = text.replace("\r\n", "\n");
    let mut lines = normalized
        .split('\n')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if trailing_newline {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    (lines, line_ending, trailing_newline)
}

fn sort_entries(entries: &mut [ResourceEntry]) {
    entries.sort_by(|left, right| {
        let left_dir = matches!(
            left.kind,
            ResourceKind::ProjectRoot | ResourceKind::Directory
        );
        let right_dir = matches!(
            right.kind,
            ResourceKind::ProjectRoot | ResourceKind::Directory
        );
        right_dir
            .cmp(&left_dir)
            .then_with(|| {
                left.path
                    .display()
                    .to_lowercase()
                    .cmp(&right.path.display().to_lowercase())
            })
            .then_with(|| left.path.display().cmp(&right.path.display()))
    });
}

fn local_error(error: std::io::Error) -> BackendError {
    BackendError::Local(error.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, ByteContent, ByteRange, CancellationResult,
        CollectionRevision, CwdHandle, ListPage, MutationEntryResult, MutationKind, MutationResult,
        OperationHandle, OperationId, OperationPhase, OperationStatus, PreparedScmMutation,
        ProjectIdentity, ProjectKey, ReadTextRequest, ReleaseResult, ResourceScope, ScmChangeKind,
        ScmCommit, ScmDiffLine, ScmDiffLineKind, ScmDiffPage, ScmDiffRequest, ScmDiscoverRequest,
        ScmDiscoverResult, ScmLogPage, ScmLogRequest, ScmMutation, ScmMutationPreview,
        ScmMutationResult, ScmReadSidePage, ScmReadSideRequest, ScmRepository,
        ScmRepositoryRevisions, ScmRevision, ScmStatusEntry, ScmStatusPage, ScmStatusRequest,
        SearchHit, SearchPage, SearchScanCounts, SequenceMetadata, SessionBindingId,
        SessionWorkspaceBinding, SourceTrustAnchor, TextContent, WatchCloseResult,
        WatchResyncReason, WatchSubscription, WorkspaceCapabilities, WorkspaceCapability,
        WorkspaceCursor, WorkspaceEventKind, WorkspaceHandle, WorkspaceMutationService,
        WorkspaceReadService, WorkspaceScmMutationService, WorkspaceScmReadService,
        WorkspaceSearchService, WorkspaceServices, WorkspaceWatchService,
    };
    use tempfile::TempDir;

    use super::*;

    const FILE: &str = "same-name.txt";
    const DIRECTORY: &str = "src";
    const NESTED: &str = "src/lib.rs";
    const RENAMED: &str = "src/main.rs";
    const ORIGINAL: &str = "one\ntwo\n";
    const CHANGED: &str = "changed\n";
    const WATCH_WAIT: &str = "an idle watch must await an event, not trigger a reconnect";

    #[derive(Clone)]
    struct FakeFile {
        id: ResourceId,
        revision: ResourceRevision,
        kind: ResourceKind,
        bytes: Vec<u8>,
    }

    struct FakeWorkspace {
        binding: SessionWorkspaceBinding,
        files: Mutex<HashMap<WorkspacePath, FakeFile>>,
        watch: (
            flume::Sender<WatchEventPage>,
            flume::Receiver<WatchEventPage>,
        ),
        scm_calls: AtomicUsize,
        search_probe: Mutex<Option<(flume::Sender<()>, flume::Sender<()>)>>,
    }

    struct SearchDropProbe(flume::Sender<()>);

    impl Drop for SearchDropProbe {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    pub(crate) struct RemoteControl(Arc<FakeWorkspace>);

    impl RemoteControl {
        pub(crate) fn replace(&self, path: &str, contents: &str) {
            let path = WorkspacePath::new(path).unwrap();
            let mut files = self.0.files.lock().unwrap();
            let revision = FakeWorkspace::revision(999);
            let file = files.get_mut(&path).unwrap();
            file.bytes = contents.as_bytes().to_vec();
            file.revision = revision;
        }

        pub(crate) fn insert(&self, path: &str, contents: &str) {
            let mut files = self.0.files.lock().unwrap();
            let index = files.len();
            let revision = FakeWorkspace::next_revision(&files);
            files.insert(
                WorkspacePath::new(path).unwrap(),
                FakeFile {
                    id: ResourceId::new(format!("inserted-{index}")).unwrap(),
                    revision,
                    kind: ResourceKind::File,
                    bytes: contents.as_bytes().to_vec(),
                },
            );
        }

        pub(crate) fn contents(&self, path: &str) -> String {
            let files = self.0.files.lock().unwrap();
            String::from_utf8(
                files
                    .get(&WorkspacePath::new(path).unwrap())
                    .unwrap()
                    .bytes
                    .clone(),
            )
            .unwrap()
        }

        pub(crate) fn watch_change(&self, path: &str) {
            self.0
                .watch
                .0
                .send(WatchEventPage {
                    subscription_id: WatchSubscriptionId::new("watch").unwrap(),
                    state: WatchPollState::Current {
                        next_cursor: WatchCursor::new("changed").unwrap(),
                        expires_at_unix_ms: 2,
                    },
                    sequence: sequence(2),
                    events: vec![WorkspaceEvent {
                        sequence: 1,
                        kind: WorkspaceEventKind::Changed,
                        path: WorkspacePath::new(path).unwrap(),
                        previous_path: None,
                    }],
                })
                .unwrap();
        }

        pub(crate) fn watch_resync(&self) {
            self.0
                .watch
                .0
                .send(WatchEventPage {
                    subscription_id: WatchSubscriptionId::new("watch").unwrap(),
                    state: WatchPollState::FullResync {
                        reason: WatchResyncReason::RetentionLost,
                    },
                    sequence: SequenceMetadata {
                        first_retained_sequence: Some(1),
                        next_sequence: 1,
                        gap_before_first: true,
                    },
                    events: Vec::new(),
                })
                .unwrap();
        }

        pub(crate) fn scm_calls(&self) -> usize {
            self.0.scm_calls.load(Ordering::Relaxed)
        }
    }

    impl FakeWorkspace {
        fn resource(&self, path: &WorkspacePath, file: &FakeFile) -> WorkspaceResource {
            WorkspaceResource {
                project: self.binding.project().clone(),
                scope: ResourceScope::new(vec![ResourceId::new("root").unwrap()], file.id.clone())
                    .unwrap(),
                path: Some(path.clone()),
                kind: file.kind,
                revision: Some(file.revision.clone()),
                size_bytes: Some(file.bytes.len() as u64),
            }
        }

        fn revision(index: usize) -> ResourceRevision {
            ResourceRevision::new(format!("revision-{index}")).unwrap()
        }

        fn next_revision(files: &HashMap<WorkspacePath, FakeFile>) -> ResourceRevision {
            Self::revision(files.len() + 10)
        }
    }

    #[async_trait]
    impl WorkspaceReadService for FakeWorkspace {
        async fn resolve(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            path: &WorkspacePath,
        ) -> Result<WorkspaceResource, WorkspaceError> {
            let files = self.files.lock().unwrap();
            let file = files.get(path).ok_or(WorkspaceError::Unavailable)?;
            Ok(self.resource(path, file))
        }

        async fn resolve_directory(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _path: &WorkspacePath,
        ) -> Result<caudra_workspace::ResolvedWorkspaceDirectory, WorkspaceError> {
            Err(WorkspaceError::Unavailable)
        }

        async fn stat(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            resource: &ResourceSelector,
        ) -> Result<WorkspaceResource, WorkspaceError> {
            let files = self.files.lock().unwrap();
            let (path, file) = files
                .iter()
                .find(|(path, file)| match resource {
                    ResourceSelector::Path(expected) => *path == expected,
                    ResourceSelector::Id(expected) => file.id == *expected,
                    ResourceSelector::Current => false,
                })
                .ok_or(WorkspaceError::Unavailable)?;
            Ok(self.resource(path, file))
        }

        async fn list(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            request: &ListRequest,
        ) -> Result<ListPage, WorkspaceError> {
            let parent = match &request.parent {
                ResourceSelector::Path(path) => path,
                _ => return Err(WorkspaceError::Unavailable),
            };
            let mut resources = self
                .files
                .lock()
                .unwrap()
                .iter()
                .filter(|(path, _)| {
                    if parent.is_root() {
                        request.recursive || !path.as_str().contains('/')
                    } else {
                        let suffix = path
                            .as_str()
                            .strip_prefix(parent.as_str())
                            .and_then(|suffix| suffix.strip_prefix('/'));
                        suffix.is_some_and(|suffix| request.recursive || !suffix.contains('/'))
                    }
                })
                .map(|(path, file)| self.resource(path, file))
                .collect::<Vec<_>>();
            resources.sort_by_key(|resource| resource.path.clone());
            let offset = request
                .continuation
                .as_ref()
                .and_then(|token| token.as_str().parse::<usize>().ok())
                .unwrap_or(0);
            let end = (offset + 1).min(resources.len());
            let next =
                (end < resources.len()).then(|| ContinuationToken::new(end.to_string()).unwrap());
            Ok(ListPage {
                revision: CollectionRevision::new("list-revision").unwrap(),
                resources: resources[offset..end].to_vec(),
                truncated: next.is_some(),
                incomplete: next.is_some(),
                continuation: next,
            })
        }

        async fn read_text(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _request: &ReadTextRequest,
        ) -> Result<TextContent, WorkspaceError> {
            panic!("remote editor must use bounded byte reads")
        }

        async fn read_bytes(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            request: &ReadBytesRequest,
        ) -> Result<ByteContent, WorkspaceError> {
            let files = self.files.lock().unwrap();
            let file = files
                .values()
                .find(
                    |file| matches!(&request.resource, ResourceSelector::Id(id) if *id == file.id),
                )
                .ok_or(WorkspaceError::Unavailable)?;
            if request
                .if_revision
                .as_ref()
                .is_some_and(|revision| *revision != file.revision)
            {
                return Err(WorkspaceError::StaleResource {
                    resource_id: file.id.clone(),
                });
            }
            Ok(ByteContent {
                bytes: file.bytes.clone(),
                resource_id: file.id.clone(),
                revision: file.revision.clone(),
                range: ByteRange {
                    start: 0,
                    end_exclusive: file.bytes.len() as u64,
                },
                total_bytes: Some(file.bytes.len() as u64),
                truncated: false,
                next_byte_offset: None,
            })
        }
    }

    #[async_trait]
    impl WorkspaceMutationService for FakeWorkspace {
        async fn execute(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            request: &MutationRequest,
        ) -> Result<OperationStatus<MutationResult>, WorkspaceError> {
            let mut files = self.files.lock().unwrap();
            let mutation = request.mutations.first().ok_or(WorkspaceError::Conflict)?;
            let (kind, path, destination, revision) = match mutation {
                Mutation::Write {
                    path,
                    content,
                    condition,
                } => {
                    let bytes = match content {
                        WriteContent::Text(text) => text.as_bytes().to_vec(),
                        WriteContent::Bytes(bytes) => bytes.clone(),
                    };
                    match condition {
                        MutationCondition::MustNotExist if files.contains_key(path) => {
                            return Err(WorkspaceError::Conflict);
                        }
                        MutationCondition::Matches(expected)
                            if files
                                .get(path)
                                .is_none_or(|file| file.revision != *expected) =>
                        {
                            return Err(WorkspaceError::Conflict);
                        }
                        _ => {}
                    }
                    let revision = Self::next_revision(&files);
                    let id = files
                        .get(path)
                        .map(|file| file.id.clone())
                        .unwrap_or_else(|| {
                            ResourceId::new(format!("resource-{}", files.len())).unwrap()
                        });
                    files.insert(
                        path.clone(),
                        FakeFile {
                            id,
                            revision: revision.clone(),
                            kind: ResourceKind::File,
                            bytes,
                        },
                    );
                    (MutationKind::Write, path.clone(), None, Some(revision))
                }
                Mutation::CreateDirectory { path } => {
                    if files.contains_key(path) {
                        return Err(WorkspaceError::Conflict);
                    }
                    let revision = Self::next_revision(&files);
                    let resource_index = files.len();
                    files.insert(
                        path.clone(),
                        FakeFile {
                            id: ResourceId::new(format!("resource-{resource_index}")).unwrap(),
                            revision: revision.clone(),
                            kind: ResourceKind::Directory,
                            bytes: Vec::new(),
                        },
                    );
                    (
                        MutationKind::CreateDirectory,
                        path.clone(),
                        None,
                        Some(revision),
                    )
                }
                Mutation::Move {
                    source,
                    destination,
                    expected_revision,
                } => {
                    if files.contains_key(destination) {
                        return Err(WorkspaceError::Conflict);
                    }
                    let mut file = files.remove(source).ok_or(WorkspaceError::Conflict)?;
                    if file.revision != *expected_revision {
                        files.insert(source.clone(), file);
                        return Err(WorkspaceError::Conflict);
                    }
                    file.revision = Self::next_revision(&files);
                    let revision = file.revision.clone();
                    files.insert(destination.clone(), file);
                    (
                        MutationKind::Move,
                        source.clone(),
                        Some(destination.clone()),
                        Some(revision),
                    )
                }
                Mutation::Remove {
                    path,
                    expected_revision,
                } => {
                    let file = files.get(path).ok_or(WorkspaceError::Conflict)?;
                    if file.revision != *expected_revision {
                        return Err(WorkspaceError::Conflict);
                    }
                    files.remove(path);
                    (MutationKind::Remove, path.clone(), None, None)
                }
            };
            Ok(completed(MutationResult {
                committed: true,
                rolled_back: false,
                atomic_across_files: true,
                results: vec![MutationEntryResult {
                    kind,
                    path,
                    destination,
                    revision,
                }],
            }))
        }

        async fn status(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            operation: &OperationHandle,
        ) -> Result<OperationStatus<MutationResult>, WorkspaceError> {
            Ok(OperationStatus {
                handle: operation.clone(),
                state: OperationState::NeverSeen,
                progress: Vec::new(),
                progress_metadata: sequence(0),
            })
        }

        async fn cancel(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _operation: &OperationHandle,
        ) -> Result<CancellationResult, WorkspaceError> {
            Ok(CancellationResult {
                state: OperationPhase::Completed,
                cancellation_requested: false,
            })
        }
    }

    #[async_trait]
    impl WorkspaceSearchService for FakeWorkspace {
        async fn search(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            request: &SearchRequest,
        ) -> Result<SearchPage, WorkspaceError> {
            let probe = self.search_probe.lock().unwrap().clone();
            if let Some((started, dropped)) = &probe
                && request.continuation.is_some()
            {
                let _guard = SearchDropProbe(dropped.clone());
                started.send(()).unwrap();
                return std::future::pending().await;
            }
            let files = self.files.lock().unwrap();
            let hits = files
                .iter()
                .filter(|(_, file)| file.kind == ResourceKind::File)
                .flat_map(|(path, file)| {
                    String::from_utf8_lossy(&file.bytes)
                        .lines()
                        .enumerate()
                        .filter(|(_, line)| line.contains(&request.query))
                        .map(|(line, text)| SearchHit {
                            resource: self.resource(path, file),
                            line: u32::try_from(line + 1).unwrap(),
                            text: text.to_owned(),
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            Ok(SearchPage {
                revision: CollectionRevision::new("search-revision").unwrap(),
                scan_counts: SearchScanCounts {
                    files_scanned: files.len() as u32,
                    files_listed: files.len() as u32,
                },
                hits,
                truncated: false,
                incomplete: false,
                continuation: probe.map(|_| ContinuationToken::new("next-page").unwrap()),
            })
        }
    }

    #[async_trait]
    impl WorkspaceWatchService for FakeWorkspace {
        async fn open(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _request: &WatchOpenRequest,
        ) -> Result<WatchSubscription, WorkspaceError> {
            Ok(WatchSubscription {
                subscription_id: WatchSubscriptionId::new("watch").unwrap(),
                cursor: WatchCursor::new("cursor-0").unwrap(),
                expires_at_unix_ms: 1,
            })
        }

        async fn poll(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _request: &WatchPollRequest,
        ) -> Result<WatchEventPage, WorkspaceError> {
            self.watch
                .1
                .recv_async()
                .await
                .map_err(|_| WorkspaceError::Unavailable)
        }

        async fn close(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            subscription_id: &WatchSubscriptionId,
        ) -> Result<WatchCloseResult, WorkspaceError> {
            Ok(WatchCloseResult {
                subscription_id: subscription_id.clone(),
                closed: true,
            })
        }
    }

    #[async_trait]
    impl WorkspaceScmReadService for FakeWorkspace {
        async fn discover(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _request: &ScmDiscoverRequest,
        ) -> Result<ScmDiscoverResult, WorkspaceError> {
            self.scm_calls.fetch_add(1, Ordering::Relaxed);
            Ok(ScmDiscoverResult {
                repository: scm_repository(),
            })
        }

        async fn status(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _request: &ScmStatusRequest,
        ) -> Result<ScmStatusPage, WorkspaceError> {
            self.scm_calls.fetch_add(1, Ordering::Relaxed);
            Ok(ScmStatusPage {
                revisions: scm_revisions(),
                collection_revision: CollectionRevision::new("scm-status").unwrap(),
                entries: vec![ScmStatusEntry {
                    path: WorkspacePath::new(FILE).unwrap(),
                    staged: Some(ScmChangeKind::Modified),
                    unstaged: Some(ScmChangeKind::Modified),
                    untracked: false,
                    conflicted: false,
                }],
                truncated: false,
                incomplete: false,
                continuation: None,
            })
        }

        async fn log(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _request: &ScmLogRequest,
        ) -> Result<ScmLogPage, WorkspaceError> {
            self.scm_calls.fetch_add(1, Ordering::Relaxed);
            Ok(ScmLogPage {
                head_revision: scm_revision("scm-head"),
                collection_revision: CollectionRevision::new("scm-log").unwrap(),
                commits: vec![ScmCommit {
                    id: scm_revision("scm-head"),
                    parents: vec![scm_revision("scm-parent")],
                    author_name: "Author".into(),
                    author_email: "author@example.test".into(),
                    committed_unix_seconds: 1,
                    summary: "remote commit".into(),
                    body: None,
                }],
                truncated: false,
                incomplete: false,
                continuation: None,
            })
        }

        async fn diff(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            request: &ScmDiffRequest,
        ) -> Result<ScmDiffPage, WorkspaceError> {
            self.scm_calls.fetch_add(1, Ordering::Relaxed);
            Ok(ScmDiffPage {
                repository_revision: scm_revision("scm-repository"),
                collection_revision: CollectionRevision::new("scm-diff").unwrap(),
                lines: vec![ScmDiffLine {
                    path: request
                        .path
                        .clone()
                        .unwrap_or_else(|| WorkspacePath::new(FILE).unwrap()),
                    kind: ScmDiffLineKind::Addition,
                    change: Some(ScmChangeKind::Modified),
                    old_line: None,
                    new_line: Some(1),
                    text: "remote".into(),
                }],
                truncated: false,
                incomplete: false,
                continuation: None,
            })
        }

        async fn read_side(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            request: &ScmReadSideRequest,
        ) -> Result<ScmReadSidePage, WorkspaceError> {
            self.scm_calls.fetch_add(1, Ordering::Relaxed);
            Ok(ScmReadSidePage {
                repository_revision: scm_revision("scm-repository"),
                resource_id: ResourceId::new("scm-side").unwrap(),
                revision: ResourceRevision::new("scm-side-revision").unwrap(),
                path: request.path.clone(),
                side: request.side.clone(),
                content: "remote\n".into(),
                start_line: request.start_line,
                end_line: request.start_line,
                total_lines: 1,
                truncated: false,
                incomplete: false,
                next_start_line: None,
            })
        }
    }

    #[async_trait]
    impl WorkspaceScmMutationService for FakeWorkspace {
        async fn prepare(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _repository_handle: &ResourceId,
            mutation: &ScmMutation,
        ) -> Result<PreparedScmMutation, WorkspaceError> {
            self.scm_calls.fetch_add(1, Ordering::Relaxed);
            let paths = match mutation {
                ScmMutation::Stage { paths }
                | ScmMutation::Unstage { paths }
                | ScmMutation::Discard { paths } => paths,
            };
            Ok(PreparedScmMutation {
                operation: operation_handle(),
                preview: ScmMutationPreview {
                    mutation: mutation.clone(),
                    repository_identity: scm_revision("scm-identity"),
                    revisions: scm_revisions(),
                    entries: paths
                        .iter()
                        .cloned()
                        .map(|path| ScmStatusEntry {
                            path,
                            staged: Some(ScmChangeKind::Modified),
                            unstaged: Some(ScmChangeKind::Modified),
                            untracked: false,
                            conflicted: false,
                        })
                        .collect(),
                },
            })
        }

        async fn execute(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            prepared: &PreparedScmMutation,
        ) -> Result<OperationStatus<ScmMutationResult>, WorkspaceError> {
            self.scm_calls.fetch_add(1, Ordering::Relaxed);
            Ok(OperationStatus {
                handle: operation_handle(),
                state: OperationState::Completed {
                    result: ScmMutationResult {
                        mutation: prepared.preview.mutation.clone(),
                        revisions: scm_revisions(),
                    },
                    side_effects_possible: true,
                },
                progress: Vec::new(),
                progress_metadata: sequence(0),
            })
        }

        async fn status(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _operation: &OperationHandle,
        ) -> Result<OperationStatus<ScmMutationResult>, WorkspaceError> {
            Err(WorkspaceError::IndeterminateOutcome)
        }

        async fn cancel(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _operation: &OperationHandle,
        ) -> Result<CancellationResult, WorkspaceError> {
            Ok(CancellationResult {
                state: OperationPhase::Cancelled,
                cancellation_requested: true,
            })
        }

        async fn release(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _prepared: &PreparedScmMutation,
        ) -> Result<ReleaseResult, WorkspaceError> {
            self.scm_calls.fetch_add(1, Ordering::Relaxed);
            Ok(ReleaseResult {
                state: OperationPhase::Completed,
                released: true,
            })
        }
    }

    fn scm_revision(value: &str) -> ScmRevision {
        ScmRevision::new(value).unwrap()
    }

    fn scm_revisions() -> ScmRepositoryRevisions {
        ScmRepositoryRevisions {
            repository: scm_revision("scm-repository"),
            head: scm_revision("scm-head"),
            index: scm_revision("scm-index"),
            worktree: scm_revision("scm-worktree"),
        }
    }

    fn scm_repository() -> ScmRepository {
        ScmRepository {
            handle: ResourceId::new("scm-handle").unwrap(),
            resource_id: ResourceId::new("scm-resource").unwrap(),
            root: WorkspacePath::root(),
            identity: scm_revision("scm-identity"),
            revisions: scm_revisions(),
        }
    }

    fn operation_handle() -> OperationHandle {
        OperationHandle {
            preparation_id: OperationId::new("scm-prepare").unwrap(),
            invocation_id: Some(OperationId::new("scm-invoke").unwrap()),
            execution_id: Some(OperationId::new("scm-execute").unwrap()),
            expires_at_unix_ms: None,
        }
    }

    fn sequence(next: u64) -> SequenceMetadata {
        SequenceMetadata {
            first_retained_sequence: None,
            next_sequence: next,
            gap_before_first: false,
        }
    }

    fn completed(result: MutationResult) -> OperationStatus<MutationResult> {
        OperationStatus {
            handle: OperationHandle {
                preparation_id: OperationId::new("prepare").unwrap(),
                invocation_id: Some(OperationId::new("invoke").unwrap()),
                execution_id: Some(OperationId::new("execute").unwrap()),
                expires_at_unix_ms: None,
            },
            state: OperationState::Completed {
                result,
                side_effects_possible: true,
            },
            progress: Vec::new(),
            progress_metadata: sequence(0),
        }
    }

    fn session_fixture() -> (WorkspaceSession, Arc<FakeWorkspace>) {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("test-anchor").unwrap(),
            "test-authority",
            "test-workspace",
            "test-generation",
            "test-namespace",
        )
        .unwrap();
        let principal = AuthenticatedPrincipalId::new(authority.clone(), "principal").unwrap();
        let project = ProjectIdentity::new(authority.clone(), ProjectKey::new("project").unwrap());
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new("session").unwrap(),
            authority.clone(),
            principal,
            project,
        )
        .unwrap();
        let files = HashMap::from([
            (
                WorkspacePath::new(FILE).unwrap(),
                FakeFile {
                    id: ResourceId::new("file").unwrap(),
                    revision: FakeWorkspace::revision(1),
                    kind: ResourceKind::File,
                    bytes: ORIGINAL.as_bytes().to_vec(),
                },
            ),
            (
                WorkspacePath::new(DIRECTORY).unwrap(),
                FakeFile {
                    id: ResourceId::new("directory").unwrap(),
                    revision: FakeWorkspace::revision(2),
                    kind: ResourceKind::Directory,
                    bytes: Vec::new(),
                },
            ),
        ]);
        let fake = Arc::new(FakeWorkspace {
            binding: binding.clone(),
            files: Mutex::new(files),
            watch: flume::unbounded(),
            scm_calls: AtomicUsize::new(0),
            search_probe: Mutex::new(None),
        });
        let capabilities = WorkspaceCapabilities::from([
            WorkspaceCapability::Resolve,
            WorkspaceCapability::List,
            WorkspaceCapability::ReadBytes,
            WorkspaceCapability::MutationExecute,
            WorkspaceCapability::MutationStatus,
            WorkspaceCapability::MutationCancel,
            WorkspaceCapability::Search,
            WorkspaceCapability::WatchOpen,
            WorkspaceCapability::WatchPoll,
            WorkspaceCapability::WatchClose,
            WorkspaceCapability::WatchRecursive,
            WorkspaceCapability::ScmDiscover,
            WorkspaceCapability::ScmStatus,
            WorkspaceCapability::ScmLog,
            WorkspaceCapability::ScmDiff,
            WorkspaceCapability::ScmReadSide,
            WorkspaceCapability::ScmStage,
            WorkspaceCapability::ScmUnstage,
            WorkspaceCapability::ScmDiscard,
            WorkspaceCapability::ScmMutationStatus,
            WorkspaceCapability::ScmMutationCancel,
            WorkspaceCapability::ScmMutationRelease,
        ]);
        let workspace = WorkspaceHandle::new(
            authority,
            capabilities,
            WorkspaceServices {
                read: Some(fake.clone()),
                mutation: Some(fake.clone()),
                search: Some(fake.clone()),
                watch: Some(fake.clone()),
                scm_read: Some(fake.clone()),
                scm_mutation: Some(fake.clone()),
                ..WorkspaceServices::default()
            },
        )
        .unwrap();
        let cursor = WorkspaceCursor::new(
            &binding,
            ResourceScope::root(ResourceId::new("root").unwrap()),
            1,
            CwdHandle::new("cwd").unwrap(),
        );
        let session = WorkspaceSession::new(workspace, binding, cursor).unwrap();
        (session, fake)
    }

    fn fixture() -> (WorkspaceFilesystem, Arc<FakeWorkspace>) {
        let (session, fake) = session_fixture();
        (WorkspaceFilesystem::new(session).unwrap(), fake)
    }

    pub(crate) fn widget_fixture() -> (WorkspaceSession, RemoteControl) {
        let (session, fake) = session_fixture();
        (session, RemoteControl(fake))
    }

    #[test]
    fn remote_tree_editor_mutations_search_and_local_canary_are_isolated() {
        smol::block_on(async {
            let canary = TempDir::new().unwrap();
            let canary_path = canary.path().join(FILE);
            fs::write(&canary_path, "local canary").unwrap();
            let (backend, fake) = fixture();
            let root = WorkbenchPath::Remote(WorkspacePath::root());
            let first = backend.list(&root, false, None).await.unwrap();
            assert_eq!(first.entries.len(), 1);
            let second = backend
                .list(&root, false, first.continuation)
                .await
                .unwrap();
            assert_eq!(second.entries.len(), 1);

            let file = backend
                .resolve_entry(&WorkspacePath::new(FILE).unwrap())
                .await
                .unwrap();
            let loaded = backend.read(&file).await.unwrap();
            assert_eq!(loaded.lines, ["one", "two"]);
            let saved = backend.write(&loaded.entry, CHANGED.into()).await.unwrap();
            assert!(matches!(saved.revision, Some(BackendRevision::Remote(_))));

            let stale = loaded.entry;
            let unsaved = "my unsaved buffer".to_owned();
            assert!(matches!(
                backend.write(&stale, unsaved.clone()).await,
                Err(BackendError::Conflict)
            ));
            assert_eq!(unsaved, "my unsaved buffer");

            let nested = WorkbenchPath::Remote(WorkspacePath::new(NESTED).unwrap());
            let created = backend.create_file(&nested).await.unwrap();
            let renamed_path = WorkbenchPath::Remote(WorkspacePath::new(RENAMED).unwrap());
            let renamed = backend.rename(&created, &renamed_path).await.unwrap();
            assert_eq!(backend.count(&root).await.unwrap(), 3);
            let search = backend.search("changed".into(), None, None).await.unwrap();
            assert_eq!(search.hits.len(), 1);
            backend.delete(&renamed).await.unwrap();
            assert_eq!(backend.count(&root).await.unwrap(), 2);
            assert_eq!(fs::read_to_string(canary_path).unwrap(), "local canary");
            assert!(
                !fake
                    .files
                    .lock()
                    .unwrap()
                    .contains_key(&WorkspacePath::new(RENAMED).unwrap())
            );
        });
    }

    #[test]
    fn remote_watch_validates_order_and_requests_resync_without_sleeping() {
        smol::block_on(async {
            let (backend, fake) = fixture();
            let handle = backend.watch_open().await.unwrap().unwrap();
            let mut pending = Box::pin(backend.watch_poll(handle.clone()));
            assert!(
                smol::future::poll_once(pending.as_mut()).await.is_none(),
                "{WATCH_WAIT}"
            );
            fake.watch
                .0
                .send(WatchEventPage {
                    subscription_id: handle.subscription_id.clone(),
                    state: WatchPollState::Current {
                        next_cursor: WatchCursor::new("cursor-1").unwrap(),
                        expires_at_unix_ms: 2,
                    },
                    sequence: sequence(2),
                    events: vec![WorkspaceEvent {
                        sequence: 1,
                        kind: WorkspaceEventKind::Changed,
                        path: WorkspacePath::new(FILE).unwrap(),
                        previous_path: None,
                    }],
                })
                .unwrap();
            let current = pending.await.unwrap();
            assert!(matches!(current.update, WatchUpdate::Events(_)));

            fake.watch
                .0
                .send(WatchEventPage {
                    subscription_id: current.handle.subscription_id.clone(),
                    state: WatchPollState::Current {
                        next_cursor: WatchCursor::new("cursor-2").unwrap(),
                        expires_at_unix_ms: 3,
                    },
                    sequence: sequence(4),
                    events: vec![WorkspaceEvent {
                        sequence: 3,
                        kind: WorkspaceEventKind::Changed,
                        path: WorkspacePath::new(FILE).unwrap(),
                        previous_path: None,
                    }],
                })
                .unwrap();
            assert!(matches!(
                backend.watch_poll(current.handle.clone()).await,
                Err(BackendError::InvalidResponse)
            ));

            fake.watch
                .0
                .send(WatchEventPage {
                    subscription_id: current.handle.subscription_id.clone(),
                    state: WatchPollState::FullResync {
                        reason: WatchResyncReason::RetentionLost,
                    },
                    sequence: SequenceMetadata {
                        first_retained_sequence: Some(9),
                        next_sequence: 9,
                        gap_before_first: true,
                    },
                    events: Vec::new(),
                })
                .unwrap();
            let resync = backend.watch_poll(current.handle).await.unwrap();
            assert_eq!(resync.update, WatchUpdate::Resync);
            backend.watch_close(resync.handle).await.unwrap();
        });
    }

    #[test_case::test_case(false; "cancel")]
    #[test_case::test_case(true; "suspend")]
    fn search_cancellation_drops_active_pagination_request(suspend: bool) {
        smol::block_on(async {
            let (backend, fake) = fixture();
            let (started_tx, started_rx) = flume::unbounded();
            let (dropped_tx, dropped_rx) = flume::unbounded();
            *fake.search_probe.lock().unwrap() = Some((started_tx, dropped_tx));
            let directory = TempDir::new().unwrap();
            let root = WorkbenchPath::Local(directory.path().to_path_buf());
            let mut driver = BackendDriver::new(
                WorkbenchBackend::local(directory.path().to_path_buf(), false),
                root,
            );
            driver.backend = Arc::new(backend);
            let request = driver.search(ORIGINAL.into(), None);
            started_rx.recv_async().await.unwrap();
            if suspend {
                driver.suspend();
            } else {
                driver.cancel(request);
            }
            dropped_rx.recv_async().await.unwrap();
            assert!(driver.drain().is_empty());
        });
    }

    #[test]
    fn driver_rebind_cancellation_and_instances_reject_cross_talk() {
        let first_dir = TempDir::new().unwrap();
        let second_dir = TempDir::new().unwrap();
        let first_root = WorkbenchPath::Local(first_dir.path().to_path_buf());
        let second_root = WorkbenchPath::Local(second_dir.path().to_path_buf());
        let first_backend = WorkbenchBackend::local(first_dir.path().to_path_buf(), false);
        let second_backend = WorkbenchBackend::local(second_dir.path().to_path_buf(), false);
        let mut first = BackendDriver::new(first_backend.clone(), first_root.clone());
        let mut second = BackendDriver::new(second_backend.clone(), second_root.clone());
        let stale_entry = ResourceEntry {
            path: WorkbenchPath::Local(first_dir.path().join(FILE)),
            resource_id: None,
            revision: Some(BackendRevision::Local(None)),
            kind: ResourceKind::File,
            size_bytes: Some(0),
        };
        first
            .sender
            .send(Envelope {
                generation: first.generation,
                event: BackendEvent::Listed {
                    request: 41,
                    parent: first_root,
                    result: Ok(ListResult {
                        entries: vec![stale_entry.clone()],
                        continuation: None,
                        incomplete: false,
                    }),
                },
            })
            .unwrap();
        first.rebind(second_backend, second_root);
        assert!(
            !first
                .drain()
                .iter()
                .any(|event| matches!(event, BackendEvent::Listed { request: 41, .. }))
        );
        assert!(first.resource(&stale_entry.path).is_none());

        second.cancel(42);
        second
            .sender
            .send(Envelope {
                generation: second.generation,
                event: BackendEvent::Opened {
                    request: 42,
                    result: Err(BackendError::Conflict),
                },
            })
            .unwrap();
        assert!(second.drain().is_empty());
        assert!(first.resource(&stale_entry.path).is_none());
    }
}
