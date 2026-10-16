//! Turn-end reviews. Before a main-session turn hands back, the delegate may
//! review the plan the run wrote, answer what its reply asks the user, review
//! a stall that would stop the run, or give a second opinion on a goal judged
//! impossible. The run decides when a review applies and takes the verdict
//! in. This module frames what the delegate reads and what autopilot writes
//! back.

use caudra_providers::{AutopilotMessageKind, AutopilotOrigin, Message, tag_safe_literal};

use super::context::{DecisionRequest, clip};
use super::delegate::{DecisionOrigin, DecisionSlot};
use super::wire::{DecisionKind, DelegateAction, DelegateOutput};
use super::{AutopilotHandle, DecisionOutcome};
use crate::EventSender;
use crate::agent::GoalSnapshot;
use crate::cancel::CancelToken;
use crate::prompt::{AUTOPILOT_UNREPORTED_WORK_PROMPT, WORK_SLOT};

const MAX_ITEM_BYTES: usize = 32 * 1024;
const MAX_SUBJECT_BYTES: usize = 160;
const PLAN_LABEL: &str = "The plan this run wrote, waiting for approval to implement:";
const REPLY_LABEL: &str = "The reply the agent handed back with, which asks the user:";
const ON_BEHALF_PREFIX: &str = "Autopilot (on the user's behalf):";
const LOOP_CAP_SUBJECT: &str = "Goal continuation limit";
const NO_PROGRESS_SUBJECT: &str = "Goal made no progress";
const TURN_LIMIT_SUBJECT: &str = "Turn limit";
const STEERING_SUBJECT: &str = "Steering exhausted";
const LOOP_CAP_LABEL: &str = "The run is about to stop with its goal open. The evaluator judged the goal not met after every continuation this run allows. Continuations used:";
const NO_PROGRESS_LABEL: &str = "The run is about to stop with its goal open, because its last turns made no progress: the evaluator saw nothing new, a turn repeated an earlier one, or the agent stopped to ask a person. Continue only with guidance that changes the approach or settles what the agent waits on; otherwise let a human decide. Turns without progress:";
const TURN_LIMIT_LABEL: &str =
    "The run is about to stop: it used every turn it was given. Turns allowed:";
const STEERING_LABEL: &str = "The run is about to fail: steering spent its allowance recovering the agent from a stall and gave up. The steering rule:";
const SECOND_OPINION_LABEL: &str = "The goal evaluator judged the goal impossible, which finishes the goal. Give a second opinion before it does.";
const GOAL_LABEL: &str = "The goal:";
const EVALUATIONS_LABEL: &str = "Evaluations so far:";
const LAST_REASON_LABEL: &str = "The evaluator's last reason:";
const EVALUATOR_REASON_LABEL: &str = "The evaluator's reason:";
/// Why a `continue` review is left to a human once the streak is full.
pub const STREAK_REASON: &str = "autopilot already continued as many reviews in a row as it may without human input, so a human decides this one";

/// Where a run would stop short, as a review puts it to the delegate.
pub enum Stall<'a> {
    /// The goal was still not met after the continuations the run allows.
    LoopCap { continuations: u32 },
    /// The goal's last turns in a row made no progress.
    NoProgress { turns: u32 },
    /// The run used the turns it was given.
    TurnLimit { turns: u32 },
    /// A steering rule spent its allowance.
    Steering { rule: &'a str },
}

/// The delegate's verdict on one turn-end item. An answer that never came is
/// [`DelegateAction::Escalate`].
#[derive(Debug)]
pub struct Verdict {
    pub seq: u32,
    pub action: DelegateAction,
}

impl Verdict {
    /// What a `continue` goes on with, written as autopilot's guidance.
    /// `None` for any other action.
    pub fn guidance(self) -> Option<Message> {
        match self.action {
            DelegateAction::Continue { guidance } => Some(on_behalf(
                self.seq,
                AutopilotMessageKind::Guidance,
                &guidance,
            )),
            _ => None,
        }
    }
}

/// One item a turn about to hand back puts to the delegate.
pub struct TurnEnd {
    kind: DecisionKind,
    subject: String,
    item: String,
}

impl TurnEnd {
    /// A plan, named by its first line, where its title sits.
    pub fn plan(plan: &str) -> Self {
        Self::new(
            DecisionKind::Plan,
            telling_line(plan.lines()),
            format!("{PLAN_LABEL}\n{plan}"),
        )
    }

    /// A reply, named by its last line, where its question usually sits.
    pub fn reply(reply: &str) -> Self {
        Self::new(
            DecisionKind::Reply,
            telling_line(reply.lines().rev()),
            format!("{REPLY_LABEL}\n{reply}"),
        )
    }

    /// A stall, with the goal the run works toward when it has one.
    pub fn stall(stall: &Stall, goal: Option<&GoalSnapshot>) -> Self {
        let (subject, label, detail) = match stall {
            Stall::LoopCap { continuations } => {
                (LOOP_CAP_SUBJECT, LOOP_CAP_LABEL, continuations.to_string())
            }
            Stall::NoProgress { turns } => {
                (NO_PROGRESS_SUBJECT, NO_PROGRESS_LABEL, turns.to_string())
            }
            Stall::TurnLimit { turns } => (TURN_LIMIT_SUBJECT, TURN_LIMIT_LABEL, turns.to_string()),
            Stall::Steering { rule } => (STEERING_SUBJECT, STEERING_LABEL, (*rule).to_owned()),
        };
        let goal = goal.map_or_else(String::new, |goal| {
            let reason = goal
                .last_reason
                .as_deref()
                .map_or_else(String::new, |reason| {
                    format!("\n{LAST_REASON_LABEL} {reason}")
                });
            format!("{}{reason}", goal_lines(&goal.condition, goal.evaluations))
        });
        Self::new(
            DecisionKind::Review,
            &format!("{subject}: {detail}"),
            format!("{label} {detail}{goal}"),
        )
    }

    /// A goal the evaluator judged impossible at `evaluation`, named by its
    /// first line.
    pub fn second_opinion(condition: &str, evaluation: u32, reason: &str) -> Self {
        Self::new(
            DecisionKind::SecondOpinion,
            telling_line(condition.lines()),
            format!(
                "{SECOND_OPINION_LABEL}{}\n{EVALUATOR_REASON_LABEL} {reason}",
                goal_lines(condition, evaluation)
            ),
        )
    }

    fn new(kind: DecisionKind, subject: &str, item: String) -> Self {
        Self {
            kind,
            subject: clip(subject.to_owned(), MAX_SUBJECT_BYTES),
            item: clip(item, MAX_ITEM_BYTES),
        }
    }

    /// Announces the decision, asks the delegate and records the decision's
    /// row. `None` when `cancel` fired first. A review the delegate continues
    /// once the streak is full is recorded as left to a human. An answer
    /// autopilot gave is recorded as applied, and an escalation is left for
    /// whoever answers it to settle.
    pub async fn decide(
        self,
        handle: &AutopilotHandle,
        event_tx: &EventSender,
        cancel: &CancelToken,
    ) -> Option<Verdict> {
        let origin = DecisionOrigin::new(handle, self.subject, &self.item);
        let slot = DecisionSlot::new(handle.clone(), self.kind, origin, event_tx);
        slot.announce();
        let request = DecisionRequest {
            kind: self.kind,
            reserved: false,
            item: &self.item,
        };
        let mut outcome = slot.decide(&request, cancel).await?;
        if self.kind == DecisionKind::Review
            && matches!(
                outcome.answer,
                Ok(DelegateOutput {
                    action: DelegateAction::Continue { .. },
                    ..
                })
            )
            && !handle.review_continue()
        {
            outcome.answer = Err(STREAK_REASON.into());
        }
        let applied = matches!(&outcome.answer, Ok(output)
            if !matches!(output.action, DelegateAction::Escalate));
        slot.record(&outcome, applied.then_some(DecisionOutcome::Applied));
        let action = outcome
            .answer
            .map_or(DelegateAction::Escalate, |output| output.action);
        Some(Verdict {
            seq: slot.seq(),
            action,
        })
    }
}

/// The first line of `lines` with text on it.
fn telling_line<'a>(lines: impl Iterator<Item = &'a str>) -> &'a str {
    lines
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
}

fn goal_lines(condition: &str, evaluations: u32) -> String {
    format!("\n{GOAL_LABEL} {condition}\n{EVALUATIONS_LABEL} {evaluations}")
}

/// What the delegate of decision `seq` writes into the conversation, framed
/// so the agent reads it as autopilot's rather than the user's own words.
pub fn on_behalf(seq: u32, kind: AutopilotMessageKind, text: &str) -> Message {
    Message::autopilot_observation(
        format!("{ON_BEHALF_PREFIX}\n{text}"),
        AutopilotOrigin { seq, kind },
    )
}

/// Asks the agent to report the group work item `work` it still holds.
pub fn unreported_work(seq: u32, work: &str) -> Message {
    Message::autopilot_observation(
        AUTOPILOT_UNREPORTED_WORK_PROMPT.replace(WORK_SLOT, &tag_safe_literal(work)),
        AutopilotOrigin {
            seq,
            kind: AutopilotMessageKind::Reminder,
        },
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use test_case::test_case;

    use super::*;
    use crate::agent::GoalHandle;

    const TITLE: &str = "# Plan: retry uploads";
    const QUESTION: &str = "Should I retry on every status?";
    const BODY: &str = "Wrap the upload in a retry loop.";
    const WORK: &str = "ci-triage-7";
    const REPLY_TEXT: &str = "Retry only on 5xx.";
    const GUIDANCE: &str = "Run the flaky test alone first.";
    const RULE: &str = "empty_response";
    const EVALUATOR_REASON: &str = "the upload endpoint was removed";
    const EVALUATIONS: u32 = 3;
    const LIMIT: u32 = 5;
    const SEQ: u32 = 4;
    const NAMED: &str = "an item must be named by the line that says what it is";
    const BOUNDED: &str = "an item must fit the delegate's bound";
    const FRAMED: &str = "autopilot's words must be marked as its own";
    const STALL_TOLD: &str = "a stall must say which it is and where the goal stands";
    const GUIDED: &str = "only a continue goes on, with its guidance as autopilot's";

    #[test_case(TurnEnd::plan(&format!("\n{TITLE}\n{BODY}\n{QUESTION}")), DecisionKind::Plan, TITLE ; "a_plan_by_its_title")]
    #[test_case(TurnEnd::reply(&format!("{BODY}\n{QUESTION}\n\n")), DecisionKind::Reply, QUESTION ; "a_reply_by_its_question")]
    #[test_case(TurnEnd::second_opinion(&format!("{TITLE}\n{BODY}"), EVALUATIONS, QUESTION), DecisionKind::SecondOpinion, TITLE ; "a_second_opinion_by_its_goal")]
    fn an_item_is_named_by_its_telling_line(review: TurnEnd, kind: DecisionKind, subject: &str) {
        assert_eq!(review.kind, kind);
        assert_eq!(review.subject, subject, "{NAMED}");
        assert!(review.item.contains(BODY));
    }

    fn goal(last_reason: Option<&str>) -> GoalSnapshot {
        let mut goal = GoalHandle::default().set(BODY).unwrap();
        goal.evaluations = EVALUATIONS;
        goal.last_reason = last_reason.map(Arc::from);
        goal
    }

    #[test_case(Stall::LoopCap { continuations: LIMIT }, LOOP_CAP_LABEL, &LIMIT.to_string() ; "loop_cap")]
    #[test_case(Stall::NoProgress { turns: LIMIT }, NO_PROGRESS_LABEL, &LIMIT.to_string() ; "no_progress")]
    #[test_case(Stall::TurnLimit { turns: LIMIT }, TURN_LIMIT_LABEL, &LIMIT.to_string() ; "turn_limit")]
    #[test_case(Stall::Steering { rule: RULE }, STEERING_LABEL, RULE ; "steering")]
    fn a_stall_says_which_it_is_and_where_the_goal_stands(stall: Stall, label: &str, detail: &str) {
        let with_reason = TurnEnd::stall(&stall, Some(&goal(Some(EVALUATOR_REASON))));
        let without_reason = TurnEnd::stall(&stall, Some(&goal(None)));
        let without_goal = TurnEnd::stall(&stall, None);

        assert_eq!(with_reason.kind, DecisionKind::Review);
        assert!(with_reason.subject.ends_with(detail), "{STALL_TOLD}");
        for expected in [
            format!("{label} {detail}"),
            format!("{GOAL_LABEL} {BODY}"),
            format!("{EVALUATIONS_LABEL} {EVALUATIONS}"),
            format!("{LAST_REASON_LABEL} {EVALUATOR_REASON}"),
        ] {
            assert!(with_reason.item.contains(&expected), "{STALL_TOLD}");
        }
        assert!(
            !without_reason.item.contains(LAST_REASON_LABEL),
            "{STALL_TOLD}"
        );
        assert_eq!(
            without_goal.item,
            format!("{label} {detail}"),
            "{STALL_TOLD}"
        );
    }

    #[test]
    fn a_second_opinion_carries_the_verdict_it_questions() {
        let review = TurnEnd::second_opinion(BODY, EVALUATIONS, EVALUATOR_REASON);

        for expected in [
            SECOND_OPINION_LABEL.to_owned(),
            format!("{GOAL_LABEL} {BODY}"),
            format!("{EVALUATIONS_LABEL} {EVALUATIONS}"),
            format!("{EVALUATOR_REASON_LABEL} {EVALUATOR_REASON}"),
        ] {
            assert!(review.item.contains(&expected), "{STALL_TOLD}");
        }
    }

    #[test_case(TurnEnd::plan ; "plan")]
    #[test_case(TurnEnd::reply ; "reply")]
    #[test_case(|text| TurnEnd::stall(&Stall::Steering { rule: text }, None) ; "stall")]
    #[test_case(|text| TurnEnd::second_opinion(text, EVALUATIONS, text) ; "second_opinion")]
    fn an_item_is_bounded(review: fn(&str) -> TurnEnd) {
        let text = BODY.repeat(MAX_ITEM_BYTES);

        let review = review(&text);

        assert!(review.item.len() <= MAX_ITEM_BYTES, "{BOUNDED}");
        assert!(review.subject.len() <= MAX_SUBJECT_BYTES, "{BOUNDED}");
    }

    #[test]
    fn autopilot_writes_on_the_users_behalf() {
        let message = on_behalf(SEQ, AutopilotMessageKind::Reply, REPLY_TEXT);

        let text = message.user_text().unwrap();
        assert!(text.starts_with(ON_BEHALF_PREFIX), "{FRAMED}");
        assert!(text.ends_with(REPLY_TEXT));
        assert!(message.is_observation(), "{FRAMED}");
        assert_eq!(
            message.autopilot_event,
            Some(AutopilotOrigin {
                seq: SEQ,
                kind: AutopilotMessageKind::Reply
            })
        );
    }

    #[test_case(DelegateAction::Continue { guidance: GUIDANCE.into() }, true ; "goes_on")]
    #[test_case(DelegateAction::Agree, false ; "agree")]
    #[test_case(DelegateAction::Escalate, false ; "escalate")]
    fn only_a_continue_goes_on_with_guidance(action: DelegateAction, guided: bool) {
        let message = Verdict { seq: SEQ, action }.guidance();

        assert_eq!(message.is_some(), guided, "{GUIDED}");
        if let Some(message) = message {
            assert_eq!(
                message.user_text(),
                Some(format!("{ON_BEHALF_PREFIX}\n{GUIDANCE}").as_str()),
                "{GUIDED}"
            );
            assert_eq!(
                message.autopilot_event,
                Some(AutopilotOrigin {
                    seq: SEQ,
                    kind: AutopilotMessageKind::Guidance
                }),
                "{GUIDED}"
            );
        }
    }

    #[test]
    fn the_unreported_work_reminder_names_the_item() {
        let message = unreported_work(SEQ, WORK);

        let text = message.user_text().unwrap();
        assert!(text.contains(&tag_safe_literal(WORK)));
        assert!(!text.contains(WORK_SLOT));
        assert_eq!(
            message.autopilot_event.map(|origin| origin.kind),
            Some(AutopilotMessageKind::Reminder)
        );
    }
}
