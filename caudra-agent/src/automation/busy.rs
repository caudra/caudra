//! The busy-period tracker: what the session did between two settles, as an `idle` event reports
//! it. The frontend feeds it run starts, the automation and peer messages runs took, the outcomes
//! of the session's group work and run ends, and takes the period's [`IdleDetail`] when the
//! session settles.

use caudra_automation::event::{
    Audience, ErrorKind, IdleDetail, MAX_LAST_RESPONSE_BYTES, PauseReason, SenderKind, StartedBy,
    TurnOutcome, WorkReport, cap_text,
};
use caudra_automation::untrusted::Untrusted;
use caudra_providers::{
    AutomationEventOrigin, PEER_AUTOMATION_SENDER, PEER_SCRIPT_SENDER, PeerAudience,
    PeerMessageOrigin,
};
use tracing::debug;

use crate::peers::{WorkPause, WorkReported};

const MILLIS_PER_SECOND: i64 = 1_000;
/// A settle with no run end recorded reads as cancelled, so no automation takes it for success.
const UNFINISHED_OUTCOME: TurnOutcome = TurnOutcome::Cancelled;

#[derive(Debug, Default)]
pub struct BusyTracker {
    period: Option<Period>,
}

#[derive(Debug)]
struct Period {
    started_at: i64,
    started_by: StartedBy,
    automations: Vec<String>,
    runs: u32,
    cost: Option<f64>,
    work: Vec<WorkReport>,
    last: Option<RunEnd>,
}

/// How one run of the period ended.
#[derive(Debug, Clone, PartialEq)]
pub struct RunEnd {
    pub outcome: TurnOutcome,
    pub error_kind: Option<ErrorKind>,
    pub error: Option<String>,
    /// USD.
    pub cost: Option<f64>,
    /// The run's final response.
    pub last_response: String,
}

impl BusyTracker {
    /// A run started; the first run of a period names who started the period.
    pub fn run_started(&mut self, started_by: StartedBy, now_ms: i64) {
        let period = self.period.get_or_insert_with(|| Period {
            started_at: now_ms,
            started_by,
            automations: Vec::new(),
            runs: 0,
            cost: None,
            work: Vec::new(),
            last: None,
        });
        period.runs = period.runs.saturating_add(1);
    }

    /// A run of the period took a message an automation queued.
    pub fn injected(&mut self, origin: &AutomationEventOrigin) {
        if let Some(period) = &mut self.period
            && !period.automations.contains(&origin.automation)
        {
            period.automations.push(origin.automation.clone());
        }
    }

    /// A run of the period took in a peer message. When the mailbox started the period and that
    /// run is still its only one, the first such message started it: as the work it assigns, or
    /// else as the message itself.
    pub fn peer_injected(&mut self, origin: &PeerMessageOrigin) {
        if let Some(period) = &mut self.period
            && period.runs == 1
            && period.started_by == StartedBy::Mailbox
        {
            period.started_by = peer_start(origin);
        }
    }

    /// What became of the session's group work. A report the period already has by group, work,
    /// outcome and pause reason adds nothing. Outside a period the report is dropped: no run was
    /// under way, and the frontend hands reports over before it settles, so no `idle` covers it.
    pub fn work_reported(&mut self, reported: WorkReported) {
        let Some(period) = &mut self.period else {
            debug!(
                group = %reported.group,
                work = %reported.work,
                outcome = ?reported.outcome,
                "group work report outside a busy period dropped"
            );
            return;
        };
        let reason = reported.pause.map(pause_reason);
        if period.work.iter().any(|report| {
            report.group == reported.group
                && report.work == reported.work
                && report.outcome == reported.outcome
                && report.pause_reason == reason
        }) {
            return;
        }
        period.work.push(WorkReport {
            group: reported.group,
            work: reported.work,
            outcome: reported.outcome,
            pause_reason: reason,
            detail: reported.detail.map(Untrusted::text),
        });
    }

    pub fn run_ended(&mut self, end: RunEnd) {
        if let Some(period) = &mut self.period {
            period.cost = match (period.cost, end.cost) {
                (Some(total), Some(cost)) => Some(total + cost),
                (total, cost) => total.or(cost),
            };
            period.last = Some(end);
        }
    }

    /// Ends the period as the session settles. `None` when nothing ran since the last settle.
    pub fn settle(&mut self, now_ms: i64) -> Option<IdleDetail> {
        let period = self.period.take()?;
        let busy_ms = now_ms.saturating_sub(period.started_at).max(0);
        let (outcome, error_kind, error, last_response) = match period.last {
            Some(end) => (end.outcome, end.error_kind, end.error, end.last_response),
            None => (UNFINISHED_OUTCOME, None, None, String::new()),
        };
        Some(IdleDetail {
            outcome,
            error_kind,
            error: error.map(Untrusted::text),
            started_by: period.started_by,
            automations: period.automations,
            runs: period.runs,
            busy_s: u64::try_from(busy_ms / MILLIS_PER_SECOND).unwrap_or_default(),
            cost: period.cost,
            work: period.work,
            last_response: Untrusted::text(cap_text(last_response, MAX_LAST_RESPONSE_BYTES).0),
        })
    }
}

/// The reason automation events give for a pause of group work.
pub fn pause_reason(pause: WorkPause) -> PauseReason {
    match pause {
        WorkPause::CompletionRequired => PauseReason::CompletionRequired,
        WorkPause::Cancelled => PauseReason::Cancelled,
        WorkPause::TurnLimit => PauseReason::TurnLimit,
        WorkPause::TurnFailed => PauseReason::TurnFailed,
        WorkPause::SessionClosed => PauseReason::SessionClosed,
        WorkPause::Manual => PauseReason::Manual,
    }
}

/// Who started a period by the peer message its first run took in. The `message_id` is the one
/// the model saw: the origin names no sender route, so it cannot cite the message the way a
/// `message_received` event does. A script has no name to reply to, so it is no `sender`.
fn peer_start(origin: &PeerMessageOrigin) -> StartedBy {
    let message_id = origin.message_id.clone();
    let topic = origin.audience.topic().map(str::to_owned);
    match &origin.assignment {
        Some(assignment) => StartedBy::Work {
            group: assignment.group.clone(),
            work: assignment.work.clone(),
            attempt: assignment.attempt,
            max_attempts: assignment.max_attempts,
            message_id,
            topic,
        },
        None => StartedBy::Peer {
            message_id,
            sender: (!origin.external).then(|| origin.reply_target.clone()),
            sender_kind: sender_kind(origin),
            audience: audience(&origin.audience),
            topic,
        },
    }
}

fn sender_kind(origin: &PeerMessageOrigin) -> SenderKind {
    match origin.sender_kind() {
        PEER_SCRIPT_SENDER => SenderKind::Script,
        PEER_AUTOMATION_SENDER => SenderKind::Automation,
        _ => SenderKind::Session,
    }
}

fn audience(audience: &PeerAudience) -> Audience {
    match audience {
        PeerAudience::Direct => Audience::Direct,
        PeerAudience::Topic { .. } => Audience::Topic,
        PeerAudience::Broadcast => Audience::Broadcast,
    }
}

#[cfg(test)]
mod tests {
    use caudra_automation::event::{CUT_MARKER, WorkOutcome};
    use caudra_providers::PeerAssignment;
    use test_case::test_case;

    use super::*;

    const START_MS: i64 = 1_790_000_000_000;
    const BUSY_S: u64 = 95;
    const NAME: &str = "keep-going";
    const OTHER: &str = "goal-chain";
    const FIRE_ID: &str = "fire-1";
    const RESPONSE: &str = "All tests pass.";
    const ERROR: &str = "overloaded";
    const FIRST_COST: f64 = 0.25;
    const SECOND_COST: f64 = 0.5;
    const MESSAGE_ID: &str = "brisk-calm-otter";
    const SENDER_NAME: &str = "Reviewer";
    const REPLY_TARGET: &str = "@reviewer";
    const SENDER_AUTOMATION: &str = "nightly-digest";
    const TOPIC: &str = "ci.failures";
    const GROUP: &str = "ci-triage";
    const WORK: &str = "quiet-amber-fox";
    const OTHER_WORK: &str = "steady-teal-wren";
    const ATTEMPT: u32 = 2;
    const MAX_ATTEMPTS: u32 = 3;
    const SUMMARY: &str = "Fixed the flaky linker step";
    const NOTHING_RAN: &str = "a settle without a run must not report an idle period";
    const RESPONSE_IS_TEXT: &str = "the last response must stay untrusted text";
    const NOTHING_CARRIED: &str = "nothing that came outside a period may reach the next one";

    fn origin(automation: &str) -> AutomationEventOrigin {
        AutomationEventOrigin {
            automation: automation.into(),
            fire_id: FIRE_ID.into(),
            seq: 0,
        }
    }

    fn end(outcome: TurnOutcome, cost: Option<f64>) -> RunEnd {
        RunEnd {
            outcome,
            error_kind: (outcome == TurnOutcome::Error).then_some(ErrorKind::Overloaded),
            error: (outcome == TurnOutcome::Error).then(|| ERROR.to_owned()),
            cost,
            last_response: RESPONSE.into(),
        }
    }

    /// A direct message from a session.
    fn message() -> PeerMessageOrigin {
        PeerMessageOrigin {
            message_id: MESSAGE_ID.into(),
            audience: PeerAudience::Direct,
            sender_name: SENDER_NAME.into(),
            sender_handle: None,
            reply_target: REPLY_TARGET.into(),
            reply_to: None,
            external: false,
            automation: None,
            assignment: None,
        }
    }

    fn topic() -> PeerAudience {
        PeerAudience::Topic {
            topic: TOPIC.into(),
        }
    }

    fn assignment() -> PeerMessageOrigin {
        PeerMessageOrigin {
            audience: topic(),
            assignment: Some(PeerAssignment {
                group: GROUP.into(),
                work: WORK.into(),
                attempt: ATTEMPT,
                max_attempts: MAX_ATTEMPTS,
            }),
            ..message()
        }
    }

    fn started_by_peer(
        sender: Option<&str>,
        sender_kind: SenderKind,
        audience: Audience,
        topic: Option<&str>,
    ) -> StartedBy {
        StartedBy::Peer {
            message_id: MESSAGE_ID.into(),
            sender: sender.map(str::to_owned),
            sender_kind,
            audience,
            topic: topic.map(str::to_owned),
        }
    }

    fn started_by_work() -> StartedBy {
        StartedBy::Work {
            group: GROUP.into(),
            work: WORK.into(),
            attempt: ATTEMPT,
            max_attempts: MAX_ATTEMPTS,
            message_id: MESSAGE_ID.into(),
            topic: Some(TOPIC.into()),
        }
    }

    fn reported(work: &str, outcome: WorkOutcome, pause: Option<WorkPause>) -> WorkReported {
        WorkReported {
            group: GROUP.into(),
            work: work.into(),
            outcome,
            pause,
            detail: Some(SUMMARY.into()),
        }
    }

    /// The `idle` of a period the mailbox started, once `runs` went through it.
    fn mailbox_idle(runs: impl FnOnce(&mut BusyTracker)) -> IdleDetail {
        let mut tracker = BusyTracker::default();
        tracker.run_started(StartedBy::Mailbox, START_MS);
        runs(&mut tracker);
        tracker.settle(START_MS).unwrap()
    }

    #[test]
    fn a_period_reports_who_started_it_what_ran_and_how_it_ended() {
        let mut tracker = BusyTracker::default();
        let started_by = StartedBy::Automation {
            automation: NAME.into(),
            fire_id: FIRE_ID.into(),
        };

        tracker.run_started(started_by.clone(), START_MS);
        tracker.injected(&origin(NAME));
        tracker.run_ended(end(TurnOutcome::Completed, Some(FIRST_COST)));
        tracker.run_started(StartedBy::User, START_MS + 1);
        tracker.injected(&origin(OTHER));
        tracker.injected(&origin(NAME));
        tracker.run_ended(end(TurnOutcome::Error, Some(SECOND_COST)));
        let idle = tracker
            .settle(START_MS + i64::try_from(BUSY_S).unwrap() * MILLIS_PER_SECOND)
            .unwrap();

        assert_eq!(idle.started_by, started_by);
        assert_eq!(idle.automations, [NAME, OTHER]);
        assert_eq!(idle.runs, 2);
        assert_eq!(idle.busy_s, BUSY_S);
        assert_eq!(idle.cost, Some(FIRST_COST + SECOND_COST));
        assert_eq!(idle.outcome, TurnOutcome::Error);
        assert_eq!(idle.error_kind, Some(ErrorKind::Overloaded));
        assert_eq!(idle.error, Some(Untrusted::text(ERROR)));
        assert_eq!(idle.last_response, Untrusted::text(RESPONSE));
        assert!(idle.work.is_empty());
        assert!(tracker.settle(START_MS).is_none(), "{NOTHING_RAN}");
    }

    #[test_case(None, None => None; "no_cost_known")]
    #[test_case(Some(FIRST_COST), None => Some(FIRST_COST); "one_cost_known")]
    fn unknown_costs_stay_unknown(first: Option<f64>, second: Option<f64>) -> Option<f64> {
        let mut tracker = BusyTracker::default();
        tracker.run_started(StartedBy::User, START_MS);
        tracker.run_ended(end(TurnOutcome::Completed, first));
        tracker.run_ended(end(TurnOutcome::Completed, second));
        tracker.settle(START_MS).unwrap().cost
    }

    #[test]
    fn a_long_response_is_cut_to_the_event_limit() {
        let mut tracker = BusyTracker::default();
        tracker.run_started(StartedBy::User, START_MS);
        tracker.run_ended(RunEnd {
            last_response: "x".repeat(MAX_LAST_RESPONSE_BYTES + 1),
            ..end(TurnOutcome::Completed, None)
        });

        match tracker.settle(START_MS).unwrap().last_response {
            Untrusted::Text(response) => {
                assert_eq!(response.len(), MAX_LAST_RESPONSE_BYTES);
                assert!(response.ends_with(CUT_MARKER));
            }
            other => panic!("{RESPONSE_IS_TEXT}: {other:?}"),
        }
    }

    #[test]
    fn messages_outside_a_period_are_ignored() {
        let mut tracker = BusyTracker::default();
        tracker.injected(&origin(NAME));
        tracker.peer_injected(&assignment());
        tracker.work_reported(reported(WORK, WorkOutcome::Completed, None));
        tracker.run_ended(end(TurnOutcome::Completed, None));

        assert!(tracker.settle(START_MS).is_none(), "{NOTHING_RAN}");
        tracker.run_started(StartedBy::Mailbox, START_MS);
        let idle = tracker.settle(START_MS).unwrap();
        assert_eq!(idle.started_by, StartedBy::Mailbox, "{NOTHING_CARRIED}");
        assert!(
            idle.automations.is_empty() && idle.work.is_empty(),
            "{NOTHING_CARRIED}"
        );
    }

    #[test_case(
        message(),
        started_by_peer(Some(REPLY_TARGET), SenderKind::Session, Audience::Direct, None);
        "direct_from_a_session"
    )]
    #[test_case(
        PeerMessageOrigin { audience: topic(), external: true, ..message() },
        started_by_peer(None, SenderKind::Script, Audience::Topic, Some(TOPIC));
        "topic_from_a_script"
    )]
    #[test_case(
        PeerMessageOrigin {
            audience: PeerAudience::Broadcast,
            automation: Some(SENDER_AUTOMATION.into()),
            ..message()
        },
        started_by_peer(Some(REPLY_TARGET), SenderKind::Automation, Audience::Broadcast, None);
        "broadcast_from_an_automation"
    )]
    #[test_case(assignment(), started_by_work(); "work_assignment")]
    fn the_first_peer_message_of_a_mailbox_run_names_who_started_the_period(
        origin: PeerMessageOrigin,
        expected: StartedBy,
    ) {
        let idle = mailbox_idle(|tracker| tracker.peer_injected(&origin));
        assert_eq!(idle.started_by, expected);
    }

    #[test_case(
        StartedBy::Mailbox,
        |tracker| {
            tracker.peer_injected(&assignment());
            tracker.peer_injected(&message());
        },
        started_by_work();
        "a_later_message_of_the_first_run"
    )]
    #[test_case(
        StartedBy::Mailbox,
        |tracker| {
            tracker.run_ended(end(TurnOutcome::Completed, None));
            tracker.run_started(StartedBy::Mailbox, START_MS);
            tracker.peer_injected(&message());
        },
        StartedBy::Mailbox;
        "a_message_of_a_later_run"
    )]
    #[test_case(
        StartedBy::User,
        |tracker| tracker.peer_injected(&message()),
        StartedBy::User;
        "a_period_a_person_started"
    )]
    fn who_started_the_period_stays_put(
        started_by: StartedBy,
        runs: fn(&mut BusyTracker),
        expected: StartedBy,
    ) {
        let mut tracker = BusyTracker::default();
        tracker.run_started(started_by, START_MS);
        runs(&mut tracker);
        assert_eq!(tracker.settle(START_MS).unwrap().started_by, expected);
    }

    #[test_case(WorkOutcome::Completed, None, None; "completed")]
    #[test_case(WorkOutcome::Retry, None, None; "retry")]
    #[test_case(WorkOutcome::Failed, None, None; "failed")]
    #[test_case(
        WorkOutcome::Paused,
        Some(WorkPause::CompletionRequired),
        Some(PauseReason::CompletionRequired);
        "completion_required"
    )]
    #[test_case(WorkOutcome::Paused, Some(WorkPause::Cancelled), Some(PauseReason::Cancelled); "cancelled")]
    #[test_case(WorkOutcome::Paused, Some(WorkPause::TurnLimit), Some(PauseReason::TurnLimit); "turn_limit")]
    #[test_case(WorkOutcome::Paused, Some(WorkPause::TurnFailed), Some(PauseReason::TurnFailed); "turn_failed")]
    #[test_case(
        WorkOutcome::Paused,
        Some(WorkPause::SessionClosed),
        Some(PauseReason::SessionClosed);
        "session_closed"
    )]
    #[test_case(WorkOutcome::Paused, Some(WorkPause::Manual), Some(PauseReason::Manual); "manual")]
    fn idle_reports_each_work_outcome_with_its_pause_reason(
        outcome: WorkOutcome,
        pause: Option<WorkPause>,
        pause_reason: Option<PauseReason>,
    ) {
        let idle = mailbox_idle(|tracker| tracker.work_reported(reported(WORK, outcome, pause)));
        assert_eq!(
            idle.work,
            [WorkReport {
                group: GROUP.into(),
                work: WORK.into(),
                outcome,
                pause_reason,
                detail: Some(Untrusted::text(SUMMARY)),
            }]
        );
    }

    #[test]
    fn a_repeated_report_adds_nothing() {
        let reports = [
            (WORK, WorkOutcome::Completed),
            (WORK, WorkOutcome::Completed),
            (OTHER_WORK, WorkOutcome::Completed),
            (WORK, WorkOutcome::Retry),
        ];
        let idle = mailbox_idle(|tracker| {
            for (work, outcome) in reports {
                tracker.work_reported(reported(work, outcome, None));
            }
        });

        let kept: Vec<_> = idle
            .work
            .iter()
            .map(|report| (report.work.as_str(), report.outcome))
            .collect();
        assert_eq!(kept, [reports[0], reports[2], reports[3]]);
    }

    #[test]
    fn pauses_for_different_reasons_are_different_reports() {
        let idle = mailbox_idle(|tracker| {
            for pause in [WorkPause::TurnLimit, WorkPause::Manual, WorkPause::Manual] {
                tracker.work_reported(reported(WORK, WorkOutcome::Paused, Some(pause)));
            }
        });

        let reasons: Vec<_> = idle.work.iter().map(|report| report.pause_reason).collect();
        assert_eq!(
            reasons,
            [Some(PauseReason::TurnLimit), Some(PauseReason::Manual)]
        );
    }
}
