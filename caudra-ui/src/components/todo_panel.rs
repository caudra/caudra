//! The bottom-stack panel listing the model's current todos.
//!
//! State is the latest `ToolOutput::TodoList` for this session, which is the
//! whole truth: `todo_write` has replace-all semantics, so there is nothing to
//! replay and a restored session paints from its last output alone.
//!
//! The panel shares `Ctrl+T` with the plan form. They never compete: the plan
//! form takes the key only when a plan is ready, and it is hidden otherwise.

use caudra_agent::types::{TodoItem, TodoStatus};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::components::keybindings::key;
use crate::components::{apply_scroll_delta, hover_style};
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
    scroll: u16,
    /// Whether the viewport is tracking the first unfinished item. The wheel
    /// turns it off, a new list turns it back on: a reader who scrolled back
    /// to an earlier todo must not be yanked forward on the next repaint.
    follow_focus: bool,
    /// Where the panel and its header last drew. A wheel event needs the
    /// panel, a dismissing click needs the header.
    area: Rect,
    header: Rect,
    header_down: bool,
    header_hover: bool,
}

impl TodoPanel {
    /// Returns whether anything changed, so the caller can skip a repaint.
    pub fn set_items(&mut self, items: Vec<TodoItem>) -> bool {
        if self.items == items {
            return false;
        }
        // A new list is new information: undo a dismissal so the user sees it.
        self.dismissed = false;
        self.follow_focus = true;
        self.items = items;
        true
    }

    pub fn reset(&mut self) {
        self.items.clear();
        self.dismissed = false;
        self.scroll = 0;
        self.follow_focus = true;
        self.area = Rect::default();
        self.header = Rect::default();
        self.header_down = false;
        self.header_hover = false;
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

    /// The rows past `MAX_VISIBLE_ROWS` that only scrolling can reach.
    fn overflow(&self) -> usize {
        self.items.len().saturating_sub(MAX_VISIBLE_ROWS)
    }

    /// Keeps the first unfinished item on screen. Once work is underway that
    /// is the only row the user is actually watching, until they say otherwise
    /// by scrolling.
    fn viewport(&self) -> usize {
        if !self.follow_focus {
            return usize::from(self.scroll).min(self.overflow());
        }
        self.items
            .iter()
            .position(|i| i.status == TodoStatus::InProgress)
            .or_else(|| {
                self.items
                    .iter()
                    .position(|i| i.status == TodoStatus::Pending)
            })
            .unwrap_or(0)
            .saturating_sub(MAX_VISIBLE_ROWS / 2)
            .min(self.overflow())
    }

    /// True while the pointer is over the panel, so the wheel scrolls the list
    /// rather than the transcript behind it.
    pub fn contains(&self, pos: Position) -> bool {
        self.is_visible() && self.area.contains(pos)
    }

    pub fn clear_hover(&mut self) {
        self.header_hover = false;
    }

    pub fn scroll(&mut self, delta: i32) {
        // Taking over from the focus-following viewport has to start where the
        // reader can see, or the first notch jumps somewhere else entirely.
        if self.follow_focus {
            self.scroll = self.viewport() as u16;
            self.follow_focus = false;
        }
        self.scroll = apply_scroll_delta(self.scroll, delta).min(self.overflow() as u16);
    }

    /// The header doubles as the dismiss control, so the panel can be put away
    /// without knowing about `Ctrl+T`. Rows are not targets: todos belong to
    /// the model, and there is no path back for a change made here.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> bool {
        if !self.is_visible() {
            return false;
        }
        let hit = self.header.contains(Position::new(event.column, event.row));
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.header_down = hit;
                hit
            }
            // A drag that began on the header is the reader selecting its text.
            MouseEventKind::Drag(MouseButton::Left) => {
                self.header_down = false;
                false
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let pressed = std::mem::take(&mut self.header_down);
                if pressed && hit {
                    self.dismissed = true;
                }
                pressed && hit
            }
            // Read on the next frame. The move itself is not the panel's to
            // consume: the transcript behind it still tracks its own hover.
            MouseEventKind::Moved => {
                self.header_hover = hit;
                false
            }
            _ => false,
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) {
        if !self.is_visible() || area.width < 2 || area.height < 2 {
            self.area = Rect::default();
            self.header = Rect::default();
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
        let title = format!("{TITLE} · {}/{}", self.completed(), self.items.len());
        // The control is the title, not the blank cells after it: a click
        // target the reader cannot see is not one they can aim at.
        let header = Rect::new(
            content.x,
            area.y,
            (title.width() as u16).min(content.width),
            1,
        );
        self.area = area;
        self.header = header;

        frame.render_widget(Block::default().style(t.panel_style()), area);
        for y in area.y..area.bottom() {
            if let Some(cell) = frame.buffer_mut().cell_mut((area.x, y)) {
                cell.set_char('┃').set_style(t.panel_border);
            }
        }
        frame.render_widget(
            Paragraph::new(Line::from(title)).style(hover_style(t.panel_title, self.header_hover)),
            header,
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
    use ratatui::style::Modifier;
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

    fn render(panel: &mut TodoPanel, width: u16) -> String {
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
        let mut panel = panel(&[
            TodoStatus::Completed,
            TodoStatus::Completed,
            TodoStatus::Pending,
        ]);
        assert!(render(&mut panel, WIDE).contains("Todos · 2/3"));
    }

    #[test]
    fn every_status_renders_its_marker() {
        let mut panel = panel(&[
            TodoStatus::Completed,
            TodoStatus::InProgress,
            TodoStatus::Pending,
            TodoStatus::Cancelled,
        ]);
        let screen = render(&mut panel, WIDE);
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
        let screen = render(&mut panel, WIDE);
        assert!(screen.contains(ELLIPSIS), "{screen}");
        assert!(screen.lines().all(|l| l.width() <= WIDE as usize));
    }

    /// The row being worked on is the one the user is watching, so it must
    /// survive a list longer than the panel.
    #[test]
    fn the_viewport_follows_the_in_progress_item() {
        let mut statuses = vec![TodoStatus::Completed; 20];
        statuses[15] = TodoStatus::InProgress;
        let mut panel = panel(&statuses);
        assert!(render(&mut panel, WIDE).contains("task 15"));
    }

    /// With nothing left to do there is no row to follow, so the list stays
    /// where it started rather than jumping to an arbitrary end.
    #[test]
    fn a_finished_list_shows_its_head() {
        let mut panel = panel(&[TodoStatus::Completed; 20]);
        let screen = render(&mut panel, WIDE);
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

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    fn click(panel: &mut TodoPanel, column: u16, row: u16) -> bool {
        panel.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), column, row));
        panel.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), column, row))
    }

    /// Rows past the cap were unreachable by any input before the wheel
    /// landed here: the viewport was derived and had nothing to override.
    #[test]
    fn the_wheel_reaches_rows_past_the_visible_cap() {
        let mut panel = panel(&[TodoStatus::Completed; 20]);
        let _ = render(&mut panel, WIDE);
        panel.scroll(-(MAX_VISIBLE_ROWS as i32));
        let screen = render(&mut panel, WIDE);
        assert!(screen.contains("task 8"), "{screen}");
        assert!(!screen.contains("task 0"), "{screen}");
    }

    /// Positive delta is `ScrollUp` everywhere else in the app, so it has to
    /// mean the same here.
    #[test]
    fn the_wheel_scrolls_up_on_a_positive_delta() {
        let mut panel = panel(&[TodoStatus::Completed; 20]);
        let _ = render(&mut panel, WIDE);
        panel.scroll(-4);
        panel.scroll(4);
        let screen = render(&mut panel, WIDE);
        assert!(screen.contains("task 0"), "{screen}");
    }

    /// A scrolled reader is looking at an earlier todo on purpose; the
    /// focus-following viewport must not drag them forward again.
    #[test]
    fn scrolling_stops_the_viewport_chasing_the_active_row() {
        let mut statuses = vec![TodoStatus::Completed; 20];
        statuses[15] = TodoStatus::InProgress;
        let mut panel = panel(&statuses);
        assert!(render(&mut panel, WIDE).contains("task 15"));
        panel.scroll(i32::from(u16::MAX));
        let screen = render(&mut panel, WIDE);
        assert!(screen.contains("task 0"), "{screen}");
    }

    /// A new list is new information, which is also why it undoes a dismissal.
    #[test]
    fn a_new_list_puts_the_viewport_back_on_the_active_row() {
        let mut statuses = vec![TodoStatus::Completed; 20];
        statuses[15] = TodoStatus::InProgress;
        let mut panel = panel(&statuses);
        let _ = render(&mut panel, WIDE);
        panel.scroll(i32::from(u16::MAX));
        statuses[16] = TodoStatus::InProgress;
        assert!(
            panel.set_items(
                statuses
                    .iter()
                    .enumerate()
                    .map(|(i, s)| todo(&format!("task {i}"), *s))
                    .collect(),
            )
        );
        assert!(render(&mut panel, WIDE).contains("task 15"));
    }

    #[test]
    fn clicking_the_header_dismisses_the_panel() {
        let mut panel = panel(&[TodoStatus::Pending]);
        let _ = render(&mut panel, WIDE);
        let header = panel.header;
        assert!(click(&mut panel, header.x, header.y));
        assert!(!panel.is_visible());
        assert!(panel.hint_line().is_some(), "the list is still reachable");
    }

    /// The press and release must land on the same control, or a drag that
    /// happens to end on the header would put the panel away.
    #[test]
    fn a_press_elsewhere_does_not_dismiss_on_release_over_the_header() {
        let mut panel = panel(&[TodoStatus::Pending; 3]);
        let _ = render(&mut panel, WIDE);
        let header = panel.header;
        panel.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            header.x,
            header.y + 1,
        ));
        assert!(!panel.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            header.x,
            header.y
        )));
        assert!(panel.is_visible());
    }

    /// Dragging out of the header is the reader selecting its text.
    #[test]
    fn dragging_off_the_header_does_not_dismiss() {
        let mut panel = panel(&[TodoStatus::Pending; 3]);
        let _ = render(&mut panel, WIDE);
        let header = panel.header;
        panel.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            header.x,
            header.y,
        ));
        panel.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            header.right(),
            header.y,
        ));
        assert!(!panel.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            header.x,
            header.y
        )));
        assert!(panel.is_visible());
    }

    /// Todos belong to the model; a click on one has nowhere to go.
    #[test]
    fn clicking_a_row_does_nothing() {
        let mut panel = panel(&[TodoStatus::Pending; 3]);
        let _ = render(&mut panel, WIDE);
        let header = panel.header;
        assert!(!click(&mut panel, header.x, header.y + 1));
        assert!(panel.is_visible());
    }

    /// A dismissed panel draws nothing, so it must not keep claiming the rows
    /// it used to occupy.
    #[test]
    fn a_dismissed_panel_owns_no_screen_area() {
        let mut panel = panel(&[TodoStatus::Pending]);
        let _ = render(&mut panel, WIDE);
        let inside = Position::new(panel.header.x, panel.header.y);
        assert!(panel.contains(inside));
        assert!(panel.toggle());
        let _ = render(&mut panel, WIDE);
        assert!(!panel.contains(inside));
    }
    const EXPECT_MARKED: &str = "the control under the pointer has to be marked";
    const EXPECT_UNMARKED: &str = "nothing else may be marked";

    fn reversed_cells(panel: &mut TodoPanel, width: u16) -> Vec<Position> {
        let height = panel.height().max(1);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| panel.view(frame, Rect::new(0, 0, width, height)))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .flat_map(|y| (0..width).map(move |x| Position::new(x, y)))
            .filter(|pos| {
                buffer
                    .cell((pos.x, pos.y))
                    .unwrap()
                    .modifier
                    .contains(Modifier::REVERSED)
            })
            .collect()
    }

    fn move_to(panel: &mut TodoPanel, pos: Position) -> bool {
        panel.handle_mouse(mouse_at(MouseEventKind::Moved, pos))
    }

    fn mouse_at(kind: MouseEventKind, pos: Position) -> MouseEvent {
        MouseEvent {
            kind,
            column: pos.x,
            row: pos.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    #[test]
    fn hovering_the_header_marks_it() {
        let mut panel = panel(&[TodoStatus::Pending, TodoStatus::Completed]);
        let _ = render(&mut panel, WIDE);
        assert!(
            reversed_cells(&mut panel, WIDE).is_empty(),
            "{EXPECT_UNMARKED}"
        );

        let header = panel.header;
        // A move is never the panel's to consume: the transcript behind it
        // still tracks its own hover.
        assert!(!move_to(&mut panel, Position::new(header.x, header.y)));
        let marked = reversed_cells(&mut panel, WIDE);
        assert!(!marked.is_empty(), "{EXPECT_MARKED}");
        assert!(
            marked.iter().all(|pos| header.contains(*pos)),
            "{EXPECT_UNMARKED}: {marked:?} outside {header:?}"
        );
    }

    /// The dismiss control is the title, not the blank cells beside it: a
    /// target the reader cannot see is not one they can aim at.
    #[test]
    fn the_header_control_stops_at_its_title() {
        let mut panel = panel(&[TodoStatus::Pending]);
        let _ = render(&mut panel, WIDE);
        assert!(panel.header.width < WIDE);

        let beside = Position::new(panel.header.right(), panel.header.y);
        assert!(!move_to(&mut panel, beside));
        assert!(
            reversed_cells(&mut panel, WIDE).is_empty(),
            "{EXPECT_UNMARKED}"
        );
        assert!(
            !click_at(&mut panel, beside),
            "a blank cell is not the control"
        );
        assert!(panel.is_visible());
    }

    #[test]
    fn moving_off_the_header_unmarks_it() {
        let mut panel = panel(&[TodoStatus::Pending, TodoStatus::Completed]);
        let _ = render(&mut panel, WIDE);
        let header = panel.header;
        move_to(&mut panel, Position::new(header.x, header.y));
        assert!(
            !reversed_cells(&mut panel, WIDE).is_empty(),
            "{EXPECT_MARKED}"
        );

        move_to(&mut panel, Position::new(header.x, header.bottom()));
        assert!(
            reversed_cells(&mut panel, WIDE).is_empty(),
            "{EXPECT_UNMARKED}"
        );
    }

    fn click_at(panel: &mut TodoPanel, pos: Position) -> bool {
        panel.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), pos));
        panel.handle_mouse(mouse_at(MouseEventKind::Up(MouseButton::Left), pos))
    }
}
