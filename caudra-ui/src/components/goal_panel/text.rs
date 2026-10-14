//! The wording every goal surface shares: the transcript rows a goal's
//! checks draw, how a goal ended, and what became of a check.

use std::time::Duration;

use caudra_agent::GoalVerdict;
use caudra_storage::goal_log::{GoalEnd, GoalEvaluationOutcome, StoredGoalEvaluation};

use crate::components::GoalTone;

pub(crate) const ACTIVE_END: &str = "active";
pub(crate) const MET_END: &str = "met";
pub(crate) const IMPOSSIBLE_END: &str = "impossible";
pub(crate) const CLEARED_END: &str = "cleared";
pub(crate) const REPLACED_END: &str = "replaced";
pub(crate) const ERROR_END: &str = "error";
pub(crate) const MET_STATUS: &str = "met";
pub(crate) const NOT_MET_STATUS: &str = "not met";
pub(crate) const IMPOSSIBLE_STATUS: &str = "impossible";
pub(crate) const RECONSIDERED_STATUS: &str = "reconsidered";
pub(crate) const FAILED_STATUS: &str = "failed";
pub(crate) const SKIPPED_STATUS: &str = "skipped";
pub(crate) const DEFERRED_STATUS: &str = "deferred";
pub(crate) const STALE_STATUS: &str = "stale";
const ACHIEVED_LABEL: &str = "Goal achieved";
const NOT_ACHIEVED_LABEL: &str = "Goal could not be achieved";
const SECONDS_PER_HOUR: u64 = 3_600;
const SECONDS_PER_MINUTE: u64 = 60;
const PERCENT: f64 = 100.0;

/// The row an applied not-met verdict draws.
pub(crate) fn not_met_row(evaluation: u32, reason: &str) -> String {
    format!("Goal not yet met (#{evaluation}): {reason}")
}

/// The row a failed check draws while its goal stays active.
pub(crate) fn evaluation_failed_row(evaluation: u32, message: &str) -> String {
    format!("Goal evaluation #{evaluation} failed; the goal remains active: {message}")
}

/// The row a goal's end by verdict draws, and the tone it draws in.
pub(crate) fn finished_row(verdict: GoalVerdict, reason: &str) -> (GoalTone, String) {
    let (tone, label) = match verdict {
        GoalVerdict::Met => (GoalTone::Done, ACHIEVED_LABEL),
        GoalVerdict::Impossible | GoalVerdict::NotMet => (GoalTone::Error, NOT_ACHIEVED_LABEL),
    };
    (tone, format!("{label}: {reason}"))
}

/// The row a goal the run cleared after it failed draws.
pub(crate) fn cleared_after_error_row(condition: &str, message: &str) -> String {
    format!("Goal cleared after an unrecoverable error: {condition} ({message})")
}

/// The transcript row a logged check drew when it was made, if any: a verdict
/// that took effect, or a failure that stood.
pub(crate) fn evaluation_row(evaluation: &StoredGoalEvaluation) -> Option<(GoalTone, String)> {
    match &evaluation.outcome {
        GoalEvaluationOutcome::Reconsidered => Some((
            GoalTone::Notice,
            not_met_row(evaluation.evaluation, &evaluation.reason),
        )),
        GoalEvaluationOutcome::Applied => match stored_verdict(evaluation)? {
            GoalVerdict::NotMet => Some((
                GoalTone::Notice,
                not_met_row(evaluation.evaluation, &evaluation.reason),
            )),
            verdict => Some(finished_row(verdict, &evaluation.reason)),
        },
        GoalEvaluationOutcome::Failed {
            message,
            applied: true,
        } => Some((
            GoalTone::Error,
            evaluation_failed_row(evaluation.evaluation, message),
        )),
        GoalEvaluationOutcome::Failed { applied: false, .. }
        | GoalEvaluationOutcome::Deferred
        | GoalEvaluationOutcome::Stale
        | GoalEvaluationOutcome::Skipped { .. } => None,
    }
}

/// The evaluator's own verdict as the log stores it.
fn stored_verdict(evaluation: &StoredGoalEvaluation) -> Option<GoalVerdict> {
    [
        GoalVerdict::Met,
        GoalVerdict::NotMet,
        GoalVerdict::Impossible,
    ]
    .into_iter()
    .find(|verdict| evaluation.verdict.as_deref() == Some(verdict.to_string().as_str()))
}

pub(crate) fn end_label(end: Option<&GoalEnd>) -> &'static str {
    match end {
        None => ACTIVE_END,
        Some(GoalEnd::Met) => MET_END,
        Some(GoalEnd::Impossible) => IMPOSSIBLE_END,
        Some(GoalEnd::Cleared) => CLEARED_END,
        Some(GoalEnd::Replaced) => REPLACED_END,
        Some(GoalEnd::Error { .. }) => ERROR_END,
    }
}

/// What became of a check, in a word or two.
pub(crate) fn status_label(evaluation: &StoredGoalEvaluation) -> &'static str {
    match &evaluation.outcome {
        GoalEvaluationOutcome::Applied => match stored_verdict(evaluation) {
            Some(GoalVerdict::Met) => MET_STATUS,
            Some(GoalVerdict::Impossible) => IMPOSSIBLE_STATUS,
            Some(GoalVerdict::NotMet) | None => NOT_MET_STATUS,
        },
        GoalEvaluationOutcome::Reconsidered => RECONSIDERED_STATUS,
        GoalEvaluationOutcome::Failed { .. } => FAILED_STATUS,
        GoalEvaluationOutcome::Skipped { .. } => SKIPPED_STATUS,
        GoalEvaluationOutcome::Deferred => DEFERRED_STATUS,
        GoalEvaluationOutcome::Stale => STALE_STATUS,
    }
}

/// What became of a check, as a sentence.
pub(crate) fn outcome_sentence(outcome: &GoalEvaluationOutcome) -> String {
    match outcome {
        GoalEvaluationOutcome::Applied => "The verdict took effect.".to_owned(),
        GoalEvaluationOutcome::Reconsidered => {
            "The evaluator judged the goal impossible; a second opinion overruled it, so the goal continued as not met.".to_owned()
        }
        GoalEvaluationOutcome::Deferred => {
            "Work was still running when the verdict came, so the check waits for it to settle.".to_owned()
        }
        GoalEvaluationOutcome::Stale => {
            "The goal was replaced or cleared before the verdict came, so it was discarded.".to_owned()
        }
        GoalEvaluationOutcome::Skipped { pass_probability } => format!(
            "The prescreen put the chance of a pass at {:.0}%, so the evaluator did not run.",
            pass_probability * PERCENT
        ),
        GoalEvaluationOutcome::Failed { applied: true, .. } => {
            "The check failed, and the goal stays active.".to_owned()
        }
        GoalEvaluationOutcome::Failed { applied: false, .. } => {
            "The check failed without standing: a fallback retried it, or its goal had moved on.".to_owned()
        }
    }
}

/// The failure's message, or the evaluator's reason.
pub(crate) fn evaluation_text(evaluation: &StoredGoalEvaluation) -> &str {
    match &evaluation.outcome {
        GoalEvaluationOutcome::Failed { message, .. } => message,
        _ => &evaluation.reason,
    }
}

pub(crate) fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / SECONDS_PER_HOUR;
    let minutes = seconds % SECONDS_PER_HOUR / SECONDS_PER_MINUTE;
    let seconds = seconds % SECONDS_PER_MINUTE;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use caudra_storage::goal_log::GoalEvaluationOutcome;
    use test_case::test_case;

    use super::*;

    const REASON: &str = "the suite still fails";
    const MESSAGE: &str = "rate limited";
    const EVALUATION: u32 = 2;

    fn evaluation(
        verdict: Option<GoalVerdict>,
        outcome: GoalEvaluationOutcome,
    ) -> StoredGoalEvaluation {
        StoredGoalEvaluation {
            seq: 1,
            goal: 1,
            evaluation: EVALUATION,
            started_at_ms: 0,
            at_ms: 0,
            history_len: 0,
            verdict: verdict.map(|verdict| verdict.to_string()),
            reason: REASON.into(),
            outcome,
            model: None,
            usage: None,
            cost: None,
        }
    }

    #[test]
    fn duration_is_compact() {
        assert_eq!(format_duration(Duration::from_secs(8)), "8s");
        assert_eq!(format_duration(Duration::from_secs(68)), "1m 8s");
        assert_eq!(format_duration(Duration::from_secs(3_668)), "1h 1m");
    }

    #[test_case(Some(GoalVerdict::NotMet), GoalEvaluationOutcome::Applied, Some((GoalTone::Notice, not_met_row(EVALUATION, REASON))) ; "not_met")]
    #[test_case(Some(GoalVerdict::Impossible), GoalEvaluationOutcome::Reconsidered, Some((GoalTone::Notice, not_met_row(EVALUATION, REASON))) ; "reconsidered_reads_not_met")]
    #[test_case(Some(GoalVerdict::Met), GoalEvaluationOutcome::Applied, Some(finished_row(GoalVerdict::Met, REASON)) ; "met")]
    #[test_case(Some(GoalVerdict::Impossible), GoalEvaluationOutcome::Applied, Some(finished_row(GoalVerdict::Impossible, REASON)) ; "impossible")]
    #[test_case(None, GoalEvaluationOutcome::Failed { message: MESSAGE.into(), applied: true }, Some((GoalTone::Error, evaluation_failed_row(EVALUATION, MESSAGE))) ; "a_failure_that_stood")]
    #[test_case(None, GoalEvaluationOutcome::Failed { message: MESSAGE.into(), applied: false }, None ; "a_failure_that_did_not")]
    #[test_case(Some(GoalVerdict::Met), GoalEvaluationOutcome::Stale, None ; "stale")]
    #[test_case(Some(GoalVerdict::Met), GoalEvaluationOutcome::Deferred, None ; "deferred")]
    #[test_case(None, GoalEvaluationOutcome::Skipped { pass_probability: 0.1 }, None ; "skipped")]
    fn a_check_redraws_the_row_it_drew(
        verdict: Option<GoalVerdict>,
        outcome: GoalEvaluationOutcome,
        row: Option<(GoalTone, String)>,
    ) {
        assert_eq!(evaluation_row(&evaluation(verdict, outcome)), row);
    }
}
