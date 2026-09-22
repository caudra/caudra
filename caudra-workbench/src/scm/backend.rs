use std::collections::{BTreeSet, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use caudra_workspace::{
    OperationState, PreparedScmMutation, ScmCommit, ScmDiffLine, ScmDiffPage, ScmDiffRequest,
    ScmDiffTarget, ScmDiscoverRequest, ScmLogPage, ScmLogRequest, ScmMutation, ScmMutationResult,
    ScmReadSidePage, ScmReadSideRequest, ScmRepository, ScmRevision, ScmSide, ScmStatusEntry,
    ScmStatusPage, ScmStatusRequest, WorkspaceError, WorkspacePath, WorkspaceScmMutationService,
    WorkspaceScmReadService, WorkspaceSession,
};

use crate::fs::backend::MutationGate;

const PAGE_SIZE: u32 = 128;
const DIFF_LINES: u32 = 1_024;
const DIFF_BYTES: u32 = 512 * 1_024;
const SIDE_LINES: u32 = 1_024;
const SIDE_BYTES: u32 = 512 * 1_024;
const MAX_PAGES: usize = 1_024;
const MAX_OPERATION_POLLS: usize = 1_024;
const SNAPSHOT_ATTEMPTS: usize = 3;
const OPERATION_POLL_INTERVAL: Duration = Duration::from_millis(25);

pub type RequestId = u64;

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub repository: ScmRepository,
    pub status: Vec<ScmStatusEntry>,
    pub commits: Vec<ScmCommit>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DiffResult {
    pub path: WorkspacePath,
    pub target: ScmDiffTarget,
    pub lines: Vec<ScmDiffLine>,
    pub old: String,
    pub new: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CommitFilesResult {
    pub commit: ScmRevision,
    pub lines: Vec<ScmDiffLine>,
    pub truncated: bool,
    pub incomplete: bool,
}

#[derive(Debug)]
pub enum Event {
    Refreshed {
        request: RequestId,
        result: Result<Snapshot, Error>,
    },
    Diffed {
        request: RequestId,
        result: Result<DiffResult, Error>,
    },
    CommitFiles {
        request: RequestId,
        result: Result<CommitFilesResult, Error>,
    },
    Mutated {
        request: RequestId,
        mutation: ScmMutation,
        result: Result<ScmMutationResult, Error>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("remote source control is unavailable")]
    Unavailable,
    #[error("remote source control response changed while it was being paged")]
    StaleResponse,
    #[error("remote source control returned an invalid response")]
    InvalidResponse,
    #[error("remote source control operation was cancelled")]
    Cancelled,
    #[error("a remote source control operation is already running")]
    Busy,
    #[error(
        "remote source control operation outcome is indeterminate; repository state was refreshed"
    )]
    Indeterminate,
    #[error("remote source control operation failed; repository state was refreshed")]
    MutationFailed,
    #[error("{0}")]
    Workspace(#[from] WorkspaceError),
}

struct Envelope {
    generation: u64,
    event: Event,
}

pub struct Driver {
    session: WorkspaceSession,
    read: Arc<dyn WorkspaceScmReadService>,
    mutation: Option<Arc<dyn WorkspaceScmMutationService>>,
    gate: MutationGate,
    sender: flume::Sender<Envelope>,
    receiver: flume::Receiver<Envelope>,
    generation: u64,
    next_request: RequestId,
    active_refresh: Option<RequestId>,
    refresh_again: bool,
    active_diff: Option<RequestId>,
    active_commit: Option<RequestId>,
    active_mutation: Option<(RequestId, Arc<AtomicBool>)>,
    cancelled: HashSet<RequestId>,
}

impl Driver {
    pub fn new_with_gate(session: WorkspaceSession, gate: MutationGate) -> Result<Self, Error> {
        let services = session.workspace().services();
        let read = services.scm_read.clone().ok_or(Error::Unavailable)?;
        let mutation = services.scm_mutation.clone();
        let (sender, receiver) = flume::unbounded();
        Ok(Self {
            session,
            read,
            mutation,
            gate,
            sender,
            receiver,
            generation: 0,
            next_request: 1,
            active_refresh: None,
            refresh_again: false,
            active_diff: None,
            active_commit: None,
            active_mutation: None,
            cancelled: HashSet::new(),
        })
    }

    pub fn refresh(&mut self) -> RequestId {
        self.cancel_repository_reads();
        if let Some(request) = self.active_refresh {
            self.refresh_again = true;
            return request;
        }
        let request = self.request_id();
        self.active_refresh = Some(request);
        let session = self.session.clone();
        let read = Arc::clone(&self.read);
        self.spawn(async move {
            Event::Refreshed {
                request,
                result: snapshot(&session, read.as_ref()).await,
            }
        });
        request
    }

    pub fn diff(
        &mut self,
        repository: &ScmRepository,
        path: WorkspacePath,
        target: ScmDiffTarget,
        old_side: ScmSide,
        new_side: ScmSide,
    ) -> RequestId {
        if let Some(request) = self.active_diff.replace(0) {
            self.cancelled.insert(request);
        }
        let request = self.request_id();
        self.active_diff = Some(request);
        let session = self.session.clone();
        let read = Arc::clone(&self.read);
        let repository = repository.clone();
        self.spawn(async move {
            Event::Diffed {
                request,
                result: diff(
                    &session,
                    read.as_ref(),
                    &repository,
                    path,
                    target,
                    old_side,
                    new_side,
                )
                .await,
            }
        });
        request
    }

    pub fn commit_files(
        &mut self,
        repository: &ScmRepository,
        commit: ScmRevision,
        parent: ScmRevision,
    ) -> RequestId {
        if let Some(request) = self.active_commit.replace(0) {
            self.cancelled.insert(request);
        }
        let request = self.request_id();
        self.active_commit = Some(request);
        let session = self.session.clone();
        let read = Arc::clone(&self.read);
        let repository = repository.clone();
        self.spawn(async move {
            let target = ScmDiffTarget::Tree {
                base: parent,
                target: commit.clone(),
            };
            Event::CommitFiles {
                request,
                result: diff_pages(&session, read.as_ref(), &repository, target, None)
                    .await
                    .map(|page| CommitFilesResult {
                        commit,
                        lines: page.lines,
                        truncated: page.truncated,
                        incomplete: page.incomplete,
                    }),
            }
        });
        request
    }

    pub fn mutate(
        &mut self,
        repository: &ScmRepository,
        mutation: ScmMutation,
    ) -> Result<RequestId, Error> {
        let service = self.mutation.clone().ok_or(Error::Unavailable)?;
        if self.active_mutation.is_some() {
            return Err(Error::Busy);
        }
        self.cancel_repository_reads();
        let request = self.request_id();
        let cancel = Arc::new(AtomicBool::new(false));
        self.active_mutation = Some((request, Arc::clone(&cancel)));
        let session = self.session.clone();
        let gate = self.gate.clone();
        let repository = repository.clone();
        let event_mutation = mutation.clone();
        self.spawn(async move {
            Event::Mutated {
                request,
                mutation: event_mutation,
                result: match gate.ensure().await {
                    Ok(()) => {
                        mutate(&session, service.as_ref(), &repository, mutation, cancel).await
                    }
                    Err(_) => Err(Error::Workspace(WorkspaceError::Unavailable)),
                },
            }
        });
        Ok(request)
    }

    pub fn cancel_mutation(&mut self) -> bool {
        let Some((_, cancel)) = &self.active_mutation else {
            return false;
        };
        cancel.store(true, Ordering::Release);
        true
    }

    pub fn is_busy(&self) -> bool {
        self.active_refresh.is_some()
            || self.active_diff.is_some()
            || self.active_commit.is_some()
            || self.active_mutation.is_some()
    }

    pub fn drain(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        let envelopes: Vec<_> = self.receiver.try_iter().collect();
        for envelope in envelopes {
            if envelope.generation != self.generation {
                continue;
            }
            let request = event_request(&envelope.event);
            if self.cancelled.remove(&request) {
                continue;
            }
            if matches!(&envelope.event, Event::Refreshed { .. }) && self.refresh_again {
                self.active_refresh = None;
                self.refresh_again = false;
                self.refresh();
                continue;
            }
            match &envelope.event {
                Event::Refreshed { .. } => self.active_refresh = None,
                Event::Diffed { .. } => self.active_diff = None,
                Event::Mutated { .. } => self.active_mutation = None,
                Event::CommitFiles { .. } => self.active_commit = None,
            }
            events.push(envelope.event);
        }
        events
    }

    pub fn suspend(&mut self) {
        self.cancel_mutation();
        self.generation = self.generation.wrapping_add(1);
        self.active_refresh = None;
        self.refresh_again = false;
        self.active_diff = None;
        self.active_commit = None;
        self.cancelled.clear();
    }

    fn request_id(&mut self) -> RequestId {
        let request = self.next_request;
        self.next_request = self.next_request.wrapping_add(1).max(1);
        request
    }

    fn cancel_repository_reads(&mut self) {
        for request in [self.active_diff.take(), self.active_commit.take()]
            .into_iter()
            .flatten()
        {
            self.cancelled.insert(request);
        }
    }

    fn spawn<F>(&self, future: F)
    where
        F: Future<Output = Event> + Send + 'static,
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

impl Drop for Driver {
    fn drop(&mut self) {
        self.suspend();
    }
}

async fn snapshot(
    session: &WorkspaceSession,
    service: &dyn WorkspaceScmReadService,
) -> Result<Snapshot, Error> {
    for _ in 1..SNAPSHOT_ATTEMPTS {
        match snapshot_once(session, service).await {
            Err(Error::StaleResponse) => {}
            result => return result,
        }
    }
    snapshot_once(session, service).await
}

async fn snapshot_once(
    session: &WorkspaceSession,
    service: &dyn WorkspaceScmReadService,
) -> Result<Snapshot, Error> {
    let repository = service
        .discover(
            session.binding(),
            session.cursor(),
            &ScmDiscoverRequest {
                path: WorkspacePath::root(),
            },
        )
        .await?
        .repository;
    let status = status_pages(session, service, &repository).await?;
    if status.revisions.repository != repository.revisions.repository {
        return Err(Error::StaleResponse);
    }
    let log = log_pages(session, service, &repository).await?;
    if log.head_revision != status.revisions.head {
        return Err(Error::StaleResponse);
    }
    let mut repository = repository;
    repository.revisions = status.revisions;
    let mut warnings = Vec::new();
    if status.truncated || status.incomplete {
        warnings.push("Remote status is incomplete".to_owned());
    }
    if log.truncated || log.incomplete {
        warnings.push("Remote history is incomplete".to_owned());
    }
    Ok(Snapshot {
        repository,
        status: status.entries,
        commits: log.commits,
        warnings,
    })
}

async fn status_pages(
    session: &WorkspaceSession,
    service: &dyn WorkspaceScmReadService,
    repository: &ScmRepository,
) -> Result<ScmStatusPage, Error> {
    let mut continuation = None;
    let mut seen = HashSet::new();
    let mut combined: Option<ScmStatusPage> = None;
    for _ in 0..MAX_PAGES {
        let page = service
            .status(
                session.binding(),
                session.cursor(),
                &ScmStatusRequest {
                    repository_handle: repository.handle.clone(),
                    page_size: PAGE_SIZE,
                    continuation: continuation.clone(),
                },
            )
            .await?;
        if let Some(current) = &mut combined {
            if current.collection_revision != page.collection_revision
                || current.revisions != page.revisions
            {
                return Err(Error::StaleResponse);
            }
            current.entries.extend(page.entries);
            current.truncated = page.truncated;
            current.incomplete |= page.incomplete;
            current.continuation = page.continuation;
        } else {
            combined = Some(page);
        }
        let current = combined.as_ref().ok_or(Error::InvalidResponse)?;
        let Some(next) = current.continuation.clone() else {
            return combined.ok_or(Error::InvalidResponse);
        };
        if !seen.insert(next.clone()) {
            return Err(Error::InvalidResponse);
        }
        continuation = Some(next);
    }
    Err(Error::InvalidResponse)
}

async fn log_pages(
    session: &WorkspaceSession,
    service: &dyn WorkspaceScmReadService,
    repository: &ScmRepository,
) -> Result<ScmLogPage, Error> {
    let mut continuation = None;
    let mut seen = HashSet::new();
    let mut combined: Option<ScmLogPage> = None;
    for _ in 0..MAX_PAGES {
        let page = service
            .log(
                session.binding(),
                session.cursor(),
                &ScmLogRequest {
                    repository_handle: repository.handle.clone(),
                    page_size: PAGE_SIZE,
                    continuation: continuation.clone(),
                },
            )
            .await?;
        if let Some(current) = &mut combined {
            if current.collection_revision != page.collection_revision
                || current.head_revision != page.head_revision
            {
                return Err(Error::StaleResponse);
            }
            current.commits.extend(page.commits);
            current.truncated = page.truncated;
            current.incomplete |= page.incomplete;
            current.continuation = page.continuation;
        } else {
            combined = Some(page);
        }
        let current = combined.as_ref().ok_or(Error::InvalidResponse)?;
        let Some(next) = current.continuation.clone() else {
            return combined.ok_or(Error::InvalidResponse);
        };
        if !seen.insert(next.clone()) {
            return Err(Error::InvalidResponse);
        }
        continuation = Some(next);
    }
    Err(Error::InvalidResponse)
}

async fn diff(
    session: &WorkspaceSession,
    service: &dyn WorkspaceScmReadService,
    repository: &ScmRepository,
    path: WorkspacePath,
    target: ScmDiffTarget,
    old_side: ScmSide,
    new_side: ScmSide,
) -> Result<DiffResult, Error> {
    let page = diff_pages(
        session,
        service,
        repository,
        target.clone(),
        Some(path.clone()),
    )
    .await?;
    let old = side_pages(session, service, repository, path.clone(), old_side).await?;
    let new = side_pages(session, service, repository, path.clone(), new_side).await?;
    let mut warnings = Vec::new();
    if page.truncated || page.incomplete {
        warnings.push("Remote diff is incomplete".to_owned());
    }
    if old.truncated || old.incomplete || new.truncated || new.incomplete {
        warnings.push("Remote diff side is incomplete".to_owned());
    }
    Ok(DiffResult {
        path,
        target,
        lines: page.lines,
        old: old.content,
        new: new.content,
        warnings,
    })
}

async fn diff_pages(
    session: &WorkspaceSession,
    service: &dyn WorkspaceScmReadService,
    repository: &ScmRepository,
    target: ScmDiffTarget,
    path: Option<WorkspacePath>,
) -> Result<ScmDiffPage, Error> {
    let mut continuation = None;
    let mut seen = HashSet::new();
    let mut combined: Option<ScmDiffPage> = None;
    for _ in 0..MAX_PAGES {
        let page = service
            .diff(
                session.binding(),
                session.cursor(),
                &ScmDiffRequest {
                    repository_handle: repository.handle.clone(),
                    target: target.clone(),
                    path: path.clone(),
                    max_lines: DIFF_LINES,
                    max_bytes: DIFF_BYTES,
                    continuation: continuation.clone(),
                },
            )
            .await?;
        if page.repository_revision != repository.revisions.repository {
            return Err(Error::StaleResponse);
        }
        if let Some(current) = &mut combined {
            if current.collection_revision != page.collection_revision
                || current.repository_revision != page.repository_revision
            {
                return Err(Error::StaleResponse);
            }
            current.lines.extend(page.lines);
            current.truncated = page.truncated;
            current.incomplete |= page.incomplete;
            current.continuation = page.continuation;
        } else {
            combined = Some(page);
        }
        let current = combined.as_ref().ok_or(Error::InvalidResponse)?;
        let Some(next) = current.continuation.clone() else {
            return combined.ok_or(Error::InvalidResponse);
        };
        if !seen.insert(next.clone()) {
            return Err(Error::InvalidResponse);
        }
        continuation = Some(next);
    }
    Err(Error::InvalidResponse)
}

async fn side_pages(
    session: &WorkspaceSession,
    service: &dyn WorkspaceScmReadService,
    repository: &ScmRepository,
    path: WorkspacePath,
    side: ScmSide,
) -> Result<ScmReadSidePage, Error> {
    let mut start_line = 1;
    let mut combined: Option<ScmReadSidePage> = None;
    for _ in 0..MAX_PAGES {
        let page = service
            .read_side(
                session.binding(),
                session.cursor(),
                &ScmReadSideRequest {
                    repository_handle: repository.handle.clone(),
                    path: path.clone(),
                    side: side.clone(),
                    start_line,
                    max_lines: SIDE_LINES,
                    max_bytes: SIDE_BYTES,
                },
            )
            .await?;
        if page.repository_revision != repository.revisions.repository
            || page.path != path
            || page.side != side
            || page.start_line != start_line
        {
            return Err(Error::StaleResponse);
        }
        if let Some(current) = &mut combined {
            if current.resource_id != page.resource_id
                || current.revision != page.revision
                || current.total_lines != page.total_lines
            {
                return Err(Error::StaleResponse);
            }
            current.content.push_str(&page.content);
            current.end_line = page.end_line;
            current.truncated = page.truncated;
            current.incomplete |= page.incomplete;
            current.next_start_line = page.next_start_line;
        } else {
            combined = Some(page);
        }
        let current = combined.as_ref().ok_or(Error::InvalidResponse)?;
        let Some(next) = current.next_start_line else {
            return combined.ok_or(Error::InvalidResponse);
        };
        if next <= start_line {
            return Err(Error::InvalidResponse);
        }
        start_line = next;
    }
    Err(Error::InvalidResponse)
}

async fn mutate(
    session: &WorkspaceSession,
    service: &dyn WorkspaceScmMutationService,
    repository: &ScmRepository,
    mutation: ScmMutation,
    cancel: Arc<AtomicBool>,
) -> Result<ScmMutationResult, Error> {
    let prepared = service
        .prepare(
            session.binding(),
            session.cursor(),
            &repository.handle,
            &mutation,
        )
        .await?;
    if let Err(error) = validate_preview(repository, &mutation, &prepared) {
        let _ = service
            .release(session.binding(), session.cursor(), &prepared)
            .await;
        return Err(error);
    }
    if cancel.load(Ordering::Acquire) {
        let _ = service
            .cancel(session.binding(), session.cursor(), &prepared.operation)
            .await;
        let _ = service
            .release(session.binding(), session.cursor(), &prepared)
            .await;
        return Err(Error::Cancelled);
    }
    let mut status = match service
        .execute(session.binding(), session.cursor(), &prepared)
        .await
    {
        Ok(status) => status,
        Err(error) => {
            let _ = service
                .release(session.binding(), session.cursor(), &prepared)
                .await;
            return Err(error.into());
        }
    };
    for _ in 0..MAX_OPERATION_POLLS {
        if cancel.load(Ordering::Acquire) {
            let _ = service
                .cancel(session.binding(), session.cursor(), &status.handle)
                .await;
        }
        match status.state {
            OperationState::Completed { result, .. } => {
                let _ = service
                    .release(session.binding(), session.cursor(), &prepared)
                    .await;
                return (result.mutation == mutation)
                    .then_some(result)
                    .ok_or(Error::InvalidResponse);
            }
            OperationState::Cancelled { .. } => {
                let _ = service
                    .release(session.binding(), session.cursor(), &prepared)
                    .await;
                return Err(Error::Cancelled);
            }
            OperationState::Indeterminate { .. } | OperationState::Forgotten => {
                let _ = service
                    .release(session.binding(), session.cursor(), &prepared)
                    .await;
                return Err(Error::Indeterminate);
            }
            OperationState::Failed { .. } => {
                let _ = service
                    .release(session.binding(), session.cursor(), &prepared)
                    .await;
                return Err(Error::MutationFailed);
            }
            OperationState::NeverSeen | OperationState::Prepared | OperationState::Running => {}
        }
        smol::Timer::after(OPERATION_POLL_INTERVAL).await;
        status = match service
            .status(session.binding(), session.cursor(), &status.handle)
            .await
        {
            Ok(status) => status,
            Err(error) => {
                let _ = service
                    .release(session.binding(), session.cursor(), &prepared)
                    .await;
                return Err(error.into());
            }
        };
    }
    let _ = service
        .cancel(session.binding(), session.cursor(), &status.handle)
        .await;
    Err(Error::Indeterminate)
}

fn validate_preview(
    repository: &ScmRepository,
    mutation: &ScmMutation,
    prepared: &PreparedScmMutation,
) -> Result<(), Error> {
    if &prepared.preview.mutation != mutation
        || prepared.preview.repository_identity != repository.identity
        || prepared.preview.revisions != repository.revisions
    {
        return Err(Error::StaleResponse);
    }
    let requested = mutation_paths(mutation);
    let previewed: BTreeSet<_> = prepared
        .preview
        .entries
        .iter()
        .map(|entry| entry.path.clone())
        .collect();
    if requested != previewed {
        return Err(Error::InvalidResponse);
    }
    Ok(())
}

fn mutation_paths(mutation: &ScmMutation) -> BTreeSet<WorkspacePath> {
    match mutation {
        ScmMutation::Stage { paths }
        | ScmMutation::Unstage { paths }
        | ScmMutation::Discard { paths } => paths.iter().cloned().collect(),
    }
}

fn event_request(event: &Event) -> RequestId {
    match event {
        Event::Refreshed { request, .. }
        | Event::Diffed { request, .. }
        | Event::CommitFiles { request, .. }
        | Event::Mutated { request, .. } => *request,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use async_trait::async_trait;
    use caudra_workspace::{
        AuthenticatedPrincipalId, AuthorityIdentity, CancellationResult, CollectionRevision,
        ContinuationToken, CwdHandle, OperationHandle, OperationId, OperationPhase,
        OperationStatus, ProjectIdentity, ProjectKey, ReleaseResult, ResourceId, ResourceScope,
        ScmChangeKind, ScmDiffLineKind, ScmDiscoverResult, ScmMutationPreview,
        ScmRepositoryRevisions, SequenceMetadata, SessionBindingId, SessionWorkspaceBinding,
        SourceTrustAnchor, WorkspaceCapabilities, WorkspaceCapability, WorkspaceCursor,
        WorkspaceHandle, WorkspaceServices,
    };

    use super::*;

    const PATH: &str = "src/lib.rs";
    const PREVIEW_MISMATCH: &str = "prepared mutation preview must match the requested repository";
    const PAGINATION_LOST: &str = "all structured SCM pages must be combined without reparsing";
    const LIFECYCLE_LOST: &str = "prepared mutation lifecycle was not completed";

    #[derive(Clone, Copy)]
    enum MutationMode {
        Completed,
        Indeterminate,
        StalePreview,
        Locked,
    }

    struct FakeScm {
        repository: ScmRepository,
        mode: Mutex<MutationMode>,
        cancels: AtomicUsize,
        releases: AtomicUsize,
    }

    impl FakeScm {
        fn revisions(&self) -> ScmRepositoryRevisions {
            self.repository.revisions.clone()
        }

        fn handle(&self) -> OperationHandle {
            OperationHandle {
                preparation_id: OperationId::new("prepare").expect("valid operation"),
                invocation_id: Some(OperationId::new("invoke").expect("valid operation")),
                execution_id: Some(OperationId::new("execute").expect("valid operation")),
                expires_at_unix_ms: None,
            }
        }

        fn status(
            &self,
            state: OperationState<ScmMutationResult>,
        ) -> OperationStatus<ScmMutationResult> {
            OperationStatus {
                handle: self.handle(),
                state,
                progress: Vec::new(),
                progress_metadata: SequenceMetadata {
                    first_retained_sequence: None,
                    next_sequence: 0,
                    gap_before_first: false,
                },
            }
        }
    }

    #[async_trait]
    impl WorkspaceScmReadService for FakeScm {
        async fn discover(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _request: &ScmDiscoverRequest,
        ) -> Result<ScmDiscoverResult, WorkspaceError> {
            Ok(ScmDiscoverResult {
                repository: self.repository.clone(),
            })
        }

        async fn status(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            request: &ScmStatusRequest,
        ) -> Result<ScmStatusPage, WorkspaceError> {
            let continuation = request.continuation.as_ref().map(ContinuationToken::as_str);
            let (entries, continuation) = match continuation {
                None => (
                    vec![ScmStatusEntry {
                        path: WorkspacePath::new(PATH).expect("valid path"),
                        staged: Some(ScmChangeKind::Renamed),
                        unstaged: Some(ScmChangeKind::Modified),
                        untracked: false,
                        conflicted: true,
                    }],
                    Some(ContinuationToken::new("status-2").expect("valid continuation")),
                ),
                Some("status-2") => (
                    vec![ScmStatusEntry {
                        path: WorkspacePath::new("new.txt").expect("valid path"),
                        staged: None,
                        unstaged: None,
                        untracked: true,
                        conflicted: false,
                    }],
                    None,
                ),
                _ => return Err(WorkspaceError::Conflict),
            };
            Ok(ScmStatusPage {
                revisions: self.revisions(),
                collection_revision: CollectionRevision::new("status-revision")
                    .expect("valid revision"),
                entries,
                truncated: continuation.is_some(),
                incomplete: false,
                continuation,
            })
        }

        async fn log(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            request: &ScmLogRequest,
        ) -> Result<ScmLogPage, WorkspaceError> {
            let second = request.continuation.is_some();
            let id = if second { "commit-1" } else { "commit-2" };
            Ok(ScmLogPage {
                head_revision: self.repository.revisions.head.clone(),
                collection_revision: CollectionRevision::new("log-revision")
                    .expect("valid revision"),
                commits: vec![ScmCommit {
                    id: ScmRevision::new(id).expect("valid revision"),
                    parents: Vec::new(),
                    author_name: "Author".to_owned(),
                    author_email: "author@example.test".to_owned(),
                    committed_unix_seconds: 1,
                    summary: id.to_owned(),
                    body: None,
                }],
                truncated: !second,
                incomplete: false,
                continuation: (!second)
                    .then(|| ContinuationToken::new("log-2").expect("valid continuation")),
            })
        }

        async fn diff(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            request: &ScmDiffRequest,
        ) -> Result<ScmDiffPage, WorkspaceError> {
            let second = request.continuation.is_some();
            Ok(ScmDiffPage {
                repository_revision: self.repository.revisions.repository.clone(),
                collection_revision: CollectionRevision::new("diff-revision")
                    .expect("valid revision"),
                lines: vec![ScmDiffLine {
                    path: WorkspacePath::new(PATH).expect("valid path"),
                    kind: if second {
                        ScmDiffLineKind::Addition
                    } else {
                        ScmDiffLineKind::Deletion
                    },
                    change: Some(ScmChangeKind::Modified),
                    old_line: (!second).then_some(7),
                    new_line: second.then_some(7),
                    text: if second { "new" } else { "old" }.to_owned(),
                }],
                truncated: !second,
                incomplete: false,
                continuation: (!second)
                    .then(|| ContinuationToken::new("diff-2").expect("valid continuation")),
            })
        }

        async fn read_side(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            request: &ScmReadSideRequest,
        ) -> Result<ScmReadSidePage, WorkspaceError> {
            let first = request.start_line == 1;
            Ok(ScmReadSidePage {
                repository_revision: self.repository.revisions.repository.clone(),
                resource_id: ResourceId::new("side-resource").expect("valid resource"),
                revision: caudra_workspace::ResourceRevision::new(format!(
                    "side-{:?}",
                    request.side
                ))
                .expect("valid revision"),
                path: request.path.clone(),
                side: request.side.clone(),
                content: if first { "one\n" } else { "two\n" }.to_owned(),
                start_line: request.start_line,
                end_line: request.start_line,
                total_lines: 2,
                truncated: first,
                incomplete: false,
                next_start_line: first.then_some(2),
            })
        }
    }

    #[async_trait]
    impl WorkspaceScmMutationService for FakeScm {
        async fn prepare(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _repository_handle: &ResourceId,
            mutation: &ScmMutation,
        ) -> Result<PreparedScmMutation, WorkspaceError> {
            if matches!(*self.mode.lock().expect("mode"), MutationMode::Locked) {
                return Err(WorkspaceError::PendingOperation {
                    operation_id: "index-lock".to_owned(),
                });
            }
            let paths = mutation_paths(mutation);
            let mut revisions = self.revisions();
            if matches!(*self.mode.lock().expect("mode"), MutationMode::StalePreview) {
                revisions.index = ScmRevision::new("stale-index").expect("valid revision");
            }
            Ok(PreparedScmMutation {
                operation: self.handle(),
                preview: ScmMutationPreview {
                    mutation: mutation.clone(),
                    repository_identity: self.repository.identity.clone(),
                    revisions,
                    entries: paths
                        .into_iter()
                        .map(|path| ScmStatusEntry {
                            path,
                            staged: None,
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
            let result = ScmMutationResult {
                mutation: prepared.preview.mutation.clone(),
                revisions: self.revisions(),
            };
            let state = match *self.mode.lock().expect("mode") {
                MutationMode::Indeterminate => OperationState::Indeterminate {
                    side_effects_possible: true,
                },
                _ => OperationState::Completed {
                    result,
                    side_effects_possible: true,
                },
            };
            Ok(self.status(state))
        }

        async fn status(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _operation: &OperationHandle,
        ) -> Result<OperationStatus<ScmMutationResult>, WorkspaceError> {
            Ok(self.status(OperationState::Indeterminate {
                side_effects_possible: true,
            }))
        }

        async fn cancel(
            &self,
            _binding: &SessionWorkspaceBinding,
            _cursor: &WorkspaceCursor,
            _operation: &OperationHandle,
        ) -> Result<CancellationResult, WorkspaceError> {
            self.cancels.fetch_add(1, Ordering::Relaxed);
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
            self.releases.fetch_add(1, Ordering::Relaxed);
            Ok(ReleaseResult {
                state: OperationPhase::Completed,
                released: true,
            })
        }
    }

    fn revision(value: &str) -> ScmRevision {
        ScmRevision::new(value).expect("valid revision")
    }

    fn fixture() -> (WorkspaceSession, Arc<FakeScm>) {
        let authority = AuthorityIdentity::new(
            SourceTrustAnchor::new("test-anchor").expect("valid anchor"),
            "test-authority",
            "test-workspace",
            "test-generation",
            "test-namespace",
        )
        .expect("valid authority");
        let principal =
            AuthenticatedPrincipalId::new(authority.clone(), "principal").expect("valid principal");
        let project = ProjectIdentity::new(
            authority.clone(),
            ProjectKey::new("project").expect("valid project"),
        );
        let binding = SessionWorkspaceBinding::new(
            SessionBindingId::new("session").expect("valid session"),
            authority.clone(),
            principal,
            project,
        )
        .expect("valid binding");
        let revisions = ScmRepositoryRevisions {
            repository: revision("repository-1"),
            head: revision("head-1"),
            index: revision("index-1"),
            worktree: revision("worktree-1"),
        };
        let fake = Arc::new(FakeScm {
            repository: ScmRepository {
                handle: ResourceId::new("repository-handle").expect("valid handle"),
                resource_id: ResourceId::new("repository-resource").expect("valid resource"),
                root: WorkspacePath::root(),
                identity: revision("repository-identity"),
                revisions,
            },
            mode: Mutex::new(MutationMode::Completed),
            cancels: AtomicUsize::new(0),
            releases: AtomicUsize::new(0),
        });
        let capabilities = WorkspaceCapabilities::from([
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
                scm_read: Some(fake.clone()),
                scm_mutation: Some(fake.clone()),
                ..WorkspaceServices::default()
            },
        )
        .expect("valid workspace");
        let cursor = WorkspaceCursor::new(
            &binding,
            ResourceScope::root(ResourceId::new("root").expect("valid root")),
            1,
            CwdHandle::new("cwd").expect("valid cursor"),
        );
        (
            WorkspaceSession::new(workspace, binding, cursor).expect("valid session"),
            fake,
        )
    }

    #[test]
    fn paginated_read_models_are_combined_with_exact_revisions_and_lines() {
        smol::block_on(async {
            let (session, fake) = fixture();
            let snapshot = snapshot(&session, fake.as_ref()).await.expect("snapshot");
            assert_eq!(snapshot.status.len(), 2, "{PAGINATION_LOST}");
            assert_eq!(snapshot.commits.len(), 2, "{PAGINATION_LOST}");
            assert_eq!(snapshot.repository.revisions, fake.revisions());

            let result = diff(
                &session,
                fake.as_ref(),
                &fake.repository,
                WorkspacePath::new(PATH).expect("valid path"),
                ScmDiffTarget::Unstaged,
                ScmSide::Index,
                ScmSide::Worktree,
            )
            .await
            .expect("diff");
            assert_eq!(result.lines.len(), 2, "{PAGINATION_LOST}");
            assert_eq!(result.lines[0].kind, ScmDiffLineKind::Deletion);
            assert_eq!(result.lines[0].old_line, Some(7));
            assert_eq!(result.lines[1].kind, ScmDiffLineKind::Addition);
            assert_eq!(result.lines[1].new_line, Some(7));
            assert_eq!(result.old, "one\ntwo\n");
            assert_eq!(result.new, "one\ntwo\n");
        });
    }

    #[test]
    fn prepared_mutations_release_and_preserve_stale_lock_indeterminate_and_cancel_states() {
        smol::block_on(async {
            let (session, fake) = fixture();
            let path = WorkspacePath::new(PATH).expect("valid path");
            for mutation in [
                ScmMutation::Stage {
                    paths: vec![path.clone()],
                },
                ScmMutation::Unstage {
                    paths: vec![path.clone()],
                },
                ScmMutation::Discard {
                    paths: vec![path.clone()],
                },
            ] {
                let result = mutate(
                    &session,
                    fake.as_ref(),
                    &fake.repository,
                    mutation.clone(),
                    Arc::new(AtomicBool::new(false)),
                )
                .await
                .expect("completed mutation");
                assert_eq!(result.mutation, mutation, "{PREVIEW_MISMATCH}");
            }
            assert_eq!(fake.releases.load(Ordering::Relaxed), 3, "{LIFECYCLE_LOST}");

            *fake.mode.lock().expect("mode") = MutationMode::StalePreview;
            let stale = mutate(
                &session,
                fake.as_ref(),
                &fake.repository,
                ScmMutation::Stage {
                    paths: vec![path.clone()],
                },
                Arc::new(AtomicBool::new(false)),
            )
            .await;
            assert!(matches!(stale, Err(Error::StaleResponse)));

            *fake.mode.lock().expect("mode") = MutationMode::Locked;
            let locked = mutate(
                &session,
                fake.as_ref(),
                &fake.repository,
                ScmMutation::Stage {
                    paths: vec![path.clone()],
                },
                Arc::new(AtomicBool::new(false)),
            )
            .await;
            assert!(matches!(
                locked,
                Err(Error::Workspace(WorkspaceError::PendingOperation { .. }))
            ));

            *fake.mode.lock().expect("mode") = MutationMode::Indeterminate;
            let indeterminate = mutate(
                &session,
                fake.as_ref(),
                &fake.repository,
                ScmMutation::Stage {
                    paths: vec![path.clone()],
                },
                Arc::new(AtomicBool::new(false)),
            )
            .await;
            assert!(matches!(indeterminate, Err(Error::Indeterminate)));

            *fake.mode.lock().expect("mode") = MutationMode::Completed;
            let cancelled = mutate(
                &session,
                fake.as_ref(),
                &fake.repository,
                ScmMutation::Stage { paths: vec![path] },
                Arc::new(AtomicBool::new(true)),
            )
            .await;
            assert!(matches!(cancelled, Err(Error::Cancelled)));
            assert_eq!(fake.cancels.load(Ordering::Relaxed), 1, "{LIFECYCLE_LOST}");
            assert_eq!(fake.releases.load(Ordering::Relaxed), 6, "{LIFECYCLE_LOST}");
        });
    }
}
