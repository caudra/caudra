//! The published read model: how a stored row reads as a snapshot, and how
//! one run's change lands in the session-wide [`WorkflowState`].

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use caudra_storage::workflow::{
    WorkflowEventKind, WorkflowEventRow, WorkflowRunRow, WorkflowRunStatus, WorkflowSourceKind,
};
use caudra_workflow::{
    AgentRosterEntry, LogLine, MAX_PHASE_HISTORY, MAX_RUN_LOG_ENTRIES, PhaseRecord, RunEvent,
    RunEventKind, RunSnapshot, RunStatus, RunUsage, SourceKind, WorkflowState, parse_meta,
};
use tracing::warn;

pub(super) type Published = Arc<ArcSwap<WorkflowState>>;

pub(super) fn snapshot_from_row(row: &WorkflowRunRow) -> RunSnapshot {
    let roster: Vec<AgentRosterEntry> = serde_json::from_str(&row.roster).unwrap_or_else(|error| {
        warn!(run_id = %row.run_id, %error, "workflow roster column is unreadable");
        Vec::new()
    });
    let usage: RunUsage = serde_json::from_str(&row.usage).unwrap_or_else(|error| {
        warn!(run_id = %row.run_id, %error, "workflow usage column is unreadable");
        RunUsage::default()
    });
    let result = row.result.as_deref().and_then(|result| {
        serde_json::from_str(result)
            .map_err(|error| warn!(run_id = %row.run_id, %error, "workflow result is unreadable"))
            .ok()
    });
    RunSnapshot {
        run_id: row.run_id.clone(),
        display_name: row.display_name.clone(),
        workflow_name: row.workflow_name.clone(),
        source_kind: source_kind(row.source_kind),
        objective: row.objective.clone(),
        status: run_status(row.status),
        pause_kind: row.pause_kind.clone(),
        pause_message: row.pause_message.clone(),
        revision: row.revision,
        execution_epoch: row.execution_epoch,
        phase: row.phase.clone(),
        phases: parse_meta(&row.source)
            .map(|meta| meta.phases.into_iter().map(|phase| phase.title).collect())
            .unwrap_or_default(),
        agent_budget: u32::try_from(row.agent_budget).unwrap_or(u32::MAX),
        usage,
        roster,
        result,
        error: row.error.clone(),
        phase_history: Vec::new(),
        logs: Vec::new(),
        outbox_pending: row.outbox_pending,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

pub(super) fn run_status(status: WorkflowRunStatus) -> RunStatus {
    match status {
        WorkflowRunStatus::Active => RunStatus::Active,
        WorkflowRunStatus::Paused => RunStatus::Paused,
        WorkflowRunStatus::BudgetLimited => RunStatus::BudgetLimited,
        WorkflowRunStatus::Interrupted => RunStatus::Interrupted,
        WorkflowRunStatus::Completed => RunStatus::Completed,
        WorkflowRunStatus::Cancelled => RunStatus::Cancelled,
        WorkflowRunStatus::Failed => RunStatus::Failed,
    }
}

pub(super) fn stored_status(status: RunStatus) -> WorkflowRunStatus {
    match status {
        RunStatus::Active => WorkflowRunStatus::Active,
        RunStatus::Paused => WorkflowRunStatus::Paused,
        RunStatus::BudgetLimited => WorkflowRunStatus::BudgetLimited,
        RunStatus::Interrupted => WorkflowRunStatus::Interrupted,
        RunStatus::Completed => WorkflowRunStatus::Completed,
        RunStatus::Cancelled => WorkflowRunStatus::Cancelled,
        RunStatus::Failed => WorkflowRunStatus::Failed,
    }
}

fn source_kind(kind: WorkflowSourceKind) -> SourceKind {
    match kind {
        WorkflowSourceKind::Builtin => SourceKind::Builtin,
        WorkflowSourceKind::Project => SourceKind::Project,
        WorkflowSourceKind::User => SourceKind::User,
    }
}

pub(super) fn stored_source_kind(kind: SourceKind) -> WorkflowSourceKind {
    match kind {
        SourceKind::Builtin => WorkflowSourceKind::Builtin,
        SourceKind::Project => WorkflowSourceKind::Project,
        SourceKind::User => WorkflowSourceKind::User,
    }
}

/// Fills a snapshot's timeline from its stored events, keeping the newest
/// of each kind within the snapshot's own bounds.
pub(super) fn restore_timeline(snapshot: &mut RunSnapshot, events: &[WorkflowEventRow]) {
    snapshot.phase_history = events
        .iter()
        .filter(|event| event.kind == WorkflowEventKind::Phase)
        .map(|event| PhaseRecord {
            title: event.text.clone(),
            started_at: event.at,
        })
        .collect();
    let excess = snapshot
        .phase_history
        .len()
        .saturating_sub(MAX_PHASE_HISTORY);
    snapshot.phase_history.drain(..excess);
    snapshot.logs = events
        .iter()
        .filter(|event| event.kind == WorkflowEventKind::Log)
        .map(|event| LogLine {
            at: event.at,
            message: event.text.clone(),
        })
        .collect();
    let excess = snapshot.logs.len().saturating_sub(MAX_RUN_LOG_ENTRIES);
    snapshot.logs.drain(..excess);
}

pub(super) fn run_event(row: &WorkflowEventRow) -> RunEvent {
    RunEvent {
        seq: row.seq,
        at: row.at,
        kind: match row.kind {
            WorkflowEventKind::Phase => RunEventKind::Phase,
            WorkflowEventKind::Log => RunEventKind::Log,
        },
        text: row.text.clone(),
    }
}

/// Replaces the run's entry, or files a new run at the front so the model
/// stays newest-first like the store's own listing.
pub(super) fn publish(published: &Published, snapshot: &RunSnapshot) {
    published.rcu(|state| {
        let mut runs = state.runs.clone();
        match runs.iter().position(|run| run.run_id == snapshot.run_id) {
            Some(index) => runs[index] = snapshot.clone(),
            None => runs.insert(0, snapshot.clone()),
        }
        WorkflowState { runs }
    });
}

pub(super) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
