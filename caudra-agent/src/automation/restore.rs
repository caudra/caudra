//! What a resumed session's runtime takes back from storage before it serves. The startup rules
//! run first: firings the previous process left running end as `interrupted`, and waiting
//! `armed`, `idle`, `needs_input` and `schedule` events are dropped, because resume fires `armed`
//! and schedules catch up. Then come the consumed messages finished firings still hand back, the
//! bindings to arm again, the one-shot events to queue again ahead of `armed` with reason
//! `resume`, the outbox items to return, and the newest firings for the mirror.

use caudra_automation::event::Event;
use caudra_automation::matcher::first_match;
use caudra_automation::meta::{Trigger, TriggerKind};
use caudra_automation::request::AutomationError;
use caudra_automation::snapshot::{FiringSummary, MAX_RECENT_FIRINGS};
use caudra_storage::automation::AutomationRelease;
use tracing::info;

use super::manager::Topics;
use super::outbox::Pending;
use super::store::{AutomationStore, BindingRecord, PendingFiring, QueuedDelivery};

pub const NOT_ARMED: &str = "its automation is not armed after the restart";
pub const NO_MATCH: &str = "no trigger of its automation matches it after the restart";
pub const EVENT_CUT: &str = "its event was too large to keep, so it cannot run again";
pub const RUN_MOVED_ON: &str =
    "its event was too large to keep, and its workflow run is no longer as it settled";
pub const UNREADABLE_EVENT: &str = "its stored event no longer reads as an event";
pub const REQUEST_CUT: &str = "its request was too large to keep, so it cannot be delivered";
pub const UNREADABLE_REQUEST: &str = "its stored request no longer reads as a delivery";

pub(super) struct Restored {
    pub(super) releases: Vec<AutomationRelease>,
    pub(super) bindings: Vec<BindingRecord>,
    pub(super) pending: Vec<PendingFiring>,
    pub(super) outbox: Vec<QueuedDelivery>,
    pub(super) recent: Vec<FiringSummary>,
}

/// Applies the startup rules, then loads what the runtime takes back.
pub(super) async fn load(store: &AutomationStore) -> Result<Restored, AutomationError> {
    let startup = store.interrupt().await?;
    if startup.interrupted > 0 || startup.dropped > 0 || !startup.releases.is_empty() {
        info!(
            interrupted = startup.interrupted,
            dropped = startup.dropped,
            releases = startup.releases.len(),
            "automation firings lost with the previous process"
        );
    }
    Ok(Restored {
        releases: startup.releases,
        bindings: store.load_bindings().await?,
        pending: store.load_pending().await?,
        outbox: store.load_outbox().await?,
        recent: store.load_firings(None, MAX_RECENT_FIRINGS).await?,
    })
}

/// The event a waiting firing queues again with, given the triggers of its automation when it
/// is armed, or why it is dropped instead. A `workflow_finished` event too large to keep comes
/// back as `rebuilt` makes it from the firing's event key, the run and epoch it settled at.
pub(super) fn requeue(
    pending: &PendingFiring,
    triggers: Option<&[Trigger]>,
    rebuilt: impl FnOnce(&str) -> Option<Event>,
) -> Result<Event, &'static str> {
    let triggers = triggers.ok_or(NOT_ARMED)?;
    let event = match (pending.event_cut, pending.firing.trigger) {
        (false, _) => {
            serde_json::from_value(pending.event.clone()).map_err(|_| UNREADABLE_EVENT)?
        }
        (true, TriggerKind::WorkflowFinished) => pending
            .firing
            .event_key
            .as_deref()
            .and_then(rebuilt)
            .ok_or(RUN_MOVED_ON)?,
        (true, _) => return Err(EVENT_CUT),
    };
    first_match(triggers, &event.detail, &Topics).ok_or(NO_MATCH)?;
    Ok(event)
}

/// The outbox item a stored delivery returns as, or why it cannot.
pub(super) fn returned(delivery: &QueuedDelivery) -> Result<Pending, &'static str> {
    let item = &delivery.item;
    Pending::from_journal(
        &item.automation,
        &item.fire_id,
        item.seq,
        &delivery.request,
        item.queued_at,
        item.expires_at,
    )
    .ok_or(if delivery.request_cut {
        REQUEST_CUT
    } else {
        UNREADABLE_REQUEST
    })
}

#[cfg(test)]
mod tests {
    use caudra_automation::event::{
        EventDetail, GoalFinishedDetail, GoalVerdict, SessionStatus, SessionView, WorkView,
        WorkflowFinishedDetail, WorkflowStatus,
    };
    use caudra_automation::host::{ActionKind, ActionRequest, DeliveryMode, MessageRequest};
    use caudra_automation::snapshot::{FiringStatus, OutboxItem};
    use caudra_automation::untrusted::Untrusted;
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::*;

    const AUTOMATION: &str = "goal-chain";
    const FIRE_ID: &str = "fire-1";
    const SESSION: &str = "session-1";
    const CONDITION: &str = "the tests pass";
    const TEXT: &str = "Continue.";
    const AT: i64 = 1_790_000_000;
    const RUN_ID: &str = "run-1";
    const KEY: &str = "run-1:2";
    const WORKFLOW: &str = "review-changes";
    const REPORT: &str = "All green.";
    const REBUILT: &str = "All green, rebuilt.";
    const ONLY_CUT_WORKFLOW_REBUILDS: &str = "only a cut workflow_finished event may be rebuilt";

    fn goal_event() -> Event {
        Event {
            fire_id: FIRE_ID.into(),
            at: AT,
            session: SessionView {
                id: SESSION.into(),
                title: Untrusted::text(SESSION),
                name: None,
                mode: String::new(),
                status: SessionStatus::Idle,
                status_since: AT,
                goal: None,
                cost: None,
                groups: Vec::new(),
                work: WorkView::default(),
            },
            detail: EventDetail::GoalFinished(GoalFinishedDetail {
                verdict: GoalVerdict::Met,
                condition: CONDITION.into(),
                reason: Untrusted::text(CONDITION),
                evaluations: 1,
                duration_s: 1,
                cost: None,
            }),
        }
    }

    fn pending(event: Value, event_cut: bool) -> PendingFiring {
        PendingFiring {
            firing: FiringSummary {
                fire_id: FIRE_ID.into(),
                automation: AUTOMATION.into(),
                digest: String::new(),
                trigger: TriggerKind::GoalFinished,
                trigger_index: 0,
                event_key: None,
                consumed: false,
                status: FiringStatus::Queued,
                reason: None,
                error: None,
                repeats: 1,
                attempts: 0,
                operations: 0,
                state_outcome: None,
                queued_at: AT,
                deferred_until: None,
                started_at: None,
                finished_at: None,
                action_count: 0,
                first_action: None,
            },
            event,
            event_cut,
        }
    }

    fn workflow_event(report: &str) -> Event {
        Event {
            detail: EventDetail::WorkflowFinished(WorkflowFinishedDetail {
                run_id: RUN_ID.into(),
                name: WORKFLOW.into(),
                workflow: WORKFLOW.into(),
                status: WorkflowStatus::Completed,
                report: Some(Untrusted::text(report)),
                result: None,
                error: None,
                scratch_dir: None,
                agents: 0,
                tokens: 0,
            }),
            ..goal_event()
        }
    }

    /// A waiting `workflow_finished` firing keyed `key`, whose event is `event`.
    fn workflow_pending(event: Value, event_cut: bool, key: Option<&str>) -> PendingFiring {
        let mut waiting = pending(event, event_cut);
        waiting.firing.trigger = TriggerKind::WorkflowFinished;
        waiting.firing.event_key = key.map(str::to_owned);
        waiting
    }

    fn goal_triggers(verdicts: Vec<GoalVerdict>) -> Vec<Trigger> {
        vec![Trigger::Armed, Trigger::GoalFinished { verdicts }]
    }

    fn workflow_triggers() -> Vec<Trigger> {
        vec![Trigger::WorkflowFinished {
            workflows: Vec::new(),
            statuses: vec![WorkflowStatus::Completed],
        }]
    }

    /// Rebuilds [`workflow_event`] with [`REBUILT`] for [`KEY`] alone.
    fn rebuilt(key: &str) -> Option<Event> {
        (key == KEY).then(|| workflow_event(REBUILT))
    }

    fn never_rebuilt(_: &str) -> Option<Event> {
        panic!("{ONLY_CUT_WORKFLOW_REBUILDS}");
    }

    #[test]
    fn a_matching_one_shot_event_queues_again_as_it_was() {
        let event = goal_event();
        let triggers = goal_triggers(vec![GoalVerdict::Met]);

        let requeued = requeue(
            &pending(event.to_tagged(), false),
            Some(&triggers),
            never_rebuilt,
        );

        assert_eq!(requeued, Ok(event));
    }

    #[test]
    fn an_uncut_workflow_event_queues_again_as_it_was() {
        let event = workflow_event(REPORT);

        let requeued = requeue(
            &workflow_pending(event.to_tagged(), false, Some(KEY)),
            Some(&workflow_triggers()),
            never_rebuilt,
        );

        assert_eq!(requeued, Ok(event));
    }

    #[test]
    fn a_cut_workflow_event_is_rebuilt_from_its_key() {
        let requeued = requeue(
            &workflow_pending(json!(REPORT), true, Some(KEY)),
            Some(&workflow_triggers()),
            rebuilt,
        );

        assert_eq!(requeued, Ok(workflow_event(REBUILT)));
    }

    #[test_case(Some("run-1:3"); "its_run_moved_on")]
    #[test_case(None; "it_has_no_key")]
    fn a_cut_workflow_event_that_cannot_be_rebuilt_is_dropped(key: Option<&str>) {
        let requeued = requeue(
            &workflow_pending(json!(REPORT), true, key),
            Some(&workflow_triggers()),
            rebuilt,
        );

        assert_eq!(requeued, Err(RUN_MOVED_ON));
    }

    #[test_case(pending(goal_event().to_tagged(), false), None, NOT_ARMED; "not_armed")]
    #[test_case(pending(goal_event().to_tagged(), false), Some(goal_triggers(vec![GoalVerdict::Impossible])), NO_MATCH; "no_trigger_matches")]
    #[test_case(pending(json!(CONDITION), true), Some(goal_triggers(vec![GoalVerdict::Met])), EVENT_CUT; "event_cut")]
    #[test_case(pending(json!({ "trigger": "armed" }), false), Some(goal_triggers(vec![GoalVerdict::Met])), UNREADABLE_EVENT; "unreadable")]
    fn a_waiting_event_that_cannot_run_again_is_dropped(
        pending: PendingFiring,
        triggers: Option<Vec<Trigger>>,
        reason: &str,
    ) {
        assert_eq!(
            requeue(&pending, triggers.as_deref(), never_rebuilt),
            Err(reason)
        );
    }

    #[test_case(false, Ok(TEXT); "a_stored_message_returns")]
    #[test_case(true, Err(REQUEST_CUT); "a_cut_request_cannot")]
    fn a_stored_delivery_returns_to_the_outbox(cut: bool, expected: Result<&str, &str>) {
        let request = ActionRequest::Message(MessageRequest {
            text: TEXT.into(),
            attach: None,
            delivery: DeliveryMode::Next,
            expires: None,
        })
        .to_journal();
        let delivery = QueuedDelivery {
            item: OutboxItem {
                automation: AUTOMATION.into(),
                fire_id: FIRE_ID.into(),
                seq: 0,
                kind: ActionKind::Message,
                summary: TEXT.into(),
                delivery: DeliveryMode::Next,
                queued_at: AT,
                expires_at: None,
                wait: None,
            },
            request: if cut { json!(TEXT) } else { request },
            request_cut: cut,
        };

        let returned = returned(&delivery).map(|pending| pending.item.summary);

        assert_eq!(returned.as_deref().map_err(|reason| *reason), expected);
    }
}
