//! The Log tab: every check the session's goal log keeps, grouped under the
//! goal it judged, newest first, with what the evaluator said and what became
//! of it.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::iter;
use std::time::Duration;

use caudra_config::ClockFormat;
use caudra_providers::TokenUsage;
use caudra_storage::goal_log::{
    GoalEvaluationOutcome, GoalLogWrite, StoredGoalEvaluation, StoredGoalRecord,
};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::text::{end_label, evaluation_text, format_duration, outcome_sentence, status_label};
use crate::clock::{hm, hms};
use crate::components::audit_log::{
    AuditLog, DATE_FORMAT, EntryFilter, LogRow, counted, field, hint, local_time, section,
    text_lines,
};
use crate::components::escape_terminal_controls;
use crate::theme::Theme;

pub(crate) const READING_LOG: &str = "Reading the goal log\u{2026}";
pub(crate) const NO_GOAL_LOG: &str = "No goal log for this session yet";
pub(crate) const NOT_RECORDED: &str = "not recorded";
pub(crate) const REASON_HEADING: &str = "Reason";
pub(crate) const MESSAGE_HEADING: &str = "Message";
pub(crate) const CONDITION_HEADING: &str = "Condition";
pub(crate) const OUTCOME_HEADING: &str = "Outcome";
pub(crate) const SEPARATOR: &str = " \u{b7} ";
pub(crate) const SET_PREFIX: &str = "set";
const TITLE: &str = " Goal ";
const GOAL_MARK: &str = "#";
const EVALUATION_WORD: &str = "evaluation";
const EVALUATIONS_WORD: &str = "evaluations";
const SUBSCRIPTION_WORD: &str = "subscription";
const WORD_GAP: &str = " ";
const NONE: &str = "none";
const GOAL_LABEL: &str = "Goal";
const EVALUATION_LABEL: &str = "Evaluation";
const STARTED_LABEL: &str = "Started";
const FINISHED_LABEL: &str = "Finished";
const DURATION_LABEL: &str = "Duration";
const VERDICT_LABEL: &str = "Verdict";
const MODEL_LABEL: &str = "Model";
const SPEND_LABEL: &str = "Spend";
const ALL_FILTER: &str = "All";
const VERDICTS_FILTER: &str = "Verdicts";
const FAILURES_FILTER: &str = "Failures";
const DISCARDED_FILTER: &str = "Skipped & discarded";

/// A session's goal log as read when the panel opened, kept up to date with
/// what the session writes to it while the panel stays open.
#[derive(Debug, Default)]
pub(crate) struct GoalLog {
    goals: Vec<StoredGoalRecord>,
    evaluations: Vec<StoredGoalEvaluation>,
}

/// Which checks the list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LogFilter {
    #[default]
    All,
    /// Verdicts that took effect, the overruled impossible ones included.
    Verdicts,
    Failures,
    /// Checks the prescreen skipped, and verdicts that came too late or too
    /// early to count.
    Discarded,
}

impl EntryFilter for LogFilter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Verdicts,
            Self::Verdicts => Self::Failures,
            Self::Failures => Self::Discarded,
            Self::Discarded => Self::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => ALL_FILTER,
            Self::Verdicts => VERDICTS_FILTER,
            Self::Failures => FAILURES_FILTER,
            Self::Discarded => DISCARDED_FILTER,
        }
    }
}

impl LogFilter {
    fn admits(self, evaluation: &StoredGoalEvaluation) -> bool {
        match (self, &evaluation.outcome) {
            (Self::All, _)
            | (
                Self::Verdicts,
                GoalEvaluationOutcome::Applied | GoalEvaluationOutcome::Reconsidered,
            )
            | (Self::Failures, GoalEvaluationOutcome::Failed { .. })
            | (
                Self::Discarded,
                GoalEvaluationOutcome::Skipped { .. }
                | GoalEvaluationOutcome::Deferred
                | GoalEvaluationOutcome::Stale,
            ) => true,
            (Self::Verdicts | Self::Failures | Self::Discarded, _) => false,
        }
    }
}

impl GoalLog {
    pub(crate) fn new(
        goals: Vec<StoredGoalRecord>,
        evaluations: Vec<StoredGoalEvaluation>,
    ) -> Self {
        Self { goals, evaluations }
    }

    fn goal(&self, number: u32) -> Option<&StoredGoalRecord> {
        self.goals.iter().find(|kept| kept.goal == number)
    }

    fn goal_mut(&mut self, number: u32) -> Option<&mut StoredGoalRecord> {
        self.goals.iter_mut().find(|kept| kept.goal == number)
    }
}

impl AuditLog for GoalLog {
    type Write = GoalLogWrite;
    type Entry = StoredGoalEvaluation;
    type Filter = LogFilter;
    type Group = u32;
    const TITLE: &'static str = TITLE;
    #[cfg(debug_assertions)]
    const SCOPE: &'static str = "goal_panel";
    const READING: &'static str = READING_LOG;
    const EMPTY: &'static str = NO_GOAL_LOG;

    /// A goal or a check replaces the one under its number, and an end lands
    /// only on a goal the log holds.
    fn apply(&mut self, write: &GoalLogWrite) {
        match write {
            GoalLogWrite::Begun(goal) => match self.goal_mut(goal.goal) {
                Some(kept) => kept.clone_from(goal),
                None => self.goals.push(goal.clone()),
            },
            GoalLogWrite::Evaluated(evaluation) => match self
                .evaluations
                .iter_mut()
                .find(|kept| kept.seq == evaluation.seq)
            {
                Some(kept) => kept.clone_from(evaluation),
                None => self
                    .evaluations
                    .push(StoredGoalEvaluation::clone(evaluation)),
            },
            GoalLogWrite::Ended(ended) => {
                if let Some(kept) = self.goal_mut(ended.goal) {
                    kept.ended_at_ms = ended.ended_at_ms;
                    kept.ended_history_len = ended.ended_history_len;
                    kept.end.clone_from(&ended.end);
                    kept.evaluations = ended.evaluations;
                    kept.elapsed_ms = ended.elapsed_ms;
                    kept.usage = ended.usage;
                    kept.cost_usd = ended.cost_usd;
                    kept.subscription_cost_usd = ended.subscription_cost_usd;
                }
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.goals.is_empty() && self.evaluations.is_empty()
    }

    fn entry(&self, index: usize) -> Option<&StoredGoalEvaluation> {
        self.evaluations.get(index)
    }

    fn seq(evaluation: &StoredGoalEvaluation) -> u64 {
        evaluation.seq
    }

    /// Newest goal first and its newest check first. Unfiltered, a goal never
    /// checked still has its header; filtered, a goal with nothing to list
    /// has none.
    fn rows(&self, filter: LogFilter) -> Vec<LogRow<u32>> {
        let mut groups: BTreeMap<u32, (usize, Vec<usize>)> = BTreeMap::new();
        if filter == LogFilter::All {
            for goal in &self.goals {
                groups.entry(goal.goal).or_default();
            }
        }
        for (index, evaluation) in self.evaluations.iter().enumerate() {
            let (total, listed) = groups.entry(evaluation.goal).or_default();
            *total += 1;
            if filter.admits(evaluation) {
                listed.push(index);
            }
        }
        groups
            .into_iter()
            .rev()
            .filter(|(_, (_, listed))| filter == LogFilter::All || !listed.is_empty())
            .flat_map(|(group, (entries, mut listed))| {
                listed.sort_by_key(|index| Reverse(self.evaluations[*index].seq));
                iter::once(LogRow::Header { group, entries })
                    .chain(listed.into_iter().map(LogRow::Entry))
            })
            .collect()
    }

    /// Such as `#3 · the suite is green · set 14:02 · met · 4 evaluations ·
    /// 2m 3s · $0.42`. A goal the log trimmed keeps its number and count.
    fn header(&self, number: u32, entries: usize, clock: ClockFormat) -> String {
        let mut parts = vec![format!("{GOAL_MARK}{number}")];
        let goal = self.goal(number);
        if let Some(goal) = goal {
            parts.push(escape_terminal_controls(&goal.condition));
            parts.push(format!(
                "{SET_PREFIX} {}",
                local_time(goal.set_at_ms, hm(clock))
            ));
            parts.push(end_label(goal.end.as_ref()).to_owned());
        }
        parts.push(counted(
            u64::try_from(entries).unwrap_or(u64::MAX),
            EVALUATION_WORD,
            EVALUATIONS_WORD,
        ));
        if let Some(goal) = goal.filter(|goal| goal.end.is_some()) {
            parts.push(format_duration(Duration::from_millis(goal.elapsed_ms)));
            parts.extend(goal_spend(goal));
        }
        parts.join(SEPARATOR)
    }

    /// `time #evaluation status · duration · model · cost · reason`.
    fn entry_spans(
        evaluation: &StoredGoalEvaluation,
        clock: ClockFormat,
        theme: &Theme,
    ) -> Vec<Span<'static>> {
        let mut details = vec![format_duration(check_duration(evaluation))];
        details.extend(evaluation.model.as_deref().map(escape_terminal_controls));
        details.extend(evaluation.cost.map(|cost| format!("${cost:.3}")));
        details.extend(
            evaluation_text(evaluation)
                .lines()
                .next()
                .filter(|line| !line.is_empty())
                .map(escape_terminal_controls),
        );
        vec![
            Span::styled(
                format!("{}{WORD_GAP}", entry_time(evaluation.at_ms, clock)),
                theme.tool_dim,
            ),
            Span::raw(format!("{GOAL_MARK}{}{WORD_GAP}", evaluation.evaluation)),
            Span::styled(
                status_label(evaluation),
                status_style(&evaluation.outcome, theme),
            ),
            Span::styled(
                format!("{SEPARATOR}{}", details.join(SEPARATOR)),
                theme.tool_dim,
            ),
        ]
    }

    /// Everything the log keeps about a check, under its title.
    fn entry_detail(
        &self,
        evaluation: &StoredGoalEvaluation,
        clock: ClockFormat,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let full = format!("{DATE_FORMAT}{}", hms(clock));
        let mut lines = vec![
            Line::styled(
                format!(
                    "{GOAL_MARK}{}{SEPARATOR}{}",
                    evaluation.evaluation,
                    status_label(evaluation)
                ),
                status_style(&evaluation.outcome, theme).patch(theme.bold),
            ),
            Line::default(),
            field(GOAL_LABEL, format!("{GOAL_MARK}{}", evaluation.goal), theme),
            field(
                EVALUATION_LABEL,
                format!("{GOAL_MARK}{}", evaluation.evaluation),
                theme,
            ),
            field(
                STARTED_LABEL,
                local_time(evaluation.started_at_ms, &full),
                theme,
            ),
            field(FINISHED_LABEL, local_time(evaluation.at_ms, &full), theme),
            field(
                DURATION_LABEL,
                format_duration(check_duration(evaluation)),
                theme,
            ),
            field(
                VERDICT_LABEL,
                evaluation
                    .verdict
                    .as_deref()
                    .map_or_else(|| NONE.to_owned(), escape_terminal_controls),
                theme,
            ),
        ];
        section(
            &mut lines,
            CONDITION_HEADING,
            self.goal(evaluation.goal).map_or_else(
                || vec![hint(NOT_RECORDED, theme)],
                |goal| text_lines(&goal.condition, Style::default()),
            ),
            theme,
        );
        section(
            &mut lines,
            OUTCOME_HEADING,
            text_lines(&outcome_sentence(&evaluation.outcome), Style::default()),
            theme,
        );
        let heading = match evaluation.outcome {
            GoalEvaluationOutcome::Failed { .. } => MESSAGE_HEADING,
            _ => REASON_HEADING,
        };
        section(
            &mut lines,
            heading,
            match evaluation_text(evaluation) {
                "" => vec![hint(NONE, theme)],
                text => text_lines(text, Style::default()),
            },
            theme,
        );
        lines.push(Line::default());
        lines.push(field(
            MODEL_LABEL,
            evaluation
                .model
                .as_deref()
                .map_or_else(|| NONE.to_owned(), escape_terminal_controls),
            theme,
        ));
        lines.push(field(
            SPEND_LABEL,
            evaluation_spend(evaluation).unwrap_or_else(|| NONE.to_owned()),
            theme,
        ));
        lines
    }
}

fn check_duration(evaluation: &StoredGoalEvaluation) -> Duration {
    Duration::from_millis(evaluation.at_ms.saturating_sub(evaluation.started_at_ms))
}

/// The time a check was filed, its date first when it was not today.
fn entry_time(at_ms: u64, clock: ClockFormat) -> String {
    let today = Timestamp::now().to_zoned(TimeZone::system()).date();
    let same_day = i64::try_from(at_ms)
        .ok()
        .and_then(|millis| Timestamp::from_millisecond(millis).ok())
        .is_some_and(|at| at.to_zoned(TimeZone::system()).date() == today);
    match same_day {
        true => local_time(at_ms, hms(clock)),
        false => local_time(at_ms, &format!("{DATE_FORMAT}{}", hms(clock))),
    }
}

fn status_style(outcome: &GoalEvaluationOutcome, theme: &Theme) -> Style {
    match outcome {
        GoalEvaluationOutcome::Applied | GoalEvaluationOutcome::Reconsidered => Style::default(),
        GoalEvaluationOutcome::Failed { .. } => theme.tool_error,
        GoalEvaluationOutcome::Skipped { .. }
        | GoalEvaluationOutcome::Deferred
        | GoalEvaluationOutcome::Stale => theme.tool_dim,
    }
}

/// A check's tokens and cost, as much of it as is known.
fn evaluation_spend(evaluation: &StoredGoalEvaluation) -> Option<String> {
    match (evaluation.usage, evaluation.cost) {
        (Some(usage), cost) => Some(TokenUsage::from(usage).format(cost)),
        (None, Some(cost)) => Some(format!("${cost:.3}")),
        (None, None) => None,
    }
}

/// What an ended goal spent, each kind once spent.
fn goal_spend(goal: &StoredGoalRecord) -> Vec<String> {
    let mut parts = Vec::new();
    if goal.cost_usd > 0.0 {
        parts.push(format!("${:.2}", goal.cost_usd));
    }
    if goal.subscription_cost_usd > 0.0 {
        parts.push(format!(
            "${:.2} {SUBSCRIPTION_WORD}",
            goal.subscription_cost_usd
        ));
    }
    parts
}

#[cfg(test)]
pub(crate) mod fixtures {
    use caudra_storage::goal_log::{GoalEnd, GoalEvaluationOutcome};
    use caudra_storage::sessions::StoredTokenUsage;

    use super::*;

    pub(crate) const CONDITION: &str = "the suite is green";
    pub(crate) const REASON: &str = "two tests still fail";

    pub(crate) fn goal(number: u32, end: Option<GoalEnd>) -> StoredGoalRecord {
        StoredGoalRecord {
            goal: number,
            condition: CONDITION.into(),
            set_at_ms: 0,
            set_history_len: 0,
            ended_at_ms: end.as_ref().map(|_| 0),
            ended_history_len: end.as_ref().map(|_| 0),
            end,
            evaluations: 0,
            elapsed_ms: 0,
            usage: StoredTokenUsage::default(),
            cost_usd: 0.0,
            subscription_cost_usd: 0.0,
        }
    }

    pub(crate) fn evaluation(
        seq: u64,
        goal: u32,
        outcome: GoalEvaluationOutcome,
    ) -> StoredGoalEvaluation {
        StoredGoalEvaluation {
            seq,
            goal,
            evaluation: u32::try_from(seq).unwrap_or(u32::MAX),
            started_at_ms: 0,
            at_ms: 0,
            history_len: 0,
            verdict: Some(caudra_agent::GoalVerdict::NotMet.to_string()),
            reason: REASON.into(),
            outcome,
            model: None,
            usage: None,
            cost: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use caudra_storage::goal_log::GoalEnd;
    use test_case::test_case;

    use super::super::text::{ACTIVE_END, FAILED_STATUS, MET_END, NOT_MET_STATUS};
    use super::fixtures::{CONDITION, REASON, evaluation, goal};
    use super::*;
    use crate::theme;

    const FIRST: u32 = 1;
    const SECOND: u32 = 2;
    const MESSAGE: &str = "rate limited";
    const NEWEST_FIRST: &str = "goals and their checks read newest first";

    fn failed() -> GoalEvaluationOutcome {
        GoalEvaluationOutcome::Failed {
            message: MESSAGE.into(),
            applied: true,
        }
    }

    /// Two goals, the first met, with a check of each outcome kind.
    fn log() -> GoalLog {
        GoalLog::new(
            vec![goal(FIRST, Some(GoalEnd::Met)), goal(SECOND, None)],
            vec![
                evaluation(1, FIRST, GoalEvaluationOutcome::Applied),
                evaluation(2, FIRST, failed()),
                evaluation(
                    3,
                    SECOND,
                    GoalEvaluationOutcome::Skipped {
                        pass_probability: 0.2,
                    },
                ),
                evaluation(4, SECOND, GoalEvaluationOutcome::Reconsidered),
                evaluation(5, SECOND, GoalEvaluationOutcome::Stale),
            ],
        )
    }

    fn entries(log: &GoalLog, filter: LogFilter) -> Vec<u64> {
        log.rows(filter)
            .into_iter()
            .filter_map(|row| match row {
                LogRow::Entry(index) => log.entry(index).map(|evaluation| evaluation.seq),
                LogRow::Header { .. } => None,
            })
            .collect()
    }

    #[test]
    fn rows_group_by_goal_newest_first() {
        let rows = log().rows(LogFilter::All);

        assert_eq!(
            rows,
            [
                LogRow::Header {
                    group: SECOND,
                    entries: 3
                },
                LogRow::Entry(4),
                LogRow::Entry(3),
                LogRow::Entry(2),
                LogRow::Header {
                    group: FIRST,
                    entries: 2
                },
                LogRow::Entry(1),
                LogRow::Entry(0),
            ],
            "{NEWEST_FIRST}"
        );
    }

    #[test_case(LogFilter::All, LogFilter::Verdicts, &[5, 4, 3, 2, 1] ; "all")]
    #[test_case(LogFilter::Verdicts, LogFilter::Failures, &[4, 1] ; "verdicts")]
    #[test_case(LogFilter::Failures, LogFilter::Discarded, &[2] ; "failures")]
    #[test_case(LogFilter::Discarded, LogFilter::All, &[5, 3] ; "discarded_wraps")]
    fn each_filter_lists_its_checks_and_steps_to_the_next(
        filter: LogFilter,
        next: LogFilter,
        listed: &[u64],
    ) {
        assert_eq!(entries(&log(), filter), listed);
        assert_eq!(filter.next(), next);
    }

    #[test]
    fn a_filter_drops_goals_with_nothing_to_list() {
        let rows = log().rows(LogFilter::Failures);

        assert_eq!(
            rows,
            [
                LogRow::Header {
                    group: FIRST,
                    entries: 2
                },
                LogRow::Entry(1),
            ]
        );
    }

    #[test]
    fn headers_name_the_goal_its_end_and_its_count() {
        let log = log();
        let clock = ClockFormat::default();
        let set = format!("{SET_PREFIX} {}", local_time(0, hm(clock)));

        assert_eq!(
            log.header(FIRST, 2, clock),
            [
                format!("{GOAL_MARK}{FIRST}"),
                CONDITION.to_owned(),
                set.clone(),
                MET_END.to_owned(),
                format!("2 {EVALUATIONS_WORD}"),
                format_duration(Duration::ZERO),
            ]
            .join(SEPARATOR)
        );
        assert_eq!(
            log.header(SECOND, 1, clock),
            [
                format!("{GOAL_MARK}{SECOND}"),
                CONDITION.to_owned(),
                set,
                ACTIVE_END.to_owned(),
                format!("1 {EVALUATION_WORD}"),
            ]
            .join(SEPARATOR)
        );
    }

    #[test_case(GoalEvaluationOutcome::Applied, NOT_MET_STATUS, REASON ; "a_verdict_shows_its_reason")]
    #[test_case(failed(), FAILED_STATUS, MESSAGE ; "a_failure_shows_its_message")]
    fn a_row_names_its_status_and_first_line(
        outcome: GoalEvaluationOutcome,
        status: &str,
        text: &str,
    ) {
        let row: String = GoalLog::entry_spans(
            &evaluation(1, FIRST, outcome),
            ClockFormat::default(),
            &theme::current(),
        )
        .iter()
        .map(|span| span.content.as_ref())
        .collect();

        assert!(row.contains(status) && row.contains(text), "{row}");
    }

    #[test]
    fn the_detail_shows_the_condition_outcome_and_message() {
        let log = log();
        let shown = log
            .entry_detail(
                &evaluation(2, FIRST, failed()),
                ClockFormat::default(),
                &theme::current(),
            )
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        for part in [
            CONDITION_HEADING,
            CONDITION,
            OUTCOME_HEADING,
            MESSAGE_HEADING,
            MESSAGE,
            VERDICT_LABEL,
        ] {
            assert!(shown.contains(part), "{part}\n{shown}");
        }
    }

    #[test]
    fn writes_update_the_log_in_place() {
        let mut log = GoalLog::default();
        log.apply(&GoalLogWrite::Begun(goal(FIRST, None)));
        log.apply(&GoalLogWrite::Evaluated(Box::new(evaluation(
            1,
            FIRST,
            GoalEvaluationOutcome::Applied,
        ))));
        log.apply(&GoalLogWrite::Ended(goal(FIRST, Some(GoalEnd::Met))));
        log.apply(&GoalLogWrite::Ended(goal(SECOND, Some(GoalEnd::Cleared))));

        assert_eq!(entries(&log, LogFilter::All), [1]);
        assert!(
            log.header(FIRST, 1, ClockFormat::default())
                .contains(MET_END)
        );
        assert_eq!(
            log.goals.len(),
            1,
            "an end lands only on a goal the log holds"
        );
    }
}
