//! The Status tab: the goal active or last finished, its evaluator, the
//! session's continuation limit, and what the goal has spent.

use caudra_agent::{GoalStatus, GoalVerdict};
use caudra_providers::model_registry::Binding;
use ratatui::text::{Line, Span};

use super::text::format_duration;
use crate::theme::Theme;

pub(crate) const NO_GOAL: &str = "No goal set";
pub(crate) const SET_HINT: &str = "/goal <condition> sets one";
const UNBOUND_EVALUATOR: &str = "default (fast, then chat)";
const ACTIVE_TITLE: &str = "ACTIVE";
const ACHIEVED_TITLE: &str = "ACHIEVED";
const NOT_ACHIEVED_TITLE: &str = "NOT ACHIEVED";
const CONDITION_HEADING: &str = "Condition";
const REASON_HEADING: &str = "Reason";
const EVALUATOR_LABEL: &str = "Evaluator  ";
const CONTINUATIONS_LABEL: &str = "Automatic continuations  ";
const ADJUST_HINT: &str = "    \u{2190}/\u{2192} adjust for this session";
const EVALUATIONS_LABEL: &str = "Evaluations  ";
const ELAPSED_LABEL: &str = "    Elapsed  ";
const SPEND_LABEL: &str = "Goal spend  ";
const VERDICT_LABEL: &str = "Latest verdict  ";

/// What the Status tab reads, gathered by the App each frame it is drawn.
pub(crate) struct StatusContent<'a> {
    pub status: Option<&'a GoalStatus>,
    pub evaluator: Option<&'a Binding>,
    pub continuation_limit: u32,
}

pub(super) fn status_lines(content: &StatusContent<'_>, theme: &Theme) -> Vec<Line<'static>> {
    let Some(status) = content.status else {
        return vec![
            Line::from(Span::styled(NO_GOAL, theme.status_dim)),
            Line::from(Span::styled(SET_HINT, theme.tool_dim)),
            Line::default(),
            evaluator_line(content.evaluator, theme),
            continuation_line(content.continuation_limit, theme),
        ];
    };
    let (condition, verdict, reason, evaluations, duration, usage, cost, active) = match status {
        GoalStatus::Active(goal) => (
            goal.condition.as_ref(),
            goal.last_verdict,
            goal.last_reason.as_deref(),
            goal.evaluations,
            goal.elapsed(),
            goal.usage,
            goal.cost,
            true,
        ),
        GoalStatus::Finished(goal) => (
            goal.condition.as_ref(),
            Some(goal.verdict),
            Some(goal.reason.as_ref()),
            goal.evaluations,
            goal.duration,
            goal.usage,
            goal.cost,
            false,
        ),
    };
    let title = if active {
        ACTIVE_TITLE
    } else {
        match verdict {
            Some(GoalVerdict::Met) => ACHIEVED_TITLE,
            Some(GoalVerdict::Impossible | GoalVerdict::NotMet) | None => NOT_ACHIEVED_TITLE,
        }
    };
    let title_style = if active || verdict == Some(GoalVerdict::Met) {
        theme.status_notice
    } else {
        theme.error
    };
    let mut lines = vec![
        Line::from(Span::styled(title, title_style)),
        Line::default(),
        Line::from(Span::styled(CONDITION_HEADING, theme.panel_title)),
        Line::from(condition.to_owned()),
        Line::default(),
        evaluator_line(content.evaluator, theme),
        continuation_line(content.continuation_limit, theme),
        Line::from(vec![
            Span::styled(EVALUATIONS_LABEL, theme.tool_dim),
            Span::raw(evaluations.to_string()),
            Span::styled(ELAPSED_LABEL, theme.tool_dim),
            Span::raw(format_duration(duration)),
        ]),
        Line::from(vec![
            Span::styled(SPEND_LABEL, theme.tool_dim),
            Span::raw(usage.format(cost)),
        ]),
    ];
    if let Some(verdict) = verdict {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled(VERDICT_LABEL, theme.tool_dim),
            Span::raw(verdict.to_string()),
        ]));
    }
    if let Some(reason) = reason {
        lines.push(Line::from(Span::styled(REASON_HEADING, theme.panel_title)));
        lines.push(Line::from(reason.to_owned()));
    }
    lines
}

fn evaluator_line(binding: Option<&Binding>, theme: &Theme) -> Line<'static> {
    let value = binding.map_or_else(|| UNBOUND_EVALUATOR.to_owned(), Binding::to_string);
    Line::from(vec![
        Span::styled(EVALUATOR_LABEL, theme.tool_dim),
        Span::raw(value),
    ])
}

fn continuation_line(limit: u32, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(CONTINUATIONS_LABEL, theme.tool_dim),
        Span::raw(limit.to_string()),
        Span::styled(ADJUST_HINT, theme.tool_dim),
    ])
}
