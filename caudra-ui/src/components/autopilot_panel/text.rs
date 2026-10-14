//! The wording every autopilot surface shares: a decision's line and details,
//! what became of it, why an engagement ended and what it spent, and the notes
//! a card carries while autopilot decides it or once it left it to the user.

use caudra_agent::autopilot::ActionName;
use caudra_agent::permissions::AutopilotNote;
use caudra_config::AutopilotConfig;
use caudra_providers::TokenUsage;
use caudra_storage::autopilot::{
    AutopilotBudget, AutopilotEnd, DecisionOutcome, DecisionPayload, StoredAutopilot,
    StoredAutopilotDecision, StoredAutopilotEngagement,
};

use crate::components::audit_log::counted;

const DECIDING_NOTE: &str = "Autopilot is deciding. Press any key to answer yourself.";
const LEFT_TO_YOU_NOTE: &str = "Autopilot left this to you";
const SENTENCE_END: char = '.';
pub(crate) const STOPPED: &str = "stopped";
pub(crate) const GOAL_MET: &str = "goal met";
pub(crate) const GOAL_IMPOSSIBLE: &str = "goal impossible";
pub(crate) const CONTINUATIONS_SPENT: &str = "continuation limit reached";
pub(crate) const COST_SPENT: &str = "cost limit reached";
pub(crate) const TIME_SPENT: &str = "time limit reached";
pub(crate) const ERROR_END: &str = "error";
const LEFT_TO_YOU_LEAD: &str = "left the";
const LEFT_TO_YOU_TAIL: &str = "to you";
pub(crate) const REASON_SEPARATOR: &str = " · ";
pub(crate) const HUMAN_FIRST_TAG: &str = "you answered first";
pub(crate) const TAKEN_OVER_TAG: &str = "you took it";
pub(crate) const ANSWERED_TAG: &str = "you";
pub(crate) const SUPERSEDED_TAG: &str = "superseded";
pub(crate) const CANCELLED_TAG: &str = "cancelled";
pub(crate) const DENIED_UNATTENDED_TAG: &str = "denied, nobody to ask";
pub(crate) const LEFT_TO_YOU_TAG: &str = "left to you";
const APPLIED_OUTCOME: &str = "autopilot's answer took effect";
const HUMAN_FIRST_OUTCOME: &str = "you answered before autopilot";
const TAKEN_OVER_OUTCOME: &str = "you took it over while autopilot was deciding";
const SUPERSEDED_OUTCOME: &str = "settled before autopilot answered";
const CANCELLED_OUTCOME: &str = "the run stopped before anyone answered";
const ANSWERED_OUTCOME: &str = "left to you, and you answered";
const DENIED_UNATTENDED_OUTCOME: &str = "left to you with nobody to ask, so it was denied";
const GUIDANCE_LABEL: &str = "Guidance";
const REPLY_LABEL: &str = "Reply";
const FEEDBACK_LABEL: &str = "Feedback";
const ANSWER_LABEL: &str = "Answer";
const NOTHING_PICKED: &str = "nothing picked";
const LABEL_SEPARATOR: &str = ", ";
pub(crate) const OUTCOME_LABEL: &str = "Outcome";
pub(crate) const AGENT_LABEL: &str = "Agent";
pub(crate) const MAIN_AGENT: &str = "main";
pub(crate) const DELEGATE_LABEL: &str = "Delegate";
const CONTINUATION_WORD: &str = "continuation";
const RETRY_WORD: &str = "retry";
const RETRIES_WORD: &str = "retries";
const SUBSCRIPTION_WORD: &str = "subscription";
/// Joins the words of a kind's wire name, such as `second_opinion`.
const NAME_WORD_JOINER: char = '_';
const WORD_SEPARATOR: &str = " ";
pub(crate) const ENGAGED_STATE: &str = "Engaged";
pub(crate) const DECIDING_STATE: &str = "Deciding";
pub(crate) const WAITING_STATE: &str = "Waiting on you";
const ENDED_STATE: &str = "Ended";
pub(super) const OWNED_GOAL_HEADING: &str = "Goal (autopilot's own)";
pub(super) const GOAL_HEADING: &str = "Goal";
pub(super) const NO_GOAL: &str = "No goal";
pub(super) const MINUTES: &str = "min";
const LEFT_SUFFIX: &str = "left";
pub(super) const BUDGET_SEPARATOR: &str = " · ";
const CONTINUATIONS_WORD: &str = "continuations";
pub(super) const MS_PER_MINUTE: u64 = 60_000;

pub(crate) fn end_reason(reason: &AutopilotEnd) -> &str {
    match reason {
        AutopilotEnd::Error { message } => message,
        other => end_notice(other),
    }
}

/// The end in fixed words, for text that can leave the terminal: an
/// error's message stays out of it.
pub(crate) fn end_notice(reason: &AutopilotEnd) -> &'static str {
    match reason {
        AutopilotEnd::Stopped { .. } => STOPPED,
        AutopilotEnd::GoalMet => GOAL_MET,
        AutopilotEnd::GoalImpossible => GOAL_IMPOSSIBLE,
        AutopilotEnd::Budget {
            kind: AutopilotBudget::Continuations,
        } => CONTINUATIONS_SPENT,
        AutopilotEnd::Budget {
            kind: AutopilotBudget::Cost,
        } => COST_SPENT,
        AutopilotEnd::Budget {
            kind: AutopilotBudget::Time,
        } => TIME_SPENT,
        AutopilotEnd::Error { .. } => ERROR_END,
    }
}

/// What was decided about what, and why. An escalation reads as the thing
/// handed back to the user. `subject` goes in as given, so a caller drawing
/// markdown quotes it first.
pub(crate) fn decision_text(kind: &str, action: &str, subject: &str, reason: &str) -> String {
    let kind = kind_words(kind);
    let head = if action == ActionName::Escalate.name() {
        format!("{LEFT_TO_YOU_LEAD} {kind} {LEFT_TO_YOU_TAIL}")
    } else {
        format!("{kind} {action}")
    };
    if subject.is_empty() {
        format!("{head}{REASON_SEPARATOR}{reason}")
    } else {
        format!("{head}: {subject}{REASON_SEPARATOR}{reason}")
    }
}

/// A kind's wire name as words, such as `second opinion`.
pub(super) fn kind_words(kind: &str) -> String {
    kind.replace(NAME_WORD_JOINER, WORD_SEPARATOR)
}

/// A decision's line as [`decision_text`] reads it, or for one autopilot
/// stood down from, its kind and subject. What became of it follows unless
/// autopilot's own action took effect. `subject` goes in as given.
pub(crate) fn decision_title(decision: &StoredAutopilotDecision, subject: &str) -> String {
    let head = match &decision.action {
        Some(action) => decision_text(&decision.kind, action, subject, &decision.reason),
        None if subject.is_empty() => kind_words(&decision.kind),
        None => format!("{}: {subject}", kind_words(&decision.kind)),
    };
    match decision.outcome.as_ref().and_then(outcome_tag) {
        Some(tag) => format!("{head}{REASON_SEPARATOR}{tag}"),
        None => head,
    }
}

/// What became of a decision in a few words, `None` when autopilot's own
/// action took effect.
pub(crate) fn outcome_tag(outcome: &DecisionOutcome) -> Option<String> {
    let tag = match outcome {
        DecisionOutcome::Applied => return None,
        DecisionOutcome::Answered { answer } => return Some(format!("{ANSWERED_TAG}: {answer}")),
        DecisionOutcome::HumanFirst { .. } => HUMAN_FIRST_TAG,
        DecisionOutcome::TakenOver => TAKEN_OVER_TAG,
        DecisionOutcome::Superseded => SUPERSEDED_TAG,
        DecisionOutcome::Cancelled => CANCELLED_TAG,
        DecisionOutcome::DeniedUnattended => DENIED_UNATTENDED_TAG,
    };
    Some(tag.to_owned())
}

/// Whether autopilot handed the decision back to the user.
pub(super) fn is_escalation(decision: &StoredAutopilotDecision) -> bool {
    decision.action.as_deref() == Some(ActionName::Escalate.name())
}

/// What happened once autopilot decided, in words. An escalation nothing is
/// known to have followed was left to the user. `None` when nothing is known,
/// as on a row stored before outcomes were.
pub(crate) fn outcome_sentence(decision: &StoredAutopilotDecision) -> Option<String> {
    let sentence = match &decision.outcome {
        Some(DecisionOutcome::HumanFirst { answer }) => {
            return Some(format!("{HUMAN_FIRST_OUTCOME}: {answer}"));
        }
        Some(DecisionOutcome::Answered { answer }) => {
            return Some(format!("{ANSWERED_OUTCOME}: {answer}"));
        }
        Some(DecisionOutcome::Applied) => APPLIED_OUTCOME,
        Some(DecisionOutcome::TakenOver) => TAKEN_OVER_OUTCOME,
        Some(DecisionOutcome::Superseded) => SUPERSEDED_OUTCOME,
        Some(DecisionOutcome::Cancelled) => CANCELLED_OUTCOME,
        Some(DecisionOutcome::DeniedUnattended) => DENIED_UNATTENDED_OUTCOME,
        None if is_escalation(decision) => LEFT_TO_YOU_TAG,
        None => return None,
    };
    Some(sentence.to_owned())
}

/// What autopilot said beside its reason: its guidance, its reply, its
/// feedback, or the labels it picked for each question, one line each.
pub(crate) fn payload_lines(payload: &DecisionPayload) -> Vec<String> {
    match payload {
        DecisionPayload::Guidance(text) => vec![format!("{GUIDANCE_LABEL}: {text}")],
        DecisionPayload::Text(text) => vec![format!("{REPLY_LABEL}: {text}")],
        DecisionPayload::Feedback(text) => vec![format!("{FEEDBACK_LABEL}: {text}")],
        DecisionPayload::Answers(answers) => answers
            .iter()
            .enumerate()
            .map(|(index, labels)| {
                let picked = if labels.is_empty() {
                    NOTHING_PICKED.to_owned()
                } else {
                    labels.join(LABEL_SEPARATOR)
                };
                format!("{ANSWER_LABEL} {}: {picked}", index + 1)
            })
            .collect(),
    }
}

/// The agent a decision was made for, [`MAIN_AGENT`] for the main one.
pub(crate) fn agent_name(decision: &StoredAutopilotDecision) -> &str {
    decision.agent.as_deref().unwrap_or(MAIN_AGENT)
}

/// The delegate's model, tokens and cost, as much of it as is known.
pub(crate) fn delegate_spend(decision: &StoredAutopilotDecision) -> Option<String> {
    let spend = match (decision.usage, decision.cost) {
        (Some(usage), cost) => Some(TokenUsage::from(usage).format(cost)),
        (None, Some(cost)) => Some(format!("${cost:.3}")),
        (None, None) => None,
    };
    let parts: Vec<String> = decision.model.iter().cloned().chain(spend).collect();
    (!parts.is_empty()).then(|| parts.join(REASON_SEPARATOR))
}

/// What a decision's transcript row opens to: what autopilot said, what
/// happened, the agent, and the delegate's model and spend. What the
/// delegate was shown stays in the log.
pub(crate) fn decision_details(decision: &StoredAutopilotDecision) -> Vec<String> {
    let mut details = decision
        .payload
        .as_ref()
        .map(payload_lines)
        .unwrap_or_default();
    details.extend(outcome_sentence(decision).map(|outcome| format!("{OUTCOME_LABEL}: {outcome}")));
    details.push(format!("{AGENT_LABEL}: {}", agent_name(decision)));
    details.extend(delegate_spend(decision).map(|spend| format!("{DELEGATE_LABEL}: {spend}")));
    details
}

/// What an engagement used, such as `12 continuations · $0.42 · 18 min ·
/// 2 retries`. Continuations always count, and the rest only once spent.
pub(crate) fn spend_summary(engagement: &StoredAutopilotEngagement) -> String {
    let mut parts = vec![counted(
        engagement.continuations,
        CONTINUATION_WORD,
        CONTINUATIONS_WORD,
    )];
    if engagement.cost_usd > 0.0 {
        parts.push(format!("${:.2}", engagement.cost_usd));
    }
    if engagement.subscription_cost_usd > 0.0 {
        parts.push(format!(
            "${:.2} {SUBSCRIPTION_WORD}",
            engagement.subscription_cost_usd
        ));
    }
    let minutes = engagement.elapsed_ms / MS_PER_MINUTE;
    if minutes > 0 {
        parts.push(format!("{minutes} {MINUTES}"));
    }
    if engagement.retries > 0 {
        parts.push(counted(engagement.retries, RETRY_WORD, RETRIES_WORD));
    }
    parts.join(BUDGET_SEPARATOR)
}

/// The line a card carries while autopilot decides it or once autopilot
/// handed it back.
pub(crate) fn card_note(note: &AutopilotNote) -> String {
    match note {
        AutopilotNote::Deciding => DECIDING_NOTE.to_owned(),
        AutopilotNote::Escalated { reason } => format!(
            "{LEFT_TO_YOU_NOTE}: {}{SENTENCE_END}",
            reason.trim().trim_end_matches(SENTENCE_END)
        ),
    }
}

/// The goal in one line, saying when autopilot set it.
pub(crate) fn goal_line(goal: Option<&str>, owns_goal: bool) -> String {
    match goal {
        Some(goal) if owns_goal => format!("{OWNED_GOAL_HEADING}: {goal}"),
        Some(goal) => format!("{GOAL_HEADING}: {goal}"),
        None => NO_GOAL.to_owned(),
    }
}

/// The engagement's state in a word or two.
pub(crate) fn state_label(stored: &StoredAutopilot, deciding: bool, escalated: bool) -> String {
    match &stored.last_end {
        Some(end) if !stored.engaged => format!("{ENDED_STATE}: {}", end_reason(end)),
        _ if escalated => WAITING_STATE.to_owned(),
        _ if deciding => DECIDING_STATE.to_owned(),
        _ => ENGAGED_STATE.to_owned(),
    }
}

/// What the engagement may still spend, limit by limit, such as
/// `88 continuations left · $1.50 left · 25 min left`. A limit that is not set
/// has nothing to count down.
pub(crate) fn budget_left(stored: &StoredAutopilot, limits: &AutopilotConfig) -> String {
    let mut left = Vec::new();
    if let Some(max) = limits.max_continuations {
        left.push(format!(
            "{} {CONTINUATIONS_WORD} {LEFT_SUFFIX}",
            max.saturating_sub(stored.continuations)
        ));
    }
    if let Some(max) = limits.max_cost {
        left.push(format!(
            "${:.2} {LEFT_SUFFIX}",
            (max - stored.cost_usd).max(0.0)
        ));
    }
    if let Some(max) = limits.max_minutes {
        left.push(format!(
            "{} {MINUTES} {LEFT_SUFFIX}",
            u64::from(max).saturating_sub(stored.elapsed_ms / MS_PER_MINUTE)
        ));
    }
    left.join(BUDGET_SEPARATOR)
}

#[cfg(test)]
pub(super) mod fixtures {
    use caudra_agent::autopilot::{ActionName, AutopilotDecision, DecisionKind};
    use caudra_storage::autopilot::{
        AutopilotEnd, DecisionOutcome, StoredAutopilot, StoredAutopilotDecision,
        StoredAutopilotEngagement,
    };

    use super::MS_PER_MINUTE;

    pub(crate) const SUBJECT: &str = "shell: cargo test";
    pub(crate) const REASON: &str = "the tests need running";
    pub(crate) const CONTINUATIONS: u32 = 12;
    pub(crate) const COST: f64 = 0.5;
    pub(crate) const ELAPSED_MS: u64 = 5 * MS_PER_MINUTE;

    pub(crate) fn stored(engaged: bool, last_end: Option<AutopilotEnd>) -> StoredAutopilot {
        StoredAutopilot {
            engaged,
            engagement: 1,
            owns_goal: true,
            engaged_at_ms: None,
            continuations: CONTINUATIONS,
            cost_usd: COST,
            subscription_cost_usd: 0.0,
            elapsed_ms: ELAPSED_MS,
            retries: 0,
            last_end,
        }
    }

    pub(crate) fn decision() -> AutopilotDecision {
        AutopilotDecision {
            seq: 0,
            at_ms: 0,
            kind: DecisionKind::Permission,
            action: ActionName::Allow,
            subject: SUBJECT.into(),
            reason: REASON.into(),
            agent: None,
            tool_use_id: None,
            history_len: 0,
            model: None,
            usage: None,
            cost: None,
            engagement: None,
            item: None,
            payload: None,
            outcome: None,
        }
    }

    pub(crate) fn stored_decision(
        action: Option<ActionName>,
        outcome: Option<DecisionOutcome>,
    ) -> StoredAutopilotDecision {
        StoredAutopilotDecision {
            action: action.map(|action| action.name().to_owned()),
            outcome,
            ..decision().into()
        }
    }

    pub(crate) fn engagement(
        continuations: u32,
        cost_usd: f64,
        subscription: f64,
        elapsed_ms: u64,
        retries: u32,
    ) -> StoredAutopilotEngagement {
        StoredAutopilotEngagement {
            engagement: 1,
            engaged_at_ms: 0,
            engaged_history_len: 0,
            goal: None,
            owns_goal: false,
            ended_at_ms: None,
            ended_history_len: None,
            end: None,
            continuations,
            cost_usd,
            subscription_cost_usd: subscription,
            elapsed_ms,
            retries,
        }
    }
}

#[cfg(test)]
mod tests {
    use caudra_agent::autopilot::DecisionKind;
    use caudra_storage::autopilot::AutopilotStopper;
    use test_case::test_case;

    use super::fixtures::{
        CONTINUATIONS, COST, ELAPSED_MS, REASON, SUBJECT, engagement, stored, stored_decision,
    };
    use super::*;

    const SUBAGENT: &str = "research";
    const REASON_SENTENCE: &str = "the tests need running.";
    const BOUND_MODEL: &str = "anthropic/claude-haiku-4-5";
    const GOAL: &str = "the suite is green";
    const MAX_CONTINUATIONS: u32 = 100;
    const MAX_COST: f64 = 2.0;
    const MAX_MINUTES: u32 = 30;

    fn limits() -> AutopilotConfig {
        AutopilotConfig {
            max_continuations: Some(MAX_CONTINUATIONS),
            max_cost: Some(MAX_COST),
            max_minutes: Some(MAX_MINUTES),
            ..AutopilotConfig::default()
        }
    }

    #[test_case("permission", "allow", SUBJECT, format!("permission allow: {SUBJECT}{REASON_SEPARATOR}{REASON}") ; "a_decision")]
    #[test_case("second_opinion", "agree", "", format!("second opinion agree{REASON_SEPARATOR}{REASON}") ; "words_of_a_kind_and_no_subject")]
    #[test_case("question", "escalate", SUBJECT, format!("{LEFT_TO_YOU_LEAD} question {LEFT_TO_YOU_TAIL}: {SUBJECT}{REASON_SEPARATOR}{REASON}") ; "an_escalation")]
    fn a_decision_reads_as_what_was_decided_and_why(
        kind: &str,
        action: &str,
        subject: &str,
        expected: String,
    ) {
        assert_eq!(decision_text(kind, action, subject, REASON), expected);
    }

    #[test_case(stored(true, None), false, false, ENGAGED_STATE ; "engaged")]
    #[test_case(stored(true, None), true, false, DECIDING_STATE ; "deciding")]
    #[test_case(stored(true, None), true, true, WAITING_STATE ; "an_escalation_outranks_deciding")]
    fn the_state_names_what_autopilot_is_doing(
        stored: StoredAutopilot,
        deciding: bool,
        escalated: bool,
        expected: &str,
    ) {
        assert_eq!(state_label(&stored, deciding, escalated), expected);
    }

    #[test]
    fn an_ended_engagement_names_why() {
        let ended = stored(
            false,
            Some(AutopilotEnd::Stopped {
                by: AutopilotStopper::Command,
            }),
        );
        assert_eq!(
            state_label(&ended, false, false),
            format!("{ENDED_STATE}: {STOPPED}")
        );
    }

    #[test]
    fn the_budget_left_counts_down_every_limit() {
        assert_eq!(
            budget_left(&stored(true, None), &limits()),
            [
                format!(
                    "{} {CONTINUATIONS_WORD} {LEFT_SUFFIX}",
                    MAX_CONTINUATIONS - CONTINUATIONS
                ),
                format!("${:.2} {LEFT_SUFFIX}", MAX_COST - COST),
                format!(
                    "{} {MINUTES} {LEFT_SUFFIX}",
                    u64::from(MAX_MINUTES) - ELAPSED_MS / MS_PER_MINUTE
                ),
            ]
            .join(BUDGET_SEPARATOR)
        );
    }

    #[test]
    fn an_uncapped_engagement_has_no_continuations_to_count_down() {
        let uncapped = AutopilotConfig {
            max_continuations: None,
            ..limits()
        };

        assert!(!budget_left(&stored(true, None), &uncapped).contains(CONTINUATIONS_WORD));
    }

    #[test_case(engagement(0, 0.0, 0.0, 0, 0), format!("0 {CONTINUATIONS_WORD}") ; "nothing_spent")]
    #[test_case(engagement(1, 0.0, 0.0, MS_PER_MINUTE - 1, 1), format!("1 {CONTINUATION_WORD}{BUDGET_SEPARATOR}1 {RETRY_WORD}") ; "one_of_each_and_under_a_minute")]
    #[test_case(engagement(CONTINUATIONS, COST, COST, ELAPSED_MS, 2), format!("{CONTINUATIONS} {CONTINUATIONS_WORD}{BUDGET_SEPARATOR}${COST:.2}{BUDGET_SEPARATOR}${COST:.2} {SUBSCRIPTION_WORD}{BUDGET_SEPARATOR}{} {MINUTES}{BUDGET_SEPARATOR}2 {RETRIES_WORD}", ELAPSED_MS / MS_PER_MINUTE) ; "everything")]
    fn the_spend_summary_names_only_what_was_spent(
        engagement: StoredAutopilotEngagement,
        expected: String,
    ) {
        assert_eq!(spend_summary(&engagement), expected);
    }

    #[test_case(Some(ActionName::Allow), Some(DecisionOutcome::Applied), String::new() ; "applied_has_no_tag")]
    #[test_case(Some(ActionName::Escalate), None, String::new() ; "a_bare_escalation_has_no_tag")]
    #[test_case(Some(ActionName::Allow), Some(DecisionOutcome::HumanFirst { answer: GOAL.into() }), format!("{REASON_SEPARATOR}{HUMAN_FIRST_TAG}") ; "human_first")]
    #[test_case(Some(ActionName::Escalate), Some(DecisionOutcome::Answered { answer: GOAL.into() }), format!("{REASON_SEPARATOR}{ANSWERED_TAG}: {GOAL}") ; "answered")]
    fn a_decision_title_tags_what_became_of_it(
        action: Option<ActionName>,
        outcome: Option<DecisionOutcome>,
        tag: String,
    ) {
        let decision = stored_decision(action, outcome);
        let line = decision_text(
            &decision.kind,
            decision.action.as_deref().unwrap_or_default(),
            SUBJECT,
            REASON,
        );

        assert_eq!(decision_title(&decision, SUBJECT), format!("{line}{tag}"));
    }

    #[test]
    fn a_stood_down_title_names_the_kind_and_who_took_it() {
        let decision = stored_decision(None, Some(DecisionOutcome::TakenOver));

        assert_eq!(
            decision_title(&decision, SUBJECT),
            format!(
                "{}: {SUBJECT}{REASON_SEPARATOR}{TAKEN_OVER_TAG}",
                DecisionKind::Permission.name()
            )
        );
    }

    #[test]
    fn the_details_say_what_autopilot_said_what_happened_and_who_paid() {
        let decision = StoredAutopilotDecision {
            payload: Some(DecisionPayload::Answers(vec![
                vec![GOAL.into(), SUBJECT.into()],
                Vec::new(),
            ])),
            agent: Some(SUBAGENT.into()),
            model: Some(BOUND_MODEL.into()),
            cost: Some(COST),
            ..stored_decision(Some(ActionName::Answer), Some(DecisionOutcome::Applied))
        };

        assert_eq!(
            decision_details(&decision),
            [
                format!("{ANSWER_LABEL} 1: {GOAL}{LABEL_SEPARATOR}{SUBJECT}"),
                format!("{ANSWER_LABEL} 2: {NOTHING_PICKED}"),
                format!("{OUTCOME_LABEL}: {APPLIED_OUTCOME}"),
                format!("{AGENT_LABEL}: {SUBAGENT}"),
                format!("{DELEGATE_LABEL}: {BOUND_MODEL}{REASON_SEPARATOR}${COST:.3}"),
            ]
        );
    }

    #[test_case(REASON ; "bare")]
    #[test_case(REASON_SENTENCE ; "already_a_sentence")]
    fn the_left_to_you_note_is_one_sentence(reason: &str) {
        let note = AutopilotNote::Escalated {
            reason: reason.into(),
        };
        assert_eq!(
            card_note(&note),
            format!("{LEFT_TO_YOU_NOTE}: {REASON}{SENTENCE_END}")
        );
    }
}
