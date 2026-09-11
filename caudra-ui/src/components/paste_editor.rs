use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthChar;

use super::Overlay;
use super::modal::Modal;
use super::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::input_document::{PasteId, paste_summary_label};
use crate::text_buffer::TextBuffer;
use crate::theme;

const MODAL_TITLE: &str = " Edit pasted text ";
const MODAL_WIDTH_PERCENT: u16 = 80;
const MODAL_MAX_HEIGHT_PERCENT: u16 = 80;
const META_ROWS: u16 = 1;
const HINT_ROWS: u16 = 1;
const MIN_EDITOR_ROWS: u16 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PasteEditorTarget {
    Main,
    Subagent(String),
}

pub(crate) enum PasteEditorAction {
    Consumed,
    Save {
        target: PasteEditorTarget,
        id: PasteId,
        text: String,
    },
    Cancel,
}

pub(crate) struct PasteEditor {
    target: Option<PasteEditorTarget>,
    id: Option<PasteId>,
    buffer: TextBuffer,
    scroll_y: u16,
    follow_cursor: bool,
    editor_area: Rect,
    scrollbar: Scrollbar,
}

impl PasteEditor {
    pub fn new() -> Self {
        Self {
            target: None,
            id: None,
            buffer: TextBuffer::new(String::new()),
            scroll_y: 0,
            follow_cursor: true,
            editor_area: Rect::default(),
            scrollbar: Scrollbar::default(),
        }
    }

    pub fn open(&mut self, target: PasteEditorTarget, id: PasteId, text: String) {
        self.target = Some(target);
        self.id = Some(id);
        self.buffer = TextBuffer::new(text);
        self.scroll_y = 0;
        self.follow_cursor = true;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PasteEditorAction {
        let (Some(target), Some(id)) = (self.target.clone(), self.id) else {
            return PasteEditorAction::Consumed;
        };
        if key.code == KeyCode::Esc {
            return PasteEditorAction::Cancel;
        }
        if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
            let text = self.buffer.value();
            return if text.trim().is_empty() {
                PasteEditorAction::Consumed
            } else {
                PasteEditorAction::Save { target, id, text }
            };
        }
        if key.code == KeyCode::Enter {
            self.buffer.add_line();
        } else {
            self.buffer.handle_key(key);
        }
        self.follow_cursor = true;
        PasteEditorAction::Consumed
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        if self.id.is_none() {
            return false;
        }
        self.buffer.insert_text(text);
        self.follow_cursor = true;
        true
    }

    pub fn handle_click(&mut self, row: u16, col: u16) {
        let Some(content_y) = row.checked_sub(self.editor_area.y) else {
            return;
        };
        let Some(content_x) = col.checked_sub(self.editor_area.x) else {
            return;
        };
        if content_y >= self.editor_area.height || content_x >= self.editor_area.width {
            return;
        }

        let width = self.editor_area.width.max(1) as usize;
        let target_row = content_y as usize + self.scroll_y as usize;
        let Some(row) = visual_rows(&self.buffer, width).get(target_row).copied() else {
            return;
        };
        let line = &self.buffer.lines()[row.y];
        let widths: Vec<usize> = line.chars().map(|c| c.width().unwrap_or(1)).collect();
        let target_col = content_x as usize;
        let mut display_col = 0;
        let mut x = row.start;
        for character_width in &widths[row.start..row.end] {
            if display_col + character_width > target_col {
                break;
            }
            display_col += character_width;
            x += 1;
        }
        self.buffer.set_cursor(row.y, x);
        self.follow_cursor = true;
    }

    /// The editor reads the pointer only for the caret, which the host routes
    /// through `handle_click`, so the bar is all this takes.
    pub fn handle_mouse(&mut self, event: &MouseEvent) -> bool {
        match self.scrollbar.handle(event) {
            ScrollbarMouse::Ignored => false,
            ScrollbarMouse::Consumed => true,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll_y = top as u16;
                true
            }
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        if delta > 0 {
            self.scroll_y = self.scroll_y.saturating_sub(delta as u16);
        } else {
            self.scroll_y = self.scroll_y.saturating_add(delta.unsigned_abs() as u16);
        }
        self.follow_cursor = false;
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if self.id.is_none() {
            return Rect::default();
        }

        let modal = Modal {
            title: MODAL_TITLE,
            width_percent: MODAL_WIDTH_PERCENT,
            max_height_percent: MODAL_MAX_HEIGHT_PERCENT,
        };
        let desired = area
            .height
            .saturating_mul(MODAL_MAX_HEIGHT_PERCENT)
            .div_ceil(100)
            .saturating_sub(2)
            .max(MIN_EDITOR_ROWS + META_ROWS + HINT_ROWS);
        let (popup, inner) = modal.render(frame, area, desired);
        let [meta_area, editor_area, hint_area] = Layout::vertical([
            Constraint::Length(META_ROWS),
            Constraint::Min(MIN_EDITOR_ROWS),
            Constraint::Length(HINT_ROWS),
        ])
        .areas(inner);
        self.editor_area = editor_area;

        self.render_meta(frame, meta_area);
        self.render_editor(frame, editor_area);
        self.render_hint(frame, hint_area);
        popup
    }

    fn render_meta(&self, frame: &mut Frame, area: Rect) {
        let text = self.buffer.value();
        let line = Line::from(vec![
            Span::styled(
                format!(" {} ", paste_summary_label(&text)),
                theme::current().active,
            ),
            Span::styled(
                format!("{} chars", text.chars().count()),
                theme::current().item_desc,
            ),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_editor(&mut self, frame: &mut Frame, area: Rect) {
        let width = area.width.max(1) as usize;
        let cursor_y = self.buffer.y();
        let cursor_x = self.buffer.x();
        let rows = visual_rows(&self.buffer, width);
        let cursor_visual_y = rows
            .iter()
            .rposition(|row| row.contains_cursor(&self.buffer, cursor_y, cursor_x))
            .unwrap_or(0);
        let viewport = area.height.max(1);
        if self.follow_cursor {
            if cursor_visual_y < self.scroll_y as usize {
                self.scroll_y = cursor_visual_y.min(u16::MAX as usize) as u16;
            } else if cursor_visual_y >= self.scroll_y as usize + viewport as usize {
                self.scroll_y = cursor_visual_y
                    .saturating_sub(viewport as usize - 1)
                    .min(u16::MAX as usize) as u16;
            }
        }

        let lines = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let line = &self.buffer.lines()[row.y];
                let text: String = line
                    .chars()
                    .skip(row.start)
                    .take(row.end - row.start)
                    .collect();
                if index != cursor_visual_y {
                    return Line::raw(text);
                }
                cursor_line(&text, cursor_x - row.start)
            })
            .collect::<Vec<_>>();
        let total = rows.len().min(u16::MAX as usize) as u16;
        let max_scroll = total.saturating_sub(area.height);
        self.scroll_y = self.scroll_y.min(max_scroll);

        frame.render_widget(
            Paragraph::new(lines)
                .scroll((self.scroll_y, 0))
                .style(Style::new().fg(theme::current().foreground)),
            area,
        );
        self.scrollbar.draw(frame, area, total, self.scroll_y);
    }

    fn render_hint(&self, frame: &mut Frame, area: Rect) {
        let line = Line::from(vec![
            Span::styled(" Ctrl+S", theme::current().keybind_key),
            Span::styled(" save  ", theme::current().keybind_desc),
            Span::styled("Esc", theme::current().keybind_key),
            Span::styled(" cancel", theme::current().keybind_desc),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }
}

impl Overlay for PasteEditor {
    fn is_open(&self) -> bool {
        self.id.is_some()
    }

    fn close(&mut self) {
        self.target = None;
        self.id = None;
        self.buffer.clear();
        self.scroll_y = 0;
        self.follow_cursor = true;
        self.editor_area = Rect::default();
    }
}

fn cursor_line(line: &str, cursor_x: usize) -> Line<'static> {
    let byte = TextBuffer::char_to_byte(line, cursor_x);
    let (before, after) = line.split_at(byte);
    let mut chars = after.chars();
    let cursor = chars.next().unwrap_or(' ');
    Line::from(vec![
        Span::raw(before.to_string()),
        Span::styled(cursor.to_string(), theme::current().cursor),
        Span::raw(chars.collect::<String>()),
    ])
}

fn wrap_ranges(widths: &[usize], width: usize) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut column = 0;
    for (index, character_width) in widths.iter().copied().enumerate() {
        if column + character_width > width && column > 0 {
            ranges.push((start, index));
            start = index;
            column = 0;
        }
        column += character_width;
    }
    ranges.push((start, widths.len()));
    ranges
}

#[derive(Clone, Copy)]
struct VisualRow {
    y: usize,
    start: usize,
    end: usize,
}

impl VisualRow {
    fn contains_cursor(&self, buffer: &TextBuffer, cursor_y: usize, cursor_x: usize) -> bool {
        if self.y != cursor_y || cursor_x < self.start {
            return false;
        }
        cursor_x < self.end
            || (self.end == buffer.lines()[self.y].chars().count() && cursor_x == self.end)
    }
}

fn visual_rows(buffer: &TextBuffer, width: usize) -> Vec<VisualRow> {
    let mut rows = Vec::new();
    for (y, line) in buffer.lines().iter().enumerate() {
        let widths: Vec<usize> = line.chars().map(|c| c.width().unwrap_or(1)).collect();
        let ranges = wrap_ranges(&widths, width);
        rows.extend(
            ranges
                .iter()
                .map(|&(start, end)| VisualRow { y, start, end }),
        );
        if y == buffer.y() && buffer.x() == widths.len() {
            let (start, end) = ranges.last().copied().unwrap_or_default();
            let row_width: usize = widths[start..end].iter().sum();
            if row_width + 1 > width {
                rows.push(VisualRow {
                    y,
                    start: widths.len(),
                    end: widths.len(),
                });
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{PasteEditor, PasteEditorAction, PasteEditorTarget, visual_rows};
    use crate::input_document::InputDocument;

    fn paste_id() -> crate::input_document::PasteId {
        let mut document = InputDocument::new();
        document.insert_paste("a\nb\nc")
    }

    #[test]
    fn saves_edited_multiline_text() {
        let mut editor = PasteEditor::new();
        editor.open(PasteEditorTarget::Main, paste_id(), "a\nb\nc".into());
        editor.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        editor.handle_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));
        let action = editor.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(matches!(
            action,
            PasteEditorAction::Save { text, .. } if text == "a!\nb\nc"
        ));
    }

    #[test]
    fn blank_text_cannot_be_saved() {
        let mut editor = PasteEditor::new();
        editor.open(PasteEditorTarget::Main, paste_id(), " ".into());
        let action = editor.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(matches!(action, PasteEditorAction::Consumed));
    }

    #[test]
    fn wrapping_geometry_is_hard_and_width_aware() {
        let buffer = crate::text_buffer::TextBuffer::new("hello world".into());
        let rows = visual_rows(&buffer, 8);
        assert_eq!(
            rows.iter()
                .map(|row| row.start..row.end)
                .collect::<Vec<_>>(),
            vec![0..8, 8..11]
        );

        let buffer = crate::text_buffer::TextBuffer::new("ab界c".into());
        let rows = visual_rows(&buffer, 3);
        assert_eq!(
            rows.iter()
                .map(|row| row.start..row.end)
                .collect::<Vec<_>>(),
            vec![0..2, 2..4]
        );
    }

    #[test]
    fn manual_scroll_disables_cursor_follow_until_edit() {
        let mut editor = PasteEditor::new();
        editor.open(PasteEditorTarget::Main, paste_id(), "a\nb\nc".into());
        editor.scroll(-2);
        assert!(!editor.follow_cursor);

        editor.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(editor.follow_cursor);
    }
}
