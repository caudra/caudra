//! Which trigger an event matches, and which messaging targets a script may reach. Topic
//! patterns stay opaque: the caller injects the messaging grammar.

use crate::event::{EventDetail, MessageDetail, SenderKind};
use crate::meta::{BROADCAST, MessageFilter, MessagingCaps, NAME_WILDCARD, Trigger};
use crate::untrusted::Untrusted;

pub trait TopicMatcher {
    fn matches(&self, pattern: &str, topic: &str) -> bool;
}

/// The index of the first trigger of the event's kind whose static options match. Schedule
/// events never match here: the scheduler routes them with their trigger index.
pub fn first_match(
    triggers: &[Trigger],
    detail: &EventDetail,
    topics: &dyn TopicMatcher,
) -> Option<usize> {
    triggers
        .iter()
        .position(|trigger| trigger_matches(trigger, detail, topics))
}

/// Whether `name`, spelled `@name`, matches `@name`, `@prefix-*` or `*`.
pub fn name_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix(NAME_WILDCARD) {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

/// Whether `messaging.send` lets the script message `target`, an `@name`.
pub fn send_allowed(caps: &MessagingCaps, target: &str) -> bool {
    caps.send
        .iter()
        .any(|pattern| name_matches(pattern, target))
}

/// Whether `messaging.publish` lists `topic` exactly, or `broadcast` for `None`.
pub fn publish_allowed(caps: &MessagingCaps, topic: Option<&str>) -> bool {
    let wanted = topic.unwrap_or(BROADCAST);
    caps.publish.iter().any(|listed| listed == wanted)
}

fn trigger_matches(trigger: &Trigger, detail: &EventDetail, topics: &dyn TopicMatcher) -> bool {
    match (trigger, detail) {
        (Trigger::Armed, EventDetail::Armed { .. })
        | (Trigger::Idle { .. }, EventDetail::Idle(_)) => true,
        (Trigger::NeedsInput { inputs, .. }, EventDetail::NeedsInput { input, .. }) => {
            inputs.contains(input)
        }
        (Trigger::GoalFinished { verdicts }, EventDetail::GoalFinished(goal)) => {
            verdicts.contains(&goal.verdict)
        }
        (Trigger::MessageReceived(filter), EventDetail::MessageReceived(message)) => {
            message_matches(filter, message, topics)
        }
        (Trigger::WorkFinished { groups, states }, EventDetail::WorkFinished(work)) => {
            listed_or_all(groups, &work.group) && states.contains(&work.state)
        }
        (
            Trigger::WorkflowFinished {
                workflows,
                statuses,
            },
            EventDetail::WorkflowFinished(run),
        ) => listed_or_all(workflows, &run.workflow) && statuses.contains(&run.status),
        _ => false,
    }
}

fn listed_or_all(listed: &[String], name: &str) -> bool {
    listed.is_empty() || listed.iter().any(|entry| entry == name)
}

fn message_matches(
    filter: &MessageFilter,
    message: &MessageDetail,
    topics: &dyn TopicMatcher,
) -> bool {
    filter.audiences.contains(&message.audience)
        && filter.admissions.contains(&message.admission)
        && (filter.from_automations || message.sender_kind != SenderKind::Automation)
        && (filter.topics.is_empty()
            || message.topic.as_deref().is_some_and(|topic| {
                filter
                    .topics
                    .iter()
                    .any(|pattern| topics.matches(pattern, topic))
            }))
        && sender_matches(filter, message)
}

/// With `senders` or `scripts` set, a session or automation must match a sender pattern by its
/// `@name`, and a script must carry one of the labels.
fn sender_matches(filter: &MessageFilter, message: &MessageDetail) -> bool {
    if filter.senders.is_empty() && filter.scripts.is_empty() {
        return true;
    }
    match message.sender_kind {
        SenderKind::Session | SenderKind::Automation => {
            message.sender.as_deref().is_some_and(|sender| {
                filter
                    .senders
                    .iter()
                    .any(|pattern| name_matches(pattern, sender))
            })
        }
        SenderKind::Script => matches!(
            &message.sender_label,
            Some(Untrusted::Text(label)) if filter.scripts.contains(label)
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use test_case::test_case;

    use super::*;
    use crate::event::{
        Admission, ArmedReason, Audience, Delivery, GoalFinishedDetail, GoalVerdict, IdleDetail,
        InputKind, StartedBy, TurnOutcome, WorkFinishedDetail, WorkState, WorkflowFinishedDetail,
        WorkflowStatus,
    };
    use crate::meta::{Cadence, CatchUp, Schedule};

    const TOPIC: &str = "ci.failures";
    const OTHER_TOPIC: &str = "swarm.status";
    const WATCHER: &str = "@ci-watcher";
    const WORKER: &str = "@worker-2";
    const WORKERS: &str = "@worker-*";
    const STRANGER: &str = "@librarian";
    const LABEL: &str = "nightly-ci";
    const OTHER_LABEL: &str = "pre-commit";
    const GROUP: &str = "swarm-tasks";
    const OTHER_GROUP: &str = "review-queue";
    const WORKFLOW: &str = "deep-research";
    const OTHER_WORKFLOW: &str = "root-cause";
    const MESSAGE_ID: &str = "message-1";
    const TEXT: &str = "CI failed on main";
    const POLL_EVERY: Duration = Duration::from_mins(5);

    struct ExactTopics;

    impl TopicMatcher for ExactTopics {
        fn matches(&self, pattern: &str, topic: &str) -> bool {
            pattern == topic
        }
    }

    fn filter() -> MessageFilter {
        MessageFilter {
            audiences: vec![Audience::Direct, Audience::Topic, Audience::Broadcast],
            topics: Vec::new(),
            senders: Vec::new(),
            scripts: Vec::new(),
            admissions: vec![Admission::Queued],
            from_automations: false,
            consume: false,
        }
    }

    fn message(sender_kind: SenderKind, sender: Option<&str>) -> MessageDetail {
        MessageDetail {
            message_id: MESSAGE_ID.to_owned(),
            audience: Audience::Topic,
            topic: Some(TOPIC.to_owned()),
            sender_kind,
            sender: sender.map(str::to_owned),
            sender_automation: None,
            sender_label: None,
            sender_title: None,
            sender_cwd: None,
            text: Untrusted::text(TEXT),
            reply_to: None,
            admission: Admission::Queued,
            delivery: Delivery::Live,
            consumed: false,
        }
    }

    fn from_script(label: &str) -> MessageDetail {
        MessageDetail {
            sender_label: Some(Untrusted::text(label)),
            ..message(SenderKind::Script, None)
        }
    }

    fn received(filter: MessageFilter, message: MessageDetail) -> bool {
        first_match(
            &[Trigger::MessageReceived(filter)],
            &EventDetail::MessageReceived(message),
            &ExactTopics,
        )
        .is_some()
    }

    fn idle() -> EventDetail {
        EventDetail::Idle(IdleDetail {
            outcome: TurnOutcome::Completed,
            error_kind: None,
            error: None,
            started_by: StartedBy::User,
            automations: Vec::new(),
            runs: 1,
            busy_s: 0,
            cost: None,
            work: Vec::new(),
            last_response: Untrusted::text(TEXT),
        })
    }

    fn goal_finished(verdict: GoalVerdict) -> EventDetail {
        EventDetail::GoalFinished(GoalFinishedDetail {
            verdict,
            condition: TEXT.to_owned(),
            reason: Untrusted::text(TEXT),
            evaluations: 1,
            duration_s: 0,
            cost: None,
        })
    }

    fn work_finished(group: &str, state: WorkState) -> EventDetail {
        EventDetail::WorkFinished(WorkFinishedDetail {
            group: group.to_owned(),
            work: MESSAGE_ID.to_owned(),
            message_id: MESSAGE_ID.to_owned(),
            topic: None,
            state,
            attempts: 1,
            max_attempts: 1,
            member: None,
            pause_reason: None,
            detail: None,
        })
    }

    fn workflow_finished(workflow: &str, status: WorkflowStatus) -> EventDetail {
        EventDetail::WorkflowFinished(WorkflowFinishedDetail {
            run_id: MESSAGE_ID.to_owned(),
            name: workflow.to_owned(),
            workflow: workflow.to_owned(),
            status,
            report: None,
            result: None,
            error: None,
            scratch_dir: None,
            agents: 0,
            tokens: 0,
        })
    }

    fn polling() -> Trigger {
        Trigger::Schedule(Schedule {
            cadence: Cadence::Every(POLL_EVERY),
            catch_up: CatchUp::Once,
        })
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn the_first_trigger_of_the_kind_wins() {
        let triggers = [
            polling(),
            Trigger::Idle {
                delay: Duration::ZERO,
            },
            Trigger::Armed,
            Trigger::Idle { delay: POLL_EVERY },
        ];
        assert_eq!(first_match(&triggers, &idle(), &ExactTopics), Some(1));
        assert_eq!(
            first_match(
                &triggers,
                &EventDetail::Armed {
                    reason: ArmedReason::Manual
                },
                &ExactTopics
            ),
            Some(2)
        );
    }

    #[test]
    fn schedule_events_are_routed_by_the_scheduler() {
        let detail = EventDetail::Schedule {
            scheduled_for: 0,
            late_by_s: 0,
        };
        assert_eq!(first_match(&[polling()], &detail, &ExactTopics), None);
    }

    #[test]
    fn a_trigger_of_another_kind_never_matches() {
        assert_eq!(first_match(&[Trigger::Armed], &idle(), &ExactTopics), None);
    }

    #[test_case(InputKind::Permission => true; "listed")]
    #[test_case(InputKind::Messages => false; "unlisted")]
    fn needs_input_filters_by_input(input: InputKind) -> bool {
        let trigger = Trigger::NeedsInput {
            delay: Duration::ZERO,
            inputs: vec![InputKind::Permission, InputKind::Question],
        };
        let detail = EventDetail::NeedsInput {
            input,
            tool: None,
            waiting_s: 0,
        };
        first_match(&[trigger], &detail, &ExactTopics).is_some()
    }

    #[test_case(GoalVerdict::Met => true; "listed")]
    #[test_case(GoalVerdict::Cleared => false; "unlisted")]
    fn goal_finished_filters_by_verdict(verdict: GoalVerdict) -> bool {
        let trigger = Trigger::GoalFinished {
            verdicts: vec![GoalVerdict::Met, GoalVerdict::Impossible],
        };
        first_match(&[trigger], &goal_finished(verdict), &ExactTopics).is_some()
    }

    #[test_case(&[GROUP], GROUP, WorkState::Completed => true; "listed_group")]
    #[test_case(&[], OTHER_GROUP, WorkState::Failed => true; "any_group")]
    #[test_case(&[GROUP], OTHER_GROUP, WorkState::Completed => false; "unlisted_group")]
    #[test_case(&[], GROUP, WorkState::Paused => false; "unlisted_state")]
    fn work_finished_filters_by_group_and_state(
        groups: &[&str],
        group: &str,
        state: WorkState,
    ) -> bool {
        let trigger = Trigger::WorkFinished {
            groups: strings(groups),
            states: vec![WorkState::Completed, WorkState::Failed],
        };
        first_match(&[trigger], &work_finished(group, state), &ExactTopics).is_some()
    }

    #[test_case(&[WORKFLOW], WORKFLOW, WorkflowStatus::Completed => true; "listed_workflow")]
    #[test_case(&[], OTHER_WORKFLOW, WorkflowStatus::Failed => true; "any_workflow")]
    #[test_case(&[WORKFLOW], OTHER_WORKFLOW, WorkflowStatus::Completed => false; "unlisted_workflow")]
    #[test_case(&[], WORKFLOW, WorkflowStatus::Interrupted => false; "unlisted_status")]
    fn workflow_finished_filters_by_workflow_and_status(
        workflows: &[&str],
        workflow: &str,
        status: WorkflowStatus,
    ) -> bool {
        let trigger = Trigger::WorkflowFinished {
            workflows: strings(workflows),
            statuses: vec![WorkflowStatus::Completed, WorkflowStatus::Failed],
        };
        first_match(
            &[trigger],
            &workflow_finished(workflow, status),
            &ExactTopics,
        )
        .is_some()
    }

    #[test_case(vec![Audience::Topic] => true; "listed")]
    #[test_case(vec![Audience::Direct] => false; "unlisted")]
    fn messages_filter_by_audience(audiences: Vec<Audience>) -> bool {
        received(
            MessageFilter {
                audiences,
                ..filter()
            },
            message(SenderKind::Session, Some(WATCHER)),
        )
    }

    #[test_case(&[], Some(OTHER_TOPIC) => true; "any_topic")]
    #[test_case(&[], None => true; "any_message")]
    #[test_case(&[OTHER_TOPIC, TOPIC], Some(TOPIC) => true; "a_matching_pattern")]
    #[test_case(&[TOPIC], Some(OTHER_TOPIC) => false; "no_matching_pattern")]
    #[test_case(&[TOPIC], None => false; "a_direct_message")]
    fn messages_filter_by_topic(patterns: &[&str], topic: Option<&str>) -> bool {
        received(
            MessageFilter {
                topics: strings(patterns),
                ..filter()
            },
            MessageDetail {
                topic: topic.map(str::to_owned),
                ..message(SenderKind::Session, Some(WATCHER))
            },
        )
    }

    #[test_case(&[], &[], message(SenderKind::Session, Some(STRANGER)) => true; "no_sender_filter")]
    #[test_case(&[WATCHER], &[LABEL], message(SenderKind::Session, Some(WATCHER)) => true; "a_named_session")]
    #[test_case(&[WORKERS], &[], message(SenderKind::Session, Some(WORKER)) => true; "a_prefix_pattern")]
    #[test_case(&[WATCHER], &[LABEL], message(SenderKind::Session, Some(STRANGER)) => false; "an_unlisted_session")]
    #[test_case(&[], &[LABEL], message(SenderKind::Session, Some(WATCHER)) => false; "a_session_when_only_scripts_are_listed")]
    #[test_case(&[WATCHER], &[LABEL], from_script(LABEL) => true; "a_listed_script")]
    #[test_case(&[WATCHER], &[LABEL], from_script(OTHER_LABEL) => false; "an_unlisted_script")]
    #[test_case(&["*"], &[], from_script(LABEL) => false; "a_script_against_sender_patterns")]
    fn messages_filter_by_sender(
        senders: &[&str],
        scripts: &[&str],
        message: MessageDetail,
    ) -> bool {
        received(
            MessageFilter {
                senders: strings(senders),
                scripts: strings(scripts),
                ..filter()
            },
            message,
        )
    }

    #[test_case(false => false; "ignored_by_default")]
    #[test_case(true => true; "when_asked_for")]
    fn automation_senders(from_automations: bool) -> bool {
        received(
            MessageFilter {
                from_automations,
                ..filter()
            },
            message(SenderKind::Automation, Some(WATCHER)),
        )
    }

    #[test_case(Admission::Queued => true; "queued")]
    #[test_case(Admission::Held => false; "held")]
    fn messages_filter_by_admission(admission: Admission) -> bool {
        received(
            filter(),
            MessageDetail {
                admission,
                ..message(SenderKind::Session, Some(WATCHER))
            },
        )
    }

    #[test_case("*", WORKER => true; "anyone")]
    #[test_case(WATCHER, WATCHER => true; "exact")]
    #[test_case(WATCHER, WORKER => false; "another_name")]
    #[test_case(WORKERS, WORKER => true; "prefix")]
    #[test_case(WORKERS, "@worker" => false; "the_bare_prefix")]
    #[test_case(WORKERS, "@workers-1" => false; "a_longer_word")]
    fn name_patterns(pattern: &str, name: &str) -> bool {
        name_matches(pattern, name)
    }

    #[test_case(&[WORKERS], WORKER => true; "a_matching_pattern")]
    #[test_case(&[WATCHER], WORKER => false; "no_matching_pattern")]
    #[test_case(&[], WORKER => false; "no_send_capability")]
    fn sending_needs_a_matching_target(send: &[&str], target: &str) -> bool {
        send_allowed(
            &MessagingCaps {
                send: strings(send),
                ..MessagingCaps::default()
            },
            target,
        )
    }

    #[test_case(&[TOPIC], Some(TOPIC) => true; "a_listed_topic")]
    #[test_case(&[TOPIC], Some(OTHER_TOPIC) => false; "an_unlisted_topic")]
    #[test_case(&["ci.*"], Some(TOPIC) => false; "patterns_are_not_expanded")]
    #[test_case(&[BROADCAST], None => true; "a_listed_broadcast")]
    #[test_case(&[TOPIC], None => false; "an_unlisted_broadcast")]
    fn publishing_needs_the_exact_topic(publish: &[&str], topic: Option<&str>) -> bool {
        publish_allowed(
            &MessagingCaps {
                publish: strings(publish),
                ..MessagingCaps::default()
            },
            topic,
        )
    }
}
