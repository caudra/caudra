//! The Status tab: the engagement's state, goal, budget and model, and what
//! autopilot left to the user and is still waiting on.

use caudra_config::AutopilotConfig;
use caudra_providers::model_registry::Binding;
use caudra_storage::autopilot::StoredAutopilot;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::text::{GOAL_HEADING, MINUTES, MS_PER_MINUTE, NO_GOAL, OWNED_GOAL_HEADING, state_label};
use crate::theme::Theme;

pub(crate) const NOT_ENGAGED: &str = "Not engaged";
const BUDGET_HEADING: &str = "Budget";
const CONTINUATIONS_LABEL: &str = "Continuations  ";
const API_COST_LABEL: &str = "API cost       ";
const SUBSCRIPTION_LABEL: &str = "Subscription   ";
const TIME_LABEL: &str = "Time           ";
const MODEL_LABEL: &str = "Model          ";
const NO_LIMIT: &str = "no limit";
const NOT_CAPPED: &str = "covered, not capped";
const UNBOUND_MODEL: &str = "unbound";
const WAITING_HEADING: &str = "Waiting on you";
const ITEM_BULLET: &str = "- ";

/// What the panel reads, gathered by the App each frame it is drawn.
pub(crate) struct PanelContent<'a> {
    /// The engagement running or last run, `None` before the first.
    pub stored: Option<&'a StoredAutopilot>,
    pub limits: &'a AutopilotConfig,
    pub goal: Option<&'a str>,
    pub model: Option<&'a Binding>,
    pub deciding: bool,
    /// What autopilot left to the user and is still waiting, one line each.
    pub escalations: &'a [String],
}

pub(super) fn status_lines(content: &PanelContent<'_>, theme: &Theme) -> Vec<Line<'static>> {
    let escalated = !content.escalations.is_empty();
    let owns_goal = content.stored.is_some_and(|stored| stored.owns_goal);
    let mut lines = vec![
        match content.stored {
            Some(stored) => {
                let style = if !stored.engaged {
                    theme.status_dim
                } else if escalated {
                    theme.todo_in_progress
                } else {
                    theme.status_notice
                };
                Line::from(Span::styled(
                    state_label(stored, content.deciding, escalated),
                    style,
                ))
            }
            None => Line::from(Span::styled(NOT_ENGAGED, theme.status_dim)),
        },
        Line::default(),
        Line::from(Span::styled(
            if owns_goal {
                OWNED_GOAL_HEADING
            } else {
                GOAL_HEADING
            },
            theme.panel_title,
        )),
        match content.goal {
            Some(goal) => Line::from(goal.to_owned()),
            None => Line::from(Span::styled(NO_GOAL, theme.status_dim)),
        },
        Line::default(),
    ];
    if let Some(stored) = content.stored {
        lines.push(Line::from(Span::styled(BUDGET_HEADING, theme.panel_title)));
        lines.extend(budget_lines(stored, content.limits, theme));
    }
    lines.push(labelled(
        MODEL_LABEL,
        content
            .model
            .map_or_else(|| UNBOUND_MODEL.to_owned(), Binding::to_string),
        theme,
    ));
    if escalated {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(WAITING_HEADING, theme.panel_title)));
        lines.extend(
            content
                .escalations
                .iter()
                .map(|line| item(line.clone(), theme.todo_in_progress, theme)),
        );
    }
    lines
}

fn budget_lines(
    stored: &StoredAutopilot,
    limits: &AutopilotConfig,
    theme: &Theme,
) -> [Line<'static>; 4] {
    let minutes = stored.elapsed_ms / MS_PER_MINUTE;
    [
        labelled(
            CONTINUATIONS_LABEL,
            match limits.max_continuations {
                Some(max) => format!("{}/{max}", stored.continuations),
                None => format!("{} ({NO_LIMIT})", stored.continuations),
            },
            theme,
        ),
        labelled(
            API_COST_LABEL,
            match limits.max_cost {
                Some(max) => format!("${:.2}/${max:.2}", stored.cost_usd),
                None => format!("${:.2} ({NO_LIMIT})", stored.cost_usd),
            },
            theme,
        ),
        labelled(
            SUBSCRIPTION_LABEL,
            format!("${:.2} ({NOT_CAPPED})", stored.subscription_cost_usd),
            theme,
        ),
        labelled(
            TIME_LABEL,
            match limits.max_minutes {
                Some(max) => format!("{minutes}/{max} {MINUTES}"),
                None => format!("{minutes} {MINUTES} ({NO_LIMIT})"),
            },
            theme,
        ),
    ]
}

fn labelled(label: &'static str, value: String, theme: &Theme) -> Line<'static> {
    Line::from(vec![Span::styled(label, theme.tool_dim), Span::raw(value)])
}

fn item(text: String, style: Style, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(ITEM_BULLET, theme.tool_dim),
        Span::styled(text, style),
    ])
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::super::text::fixtures::{CONTINUATIONS, COST, ELAPSED_MS, stored};
    use super::super::text::{ENGAGED_STATE, WAITING_STATE};
    use super::*;
    use crate::theme;

    const GOAL: &str = "the suite is green";
    const ESCALATION: &str = "permission: the user must choose";
    const BOUND_MODEL: &str = "anthropic/claude-haiku-4-5";
    const MAX_CONTINUATIONS: u32 = 100;
    const MAX_COST: f64 = 2.0;
    const MAX_MINUTES: u32 = 30;
    const MISSING: &str = "the Status tab must show every part of the engagement";
    const NO_BUDGET: &str = "a session never engaged has no budget to show";

    fn limits(max_continuations: Option<u32>) -> AutopilotConfig {
        AutopilotConfig {
            max_continuations,
            max_cost: Some(MAX_COST),
            max_minutes: Some(MAX_MINUTES),
            ..AutopilotConfig::default()
        }
    }

    fn text(stored: Option<&StoredAutopilot>, limits: &AutopilotConfig) -> String {
        let model = Binding::Exact(BOUND_MODEL.into());
        let escalations = [ESCALATION.to_owned()];
        let content = PanelContent {
            stored,
            limits,
            goal: Some(GOAL),
            model: Some(&model),
            deciding: false,
            escalations: &escalations,
        };
        status_lines(&content, &theme::current())
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test_case(Some(MAX_CONTINUATIONS), format!("{CONTINUATIONS}/{MAX_CONTINUATIONS}") ; "capped")]
    #[test_case(None, format!("{CONTINUATIONS} ({NO_LIMIT})") ; "uncapped")]
    fn the_status_shows_state_goal_budget_model_and_escalations(
        max_continuations: Option<u32>,
        continuations: String,
    ) {
        let stored = stored(true, None);

        let shown = text(Some(&stored), &limits(max_continuations));

        for part in [
            WAITING_STATE.to_owned(),
            OWNED_GOAL_HEADING.to_owned(),
            GOAL.to_owned(),
            continuations,
            format!("${COST:.2}/${MAX_COST:.2}"),
            format!("{}/{MAX_MINUTES} {MINUTES}", ELAPSED_MS / MS_PER_MINUTE),
            BOUND_MODEL.to_owned(),
            ESCALATION.to_owned(),
        ] {
            assert!(shown.contains(&part), "{MISSING}: {part}\n{shown}");
        }
    }

    #[test]
    fn a_session_never_engaged_reads_not_engaged() {
        let shown = text(None, &limits(None));

        assert!(shown.starts_with(NOT_ENGAGED), "{shown}");
        assert!(!shown.contains(ENGAGED_STATE), "{shown}");
        assert!(!shown.contains(BUDGET_HEADING), "{NO_BUDGET}\n{shown}");
        assert!(shown.contains(BOUND_MODEL), "{MISSING}\n{shown}");
    }
}
