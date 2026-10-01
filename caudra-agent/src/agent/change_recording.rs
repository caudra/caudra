//! Opens a change record before each call that may change the session
//! directory and finishes it once the call has run, so a file revert undoes
//! exactly what the call did.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use caudra_config::SnapshotsConfig;
use caudra_storage::id::CaudraId;
use caudra_storage::sessions::{RecordCoverage, UnrecordedCall};
use caudra_workspace::{
    RecordHolder, RecordLimits, RecordRequest, RecordScope, RecordTicket, WorkspaceChangeService,
    WorkspaceError, WorkspaceSession,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::tools::{ToolContext, ToolEffect};
use crate::{AgentEvent, EventSender, StoredSession};

pub const RECORD_BLOCKED: &str = "could not record the workspace before changing it";
pub const NO_CHANGE_RECORDS: &str = "the workspace keeps no change records";

/// Stored with each record and handed back as given, so a revert can tell
/// which call made it and whether it came after a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordClient {
    pub call_id: String,
    /// The call in the root conversation that this one ran under.
    pub root_call_id: String,
    /// On the clock history ids use, so "after a message" is `at > id`.
    pub at: CaudraId,
}

/// Where a recorder finds the change records of the session directory.
#[derive(Clone)]
pub enum ChangeSource {
    /// A handle bound once, like the local store's.
    Bound(Arc<dyn WorkspaceChangeService>),
    /// The remote session's, bound at each call to its binding and current
    /// cursor.
    Remote,
}

/// Records what the calls of one session change. Cheap to clone, and shared
/// by everything that session runs, so each record is held for the session
/// the user reverts.
#[derive(Clone)]
pub struct ChangeRecorder(Arc<RecorderInner>);

struct RecorderInner {
    source: ChangeSource,
    holder: RecordHolder,
    root: PathBuf,
    limits: RecordLimits,
    noticed: AtomicBool,
}

impl ChangeRecorder {
    /// `holder` is the root session's, and `root` the session directory the
    /// calls' targets are placed in. `None` when recording is switched off.
    pub fn new(
        source: ChangeSource,
        holder: RecordHolder,
        root: PathBuf,
        config: &SnapshotsConfig,
    ) -> Option<Self> {
        config.enabled.then(|| {
            Self(Arc::new(RecorderInner {
                source,
                holder,
                root,
                limits: RecordLimits {
                    max_files: u32::try_from(config.max_files).unwrap_or(u32::MAX),
                    max_file_bytes: config.max_file_bytes,
                    max_total_bytes: config.max_bytes,
                },
                noticed: AtomicBool::new(false),
            }))
        })
    }

    pub fn root(&self) -> &Path {
        &self.0.root
    }

    #[cfg(test)]
    pub(crate) fn holder(&self) -> &RecordHolder {
        &self.0.holder
    }

    fn service(&self, ctx: &ToolContext) -> Option<Arc<dyn WorkspaceChangeService>> {
        match &self.0.source {
            ChangeSource::Bound(service) => Some(Arc::clone(service)),
            ChangeSource::Remote => ctx
                .workspace_session
                .as_ref()
                .and_then(WorkspaceSession::changes),
        }
    }

    async fn begin(
        &self,
        ctx: &ToolContext,
        call_id: &str,
        scope: RecordScope,
    ) -> Result<Recording, String> {
        let client = RecordClient {
            call_id: call_id.to_owned(),
            root_call_id: ctx
                .local_root_tool_use_id
                .clone()
                .unwrap_or_else(|| call_id.to_owned()),
            at: CaudraId::generate(),
        };
        let Some(service) = self.service(ctx) else {
            self.report_gap(&ctx.event_tx, client, NO_CHANGE_RECORDS.to_owned());
            return Ok(Recording(None));
        };
        let request = RecordRequest {
            scope,
            holder: self.0.holder.clone(),
            client: serde_json::to_value(&client)
                .map_err(|error| format!("{RECORD_BLOCKED}: {error}"))?,
            limits: self.0.limits.clone(),
        };
        match service.begin(&request).await {
            Ok(ticket) => Ok(Recording(Some(Begun {
                recorder: self.clone(),
                service,
                ticket,
                client,
                events: ctx.event_tx.clone(),
                started: false,
            }))),
            Err(error) if is_refusal(&error) => {
                self.report_gap(&ctx.event_tx, client, error.to_string());
                Ok(Recording(None))
            }
            Err(error) => {
                warn!(%call_id, %error, "change record failed; the call is blocked");
                Err(format!("{RECORD_BLOCKED}: {error}"))
            }
        }
    }

    fn report_gap(&self, events: &EventSender, client: RecordClient, reason: String) {
        warn!(call_id = %client.call_id, %reason, "call runs without a change record");
        let notice = !self.0.noticed.swap(true, Ordering::Relaxed);
        events.try_send(AgentEvent::Unrecorded {
            gap: UnrecordedCall {
                at: client.at,
                call_id: client.call_id,
                reason,
            },
            notice,
        });
    }
}

/// Whether the workspace declined to record the call, which costs the call its
/// revert and nothing else. Anything else may pass once it clears, so the call
/// waits for that rather than run with no record.
fn is_refusal(error: &WorkspaceError) -> bool {
    match error {
        WorkspaceError::LimitExceeded { .. }
        | WorkspaceError::QuotaExceeded { .. }
        | WorkspaceError::UnsupportedCapability { .. }
        | WorkspaceError::UnsupportedEntry
        | WorkspaceError::Refused { .. } => true,
        WorkspaceError::Unavailable
        | WorkspaceError::CapabilityMismatch { .. }
        | WorkspaceError::StaleResource { .. }
        | WorkspaceError::StaleCursor
        | WorkspaceError::IdentityMismatch
        | WorkspaceError::PermissionDenied
        | WorkspaceError::PolicyDenied
        | WorkspaceError::Conflict
        | WorkspaceError::Busy
        | WorkspaceError::NotRepository
        | WorkspaceError::WatchUnavailable
        | WorkspaceError::TransferIntegrity
        | WorkspaceError::TransferQuota
        | WorkspaceError::PendingOperation { .. }
        | WorkspaceError::Cancelled
        | WorkspaceError::IndeterminateOutcome
        | WorkspaceError::ResponseTooLarge { .. }
        | WorkspaceError::InvalidResponse { .. }
        | WorkspaceError::Transport { .. } => false,
    }
}

/// The record `scope` asks for, when this session keeps records and the call
/// needs one. An `Err` names why the call must not run.
pub(crate) async fn begin(
    ctx: &ToolContext,
    call_id: &str,
    scope: impl FnOnce(&Path) -> Option<RecordScope>,
) -> Result<Recording, String> {
    let Some(recorder) = &ctx.changes else {
        return Ok(Recording(None));
    };
    let Some(scope) = scope(recorder.root()) else {
        return Ok(Recording(None));
    };
    recorder.begin(ctx, call_id, scope).await
}

/// The record of a call no target narrows: the whole directory, unless the
/// call cannot change a file.
pub(crate) fn whole_workspace(effect: ToolEffect) -> Option<RecordScope> {
    (!effect.is_safe_in_read_only()).then_some(RecordScope::Workspace)
}

/// Keeps `session`'s record coverage true once a recorder is built for it:
/// `store` names the store that recorder writes to, `None` when nothing
/// records. The same store keeps what it covers; another covers only what
/// follows everything the session did, not just its head, because a
/// conversation-only revert can leave a newer branch whose changes another
/// store holds, or none does.
pub fn cover_records(session: &mut StoredSession, store: Option<String>) {
    if let Some(store) = &store
        && session
            .meta
            .record_coverage
            .as_ref()
            .is_some_and(|coverage| coverage.store == *store)
    {
        return;
    }
    session.meta.record_coverage = store.map(|store| RecordCoverage {
        since: newest_happened_at(session),
        store,
    });
}

/// When the session's newest item on any branch happened, `None` for a
/// session without history. Every branch counts, since one a conversation
/// revert left behind may be newer than the head and its changes are still on
/// disk. A copy whose original is unknown counts from when it was made, which
/// is never earlier than its original.
fn newest_happened_at(session: &StoredSession) -> Option<CaudraId> {
    session
        .messages()
        .iter()
        .map(|item| item.happened_at().unwrap_or(item.id))
        .max()
}

/// A call's open record, which [`Recording::around`] finishes once the call
/// has run, whatever the outcome. Dropped before its call starts, it is
/// abandoned. Dropped after, the call may have changed files, so it is finished
/// off the caller's task.
#[must_use]
pub(crate) struct Recording(Option<Begun>);

struct Begun {
    recorder: ChangeRecorder,
    service: Arc<dyn WorkspaceChangeService>,
    ticket: RecordTicket,
    client: RecordClient,
    /// Where a record finished after its caller is gone reports a gap.
    events: EventSender,
    started: bool,
}

impl Recording {
    /// Runs the call, then finishes its record.
    pub(crate) async fn around<T>(mut self, ctx: &ToolContext, call: impl Future<Output = T>) -> T {
        if let Some(begun) = &mut self.0 {
            begun.started = true;
        }
        let output = call.await;
        if let Some(begun) = self.0.take() {
            begun.finish(&ctx.event_tx).await;
        }
        output
    }
}

impl Begun {
    async fn finish(self, events: &EventSender) {
        if let Err(error) = self.service.finish(&self.ticket).await {
            self.recorder
                .report_gap(events, self.client, error.to_string());
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        let Some(begun) = self.0.take() else {
            return;
        };
        smol::spawn(async move {
            if begun.started {
                let events = begun.events.clone();
                begun.finish(&events).await;
            } else if let Err(error) = begun.service.abandon(&begun.ticket).await {
                warn!(call_id = %begun.client.call_id, %error, "abandoning an unused change record failed");
            }
        })
        .detach();
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    use std::path::Path;
    use std::slice;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use caudra_config::SnapshotsConfig;
    use caudra_workspace::{
        CancellationResult, ChangeOperationResult, HolderPage, OpenRecord, OperationHandle,
        OperationStatus, PreparedChangeOperation, RecordHolder, RecordPage, RecordRequest,
        RecordScope, RecordSummary, RecordTicket, ReleaseResult, ReleaseSelection, ReleaseSummary,
        RevertStatus, WorkspaceChangeService, WorkspaceError,
    };
    use futures_lite::future::poll_once;

    use super::{ChangeRecorder, ChangeSource};
    use crate::tools::{LockKey, PathLocks};
    use crate::{AgentEvent, Envelope};

    pub(crate) const HOLDER: &str = "root-session";
    const TICKET: &str = "record-ticket";
    const UNUSED: WorkspaceError = WorkspaceError::Unavailable;

    /// What a recorded call went through, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Step {
        Begin(RecordScope),
        Execute,
        Finish,
        Abandon,
        /// The record was opened or finished while the guarded path was free.
        Unguarded,
    }

    /// A change service that notes every request among the steps of the
    /// calls it records, and fails each `begin` or `finish` when told to.
    pub(crate) struct FakeChanges {
        steps: Mutex<Vec<Step>>,
        requests: Mutex<Vec<RecordRequest>>,
        begin_failure: Option<WorkspaceError>,
        finish_failure: Option<WorkspaceError>,
        guarded: Option<(Arc<PathLocks>, LockKey)>,
        closed: (flume::Sender<()>, flume::Receiver<()>),
    }

    impl Default for FakeChanges {
        fn default() -> Self {
            Self {
                steps: Mutex::default(),
                requests: Mutex::default(),
                begin_failure: None,
                finish_failure: None,
                guarded: None,
                closed: flume::unbounded(),
            }
        }
    }

    impl FakeChanges {
        pub(crate) fn failing_begin(error: WorkspaceError) -> Self {
            Self {
                begin_failure: Some(error),
                ..Self::default()
            }
        }

        pub(crate) fn failing_finish(error: WorkspaceError) -> Self {
            Self {
                finish_failure: Some(error),
                ..Self::default()
            }
        }

        /// Notes each `begin` and `finish` that finds `key` unlocked.
        pub(crate) fn guarding(locks: Arc<PathLocks>, key: LockKey) -> Self {
            Self {
                guarded: Some((locks, key)),
                ..Self::default()
            }
        }

        pub(crate) fn recorder(self: &Arc<Self>, root: &Path) -> ChangeRecorder {
            let config = SnapshotsConfig {
                enabled: true,
                ..SnapshotsConfig::default()
            };
            ChangeRecorder::new(
                ChangeSource::Bound(Arc::clone(self) as Arc<dyn WorkspaceChangeService>),
                holder(),
                root.to_owned(),
                &config,
            )
            .expect("an enabled config keeps a recorder")
        }

        pub(crate) fn push(&self, step: Step) {
            self.steps.lock().unwrap().push(step);
        }

        pub(crate) fn steps(&self) -> Vec<Step> {
            self.steps.lock().unwrap().clone()
        }

        pub(crate) fn requests(&self) -> Vec<RecordRequest> {
            self.requests.lock().unwrap().clone()
        }

        /// Whether a record was finished or abandoned within `wait`, which a
        /// dropped recording does off the caller's task.
        pub(crate) fn closed_within(&self, wait: Duration) -> bool {
            self.closed.1.recv_timeout(wait).is_ok()
        }

        async fn check_guard(&self) {
            if let Some((locks, key)) = &self.guarded
                && poll_once(locks.acquire(slice::from_ref(key), &[]))
                    .await
                    .is_some()
            {
                self.push(Step::Unguarded);
            }
        }
    }

    pub(crate) fn holder() -> RecordHolder {
        RecordHolder::new(HOLDER).unwrap()
    }

    /// Each gap reported, as its call, reason, and whether it was told.
    pub(crate) fn gaps(events: &flume::Receiver<Envelope>) -> Vec<(String, String, bool)> {
        events
            .try_iter()
            .filter_map(|envelope| match envelope.event {
                AgentEvent::Unrecorded { gap, notice } => Some((gap.call_id, gap.reason, notice)),
                _ => None,
            })
            .collect()
    }

    #[async_trait]
    impl WorkspaceChangeService for FakeChanges {
        async fn begin(&self, request: &RecordRequest) -> Result<RecordTicket, WorkspaceError> {
            self.push(Step::Begin(request.scope.clone()));
            self.check_guard().await;
            self.requests.lock().unwrap().push(request.clone());
            match &self.begin_failure {
                Some(error) => Err(error.clone()),
                None => Ok(RecordTicket::new(TICKET).unwrap()),
            }
        }

        async fn finish(
            &self,
            _ticket: &RecordTicket,
        ) -> Result<Option<RecordSummary>, WorkspaceError> {
            self.push(Step::Finish);
            self.check_guard().await;
            self.closed.0.send(()).unwrap();
            self.finish_failure.clone().map_or(Ok(None), Err)
        }

        async fn abandon(&self, _ticket: &RecordTicket) -> Result<bool, WorkspaceError> {
            self.push(Step::Abandon);
            self.closed.0.send(()).unwrap();
            Ok(true)
        }

        async fn open_records(
            &self,
            _holder: &RecordHolder,
        ) -> Result<Vec<OpenRecord>, WorkspaceError> {
            Err(UNUSED)
        }

        async fn abandon_open_records(
            &self,
            _holder: &RecordHolder,
        ) -> Result<u32, WorkspaceError> {
            Err(UNUSED)
        }

        async fn records(
            &self,
            _holder: &RecordHolder,
            _after_seq: Option<u64>,
            _page_size: u32,
        ) -> Result<RecordPage, WorkspaceError> {
            Err(UNUSED)
        }

        async fn holders(
            &self,
            _after: Option<&RecordHolder>,
            _page_size: u32,
        ) -> Result<HolderPage, WorkspaceError> {
            Err(UNUSED)
        }

        async fn hold(
            &self,
            _from: &RecordHolder,
            _to: &RecordHolder,
        ) -> Result<u32, WorkspaceError> {
            Err(UNUSED)
        }

        async fn release(
            &self,
            _holder: &RecordHolder,
            _selection: &ReleaseSelection,
        ) -> Result<ReleaseSummary, WorkspaceError> {
            Err(UNUSED)
        }

        async fn prepare_revert(
            &self,
            _holder: &RecordHolder,
            _seqs: &[u64],
        ) -> Result<PreparedChangeOperation, WorkspaceError> {
            Err(UNUSED)
        }

        async fn prepare_unrevert(
            &self,
            _holder: &RecordHolder,
        ) -> Result<PreparedChangeOperation, WorkspaceError> {
            Err(UNUSED)
        }

        async fn acknowledge(
            &self,
            _holder: &RecordHolder,
        ) -> Result<RevertStatus, WorkspaceError> {
            Err(UNUSED)
        }

        async fn status(&self, _holder: &RecordHolder) -> Result<RevertStatus, WorkspaceError> {
            Err(UNUSED)
        }

        async fn prepare_cleanup(
            &self,
            _retention_bytes: u64,
        ) -> Result<PreparedChangeOperation, WorkspaceError> {
            Err(UNUSED)
        }

        async fn execute(
            &self,
            _prepared: &PreparedChangeOperation,
        ) -> Result<OperationStatus<ChangeOperationResult>, WorkspaceError> {
            Err(UNUSED)
        }

        async fn operation_status(
            &self,
            _operation: &OperationHandle,
        ) -> Result<OperationStatus<ChangeOperationResult>, WorkspaceError> {
            Err(UNUSED)
        }

        async fn cancel(
            &self,
            _operation: &OperationHandle,
        ) -> Result<CancellationResult, WorkspaceError> {
            Err(UNUSED)
        }

        async fn release_prepared(
            &self,
            _prepared: &PreparedChangeOperation,
        ) -> Result<ReleaseResult, WorkspaceError> {
            Err(UNUSED)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use caudra_config::SnapshotsConfig;
    use caudra_storage::id::CaudraId;
    use caudra_workspace::{
        RecordLimits, RecordScope, TransportErrorKind, WorkspaceCapability, WorkspaceError,
    };
    use futures_lite::future::{self, poll_once};
    use test_case::test_case;

    use super::fixture::{FakeChanges, Step, gaps, holder};
    use super::{
        ChangeRecorder, ChangeSource, NO_CHANGE_RECORDS, RECORD_BLOCKED, RecordClient, begin,
    };
    use crate::tools::ToolContext;
    use crate::tools::test_support::stub_ctx_with;
    use crate::{AgentEvent, AgentMode, Envelope, EventSender};

    const ROOT: &str = "/work/project";
    const CALL: &str = "call";
    const ROOT_CALL: &str = "root-call";
    const MAX_FILES: u32 = 3;
    const MAX_FILE_BYTES: u64 = 5;
    const MAX_BYTES: u64 = 7;
    const CLOSE_WAIT: Duration = Duration::from_secs(10);
    const RUNS_UNRECORDED: &str = "a refused record must let the call run";
    const BLOCKS: &str = "a record that may still open must hold the call back";
    const TOLD_ONCE: &str = "only a recorder's first gap is worth telling the user about";
    const ABANDONS: &str = "a record whose call never started must be abandoned";
    const FINISHES: &str = "a record whose call started may hold changes and must be finished";
    const AFTER_THE_MESSAGE: &str = "a call made after a message must be recorded after it";

    fn recording_ctx(recorder: Option<ChangeRecorder>) -> (ToolContext, flume::Receiver<Envelope>) {
        let (tx, events) = flume::unbounded();
        let mut ctx = stub_ctx_with(&AgentMode::Build, Some(&EventSender::new(tx, 0)), None);
        ctx.changes = recorder;
        (ctx, events)
    }

    fn whole_workspace(_root: &Path) -> Option<RecordScope> {
        Some(RecordScope::Workspace)
    }

    #[test_case(WorkspaceError::LimitExceeded { limit: None, maximum: None } ; "a_ceiling")]
    #[test_case(WorkspaceError::QuotaExceeded { limit: None, maximum: None } ; "a_full_store")]
    #[test_case(WorkspaceError::UnsupportedCapability { capability: WorkspaceCapability::ChangeRecords } ; "a_host_without_the_contract")]
    #[test_case(WorkspaceError::UnsupportedEntry ; "an_unsupported_entry")]
    #[test_case(WorkspaceError::Refused { code: 1, symbolic: "declined".into() } ; "an_unmapped_refusal")]
    fn a_refused_record_lets_the_call_run_and_reports_a_gap(error: WorkspaceError) {
        smol::block_on(async {
            let changes = Arc::new(FakeChanges::failing_begin(error.clone()));
            let (ctx, events) = recording_ctx(Some(changes.recorder(Path::new(ROOT))));

            let recording = begin(&ctx, CALL, whole_workspace)
                .await
                .expect(RUNS_UNRECORDED);
            recording.around(&ctx, async {}).await;

            assert_eq!(changes.steps(), [Step::Begin(RecordScope::Workspace)]);
            assert_eq!(gaps(&events), [(CALL.to_owned(), error.to_string(), true)]);
        });
    }

    #[test_case(WorkspaceError::Busy ; "a_busy_host")]
    #[test_case(WorkspaceError::Transport { kind: TransportErrorKind::Disconnected } ; "a_lost_connection")]
    #[test_case(WorkspaceError::PolicyDenied ; "a_policy_denial")]
    #[test_case(WorkspaceError::PermissionDenied ; "a_permission_denial")]
    #[test_case(WorkspaceError::Unavailable ; "an_unavailable_host")]
    #[test_case(WorkspaceError::IndeterminateOutcome ; "an_indeterminate_outcome")]
    fn a_record_that_may_still_open_blocks_the_call(error: WorkspaceError) {
        smol::block_on(async {
            let changes = Arc::new(FakeChanges::failing_begin(error.clone()));
            let (ctx, events) = recording_ctx(Some(changes.recorder(Path::new(ROOT))));

            let Err(message) = begin(&ctx, CALL, whole_workspace).await else {
                panic!("{BLOCKS}");
            };

            assert_eq!(message, format!("{RECORD_BLOCKED}: {error}"));
            assert!(gaps(&events).is_empty());
        });
    }

    #[test]
    fn only_the_first_gap_is_told() {
        smol::block_on(async {
            let changes = Arc::new(FakeChanges::failing_begin(WorkspaceError::UnsupportedEntry));
            let (ctx, events) = recording_ctx(Some(changes.recorder(Path::new(ROOT))));

            for _ in 0..2 {
                let recording = begin(&ctx, CALL, whole_workspace)
                    .await
                    .expect(RUNS_UNRECORDED);
                recording.around(&ctx, async {}).await;
            }

            let told: Vec<bool> = gaps(&events)
                .into_iter()
                .map(|(_, _, notice)| notice)
                .collect();
            assert_eq!(told, [true, false], "{TOLD_ONCE}");
        });
    }

    #[test]
    fn a_finish_that_fails_after_the_call_ran_reports_a_gap() {
        smol::block_on(async {
            let error = WorkspaceError::Transport {
                kind: TransportErrorKind::Timeout,
            };
            let changes = Arc::new(FakeChanges::failing_finish(error.clone()));
            let (ctx, events) = recording_ctx(Some(changes.recorder(Path::new(ROOT))));

            let recording = begin(&ctx, CALL, whole_workspace).await.unwrap();
            recording.around(&ctx, async {}).await;

            assert_eq!(
                changes.steps(),
                [Step::Begin(RecordScope::Workspace), Step::Finish]
            );
            assert_eq!(gaps(&events), [(CALL.to_owned(), error.to_string(), true)]);
        });
    }

    #[test]
    fn a_record_dropped_before_its_call_starts_is_abandoned() {
        smol::block_on(async {
            let changes = Arc::new(FakeChanges::default());
            let (ctx, events) = recording_ctx(Some(changes.recorder(Path::new(ROOT))));

            drop(begin(&ctx, CALL, whole_workspace).await.unwrap());

            assert!(changes.closed_within(CLOSE_WAIT), "{ABANDONS}");
            assert_eq!(
                changes.steps(),
                [Step::Begin(RecordScope::Workspace), Step::Abandon]
            );
            assert!(gaps(&events).is_empty());
        });
    }

    #[test_case(None ; "and_kept")]
    #[test_case(Some(WorkspaceError::Transport { kind: TransportErrorKind::Timeout }) ; "or_reported_as_a_gap")]
    fn a_record_whose_call_is_dropped_while_running_is_finished(failure: Option<WorkspaceError>) {
        smol::block_on(async {
            let changes = Arc::new(
                failure
                    .clone()
                    .map_or_else(FakeChanges::default, FakeChanges::failing_finish),
            );
            let (ctx, events) = recording_ctx(Some(changes.recorder(Path::new(ROOT))));
            let recording = begin(&ctx, CALL, whole_workspace).await.unwrap();

            let mut running = Box::pin(recording.around(&ctx, future::pending::<()>()));
            assert!(poll_once(&mut running).await.is_none());
            drop(running);

            assert!(changes.closed_within(CLOSE_WAIT), "{FINISHES}");
            assert_eq!(
                changes.steps(),
                [Step::Begin(RecordScope::Workspace), Step::Finish]
            );
            let expected: Vec<_> = failure
                .iter()
                .map(|error| (CALL.to_owned(), error.to_string(), true))
                .collect();
            assert_eq!(gaps_within(&events, expected.len()), expected);
        });
    }

    /// The first `count` gaps, waiting for each, since a dropped recording
    /// reports off the caller's task.
    fn gaps_within(
        events: &flume::Receiver<Envelope>,
        count: usize,
    ) -> Vec<(String, String, bool)> {
        let mut told = Vec::new();
        while told.len() < count {
            let Ok(envelope) = events.recv_timeout(CLOSE_WAIT) else {
                break;
            };
            if let AgentEvent::Unrecorded { gap, notice } = envelope.event {
                told.push((gap.call_id, gap.reason, notice));
            }
        }
        told.extend(gaps(events));
        told
    }

    #[test_case(Some(ROOT_CALL), ROOT_CALL ; "under_a_root_call")]
    #[test_case(None, CALL ; "as_the_root_call")]
    fn a_record_carries_its_call_holder_limits_and_time(
        root_call: Option<&str>,
        expected_root: &str,
    ) {
        smol::block_on(async {
            let changes = Arc::new(FakeChanges::default());
            let config = SnapshotsConfig {
                enabled: true,
                max_bytes: MAX_BYTES,
                max_files: MAX_FILES.into(),
                max_file_bytes: MAX_FILE_BYTES,
            };
            let recorder = ChangeRecorder::new(
                ChangeSource::Bound(changes.clone()),
                holder(),
                ROOT.into(),
                &config,
            );
            let (mut ctx, _events) = recording_ctx(recorder);
            ctx.local_root_tool_use_id = root_call.map(str::to_owned);
            let message = CaudraId::generate();

            let recording = begin(&ctx, CALL, whole_workspace).await.unwrap();
            recording.around(&ctx, async {}).await;

            let [request] = changes.requests().try_into().unwrap();
            let client: RecordClient = serde_json::from_value(request.client).unwrap();
            assert_eq!(request.holder, holder());
            assert_eq!(
                request.limits,
                RecordLimits {
                    max_files: MAX_FILES,
                    max_file_bytes: MAX_FILE_BYTES,
                    max_total_bytes: MAX_BYTES,
                }
            );
            assert_eq!(
                (client.call_id.as_str(), client.root_call_id.as_str()),
                (CALL, expected_root)
            );
            assert!(
                client.at.as_bytes() > message.as_bytes(),
                "{AFTER_THE_MESSAGE}"
            );
        });
    }

    #[test]
    fn a_session_without_change_records_runs_its_calls_unrecorded() {
        smol::block_on(async {
            let config = SnapshotsConfig {
                enabled: true,
                ..SnapshotsConfig::default()
            };
            let recorder =
                ChangeRecorder::new(ChangeSource::Remote, holder(), ROOT.into(), &config);
            let (ctx, events) = recording_ctx(recorder);

            let recording = begin(&ctx, CALL, whole_workspace)
                .await
                .expect(RUNS_UNRECORDED);
            recording.around(&ctx, async {}).await;

            assert_eq!(
                gaps(&events),
                [(CALL.to_owned(), NO_CHANGE_RECORDS.to_owned(), true)]
            );
        });
    }

    #[test]
    fn switching_snapshots_off_keeps_no_recorder() {
        let config = SnapshotsConfig {
            enabled: false,
            ..SnapshotsConfig::default()
        };

        assert!(
            ChangeRecorder::new(ChangeSource::Remote, holder(), ROOT.into(), &config).is_none()
        );
    }
}
