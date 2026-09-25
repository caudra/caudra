//! Backend-neutral filesystem operations used by the workbench.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use caudra_workspace::{
    ContinuationToken, ListRequest, Mutation, MutationCondition, MutationRequest, OperationState,
    ReadBytesRequest, ResourceId, ResourceKind, ResourceRevision, ResourceSelector, SearchRequest,
    TransportErrorKind, WatchCursor, WatchEventPage, WatchOpenRequest, WatchPollRequest,
    WatchPollState, WatchSubscriptionId, WorkspaceError, WorkspaceEvent, WorkspacePath,
    WorkspaceResource, WorkspaceSession, WriteContent,
};
use ignore::WalkBuilder;
use smol::lock::Mutex;

use super::read::{self, LineEnding, ReadOnly};

const PAGE_SIZE: u32 = 256;
const MAX_LIST_PAGES: usize = 1024;
const MAX_RETAINED_ENTRIES: usize = 50_000;
const MAX_LIST_DEPTH: usize = 256;
const MAX_LIST_DELTA_ENTRIES: usize = PAGE_SIZE as usize * 2;
const MAX_PINNED_PATHS: usize = 1024;
const WATCH_EVENTS: u32 = 256;
const WATCH_BYTES: u32 = 256 * 1024;
const WATCH_WAIT_MS: u64 = 250;
const WATCH_RETRY_DELAY: Duration = Duration::from_secs(1);
const WATCH_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const MAX_WATCH_RETRIES: u32 = 5;
const MAX_MUTATION_STATUS_POLLS: usize = 8;
const MAX_EDITABLE_BYTES: u64 = 2 * 1024 * 1024;
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
const NOT_FOUND_CODE: &str = "not_found";
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
    #[error("remote file does not exist")]
    NotFound,
    #[error("filesystem operation is for the wrong workspace backend")]
    WrongBackend,
    #[error("resource is not a file")]
    NotFile,
    #[error("directories cannot be opened in the editor")]
    Directory,
    #[error("pipes, devices and other special files cannot be edited")]
    SpecialFile,
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
            WorkspaceError::Refused { symbolic, .. } if symbolic == NOT_FOUND_CODE => {
                Self::NotFound
            }
            WorkspaceError::Conflict | WorkspaceError::StaleResource { .. } => Self::Conflict,
            WorkspaceError::IndeterminateOutcome => Self::Indeterminate,
            error => Self::Workspace(error),
        }
    }
}

#[async_trait]
pub trait WorkbenchFilesystem: Send + Sync {
    fn is_remote(&self) -> bool;

    async fn list_root(&self, path: &WorkbenchPath) -> Result<WorkbenchPath, BackendError> {
        Ok(path.clone())
    }

    async fn list(
        &self,
        parent: &WorkbenchPath,
        recursive: bool,
        continuation: Option<ContinuationToken>,
    ) -> Result<ListResult, BackendError>;

    async fn read(&self, entry: &ResourceEntry) -> Result<LoadedFile, BackendError>;

    async fn read_path(&self, path: &WorkbenchPath) -> Result<LoadedFile, BackendError> {
        self.read(&ResourceEntry {
            path: path.clone(),
            resource_id: None,
            revision: None,
            kind: ResourceKind::File,
            size_bytes: None,
        })
        .await
    }

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
        complete: bool,
        authoritative: bool,
        removed: Vec<WorkbenchPath>,
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
    WatchOpened {
        request: RequestId,
        result: Result<Option<WatchHandle>, BackendError>,
    },
    WatchPolled {
        request: RequestId,
        result: Result<WatchResult, BackendError>,
    },
}

struct Envelope {
    generation: u64,
    event: BackendEvent,
}

struct PendingListPage {
    entries: VecDeque<ResourceEntry>,
    continuation: Option<ContinuationToken>,
    limited: bool,
}

enum Admission {
    Added,
    ChunkFull,
    Capacity,
}

struct Listing {
    request: RequestId,
    parent: WorkbenchPath,
    recursive: bool,
    indexing: bool,
    cursors: HashSet<ContinuationToken>,
    paths: HashSet<WorkbenchPath>,
    pages: usize,
    incomplete: bool,
    refresh: bool,
    pending_page: Option<PendingListPage>,
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
    open_tasks: HashMap<RequestId, smol::Task<()>>,
    listing: Option<Listing>,
    list_task: Option<smol::Task<()>>,
    cancelled: HashSet<RequestId>,
    resources: HashMap<WorkbenchPath, ResourceEntry>,
    resource_order: VecDeque<WorkbenchPath>,
    child_counts: HashMap<WorkbenchPath, usize>,
    pinned: HashSet<WorkbenchPath>,
    pin_overflow: bool,
    #[cfg(test)]
    retained_limit: usize,
    sender: flume::Sender<Envelope>,
    events: flume::Receiver<Envelope>,
    watch: Option<(RequestId, flume::Sender<()>)>,
    watch_enabled: bool,
    watch_retry: Option<Instant>,
    watch_attempts: u32,
    watch_warned: bool,
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
            open_tasks: HashMap::new(),
            listing: None,
            list_task: None,
            cancelled: HashSet::new(),
            resources: HashMap::new(),
            resource_order: VecDeque::new(),
            child_counts: HashMap::new(),
            pinned: HashSet::new(),
            pin_overflow: false,
            #[cfg(test)]
            retained_limit: MAX_RETAINED_ENTRIES,
            sender,
            events,
            watch: None,
            watch_enabled: false,
            watch_retry: None,
            watch_attempts: 0,
            watch_warned: false,
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

    pub fn is_listing(&self) -> bool {
        self.listing.is_some()
    }

    pub fn resource(&self, path: &WorkbenchPath) -> Option<&ResourceEntry> {
        self.resources.get(path)
    }

    pub fn clear_stale(&mut self) {
        self.stale = false;
    }

    pub fn set_pinned_paths(&mut self, mut paths: impl Iterator<Item = WorkbenchPath>) {
        self.pinned.clear();
        self.pinned.extend(paths.by_ref().take(MAX_PINNED_PATHS));
        self.pin_overflow = paths.next().is_some();
    }

    fn retained_limit(&self) -> usize {
        #[cfg(test)]
        {
            self.retained_limit
        }
        #[cfg(not(test))]
        {
            MAX_RETAINED_ENTRIES
        }
    }

    #[cfg(test)]
    pub(crate) fn set_retained_limit(&mut self, limit: usize) {
        self.retained_limit = limit.min(MAX_RETAINED_ENTRIES);
    }

    pub fn rebind(&mut self, backend: WorkbenchBackend, root: WorkbenchPath) {
        self.open_tasks.clear();
        self.cancel_listing();
        self.search_task = None;
        self.close_watch();
        self.generation = self.generation.wrapping_add(1);
        self.backend = backend.filesystem();
        self.root = root;
        self.cancelled.clear();
        self.resources.clear();
        self.resource_order.clear();
        self.child_counts.clear();
        self.pinned.clear();
        self.pin_overflow = false;
        self.active_search = 0;
        self.stale = true;
    }

    pub fn suspend(&mut self) {
        self.open_tasks.clear();
        self.cancel_listing();
        self.search_task = None;
        self.close_watch();
        self.generation = self.generation.wrapping_add(1);
        self.cancelled.clear();
        self.active_search = 0;
        self.stale = true;
    }

    pub fn list(&mut self, parent: WorkbenchPath, recursive: bool) -> RequestId {
        if let Some(listing) = &mut self.listing
            && listing.parent == parent
            && listing.recursive == recursive
        {
            listing.refresh = true;
            return listing.request;
        }
        self.cancel_listing();
        let request = self.request_id();
        let listing = Listing {
            request,
            parent,
            recursive,
            indexing: recursive && !self.is_remote(),
            cursors: HashSet::new(),
            paths: HashSet::new(),
            pages: 0,
            incomplete: false,
            refresh: false,
            pending_page: None,
        };
        self.list_page(&listing, None);
        self.listing = Some(listing);
        request
    }

    pub fn cancel_listing(&mut self) -> Option<RequestId> {
        self.list_task = None;
        self.listing.take().map(|listing| listing.request)
    }

    fn list_page(&mut self, listing: &Listing, continuation: Option<ContinuationToken>) {
        let request = listing.request;
        let parent = listing.parent.clone();
        let recursive = listing.indexing;
        let backend = Arc::clone(&self.backend);
        let sender = self.sender.clone();
        let generation = self.generation;
        self.list_task = Some(smol::spawn(async move {
            let (parent, result) = match backend.list_root(&parent).await {
                Ok(parent) => {
                    let result = backend.list(&parent, recursive, continuation).await;
                    (parent, result)
                }
                Err(error) => (parent, Err(error)),
            };
            let _ = sender.send(Envelope {
                generation,
                event: BackendEvent::Listed {
                    request,
                    parent,
                    complete: false,
                    authoritative: false,
                    removed: Vec::new(),
                    result,
                },
            });
        }));
    }

    fn install_listing(
        &mut self,
        result: &mut Result<ListResult, BackendError>,
        removed: &mut Vec<WorkbenchPath>,
        authoritative: &mut bool,
    ) -> bool {
        let Some(mut listing) = self.listing.take() else {
            return true;
        };
        self.list_task = None;
        let mut page = if let Some(page) = listing.pending_page.take() {
            page
        } else {
            let Ok(page) = result else {
                self.stale = true;
                if listing.refresh {
                    listing.refresh = false;
                    listing.cursors.clear();
                    listing.paths.clear();
                    listing.pages = 0;
                    listing.incomplete = false;
                    self.list_page(&listing, None);
                    self.listing = Some(listing);
                    return false;
                }
                return true;
            };
            listing.pages += 1;
            listing.incomplete |= page.incomplete;
            let limited = page.entries.len() > PAGE_SIZE as usize;
            page.entries.truncate(PAGE_SIZE as usize);
            PendingListPage {
                entries: std::mem::take(&mut page.entries).into(),
                continuation: page.continuation.take(),
                limited,
            }
        };
        let mut delta = HashMap::new();
        while let Some(entry) = page.entries.pop_front() {
            let Some(ancestors) = self.ancestors(&entry.path) else {
                page.limited = true;
                break;
            };
            let paths = ancestors.iter().chain(std::iter::once(&entry.path));
            let additional = paths
                .clone()
                .filter(|path| !listing.paths.contains(*path))
                .count();
            if listing.paths.len() + additional > self.retained_limit() {
                page.limited = true;
                break;
            }
            let before = removed.len();
            match self.admit_entry(&entry, &ancestors, &mut delta, removed) {
                Admission::Added => {}
                Admission::Capacity => {
                    page.limited = true;
                    break;
                }
                Admission::ChunkFull => {
                    page.entries.push_front(entry);
                    listing.pending_page = Some(page);
                    let _ = self.sender.send(Envelope {
                        generation: self.generation,
                        event: BackendEvent::Listed {
                            request: listing.request,
                            parent: listing.parent.clone(),
                            complete: false,
                            authoritative: false,
                            removed: Vec::new(),
                            result: Ok(ListResult {
                                entries: Vec::new(),
                                continuation: None,
                                incomplete: false,
                            }),
                        },
                    });
                    *result = Ok(ListResult {
                        entries: delta.into_values().collect(),
                        continuation: None,
                        incomplete: listing.incomplete,
                    });
                    self.listing = Some(listing);
                    return false;
                }
            }
            listing.paths.extend(ancestors.iter().cloned());
            listing.paths.insert(entry.path.clone());
            listing.incomplete |= removed[before..]
                .iter()
                .any(|path| listing.paths.contains(path));
        }
        let mut continuation = page.continuation.take();
        let more_pages = continuation.is_some() || (listing.recursive && !listing.indexing);
        let bad_cursor = continuation
            .as_ref()
            .is_some_and(|next| !listing.cursors.insert(next.clone()));
        let invalid = page.limited || (more_pages && listing.pages >= MAX_LIST_PAGES) || bad_cursor;
        let mut complete = continuation.is_none() || invalid;
        *authoritative =
            complete && !invalid && !listing.incomplete && (listing.indexing || !listing.recursive);
        if *authoritative {
            self.resources.retain(|path, _| {
                let covered = path != &listing.parent && path.starts_with(&listing.parent);
                let mut direct_child = path.clone();
                if covered && !listing.recursive {
                    while let Some(parent) = direct_child.parent() {
                        if parent == listing.parent {
                            break;
                        }
                        direct_child = parent;
                    }
                }
                let keep = !covered || listing.paths.contains(&direct_child);
                if !keep {
                    removed.push(path.clone());
                }
                keep
            });
            self.resource_order
                .retain(|path| self.resources.contains_key(path));
            self.rebuild_child_counts();
            self.stale = false;
        }
        let incomplete = invalid || listing.incomplete;
        if complete && !invalid && listing.recursive && !listing.indexing {
            listing.indexing = true;
            listing.cursors.clear();
            listing.paths.clear();
            listing.incomplete = false;
            complete = false;
        } else if complete && !bad_cursor && listing.refresh {
            listing.refresh = false;
            listing.indexing = listing.recursive;
            listing.cursors.clear();
            listing.paths.clear();
            listing.pages = 0;
            listing.incomplete = false;
            continuation = None;
            complete = false;
        }
        *result = Ok(ListResult {
            entries: delta.into_values().collect(),
            continuation: None,
            incomplete,
        });
        if complete && incomplete {
            self.stale = true;
        }
        if !complete {
            self.list_page(&listing, continuation);
            self.listing = Some(listing);
        }
        complete
    }

    fn cache_entry(&mut self, entry: &ResourceEntry) {
        if !self.resources.contains_key(&entry.path) {
            if let Some(parent) = entry.path.parent() {
                *self.child_counts.entry(parent).or_default() += 1;
            }
            self.resource_order.push_back(entry.path.clone());
        }
        self.resources.insert(entry.path.clone(), entry.clone());
    }

    fn rebuild_child_counts(&mut self) {
        self.child_counts.clear();
        for path in self.resources.keys() {
            if let Some(parent) = path.parent() {
                *self.child_counts.entry(parent).or_default() += 1;
            }
        }
    }

    fn ancestors(&self, path: &WorkbenchPath) -> Option<Vec<WorkbenchPath>> {
        if path == &self.root || !path.starts_with(&self.root) {
            return None;
        }
        let mut ancestors = Vec::new();
        let mut parent = path.parent()?;
        while parent != self.root {
            if ancestors.len() == MAX_LIST_DEPTH {
                return None;
            }
            ancestors.push(parent.clone());
            parent = parent.parent()?;
        }
        ancestors.reverse();
        Some(ancestors)
    }

    fn admit_entry(
        &mut self,
        entry: &ResourceEntry,
        ancestors: &[WorkbenchPath],
        delta: &mut HashMap<WorkbenchPath, ResourceEntry>,
        removed: &mut Vec<WorkbenchPath>,
    ) -> Admission {
        let mut updates = ancestors
            .iter()
            .filter(|path| {
                self.resources
                    .get(*path)
                    .is_none_or(|entry| entry.kind != ResourceKind::Directory)
            })
            .map(|path| ResourceEntry {
                path: path.clone(),
                resource_id: None,
                revision: None,
                kind: ResourceKind::Directory,
                size_bytes: None,
            })
            .collect::<Vec<_>>();
        if self.resources.get(&entry.path) != Some(entry) {
            updates.push(entry.clone());
        }
        if delta.len() + updates.len() > MAX_LIST_DELTA_ENTRIES {
            return Admission::ChunkFull;
        }
        if entry.kind != ResourceKind::Directory && self.child_counts.contains_key(&entry.path) {
            self.resources.retain(|path, _| {
                let keep = path == &entry.path || !path.starts_with(&entry.path);
                if !keep {
                    removed.push(path.clone());
                    delta.remove(path);
                }
                keep
            });
            self.resource_order
                .retain(|path| self.resources.contains_key(path));
            self.rebuild_child_counts();
        }
        let required = updates
            .iter()
            .filter(|entry| !self.resources.contains_key(&entry.path))
            .count();
        if required > self.retained_limit() {
            return Admission::Capacity;
        }
        while self.resources.len() + required > self.retained_limit() {
            if self.pin_overflow {
                return Admission::Capacity;
            }
            let mut candidate = None;
            for _ in 0..self.resource_order.len() {
                let Some(path) = self.resource_order.pop_front() else {
                    break;
                };
                if !self.child_counts.contains_key(&path)
                    && !self.pinned.contains(&path)
                    && path != entry.path
                    && !ancestors.contains(&path)
                {
                    candidate = Some(path);
                    break;
                }
                self.resource_order.push_back(path);
            }
            let Some(path) = candidate else {
                return Admission::Capacity;
            };
            self.resources.remove(&path);
            delta.remove(&path);
            if let Some(parent) = path.parent()
                && let Some(count) = self.child_counts.get_mut(&parent)
            {
                *count -= 1;
                if *count == 0 {
                    self.child_counts.remove(&parent);
                }
            }
            removed.push(path);
        }
        for update in updates {
            self.cache_entry(&update);
            delta.insert(update.path.clone(), update);
        }
        Admission::Added
    }

    pub fn open(&mut self, entry: ResourceEntry) -> RequestId {
        let backend = Arc::clone(&self.backend);
        self.spawn_open(async move { backend.read(&entry).await })
    }

    pub fn open_path(&mut self, path: WorkbenchPath) -> RequestId {
        let backend = Arc::clone(&self.backend);
        self.spawn_open(async move { backend.read_path(&path).await })
    }

    fn spawn_open(
        &mut self,
        future: impl Future<Output = Result<LoadedFile, BackendError>> + Send + 'static,
    ) -> RequestId {
        let request = self.request_id();
        let sender = self.sender.clone();
        let generation = self.generation;
        self.open_tasks.insert(
            request,
            smol::spawn(async move {
                let result = future.await;
                let _ = sender.send(Envelope {
                    generation,
                    event: BackendEvent::Opened { request, result },
                });
            }),
        );
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
        if self.open_tasks.remove(&request).is_some() {
            return;
        }
        if self
            .listing
            .as_ref()
            .is_some_and(|listing| listing.request == request)
        {
            self.cancel_listing();
            return;
        }
        if self.active_search == request {
            self.search_task = None;
            self.active_search = 0;
            return;
        }
        self.cancelled.insert(request);
    }

    pub fn cancel_open(&mut self, request: RequestId) {
        self.open_tasks.remove(&request);
    }

    pub fn open_watch(&mut self) {
        if self.watch_enabled {
            return;
        }
        self.watch_enabled = true;
        self.start_watch();
    }

    fn start_watch(&mut self) {
        let request = self.request_id();
        let (acknowledge, acknowledged) = flume::bounded(1);
        self.watch = Some((request, acknowledge));
        let backend = Arc::clone(&self.backend);
        let sender = self.sender.clone();
        let generation = self.generation;
        smol::spawn(watch_session(
            backend,
            sender,
            generation,
            request,
            acknowledged,
        ))
        .detach();
    }

    pub fn close_watch(&mut self) {
        self.watch = None;
        self.watch_enabled = false;
        self.watch_retry = None;
        self.watch_attempts = 0;
        self.watch_warned = false;
    }

    pub fn drain(&mut self) -> Vec<BackendEvent> {
        self.drain_at(Instant::now())
    }

    fn drain_at(&mut self, now: Instant) -> Vec<BackendEvent> {
        let mut admitted = Vec::new();
        let mut refresh = false;
        for _ in 0..self.events.len() {
            let Ok(mut envelope) = self.events.try_recv() else {
                break;
            };
            if envelope.generation != self.generation {
                continue;
            }
            if let BackendEvent::Opened { request, .. } = &envelope.event
                && self.open_tasks.remove(request).is_none()
            {
                continue;
            }
            if let BackendEvent::WatchOpened { request, .. }
            | BackendEvent::WatchPolled { request, .. } = &envelope.event
                && self
                    .watch
                    .as_ref()
                    .is_none_or(|(active, _)| active != request)
            {
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
            if let BackendEvent::Listed {
                request,
                parent,
                result,
                complete,
                removed,
                authoritative,
                ..
            } = &mut envelope.event
            {
                if self
                    .listing
                    .as_ref()
                    .is_none_or(|listing| listing.request != *request)
                {
                    continue;
                }
                if let Some(listing) = &mut self.listing
                    && listing.parent == self.root
                    && listing.pages == 0
                {
                    self.root = parent.clone();
                    listing.parent = parent.clone();
                }
                *complete = self.install_listing(result, removed, authoritative);
            }
            match &envelope.event {
                BackendEvent::WatchOpened {
                    result: Ok(Some(_)),
                    ..
                } => {
                    refresh = true;
                    self.acknowledge_watch();
                }
                BackendEvent::WatchOpened {
                    result: Ok(None), ..
                } => self.watch = None,
                BackendEvent::WatchPolled {
                    result: Ok(result), ..
                } => {
                    if matches!(result.update, WatchUpdate::Resync) {
                        self.stale = true;
                        refresh = true;
                        self.retry_watch(now);
                    } else {
                        self.watch_attempts = 0;
                        if matches!(&result.update, WatchUpdate::Events(events) if !events.is_empty())
                        {
                            self.stale = true;
                            refresh = true;
                        }
                        self.acknowledge_watch();
                    }
                }
                BackendEvent::WatchOpened {
                    result: Err(error), ..
                }
                | BackendEvent::WatchPolled {
                    result: Err(error), ..
                } => {
                    self.watch = None;
                    if retryable_watch_error(error) {
                        self.retry_watch(now);
                    }
                    if self.watch_warned {
                        continue;
                    }
                    self.watch_warned = true;
                }
                _ => {}
            }
            admitted.push(envelope.event);
        }
        if refresh {
            self.list(self.root.clone(), true);
        }
        if self.watch_retry.is_some_and(|deadline| now >= deadline) {
            self.watch_retry = None;
            self.start_watch();
        }
        admitted
    }

    fn acknowledge_watch(&self) {
        if let Some((_, acknowledge)) = &self.watch {
            let _ = acknowledge.try_send(());
        }
    }

    fn retry_watch(&mut self, now: Instant) {
        self.watch = None;
        if self.watch_enabled && self.watch_attempts < MAX_WATCH_RETRIES {
            let delay = (WATCH_RETRY_DELAY * (1 << self.watch_attempts)).min(WATCH_RETRY_MAX_DELAY);
            self.watch_attempts += 1;
            self.watch_retry = Some(now + delay);
        }
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

fn retryable_watch_error(error: &BackendError) -> bool {
    matches!(
        error,
        BackendError::Workspace(
            WorkspaceError::WatchUnavailable
                | WorkspaceError::Unavailable
                | WorkspaceError::Busy
                | WorkspaceError::Transport {
                    kind: TransportErrorKind::Disconnected | TransportErrorKind::Timeout
                }
        )
    )
}

async fn watch_session(
    backend: Arc<dyn WorkbenchFilesystem>,
    sender: flume::Sender<Envelope>,
    generation: u64,
    request: RequestId,
    acknowledged: flume::Receiver<()>,
) {
    let result = backend.watch_open().await;
    let handle = result.as_ref().ok().and_then(Clone::clone);
    let sent = sender
        .send(Envelope {
            generation,
            event: BackendEvent::WatchOpened { request, result },
        })
        .is_ok();
    let Some(mut handle) = handle else {
        return;
    };
    if sent {
        while acknowledged.recv_async().await.is_ok() {
            let result = smol::future::race(
                async { Some(backend.watch_poll(handle.clone()).await) },
                async {
                    let _ = acknowledged.recv_async().await;
                    None
                },
            )
            .await;
            let Some(result) = result else {
                break;
            };
            let terminal = !matches!(
                &result,
                Ok(WatchResult {
                    update: WatchUpdate::Events(_),
                    ..
                })
            );
            if let Ok(result) = &result {
                handle = result.handle.clone();
            }
            if terminal {
                let _ = backend.watch_close(handle).await;
                let _ = sender.send(Envelope {
                    generation,
                    event: BackendEvent::WatchPolled { request, result },
                });
                return;
            }
            if sender
                .send(Envelope {
                    generation,
                    event: BackendEvent::WatchPolled { request, result },
                })
                .is_err()
            {
                break;
            }
        }
    }
    let _ = backend.watch_close(handle).await;
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
    cursor_path: Arc<Mutex<Option<WorkspacePath>>>,
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
        Ok(Self {
            session,
            gate,
            cursor_path: Arc::new(Mutex::new(None)),
        })
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

    async fn cursor_path(&self) -> Result<WorkspacePath, BackendError> {
        let mut cached = self.cursor_path.lock().await;
        if let Some(path) = cached.as_ref() {
            return Ok(path.clone());
        }
        let resource = self
            .read_service()?
            .stat(
                self.session.binding(),
                self.session.cursor(),
                &ResourceSelector::Current,
            )
            .await?;
        if resource.project != *self.session.binding().project()
            || resource.scope.resource_id() != self.session.cursor().scope().resource_id()
            || !matches!(
                resource.kind,
                ResourceKind::Directory | ResourceKind::ProjectRoot
            )
        {
            return Err(BackendError::InvalidResponse);
        }
        let path = resource.path.ok_or(BackendError::InvalidResponse)?;
        *cached = Some(path.clone());
        Ok(path)
    }

    async fn relative(&self, path: &WorkspacePath) -> Result<WorkspacePath, BackendError> {
        let root = self.cursor_path().await?;
        if path == &root {
            return Ok(WorkspacePath::root());
        }
        if root.is_root() {
            return Ok(path.clone());
        }
        let relative = path
            .as_str()
            .strip_prefix(root.as_str())
            .and_then(|path| path.strip_prefix('/'))
            .ok_or(BackendError::WrongBackend)?;
        WorkspacePath::new(relative).map_err(|_| BackendError::InvalidResponse)
    }

    async fn resolve_entry(&self, path: &WorkspacePath) -> Result<ResourceEntry, BackendError> {
        let relative = self.relative(path).await?;
        let resource = self
            .read_service()?
            .resolve(self.session.binding(), self.session.cursor(), &relative)
            .await?;
        if resource.path.as_ref() != Some(path) {
            return Err(BackendError::InvalidResponse);
        }
        ResourceEntry::remote(resource)
    }

    async fn mutation_revision(
        &self,
        entry: &ResourceEntry,
    ) -> Result<ResourceRevision, BackendError> {
        if entry.revision.is_some() {
            return remote_revision(entry);
        }
        let selector = match &entry.resource_id {
            Some(id) => ResourceSelector::Id(id.clone()),
            None => ResourceSelector::Path(self.relative(self.remote(&entry.path)?).await?),
        };
        let resource = self
            .read_service()?
            .stat(self.session.binding(), self.session.cursor(), &selector)
            .await?;
        let verified = ResourceEntry::remote(resource)?;
        if (entry.resource_id.is_some() && verified.resource_id != entry.resource_id)
            || verified.path != entry.path
            || verified.kind != entry.kind
        {
            return Err(BackendError::Conflict);
        }
        remote_revision(&verified)
    }

    async fn identify(&self, entry: &ResourceEntry) -> Result<ResourceEntry, BackendError> {
        if entry.resource_id.is_some() {
            return Ok(entry.clone());
        }
        let resolved = self.resolve_entry(self.remote(&entry.path)?).await?;
        if resolved.path != entry.path || resolved.kind != entry.kind {
            return Err(BackendError::Conflict);
        }
        Ok(ResourceEntry {
            resource_id: resolved.resource_id,
            ..entry.clone()
        })
    }

    async fn mutate(&self, mutation: Mutation) -> Result<Option<ResourceRevision>, BackendError> {
        let mutation = match mutation {
            Mutation::Write {
                path,
                content,
                condition,
            } => Mutation::Write {
                path: self.relative(&path).await?,
                content,
                condition,
            },
            Mutation::CreateDirectory { path } => Mutation::CreateDirectory {
                path: self.relative(&path).await?,
            },
            Mutation::Move {
                source,
                destination,
                expected_revision,
            } => Mutation::Move {
                source: self.relative(&source).await?,
                destination: self.relative(&destination).await?,
                expected_revision,
            },
            Mutation::Remove {
                path,
                expected_revision,
            } => Mutation::Remove {
                path: self.relative(&path).await?,
                expected_revision,
            },
        };
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

    async fn list_root(&self, path: &WorkbenchPath) -> Result<WorkbenchPath, BackendError> {
        if self.remote(path)?.is_root() {
            Ok(WorkbenchPath::Remote(self.cursor_path().await?))
        } else {
            Ok(path.clone())
        }
    }

    async fn read_path(&self, path: &WorkbenchPath) -> Result<LoadedFile, BackendError> {
        let entry = self.resolve_entry(self.remote(path)?).await?;
        self.read(&entry).await
    }

    async fn list(
        &self,
        parent: &WorkbenchPath,
        recursive: bool,
        continuation: Option<ContinuationToken>,
    ) -> Result<ListResult, BackendError> {
        let parent = self.list_root(parent).await?;
        let relative = self.relative(self.remote(&parent)?).await?;
        let page = self
            .read_service()?
            .list(
                self.session.binding(),
                self.session.cursor(),
                &ListRequest {
                    parent: ResourceSelector::Path(relative),
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
            incomplete: page.incomplete || (page.truncated && page.continuation.is_none()),
            continuation: page.continuation,
        })
    }

    /// Reads what the resource says now. A listing is a snapshot of a workspace
    /// another writer is still working in, so conditioning the read on the
    /// revision it recorded refuses the open of every file that moved since,
    /// and the tab that would carry the current revision never exists to
    /// recover with. Identity stays pinned to the resource id, and the revision
    /// the read reports is what a later save is conditional on.
    async fn read(&self, entry: &ResourceEntry) -> Result<LoadedFile, BackendError> {
        match entry.kind {
            ResourceKind::File => {}
            ResourceKind::Directory | ResourceKind::ProjectRoot => {
                return Err(BackendError::Directory);
            }
            ResourceKind::Other => return Err(BackendError::SpecialFile),
            ResourceKind::Symlink => return Err(BackendError::NotFile),
        }
        if entry
            .size_bytes
            .is_some_and(|size| size > MAX_EDITABLE_BYTES)
        {
            return Err(BackendError::TooLarge);
        }
        let entry = self.identify(entry).await?;
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
        if content
            .total_bytes
            .is_some_and(|total| total > MAX_EDITABLE_BYTES)
            || content.bytes.len() as u64 > MAX_EDITABLE_BYTES
        {
            return Err(BackendError::TooLarge);
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
        let entry = self.identify(entry).await?;
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
        let expected_revision = self.mutation_revision(entry).await?;
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
            expected_revision: self.mutation_revision(entry).await?,
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
        BackendEvent::WatchOpened { .. } | BackendEvent::WatchPolled { .. } => None,
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
    use crate::fs::tree::Tree;
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
    use test_case::test_case;

    use super::*;

    const FILE: &str = "same-name.txt";
    const DIRECTORY: &str = "src";
    const NESTED: &str = "src/lib.rs";
    const RENAMED: &str = "src/main.rs";
    const ORIGINAL: &str = "one\ntwo\n";
    const CHANGED: &str = "changed\n";
    const WATCH_WAIT: &str = "an idle watch must await an event, not trigger a reconnect";
    const LIST_RETAINED: &str = "partial or failed listings must retain valid rows";
    const LIST_BOUNDED: &str = "listing must coalesce refreshes and bound pagination";
    const WATCH_BOUNDED: &str = "watch retries must be bounded and stop on suspension";
    const MUTATION_CONDITIONAL: &str =
        "mutations must use a verified revision, never overwrite a conflict";
    const DELTA_BOUNDED: &str = "pages must carry only bounded changes, not accumulated snapshots";
    const ROTATING_GENERATIONS: usize = 3;
    const NOT_FOUND_ERROR_CODE: i64 = -32000;
    const LARGE_TREE_DIRECTORIES: usize = 5_000;
    const LEGACY_RETAINED_LIMIT: usize = 16_384;
    const TARGETED_RETAINED_LIMIT: usize = 2;
    const TREE_REACHABLE: &str = "every retained file must keep its complete directory ancestry";
    pub(crate) const SCOPED_ROOT: &str = "sub";
    pub(crate) const SCOPED_FILE: &str = "sub/a.rs";
    pub(crate) const SHADOW_FILE: &str = "sub/sub/a.rs";
    pub(crate) const SCOPED_CONTENTS: &str = "selected contents";
    pub(crate) const SHADOW_CONTENTS: &str = "wrong nested contents";
    const SCOPED_CREATED: &str = "sub/new.rs";
    const SCOPED_DIRECTORY: &str = "sub/new-directory";
    const SCOPED_RENAMED: &str = "sub/renamed.rs";
    const OUTSIDE_FILE: &str = "outside.rs";
    const CURSOR_EXACT: &str =
        "project-relative paths must address the same resource under a non-root cursor";
    const DEEP_ENTRY_PARTS: usize = 4;

    pub(crate) struct ListCall {
        pub(crate) request: ListRequest,
        pub(crate) reply: flume::Sender<Result<(), WorkspaceError>>,
    }

    pub(crate) struct ReadCall {
        pub(crate) path: WorkspacePath,
        pub(crate) reply: flume::Sender<()>,
    }

    type WatchReply = flume::Sender<Result<(), WorkspaceError>>;

    #[derive(Clone)]
    struct FakeFile {
        id: ResourceId,
        revision: ResourceRevision,
        kind: ResourceKind,
        bytes: Vec<u8>,
    }

    struct FakeWorkspace {
        binding: SessionWorkspaceBinding,
        cwd: WorkspacePath,
        files: Mutex<HashMap<WorkspacePath, FakeFile>>,
        watch: (
            flume::Sender<WatchEventPage>,
            flume::Receiver<WatchEventPage>,
        ),
        scm_calls: AtomicUsize,
        scm_error: Mutex<Option<WorkspaceError>>,
        stat_calls: AtomicUsize,
        list_page_size: AtomicUsize,
        list_probe: Mutex<Option<flume::Sender<ListCall>>>,
        read_probe: Mutex<Option<flume::Sender<ReadCall>>>,
        watch_probe: Mutex<Option<flume::Sender<WatchReply>>>,
        watch_poll_error: Mutex<Option<WorkspaceError>>,
        watch_closed: (flume::Sender<()>, flume::Receiver<()>),
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
        pub(crate) fn read_calls(&self) -> flume::Receiver<ReadCall> {
            let (sender, receiver) = flume::unbounded();
            *self.0.read_probe.lock().unwrap() = Some(sender);
            receiver
        }
        pub(crate) fn remove(&self, path: &str) {
            self.0
                .files
                .lock()
                .unwrap()
                .remove(&WorkspacePath::new(path).unwrap());
        }
        pub(crate) fn watch_calls(&self) -> flume::Receiver<WatchReply> {
            let (sender, receiver) = flume::unbounded();
            *self.0.watch_probe.lock().unwrap() = Some(sender);
            receiver
        }

        pub(crate) fn set_kind(&self, path: &str, kind: ResourceKind) {
            self.0
                .files
                .lock()
                .unwrap()
                .get_mut(&WorkspacePath::new(path).unwrap())
                .unwrap()
                .kind = kind;
        }

        pub(crate) fn list_calls(&self) -> flume::Receiver<ListCall> {
            let (sender, receiver) = flume::unbounded();
            *self.0.list_probe.lock().unwrap() = Some(sender);
            receiver
        }

        pub(crate) fn scm_error(&self, error: WorkspaceError) {
            *self.0.scm_error.lock().unwrap() = Some(error);
        }

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
        fn absolute(&self, path: &WorkspacePath) -> WorkspacePath {
            if path.is_root() {
                self.cwd.clone()
            } else if self.cwd.is_root() {
                path.clone()
            } else {
                WorkspacePath::new(format!("{}/{path}", self.cwd)).unwrap()
            }
        }

        fn mutation_path(&self, mutation: &Mutation) -> Mutation {
            match mutation {
                Mutation::Write {
                    path,
                    content,
                    condition,
                } => Mutation::Write {
                    path: self.absolute(path),
                    content: content.clone(),
                    condition: condition.clone(),
                },
                Mutation::CreateDirectory { path } => Mutation::CreateDirectory {
                    path: self.absolute(path),
                },
                Mutation::Move {
                    source,
                    destination,
                    expected_revision,
                } => Mutation::Move {
                    source: self.absolute(source),
                    destination: self.absolute(destination),
                    expected_revision: expected_revision.clone(),
                },
                Mutation::Remove {
                    path,
                    expected_revision,
                } => Mutation::Remove {
                    path: self.absolute(path),
                    expected_revision: expected_revision.clone(),
                },
            }
        }

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
            let path = self.absolute(path);
            let probe = self.read_probe.lock().unwrap().clone();
            if let Some(probe) = probe {
                let (reply, response) = flume::bounded(1);
                probe
                    .send(ReadCall {
                        path: path.clone(),
                        reply,
                    })
                    .map_err(|_| WorkspaceError::Cancelled)?;
                response
                    .recv_async()
                    .await
                    .map_err(|_| WorkspaceError::Cancelled)?;
            }
            let files = self.files.lock().unwrap();
            let file = files.get(&path).ok_or_else(|| WorkspaceError::Refused {
                code: NOT_FOUND_ERROR_CODE,
                symbolic: NOT_FOUND_CODE.to_owned(),
            })?;
            Ok(self.resource(&path, file))
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
            cursor: &WorkspaceCursor,
            resource: &ResourceSelector,
        ) -> Result<WorkspaceResource, WorkspaceError> {
            if matches!(resource, ResourceSelector::Current) {
                return Ok(WorkspaceResource {
                    project: self.binding.project().clone(),
                    scope: cursor.scope().clone(),
                    path: Some(self.cwd.clone()),
                    kind: ResourceKind::Directory,
                    revision: None,
                    size_bytes: None,
                });
            }
            self.stat_calls.fetch_add(1, Ordering::Relaxed);
            let files = self.files.lock().unwrap();
            let (path, file) = files
                .iter()
                .find(|(path, file)| match resource {
                    ResourceSelector::Path(expected) => **path == self.absolute(expected),
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
            let probe = self.list_probe.lock().unwrap().clone();
            if let Some(probe) = probe {
                let (reply, response) = flume::bounded(1);
                probe
                    .send(ListCall {
                        request: request.clone(),
                        reply,
                    })
                    .unwrap();
                response
                    .recv_async()
                    .await
                    .map_err(|_| WorkspaceError::Cancelled)??;
            }
            let parent = match &request.parent {
                ResourceSelector::Path(path) => self.absolute(path),
                ResourceSelector::Current => self.cwd.clone(),
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
                .map(|(path, file)| WorkspaceResource {
                    revision: None,
                    ..self.resource(path, file)
                })
                .collect::<Vec<_>>();
            resources.sort_by_key(|resource| resource.path.clone());
            let offset = request
                .continuation
                .as_ref()
                .and_then(|token| token.as_str().parse::<usize>().ok())
                .unwrap_or(0);
            let end = (offset
                + self
                    .list_page_size
                    .load(Ordering::Relaxed)
                    .min(request.limit as usize))
            .min(resources.len());
            let next =
                (end < resources.len()).then(|| ContinuationToken::new(end.to_string()).unwrap());
            Ok(ListPage {
                revision: CollectionRevision::new("list-revision").unwrap(),
                resources: resources
                    .get(offset..end)
                    .ok_or(WorkspaceError::Unavailable)?
                    .to_vec(),
                truncated: next.is_some(),
                incomplete: false,
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
            let mutation =
                self.mutation_path(request.mutations.first().ok_or(WorkspaceError::Conflict)?);
            let (kind, path, destination, revision) = match &mutation {
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
                    let file = files.get(source).ok_or(WorkspaceError::Conflict)?;
                    if file.revision != *expected_revision {
                        return Err(WorkspaceError::Conflict);
                    }
                    let revision = file.revision.clone();
                    let moved = files
                        .keys()
                        .filter(|path| {
                            WorkbenchPath::Remote((*path).clone())
                                .starts_with(&WorkbenchPath::Remote(source.clone()))
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    for path in moved {
                        let mut file = files.remove(&path).unwrap();
                        let suffix = path.as_str().strip_prefix(source.as_str()).unwrap();
                        let moved_path =
                            WorkspacePath::new(format!("{}{suffix}", destination.as_str()))
                                .unwrap();
                        file.id =
                            ResourceId::new(format!("moved-{}", moved_path.as_str())).unwrap();
                        files.insert(moved_path, file);
                    }
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
            let root = match &request.root {
                ResourceSelector::Current => self.cwd.clone(),
                ResourceSelector::Path(path) => self.absolute(path),
                ResourceSelector::Id(_) => return Err(WorkspaceError::Unavailable),
            };
            let hits = files
                .iter()
                .filter(|(path, file)| {
                    file.kind == ResourceKind::File
                        && WorkbenchPath::Remote((*path).clone())
                            .starts_with(&WorkbenchPath::Remote(root.clone()))
                })
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
            let probe = self.watch_probe.lock().unwrap().clone();
            if let Some(probe) = probe {
                let (reply, response) = flume::bounded(1);
                probe.send(reply).unwrap();
                response
                    .recv_async()
                    .await
                    .map_err(|_| WorkspaceError::Cancelled)??;
            }
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
            if let Some(error) = self.watch_poll_error.lock().unwrap().take() {
                return Err(error);
            }
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
            let _ = self.watch_closed.0.send(());
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
            if let Some(error) = self.scm_error.lock().unwrap().clone() {
                return Err(error);
            }
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
        session_fixture_at(WorkspacePath::root())
    }

    fn session_fixture_at(cwd: WorkspacePath) -> (WorkspaceSession, Arc<FakeWorkspace>) {
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
            cwd,
            files: Mutex::new(files),
            watch: flume::unbounded(),
            scm_calls: AtomicUsize::new(0),
            scm_error: Mutex::new(None),
            stat_calls: AtomicUsize::new(0),
            list_page_size: AtomicUsize::new(1),
            list_probe: Mutex::new(None),
            read_probe: Mutex::new(None),
            watch_probe: Mutex::new(None),
            watch_poll_error: Mutex::new(None),
            watch_closed: flume::unbounded(),
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
            if fake.cwd.is_root() {
                ResourceScope::root(ResourceId::new("root").unwrap())
            } else {
                ResourceScope::new(
                    vec![ResourceId::new("root").unwrap()],
                    ResourceId::new("cursor-directory").unwrap(),
                )
                .unwrap()
            },
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

    pub(crate) fn scoped_widget_fixture() -> (WorkspaceSession, RemoteControl) {
        let (session, fake) = session_fixture_at(WorkspacePath::new(SCOPED_ROOT).unwrap());
        let control = RemoteControl(fake);
        control.insert(SCOPED_FILE, SCOPED_CONTENTS);
        control.insert(SHADOW_FILE, SHADOW_CONTENTS);
        (session, control)
    }

    #[test]
    fn non_root_cursor_normalizes_all_service_paths_without_touching_shadow_resources() {
        smol::block_on(async {
            let (session, control) = scoped_widget_fixture();
            control.insert(OUTSIDE_FILE, SCOPED_CONTENTS);
            let backend = WorkspaceFilesystem::new(session).unwrap();
            let root = WorkbenchPath::Remote(WorkspacePath::root());
            assert_eq!(
                backend.list_root(&root).await.unwrap().display(),
                SCOPED_ROOT
            );
            let page = backend.list(&root, false, None).await.unwrap();
            let selected = &page.entries[0];
            assert_eq!(selected.path.display(), SCOPED_FILE, "{CURSOR_EXACT}");
            assert_eq!(
                backend.read(selected).await.unwrap().lines,
                [SCOPED_CONTENTS],
                "{CURSOR_EXACT}"
            );
            let loaded = backend.read_path(&selected.path).await.unwrap();
            assert_eq!(loaded.lines, [SCOPED_CONTENTS], "{CURSOR_EXACT}");
            let hits = backend
                .search(SCOPED_CONTENTS.to_owned(), None, None)
                .await
                .unwrap()
                .hits;
            assert_eq!(hits.len(), 1, "{CURSOR_EXACT}");
            assert_eq!(hits[0].entry.path.display(), SCOPED_FILE, "{CURSOR_EXACT}");
            let saved = backend
                .write(&loaded.entry, CHANGED.to_owned())
                .await
                .unwrap();
            assert_eq!(control.contents(SCOPED_FILE), CHANGED, "{CURSOR_EXACT}");
            let created = backend
                .create_file(&WorkbenchPath::Remote(
                    WorkspacePath::new(SCOPED_CREATED).unwrap(),
                ))
                .await
                .unwrap();
            assert_eq!(created.path.display(), SCOPED_CREATED, "{CURSOR_EXACT}");
            let directory = backend
                .create_dir(&WorkbenchPath::Remote(
                    WorkspacePath::new(SCOPED_DIRECTORY).unwrap(),
                ))
                .await
                .unwrap();
            assert_eq!(directory.path.display(), SCOPED_DIRECTORY, "{CURSOR_EXACT}");
            let renamed = backend
                .rename(
                    &saved,
                    &WorkbenchPath::Remote(WorkspacePath::new(SCOPED_RENAMED).unwrap()),
                )
                .await
                .unwrap();
            assert_eq!(renamed.path.display(), SCOPED_RENAMED, "{CURSOR_EXACT}");
            backend.delete(&renamed).await.unwrap();
            assert!(matches!(
                backend.read_path(&renamed.path).await,
                Err(BackendError::NotFound)
            ));
            assert_eq!(
                control.contents(SHADOW_FILE),
                SHADOW_CONTENTS,
                "{CURSOR_EXACT}"
            );
            assert_eq!(
                control.contents(OUTSIDE_FILE),
                SCOPED_CONTENTS,
                "{CURSOR_EXACT}"
            );
            assert!(matches!(
                backend
                    .create_file(&WorkbenchPath::Remote(
                        WorkspacePath::new(OUTSIDE_FILE).unwrap()
                    ))
                    .await,
                Err(BackendError::WrongBackend)
            ));
        });
    }

    async fn drain_next(driver: &mut BackendDriver, now: Instant) -> Vec<BackendEvent> {
        let envelope = driver.events.recv_async().await.unwrap();
        driver.sender.send(envelope).unwrap();
        driver.drain_at(now)
    }

    fn remote_driver(backend: WorkspaceFilesystem) -> BackendDriver {
        BackendDriver::new(
            WorkbenchBackend::custom(Arc::new(backend)),
            WorkbenchPath::Remote(WorkspacePath::root()),
        )
    }

    fn indexed_entry(index: usize) -> ResourceEntry {
        ResourceEntry {
            path: WorkbenchPath::Remote(WorkspacePath::new(format!("file-{index}")).unwrap()),
            resource_id: None,
            revision: None,
            kind: ResourceKind::File,
            size_bytes: None,
        }
    }

    fn deep_page_fixture() -> (BackendDriver, flume::Receiver<ListCall>) {
        let (backend, fake) = fixture();
        fake.list_page_size
            .store(PAGE_SIZE as usize, Ordering::Relaxed);
        let control = RemoteControl(fake);
        for index in 0..PAGE_SIZE {
            control.insert(&format!("a-{index}/b/c/file"), ORIGINAL);
        }
        let calls = control.list_calls();
        (remote_driver(backend), calls)
    }

    async fn install_first_deep_chunk(
        driver: &mut BackendDriver,
        calls: &flume::Receiver<ListCall>,
    ) -> Vec<BackendEvent> {
        driver.list(driver.root.clone(), true);
        let shallow = calls.recv_async().await.unwrap();
        assert!(!shallow.request.recursive);
        shallow.reply.send(Ok(())).unwrap();
        drain_next(driver, Instant::now()).await;
        let recursive = calls.recv_async().await.unwrap();
        assert!(recursive.request.recursive && recursive.request.continuation.is_none());
        recursive.reply.send(Ok(())).unwrap();
        drain_next(driver, Instant::now()).await
    }

    #[test]
    fn children_first_page_streams_every_entry_in_bounded_chunks_before_advancing_cursor() {
        smol::block_on(async {
            let (mut driver, calls) = deep_page_fixture();
            let first = install_first_deep_chunk(&mut driver, &calls).await;
            assert!(
                matches!(&first[..], [BackendEvent::Listed { complete: false, authoritative: false, result: Ok(page), .. }] if !page.incomplete && page.entries.len() == MAX_LIST_DELTA_ENTRIES),
                "{DELTA_BOUNDED}"
            );
            assert!(calls.is_empty(), "{DELTA_BOUNDED}");
            let listing = driver.listing.as_ref().unwrap();
            assert!(
                listing
                    .pending_page
                    .as_ref()
                    .is_some_and(|page| page.entries.len() <= PAGE_SIZE as usize)
            );
            let pages = listing.pages;
            let second = driver.drain();
            assert!(
                matches!(&second[..], [BackendEvent::Listed { complete: false, authoritative: false, result: Ok(page), .. }] if !page.incomplete && page.entries.len() == MAX_LIST_DELTA_ENTRIES),
                "{DELTA_BOUNDED}"
            );
            assert_eq!(
                driver.listing.as_ref().unwrap().pages,
                pages,
                "{DELTA_BOUNDED}"
            );
            assert!(driver.listing.as_ref().unwrap().pending_page.is_none());
            let next = calls.recv_async().await.unwrap();
            assert_eq!(
                next.request.continuation.as_ref().unwrap().as_str(),
                PAGE_SIZE.to_string()
            );
            next.reply.send(Ok(())).unwrap();
            let final_page = drain_next(&mut driver, Instant::now()).await;
            assert!(
                matches!(&final_page[..], [BackendEvent::Listed { complete: true, authoritative: true, result: Ok(page), .. }] if !page.incomplete && page.entries.len() <= MAX_LIST_DELTA_ENTRIES)
            );
            for index in 0..PAGE_SIZE {
                let path = WorkbenchPath::Remote(
                    WorkspacePath::new(format!("a-{index}/b/c/file")).unwrap(),
                );
                assert!(driver.resource(&path).is_some(), "{DELTA_BOUNDED}");
                for parent in driver.ancestors(&path).unwrap() {
                    assert!(driver.resource(&parent).is_some(), "{TREE_REACHABLE}");
                }
            }
            assert_eq!(
                driver.resources.len(),
                PAGE_SIZE as usize * DEEP_ENTRY_PARTS + [FILE, DIRECTORY].len()
            );
            assert!(driver.listing.is_none() && calls.is_empty());
        });
    }

    enum ListingStop {
        Cancel,
        Suspend,
        Rebind,
    }

    #[test_case(ListingStop::Cancel; "cancelled_chunk")]
    #[test_case(ListingStop::Suspend; "suspended_chunk")]
    #[test_case(ListingStop::Rebind; "rebound_chunk")]
    fn stale_pending_page_chunks_cannot_install_after_the_listing_stops(stop: ListingStop) {
        smol::block_on(async {
            let (mut driver, calls) = deep_page_fixture();
            install_first_deep_chunk(&mut driver, &calls).await;
            assert!(driver.listing.as_ref().unwrap().pending_page.is_some());
            match stop {
                ListingStop::Cancel => {
                    driver.cancel_listing();
                }
                ListingStop::Suspend => driver.suspend(),
                ListingStop::Rebind => {
                    let (backend, _) = fixture();
                    driver.rebind(
                        WorkbenchBackend::custom(Arc::new(backend)),
                        driver.root.clone(),
                    );
                    driver.cache_entry(&indexed_entry(0));
                }
            }
            let retained = driver.resources.clone();
            assert!(driver.drain().is_empty(), "{LIST_RETAINED}");
            assert_eq!(driver.resources, retained, "{LIST_RETAINED}");
            assert!(driver.listing.is_none() && calls.is_empty());
        });
    }

    fn seed_listing(driver: &mut BackendDriver, refresh: bool) {
        driver.cancel_listing();
        driver.listing = Some(Listing {
            request: driver.request_id(),
            parent: driver.root.clone(),
            recursive: true,
            indexing: true,
            cursors: HashSet::new(),
            paths: HashSet::new(),
            pages: 0,
            incomplete: false,
            refresh,
            pending_page: None,
        });
    }

    #[test]
    fn pages_emit_linear_deltas_and_enforce_a_generation_entry_bound() {
        let (backend, _) = fixture();
        let mut driver = remote_driver(backend);
        seed_listing(&mut driver, false);
        let mut emitted = 0;
        for offset in (0..MAX_RETAINED_ENTRIES + PAGE_SIZE as usize).step_by(PAGE_SIZE as usize) {
            let mut page = Ok(ListResult {
                entries: (offset..offset + PAGE_SIZE as usize)
                    .map(indexed_entry)
                    .collect(),
                continuation: Some(ContinuationToken::new(offset.to_string()).unwrap()),
                incomplete: false,
            });
            let mut removed = Vec::new();
            let mut authoritative = false;
            let complete = driver.install_listing(&mut page, &mut removed, &mut authoritative);
            let page = page.unwrap();
            assert!(page.entries.len() <= PAGE_SIZE as usize, "{DELTA_BOUNDED}");
            emitted += page.entries.len();
            assert!(!authoritative && removed.is_empty());
            assert_eq!(
                complete,
                offset + PAGE_SIZE as usize > MAX_RETAINED_ENTRIES,
                "{LIST_BOUNDED}"
            );
            assert_eq!(page.incomplete, complete, "{LIST_BOUNDED}");
            if complete {
                break;
            }
        }
        assert_eq!(emitted, MAX_RETAINED_ENTRIES, "{DELTA_BOUNDED}");
        assert_eq!(
            driver.resources.len(),
            MAX_RETAINED_ENTRIES,
            "{LIST_BOUNDED}"
        );
        assert!(driver.listing.is_none());
    }

    #[test]
    fn rotating_partial_scans_bound_retained_rows_and_emit_evictions() {
        let (backend, _) = fixture();
        let mut driver = remote_driver(backend);
        let mut presented = HashMap::new();
        let total = MAX_RETAINED_ENTRIES * ROTATING_GENERATIONS;
        for offset in (0..total).step_by(PAGE_SIZE as usize) {
            seed_listing(&mut driver, false);
            let mut result = Ok(ListResult {
                entries: (offset..offset + PAGE_SIZE as usize)
                    .map(indexed_entry)
                    .collect(),
                continuation: None,
                incomplete: true,
            });
            let mut removed = Vec::new();
            let mut authoritative = false;
            assert!(driver.install_listing(&mut result, &mut removed, &mut authoritative));
            for path in removed {
                presented.remove(&path);
            }
            for entry in result.unwrap().entries {
                presented.insert(entry.path.clone(), entry);
            }
            assert!(presented.len() <= MAX_RETAINED_ENTRIES, "{LIST_BOUNDED}");
            assert_eq!(
                driver.resource_order.len(),
                driver.resources.len(),
                "{LIST_BOUNDED}"
            );
            assert_eq!(presented.len(), driver.resources.len(), "{LIST_BOUNDED}");
            assert!(!authoritative);
        }
        assert_eq!(presented, driver.resources);
        assert!(presented.contains_key(&indexed_entry(total - 1).path));
        assert!(!presented.contains_key(&indexed_entry(0).path));
    }

    fn large_tree_entry(directory: usize, file: Option<usize>) -> ResourceEntry {
        let path = match file {
            Some(file) => format!("dir-{directory}/file-{file}"),
            None => format!("dir-{directory}"),
        };
        ResourceEntry {
            path: WorkbenchPath::Remote(WorkspacePath::new(path).unwrap()),
            resource_id: None,
            revision: None,
            kind: if file.is_some() {
                ResourceKind::File
            } else {
                ResourceKind::Directory
            },
            size_bytes: None,
        }
    }

    #[test_case(MAX_RETAINED_ENTRIES, 4, false; "twenty_five_thousand_entries")]
    #[test_case(MAX_RETAINED_ENTRIES, 9, false; "exactly_at_capacity")]
    #[test_case(MAX_RETAINED_ENTRIES, 10, false; "above_capacity")]
    #[test_case(LEGACY_RETAINED_LIMIT, 4, false; "legacy_capacity_directories_first")]
    #[test_case(LEGACY_RETAINED_LIMIT, 4, true; "legacy_capacity_children_first")]
    fn shallow_then_recursive_capacity_preserves_structural_closure(
        limit: usize,
        files: usize,
        children_first: bool,
    ) {
        let (backend, _) = fixture();
        let mut driver = remote_driver(backend);
        driver.set_retained_limit(limit);
        seed_listing(&mut driver, false);
        driver.listing.as_mut().unwrap().indexing = false;
        let pinned_directory = large_tree_entry(LARGE_TREE_DIRECTORIES - 1, None).path;
        let pinned_file = large_tree_entry(0, Some(0)).path;
        driver.set_pinned_paths([pinned_directory.clone(), pinned_file.clone()].into_iter());
        let directories = (0..LARGE_TREE_DIRECTORIES)
            .map(|dir| large_tree_entry(dir, None))
            .collect::<Vec<_>>();
        let children = (0..LARGE_TREE_DIRECTORIES)
            .flat_map(|dir| (0..files).map(move |file| large_tree_entry(dir, Some(file))))
            .collect::<Vec<_>>();
        let recursive = if children_first {
            children.iter().chain(&directories)
        } else {
            directories.iter().chain(&children)
        }
        .cloned()
        .collect::<Vec<_>>();
        let mut presented = HashMap::new();
        let mut incomplete = false;
        for phase in [&directories, &recursive] {
            for (index, page) in phase.chunks(PAGE_SIZE as usize).enumerate() {
                let last = (index + 1) * PAGE_SIZE as usize >= phase.len();
                let mut result = Ok(ListResult {
                    entries: page.to_vec(),
                    continuation: (!last)
                        .then(|| ContinuationToken::new(index.to_string()).unwrap()),
                    incomplete: false,
                });
                let mut removed = Vec::new();
                let mut authoritative = false;
                let complete =
                    driver.install_listing(&mut result, &mut removed, &mut authoritative);
                let result = result.unwrap();
                assert!(
                    result.entries.len() <= MAX_LIST_DELTA_ENTRIES,
                    "{DELTA_BOUNDED}"
                );
                incomplete |= result.incomplete;
                for path in removed {
                    presented.remove(&path);
                }
                for entry in result.entries {
                    presented.insert(entry.path.clone(), entry);
                }
                assert!(presented.len() <= limit, "{LIST_BOUNDED}");
                assert_eq!(
                    driver.resource_order.len(),
                    driver.resources.len(),
                    "{LIST_BOUNDED}"
                );
                assert!(
                    driver.child_counts.len() <= driver.resources.len(),
                    "{LIST_BOUNDED}"
                );
                if complete {
                    break;
                }
            }
        }
        assert_eq!(incomplete, directories.len() + children.len() > limit);
        assert_eq!(presented, driver.resources);
        assert!(
            presented.contains_key(&pinned_directory),
            "{TREE_REACHABLE}"
        );
        assert!(presented.contains_key(&pinned_file), "{TREE_REACHABLE}");
        for entry in presented.values() {
            for parent in driver.ancestors(&entry.path).unwrap() {
                assert!(
                    presented
                        .get(&parent)
                        .is_some_and(|entry| entry.kind == ResourceKind::Directory),
                    "{TREE_REACHABLE}"
                );
            }
        }
        let mut tree = Tree::remote(driver.root.clone(), false);
        tree.update_remote(&presented.into_values().collect::<Vec<_>>(), &[]);
        tree.reveal_workbench_path(&pinned_file);
        assert_eq!(
            tree.selected().unwrap().path,
            pinned_file,
            "{TREE_REACHABLE}"
        );
    }

    #[test]
    fn capped_scan_runs_one_coalesced_followup_after_an_already_processed_file_changes() {
        smol::block_on(async {
            let (backend, fake) = fixture();
            let control = RemoteControl(fake);
            control.insert(NESTED, ORIGINAL);
            let calls = control.list_calls();
            let mut driver = remote_driver(backend);
            driver.set_retained_limit(TARGETED_RETAINED_LIMIT);
            let request = driver.list(driver.root.clone(), true);
            for _ in [FILE, DIRECTORY] {
                let call = calls.recv_async().await.unwrap();
                assert!(!call.request.recursive);
                call.reply.send(Ok(())).unwrap();
                drain_next(&mut driver, Instant::now()).await;
            }
            let first = calls.recv_async().await.unwrap();
            assert!(first.request.recursive && first.request.continuation.is_none());
            first.reply.send(Ok(())).unwrap();
            drain_next(&mut driver, Instant::now()).await;
            control.replace(FILE, CHANGED);
            assert_eq!(driver.list(driver.root.clone(), true), request);
            for _ in [DIRECTORY, NESTED] {
                calls
                    .recv_async()
                    .await
                    .unwrap()
                    .reply
                    .send(Ok(()))
                    .unwrap();
                drain_next(&mut driver, Instant::now()).await;
            }
            let followup = calls.recv_async().await.unwrap();
            assert!(
                followup.request.recursive && followup.request.continuation.is_none(),
                "{LIST_BOUNDED}"
            );
            followup.reply.send(Ok(())).unwrap();
            drain_next(&mut driver, Instant::now()).await;
            let path = WorkbenchPath::Remote(WorkspacePath::new(FILE).unwrap());
            assert_eq!(
                driver.resource(&path).unwrap().size_bytes,
                Some(CHANGED.len() as u64)
            );
            for _ in [DIRECTORY, NESTED] {
                calls
                    .recv_async()
                    .await
                    .unwrap()
                    .reply
                    .send(Ok(()))
                    .unwrap();
                drain_next(&mut driver, Instant::now()).await;
            }
            assert!(driver.listing.is_none(), "{LIST_BOUNDED}");
            assert!(calls.is_empty(), "{LIST_BOUNDED}");
        });
    }

    #[test]
    fn pinned_path_overflow_preserves_retained_targets_without_an_unbounded_pin_set() {
        let (backend, _) = fixture();
        let mut driver = remote_driver(backend);
        driver.set_retained_limit(TARGETED_RETAINED_LIMIT);
        driver.cache_entry(&indexed_entry(0));
        driver.cache_entry(&indexed_entry(1));
        driver.set_pinned_paths(
            (TARGETED_RETAINED_LIMIT..MAX_PINNED_PATHS + TARGETED_RETAINED_LIMIT + 1)
                .map(|index| indexed_entry(index).path),
        );
        assert_eq!(driver.pinned.len(), MAX_PINNED_PATHS, "{LIST_BOUNDED}");
        seed_listing(&mut driver, false);
        let mut result = Ok(ListResult {
            entries: vec![indexed_entry(TARGETED_RETAINED_LIMIT)],
            continuation: None,
            incomplete: false,
        });
        let mut removed = Vec::new();
        let mut authoritative = false;
        assert!(driver.install_listing(&mut result, &mut removed, &mut authoritative));
        assert!(result.unwrap().incomplete);
        assert!(removed.is_empty());
        assert!(!authoritative);
        assert!(
            driver.resources.contains_key(&indexed_entry(0).path),
            "{TREE_REACHABLE}"
        );
        assert!(
            driver.resources.contains_key(&indexed_entry(1).path),
            "{TREE_REACHABLE}"
        );
    }

    #[test]
    fn completed_scan_reconciles_with_a_refresh_already_queued() {
        let (backend, _) = fixture();
        let mut driver = remote_driver(backend);
        let old = indexed_entry(0);
        let fresh = indexed_entry(1);
        driver.cache_entry(&old);
        seed_listing(&mut driver, true);
        let mut result = Ok(ListResult {
            entries: vec![fresh.clone()],
            continuation: None,
            incomplete: false,
        });
        let mut removed = Vec::new();
        let mut authoritative = false;
        assert!(!driver.install_listing(&mut result, &mut removed, &mut authoritative));
        assert!(authoritative, "{LIST_RETAINED}");
        assert_eq!(removed, [old.path], "{LIST_RETAINED}");
        assert_eq!(driver.resources.len(), 1);
        assert_eq!(driver.resource(&fresh.path), Some(&fresh));
        assert!(driver.listing.is_some());
    }

    #[test_case(false; "late_failure")]
    #[test_case(true; "queued_refresh_after_failure")]
    fn shallow_pages_are_installed_before_later_pages_and_keep_previous_rows(refresh: bool) {
        smol::block_on(async {
            let (backend, fake) = fixture();
            let control = RemoteControl(fake.clone());
            control.insert(NESTED, ORIGINAL);
            let old = backend
                .resolve_entry(&WorkspacePath::new(NESTED).unwrap())
                .await
                .unwrap();
            let calls = control.list_calls();
            let mut driver = remote_driver(backend);
            driver.cache_entry(&old);
            let request = driver.list(driver.root.clone(), true);
            let first = calls.recv_async().await.unwrap();
            assert!(!first.request.recursive, "{LIST_BOUNDED}");
            first.reply.send(Ok(())).unwrap();
            let events = drain_next(&mut driver, Instant::now()).await;
            assert!(
                matches!(&events[..], [BackendEvent::Listed { complete: false, result: Ok(page), .. }] if page.entries.len() == 1),
                "{LIST_RETAINED}"
            );
            let later = calls.recv_async().await.unwrap();
            assert!(later.request.continuation.is_some(), "{LIST_BOUNDED}");
            assert!(driver.resource(&old.path).is_some(), "{LIST_RETAINED}");
            if refresh {
                assert_eq!(
                    driver.list(driver.root.clone(), true),
                    request,
                    "{LIST_BOUNDED}"
                );
                assert_eq!(
                    driver.list(driver.root.clone(), true),
                    request,
                    "{LIST_BOUNDED}"
                );
            }
            later.reply.send(Err(WorkspaceError::Unavailable)).unwrap();
            let events = drain_next(&mut driver, Instant::now()).await;
            assert!(
                matches!(&events[..], [BackendEvent::Listed { complete, result: Err(_), .. }] if *complete != refresh)
            );
            assert!(driver.resource(&old.path).is_some(), "{LIST_RETAINED}");
            assert_eq!(driver.resources.len(), 2, "{LIST_RETAINED}");
            if !refresh {
                assert_ne!(driver.list(driver.root.clone(), true), request);
            }
            let retry = calls.recv_async().await.unwrap();
            assert!(retry.request.continuation.is_none(), "{LIST_BOUNDED}");
            driver.suspend();
        });
    }

    #[test_case(false, false; "incomplete")]
    #[test_case(true, false; "repeated_cursor")]
    #[test_case(false, true; "page_ceiling")]
    fn incomplete_listings_deduplicate_without_removing_old_entries(repeated: bool, ceiling: bool) {
        smol::block_on(async {
            let (backend, fake) = fixture();
            let old = backend
                .resolve_entry(&WorkspacePath::new(DIRECTORY).unwrap())
                .await
                .unwrap();
            let entry = backend
                .resolve_entry(&WorkspacePath::new(FILE).unwrap())
                .await
                .unwrap();
            let calls = RemoteControl(fake).list_calls();
            let mut driver = remote_driver(backend);
            driver.cache_entry(&old);
            let request = driver.list(driver.root.clone(), false);
            let _blocked = calls.recv_async().await.unwrap();
            let token = ContinuationToken::new("next").unwrap();
            if repeated {
                driver
                    .listing
                    .as_mut()
                    .unwrap()
                    .cursors
                    .insert(token.clone());
            }
            if ceiling {
                driver.listing.as_mut().unwrap().pages = MAX_LIST_PAGES - 1;
            }
            driver
                .sender
                .send(Envelope {
                    generation: driver.generation,
                    event: BackendEvent::Listed {
                        request,
                        parent: driver.root.clone(),
                        complete: false,
                        authoritative: false,
                        removed: Vec::new(),
                        result: Ok(ListResult {
                            entries: vec![entry.clone(), entry],
                            continuation: (repeated || ceiling).then_some(token),
                            incomplete: !repeated && !ceiling,
                        }),
                    },
                })
                .unwrap();
            let events = driver.drain();
            assert!(
                matches!(&events[..], [BackendEvent::Listed { complete: true, result: Ok(page), .. }] if page.incomplete && page.entries.len() == 1),
                "{LIST_RETAINED}"
            );
            assert!(driver.resource(&old.path).is_some(), "{LIST_RETAINED}");
            assert!(driver.listing.is_none(), "{LIST_BOUNDED}");
        });
    }

    #[test_case(false; "cancelled_request")]
    #[test_case(true; "suspended_generation")]
    fn stale_listing_responses_cannot_replace_new_rows(suspend: bool) {
        smol::block_on(async {
            let (backend, fake) = fixture();
            let calls = RemoteControl(fake).list_calls();
            let mut driver = remote_driver(backend);
            let request = driver.list(driver.root.clone(), false);
            let _old_call = calls.recv_async().await.unwrap();
            let generation = driver.generation;
            if suspend {
                driver.suspend();
            } else {
                driver.cancel(request);
            }
            let fresh = driver.list(driver.root.clone(), false);
            let first = calls.recv_async().await.unwrap();
            first.reply.send(Ok(())).unwrap();
            drain_next(&mut driver, Instant::now()).await;
            let second = calls.recv_async().await.unwrap();
            second.reply.send(Ok(())).unwrap();
            drain_next(&mut driver, Instant::now()).await;
            driver
                .sender
                .send(Envelope {
                    generation,
                    event: BackendEvent::Listed {
                        request,
                        parent: driver.root.clone(),
                        complete: true,
                        authoritative: true,
                        removed: Vec::new(),
                        result: Ok(ListResult {
                            entries: Vec::new(),
                            continuation: None,
                            incomplete: false,
                        }),
                    },
                })
                .unwrap();
            assert_ne!(request, fresh);
            assert!(driver.drain().is_empty(), "{LIST_RETAINED}");
            assert_eq!(driver.resources.len(), 2, "{LIST_RETAINED}");
        });
    }

    #[test_case(false, false, false; "unrevisioned_delete")]
    #[test_case(true, false, false; "unrevisioned_rename")]
    #[test_case(false, true, false; "revisioned_delete")]
    #[test_case(true, true, false; "revisioned_rename")]
    #[test_case(false, false, true; "delete_stat_race")]
    #[test_case(true, false, true; "rename_stat_race")]
    fn directory_mutations_stat_only_when_needed_and_remain_conditional(
        rename: bool,
        revisioned: bool,
        conflict: bool,
    ) {
        smol::block_on(async {
            let (mut backend, fake) = fixture();
            let path = WorkspacePath::new(DIRECTORY).unwrap();
            let mut entry = backend.resolve_entry(&path).await.unwrap();
            if !revisioned {
                entry.revision = None;
            }
            if conflict {
                let fake = fake.clone();
                backend.gate = MutationGate::new(move || {
                    let fake = fake.clone();
                    Box::pin(async move {
                        RemoteControl(fake).replace(DIRECTORY, CHANGED);
                        Ok(())
                    })
                });
            }
            let destination = WorkbenchPath::Remote(WorkspacePath::new(RENAMED).unwrap());
            let result = if rename {
                backend.rename(&entry, &destination).await.map(|_| ())
            } else {
                backend.delete(&entry).await
            };
            assert_eq!(
                fake.stat_calls.load(Ordering::Relaxed),
                usize::from(!revisioned),
                "{MUTATION_CONDITIONAL}"
            );
            if conflict {
                assert!(
                    matches!(result, Err(BackendError::Conflict)),
                    "{MUTATION_CONDITIONAL}"
                );
                assert!(
                    fake.files.lock().unwrap().contains_key(&path),
                    "{MUTATION_CONDITIONAL}"
                );
            } else {
                result.unwrap();
                assert!(
                    !fake.files.lock().unwrap().contains_key(&path),
                    "{MUTATION_CONDITIONAL}"
                );
            }
        });
    }

    #[test_case(WorkspaceError::WatchUnavailable, true; "watch_unavailable")]
    #[test_case(WorkspaceError::Transport { kind: TransportErrorKind::Timeout }, true; "timeout")]
    #[test_case(WorkspaceError::IdentityMismatch, false; "identity")]
    #[test_case(WorkspaceError::PermissionDenied, false; "permission")]
    #[test_case(WorkspaceError::PolicyDenied, false; "policy")]
    fn watch_failures_retry_with_injected_time_and_warn_once(error: WorkspaceError, retry: bool) {
        smol::block_on(async {
            let (backend, fake) = fixture();
            let (sender, calls) = flume::unbounded();
            *fake.watch_probe.lock().unwrap() = Some(sender);
            let mut driver = remote_driver(backend);
            let now = Instant::now();
            driver.open_watch();
            let first = calls.recv_async().await.unwrap();
            let next_request = driver.next_request;
            driver.open_watch();
            assert_eq!(driver.next_request, next_request, "{WATCH_BOUNDED}");
            first.send(Err(error.clone())).unwrap();
            assert_eq!(
                drain_next(&mut driver, now).await.len(),
                1,
                "{WATCH_BOUNDED}"
            );
            assert_eq!(driver.watch_retry.is_some(), retry, "{WATCH_BOUNDED}");
            assert!(driver.listing.is_none(), "{WATCH_BOUNDED}");
            if retry {
                driver.drain_at(now);
                assert_eq!(driver.next_request, next_request, "{WATCH_BOUNDED}");
                for _ in 0..MAX_WATCH_RETRIES {
                    let deadline = driver.watch_retry.unwrap();
                    driver.drain_at(deadline);
                    calls
                        .recv_async()
                        .await
                        .unwrap()
                        .send(Err(error.clone()))
                        .unwrap();
                    assert!(
                        drain_next(&mut driver, deadline).await.is_empty(),
                        "{WATCH_BOUNDED}"
                    );
                }
                assert!(driver.watch_retry.is_none(), "{WATCH_BOUNDED}");
            }
            driver.suspend();
            let next_request = driver.next_request;
            driver.drain_at(now + WATCH_RETRY_MAX_DELAY * MAX_WATCH_RETRIES);
            assert_eq!(driver.next_request, next_request, "{WATCH_BOUNDED}");
        });
    }

    #[test_case(false, false; "pending_open_suspend")]
    #[test_case(true, false; "queued_open_suspend")]
    #[test_case(false, true; "pending_open_rebind")]
    #[test_case(true, true; "queued_open_rebind")]
    fn stopped_watches_close_even_when_open_response_is_pending(queued: bool, rebind: bool) {
        smol::block_on(async {
            let (backend, fake) = fixture();
            let (sender, calls) = flume::unbounded();
            *fake.watch_probe.lock().unwrap() = Some(sender);
            let mut driver = remote_driver(backend);
            driver.open_watch();
            let pending = calls.recv_async().await.unwrap();
            if queued {
                pending.send(Ok(())).unwrap();
                let envelope = driver.events.recv_async().await.unwrap();
                driver.sender.send(envelope).unwrap();
            }
            if rebind {
                let (replacement, _) = fixture();
                driver.rebind(
                    WorkbenchBackend::custom(Arc::new(replacement)),
                    driver.root.clone(),
                );
            } else {
                driver.suspend();
            }
            if !queued {
                pending.send(Ok(())).unwrap();
            }
            fake.watch_closed.1.recv_async().await.unwrap();
            assert!(driver.drain().is_empty(), "{WATCH_BOUNDED}");
            assert!(fake.watch_closed.1.is_empty(), "{WATCH_BOUNDED}");
        });
    }

    #[test_case(false; "open_failure")]
    #[test_case(true; "poll_failure")]
    fn watch_recovery_rescans_once_then_suspension_closes_the_subscription(poll: bool) {
        smol::block_on(async {
            let (backend, fake) = fixture();
            let control = RemoteControl(fake.clone());
            let calls = control.watch_calls();
            let listings = control.list_calls();
            if poll {
                *fake.watch_poll_error.lock().unwrap() = Some(WorkspaceError::WatchUnavailable);
            }
            let mut driver = remote_driver(backend);
            let now = Instant::now();
            driver.open_watch();
            let first = calls.recv_async().await.unwrap();
            first
                .send(if poll {
                    Ok(())
                } else {
                    Err(WorkspaceError::WatchUnavailable)
                })
                .unwrap();
            drain_next(&mut driver, now).await;
            if poll {
                drain_next(&mut driver, now).await;
                fake.watch_closed.1.recv_async().await.unwrap();
            }
            assert_eq!(driver.listing.is_some(), poll, "{WATCH_BOUNDED}");
            let deadline = driver.watch_retry.unwrap();
            driver.drain_at(deadline);
            calls.recv_async().await.unwrap().send(Ok(())).unwrap();
            let events = drain_next(&mut driver, deadline).await;
            assert!(
                matches!(
                    &events[..],
                    [BackendEvent::WatchOpened {
                        result: Ok(Some(_)),
                        ..
                    }]
                ),
                "{WATCH_BOUNDED}"
            );
            let _listing = listings.recv_async().await.unwrap();
            assert!(driver.watch_retry.is_none(), "{WATCH_BOUNDED}");
            driver.suspend();
            fake.watch_closed.1.recv_async().await.unwrap();
            let request = driver.next_request;
            driver.drain_at(deadline + WATCH_RETRY_MAX_DELAY);
            assert_eq!(driver.next_request, request, "{WATCH_BOUNDED}");
            assert!(listings.is_empty(), "{WATCH_BOUNDED}");
        });
    }

    #[test_case(false; "suspend")]
    #[test_case(true; "rebind")]
    fn suspension_cancels_scheduled_watch_retry(rebind: bool) {
        smol::block_on(async {
            let (backend, fake) = fixture();
            let calls = RemoteControl(fake).watch_calls();
            let mut driver = remote_driver(backend);
            let now = Instant::now();
            driver.open_watch();
            calls
                .recv_async()
                .await
                .unwrap()
                .send(Err(WorkspaceError::WatchUnavailable))
                .unwrap();
            drain_next(&mut driver, now).await;
            assert!(driver.watch_retry.is_some(), "{WATCH_BOUNDED}");
            if rebind {
                let (backend, _) = fixture();
                driver.rebind(
                    WorkbenchBackend::custom(Arc::new(backend)),
                    driver.root.clone(),
                );
            } else {
                driver.suspend();
            }
            let request = driver.next_request;
            driver.drain_at(now + WATCH_RETRY_MAX_DELAY);
            assert_eq!(driver.next_request, request, "{WATCH_BOUNDED}");
            assert!(driver.watch_retry.is_none(), "{WATCH_BOUNDED}");
        });
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

    #[test_case(WatchResyncReason::RetentionLost; "overflow")]
    #[test_case(WatchResyncReason::SubscriptionExpired; "expired_subscription")]
    fn remote_watch_validates_order_and_requests_resync_without_sleeping(
        reason: WatchResyncReason,
    ) {
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
                    state: WatchPollState::FullResync { reason },
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
                    complete: true,
                    authoritative: true,
                    removed: Vec::new(),
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
