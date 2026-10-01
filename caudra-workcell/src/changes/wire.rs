//! Change records as the Workcell host contract spells them, for the engine
//! run in process and for a remote host alike.

use caudra_workspace::{
    CleanupPreview, CleanupSummary, HolderPage, HolderSummary, OpenRecord, PendingRevert,
    RecordHolder, RecordLimits, RecordListing, RecordPage, RecordRequest, RecordScope, RecordState,
    RecordSummary, RecordTicket, ReleaseSelection, ReleaseSummary, RevertChangeKind,
    RevertConflict, RevertConflictKind, RevertCounts, RevertDirection, RevertId, RevertPath,
    RevertPreview, RevertState, RevertStatus, UnrecordedReason, WorkspaceError, WorkspacePath,
};
use workcell::host_contract as contract;

use crate::remote::{invalid_response, workspace_path};

/// The code a remote host refuses a change-record request with, so a refusal
/// reads the same whichever side made it.
pub(crate) const REFUSAL_RPC_CODE: i64 = -32_602;
const INVALID_REQUEST_CODE: &str = "invalid_request";

/// `directory` is the session directory relative to the root the host
/// records, and the scope's paths are relative to it. `accepted` holds the
/// largest limits the store records within.
pub(crate) fn record_request(
    request: &RecordRequest,
    directory: &WorkspacePath,
    accepted: &contract::WorkspaceChangesLimits,
) -> Result<contract::RecordRequest, WorkspaceError> {
    let scope = match &request.scope {
        RecordScope::Paths(paths) => contract::RecordScope::Paths {
            paths: paths
                .iter()
                .map(|path| beneath(directory, path))
                .collect::<Result<_, _>>()?,
        },
        RecordScope::Workspace => contract::RecordScope::Workspace {
            directory: beneath(directory, &WorkspacePath::root())?,
        },
    };
    Ok(contract::RecordRequest {
        scope,
        holder: holder(&request.holder)?,
        client: contract::RecordClientMetadata::new(request.client.clone())
            .map_err(|_| invalid_request())?,
        limits: record_limits(&request.limits, accepted),
    })
}

/// `limits`, each lowered to what the store accepts, as a store refuses a
/// record with any limit above its own. None is raised.
fn record_limits(
    limits: &RecordLimits,
    accepted: &contract::WorkspaceChangesLimits,
) -> contract::RecordLimits {
    contract::RecordLimits {
        max_files: limits.max_files.min(accepted.max_files),
        max_file_bytes: limits.max_file_bytes.min(accepted.max_file_bytes),
        max_total_bytes: limits.max_total_bytes.min(accepted.max_total_bytes),
    }
}

pub(crate) fn holder(holder: &RecordHolder) -> Result<contract::RecordHolder, WorkspaceError> {
    contract::RecordHolder::new(holder.as_str()).map_err(|_| invalid_request())
}

pub(crate) fn ticket(ticket: &RecordTicket) -> Result<contract::Identifier, WorkspaceError> {
    contract::Identifier::new(ticket.as_str()).map_err(|_| invalid_request())
}

pub(crate) fn release_selection(selection: &ReleaseSelection) -> contract::ReleaseSelection {
    match selection {
        ReleaseSelection::All => contract::ReleaseSelection::All,
        ReleaseSelection::Seqs(seqs) => contract::ReleaseSelection::Seqs(seqs.clone()),
    }
}

/// A request the contract cannot spell, refused as a host refuses one it
/// cannot accept.
fn invalid_request() -> WorkspaceError {
    WorkspaceError::Refused {
        code: REFUSAL_RPC_CODE,
        symbolic: INVALID_REQUEST_CODE.to_owned(),
    }
}

fn beneath(
    directory: &WorkspacePath,
    path: &WorkspacePath,
) -> Result<contract::WorkspacePath, WorkspaceError> {
    let joined = match (directory.is_root(), path.is_root()) {
        (true, _) => path.as_str().to_owned(),
        (false, true) => directory.as_str().to_owned(),
        (false, false) => format!("{}/{}", directory.as_str(), path.as_str()),
    };
    contract::WorkspacePath::new(joined).map_err(|_| invalid_request())
}

pub(crate) fn record_ticket(ticket: &contract::Identifier) -> Result<RecordTicket, WorkspaceError> {
    RecordTicket::new(ticket.as_str()).map_err(|_| invalid_response())
}

pub(crate) fn record_holder(
    holder: &contract::RecordHolder,
) -> Result<RecordHolder, WorkspaceError> {
    RecordHolder::new(holder.as_str()).map_err(|_| invalid_response())
}

pub(crate) fn record_summary(summary: contract::RecordSummary) -> RecordSummary {
    RecordSummary {
        seq: summary.seq,
        paths: summary.paths,
        unrecorded: summary.unrecorded,
    }
}

pub(crate) fn record_page(page: contract::RecordPage) -> RecordPage {
    RecordPage {
        records: page
            .records
            .into_iter()
            .map(|record| RecordListing {
                seq: record.seq,
                client: record.client.as_value().clone(),
                state: match record.state {
                    contract::RecordState::Applied => RecordState::Applied,
                    contract::RecordState::Reverted => RecordState::Reverted,
                },
                paths: record.paths,
                unrecorded: record.unrecorded,
            })
            .collect(),
        next_after_seq: page.next_after_seq,
        evicted_through: page.evicted_through.map(|client| client.as_value().clone()),
    }
}

pub(crate) fn open_record(record: contract::OpenRecord) -> Result<OpenRecord, WorkspaceError> {
    Ok(OpenRecord {
        ticket: record_ticket(&record.ticket)?,
        client: record.client.as_value().clone(),
        opened_at_unix_ms: record.opened_at_unix_ms,
    })
}

pub(crate) fn holder_summary(
    summary: contract::HolderSummary,
) -> Result<HolderSummary, WorkspaceError> {
    Ok(HolderSummary {
        holder: record_holder(&summary.holder)?,
        records: summary.records,
        open_records: summary.open_records,
        pending_reverts: summary.pending_reverts,
    })
}

pub(crate) fn holder_page(
    holders: Vec<contract::HolderSummary>,
    next_after: Option<contract::RecordHolder>,
) -> Result<HolderPage, WorkspaceError> {
    Ok(HolderPage {
        holders: holders
            .into_iter()
            .map(holder_summary)
            .collect::<Result<_, _>>()?,
        next_after: next_after.as_ref().map(record_holder).transpose()?,
    })
}

pub(crate) fn release_summary(summary: contract::ReleaseSummary) -> ReleaseSummary {
    ReleaseSummary {
        released: summary.released,
        deleted: summary.deleted,
    }
}

pub(crate) fn revert_preview(
    preview: contract::RevertPreview,
) -> Result<RevertPreview, WorkspaceError> {
    let counts = preview.counts;
    Ok(RevertPreview {
        revert_id: revert_id(&preview.revert_id)?,
        direction: revert_direction(preview.direction),
        records: preview.records,
        counts: RevertCounts {
            create: counts.create,
            replace: counts.replace,
            delete: counts.delete,
            unchanged: counts.unchanged,
            conflicts: counts.conflicts,
            created_directories: counts.created_directories,
        },
        planned: preview
            .planned
            .iter()
            .map(|planned| {
                Ok(RevertPath {
                    path: workspace_path(&planned.path)?,
                    kind: match planned.kind {
                        contract::RevertChangeKind::Create => RevertChangeKind::Create,
                        contract::RevertChangeKind::Replace => RevertChangeKind::Replace,
                        contract::RevertChangeKind::Delete => RevertChangeKind::Delete,
                    },
                })
            })
            .collect::<Result<_, WorkspaceError>>()?,
        conflicts: preview
            .conflicts
            .iter()
            .map(|conflict| {
                Ok(RevertConflict {
                    path: workspace_path(&conflict.path)?,
                    kind: match conflict.kind {
                        contract::RevertConflictKind::ChangedSince => {
                            RevertConflictKind::ChangedSince
                        }
                        contract::RevertConflictKind::Interleaved => {
                            RevertConflictKind::Interleaved
                        }
                        contract::RevertConflictKind::Unrecorded => RevertConflictKind::Unrecorded,
                    },
                    reason: conflict.reason.map(unrecorded_reason),
                })
            })
            .collect::<Result<_, WorkspaceError>>()?,
        created_directories: preview
            .created_directories
            .iter()
            .map(workspace_path)
            .collect::<Result<_, _>>()?,
    })
}

pub(crate) fn revert_status(
    status: contract::RevertStatus,
) -> Result<RevertStatus, WorkspaceError> {
    Ok(RevertStatus {
        pending: status
            .pending
            .iter()
            .map(pending_revert)
            .collect::<Result<_, _>>()?,
    })
}

pub(crate) fn cleanup_preview(preview: contract::CleanupPreview) -> CleanupPreview {
    CleanupPreview {
        stale_open_records: preview.stale_open_records,
        evicted_records: preview.evicted_records,
        reclaimable_bytes: preview.reclaimable_bytes,
    }
}

pub(crate) fn cleanup_summary(summary: contract::CleanupSummary) -> CleanupSummary {
    CleanupSummary {
        abandoned_open_records: summary.abandoned_open_records,
        evicted_records: summary.evicted_records,
        deleted_objects: summary.deleted_objects,
        reclaimed_bytes: summary.reclaimed_bytes,
    }
}

fn pending_revert(pending: &contract::PendingRevert) -> Result<PendingRevert, WorkspaceError> {
    Ok(PendingRevert {
        revert_id: revert_id(&pending.revert_id)?,
        direction: revert_direction(pending.direction),
        state: match pending.state {
            contract::RevertState::Publishing => RevertState::Publishing,
            contract::RevertState::Completed => RevertState::Completed,
            contract::RevertState::Partial => RevertState::Partial,
            contract::RevertState::Indeterminate => RevertState::Indeterminate,
        },
        records: pending.records,
        applied_files: pending.applied_files,
        total_files: pending.total_files,
        reconciliation_required: pending.reconciliation_required,
        stopped_at: pending
            .stopped_at
            .as_ref()
            .map(workspace_path)
            .transpose()?,
    })
}

fn revert_id(id: &contract::Identifier) -> Result<RevertId, WorkspaceError> {
    RevertId::new(id.as_str()).map_err(|_| invalid_response())
}

fn revert_direction(direction: contract::RevertDirection) -> RevertDirection {
    match direction {
        contract::RevertDirection::Revert => RevertDirection::Revert,
        contract::RevertDirection::Unrevert => RevertDirection::Unrevert,
    }
}

fn unrecorded_reason(reason: contract::UnrecordedReason) -> UnrecordedReason {
    match reason {
        contract::UnrecordedReason::Oversized => UnrecordedReason::Oversized,
        contract::UnrecordedReason::Unstable => UnrecordedReason::Unstable,
        contract::UnrecordedReason::Unreadable => UnrecordedReason::Unreadable,
        contract::UnrecordedReason::Blocked => UnrecordedReason::Blocked,
        contract::UnrecordedReason::Special => UnrecordedReason::Special,
        contract::UnrecordedReason::Interleaved => UnrecordedReason::Interleaved,
    }
}
