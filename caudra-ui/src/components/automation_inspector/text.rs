//! How the inspector words the read model: a name for each of its enums, and
//! times told against the clock the frame was drawn at.

use caudra_automation::catalog::{Scope, Trust};
use caudra_automation::event::{InputKind, TurnOutcome};
use caudra_automation::host::{ActionKind, DeliveryMode};
use caudra_automation::limits::LimitReason;
use caudra_automation::meta::TriggerKind;
use caudra_automation::snapshot::{ArmOrigin, PauseSource, SettleBlocker, WaitReason};
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::components::format_elapsed;

const CLOCK_FORMAT: &str = "%H:%M:%S";
const MILLIS_PER_SECOND: u64 = 1_000;
const MILLIS_UNIT: &str = "ms";
const AGO_SUFFIX: &str = " ago";
const IN_PREFIX: &str = "in ";
pub(super) const PAUSED_WAIT: &str = "automations are paused";
pub(super) const PROMPT_WAIT: &str = "a human prompt is queued";
pub(super) const MODAL_WAIT: &str = "a modal is open";
pub(super) const BUSY_WAIT: &str = "the session is busy";
pub(super) const PEERS_WAIT: &str = "peer messages or group work go first";
pub(super) const TURN_RATE_WAIT: &str = "turns_per_hour is full until ";
pub(super) const UNATTENDED_WAIT: &str = "unattended turns reached the cap of ";
pub(super) const BACKOFF_WAIT: &str = "delivery backoff until ";
pub(super) const EXPIRES_WAIT: &str = "expires at ";
const TURNS_UNIT: &str = " turns";

/// The wall clock in unix milliseconds, as the runtime's own clock reads it.
pub(super) fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

/// `14:02:05` in the local zone, or nothing for a time the clock cannot hold.
pub(super) fn clock(at: i64) -> String {
    Timestamp::from_millisecond(at)
        .map(|at| {
            at.to_zoned(TimeZone::system())
                .strftime(CLOCK_FORMAT)
                .to_string()
        })
        .unwrap_or_default()
}

/// `3m05s`, or `450ms` for a span under a second, which is how long most
/// firings take.
pub(super) fn span(millis: i64) -> String {
    match millis.unsigned_abs() {
        millis if millis < MILLIS_PER_SECOND => format!("{millis}{MILLIS_UNIT}"),
        millis => format_elapsed(millis / MILLIS_PER_SECOND),
    }
}

/// `in 5m00s` or `2m10s ago`.
pub(super) fn relative(at: i64, now: i64) -> String {
    match at >= now {
        true => format!("{IN_PREFIX}{}", span(at - now)),
        false => format!("{}{AGO_SUFFIX}", span(now - at)),
    }
}

/// `14:02:05 (in 5m00s)`: the time to look for, and how far off it is.
pub(super) fn moment(at: i64, now: i64) -> String {
    format!("{} ({})", clock(at), relative(at, now))
}

pub(super) fn wait_text(reason: WaitReason, now: i64) -> String {
    match reason {
        WaitReason::Paused => PAUSED_WAIT.to_owned(),
        WaitReason::HumanPromptQueued => PROMPT_WAIT.to_owned(),
        WaitReason::ModalOpen => MODAL_WAIT.to_owned(),
        WaitReason::Busy => BUSY_WAIT.to_owned(),
        WaitReason::PeersFirst => PEERS_WAIT.to_owned(),
        WaitReason::TurnRateFull { until } => format!("{TURN_RATE_WAIT}{}", moment(until, now)),
        WaitReason::UnattendedCap { cap } => format!("{UNATTENDED_WAIT}{cap}{TURNS_UNIT}"),
        WaitReason::Backoff { until } => format!("{BACKOFF_WAIT}{}", moment(until, now)),
        WaitReason::ExpiresAt { at } => format!("{EXPIRES_WAIT}{}", moment(at, now)),
    }
}

pub(super) fn trigger_text(kind: TriggerKind) -> &'static str {
    match kind {
        TriggerKind::Armed => "armed",
        TriggerKind::Idle => "idle",
        TriggerKind::NeedsInput => "needs_input",
        TriggerKind::GoalFinished => "goal_finished",
        TriggerKind::MessageReceived => "message_received",
        TriggerKind::WorkFinished => "work_finished",
        TriggerKind::WorkflowFinished => "workflow_finished",
        TriggerKind::Schedule => "schedule",
    }
}

/// The host function a script called, as the script spells it.
pub(super) fn action_text(kind: ActionKind) -> &'static str {
    match kind {
        ActionKind::Message => "message",
        ActionKind::SetGoal => "set_goal",
        ActionKind::Notify => "notify",
        ActionKind::Http => "http",
        ActionKind::Reply => "reply",
        ActionKind::Send => "send",
        ActionKind::Publish => "publish",
        ActionKind::Broadcast => "broadcast",
        ActionKind::StartWorkflow => "start_workflow",
        ActionKind::Pause => "pause_automations",
        ActionKind::Log => "log",
    }
}

pub(super) fn scope_text(scope: Scope) -> &'static str {
    match scope {
        Scope::User => "user",
        Scope::Project => "project",
    }
}

pub(super) fn trust_text(trust: Trust) -> &'static str {
    match trust {
        Trust::Location => "trusted where it lives",
        Trust::Approved => "this digest is trusted",
        Trust::Required => "needs trust",
    }
}

pub(super) fn origin_text(origin: ArmOrigin) -> &'static str {
    match origin {
        ArmOrigin::Cli => "armed by --automation",
        ArmOrigin::Profile => "armed by the profile",
        ArmOrigin::Manual => "armed by hand",
        ArmOrigin::Always => "armed in every session (arm: \"always\")",
        ArmOrigin::Sdk => "armed by the SDK",
    }
}

pub(super) fn pause_source_text(source: &PauseSource) -> String {
    match source {
        PauseSource::User => "the user".to_owned(),
        PauseSource::Sdk => "the SDK client".to_owned(),
        PauseSource::Script { automation } => format!("pause_automations() in {automation}"),
        PauseSource::Inspector => "the inspector".to_owned(),
    }
}

pub(super) fn blocker_text(blocker: SettleBlocker) -> String {
    match blocker {
        SettleBlocker::Busy => "a turn, a tool or a compaction is running".to_owned(),
        SettleBlocker::NeedsInput(kind) => format!("waiting on {}", input_text(kind)),
        SettleBlocker::PromptQueued => "a human prompt is queued".to_owned(),
        SettleBlocker::PeerMessages => "peer messages wait to be claimed".to_owned(),
        SettleBlocker::GroupWork => "group work is offered".to_owned(),
        SettleBlocker::MailboxWake => "a mailbox wake is pending".to_owned(),
        SettleBlocker::GoalCheckin => "a goal check-in is due".to_owned(),
        SettleBlocker::AgentEvents => "agent events wait to be handled".to_owned(),
    }
}

fn input_text(kind: InputKind) -> &'static str {
    match kind {
        InputKind::Permission => "a permission prompt",
        InputKind::Question => "a question",
        InputKind::Plan => "a plan",
        InputKind::Auth => "a login",
        InputKind::Plugin => "a plugin prompt",
        InputKind::Messages => "held messages",
    }
}

pub(super) fn outcome_text(outcome: TurnOutcome) -> &'static str {
    match outcome {
        TurnOutcome::Completed => "completed",
        TurnOutcome::Error => "error",
        TurnOutcome::Cancelled => "cancelled",
        TurnOutcome::MaxTurns => "max turns",
    }
}

pub(super) fn delivery_text(mode: DeliveryMode) -> &'static str {
    match mode {
        DeliveryMode::Next => "next",
        DeliveryMode::Guide => "guide",
    }
}

/// The automation limit that holds an acting firing back.
pub(super) fn limit_text(reason: LimitReason) -> &'static str {
    match reason {
        LimitReason::Cooldown => "its cooldown",
        LimitReason::MaxPerHour => "max_per_hour",
        LimitReason::Backoff => "its failure backoff",
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const NOW: i64 = 1_800_000_000_000;
    const FIVE_MINUTES: i64 = 5 * 60 * 1_000;
    const AHEAD: &str = "in 5m00s";
    const BEHIND: &str = "5m00s ago";
    const CAP: u32 = 12;

    #[test_case(WaitReason::Paused, PAUSED_WAIT; "paused")]
    #[test_case(WaitReason::HumanPromptQueued, PROMPT_WAIT; "human_prompt_queued")]
    #[test_case(WaitReason::ModalOpen, MODAL_WAIT; "modal_open")]
    #[test_case(WaitReason::Busy, BUSY_WAIT; "busy")]
    #[test_case(WaitReason::PeersFirst, PEERS_WAIT; "peers_first")]
    fn a_wait_without_a_time_reads_as_its_reason(reason: WaitReason, expected: &str) {
        assert_eq!(wait_text(reason, NOW), expected);
    }

    #[test_case(WaitReason::TurnRateFull { until: NOW + FIVE_MINUTES }, TURN_RATE_WAIT; "turn_rate_full")]
    #[test_case(WaitReason::Backoff { until: NOW + FIVE_MINUTES }, BACKOFF_WAIT; "backoff")]
    #[test_case(WaitReason::ExpiresAt { at: NOW + FIVE_MINUTES }, EXPIRES_WAIT; "expires_at")]
    fn a_timed_wait_names_its_clock_and_how_far_off_it_is(reason: WaitReason, prefix: &str) {
        let text = wait_text(reason, NOW);

        assert!(text.starts_with(prefix), "{text}");
        assert!(text.contains(&clock(NOW + FIVE_MINUTES)), "{text}");
        assert!(text.ends_with(&format!("({AHEAD})")), "{text}");
    }

    #[test]
    fn the_unattended_cap_names_the_cap() {
        assert_eq!(
            wait_text(WaitReason::UnattendedCap { cap: CAP }, NOW),
            format!("{UNATTENDED_WAIT}{CAP}{TURNS_UNIT}")
        );
    }

    #[test_case(NOW + FIVE_MINUTES, AHEAD; "ahead")]
    #[test_case(NOW - FIVE_MINUTES, BEHIND; "behind")]
    fn relative_times_say_which_way(at: i64, expected: &str) {
        assert_eq!(relative(at, NOW), expected);
    }
}
