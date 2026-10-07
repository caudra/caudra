//! Dry runs: a finished firing's event run again against the script on disk now, the session's
//! args and a copy of its automation's state, with `now()` pinned to the firing's time. The actor
//! gathers what one needs; the run goes off the actor, one at a time per runtime, performs and
//! stores nothing, and answers with a trace that renders like a real firing's, or `Unavailable`
//! once the runtime stopped.

use std::slice;
use std::sync::Arc;

use async_lock::Semaphore;
use caudra_automation::catalog::CatalogEntry;
use caudra_automation::engine::FiringLimits;
use caudra_automation::event::{Event, EventDetail};
use caudra_automation::host::{ActionRequest, HttpRequest, HttpTarget, request_hash};
use caudra_automation::limits::LimitRefusal;
use caudra_automation::matcher::first_match;
use caudra_automation::meta::Trigger;
use caudra_automation::replay::{
    Answer, DRY_RUN_ID, DryAction, DryRun, JournalEntry, JournalResult, dry_run,
};
use caudra_automation::request::{
    AutomationError, AutomationResponse, REPLAY_EVENT_CUT, REPLAY_EVENT_UNREADABLE,
    REPLAY_NO_TRIGGER, REPLAY_NOT_FINISHED,
};
use caudra_automation::snapshot::{
    ActionRow, ActionStatus, DryRunDetail, FiringDetail, FiringStatus, FiringSummary,
    request_summary,
};
use serde_json::{Map, Value};
use url::Url;

use super::handle::Reply;
use super::manager::{
    Disposition, Topics, delivery_terms, disposition, end_error, end_reason, trigger_index,
};
use super::messaging::Outgoing;
use super::store::{error_view, source_line};
use crate::cancel::{CancelToken, CancelTrigger};

/// Dry runs that run at once in one session; later ones wait their turn off the actor.
const MAX_DRY_RUNS: usize = 1;

/// The dry runs of one runtime. One runs at a time, and once the runtime drops this, however it
/// stops, each one still running or waiting answers `Unavailable` at once, so no caller waits for
/// it. A running one's engine thread runs on to its limits unwatched.
pub(super) struct DryRuns {
    pub(super) turns: Arc<Semaphore>,
    stopped: CancelToken,
    _stop: CancelTrigger,
}

/// A finished firing to run again, with everything the run needs.
pub(super) struct DryRunJob {
    /// The firing it replays, as this session's journal keeps it.
    pub(super) replayed: FiringDetail,
    pub(super) event: Event,
    /// The script on disk now, and its source.
    pub(super) entry: CatalogEntry,
    pub(super) source: String,
    /// The entry of the script's triggers the event runs under.
    pub(super) trigger_index: u32,
    pub(super) args: Map<String, Value>,
    /// A copy of the automation's state, and its revision.
    pub(super) state: Value,
    pub(super) revision: u64,
    pub(super) journal: Vec<JournalEntry>,
    /// What the automation's limits answer now.
    pub(super) admission: Result<(), LimitRefusal>,
}

/// The event of a finished firing, or why it cannot run again.
pub(super) fn replayed_event(replayed: &FiringDetail) -> Result<Event, AutomationError> {
    let fire_id = &replayed.firing.fire_id;
    if replayed.firing.status.is_pending() {
        return Err(not_replayable(fire_id, REPLAY_NOT_FINISHED));
    }
    if replayed.event_cut {
        return Err(not_replayable(fire_id, REPLAY_EVENT_CUT));
    }
    serde_json::from_value(replayed.event.clone())
        .map_err(|_| not_replayable(fire_id, REPLAY_EVENT_UNREADABLE))
}

/// The entry of the current script's `triggers` the event of `fire_id`, which ran under entry
/// `stored`, runs under now, as the runtime routes a real event. The scheduler, `idle` and
/// `needs_input` fire an entry by its index, so such an event keeps its entry while that entry
/// still takes it; every other event goes to the first entry it matches.
pub(super) fn replayed_trigger(
    fire_id: &str,
    stored: u32,
    triggers: &[Trigger],
    detail: &EventDetail,
) -> Result<u32, AutomationError> {
    usize::try_from(stored)
        .ok()
        .filter(|&index| {
            triggers
                .get(index)
                .is_some_and(|trigger| keeps_its_entry(trigger, detail))
        })
        .or_else(|| first_match(triggers, detail, &Topics))
        .map(trigger_index)
        .ok_or_else(|| not_replayable(fire_id, REPLAY_NO_TRIGGER))
}

impl DryRuns {
    pub(super) fn new() -> Self {
        let (stop, stopped) = CancelToken::new();
        Self {
            turns: Arc::new(Semaphore::new(MAX_DRY_RUNS)),
            stopped,
            _stop: stop,
        }
    }
}

impl DryRunJob {
    /// Runs once no other dry run of `runs` runs, off the actor, and answers `reply`. Nothing
    /// here belongs to the actor or the store, so shutdown never waits for it, and an answer
    /// nobody waits for any more is dropped.
    pub(super) fn spawn(self, runs: &DryRuns, reply: Reply<AutomationResponse>) {
        let turns = Arc::clone(&runs.turns);
        let run = async move {
            let _turn = turns.acquire_arc().await;
            smol::unblock(move || self.run()).await
        };
        let stopped = runs.stopped.clone();
        smol::spawn(async move {
            let answer = stopped
                .race(run)
                .await
                .map(|detail| AutomationResponse::DryRun(Box::new(detail)))
                .map_err(|_| AutomationError::Unavailable);
            let _ = reply.send(answer);
        })
        .detach();
    }

    fn run(self) -> DryRunDetail {
        let Self {
            replayed,
            event,
            entry,
            source,
            trigger_index,
            args,
            state,
            revision,
            journal,
            admission,
        } = self;
        let original = replayed.firing;
        let at = original.started_at.unwrap_or(original.queued_at);
        let report = dry_run(DryRun {
            source: &source,
            meta: &entry.meta,
            event: &event,
            state: &state,
            args: &args,
            limits: &FiringLimits::default(),
            now_ms: at,
            journal: &journal,
            admission,
        });
        let end = &report.outcome.end;
        let error = end_error(end);
        let error_source = error
            .as_ref()
            .and_then(|error| error.line)
            .and_then(|line| source_line(&source, line));
        let status = match disposition(end, original.trigger) {
            Disposition::Finish(status) => status,
            Disposition::Defer(_) => FiringStatus::Deferred,
        };
        let actions = action_rows(&report.actions, &journal, &event, &original.automation, at);
        let firing = FiringSummary {
            fire_id: DRY_RUN_ID.to_owned(),
            automation: original.automation,
            digest: entry.digest,
            trigger: original.trigger,
            trigger_index,
            event_key: original.event_key,
            consumed: original.consumed,
            status,
            reason: end_reason(end),
            error: error.map(error_view),
            repeats: 1,
            attempts: 0,
            operations: report.outcome.operations,
            state_outcome: None,
            queued_at: at,
            deferred_until: None,
            started_at: Some(at),
            finished_at: Some(at),
            action_count: u64::try_from(report.actions.len()).unwrap_or(u64::MAX),
            first_action: report.actions.first().map(|action| action.request.kind()),
        };
        DryRunDetail {
            fire_id: original.fire_id,
            trace: FiringDetail {
                firing,
                event: replayed.event,
                event_cut: false,
                state_patch: report.outcome.state.map(|change| change.patch),
                patch_cut: false,
                actions,
                error_source,
            },
            answers: report.actions.iter().map(|action| action.answer).collect(),
            limited: report.limited,
            state_revision: revision,
        }
    }
}

/// Whether `trigger`, the entry an event the runtime fires by index ran under, still takes it.
fn keeps_its_entry(trigger: &Trigger, detail: &EventDetail) -> bool {
    match detail {
        EventDetail::Schedule { .. } => matches!(trigger, Trigger::Schedule(_)),
        EventDetail::Idle(_) | EventDetail::NeedsInput { .. } => {
            first_match(slice::from_ref(trigger), detail, &Topics).is_some()
        }
        EventDetail::Armed { .. }
        | EventDetail::GoalFinished(_)
        | EventDetail::MessageReceived(_)
        | EventDetail::WorkFinished(_)
        | EventDetail::WorkflowFinished(_) => false,
    }
}

/// The trace's rows, as a real firing's journal shows its actions, every one answered at `at`:
/// a request the journal answered with a failure failed with it, and every other one is done.
fn action_rows(
    actions: &[DryAction],
    journal: &[JournalEntry],
    event: &Event,
    automation: &str,
    at: i64,
) -> Vec<ActionRow> {
    let mut unmatched: Vec<&JournalEntry> = journal.iter().collect();
    actions
        .iter()
        .map(|action| {
            let request = action.request.to_journal();
            let error = match action.answer {
                Answer::Journal | Answer::Cut => journaled_failure(&mut unmatched, &request),
                Answer::Recorded | Answer::Stubbed => None,
            };
            let seq = u64::from(action.site.seq);
            let (delivery, expires_at) = delivery_terms(&action.request, at);
            ActionRow {
                seq,
                kind: action.request.kind(),
                line: action.site.line,
                column: action.site.column,
                status: if error.is_some() {
                    ActionStatus::Failed
                } else {
                    ActionStatus::Done
                },
                summary: request_summary(&request),
                error,
                target: target(&action.request, event, automation, seq),
                delivery,
                expires_at,
                wait: None,
                turn_outcome: None,
                turn_cost: None,
                started_at: at,
                finished_at: Some(at),
                delivered_at: None,
                request_cut: false,
                result_cut: false,
            }
        })
        .collect()
}

/// The failure the journal answered `request` with, taking the entry the dry run took: the
/// first one left with the same request hash.
fn journaled_failure(unmatched: &mut Vec<&JournalEntry>, request: &Value) -> Option<String> {
    let hash = request_hash(request);
    let index = unmatched
        .iter()
        .position(|entry| entry.request_hash == hash)?;
    match &unmatched.remove(index).result {
        JournalResult::Failure(failure) => Some(failure.to_string()),
        JournalResult::Response(_) | JournalResult::Cut => None,
    }
}

/// What a real row names as the action's target, with the placeholder for a run id: the run,
/// the recipient or topic of a message, or the origin of an `http` request to a URL the script
/// gave. A `url_env` origin stays unread.
fn target(request: &ActionRequest, event: &Event, automation: &str, seq: u64) -> Option<String> {
    match request {
        ActionRequest::StartWorkflow(_) => Some(DRY_RUN_ID.to_owned()),
        ActionRequest::Http(HttpRequest {
            target: HttpTarget::Url(url),
            ..
        }) => Url::parse(url)
            .ok()
            .map(|url| url.origin().ascii_serialization()),
        ActionRequest::Reply { .. }
        | ActionRequest::Send(_)
        | ActionRequest::Publish { .. }
        | ActionRequest::Broadcast { .. } => {
            Outgoing::new(request, event, automation, DRY_RUN_ID, seq)
                .ok()?
                .target()
        }
        ActionRequest::Message(_)
        | ActionRequest::SetGoal(_)
        | ActionRequest::Notify { .. }
        | ActionRequest::Http(_)
        | ActionRequest::Pause { .. }
        | ActionRequest::Log { .. } => None,
    }
}

fn not_replayable(fire_id: &str, reason: &str) -> AutomationError {
    AutomationError::NotReplayable {
        fire_id: fire_id.to_owned(),
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use caudra_automation::event::{ArmedReason, InputKind};
    use caudra_automation::meta::{Cadence, CatchUp, Schedule};
    use test_case::test_case;

    use super::*;

    const FIRE_ID: &str = "fire-1";
    const DELAY: Duration = Duration::from_secs(60);
    const PERIOD: Duration = Duration::from_secs(60 * 60);

    fn armed() -> EventDetail {
        EventDetail::Armed {
            reason: ArmedReason::Launch,
        }
    }

    fn scheduled() -> EventDetail {
        EventDetail::Schedule {
            scheduled_for: 0,
            late_by_s: 0,
        }
    }

    fn permission() -> EventDetail {
        EventDetail::NeedsInput {
            input: InputKind::Permission,
            tool: None,
            waiting_s: 0,
        }
    }

    fn idle() -> Trigger {
        Trigger::Idle {
            delay: Duration::ZERO,
        }
    }

    fn hourly() -> Trigger {
        Trigger::Schedule(Schedule {
            cadence: Cadence::Every(PERIOD),
            catch_up: CatchUp::Skip,
        })
    }

    fn needs_permission(delay: Duration) -> Trigger {
        Trigger::NeedsInput {
            delay,
            inputs: vec![InputKind::Permission],
        }
    }

    fn no_trigger() -> AutomationError {
        not_replayable(FIRE_ID, REPLAY_NO_TRIGGER)
    }

    #[test_case(armed(), 0, vec![idle(), Trigger::Armed] => Ok(1); "an_armed_event_goes_to_the_first_entry_it_matches")]
    #[test_case(scheduled(), 1, vec![Trigger::Armed, hourly()] => Ok(1); "a_schedule_event_keeps_its_entry")]
    #[test_case(scheduled(), 0, vec![Trigger::Armed, hourly()] => Err(no_trigger()); "a_schedule_event_whose_entry_is_no_schedule_has_none")]
    #[test_case(permission(), 1, vec![needs_permission(Duration::ZERO), needs_permission(DELAY)] => Ok(1); "a_delayed_needs_input_event_keeps_its_entry")]
    #[test_case(permission(), 1, vec![needs_permission(DELAY), Trigger::Armed] => Ok(0); "a_needs_input_event_whose_entry_changed_goes_to_the_first_match")]
    #[test_case(armed(), 0, vec![idle()] => Err(no_trigger()); "an_event_no_entry_takes_has_none")]
    fn a_replayed_event_runs_under_the_entry_the_runtime_routes_it_to(
        detail: EventDetail,
        stored: u32,
        triggers: Vec<Trigger>,
    ) -> Result<u32, AutomationError> {
        replayed_trigger(FIRE_ID, stored, &triggers, &detail)
    }
}
