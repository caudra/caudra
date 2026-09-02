//! The bottom-stack panel listing the model's current todos.
//!
//! State is the latest `ToolOutput::TodoList` for this session, which is the
//! whole truth: `todo_write` has replace-all semantics, so there is nothing to
//! replay and a restored session paints from its last output alone.
//!
//! The panel shares `Ctrl+T` with the plan form. They never compete: the plan
//! form takes the key only when a plan is ready, and it is hidden otherwise.

use caudra_agent::types::{TodoItem, TodoStatus};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::components::keybindings::key;
use crate::theme;

const TITLE: &str = "Todos";
const ELLIPSIS: &str = "…";

/// Beyond this the panel would crowd out the transcript; the list scrolls to
/// keep the in-progress item in view instead.
const MAX_VISIBLE_ROWS: usize = 8;

/// Header plus a trailing blank, matching the queue panel's grid.
const CHROME_ROWS: u16 = 2;

/// Hidden means the user dismissed it; a fresh list re-opens the panel, which
/// is what makes the tool's progress visible without a keystroke.
#[derive(Default)]
pub struct TodoPanel {
    items: Vec<TodoItem>,
    dismissed: bool,
}

impl TodoPanel {
    /// Returns whether anything changed, so the caller can skip a repaint.
    pub fn set_items(&mut self, items: Vec<TodoItem>) -> bool {
        if self.items == items {
            return false;
        }
        // A new list is new information: undo a dismissal so the user sees it.
        self.dismissed = false;
        self.items = items;
        true
    }

    pub fn reset(&mut self) {
        self.items.clear();
        self.dismissed = false;
    }

    pub fn is_visible(&self) -> bool {
        !self.items.is_empty() && !self.dismissed
    }

    /// Ignored when there is nothing to show, so the key stays available to
    /// whatever else wants it.
    pub fn toggle(&mut self) -> bool {
        if self.items.is_empty() {
            return false;
        }
        self.dismissed = !self.dismissed;
        true
    }

    pub fn height(&self) -> u16 {
        if !self.is_visible() {
            return 0;
        }
        self.items.len().min(MAX_VISIBLE_ROWS) as u16 + CHROME_ROWS
    }

    /// The counter shown next to `Ctrl+T` while the panel is dismissed, so the
    /// list stays discoverable without occupying rows.
    pub fn hint_line(&self) -> Option<Line<'static>> {
        if self.items.is_empty() || !self.dismissed {
            return None;
        }
        let t = theme::current();
        Some(Line::from(vec![
            Span::styled(
                format!(" {}/{} ", self.completed(), self.items.len()),
                Style::new().fg(t.foreground),
            ),
            Span::styled(key::PLAN_TOGGLE.label, t.keybind_key),
            Span::raw(" "),
        ]))
    }

    fn completed(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.status == TodoStatus::Completed)
            .count()
    }

    /// Keeps the first unfinished item on screen. Once work is underway that
    /// is the only row the user is actually watching.
    fn viewport(&self) -> usize {
        let focus = self
            .items
            .iter()
            .position(|i| i.status == TodoStatus::InProgress)
            .or_else(|| {
                self.items
                    .iter()
                    .position(|i| i.status == TodoStatus::Pending)
            })
            .unwrap_or(0);
        let overflow = self.items.len().saturating_sub(MAX_VISIBLE_ROWS);
        focus.saturating_sub(MAX_VISIBLE_ROWS / 2).min(overflow)
    }

    pub fn view(&self, frame: &mut Frame, area: Rect) {
        if !self.is_visible() || area.width < 2 || area.height < 2 {
            return;
        }
        let t = theme::current();
        let left = if area.width >= 32 {
            3
        } else if area.width >= 16 {
            2
        } else {
            1
        };
        let right = u16::from(area.width >= 32);
        let content = Rect::new(
            area.x.saturating_add(left),
            area.y.saturating_add(1),
            area.width.saturating_sub(left.saturating_add(right)),
            area.height.saturating_sub(CHROME_ROWS),
        );
        let start = self.viewport();
        let end = (start + usize::from(content.height)).min(self.items.len());

        frame.render_widget(Block::default().style(t.panel_style()), area);
        for y in area.y..area.bottom() {
            if let Some(cell) = frame.buffer_mut().cell_mut((area.x, y)) {
                cell.set_char('┃').set_style(t.panel_border);
            }
        }
        frame.render_widget(
            Paragraph::new(Line::from(format!(
                "{TITLE} · {}/{}",
                self.completed(),
                self.items.len()
            )))
            .style(t.panel_title),
            Rect::new(content.x, area.y, content.width, 1),
        );
        let lines = self.items[start..end]
            .iter()
            .map(|item| row(item, content.width as usize))
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines).style(Style::new().fg(t.foreground)),
            content,
        );
    }
}

fn row(item: &TodoItem, width: usize) -> Line<'static> {
    let t = theme::current();
    let style = match item.status {
        TodoStatus::Completed => t.todo_completed,
        TodoStatus::InProgress => t.todo_in_progress,
        TodoStatus::Pending => t.todo_pending,
        TodoStatus::Cancelled => t.todo_cancelled,
    };
    let marker = item.status.marker();
    let text = format!("{marker} {}", item.content);
    Line::from(Span::styled(truncate(&text, width), style))
}

fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let budget = width.saturating_sub(ELLIPSIS.width());
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.to_string().width();
        if used + w > budget {
            break;
        }
        used += w;
        out.push(ch);
    }
    out.push_str(ELLIPSIS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use test_case::test_case;

    const WIDE: u16 = 40;

    fn todo(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.into(),
            status,
            priority: caudra_agent::types::TodoPriority::Medium,
        }
    }

    fn panel(statuses: &[TodoStatus]) -> TodoPanel {
        let mut panel = TodoPanel::default();
        let items = statuses
            .iter()
            .enumerate()
            .map(|(i, s)| todo(&format!("task {i}"), *s))
            .collect();
        panel.set_items(items);
        panel
    }

    fn render(panel: &TodoPanel, width: u16) -> String {
        let height = panel.height().max(1);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| panel.view(frame, Rect::new(0, 0, width, height)))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test_case(0, 0 ; "empty list takes no rows")]
    #[test_case(1, 3 ; "one item plus chrome")]
    #[test_case(8, 10 ; "at the cap")]
    #[test_case(40, 10 ; "beyond the cap it stops growing")]
    fn height_is_bounded(count: usize, expected: u16) {
        let panel = panel(&vec![TodoStatus::Pending; count]);
        assert_eq!(panel.height(), expected);
    }

    #[test]
    fn a_dismissed_panel_takes_no_rows_but_keeps_its_items() {
        let mut panel = panel(&[TodoStatus::Pending]);
        assert!(panel.toggle());
        assert_eq!(panel.height(), 0);
        assert!(panel.hint_line().is_some(), "the list is still there");
    }

    #[test]
    fn toggling_an_empty_panel_does_nothing() {
        let mut panel = TodoPanel::default();
        assert!(!panel.toggle());
        assert!(!panel.is_visible());
    }

    /// A dismissal must not outlive the list it was aimed at, or the next
    /// task's progress would be invisible.
    #[test]
    fn a_new_list_undoes_a_dismissal() {
        let mut panel = panel(&[TodoStatus::Pending]);
        panel.toggle();
        assert!(!panel.is_visible());
        panel.set_items(vec![todo("fresh", TodoStatus::InProgress)]);
        assert!(panel.is_visible());
    }

    #[test]
    fn an_identical_list_is_not_a_change() {
        let mut panel = panel(&[TodoStatus::Pending]);
        let same = vec![todo("task 0", TodoStatus::Pending)];
        assert!(!panel.set_items(same));
    }

    /// Re-sending the same list must not reopen a panel the user dismissed;
    /// the model repeats its list constantly.
    #[test]
    fn an_identical_list_does_not_undo_a_dismissal() {
        let mut panel = panel(&[TodoStatus::Pending]);
        panel.toggle();
        panel.set_items(vec![todo("task 0", TodoStatus::Pending)]);
        assert!(!panel.is_visible());
    }

    #[test]
    fn the_hint_appears_only_while_dismissed() {
        let mut panel = panel(&[TodoStatus::Completed, TodoStatus::Pending]);
        assert!(panel.hint_line().is_none(), "visible panel needs no hint");
        panel.toggle();
        let hint = panel.hint_line().expect("dismissed panel shows a hint");
        assert!(hint.spans[0].content.contains("1/2"), "{hint:?}");
    }

    #[test]
    fn the_header_counts_completed_items() {
        let panel = panel(&[
            TodoStatus::Completed,
            TodoStatus::Completed,
            TodoStatus::Pending,
        ]);
        assert!(render(&panel, WIDE).contains("Todos · 2/3"));
    }

    #[test]
    fn every_status_renders_its_marker() {
        let panel = panel(&[
            TodoStatus::Completed,
            TodoStatus::InProgress,
            TodoStatus::Pending,
            TodoStatus::Cancelled,
        ]);
        let screen = render(&panel, WIDE);
        for status in [
            TodoStatus::Completed,
            TodoStatus::InProgress,
            TodoStatus::Pending,
            TodoStatus::Cancelled,
        ] {
            assert!(screen.contains(status.marker()), "{status:?}\n{screen}");
        }
    }

    #[test]
    fn a_long_item_is_truncated_to_the_panel_width() {
        let mut panel = TodoPanel::default();
        panel.set_items(vec![todo(&"x".repeat(200), TodoStatus::Pending)]);
        let screen = render(&panel, WIDE);
        assert!(screen.contains(ELLIPSIS), "{screen}");
        assert!(screen.lines().all(|l| l.width() <= WIDE as usize));
    }

    /// The row being worked on is the one the user is watching, so it must
    /// survive a list longer than the panel.
    #[test]
    fn the_viewport_follows_the_in_progress_item() {
        let mut statuses = vec![TodoStatus::Completed; 20];
        statuses[15] = TodoStatus::InProgress;
        let panel = panel(&statuses);
        assert!(render(&panel, WIDE).contains("task 15"));
    }

    /// With nothing left to do there is no row to follow, so the list stays
    /// where it started rather than jumping to an arbitrary end.
    #[test]
    fn a_finished_list_shows_its_head() {
        let panel = panel(&[TodoStatus::Completed; 20]);
        let screen = render(&panel, WIDE);
        assert!(screen.contains("task 0"), "{screen}");
        assert!(!screen.contains("task 19"), "{screen}");
    }

    #[test]
    fn a_reset_panel_forgets_everything() {
        let mut panel = panel(&[TodoStatus::Pending]);
        panel.reset();
        assert_eq!(panel.height(), 0);
        assert!(panel.hint_line().is_none());
    }
}
