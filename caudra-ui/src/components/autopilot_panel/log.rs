//! The Log tab: every decision the session's log keeps, grouped under the
//! engagement it was made in, newest first, with what autopilot was shown,
//! what it said and what became of it.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::iter;

use caudra_agent::autopilot::DecisionKind;
use caudra_config::ClockFormat;
use caudra_storage::autopilot::{
    DecisionOutcome, StoredAutopilotDecision, StoredAutopilotEngagement,
};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::text::{
    AGENT_LABEL, DELEGATE_LABEL, LEFT_TO_YOU_TAG, OUTCOME_LABEL, REASON_SEPARATOR, agent_name,
    decision_title, delegate_spend, end_reason, is_escalation, kind_words, outcome_sentence,
    outcome_tag, payload_lines,
};
use crate::clock::{hm, hms};
use crate::components::audit_log::{
    AuditLog, DATE_FORMAT, EntryFilter, LogRow, counted, field, hint, local_time, section,
    text_lines,
};
use crate::components::escape_terminal_controls;
use crate::storage_writer::AutopilotWrite;
use crate::theme::Theme;

pub(crate) const NOT_RECORDED: &str = "not recorded";
pub(crate) const EARLIER_DECISIONS: &str = "Earlier decisions";
pub(crate) const ENGAGEMENT_HEADER: &str = "Engagement";
pub(crate) const ENGAGED_SINCE: &str = "engaged since";
pub(crate) const SPAN_JOINER: &str = "to";
pub(crate) const ASKED_HEADING: &str = "Asked";
pub(crate) const SAID_HEADING: &str = "Autopilot said";
pub(crate) const REASON_HEADING: &str = "Reason";
pub(crate) const STOOD_DOWN_ACTION: &str = "none, autopilot stood down";
const DECISION_WORD: &str = "decision";
const DECISIONS_WORD: &str = "decisions";
const HISTORY_ITEM_WORD: &str = "history item";
const HISTORY_ITEMS_WORD: &str = "history items";
const TRANSCRIPT_PREFIX: &str = "after";
const NONE: &str = "none";
const KIND_LABEL: &str = "Kind";
const ACTION_LABEL: &str = "Action";
const TIME_LABEL: &str = "Time";
const ENGAGEMENT_LABEL: &str = "Engagement";
const SUBJECT_LABEL: &str = "Subject";
const TRANSCRIPT_LABEL: &str = "Transcript";
const WORD_GAP: &str = " ";
const ALL_FILTER: &str = "All";
const LEFT_TO_YOU_FILTER: &str = "Left to you";
const TITLE: &str = " Autopilot ";
const SCOPE: &str = "autopilot_panel";
pub(crate) const READING_LOG: &str = "Reading the autopilot log\u{2026}";
pub(crate) const NO_ENTRIES: &str = "No autopilot log for this session yet";

/// A session's log as read when the panel opened, kept up to date with what
/// the session writes to it while the panel stays open.
#[derive(Debug, Default)]
pub(crate) struct AutopilotLog {
    decisions: Vec<StoredAutopilotDecision>,
    engagements: Vec<StoredAutopilotEngagement>,
}

/// Ordered so the newest engagement sorts last, and the rows stored before
/// decisions named their engagement first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Group {
    Earlier,
    Engagement(u32),
}

/// Which decisions the list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LogFilter {
    #[default]
    All,
    /// What autopilot left to the user, and what the user answered or took.
    LeftToYou,
    Kind(DecisionKind),
}

impl EntryFilter for LogFilter {
    /// All, Left to you, then each kind in [`DecisionKind::ALL`] order.
    fn next(self) -> Self {
        let kinds = DecisionKind::ALL;
        match self {
            Self::All => Self::LeftToYou,
            Self::LeftToYou => Self::Kind(kinds[0]),
            Self::Kind(kind) => kinds
                .iter()
                .skip_while(|candidate| **candidate != kind)
                .nth(1)
                .map_or(Self::All, |next| Self::Kind(*next)),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => ALL_FILTER,
            Self::LeftToYou => LEFT_TO_YOU_FILTER,
            Self::Kind(DecisionKind::Permission) => "Permission",
            Self::Kind(DecisionKind::Question) => "Question",
            Self::Kind(DecisionKind::Reply) => "Reply",
            Self::Kind(DecisionKind::Plan) => "Plan",
            Self::Kind(DecisionKind::Review) => "Review",
            Self::Kind(DecisionKind::SecondOpinion) => "Second opinion",
        }
    }
}

impl LogFilter {
    fn admits(self, decision: &StoredAutopilotDecision) -> bool {
        match self {
            Self::All => true,
            Self::LeftToYou => {
                is_escalation(decision)
                    || matches!(
                        decision.outcome,
                        Some(
                            DecisionOutcome::HumanFirst { .. }
                                | DecisionOutcome::TakenOver
                                | DecisionOutcome::Answered { .. }
                                | DecisionOutcome::DeniedUnattended
                        )
                    )
            }
            Self::Kind(kind) => decision.kind == kind.name(),
        }
    }
}

impl AutopilotLog {
    pub(crate) fn new(
        decisions: Vec<StoredAutopilotDecision>,
        engagements: Vec<StoredAutopilotEngagement>,
    ) -> Self {
        Self {
            decisions,
            engagements,
        }
    }

    fn engagement_mut(&mut self, number: u32) -> Option<&mut StoredAutopilotEngagement> {
        self.engagements
            .iter_mut()
            .find(|kept| kept.engagement == number)
    }
}

impl AuditLog for AutopilotLog {
    type Write = AutopilotWrite;
    type Entry = StoredAutopilotDecision;
    type Filter = LogFilter;
    type Group = Group;
    const TITLE: &'static str = TITLE;
    const SCOPE: &'static str = SCOPE;
    const READING: &'static str = READING_LOG;
    const EMPTY: &'static str = NO_ENTRIES;

    /// A decision replaces the one with its seq, and an end lands only on an
    /// engagement the log holds.
    fn apply(&mut self, write: &AutopilotWrite) {
        match write {
            AutopilotWrite::Decision(decision) => {
                match self
                    .decisions
                    .iter_mut()
                    .find(|kept| kept.seq == decision.seq)
                {
                    Some(kept) => kept.clone_from(decision),
                    None => self
                        .decisions
                        .push(StoredAutopilotDecision::clone(decision)),
                }
            }
            AutopilotWrite::Settle { seq, outcome } => {
                if let Some(kept) = self.decisions.iter_mut().find(|kept| kept.seq == *seq) {
                    kept.outcome = Some(outcome.clone());
                }
            }
            AutopilotWrite::Engaged(engagement) => match self.engagement_mut(engagement.engagement)
            {
                Some(kept) => kept.clone_from(engagement),
                None => self.engagements.push(engagement.clone()),
            },
            AutopilotWrite::Ended(ended) => {
                if let Some(kept) = self.engagement_mut(ended.engagement) {
                    kept.ended_at_ms = ended.ended_at_ms;
                    kept.ended_history_len = ended.ended_history_len;
                    kept.end.clone_from(&ended.end);
                    kept.continuations = ended.continuations;
                    kept.cost_usd = ended.cost_usd;
                    kept.subscription_cost_usd = ended.subscription_cost_usd;
                    kept.elapsed_ms = ended.elapsed_ms;
                    kept.retries = ended.retries;
                }
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.decisions.is_empty() && self.engagements.is_empty()
    }

    fn entry(&self, index: usize) -> Option<&StoredAutopilotDecision> {
        self.decisions.get(index)
    }

    fn seq(decision: &StoredAutopilotDecision) -> u64 {
        decision.seq
    }

    /// The list under `filter`: newest engagement first and its newest
    /// decision first, with the decisions stored before engagements were
    /// numbered last. Unfiltered, an engagement that decided nothing still
    /// has its header; filtered, a group with nothing to list has none.
    fn rows(&self, filter: LogFilter) -> Vec<LogRow<Group>> {
        let mut groups: BTreeMap<Group, (usize, Vec<usize>)> = BTreeMap::new();
        if filter == LogFilter::All {
            for engagement in &self.engagements {
                groups
                    .entry(Group::Engagement(engagement.engagement))
                    .or_default();
            }
        }
        for (index, decision) in self.decisions.iter().enumerate() {
            let group = decision
                .engagement
                .map_or(Group::Earlier, Group::Engagement);
            let (total, listed) = groups.entry(group).or_default();
            *total += 1;
            if filter.admits(decision) {
                listed.push(index);
            }
        }
        groups
            .into_iter()
            .rev()
            .filter(|(_, (_, listed))| filter == LogFilter::All || !listed.is_empty())
            .flat_map(|(group, (entries, mut listed))| {
                listed.sort_by_key(|index| Reverse(self.decisions[*index].seq));
                iter::once(LogRow::Header { group, entries })
                    .chain(listed.into_iter().map(LogRow::Entry))
            })
            .collect()
    }

    /// A group's header, such as `Engagement 3 · 14:02 to 14:41 · goal met ·
    /// 23 decisions · $0.42`. The one still running reads `engaged since
    /// 14:02`.
    fn header(&self, group: Group, decisions: usize, clock: ClockFormat) -> String {
        let Group::Engagement(number) = group else {
            return EARLIER_DECISIONS.to_owned();
        };
        let mut parts = vec![format!("{ENGAGEMENT_HEADER} {number}")];
        let engagement = self
            .engagements
            .iter()
            .find(|kept| kept.engagement == number);
        if let Some(engagement) = engagement {
            let started = local_time(engagement.engaged_at_ms, hm(clock));
            parts.push(match engagement.ended_at_ms {
                Some(ended) => format!("{started} {SPAN_JOINER} {}", local_time(ended, hm(clock))),
                None => format!("{ENGAGED_SINCE} {started}"),
            });
            parts.extend(
                engagement
                    .end
                    .as_ref()
                    .map(|end| end_reason(end).to_owned()),
            );
        }
        parts.push(counted(
            u64::try_from(decisions).unwrap_or(u64::MAX),
            DECISION_WORD,
            DECISIONS_WORD,
        ));
        parts.extend(
            engagement
                .filter(|engagement| engagement.cost_usd > 0.0)
                .map(|engagement| format!("${:.2}", engagement.cost_usd)),
        );
        parts.join(REASON_SEPARATOR)
    }

    /// A list row: `time kind action subject`, then its tag. An escalation reads
    /// in the colour of what waits on the user.
    fn entry_spans(
        decision: &StoredAutopilotDecision,
        clock: ClockFormat,
        theme: &Theme,
    ) -> Vec<Span<'static>> {
        let style = escalation_style(decision, theme);
        let mut words = vec![kind_words(&decision.kind)];
        words.extend(decision.action.clone());
        if !decision.subject.is_empty() {
            words.push(escape_terminal_controls(&decision.subject));
        }
        let mut spans = vec![
            Span::styled(
                format!("{}{WORD_GAP}", local_time(decision.at_ms, hms(clock))),
                theme.tool_dim,
            ),
            Span::styled(words.join(WORD_GAP), style),
        ];
        spans.extend(entry_tag(decision).map(|tag| {
            Span::styled(
                format!("{REASON_SEPARATOR}{}", escape_terminal_controls(&tag)),
                theme.tool_dim,
            )
        }));
        spans
    }

    /// Everything the log keeps about a decision, under its title. A field an
    /// older row never stored reads [`NOT_RECORDED`].
    fn entry_detail(
        &self,
        decision: &StoredAutopilotDecision,
        clock: ClockFormat,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let old = decision.engagement.is_none();
        let mut lines = vec![
            Line::styled(
                escape_terminal_controls(&decision_title(decision, &decision.subject)),
                escalation_style(decision, theme).patch(theme.bold),
            ),
            Line::default(),
            field(KIND_LABEL, kind_words(&decision.kind), theme),
            field(
                ACTION_LABEL,
                decision
                    .action
                    .clone()
                    .unwrap_or_else(|| STOOD_DOWN_ACTION.to_owned()),
                theme,
            ),
            field(
                TIME_LABEL,
                local_time(decision.at_ms, &format!("{DATE_FORMAT}{}", hms(clock))),
                theme,
            ),
            field(
                ENGAGEMENT_LABEL,
                decision
                    .engagement
                    .map_or_else(|| NOT_RECORDED.to_owned(), |number| number.to_string()),
                theme,
            ),
            field(
                SUBJECT_LABEL,
                match decision.subject.as_str() {
                    "" => NONE.to_owned(),
                    subject => escape_terminal_controls(subject),
                },
                theme,
            ),
        ];
        section(
            &mut lines,
            ASKED_HEADING,
            decision.item.as_deref().map_or_else(
                || vec![hint(NOT_RECORDED, theme)],
                |item| text_lines(item, theme.inline_code),
            ),
            theme,
        );
        let said = match &decision.payload {
            Some(payload) => Some(
                payload_lines(payload)
                    .iter()
                    .flat_map(|line| text_lines(line, Style::default()))
                    .collect(),
            ),
            None if old => Some(vec![hint(NOT_RECORDED, theme)]),
            None => None,
        };
        if let Some(said) = said {
            section(&mut lines, SAID_HEADING, said, theme);
        }
        section(
            &mut lines,
            REASON_HEADING,
            match decision.reason.as_str() {
                "" => vec![hint(NONE, theme)],
                reason => text_lines(reason, Style::default()),
            },
            theme,
        );
        section(
            &mut lines,
            OUTCOME_LABEL,
            outcome_sentence(decision).map_or_else(
                || vec![hint(NOT_RECORDED, theme)],
                |outcome| text_lines(&outcome, Style::default()),
            ),
            theme,
        );
        lines.push(Line::default());
        lines.push(field(
            AGENT_LABEL,
            escape_terminal_controls(agent_name(decision)),
            theme,
        ));
        lines.push(field(
            DELEGATE_LABEL,
            delegate_spend(decision).map_or_else(
                || NOT_RECORDED.to_owned(),
                |spend| escape_terminal_controls(&spend),
            ),
            theme,
        ));
        lines.push(field(
            TRANSCRIPT_LABEL,
            format!(
                "{TRANSCRIPT_PREFIX} {}",
                counted(
                    u64::try_from(decision.history_len).unwrap_or(u64::MAX),
                    HISTORY_ITEM_WORD,
                    HISTORY_ITEMS_WORD,
                )
            ),
            theme,
        ));
        lines
    }
}

/// What a decision was tagged with in the list: what became of it, or that
/// autopilot left it to the user and nothing is known to have followed.
pub(super) fn entry_tag(decision: &StoredAutopilotDecision) -> Option<String> {
    match &decision.outcome {
        Some(outcome) => outcome_tag(outcome),
        None if is_escalation(decision) => Some(LEFT_TO_YOU_TAG.to_owned()),
        None => None,
    }
}

fn escalation_style(decision: &StoredAutopilotDecision, theme: &Theme) -> Style {
    if is_escalation(decision) {
        theme.todo_in_progress
    } else {
        Style::default()
    }
}

#[cfg(test)]
mod tests {
    use caudra_agent::autopilot::ActionName;
    use caudra_storage::autopilot::{AutopilotEnd, DecisionPayload};
    use test_case::test_case;

    use super::super::text::fixtures::{decision, engagement};
    use super::super::text::{GOAL_MET, TAKEN_OVER_TAG};
    use super::*;
    use crate::theme;

    const FIRST: u32 = 1;
    const SECOND: u32 = 2;
    const ITEM: &str = "shell command:\ncargo test --workspace";
    const GUIDANCE: &str = "run the narrower suite first";
    const COST: f64 = 0.42;
    const ANSWER: &str = "allow once";
    const EARLIER_LAST: &str = "decisions stored before engagements were numbered come last";
    const NEWEST_FIRST: &str = "engagements and their decisions read newest first";

    fn logged(
        seq: u64,
        engagement: Option<u32>,
        kind: DecisionKind,
        action: Option<ActionName>,
        outcome: Option<DecisionOutcome>,
    ) -> StoredAutopilotDecision {
        StoredAutopilotDecision {
            seq,
            kind: kind.name().to_owned(),
            action: action.map(|action| action.name().to_owned()),
            engagement,
            outcome,
            ..decision().into()
        }
    }

    fn engaged(number: u32, end: Option<AutopilotEnd>) -> StoredAutopilotEngagement {
        StoredAutopilotEngagement {
            engagement: number,
            ended_at_ms: end.as_ref().map(|_| 0),
            end,
            cost_usd: COST,
            ..engagement(0, 0.0, 0.0, 0, 0)
        }
    }

    /// An older row, then two engagements with one of each kind of what can
    /// become of a decision.
    fn log() -> AutopilotLog {
        AutopilotLog::new(
            vec![
                logged(
                    0,
                    None,
                    DecisionKind::Permission,
                    Some(ActionName::Allow),
                    None,
                ),
                logged(
                    1,
                    Some(FIRST),
                    DecisionKind::Question,
                    Some(ActionName::Escalate),
                    None,
                ),
                logged(
                    2,
                    Some(FIRST),
                    DecisionKind::Permission,
                    Some(ActionName::Allow),
                    Some(DecisionOutcome::Applied),
                ),
                logged(
                    3,
                    Some(SECOND),
                    DecisionKind::Plan,
                    None,
                    Some(DecisionOutcome::TakenOver),
                ),
                logged(
                    4,
                    Some(SECOND),
                    DecisionKind::Reply,
                    Some(ActionName::Reply),
                    Some(DecisionOutcome::Applied),
                ),
            ],
            vec![
                engaged(FIRST, Some(AutopilotEnd::GoalMet)),
                engaged(SECOND, None),
            ],
        )
    }

    fn entries(log: &AutopilotLog, filter: LogFilter) -> Vec<u64> {
        log.rows(filter)
            .into_iter()
            .filter_map(|row| match row {
                LogRow::Entry(index) => log.entry(index).map(|decision| decision.seq),
                LogRow::Header { .. } => None,
            })
            .collect()
    }

    #[test]
    fn rows_group_by_engagement_newest_first_with_earlier_decisions_last() {
        let log = log();

        let rows = log.rows(LogFilter::All);

        assert_eq!(
            rows,
            [
                LogRow::Header {
                    group: Group::Engagement(SECOND),
                    entries: 2
                },
                LogRow::Entry(4),
                LogRow::Entry(3),
                LogRow::Header {
                    group: Group::Engagement(FIRST),
                    entries: 2
                },
                LogRow::Entry(2),
                LogRow::Entry(1),
                LogRow::Header {
                    group: Group::Earlier,
                    entries: 1
                },
                LogRow::Entry(0),
            ],
            "{NEWEST_FIRST}; {EARLIER_LAST}"
        );
    }

    #[test_case(LogFilter::All, LogFilter::LeftToYou, &[4, 3, 2, 1, 0] ; "all")]
    #[test_case(LogFilter::LeftToYou, LogFilter::Kind(DecisionKind::Permission), &[3, 1] ; "left_to_you")]
    #[test_case(LogFilter::Kind(DecisionKind::Permission), LogFilter::Kind(DecisionKind::Question), &[2, 0] ; "permission")]
    #[test_case(LogFilter::Kind(DecisionKind::Question), LogFilter::Kind(DecisionKind::Reply), &[1] ; "question")]
    #[test_case(LogFilter::Kind(DecisionKind::Reply), LogFilter::Kind(DecisionKind::Plan), &[4] ; "reply")]
    #[test_case(LogFilter::Kind(DecisionKind::Plan), LogFilter::Kind(DecisionKind::Review), &[3] ; "plan")]
    #[test_case(LogFilter::Kind(DecisionKind::Review), LogFilter::Kind(DecisionKind::SecondOpinion), &[] ; "review")]
    #[test_case(LogFilter::Kind(DecisionKind::SecondOpinion), LogFilter::All, &[] ; "second_opinion_wraps")]
    fn each_filter_lists_its_decisions_and_steps_to_the_next(
        filter: LogFilter,
        next: LogFilter,
        listed: &[u64],
    ) {
        let log = log();

        assert_eq!(entries(&log, filter), listed);
        assert_eq!(filter.next(), next);
    }

    #[test]
    fn a_filter_drops_groups_with_nothing_to_list() {
        let rows = log().rows(LogFilter::Kind(DecisionKind::Plan));

        assert_eq!(
            rows,
            [
                LogRow::Header {
                    group: Group::Engagement(SECOND),
                    entries: 2
                },
                LogRow::Entry(3),
            ]
        );
    }

    #[test]
    fn headers_name_the_span_the_end_the_count_and_the_cost() {
        let log = log();
        let clock = ClockFormat::default();
        let at = local_time(0, hm(clock));

        assert_eq!(
            log.header(Group::Engagement(FIRST), 2, clock),
            [
                format!("{ENGAGEMENT_HEADER} {FIRST}"),
                format!("{at} {SPAN_JOINER} {at}"),
                GOAL_MET.to_owned(),
                format!("2 {DECISIONS_WORD}"),
                format!("${COST:.2}"),
            ]
            .join(REASON_SEPARATOR)
        );
        assert_eq!(
            log.header(Group::Engagement(SECOND), 1, clock),
            [
                format!("{ENGAGEMENT_HEADER} {SECOND}"),
                format!("{ENGAGED_SINCE} {at}"),
                format!("1 {DECISION_WORD}"),
                format!("${COST:.2}"),
            ]
            .join(REASON_SEPARATOR)
        );
        assert_eq!(log.header(Group::Earlier, 1, clock), EARLIER_DECISIONS);
    }

    #[test_case(Some(ActionName::Escalate), None, Some(LEFT_TO_YOU_TAG) ; "a_bare_escalation_is_left_to_you")]
    #[test_case(None, Some(DecisionOutcome::TakenOver), Some(TAKEN_OVER_TAG) ; "taken_over")]
    #[test_case(Some(ActionName::Allow), Some(DecisionOutcome::Applied), None ; "applied")]
    fn a_row_is_tagged_with_what_became_of_it(
        action: Option<ActionName>,
        outcome: Option<DecisionOutcome>,
        tag: Option<&str>,
    ) {
        let decision = logged(0, Some(FIRST), DecisionKind::Question, action, outcome);

        assert_eq!(entry_tag(&decision).as_deref(), tag);
    }

    fn detail_text(decision: &StoredAutopilotDecision) -> String {
        AutopilotLog::default()
            .entry_detail(decision, ClockFormat::default(), &theme::current())
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_detail_shows_what_was_asked_said_and_what_followed() {
        let decision = StoredAutopilotDecision {
            item: Some(ITEM.into()),
            payload: Some(DecisionPayload::Guidance(GUIDANCE.into())),
            cost: Some(COST),
            ..logged(
                0,
                Some(FIRST),
                DecisionKind::Permission,
                Some(ActionName::Deny),
                Some(DecisionOutcome::HumanFirst {
                    answer: ANSWER.into(),
                }),
            )
        };

        let shown = detail_text(&decision);

        for part in [
            ASKED_HEADING,
            "cargo test --workspace",
            SAID_HEADING,
            GUIDANCE,
            REASON_HEADING,
            OUTCOME_LABEL,
            ANSWER,
            AGENT_LABEL,
            TRANSCRIPT_LABEL,
        ] {
            assert!(shown.contains(part), "{part}\n{shown}");
        }
        assert!(!shown.contains(NOT_RECORDED), "{shown}");
    }

    #[test]
    fn an_old_row_reads_not_recorded_where_it_stored_nothing() {
        let decision = logged(
            0,
            None,
            DecisionKind::Permission,
            Some(ActionName::Allow),
            None,
        );

        let shown = detail_text(&decision);

        for label in [
            ENGAGEMENT_LABEL,
            ASKED_HEADING,
            SAID_HEADING,
            OUTCOME_LABEL,
            DELEGATE_LABEL,
        ] {
            let after = shown
                .split_once(label)
                .map(|(_, rest)| rest.lines().take(2).collect::<String>());
            assert!(
                after.is_some_and(|after| after.contains(NOT_RECORDED)),
                "{label}\n{shown}"
            );
        }
    }

    #[test]
    fn a_stood_down_row_has_no_action_and_nothing_said() {
        let decision = logged(
            0,
            Some(FIRST),
            DecisionKind::Plan,
            None,
            Some(DecisionOutcome::TakenOver),
        );

        let shown = detail_text(&decision);

        assert!(shown.contains(STOOD_DOWN_ACTION), "{shown}");
        assert!(!shown.contains(SAID_HEADING), "{shown}");
    }

    #[test]
    fn writes_update_the_log_in_place() {
        let mut log = log();
        let decided = logged(
            5,
            Some(SECOND),
            DecisionKind::Review,
            Some(ActionName::Escalate),
            None,
        );

        log.apply(&AutopilotWrite::Decision(Box::new(decided.clone())));
        log.apply(&AutopilotWrite::Decision(Box::new(decided)));
        log.apply(&AutopilotWrite::Settle {
            seq: 5,
            outcome: DecisionOutcome::Answered {
                answer: ANSWER.into(),
            },
        });
        log.apply(&AutopilotWrite::Ended(engaged(
            SECOND,
            Some(AutopilotEnd::GoalMet),
        )));

        assert_eq!(entries(&log, LogFilter::All), [5, 4, 3, 2, 1, 0]);
        assert_eq!(
            log.entry(5).and_then(|decision| decision.outcome.clone()),
            Some(DecisionOutcome::Answered {
                answer: ANSWER.into()
            })
        );
        assert!(
            log.header(Group::Engagement(SECOND), 3, ClockFormat::default())
                .contains(GOAL_MET)
        );
    }
}
