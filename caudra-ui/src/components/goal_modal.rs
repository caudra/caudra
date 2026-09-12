use std::time::Duration;

use caudra_agent::{GoalStatus, GoalVerdict, MAX_GOAL_CONTINUATION_LIMIT};
use caudra_providers::model_registry::Binding;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::ModalScroll;
use super::Overlay;
use super::modal::{FooterHits, FooterLine, Modal};
use super::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::theme;

const WIDTH_PERCENT: u16 = 72;
const MAX_HEIGHT_PERCENT: u16 = 70;
const H_PAD: u16 = 2;
const GOAL_CLEAR: &str = "/goal-clear";
const GOAL_MODEL: &str = "/goal-model";
const GOAL_START: &str = "/goal <condition>";
const SEPARATOR: &str = " · ";
const CLOSE_HINT: &str = " · Esc close";
const UNBOUND_EVALUATOR: &str = "default (fast, then chat)";

pub struct GoalModal {
    open: bool,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    popup: Rect,
    footer: FooterHits,
    /// Which footer was last drawn. A click is answered from the same table
    /// that produced the hits, so a cleared goal cannot report `/goal-clear`.
    active: bool,
}

impl Default for GoalModal {
    fn default() -> Self {
        Self {
            open: false,
            scroll: ModalScroll::new_top(),
            scrollbar: Scrollbar::default(),
            popup: Rect::default(),
            footer: FooterHits::default(),
            active: false,
        }
    }
}

impl GoalModal {
    pub fn open(&mut self) {
        self.open = true;
        self.scroll.reset();
        self.footer.clear();
    }

    pub fn handle_key(&mut self, key: KeyEvent, continuation_limit: u32) -> Option<u32> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.close();
                None
            }
            KeyCode::Left | KeyCode::Char('-') => Some(continuation_limit.saturating_sub(1)),
            KeyCode::Right | KeyCode::Char('+') | KeyCode::Char('=') => Some(
                continuation_limit
                    .saturating_add(1)
                    .min(MAX_GOAL_CONTINUATION_LIMIT),
            ),
            _ => {
                self.scroll.handle_key(key);
                None
            }
        }
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    #[cfg(test)]
    pub(crate) fn footer_hit(&self, command: &str) -> Rect {
        footer_commands(self.active)
            .iter()
            .position(|name| *name == command)
            .map(|index| self.footer.hit(index))
            .unwrap_or_default()
    }

    /// The command line a footer click asked for, left to the host to run: the
    /// footer names session commands, not modal state.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> Option<&'static str> {
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return None,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return None;
            }
        }
        self.footer
            .handle_mouse(event)
            .and_then(|index| footer_commands(self.active).get(index).copied())
    }

    pub fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        status: Option<&GoalStatus>,
        evaluator: Option<&Binding>,
        continuation_limit: u32,
    ) -> Rect {
        if !self.open {
            return Rect::default();
        }

        let width = Modal::inner_width(area.width, WIDTH_PERCENT).saturating_sub(H_PAD * 2);
        self.active = is_active(status);
        let mut lines = status_lines(status, evaluator, continuation_limit);
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
        let footer = footer(self.active);
        self.footer.set(footer.hits(padded, offset, total));
        if let Some(index) = self.footer.hovered()
            && let Some(last) = lines.last_mut()
        {
            *last = footer.line(Some(index));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((offset, 0)),
            padded,
        );
        self.scrollbar.draw(frame, inner, total, offset);
        self.popup = popup;
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
        self.footer.reset();
    }
}

fn is_active(status: Option<&GoalStatus>) -> bool {
    matches!(status, Some(GoalStatus::Active(_)))
}

/// The commands the footer offers, indexed the way [`FooterLine`] targets are.
fn footer_commands(active: bool) -> &'static [&'static str] {
    if active {
        &[GOAL_CLEAR, GOAL_MODEL]
    } else {
        &[GOAL_MODEL]
    }
}

/// Kept as short as the `/context` footer on purpose: an 80 column terminal
/// leaves this modal 51 columns, and a footer that wraps is one the pointer
/// cannot be offered at all. The commands name what they do, and the body
/// spells out the condition and evaluator they act on.
fn footer(active: bool) -> FooterLine {
    let theme = theme::current();
    let mut footer = FooterLine::default();
    if active {
        footer.command(GOAL_CLEAR, theme.keybind_key);
    } else {
        footer.text(GOAL_START, theme.tool_dim);
    }
    footer.text(SEPARATOR, theme.tool_dim);
    footer.command(GOAL_MODEL, theme.keybind_key);
    footer.text(CLOSE_HINT, theme.tool_dim);
    footer
}

fn status_lines(
    status: Option<&GoalStatus>,
    evaluator: Option<&Binding>,
    continuation_limit: u32,
) -> Vec<Line<'static>> {
    let theme = theme::current();
    let Some(status) = status else {
        return vec![
            Line::from(Span::styled("No goal set", theme.status_dim)),
            Line::default(),
            evaluator_line(evaluator),
            continuation_line(continuation_limit),
            Line::default(),
            footer(false).line(None),
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
        continuation_line(continuation_limit),
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
    lines.push(footer(active).line(None));
    lines
}

fn evaluator_line(binding: Option<&Binding>) -> Line<'static> {
    let value = binding.map_or_else(|| UNBOUND_EVALUATOR.to_string(), Binding::to_string);
    Line::from(vec![
        Span::styled("Evaluator  ", theme::current().tool_dim),
        Span::raw(value),
    ])
}

fn continuation_line(limit: u32) -> Line<'static> {
    Line::from(vec![
        Span::styled("Automatic continuations  ", theme::current().tool_dim),
        Span::raw(limit.to_string()),
        Span::styled("    ←/→ adjust for this session", theme::current().tool_dim),
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
    use caudra_agent::GoalResult;
    use caudra_providers::TokenUsage;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;

    use super::*;
    use caudra_workbench::scroll::SCROLLBAR_THUMB;

    const WIDTH: u16 = 120;
    const HEIGHT: u16 = 40;
    /// Short enough that the status outgrows the modal and earns a bar.
    const CRAMPED_HEIGHT: u16 = 10;
    const CONDITION: &str = "the suite is green";
    const REASON: &str = "every test passed";
    const HOVER_MISSED: &str = "the footer command must reverse under the pointer";
    const BAR_SCROLL_MISSED: &str = "a press on the bar's column must scroll the body";

    fn finished() -> GoalStatus {
        GoalStatus::Finished(GoalResult {
            condition: CONDITION.into(),
            verdict: GoalVerdict::Met,
            reason: REASON.into(),
            evaluations: 3,
            duration: Duration::from_secs(12),
            usage: TokenUsage::default(),
            cost: None,
            subscription_cost: None,
        })
    }

    fn mouse(kind: MouseEventKind, at: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: at.x,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn drawn(modal: &mut GoalModal, status: Option<&GoalStatus>) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area(), status, None, 16);
            })
            .unwrap();
        terminal
    }

    #[test]
    fn duration_is_compact() {
        assert_eq!(format_duration(Duration::from_secs(8)), "8s");
        assert_eq!(format_duration(Duration::from_secs(68)), "1m 8s");
        assert_eq!(format_duration(Duration::from_secs(3_668)), "1h 1m");
    }

    #[test]
    fn continuation_limit_keys_adjust_within_bounds() {
        let mut modal = GoalModal::default();
        assert_eq!(
            modal.handle_key(KeyEvent::from(KeyCode::Right), 16),
            Some(17)
        );
        assert_eq!(modal.handle_key(KeyEvent::from(KeyCode::Left), 0), Some(0));
        assert_eq!(
            modal.handle_key(
                KeyEvent::from(KeyCode::Char('+')),
                MAX_GOAL_CONTINUATION_LIMIT,
            ),
            Some(MAX_GOAL_CONTINUATION_LIMIT)
        );
    }

    #[test]
    fn the_footer_command_hovers_and_activates() {
        let status = finished();
        let mut modal = GoalModal::default();
        modal.open();
        let _ = drawn(&mut modal, Some(&status));

        let hit = modal.footer_hit(GOAL_MODEL);
        assert!(!hit.is_empty());

        modal.handle_mouse(mouse(MouseEventKind::Moved, hit));
        let terminal = drawn(&mut modal, Some(&status));
        let reversed = (0..HEIGHT)
            .flat_map(|y| (0..WIDTH).map(move |x| Position::new(x, y)))
            .filter(|position| {
                terminal.backend().buffer()[(position.x, position.y)]
                    .modifier
                    .contains(Modifier::REVERSED)
            })
            .collect::<Vec<_>>();
        assert!(
            !reversed.is_empty() && reversed.iter().all(|position| hit.contains(*position)),
            "{HOVER_MISSED}: hit={hit:?} reversed={reversed:?}"
        );

        assert_eq!(
            modal.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hit)),
            None
        );
        assert_eq!(
            modal.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hit)),
            Some(GOAL_MODEL)
        );
    }

    #[test]
    fn a_press_on_the_bar_scrolls_the_body() {
        let status = finished();
        let mut modal = GoalModal::default();
        modal.open();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, CRAMPED_HEIGHT)).unwrap();
        let mut popup = Rect::default();
        terminal
            .draw(|frame| {
                popup = modal.view(
                    frame,
                    frame.area(),
                    Some(&status),
                    None,
                    MAX_GOAL_CONTINUATION_LIMIT,
                );
            })
            .unwrap();

        let thumb = (0..CRAMPED_HEIGHT)
            .flat_map(|y| (0..WIDTH).map(move |x| Position::new(x, y)))
            .find(|at| terminal.backend().buffer()[(at.x, at.y)].symbol() == SCROLLBAR_THUMB)
            .expect(BAR_SCROLL_MISSED);

        modal.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            Rect::new(thumb.x, popup.bottom() - 2, 1, 1),
        ));

        assert!(modal.scroll.offset() > 0, "{BAR_SCROLL_MISSED}");
    }

    #[test]
    fn a_press_off_the_footer_activates_nothing() {
        let status = finished();
        let mut modal = GoalModal::default();
        modal.open();
        let _ = drawn(&mut modal, Some(&status));

        let off = Rect::new(modal.footer_hit(GOAL_MODEL).x, 0, 1, 1);
        modal.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), off));
        assert_eq!(
            modal.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), off)),
            None
        );
    }

    /// A goal that is over cannot be stopped, so the command that would stop it
    /// is not on screen and its index is not reachable.
    #[test]
    fn a_finished_goal_offers_no_clear() {
        assert_eq!(footer_commands(false), [GOAL_MODEL]);
        assert_eq!(footer_commands(true), [GOAL_CLEAR, GOAL_MODEL]);
    }
}
