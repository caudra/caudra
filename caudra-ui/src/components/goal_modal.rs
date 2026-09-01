use std::time::Duration;

use caudra_agent::{GoalStatus, GoalVerdict};
use caudra_providers::model_registry::GoalEvaluatorTarget;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::ModalScroll;
use super::Overlay;
use super::modal::Modal;
use super::scrollbar::render_vertical_scrollbar;
use crate::theme;

const WIDTH_PERCENT: u16 = 72;
const MAX_HEIGHT_PERCENT: u16 = 70;
const H_PAD: u16 = 2;

pub struct GoalModal {
    open: bool,
    scroll: ModalScroll,
}

impl Default for GoalModal {
    fn default() -> Self {
        Self {
            open: false,
            scroll: ModalScroll::new_top(),
        }
    }
}

impl GoalModal {
    pub fn open(&mut self) {
        self.open = true;
        self.scroll.reset();
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.close(),
            _ => {
                self.scroll.handle_key(key);
            }
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    pub fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        status: Option<&GoalStatus>,
        evaluator: &GoalEvaluatorTarget,
    ) -> Rect {
        if !self.open {
            return Rect::default();
        }

        let width = (area.width as u32 * WIDTH_PERCENT as u32 / 100)
            .saturating_sub((2 + H_PAD * 2) as u32) as u16;
        let lines = status_lines(status, evaluator);
        let total = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(width) as u16;
        let modal = Modal {
            title: "Goal",
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, total);
        let padded = Rect {
            x: inner.x + H_PAD,
            width: inner.width.saturating_sub(H_PAD * 2),
            ..inner
        };
        self.scroll.update_dimensions(total, padded.height);
        let offset = self.scroll.offset();
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((offset, 0)),
            padded,
        );
        if total > padded.height {
            render_vertical_scrollbar(frame, inner, total, offset);
        }
        popup
    }
}

impl Overlay for GoalModal {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        self.open = false;
        self.scroll.reset();
    }
}

fn status_lines(
    status: Option<&GoalStatus>,
    evaluator: &GoalEvaluatorTarget,
) -> Vec<Line<'static>> {
    let theme = theme::current();
    let Some(status) = status else {
        return vec![
            Line::from(Span::styled("No goal set", theme.status_dim)),
            Line::default(),
            evaluator_line(evaluator),
            Line::default(),
            Line::from(Span::styled(
                "Start with /goal <condition>  ·  Change evaluator with /goal-model",
                theme.tool_dim,
            )),
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
        "ACTIVE"
    } else {
        match verdict {
            Some(GoalVerdict::Met) => "ACHIEVED",
            Some(GoalVerdict::Impossible | GoalVerdict::NotMet) | None => "NOT ACHIEVED",
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
        Line::from(Span::styled("Condition", theme.panel_title)),
        Line::from(condition.to_owned()),
        Line::default(),
        evaluator_line(evaluator),
        Line::from(vec![
            Span::styled("Evaluations  ", theme.tool_dim),
            Span::raw(evaluations.to_string()),
            Span::styled("    Elapsed  ", theme.tool_dim),
            Span::raw(format_duration(duration)),
        ]),
        Line::from(vec![
            Span::styled("Goal spend  ", theme.tool_dim),
            Span::raw(usage.format(cost)),
        ]),
    ];
    if let Some(verdict) = verdict {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled("Latest verdict  ", theme.tool_dim),
            Span::raw(verdict.to_string()),
        ]));
    }
    if let Some(reason) = reason {
        lines.push(Line::from(Span::styled("Reason", theme.panel_title)));
        lines.push(Line::from(reason.to_owned()));
    }
    lines.push(Line::default());
    lines.push(
        Line::from(Span::styled(
             if active {
                "/goal-clear to stop  ·  /goal-model to change evaluator  ·  Esc to close"
            } else {
                "/goal <condition> to start another  ·  /goal-model to change evaluator  ·  Esc to close"
            },
            theme.tool_dim,
        ))
        .alignment(Alignment::Center),
    );
    lines
}

fn evaluator_line(target: &GoalEvaluatorTarget) -> Line<'static> {
    let value = match target {
        GoalEvaluatorTarget::Auto => "auto (weak, then current model)".into(),
        GoalEvaluatorTarget::Tier(tier) => format!("{tier} (active provider)"),
        GoalEvaluatorTarget::Model(spec) => spec.clone(),
    };
    Line::from(vec![
        Span::styled("Evaluator  ", theme::current().tool_dim),
        Span::raw(value),
    ])
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
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
    use super::*;

    #[test]
    fn duration_is_compact() {
        assert_eq!(format_duration(Duration::from_secs(8)), "8s");
        assert_eq!(format_duration(Duration::from_secs(68)), "1m 8s");
        assert_eq!(format_duration(Duration::from_secs(3_668)), "1h 1m");
    }
}
