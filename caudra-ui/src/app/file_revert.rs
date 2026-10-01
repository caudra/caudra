//! A file revert undoes this session's recorded changes made after a message,
//! newest first, and nothing else. This module decides which records those
//! are, keeps the message menu's view of them, runs the revert in two steps,
//! and settles the session's pending revert against the store at load.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use arc_swap::ArcSwapOption;
use caudra_agent::agent::change_recording::{
    ChangeRecorder, ChangeSource, RecordClient, cover_records,
};
use caudra_config::SnapshotsConfig;
use caudra_providers::HistoryItem;
use caudra_storage::id::CaudraId;
use caudra_storage::projects::workspace_key;
use caudra_storage::sessions::{
    MAX_UNRECORDED_CALLS, PendingConversationRevert, SessionMeta, UnrecordedCall,
};
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_workspace::{
    ChangeOperationPreview, ChangeOperationResult, OperationState, PendingRevert,
    PreparedChangeOperation, RecordHolder, RecordState, RevertConflict, RevertConflictKind,
    RevertDirection, RevertId, RevertPreview, RevertState, RevertStatus, UNREVEALED_ROOT,
    WorkspaceChangeService, WorkspaceSession,
};
use futures_lite::future;
use serde_json::Value;
use tracing::warn;

use super::App;
use super::session::{RevertTarget, resolve_revert_target, source_target_id};
use crate::components::{DisplaySource, RestoreMode};
use crate::repaint::Dirty;
use crate::{AppSession, ChangeServiceFactory};

pub(crate) const RECORDING_OFF: &str = "File revert is off because change recording is disabled";
pub(crate) const NO_CHANGE_RECORDS: &str =
    "File revert is unavailable: this workspace keeps no change records";
pub(crate) const UNKNOWN_BOUNDARY: &str =
    "Compaction rewrote this message, so it cannot be matched to file changes";
pub(crate) const RECORDS_EVICTED: &str =
    "Retention dropped file changes made after this message, so they cannot be reverted";
pub(crate) const RECORDS_UNREACHABLE: &str = "File revert cannot reach this message: earlier \
    file changes were recorded elsewhere or not at all";
pub(crate) const NO_RECORDED_CHANGES: &str = "No recorded file changes after this message";
pub(crate) const UNREADABLE_RECORD: &str = "a change record could not be read";
pub(crate) const OPEN_RECORDS: &str =
    "Tool calls of this session are still changing files; revert once they finish";
pub(crate) const UNSETTLED_REVERT: &str = "An earlier file revert did not finish: Unrevert it, \
    or keep working to keep the files as they are";
pub(crate) const INTERRUPTED_RECORD: &str = "interrupted before its change record finished";
pub(crate) const REVERT_ADOPTED: &str = "A file revert finished after Caudra stopped. The \
    conversation was left as it was; Unrevert puts the files back";
pub(crate) const LEGACY_RESTORE_CLEARED: &str = "A file revert saved by an older version was \
    cleared and can no longer be unreverted; if Caudra stopped during it, files may be partly \
    restored";
pub(crate) const REPEAT_TO_CONFIRM: &str = "Repeat to confirm";
pub(crate) const REVERT_CONFLICTS: &str =
    "Files changed since they were recorded, so nothing was reverted";
pub(crate) const UNREVERT_CONFLICTS: &str =
    "Files changed since the revert, so nothing was unreverted";
pub(crate) const REVERT_FAILED: &str = "File revert failed";
pub(crate) const UNREVERT_FAILED: &str = "File unrevert failed";
pub(crate) const SETTLE_FAILED: &str = "Failed to settle reverted files";
const FILE_REVERT_UNAVAILABLE: &str = "File revert is unavailable";
const NOT_A_REVERT: &str = "the store answered with something other than a revert";
const PREPARED_EXPIRED: &str = "the prepared revert is no longer known";
const INTERRUPTED: &str = "interrupted";
const STILL_RUNNING: &str = "still running";
const UNCONFIRMED: &str = "unconfirmed";
const PARTIAL: &str = "partial";
const INDETERMINATE: &str = "indeterminate";
const LISTED_CONFLICTS: usize = 5;

/// The holders this process has recorded for. A record of theirs left open
/// may belong to a call still running here, so only the others' are
/// abandoned.
static RECORDING_HOLDERS: LazyLock<Mutex<HashSet<RecordHolder>>> = LazyLock::new(Mutex::default);

/// The recorder every agent of a session reads when its run starts, swapped
/// whenever the session or its workspace changes.
pub(crate) type RecorderSlot = Arc<ArcSwapOption<ChangeRecorder>>;

/// When one of the holder's records was made, which places it before or
/// after a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordMark {
    pub(crate) seq: u64,
    pub(crate) at: CaudraId,
    pub(crate) state: RecordState,
}

/// The holder's records, as the store lists them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RecordIndex {
    pub(crate) marks: Vec<RecordMark>,
    /// When the newest record retention evicted was made.
    pub(crate) evicted_through: Option<CaudraId>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) seqs: Vec<u64>,
    pub(crate) gaps: Gaps,
}

/// The calls after the boundary that ran without a record, whose changes a
/// revert leaves where they are.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Gaps {
    pub(crate) count: usize,
    /// The list dropped older gaps, which may have come after the boundary.
    pub(crate) at_least: bool,
}

/// A file revert previewed and waiting for the same action to confirm it.
pub(super) struct RevertConfirmation {
    conversation_source: Option<CaudraId>,
    target_head: Option<CaudraId>,
    mode: RestoreMode,
    service: Arc<dyn WorkspaceChangeService>,
    prepared: PreparedChangeOperation,
}

/// What the file half of a revert did.
pub(super) enum FileRevert {
    /// It cannot run, for the reason given; the conversation half may.
    Blocked(String),
    /// Nothing moved and nothing else should: a preview, a conflict, or a
    /// failure, each already flashed.
    Held,
    /// The files moved, and `pending_revert` says so.
    Reverted,
}

enum Settled {
    Done,
    /// It may have changed files and did not finish; the text says how far.
    Unsettled(String),
    /// It changed nothing, for the reason given.
    Unchanged(String),
}

/// What one session records through: the local store's handle, which a
/// remote session binds per use instead, and the recorder its calls share.
pub(crate) struct SessionChanges {
    pub(crate) local: Option<Arc<dyn WorkspaceChangeService>>,
    pub(crate) recorder: Option<ChangeRecorder>,
    /// No call of this process could have recorded for the session yet, so a
    /// record left open belongs to a process that stopped.
    pub(crate) first: bool,
}

#[derive(Default)]
pub(crate) struct Reconciled {
    pub(crate) changed: bool,
    pub(crate) notices: Vec<&'static str>,
}

/// The applied records made after `boundary`, or every applied record when
/// there is none, plus the gaps a revert from there leaves in place. Refused
/// when the session's records do not all reach back that far.
pub(crate) fn select(
    index: &RecordIndex,
    boundary: Option<&HistoryItem>,
    meta: &SessionMeta,
) -> Result<Selection, &'static str> {
    let after = boundary
        .map(|item| item.happened_at().ok_or(UNKNOWN_BOUNDARY))
        .transpose()?;
    let reached = meta.record_coverage.as_ref().is_some_and(|coverage| {
        coverage
            .since
            .is_none_or(|since| after.is_some_and(|after| since <= after))
    });
    if !reached {
        return Err(RECORDS_UNREACHABLE);
    }
    let unrecorded = &meta.unrecorded;
    let is_after = |at: CaudraId| after.is_none_or(|after| at > after);
    if index.evicted_through.is_some_and(is_after) {
        return Err(RECORDS_EVICTED);
    }
    let seqs: Vec<u64> = index
        .marks
        .iter()
        .filter(|mark| mark.state == RecordState::Applied && is_after(mark.at))
        .map(|mark| mark.seq)
        .collect();
    if seqs.is_empty() {
        return Err(NO_RECORDED_CHANGES);
    }
    let gaps = Gaps {
        count: unrecorded.iter().filter(|gap| is_after(gap.at)).count(),
        at_least: unrecorded.len() >= MAX_UNRECORDED_CALLS
            && unrecorded
                .iter()
                .map(|gap| gap.at)
                .min()
                .is_some_and(is_after),
    };
    Ok(Selection { seqs, gaps })
}

/// The item a file revert at `target` is measured from.
pub(super) fn boundary_item<'a>(
    items: &'a [HistoryItem],
    target: &RevertTarget,
) -> Result<Option<&'a HistoryItem>, &'static str> {
    target
        .boundary
        .map(|id| {
            items
                .iter()
                .find(|item| item.id == id)
                .ok_or(UNKNOWN_BOUNDARY)
        })
        .transpose()
}

pub(crate) async fn fetch_index(
    service: &dyn WorkspaceChangeService,
    holder: &RecordHolder,
) -> Result<RecordIndex, String> {
    let mut index = RecordIndex::default();
    let mut after_seq = None;
    loop {
        let page = service
            .records(holder, after_seq, u32::MAX)
            .await
            .map_err(|error| format!("{FILE_REVERT_UNAVAILABLE}: {error}"))?;
        if let Some(evicted) = page.evicted_through {
            index.evicted_through = index.evicted_through.max(Some(made_at(evicted)?));
        }
        for record in page.records {
            index.marks.push(RecordMark {
                seq: record.seq,
                at: made_at(record.client)?,
                state: record.state,
            });
        }
        match page.next_after_seq {
            Some(next) => after_seq = Some(next),
            None => return Ok(index),
        }
    }
}

fn made_at(client: Value) -> Result<CaudraId, String> {
    serde_json::from_value::<RecordClient>(client)
        .map(|client| client.at)
        .map_err(|error| format!("{FILE_REVERT_UNAVAILABLE}: {UNREADABLE_RECORD}: {error}"))
}

pub(crate) fn holder_of(session_id: CaudraId) -> Option<RecordHolder> {
    RecordHolder::new(session_id.to_string())
        .inspect_err(|error| warn!(%session_id, %error, "session id is not a record holder"))
        .ok()
}

/// Whether this is the first time this process records for `holder`.
fn claim(holder: &RecordHolder) -> bool {
    RECORDING_HOLDERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(holder.clone())
}

/// Builds what `session` records through, and keeps its record coverage
/// true to it. A local session's records live in its directory's store; a
/// remote one's on the host, bound per call.
pub(crate) fn session_changes(
    session: &mut AppSession,
    workspace: Option<&WorkspaceSession>,
    factory: Option<&ChangeServiceFactory>,
    config: &SnapshotsConfig,
) -> SessionChanges {
    let holder = holder_of(session.id);
    let cwd = PathBuf::from(&session.cwd);
    let local = holder
        .as_ref()
        .and(factory)
        .filter(|_| workspace.is_none())
        .and_then(|factory| factory(cwd.as_path()));
    let (source, store, root) = match (&local, workspace) {
        (Some(service), _) => (
            Some(ChangeSource::Bound(Arc::clone(service))),
            workspace_key(&cwd).ok(),
            cwd,
        ),
        (None, Some(_)) => (
            Some(ChangeSource::Remote),
            session
                .workspace_binding()
                .map(StoredWorkspaceBinding::change_store_key),
            PathBuf::from(UNREVEALED_ROOT),
        ),
        (None, None) => (None, None, cwd),
    };
    let recorder = holder
        .clone()
        .zip(source)
        .and_then(|(holder, source)| ChangeRecorder::new(source, holder, root, config));
    cover_records(session, recorder.as_ref().and(store));
    SessionChanges {
        recorder,
        local,
        first: holder.as_ref().is_some_and(claim),
    }
}

pub(crate) fn service_for(
    local: Option<&Arc<dyn WorkspaceChangeService>>,
    workspace: Option<&WorkspaceSession>,
) -> Option<Arc<dyn WorkspaceChangeService>> {
    match workspace {
        Some(workspace) => workspace.changes(),
        None => local.cloned(),
    }
}

/// Reconciles a session the moment its records can be reached. A session
/// with nothing to reconcile is skipped, so a fresh one never opens a store.
pub(crate) fn reconcile_session(
    session: &mut AppSession,
    service: Option<Arc<dyn WorkspaceChangeService>>,
    abandon: bool,
    config: &SnapshotsConfig,
) -> Reconciled {
    let files_pending = session
        .meta
        .pending_revert
        .as_ref()
        .is_some_and(|pending| pending.file_status.is_some());
    if !files_pending && (!config.enabled || session.messages().is_empty()) {
        return Reconciled::default();
    }
    let (Some(service), Some(holder)) = (service, holder_of(session.id)) else {
        return Reconciled::default();
    };
    smol::block_on(reconcile_pending_revert(
        session,
        service.as_ref(),
        &holder,
        abandon,
    ))
}

/// Settles what a stopped process left behind: records it never finished
/// become gaps, and the session's pending revert is brought in line with the
/// store's, which is the truth for the files. Service errors are logged and
/// never fail the load.
pub(crate) async fn reconcile_pending_revert(
    session: &mut AppSession,
    service: &dyn WorkspaceChangeService,
    holder: &RecordHolder,
    abandon: bool,
) -> Reconciled {
    let mut reconciled = Reconciled {
        changed: abandon && gaps_from_open_records(session, service, holder).await,
        notices: Vec::new(),
    };
    let head = crate::session_history_head(session);
    let mut pending = session.meta.pending_revert.clone();
    if let Some(stale) = pending.take_if(|pending| {
        pending
            .file_status
            .as_ref()
            .is_some_and(|status| parse_status(status).is_none())
    }) {
        pending = without_files(stale, head);
        reconciled.notices.push(LEGACY_RESTORE_CLEARED);
    }
    match service.status(holder).await {
        Ok(status) => {
            let file_status = serde_json::to_value(&status).ok();
            pending = match pending {
                None if !status.pending.is_empty() => {
                    reconciled.notices.push(REVERT_ADOPTED);
                    Some(PendingConversationRevert {
                        original_head: head,
                        target_head: head,
                        file_status,
                    })
                }
                Some(mut pending) if !status.pending.is_empty() => {
                    pending.file_status = file_status;
                    Some(pending)
                }
                Some(pending) if pending.file_status.is_some() => without_files(pending, head),
                unchanged => unchanged,
            };
        }
        Err(error) => warn!(%error, holder = holder.as_str(), "change records unavailable at load"),
    }
    if pending != session.meta.pending_revert {
        session.set_conversation_state(head, pending);
        reconciled.changed = true;
    }
    reconciled
}

async fn gaps_from_open_records(
    session: &mut AppSession,
    service: &dyn WorkspaceChangeService,
    holder: &RecordHolder,
) -> bool {
    let open = match service.open_records(holder).await {
        Ok(open) => open,
        Err(error) => {
            warn!(%error, "open change records unavailable at load");
            return false;
        }
    };
    if open.is_empty() {
        return false;
    }
    for record in open {
        match serde_json::from_value::<RecordClient>(record.client) {
            Ok(client) => session.meta.push_unrecorded(UnrecordedCall {
                at: client.at,
                call_id: client.call_id,
                reason: INTERRUPTED_RECORD.to_owned(),
            }),
            Err(error) => warn!(%error, "{UNREADABLE_RECORD}"),
        }
    }
    if let Err(error) = service.abandon_open_records(holder).await {
        warn!(%error, "failed to abandon open change records");
    }
    true
}

/// `pending` once the store holds no revert for it. A revert that only moved
/// files has nothing left to undo.
fn without_files(
    mut pending: PendingConversationRevert,
    head: Option<CaudraId>,
) -> Option<PendingConversationRevert> {
    pending.file_status = None;
    (pending.original_head != head).then_some(pending)
}

fn parse_status(value: &Value) -> Option<RevertStatus> {
    serde_json::from_value(value.clone()).ok()
}

/// Whether the session's copy of the store's status holds a pending revert.
pub(super) fn files_pending(pending: &PendingConversationRevert) -> bool {
    pending
        .file_status
        .as_ref()
        .and_then(parse_status)
        .is_some_and(|status| !status.pending.is_empty())
}

fn is_unsettled(pending: &PendingRevert) -> bool {
    pending.state != RevertState::Completed || pending.reconciliation_required
}

fn revert_preview(prepared: &PreparedChangeOperation) -> Option<&RevertPreview> {
    match &prepared.preview {
        ChangeOperationPreview::Revert(preview) => Some(preview),
        ChangeOperationPreview::Cleanup(_) => None,
    }
}

fn settled(
    state: &OperationState<ChangeOperationResult>,
    revert_id: &RevertId,
    direction: RevertDirection,
) -> Settled {
    let unsettled_if = |side_effects_possible: bool, reason: &str| {
        if side_effects_possible {
            Settled::Unsettled(reason.to_owned())
        } else {
            Settled::Unchanged(reason.to_owned())
        }
    };
    match state {
        OperationState::Completed {
            result: ChangeOperationResult::Revert(status),
            ..
        } => match status
            .pending
            .iter()
            .find(|pending| &pending.revert_id == revert_id)
        {
            Some(pending) if !is_unsettled(pending) => Settled::Done,
            Some(pending) => Settled::Unsettled(describe(pending)),
            None if direction == RevertDirection::Unrevert => Settled::Done,
            None => Settled::Unsettled(UNCONFIRMED.to_owned()),
        },
        OperationState::Completed { .. } => Settled::Unchanged(NOT_A_REVERT.to_owned()),
        OperationState::Prepared | OperationState::Running => {
            Settled::Unsettled(STILL_RUNNING.to_owned())
        }
        OperationState::Failed {
            error,
            side_effects_possible,
        } => unsettled_if(*side_effects_possible, &error.message),
        OperationState::Cancelled {
            side_effects_possible,
        }
        | OperationState::Indeterminate {
            side_effects_possible,
        } => unsettled_if(*side_effects_possible, INTERRUPTED),
        OperationState::NeverSeen | OperationState::Forgotten => {
            Settled::Unchanged(PREPARED_EXPIRED.to_owned())
        }
    }
}

fn describe(pending: &PendingRevert) -> String {
    let state = match pending.state {
        RevertState::Publishing => STILL_RUNNING,
        RevertState::Completed => UNCONFIRMED,
        RevertState::Partial => PARTIAL,
        RevertState::Indeterminate => INDETERMINATE,
    };
    match &pending.stopped_at {
        Some(path) => format!("{state}, stopped at {path}"),
        None => state.to_owned(),
    }
}

/// Every conflict as "kind path", the long tail counted.
pub(crate) fn list_conflicts(conflicts: &[RevertConflict]) -> String {
    let mut listed: Vec<String> = conflicts
        .iter()
        .take(LISTED_CONFLICTS)
        .map(|conflict| {
            let kind = match conflict.kind {
                RevertConflictKind::ChangedSince => "changed",
                RevertConflictKind::Interleaved => "interleaved",
                RevertConflictKind::Unrecorded => "unrecorded",
            };
            format!("{kind} {}", conflict.path)
        })
        .collect();
    if let Some(more) = conflicts
        .len()
        .checked_sub(LISTED_CONFLICTS)
        .filter(|more| *more > 0)
    {
        listed.push(format!("+{more} more"));
    }
    listed.join(", ")
}

fn describe_preview(preview: &RevertPreview, gaps: &Gaps) -> String {
    let counts = &preview.counts;
    let gaps = match (gaps.count, gaps.at_least) {
        (0, _) => String::new(),
        (count, true) => {
            format!("; at least {count} calls after it ran unrecorded and keep their changes")
        }
        (count, false) => {
            format!("; {count} calls after it ran unrecorded and keep their changes")
        }
    };
    format!(
        "File revert: {} created, {} replaced, {} deleted{gaps}. {REPEAT_TO_CONFIRM}",
        counts.create, counts.replace, counts.delete
    )
}

fn release(service: &dyn WorkspaceChangeService, prepared: &PreparedChangeOperation) {
    if let Err(error) = smol::block_on(service.release_prepared(prepared)) {
        warn!(%error, "failed to release a prepared file revert");
    }
}

impl App {
    /// The change records of the session's workspace, when it keeps any.
    pub(super) fn change_service(&self) -> Option<Arc<dyn WorkspaceChangeService>> {
        service_for(self.local_changes.as_ref(), self.workspace_session.as_ref())
    }

    /// Points recording at the current session and its workspace: load, a
    /// new session, and every directory or workspace change come here.
    /// Answers whether this process records for the session for the first
    /// time.
    pub(crate) fn bind_change_recorder(&mut self) -> bool {
        let changes = session_changes(
            self.state.session_mut(),
            self.workspace_session.as_ref(),
            self.change_factory.as_ref(),
            &self.snapshots_config,
        );
        self.local_changes = changes.local;
        self.change_recorder.store(changes.recorder.map(Arc::new));
        self.record_index = None;
        self.record_index_refresh = None;
        changes.first
    }

    /// Reconciles the loaded session with its records and saves what moved,
    /// including record coverage its binding changed when `covered_anew`.
    pub(crate) fn reconcile_loaded_session(&mut self, abandon: bool, covered_anew: bool) {
        let service = self.change_service();
        let config = self.snapshots_config;
        let reconciled = reconcile_session(self.state.session_mut(), service, abandon, &config);
        if (reconciled.changed || covered_anew)
            && let Err(error) = self.save_session_barrier()
        {
            self.status_bar
                .flash(format!("Failed to save reconciled file reverts: {error}"));
        }
        self.state
            .warnings
            .extend(reconciled.notices.into_iter().map(str::to_owned));
    }

    /// Re-reads the session's records for the message menu, off the UI
    /// thread. A newer read replaces one still running.
    pub(crate) fn refresh_record_index(&mut self) {
        self.record_index_refresh = None;
        if !self.snapshots_config.enabled {
            self.record_index = Some(Err(RECORDING_OFF.to_owned()));
            return;
        }
        if self.state.session.messages().is_empty() {
            self.record_index = Some(Ok(RecordIndex::default()));
            return;
        }
        let (Some(service), Some(holder)) =
            (self.change_service(), holder_of(self.state.session.id))
        else {
            self.record_index = Some(Err(NO_CHANGE_RECORDS.to_owned()));
            return;
        };
        self.record_index_refresh = Some(smol::spawn(async move {
            fetch_index(service.as_ref(), &holder).await
        }));
    }

    pub(super) fn poll_record_index(&mut self) -> Dirty {
        let Some(refresh) = self.record_index_refresh.as_mut() else {
            return Dirty::NO;
        };
        if let Some(index) = future::block_on(future::poll_once(refresh)) {
            self.record_index_refresh = None;
            self.record_index = Some(index);
        }
        Dirty::NO
    }

    /// Why the last read of the records leaves no file revert anywhere.
    pub(crate) fn record_index_blocker(&self) -> Option<String> {
        self.record_index.as_ref()?.as_ref().err().cloned()
    }

    pub(super) fn open_message_actions(&mut self, source: DisplaySource) {
        let withheld = self.file_revert_withheld(source);
        self.message_actions.open(
            source,
            self.state.session.meta.pending_revert.is_some(),
            withheld.as_deref(),
        );
    }

    /// Why the message menu leaves the file reverts at `source` out. Before
    /// the first read it offers them, since the revert reads afresh.
    fn file_revert_withheld(&self, source: DisplaySource) -> Option<String> {
        let index = match self.record_index.as_ref()? {
            Ok(index) => index,
            Err(reason) => return Some(reason.clone()),
        };
        let messages = self.state.session.messages();
        let target = match resolve_revert_target(
            messages,
            crate::session_history_head(&self.state.session),
            source_target_id(source),
        ) {
            Ok(target) => target,
            Err(error) => return Some(error),
        };
        boundary_item(messages, &target)
            .and_then(|boundary| select(index, boundary, &self.state.session.meta))
            .err()
            .map(str::to_owned)
    }

    /// The file half of a revert at `target`. The first call previews it and
    /// keeps it prepared; the same action again runs it.
    pub(super) fn revert_files(
        &mut self,
        conversation_source: Option<CaudraId>,
        target: &RevertTarget,
        mode: RestoreMode,
    ) -> FileRevert {
        if let Some(confirmation) = self.revert_confirmation.take() {
            if confirmation.conversation_source == conversation_source
                && confirmation.target_head == target.head
                && confirmation.mode == mode
            {
                return self.run_file_revert(confirmation);
            }
            release(confirmation.service.as_ref(), &confirmation.prepared);
        }
        if !self.snapshots_config.enabled {
            return FileRevert::Blocked(RECORDING_OFF.to_owned());
        }
        let (Some(service), Some(holder)) =
            (self.change_service(), holder_of(self.state.session.id))
        else {
            return FileRevert::Blocked(NO_CHANGE_RECORDS.to_owned());
        };
        let selection = match boundary_item(self.state.session.messages(), target) {
            Ok(boundary) => smol::block_on(self.select_fresh(service.as_ref(), &holder, boundary)),
            Err(reason) => Err(reason.to_owned()),
        };
        let selection = match selection {
            Ok(selection) => selection,
            Err(reason) => return FileRevert::Blocked(reason),
        };
        let prepared = match smol::block_on(service.prepare_revert(&holder, &selection.seqs)) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.status_bar.flash(format!("{REVERT_FAILED}: {error}"));
                return FileRevert::Held;
            }
        };
        let Some(preview) = revert_preview(&prepared) else {
            release(service.as_ref(), &prepared);
            self.status_bar
                .flash(format!("{REVERT_FAILED}: {NOT_A_REVERT}"));
            return FileRevert::Held;
        };
        if !preview.conflicts.is_empty() {
            let conflicts = list_conflicts(&preview.conflicts);
            release(service.as_ref(), &prepared);
            self.status_bar
                .flash(format!("{REVERT_CONFLICTS}: {conflicts}"));
            return FileRevert::Held;
        }
        self.status_bar
            .flash(describe_preview(preview, &selection.gaps));
        self.revert_confirmation = Some(RevertConfirmation {
            conversation_source,
            target_head: target.head,
            mode,
            service,
            prepared,
        });
        FileRevert::Held
    }

    /// Reads the records afresh, since the menu's copy is only a hint, and
    /// refuses while anything of this session is still moving files.
    async fn select_fresh(
        &self,
        service: &dyn WorkspaceChangeService,
        holder: &RecordHolder,
        boundary: Option<&HistoryItem>,
    ) -> Result<Selection, String> {
        let unavailable = |error| format!("{FILE_REVERT_UNAVAILABLE}: {error}");
        if !service
            .open_records(holder)
            .await
            .map_err(unavailable)?
            .is_empty()
        {
            return Err(OPEN_RECORDS.to_owned());
        }
        let status = service.status(holder).await.map_err(unavailable)?;
        if status.pending.iter().any(is_unsettled) {
            return Err(UNSETTLED_REVERT.to_owned());
        }
        let index = fetch_index(service, holder).await?;
        select(&index, boundary, &self.state.session.meta).map_err(str::to_owned)
    }

    fn run_file_revert(&mut self, confirmation: RevertConfirmation) -> FileRevert {
        let RevertConfirmation {
            conversation_source,
            target_head,
            mode,
            service,
            prepared,
        } = confirmation;
        let Some(holder) = holder_of(self.state.session.id) else {
            release(service.as_ref(), &prepared);
            return FileRevert::Blocked(NO_CHANGE_RECORDS.to_owned());
        };
        let Some(revert_id) = revert_preview(&prepared).map(|preview| preview.revert_id.clone())
        else {
            release(service.as_ref(), &prepared);
            return FileRevert::Blocked(NOT_A_REVERT.to_owned());
        };
        let settled = match smol::block_on(service.execute(&prepared)) {
            Ok(status) => settled(&status.state, &revert_id, RevertDirection::Revert),
            Err(error) => Settled::Unchanged(error.to_string()),
        };
        let previous = self.state.session.meta.pending_revert.clone();
        let original_head = previous
            .as_ref()
            .map_or(conversation_source, |pending| pending.original_head);
        match settled {
            Settled::Unchanged(reason) => {
                self.status_bar.flash(format!("{REVERT_FAILED}: {reason}"));
                FileRevert::Held
            }
            Settled::Done => {
                let pending = PendingConversationRevert {
                    original_head,
                    target_head,
                    file_status: self.fresh_file_status(service.as_ref(), &holder),
                };
                let head = if mode.restores_conversation() {
                    target_head
                } else {
                    conversation_source
                };
                self.state
                    .session_mut()
                    .set_conversation_state(head, Some(pending));
                self.save_reverted_state();
                FileRevert::Reverted
            }
            Settled::Unsettled(state) => {
                let pending = PendingConversationRevert {
                    original_head,
                    target_head: conversation_source,
                    file_status: self.fresh_file_status(service.as_ref(), &holder),
                };
                self.state
                    .session_mut()
                    .set_conversation_state(conversation_source, Some(pending));
                self.save_reverted_state();
                self.status_bar.flash(format!(
                    "File revert is {state}; Unrevert puts back what it changed"
                ));
                FileRevert::Held
            }
        }
    }

    fn fresh_file_status(
        &self,
        service: &dyn WorkspaceChangeService,
        holder: &RecordHolder,
    ) -> Option<Value> {
        smol::block_on(service.status(holder))
            .inspect_err(|error| warn!(%error, "file revert status unavailable"))
            .ok()
            .and_then(|status| serde_json::to_value(status).ok())
    }

    fn save_reverted_state(&mut self) {
        if let Err(error) = self.save_session_barrier() {
            self.status_bar.flash(format!(
                "Files were reverted, but saving the session failed: {error}"
            ));
        }
        self.refresh_record_index();
    }

    /// Puts back the files the session's pending reverts changed. Answers
    /// whether the conversation may follow.
    pub(super) fn unrevert_files(&mut self) -> bool {
        let (Some(service), Some(holder)) =
            (self.change_service(), holder_of(self.state.session.id))
        else {
            self.status_bar.flash(NO_CHANGE_RECORDS.to_owned());
            return false;
        };
        let pending = match smol::block_on(service.status(&holder)) {
            Ok(status) => !status.pending.is_empty(),
            Err(error) => {
                self.status_bar.flash(format!("{UNREVERT_FAILED}: {error}"));
                return false;
            }
        };
        if !pending {
            return true;
        }
        let prepared = match smol::block_on(service.prepare_unrevert(&holder)) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.status_bar.flash(format!("{UNREVERT_FAILED}: {error}"));
                return false;
            }
        };
        let Some(preview) = revert_preview(&prepared) else {
            release(service.as_ref(), &prepared);
            self.status_bar
                .flash(format!("{UNREVERT_FAILED}: {NOT_A_REVERT}"));
            return false;
        };
        if !preview.conflicts.is_empty() {
            let conflicts = list_conflicts(&preview.conflicts);
            release(service.as_ref(), &prepared);
            self.status_bar
                .flash(format!("{UNREVERT_CONFLICTS}: {conflicts}"));
            return false;
        }
        let revert_id = preview.revert_id.clone();
        let settled = match smol::block_on(service.execute(&prepared)) {
            Ok(status) => settled(&status.state, &revert_id, RevertDirection::Unrevert),
            Err(error) => Settled::Unchanged(error.to_string()),
        };
        match settled {
            Settled::Done => true,
            Settled::Unchanged(reason) => {
                self.status_bar
                    .flash(format!("{UNREVERT_FAILED}: {reason}"));
                false
            }
            Settled::Unsettled(state) => {
                let file_status = self.fresh_file_status(service.as_ref(), &holder);
                if let Some(mut pending) = self.state.session.meta.pending_revert.clone() {
                    pending.file_status = file_status;
                    let head = crate::session_history_head(&self.state.session);
                    self.state
                        .session_mut()
                        .set_conversation_state(head, Some(pending));
                    self.save_reverted_state();
                }
                self.status_bar.flash(format!(
                    "File unrevert is {state}; the conversation was not changed"
                ));
                false
            }
        }
    }

    /// New work keeps the reverted files as they are, so the store may
    /// forget what the reverts undid.
    pub(super) fn acknowledge_reverts(&mut self) -> Result<(), String> {
        let (Some(service), Some(holder)) =
            (self.change_service(), holder_of(self.state.session.id))
        else {
            warn!("no change records to settle reverted files in");
            return Ok(());
        };
        smol::block_on(service.acknowledge(&holder)).map_err(|error| error.to_string())?;
        self.refresh_record_index();
        Ok(())
    }

    /// Lets the fork revert the files its history changed: it holds every
    /// record this session holds. Without records the fork simply has no
    /// file history.
    pub(super) fn hold_records_for(&self, child: CaudraId) {
        if !self.snapshots_config.enabled {
            return;
        }
        let (Some(service), Some(parent), Some(child)) = (
            self.change_service(),
            holder_of(self.state.session.id),
            holder_of(child),
        ) else {
            return;
        };
        if let Err(error) = smol::block_on(service.hold(&parent, &child)) {
            warn!(%error, "fork inherits no file history");
        }
    }

    pub(super) fn release_revert_confirmation(&mut self) {
        if let Some(confirmation) = self.revert_confirmation.take() {
            release(confirmation.service.as_ref(), &confirmation.prepared);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::MutexGuard;

    use arc_swap::ArcSwap;
    use async_trait::async_trait;
    use caudra_agent::{History, HistorySnapshot, SharedHistory};
    use caudra_config::PermissionsConfig;
    use caudra_providers::{ContentBlock, Message, Role, expand_message};
    use caudra_storage::StateDir;
    use caudra_storage::sessions::RecordCoverage;
    use caudra_workcell::WorkcellHost;
    use caudra_workspace::{
        CancellationResult, CwdHandle, HolderPage, OpenRecord, OperationHandle, OperationId,
        OperationPhase, OperationStatus, RecordLimits, RecordListing, RecordPage, RecordRequest,
        RecordScope, RecordSummary, RecordTicket, ReleaseResult, ReleaseSelection, ReleaseSummary,
        RevertCounts, SequenceMetadata, WorkspaceCursor, WorkspaceError, WorkspacePath,
    };
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::app::tests::{remote_workspace_session, test_app};
    use crate::components::Action;
    use crate::components::message_actions::MessageActionKind;

    const MODEL: &str = "test-model";
    const PROMPT: &str = "change the files";
    const REPLY: &str = "changed";
    const NEW_WORK: &str = "keep going";
    const FIRST_CALL: &str = "call-1";
    const SECOND_CALL: &str = "call-2";
    const TOOL: &str = "write";
    const TOOL_OUTPUT: &str = "written";
    const CHANGED: &str = "a.txt";
    const OTHER: &str = "b.txt";
    const USERS: &str = "c.txt";
    const CREATED: &str = "created.txt";
    const DELETED: &str = "deleted.txt";
    const BEFORE: &str = "before";
    const AFTER: &str = "after";
    const LATER: &str = "later";
    const TICKET: &str = "record-ticket";
    const PREPARED: &str = "prepared";
    const REVERT: &str = "revert";
    const LEGACY_FAILURE: &str = "interrupted";
    const MORE_CONFLICTS: usize = 2;
    const CHANGE_RECORDED: &str = "the call changed a recorded file";
    const STORE: &str = "workspace-key";
    const NESTED: &str = "nested";
    const UNUSED: WorkspaceError = WorkspaceError::Unavailable;
    const LIMITS: RecordLimits = RecordLimits {
        max_files: 1_000,
        max_file_bytes: 1024 * 1024,
        max_total_bytes: 16 * 1024 * 1024,
    };

    /// What the app asked the store to change, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        AbandonOpen,
        Hold(RecordHolder),
        PrepareRevert(Vec<u64>),
        PrepareUnrevert,
        Execute,
        Release,
        Acknowledge,
    }

    /// The store's answers, set by a test before it acts.
    struct Script {
        records: Vec<RecordListing>,
        open: Vec<OpenRecord>,
        pending: Vec<PendingRevert>,
        conflicts: Vec<RevertConflict>,
        lands_in: RevertState,
    }

    struct ScriptedChanges {
        script: Mutex<Script>,
        calls: Mutex<Vec<Call>>,
    }

    impl Default for ScriptedChanges {
        fn default() -> Self {
            Self {
                script: Mutex::new(Script {
                    records: Vec::new(),
                    open: Vec::new(),
                    pending: Vec::new(),
                    conflicts: Vec::new(),
                    lands_in: RevertState::Completed,
                }),
                calls: Mutex::default(),
            }
        }
    }

    impl ScriptedChanges {
        fn script(&self) -> MutexGuard<'_, Script> {
            self.script.lock().unwrap()
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn call(&self, call: Call) {
            self.calls.lock().unwrap().push(call);
        }

        fn status(&self) -> RevertStatus {
            RevertStatus {
                pending: self.script().pending.clone(),
            }
        }

        fn prepared(&self, direction: RevertDirection) -> PreparedChangeOperation {
            PreparedChangeOperation {
                operation: handle(),
                preview: ChangeOperationPreview::Revert(RevertPreview {
                    revert_id: RevertId::new(REVERT).unwrap(),
                    direction,
                    records: 1,
                    counts: RevertCounts {
                        replace: 1,
                        ..RevertCounts::default()
                    },
                    planned: Vec::new(),
                    conflicts: self.script().conflicts.clone(),
                    created_directories: Vec::new(),
                }),
            }
        }
    }

    fn handle() -> OperationHandle {
        OperationHandle {
            preparation_id: OperationId::new(PREPARED).unwrap(),
            invocation_id: None,
            execution_id: None,
            expires_at_unix_ms: None,
        }
    }

    fn pending_revert(state: RevertState) -> PendingRevert {
        PendingRevert {
            revert_id: RevertId::new(REVERT).unwrap(),
            direction: RevertDirection::Revert,
            state,
            records: 1,
            applied_files: 1,
            total_files: 1,
            reconciliation_required: false,
            stopped_at: (state == RevertState::Partial)
                .then(|| WorkspacePath::new(CHANGED).unwrap()),
        }
    }

    #[async_trait]
    impl WorkspaceChangeService for ScriptedChanges {
        async fn begin(&self, _request: &RecordRequest) -> Result<RecordTicket, WorkspaceError> {
            Err(UNUSED)
        }

        async fn finish(
            &self,
            _ticket: &RecordTicket,
        ) -> Result<Option<RecordSummary>, WorkspaceError> {
            Err(UNUSED)
        }

        async fn abandon(&self, _ticket: &RecordTicket) -> Result<bool, WorkspaceError> {
            Err(UNUSED)
        }

        async fn open_records(
            &self,
            _holder: &RecordHolder,
        ) -> Result<Vec<OpenRecord>, WorkspaceError> {
            Ok(self.script().open.clone())
        }

        async fn abandon_open_records(
            &self,
            _holder: &RecordHolder,
        ) -> Result<u32, WorkspaceError> {
            self.call(Call::AbandonOpen);
            Ok(u32::try_from(self.script().open.drain(..).count()).unwrap())
        }

        async fn records(
            &self,
            _holder: &RecordHolder,
            _after_seq: Option<u64>,
            _page_size: u32,
        ) -> Result<RecordPage, WorkspaceError> {
            Ok(RecordPage {
                records: self.script().records.clone(),
                next_after_seq: None,
                evicted_through: None,
            })
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
            to: &RecordHolder,
        ) -> Result<u32, WorkspaceError> {
            self.call(Call::Hold(to.clone()));
            Ok(0)
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
            seqs: &[u64],
        ) -> Result<PreparedChangeOperation, WorkspaceError> {
            self.call(Call::PrepareRevert(seqs.to_vec()));
            Ok(self.prepared(RevertDirection::Revert))
        }

        async fn prepare_unrevert(
            &self,
            _holder: &RecordHolder,
        ) -> Result<PreparedChangeOperation, WorkspaceError> {
            self.call(Call::PrepareUnrevert);
            Ok(self.prepared(RevertDirection::Unrevert))
        }

        async fn acknowledge(
            &self,
            _holder: &RecordHolder,
        ) -> Result<RevertStatus, WorkspaceError> {
            self.call(Call::Acknowledge);
            self.script().pending.clear();
            Ok(self.status())
        }

        async fn status(&self, _holder: &RecordHolder) -> Result<RevertStatus, WorkspaceError> {
            Ok(self.status())
        }

        async fn prepare_cleanup(
            &self,
            _retention_bytes: u64,
        ) -> Result<PreparedChangeOperation, WorkspaceError> {
            Err(UNUSED)
        }

        async fn execute(
            &self,
            prepared: &PreparedChangeOperation,
        ) -> Result<OperationStatus<ChangeOperationResult>, WorkspaceError> {
            self.call(Call::Execute);
            let direction = revert_preview(prepared).map(|preview| preview.direction);
            {
                let mut script = self.script();
                if direction == Some(RevertDirection::Unrevert) {
                    script.pending.clear();
                } else {
                    let state = script.lands_in;
                    script.pending.push(pending_revert(state));
                }
            }
            Ok(OperationStatus {
                handle: handle(),
                state: OperationState::Completed {
                    result: ChangeOperationResult::Revert(self.status()),
                    side_effects_possible: true,
                },
                progress: Vec::new(),
                progress_metadata: SequenceMetadata {
                    first_retained_sequence: None,
                    next_sequence: 0,
                    gap_before_first: false,
                },
            })
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
            self.call(Call::Release);
            Ok(ReleaseResult {
                state: OperationPhase::Forgotten,
                released: true,
            })
        }
    }

    /// Appends `message` after `parent` and answers its first item.
    fn push_after(
        items: &mut Vec<HistoryItem>,
        parent: Option<CaudraId>,
        message: Message,
    ) -> CaudraId {
        let first = items.len();
        items.extend(expand_message(&message, parent));
        items[first].id
    }

    fn push(items: &mut Vec<HistoryItem>, message: Message) -> CaudraId {
        let parent = items.last().map(|item| item.id);
        push_after(items, parent, message)
    }

    fn reply() -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: REPLY.into() }],
            ..Default::default()
        }
    }

    fn tool_call(call_id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(call_id, TOOL, json!({}))],
            ..Default::default()
        }
    }

    fn tool_result(call_id: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: call_id.into(),
                content: TOOL_OUTPUT.into(),
                is_error: false,
                output_ref: None,
            }],
            ..Default::default()
        }
    }

    fn client(at: CaudraId) -> Value {
        serde_json::to_value(RecordClient {
            call_id: FIRST_CALL.into(),
            root_call_id: FIRST_CALL.into(),
            at,
        })
        .unwrap()
    }

    fn marks(made: &[CaudraId]) -> RecordIndex {
        RecordIndex {
            marks: made
                .iter()
                .zip(1..)
                .map(|(at, seq)| RecordMark {
                    seq,
                    at: *at,
                    state: RecordState::Applied,
                })
                .collect(),
            evicted_through: None,
        }
    }

    /// A store that holds every record the session made.
    fn everything() -> Option<RecordCoverage> {
        Some(RecordCoverage {
            store: STORE.into(),
            since: None,
        })
    }

    fn covered() -> SessionMeta {
        SessionMeta {
            record_coverage: everything(),
            ..SessionMeta::default()
        }
    }

    /// Opens `changes` as the local store of any directory.
    fn factory(changes: &Arc<ScriptedChanges>) -> ChangeServiceFactory {
        let changes = Arc::clone(changes) as Arc<dyn WorkspaceChangeService>;
        Arc::new(move |_: &Path| Some(Arc::clone(&changes)))
    }

    fn conflict() -> RevertConflict {
        RevertConflict {
            path: WorkspacePath::new(CHANGED).unwrap(),
            kind: RevertConflictKind::ChangedSince,
            reason: None,
        }
    }

    fn changed_conflict() -> String {
        format!("changed {CHANGED}")
    }

    /// A turn whose two tool calls each recorded a change, a second turn a
    /// conversation-only revert abandoned with its change left on disk, and
    /// the turn that replaced it. Record `n` is the `n`th change made.
    struct Timeline {
        items: Vec<HistoryItem>,
        first_prompt: CaudraId,
        first_call: CaudraId,
        first_reply: CaudraId,
        new_prompt: CaudraId,
        index: RecordIndex,
    }

    #[derive(Debug, Clone, Copy)]
    enum At {
        FirstPrompt,
        FirstCall,
        FirstReply,
        NewPrompt,
    }

    impl Timeline {
        fn new() -> Self {
            let mut items = Vec::new();
            let mut made = Vec::new();
            let first_prompt = push(&mut items, Message::user(PROMPT.into()));
            let first_call = push(&mut items, tool_call(FIRST_CALL));
            made.push(CaudraId::generate());
            push(&mut items, tool_result(FIRST_CALL));
            push(&mut items, tool_call(SECOND_CALL));
            made.push(CaudraId::generate());
            push(&mut items, tool_result(SECOND_CALL));
            let first_reply = push(&mut items, reply());
            push(&mut items, Message::user(PROMPT.into()));
            made.push(CaudraId::generate());
            push(&mut items, reply());
            let new_prompt =
                push_after(&mut items, Some(first_reply), Message::user(PROMPT.into()));
            made.push(CaudraId::generate());
            push(&mut items, reply());
            Self {
                items,
                first_prompt,
                first_call,
                first_reply,
                new_prompt,
                index: marks(&made),
            }
        }

        fn item(&self, at: At) -> &HistoryItem {
            let id = match at {
                At::FirstPrompt => self.first_prompt,
                At::FirstCall => self.first_call,
                At::FirstReply => self.first_reply,
                At::NewPrompt => self.new_prompt,
            };
            self.items.iter().find(|item| item.id == id).unwrap()
        }

        /// The records a file revert chosen at `at` reaches.
        fn reached(&self, at: At) -> Result<Vec<u64>, String> {
            let head = self.items.last().map(|item| item.id);
            let target = resolve_revert_target(&self.items, head, self.item(at).id)?;
            let boundary = boundary_item(&self.items, &target)?;
            Ok(select(&self.index, boundary, &covered())?.seqs)
        }
    }

    #[test_case(At::FirstCall, &[2, 3, 4] ; "a_tool_row_mid_run_keeps_its_own_change")]
    #[test_case(At::FirstPrompt, &[1, 2, 3, 4] ; "a_user_prompt_reaches_its_whole_turn")]
    #[test_case(At::NewPrompt, &[4] ; "an_abandoned_branch_kept_by_a_conversation_revert_stays")]
    #[test_case(At::FirstReply, &[3, 4] ; "a_revert_from_before_the_branch_reaches_the_kept_change")]
    fn a_file_revert_reaches_the_records_made_after_its_message(at: At, expected: &[u64]) {
        assert_eq!(Timeline::new().reached(at).unwrap(), expected);
    }

    #[test_case(true, Ok(vec![4]) ; "a_mapped_copy_measures_from_its_original")]
    #[test_case(false, Err(UNKNOWN_BOUNDARY) ; "an_unknown_copy_is_refused")]
    fn a_compaction_copy_measures_from_its_original(
        mapped: bool,
        expected: Result<Vec<u64>, &str>,
    ) {
        let timeline = Timeline::new();
        let original = timeline.item(At::NewPrompt);
        let mut copy = original.clone();
        copy.id = CaudraId::generate();
        copy.stands_for = Some(if mapped { original.id } else { copy.id });

        let reached =
            select(&timeline.index, Some(&copy), &covered()).map(|selection| selection.seqs);

        assert_eq!(reached, expected);
    }

    #[test]
    fn with_no_boundary_every_applied_record_is_reached() {
        let mut index = Timeline::new().index;
        index.marks[0].state = RecordState::Reverted;

        let reached = select(&index, None, &covered()).map(|selection| selection.seqs);

        assert_eq!(reached, Ok(vec![2, 3, 4]));
    }

    #[test_case(0, Ok(vec![4]) ; "eviction_before_the_boundary_is_harmless")]
    #[test_case(3, Err(RECORDS_EVICTED) ; "eviction_past_the_boundary_is_refused")]
    fn eviction_past_the_boundary_refuses_the_revert(
        evicted: usize,
        expected: Result<Vec<u64>, &str>,
    ) {
        let timeline = Timeline::new();
        let mut index = timeline.index.clone();
        index.evicted_through = Some(index.marks[evicted].at);

        let reached = select(&index, Some(timeline.item(At::NewPrompt)), &covered())
            .map(|selection| selection.seqs);

        assert_eq!(reached, expected);
    }

    #[test_case(Some(At::FirstReply), Ok(vec![3, 4]) ; "a_message_where_the_store_took_over_is_reached")]
    #[test_case(Some(At::FirstCall), Err(RECORDS_UNREACHABLE) ; "a_message_before_it_is_refused")]
    #[test_case(None, Err(RECORDS_UNREACHABLE) ; "the_whole_session_is_refused")]
    fn a_revert_reaches_back_only_to_where_the_store_took_over(
        at: Option<At>,
        expected: Result<Vec<u64>, &str>,
    ) {
        let timeline = Timeline::new();
        let meta = SessionMeta {
            record_coverage: Some(RecordCoverage {
                store: STORE.into(),
                since: Some(timeline.first_reply),
            }),
            ..SessionMeta::default()
        };

        let reached = select(&timeline.index, at.map(|at| timeline.item(at)), &meta)
            .map(|selection| selection.seqs);

        assert_eq!(reached, expected);
    }

    #[test]
    fn a_session_recording_nowhere_reaches_no_message() {
        let timeline = Timeline::new();

        let reached = select(
            &timeline.index,
            Some(timeline.item(At::NewPrompt)),
            &SessionMeta::default(),
        );

        assert_eq!(reached, Err(RECORDS_UNREACHABLE));
    }

    #[test_case(2, true, Gaps { count: 1, at_least: false } ; "only_gaps_after_the_boundary_count")]
    #[test_case(MAX_UNRECORDED_CALLS, false, Gaps { count: MAX_UNRECORDED_CALLS, at_least: true } ; "a_full_list_after_the_boundary_is_a_floor")]
    #[test_case(MAX_UNRECORDED_CALLS, true, Gaps { count: MAX_UNRECORDED_CALLS - 1, at_least: false } ; "a_full_list_reaching_back_past_it_is_exact")]
    fn gaps_after_the_boundary_are_counted(total: usize, oldest_before: bool, expected: Gaps) {
        let timeline = Timeline::new();
        let unrecorded: Vec<_> = (0..total)
            .map(|gap| UnrecordedCall {
                at: if gap == 0 && oldest_before {
                    timeline.first_prompt
                } else {
                    CaudraId::generate()
                },
                call_id: FIRST_CALL.into(),
                reason: INTERRUPTED_RECORD.into(),
            })
            .collect();

        let selection = select(
            &timeline.index,
            Some(timeline.item(At::NewPrompt)),
            &SessionMeta {
                unrecorded,
                ..covered()
            },
        )
        .unwrap();

        assert_eq!(selection.gaps, expected);
    }

    #[test_case(1 ; "every_conflict_listed")]
    #[test_case(LISTED_CONFLICTS + MORE_CONFLICTS ; "a_long_list_counts_its_tail")]
    fn conflicts_are_listed_as_kind_and_path(count: usize) {
        let mut expected = vec![changed_conflict(); count.min(LISTED_CONFLICTS)];
        if count > LISTED_CONFLICTS {
            expected.push(format!("+{MORE_CONFLICTS} more"));
        }

        assert_eq!(
            list_conflicts(&vec![conflict(); count]),
            expected.join(", ")
        );
    }

    /// Two turns, each of which recorded one change while it ran.
    struct Turns {
        items: Vec<HistoryItem>,
        first_prompt: CaudraId,
        first_reply: CaudraId,
        second_prompt: CaudraId,
        made: [CaudraId; 2],
    }

    fn two_turns() -> Turns {
        let mut items = Vec::new();
        let first_prompt = push(&mut items, Message::user(PROMPT.into()));
        let first_made = CaudraId::generate();
        let first_reply = push(&mut items, reply());
        let second_prompt = push(&mut items, Message::user(PROMPT.into()));
        let second_made = CaudraId::generate();
        push(&mut items, reply());
        Turns {
            items,
            first_prompt,
            first_reply,
            second_prompt,
            made: [first_made, second_made],
        }
    }

    fn scripted_app(turns: &Turns) -> (App, Arc<ScriptedChanges>) {
        let changes = Arc::new(ScriptedChanges::default());
        changes.script().records = turns
            .made
            .iter()
            .zip(1..)
            .map(|(at, seq)| RecordListing {
                seq,
                client: client(*at),
                state: RecordState::Applied,
                paths: 1,
                unrecorded: 0,
            })
            .collect();
        let mut app = test_app();
        let session = app.state.session_mut();
        session.replace_messages(turns.items.clone());
        session.meta.record_coverage = everything();
        app.local_changes = Some(Arc::clone(&changes) as Arc<dyn WorkspaceChangeService>);
        (app, changes)
    }

    fn head(app: &App) -> Option<CaudraId> {
        crate::session_history_head(&app.state.session)
    }

    fn flash(app: &App) -> &str {
        app.status_bar.flash_text().unwrap_or_default()
    }

    /// The menu's read of the records, waited for.
    fn read_index(app: &mut App) {
        app.refresh_record_index();
        if let Some(refresh) = app.record_index_refresh.take() {
            app.record_index = Some(smol::block_on(refresh));
        }
    }

    #[test_case(2, true ; "offered_while_a_record_follows")]
    #[test_case(1, false ; "withheld_with_the_reason")]
    fn the_menu_offers_file_reverts_only_with_records_after_the_message(
        recorded: usize,
        offered: bool,
    ) {
        let turns = two_turns();
        let (mut app, changes) = scripted_app(&turns);
        changes.script().records.truncate(recorded);
        read_index(&mut app);

        app.open_message_actions(DisplaySource::User(turns.second_prompt));

        for kind in [
            MessageActionKind::RevertBoth,
            MessageActionKind::RevertFiles,
        ] {
            assert_eq!(app.message_actions.offers(kind), offered, "{kind:?}");
        }
        assert_eq!(
            app.message_actions.title().contains(NO_RECORDED_CHANGES),
            !offered
        );
    }

    #[test_case(true ; "withheld_before_the_store_took_over")]
    #[test_case(false ; "offered_from_where_it_took_over")]
    fn the_menu_offers_file_reverts_only_as_far_back_as_the_records_reach(before: bool) {
        let turns = two_turns();
        let (mut app, _) = scripted_app(&turns);
        app.state.session_mut().meta.record_coverage = Some(RecordCoverage {
            store: STORE.into(),
            since: Some(turns.second_prompt),
        });
        read_index(&mut app);
        let message = if before {
            turns.first_prompt
        } else {
            turns.second_prompt
        };

        app.open_message_actions(DisplaySource::User(message));

        assert_eq!(
            app.message_actions.offers(MessageActionKind::RevertFiles),
            !before
        );
        assert_eq!(
            app.message_actions.title().contains(RECORDS_UNREACHABLE),
            before
        );
    }

    #[test]
    fn a_revert_from_before_the_store_took_over_prepares_nothing() {
        let turns = two_turns();
        let (mut app, changes) = scripted_app(&turns);
        app.state.session_mut().meta.record_coverage = Some(RecordCoverage {
            store: STORE.into(),
            since: Some(turns.second_prompt),
        });

        app.revert_to(turns.first_prompt, RestoreMode::Files);

        assert!(changes.calls().is_empty());
        assert_eq!(flash(&app), RECORDS_UNREACHABLE);
    }

    #[test]
    fn the_first_action_previews_and_executes_nothing() {
        let turns = two_turns();
        let (mut app, changes) = scripted_app(&turns);
        let source = head(&app);

        let actions = app.revert_to(turns.second_prompt, RestoreMode::Both);

        assert!(actions.is_empty());
        assert_eq!(changes.calls(), [Call::PrepareRevert(vec![2])]);
        assert!(flash(&app).ends_with(REPEAT_TO_CONFIRM));
        assert_eq!(head(&app), source);
        assert!(app.state.session.meta.pending_revert.is_none());
    }

    #[test]
    fn repeating_the_action_executes_the_previewed_revert() {
        let turns = two_turns();
        let (mut app, changes) = scripted_app(&turns);
        let source = head(&app);
        app.revert_to(turns.second_prompt, RestoreMode::Both);

        let actions = app.revert_to(turns.second_prompt, RestoreMode::Both);

        assert!(matches!(actions.as_slice(), [Action::LoadSession(_)]));
        assert_eq!(
            changes.calls(),
            [Call::PrepareRevert(vec![2]), Call::Execute]
        );
        assert_eq!(head(&app), Some(turns.first_reply));
        let pending = app.state.session.meta.pending_revert.as_ref().unwrap();
        assert_eq!(pending.original_head, source);
        assert!(files_pending(pending));
    }

    #[test_case(RestoreMode::Files, true ; "another_message")]
    #[test_case(RestoreMode::Both, false ; "another_mode")]
    fn a_different_request_releases_the_preview_and_previews_again(
        mode: RestoreMode,
        other_message: bool,
    ) {
        let turns = two_turns();
        let (mut app, changes) = scripted_app(&turns);
        app.revert_to(turns.second_prompt, RestoreMode::Files);
        let (target, seqs) = if other_message {
            (turns.first_prompt, vec![1, 2])
        } else {
            (turns.second_prompt, vec![2])
        };

        app.revert_to(target, mode);

        assert_eq!(
            changes.calls(),
            [
                Call::PrepareRevert(vec![2]),
                Call::Release,
                Call::PrepareRevert(seqs)
            ]
        );
    }

    #[test]
    fn a_conflict_aborts_both_halves_and_names_the_paths() {
        let turns = two_turns();
        let (mut app, changes) = scripted_app(&turns);
        changes.script().conflicts = vec![conflict()];
        let source = head(&app);

        app.revert_to(turns.second_prompt, RestoreMode::Both);
        app.revert_to(turns.second_prompt, RestoreMode::Both);

        assert_eq!(
            changes.calls(),
            [
                Call::PrepareRevert(vec![2]),
                Call::Release,
                Call::PrepareRevert(vec![2]),
                Call::Release
            ]
        );
        assert_eq!(
            flash(&app),
            format!("{REVERT_CONFLICTS}: {}", changed_conflict())
        );
        assert_eq!(head(&app), source);
        assert!(app.state.session.meta.pending_revert.is_none());
    }

    #[test]
    fn a_partial_revert_keeps_the_conversation_and_offers_unrevert() {
        let turns = two_turns();
        let (mut app, changes) = scripted_app(&turns);
        changes.script().lands_in = RevertState::Partial;
        let source = head(&app);

        app.revert_to(turns.second_prompt, RestoreMode::Both);
        app.revert_to(turns.second_prompt, RestoreMode::Both);

        assert_eq!(head(&app), source);
        assert!(flash(&app).contains(PARTIAL));
        app.open_message_actions(DisplaySource::User(turns.second_prompt));
        assert!(app.message_actions.offers(MessageActionKind::Unrevert));
    }

    #[test]
    fn unrevert_puts_the_files_back_then_the_conversation() {
        let turns = two_turns();
        let (mut app, changes) = scripted_app(&turns);
        let source = head(&app);
        app.revert_to(turns.second_prompt, RestoreMode::Both);
        app.revert_to(turns.second_prompt, RestoreMode::Both);

        let actions = app.unrevert();

        assert!(matches!(actions.as_slice(), [Action::LoadSession(_)]));
        assert_eq!(changes.calls()[2..], [Call::PrepareUnrevert, Call::Execute]);
        assert_eq!(head(&app), source);
        assert!(app.state.session.meta.pending_revert.is_none());
    }

    #[test]
    fn an_unrevert_conflict_keeps_the_revert() {
        let turns = two_turns();
        let (mut app, changes) = scripted_app(&turns);
        app.revert_to(turns.second_prompt, RestoreMode::Both);
        app.revert_to(turns.second_prompt, RestoreMode::Both);
        changes.script().conflicts = vec![conflict()];

        let actions = app.unrevert();

        assert!(actions.is_empty());
        assert_eq!(changes.calls()[2..], [Call::PrepareUnrevert, Call::Release]);
        assert_eq!(
            flash(&app),
            format!("{UNREVERT_CONFLICTS}: {}", changed_conflict())
        );
        assert_eq!(head(&app), Some(turns.first_reply));
        assert!(app.state.session.meta.pending_revert.is_some());
    }

    #[test]
    fn new_work_settles_the_reverted_files() {
        let turns = two_turns();
        let (mut app, changes) = scripted_app(&turns);
        app.revert_to(turns.second_prompt, RestoreMode::Files);
        app.revert_to(turns.second_prompt, RestoreMode::Files);
        let mirror: SharedHistory = Arc::new(ArcSwap::from_pointee(HistorySnapshot::default()));
        let mut history =
            History::restored(crate::active_session_history(&app.state.session).unwrap())
                .unwrap()
                .with_mirror(Arc::clone(&mirror));
        app.shared_history = Some(mirror);
        app.checkpoint();
        assert!(!changes.calls().contains(&Call::Acknowledge));

        history.push(Message::user(NEW_WORK.into()));
        app.checkpoint();

        assert_eq!(changes.calls().last(), Some(&Call::Acknowledge));
        assert!(app.state.session.meta.pending_revert.is_none());
    }

    #[test]
    fn a_fork_holds_the_records_of_its_parent() {
        let turns = two_turns();
        let (app, changes) = scripted_app(&turns);

        let forked = app
            .fork_at(DisplaySource::AssistantText(turns.first_reply))
            .unwrap();

        assert_eq!(
            changes.calls(),
            [Call::Hold(holder_of(forked.session.id).unwrap())]
        );
        assert_eq!(forked.session.meta.record_coverage, everything());
    }

    fn loaded(turns: &Turns) -> (AppSession, RecordHolder) {
        let mut session = AppSession::new(MODEL, "");
        session.replace_messages(turns.items.clone());
        let holder = holder_of(session.id).unwrap();
        (session, holder)
    }

    fn reconcile(
        session: &mut AppSession,
        changes: &ScriptedChanges,
        holder: &RecordHolder,
        abandon: bool,
    ) -> Reconciled {
        smol::block_on(reconcile_pending_revert(session, changes, holder, abandon))
    }

    #[test_case(true ; "a_stopped_process_left_them")]
    #[test_case(false ; "this_process_may_still_own_them")]
    fn load_turns_open_records_into_gaps(abandon: bool) {
        let turns = two_turns();
        let changes = ScriptedChanges::default();
        changes.script().open = vec![OpenRecord {
            ticket: RecordTicket::new(TICKET).unwrap(),
            client: client(turns.made[1]),
            opened_at_unix_ms: 0,
        }];
        let (mut session, holder) = loaded(&turns);

        let reconciled = reconcile(&mut session, &changes, &holder, abandon);

        let gaps: Vec<_> = session
            .meta
            .unrecorded
            .iter()
            .map(|gap| (gap.at, gap.reason.as_str()))
            .collect();
        if abandon {
            assert_eq!(gaps, [(turns.made[1], INTERRUPTED_RECORD)]);
            assert_eq!(changes.calls(), [Call::AbandonOpen]);
        } else {
            assert!(gaps.is_empty());
            assert!(changes.calls().is_empty());
        }
        assert_eq!(reconciled.changed, abandon);
    }

    #[test]
    fn load_adopts_a_revert_the_session_does_not_know() {
        let turns = two_turns();
        let changes = ScriptedChanges::default();
        changes.script().pending = vec![pending_revert(RevertState::Completed)];
        let (mut session, holder) = loaded(&turns);
        let current = crate::session_history_head(&session);

        let reconciled = reconcile(&mut session, &changes, &holder, false);

        assert_eq!(reconciled.notices, [REVERT_ADOPTED]);
        let pending = session.meta.pending_revert.as_ref().unwrap();
        assert_eq!(
            (pending.original_head, pending.target_head),
            (current, current)
        );
        assert!(files_pending(pending));
        assert_eq!(crate::session_history_head(&session), current);
    }

    #[test_case(true ; "a_files_only_revert_is_dropped")]
    #[test_case(false ; "a_conversation_revert_stays")]
    fn load_clears_a_file_status_the_store_no_longer_holds(files_only: bool) {
        let turns = two_turns();
        let changes = ScriptedChanges::default();
        let (mut session, holder) = loaded(&turns);
        let original_head = crate::session_history_head(&session);
        let current = if files_only {
            original_head
        } else {
            Some(turns.first_reply)
        };
        let stale = RevertStatus {
            pending: vec![pending_revert(RevertState::Completed)],
        };
        session.set_conversation_state(
            current,
            Some(PendingConversationRevert {
                original_head,
                target_head: current,
                file_status: serde_json::to_value(stale).ok(),
            }),
        );

        reconcile(&mut session, &changes, &holder, false);

        let pending = session.meta.pending_revert.as_ref();
        assert_eq!(pending.is_some(), !files_only);
        assert!(pending.is_none_or(|pending| pending.file_status.is_none()));
        assert_eq!(crate::session_history_head(&session), current);
    }

    #[test]
    fn old_meta_with_a_legacy_restore_loads_and_is_cleared() {
        let turns = two_turns();
        let changes = ScriptedChanges::default();
        let (mut session, holder) = loaded(&turns);
        let current = crate::session_history_head(&session).unwrap();
        let legacy: PendingConversationRevert = serde_json::from_value(json!({
            "original_head": current.to_string(),
            "target_head": turns.first_reply.to_string(),
            "original_workspace_head": { "head": current.to_string() },
            "workspace_head": { "head": current.to_string() },
            "file_status": { "status": "failed", "kind": "other", "message": LEGACY_FAILURE },
            "restore_operation": { "kind": "revert", "phase": "intent", "overwrite": false },
        }))
        .unwrap();
        session.set_conversation_state(Some(current), Some(legacy));

        let reconciled = reconcile(&mut session, &changes, &holder, false);

        assert_eq!(reconciled.notices, [LEGACY_RESTORE_CLEARED]);
        assert!(session.meta.pending_revert.is_none());
        assert_eq!(crate::session_history_head(&session), Some(current));
    }

    fn recording(enabled: bool) -> SnapshotsConfig {
        SnapshotsConfig {
            enabled,
            ..SnapshotsConfig::default()
        }
    }

    /// A session in `dir`, with the two turns as its history when it has one.
    fn session_in(dir: &Path, history: bool) -> AppSession {
        let mut session = AppSession::new(MODEL, &dir.to_string_lossy());
        if history {
            session.replace_messages(two_turns().items);
        }
        session
    }

    /// Coverage of `dir`'s store from `session`'s last item, which every
    /// fixture here appends as its newest.
    fn covered_from(dir: &Path, session: &AppSession) -> Option<RecordCoverage> {
        Some(RecordCoverage {
            store: workspace_key(dir).unwrap(),
            since: session.messages().last().map(|item| item.id),
        })
    }

    #[test_case(true ; "on_covers_from_the_newest_item")]
    #[test_case(false ; "off_covers_nothing")]
    fn recording_follows_the_setting(enabled: bool) {
        let dir = TempDir::new().unwrap();
        let mut session = session_in(dir.path(), true);
        session.meta.record_coverage = everything();

        let built = session_changes(
            &mut session,
            None,
            Some(&factory(&Arc::default())),
            &recording(enabled),
        );

        assert_eq!(built.recorder.is_some(), enabled);
        assert!(built.local.is_some());
        assert_eq!(
            session.meta.record_coverage,
            covered_from(dir.path(), &session).filter(|_| enabled)
        );
    }

    /// `at_head` is what a file revert at the head then answers, with no
    /// records to find.
    #[test_case(true, false, NO_RECORDED_CHANGES ; "a_session_with_history_is_covered_from_its_newest_item")]
    #[test_case(true, true, RECORDS_UNREACHABLE ; "a_rewound_session_is_covered_from_its_newest_branch")]
    #[test_case(false, false, NO_RECORDED_CHANGES ; "a_new_session_is_covered_whole")]
    fn a_session_without_coverage_is_covered_from_its_newest_item(
        history: bool,
        rewound: bool,
        at_head: &str,
    ) {
        let dir = TempDir::new().unwrap();
        let mut session = session_in(dir.path(), history);
        if rewound {
            let first = session.messages()[0].id;
            session.set_conversation_state(Some(first), None);
        }

        session_changes(
            &mut session,
            None,
            Some(&factory(&Arc::default())),
            &recording(true),
        );

        assert_eq!(
            session.meta.record_coverage,
            covered_from(dir.path(), &session)
        );
        let head = crate::session_history_head(&session).map(|id| {
            session
                .messages()
                .iter()
                .find(|item| item.id == id)
                .unwrap()
        });
        let reached =
            select(&RecordIndex::default(), head, &session.meta).map(|selection| selection.seqs);
        assert_eq!(reached, Err(at_head));
    }

    /// The conversation was reverted to the first prompt, then compaction
    /// appended a copy of it, which is the session's last item.
    #[test_case(true ; "a_copy_of_an_unknown_original_counts_from_when_it_was_made")]
    #[test_case(false ; "a_copy_counts_from_its_original")]
    fn a_newest_compaction_copy_is_covered_from_what_it_stands_for(unknown: bool) {
        let dir = TempDir::new().unwrap();
        let turns = two_turns();
        let mut items = turns.items;
        let newest = items.last().unwrap().id;
        let mut copy = items[0].clone();
        copy.id = CaudraId::generate();
        copy.stands_for = Some(if unknown { copy.id } else { turns.first_prompt });
        let made = copy.id;
        items.push(copy);
        let mut session = AppSession::new(MODEL, &dir.path().to_string_lossy());
        session.replace_messages(items);
        session.set_conversation_state(Some(turns.first_prompt), None);

        session_changes(
            &mut session,
            None,
            Some(&factory(&Arc::default())),
            &recording(true),
        );

        let since = session
            .meta
            .record_coverage
            .and_then(|coverage| coverage.since);
        assert_eq!(since, Some(if unknown { made } else { newest }));
    }

    #[test]
    fn a_local_cd_is_covered_from_the_newest_item() {
        let turns = two_turns();
        let (mut app, changes) = scripted_app(&turns);
        app.change_factory = Some(factory(&changes));
        let here = PathBuf::from(&app.state.session.cwd);
        app.state.session_mut().meta.record_coverage = Some(RecordCoverage {
            store: workspace_key(&here).unwrap(),
            since: None,
        });
        let there = TempDir::new().unwrap();

        app.install_working_directory(there.path(), PermissionsConfig::default());

        assert_eq!(
            app.state.session.meta.record_coverage,
            covered_from(there.path(), &app.state.session)
        );
    }

    #[test]
    fn a_load_saves_the_coverage_it_stamps() {
        let mut app = test_app();
        app.change_factory = Some(factory(&Arc::default()));
        let cwd = PathBuf::from(&app.state.session.cwd);
        let mut stored = session_in(&cwd, true);
        stored.save(&app.storage).unwrap();

        app.load_session(stored.id);

        let saved = AppSession::load(stored.id, &app.storage).unwrap();
        assert_eq!(saved.meta.record_coverage, covered_from(&cwd, &saved));
    }

    #[test]
    fn a_remote_cd_keeps_the_coverage_of_its_workspace() {
        let workspace = remote_workspace_session();
        let binding = StoredWorkspaceBinding::new_with_cursor(
            workspace.binding().clone(),
            workspace.cursor().clone(),
            None,
        )
        .unwrap();
        let coverage = Some(RecordCoverage {
            store: binding.change_store_key(),
            since: None,
        });
        let mut session = AppSession::new_with_workspace(MODEL, ".", binding.clone());
        session.replace_messages(two_turns().items);
        session.meta.record_coverage = coverage.clone();
        let moved = WorkspaceCursor::new(
            workspace.binding(),
            workspace.cursor().scope().clone(),
            workspace.cursor().generation(),
            CwdHandle::new(NESTED).unwrap(),
        );
        session
            .replace_workspace_cursor(binding.with_cursor(moved.clone()).unwrap())
            .unwrap();
        let workspace = WorkspaceSession::new(
            workspace.workspace().clone(),
            workspace.binding().clone(),
            moved,
        )
        .unwrap();

        session_changes(&mut session, Some(&workspace), None, &recording(true));

        assert_eq!(session.meta.record_coverage, coverage);
    }

    #[test]
    fn a_remote_session_records_on_its_host() {
        let mut session = AppSession::new(MODEL, "");

        let built = session_changes(
            &mut session,
            Some(&remote_workspace_session()),
            None,
            &SnapshotsConfig::default(),
        );

        assert!(built.local.is_none());
        assert_eq!(
            built.recorder.as_ref().map(ChangeRecorder::root),
            Some(Path::new(UNREVEALED_ROOT))
        );
    }

    #[test]
    fn only_the_first_binding_of_a_session_abandons_its_open_records() {
        let mut session = AppSession::new(MODEL, "");

        let first = [(); 2]
            .map(|()| session_changes(&mut session, None, None, &SnapshotsConfig::default()).first);

        assert_eq!(first, [true, false]);
    }

    /// A session whose records live in a real store for its directory.
    struct Recorded {
        app: App,
        service: Arc<dyn WorkspaceChangeService>,
        workspace: PathBuf,
        holder: RecordHolder,
        _state: TempDir,
    }

    impl Recorded {
        fn new() -> Self {
            let mut app = test_app();
            let workspace = PathBuf::from(&app.state.session.cwd);
            let state = TempDir::new().unwrap();
            let service = WorkcellHost::new(&workspace, None)
                .unwrap()
                .change_service(&workspace, &StateDir::from_path(state.path().to_path_buf()));
            app.local_changes = Some(Arc::clone(&service));
            app.state.session_mut().meta.record_coverage = everything();
            let holder = holder_of(app.state.session.id).unwrap();
            Self {
                app,
                service,
                workspace,
                holder,
                _state: state,
            }
        }

        fn write(&self, name: &str, content: &str) {
            fs::write(self.workspace.join(name), content).unwrap();
        }

        fn read(&self, name: &str) -> Option<String> {
            fs::read_to_string(self.workspace.join(name)).ok()
        }

        /// One agent call that runs `change` under a record of `scope`.
        fn record(&self, scope: RecordScope, change: impl FnOnce()) {
            let request = RecordRequest {
                scope,
                holder: self.holder.clone(),
                client: client(CaudraId::generate()),
                limits: LIMITS,
            };
            let ticket = smol::block_on(self.service.begin(&request)).unwrap();
            change();
            smol::block_on(self.service.finish(&ticket))
                .unwrap()
                .expect(CHANGE_RECORDED);
        }

        fn revert_twice(&mut self, target: CaudraId, mode: RestoreMode) {
            for _ in 0..2 {
                self.app.revert_to(target, mode);
            }
        }
    }

    fn paths(names: &[&str]) -> RecordScope {
        RecordScope::Paths(
            names
                .iter()
                .map(|name| WorkspacePath::new(*name).unwrap())
                .collect(),
        )
    }

    fn contents(recorded: &Recorded, names: &[&str]) -> Vec<Option<String>> {
        names.iter().map(|name| recorded.read(name)).collect()
    }

    fn written(texts: &[&str]) -> Vec<Option<String>> {
        texts.iter().map(|text| Some((*text).to_owned())).collect()
    }

    #[test]
    fn a_revert_undoes_only_what_the_agent_recorded() {
        let mut recorded = Recorded::new();
        for name in [CHANGED, OTHER, USERS] {
            recorded.write(name, BEFORE);
        }
        let mut items = Vec::new();
        let prompt = push(&mut items, Message::user(PROMPT.into()));
        recorded.record(paths(&[CHANGED, OTHER]), || {
            recorded.write(CHANGED, AFTER);
            recorded.write(OTHER, AFTER);
        });
        push(&mut items, reply());
        recorded.write(USERS, AFTER);
        recorded.app.state.session_mut().replace_messages(items);

        recorded.revert_twice(prompt, RestoreMode::Both);

        assert_eq!(
            contents(&recorded, &[CHANGED, OTHER, USERS]),
            written(&[BEFORE, BEFORE, AFTER])
        );
        assert_eq!(head(&recorded.app), None);
    }

    #[test]
    fn a_file_the_user_edited_since_conflicts_and_nothing_is_written() {
        let mut recorded = Recorded::new();
        recorded.write(CHANGED, BEFORE);
        let mut items = Vec::new();
        let prompt = push(&mut items, Message::user(PROMPT.into()));
        recorded.record(paths(&[CHANGED]), || recorded.write(CHANGED, AFTER));
        push(&mut items, reply());
        recorded.write(CHANGED, LATER);
        recorded.app.state.session_mut().replace_messages(items);

        recorded.revert_twice(prompt, RestoreMode::Files);

        assert_eq!(contents(&recorded, &[CHANGED]), written(&[LATER]));
        assert!(flash(&recorded.app).starts_with(REVERT_CONFLICTS));
        assert!(recorded.app.state.session.meta.pending_revert.is_none());
    }

    #[test]
    fn a_workspace_record_undoes_only_the_files_it_created_and_deleted() {
        let mut recorded = Recorded::new();
        recorded.write(DELETED, BEFORE);
        recorded.write(OTHER, BEFORE);
        let mut items = Vec::new();
        let prompt = push(&mut items, Message::user(PROMPT.into()));
        recorded.record(RecordScope::Workspace, || {
            recorded.write(CREATED, AFTER);
            fs::remove_file(recorded.workspace.join(DELETED)).unwrap();
        });
        push(&mut items, reply());
        recorded.write(USERS, LATER);
        recorded.app.state.session_mut().replace_messages(items);

        recorded.revert_twice(prompt, RestoreMode::Files);

        assert_eq!(
            contents(&recorded, &[CREATED, DELETED, OTHER, USERS]),
            [
                None,
                Some(BEFORE.to_owned()),
                Some(BEFORE.to_owned()),
                Some(LATER.to_owned())
            ]
        );
    }

    #[test]
    fn a_revert_across_a_compaction_restores_the_state_before_the_original() {
        let mut recorded = Recorded::new();
        recorded.write(CHANGED, BEFORE);
        recorded.write(OTHER, BEFORE);
        let mut items = Vec::new();
        push(&mut items, Message::user(PROMPT.into()));
        recorded.record(paths(&[CHANGED]), || recorded.write(CHANGED, AFTER));
        push(&mut items, reply());
        let second = push(&mut items, Message::user(PROMPT.into()));
        recorded.record(paths(&[CHANGED, OTHER]), || {
            recorded.write(CHANGED, LATER);
            recorded.write(OTHER, LATER);
        });
        push(&mut items, reply());
        let summary = push_after(&mut items, None, reply());
        let original = items.iter().find(|item| item.id == second).unwrap().clone();
        let copy = HistoryItem {
            id: CaudraId::generate(),
            parent_id: Some(summary),
            stands_for: Some(second),
            group_id: CaudraId::generate(),
            ..original
        };
        let copied = copy.id;
        items.push(copy);
        recorded.app.state.session_mut().replace_messages(items);

        recorded.revert_twice(copied, RestoreMode::Files);

        assert_eq!(
            contents(&recorded, &[CHANGED, OTHER]),
            written(&[AFTER, BEFORE])
        );
    }
}
