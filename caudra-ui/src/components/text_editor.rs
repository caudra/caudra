//! A multi-line editor for the modals, built on the workbench's own buffer,
//! undo history and row painter.
//!
//! Everything below is the seam rather than the editing: selection, word and
//! line motions, indent and undo coalescing all live in `caudra-workbench`, and
//! the keymap is dispatched through its [`keys`] table, so a note or a pasted
//! blob is edited exactly the way a file is and the two maps cannot drift.
//!
//! Rows always wrap. A modal is narrow, and a window that pans sideways hides
//! the start of the line the reader is in the middle of writing.

use std::ops::Range;
use std::time::Instant;

use caudra_grab::grab_scope;
use caudra_workbench::buffer::{Buffer, Cursor, Edit};
use caudra_workbench::history::History;
use caudra_workbench::{Clicks, keys, render};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use super::scrollbar::{self, Scrollbar, ScrollbarMouse};
use crate::theme;
use unicode_width::UnicodeWidthChar;

/// How far a drag that has run off the top or bottom of the body scrolls per
/// report, matching the workbench's own edge scroll.
const EDGE_SCROLL_ROWS: i32 = 1;
const MIN_GUTTER_BODY_WIDTH: u16 = 2;
const NARROW_GLYPH: &str = "�";

/// What a key left for the host to do.
pub(crate) enum EditorKey {
    Consumed,
    /// Text the host should put on the system clipboard.
    Copy(String),
    /// The key must reach the host. Only `Ctrl+C` with nothing selected answers
    /// this way: the editor fills a modal that swallows every other key, and
    /// that chord is the way out of the session before it is a copy. Anything
    /// else the editor does not understand is swallowed, so a stray chord
    /// cannot act on the transcript behind the modal.
    Passthrough,
}

pub(crate) enum EditorMouse {
    Consumed,
    Copy(String),
    Passthrough,
}

pub(crate) struct TextEditor {
    buffer: Buffer,
    history: History,
    /// The editor's own clipboard, the way `Workbench` keeps one: a terminal
    /// delivers a real paste as a paste event, so `Ctrl+V` is only ever asked
    /// for what this editor last cut or copied.
    clipboard: String,
    /// First visual row on screen.
    scroll: usize,
    /// Body rect of the last frame, which is what a press is hit-tested against.
    area: Rect,
    scrollbar: Scrollbar,
    clicks: Clicks,
    dragging: bool,
    follow_cursor: bool,
}

impl Default for TextEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl TextEditor {
    pub fn new() -> Self {
        Self {
            buffer: Buffer::new(Vec::new()),
            history: History::default(),
            clipboard: String::new(),
            scroll: 0,
            area: Rect::ZERO,
            scrollbar: Scrollbar::default(),
            clicks: Clicks::default(),
            dragging: false,
            follow_cursor: true,
        }
    }

    pub fn set_text(&mut self, text: String) {
        self.buffer = Buffer::new(text.split('\n').map(str::to_owned).collect());
        self.history = History::default();
        self.scroll = 0;
        self.cancel_selection();
        self.follow_cursor = true;
    }

    pub fn text(&self) -> String {
        self.buffer.lines().join("\n")
    }

    pub fn move_to_end(&mut self) {
        self.buffer.move_document_end(false);
        self.follow_cursor = true;
    }

    fn byte_len(&self) -> usize {
        self.buffer.lines().iter().map(String::len).sum::<usize>()
            + self.buffer.line_count().saturating_sub(1)
    }

    fn admits_insert(&self, bytes: usize, limit: usize) -> bool {
        let removed = self.buffer.selected_text().map_or(0, |text| text.len());
        self.byte_len()
            .saturating_sub(removed)
            .saturating_add(bytes)
            <= limit
    }

    pub fn handle_paste_bounded(&mut self, text: &str, limit: usize) -> bool {
        if !text.is_empty() && !self.admits_insert(text.len(), limit) {
            return false;
        }
        self.handle_paste(text);
        true
    }

    pub fn handle_key_bounded(&mut self, key: KeyEvent, limit: usize) -> Result<EditorKey, ()> {
        let typing = (key.modifiers - KeyModifiers::SHIFT).is_empty();
        if keys::PASTE.matches(key) {
            if !self.clipboard.is_empty() && !self.admits_insert(self.clipboard.len(), limit) {
                return Err(());
            }
        } else if let KeyCode::Char(character) = key.code {
            if typing && !self.admits_insert(character.len_utf8(), limit) {
                return Err(());
            }
        } else if (key.code == KeyCode::Enter && typing) || key.code == KeyCode::Tab {
            let before = self.byte_len();
            let cursor = self.buffer.cursor();
            let selection = self.buffer.selection();
            let edit = if key.code == KeyCode::Enter {
                self.buffer.insert_newline()
            } else {
                self.buffer.insert_indent()
            };
            if let Some(edit) = edit {
                let after = before
                    .saturating_sub(edit.removed.len())
                    .saturating_add(edit.inserted.len());
                if after > limit {
                    self.buffer.replay(&edit.inverted());
                    if let Some((start, end)) = selection {
                        self.buffer
                            .set_cursor(if cursor == start { end } else { start }, false);
                        self.buffer.set_cursor(cursor, true);
                    }
                    return Err(());
                }
                self.record(Some(edit));
            }
            return Ok(EditorKey::Consumed);
        }
        Ok(self.handle_key(key))
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> EditorKey {
        if keys::COPY.matches(key) {
            // Nothing selected means nothing to copy, and the chord is the way
            // out of the session before it is an editor binding.
            let Some(text) = self.buffer.selected_text() else {
                return EditorKey::Passthrough;
            };
            self.clipboard.clone_from(&text);
            return EditorKey::Copy(text);
        }
        if keys::CUT.matches(key) {
            return self.cut();
        }
        if keys::PASTE.matches(key) {
            let text = std::mem::take(&mut self.clipboard);
            self.handle_paste(&text);
            self.clipboard = text;
            return EditorKey::Consumed;
        }
        if keys::SELECT_ALL.matches(key) {
            self.buffer.select_all();
            self.follow_cursor = true;
            return EditorKey::Consumed;
        }
        if keys::UNDO.matches(key) || keys::REDO.matches(key) {
            let edit = match keys::UNDO.matches(key) {
                true => self.history.undo(),
                false => self.history.redo(),
            };
            if let Some(edit) = edit {
                self.buffer.replay(&edit);
                self.follow_cursor = true;
            }
            return EditorKey::Consumed;
        }
        if keys::KILL_LINE.matches(key) {
            let edit = self.buffer.kill_to_end_of_line();
            self.record(edit);
            return EditorKey::Consumed;
        }
        if keys::DELETE_WORD.matches(key) {
            let edit = self.buffer.delete_word_left();
            self.record(edit);
            return EditorKey::Consumed;
        }
        self.motion_key(key)
    }

    /// Motions, the keys that move the caret and extend the selection without
    /// touching the text.
    fn motion_key(&mut self, key: KeyEvent) -> EditorKey {
        let extend = key.modifiers.contains(KeyModifiers::SHIFT);
        let by_word = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.area.height.max(1) as isize;
        match key.code {
            KeyCode::Left if by_word => self.buffer.move_word_left(extend),
            KeyCode::Left => self.buffer.move_left(extend),
            KeyCode::Right if by_word => self.buffer.move_word_right(extend),
            KeyCode::Right => self.buffer.move_right(extend),
            KeyCode::Up => self.buffer.move_vertical(-1, extend),
            KeyCode::Down => self.buffer.move_vertical(1, extend),
            KeyCode::PageUp => self.buffer.move_vertical(-page, extend),
            KeyCode::PageDown => self.buffer.move_vertical(page, extend),
            KeyCode::Home if by_word => self.buffer.move_document_start(extend),
            KeyCode::Home => self.buffer.move_home(extend),
            KeyCode::End if by_word => self.buffer.move_document_end(extend),
            KeyCode::End => self.buffer.move_end(extend),
            _ => return self.text_key(key),
        }
        self.history.break_group();
        self.follow_cursor = true;
        EditorKey::Consumed
    }

    fn text_key(&mut self, key: KeyEvent) -> EditorKey {
        let by_word = key.modifiers.contains(KeyModifiers::CONTROL);
        let typing = (key.modifiers - KeyModifiers::SHIFT).is_empty();
        let edit = match key.code {
            KeyCode::Char(ch) if typing => self.buffer.insert(&ch.to_string()),
            KeyCode::Enter if typing => self.buffer.insert_newline(),
            KeyCode::Tab => self.buffer.insert_indent(),
            KeyCode::BackTab => self.buffer.dedent(),
            KeyCode::Backspace if by_word => self.buffer.delete_word_left(),
            KeyCode::Backspace => self.buffer.backspace(),
            KeyCode::Delete if by_word => self.buffer.delete_word_right(),
            KeyCode::Delete => self.buffer.delete(),
            _ => return EditorKey::Consumed,
        };
        self.record(edit);
        EditorKey::Consumed
    }

    fn cut(&mut self) -> EditorKey {
        let Some(text) = self.buffer.selected_text() else {
            return EditorKey::Consumed;
        };
        self.clipboard.clone_from(&text);
        let edit = self.buffer.delete();
        self.record(edit);
        EditorKey::Copy(text)
    }

    pub fn handle_paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let edit = self.buffer.insert(text);
        self.record(edit);
        self.history.break_group();
    }

    fn record(&mut self, edit: Option<Edit>) {
        if let Some(edit) = edit {
            self.history.record(edit);
            self.follow_cursor = true;
        }
    }

    pub fn cancel_selection(&mut self) {
        self.buffer.set_cursor(self.buffer.cursor(), false);
        self.dragging = false;
        self.clicks = Clicks::default();
        self.scrollbar = Scrollbar::default();
    }

    pub fn handle_scrollbar_mouse(&mut self, event: &MouseEvent) -> bool {
        match self.scrollbar.handle(event) {
            ScrollbarMouse::Ignored => false,
            ScrollbarMouse::Consumed => true,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll = top as usize;
                self.follow_cursor = false;
                true
            }
        }
    }

    pub fn handle_mouse(&mut self, event: &MouseEvent) -> EditorMouse {
        if self.handle_scrollbar_mouse(event) {
            return EditorMouse::Consumed;
        }
        let at = (event.column, event.row);
        match event.kind {
            MouseEventKind::ScrollUp => self.scroll(EDGE_SCROLL_ROWS),
            MouseEventKind::ScrollDown => self.scroll(-EDGE_SCROLL_ROWS),
            MouseEventKind::Down(MouseButton::Left) => return self.press(at),
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                // Only a drag off the edge can select what the window does not
                // already show.
                if event.row < self.area.y {
                    self.scroll(EDGE_SCROLL_ROWS);
                } else if event.row >= self.area.bottom() {
                    self.scroll(-EDGE_SCROLL_ROWS);
                }
                if let Some(cursor) = self.cursor_at(at) {
                    self.buffer.set_cursor(cursor, true);
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                self.dragging = false;
                // The modal holds the mouse, so the terminal underneath can no
                // longer copy a selection for the reader.
                if let Some(text) = self.buffer.selected_text() {
                    self.clipboard.clone_from(&text);
                    return EditorMouse::Copy(text);
                }
            }
            _ => return EditorMouse::Passthrough,
        }
        EditorMouse::Consumed
    }

    /// One press drops the caret and arms a drag, two take the word under it,
    /// three take the whole line.
    fn press(&mut self, at: (u16, u16)) -> EditorMouse {
        if !self.area.contains(Position::new(at.0, at.1)) {
            return EditorMouse::Passthrough;
        }
        let Some(cursor) = self.cursor_at(at) else {
            return EditorMouse::Passthrough;
        };
        match self.clicks.press(at, Instant::now()) {
            1 => self.buffer.set_cursor(cursor, false),
            2 => self.buffer.select_word_at(cursor),
            _ => self.buffer.select_line_at(cursor.line),
        }
        self.dragging = true;
        self.follow_cursor = false;
        EditorMouse::Consumed
    }

    /// The buffer position under `at`, which a drag may have carried outside
    /// the body entirely.
    fn cursor_at(&self, at: (u16, u16)) -> Option<Cursor> {
        if self.area.width == 0 || self.area.height == 0 {
            return None;
        }
        let row = at.1.clamp(self.area.y, self.area.bottom() - 1);
        let column = at.0.clamp(self.area.x, self.area.right());
        // The same walk the frame was painted from, so a press on a wrapped row
        // cannot land on a different half of the line than it points at.
        let rows = self.visual_rows();
        let visual = rows
            .get(self.scroll + usize::from(row - self.area.y))
            .or_else(|| rows.last())?;
        let reached = visual.start + usize::from(column - self.area.x).min(visual.span);
        let col = render::char_index(self.buffer.line(visual.line), reached);
        if self.area.width == 1
            && column == self.area.right()
            && col == render::char_index(self.buffer.line(visual.line), visual.start)
            && self
                .buffer
                .line(visual.line)
                .chars()
                .nth(col)
                .and_then(UnicodeWidthChar::width)
                .is_some_and(|width| width > 1)
        {
            return Some(Cursor::new(visual.line, col + 1));
        }
        Some(Cursor::new(visual.line, col))
    }

    /// Positive scrolls towards the start of the text, matching every other
    /// scrollable surface in the UI.
    pub fn scroll(&mut self, delta: i32) {
        self.scroll = self.scroll.saturating_add_signed(-delta as isize);
        self.follow_cursor = false;
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) {
        self.paint(frame, area, false, false);
    }

    pub fn view_json(&mut self, frame: &mut Frame, area: Rect) {
        self.paint(frame, area, false, true);
    }

    pub fn view_masked(&mut self, frame: &mut Frame, area: Rect) {
        self.paint(frame, area, true, false);
    }

    fn paint(&mut self, frame: &mut Frame, area: Rect, masked: bool, json: bool) {
        grab_scope!("text_editor", area);
        let height = usize::from(area.height.max(1));
        let full_rows = self.visual_rows_at(area.width);
        let gutter = scrollbar::enabled()
            && area.width > MIN_GUTTER_BODY_WIDTH
            && area.height > 0
            && full_rows.len() > height;
        let body = Rect {
            width: area.width.saturating_sub(u16::from(gutter)),
            ..area
        };
        let rows = if gutter {
            self.visual_rows_at(body.width)
        } else {
            full_rows
        };
        if self.area.width != body.width
            && !self.follow_cursor
            && let Some(anchor) = self.visual_rows().get(self.scroll)
        {
            self.scroll = rows
                .iter()
                .rposition(|row| {
                    row.line < anchor.line || (row.line == anchor.line && row.start <= anchor.start)
                })
                .unwrap_or(0);
        }
        if self.area != body {
            self.dragging = false;
            self.clicks = Clicks::default();
            self.scrollbar = Scrollbar::default();
        }
        self.area = body;
        if self.follow_cursor {
            self.reveal_cursor(&rows, height);
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(height));

        let theme = theme::current();
        let base = Style::new().fg(theme.foreground);
        let selection = Style::new().add_modifier(Modifier::REVERSED);
        let mut syntax_line = None;
        let mut syntax = Vec::new();
        let painted = rows
            .iter()
            .skip(self.scroll)
            .take(height)
            .map(|row| {
                let text = self.buffer.line(row.line);
                let hidden = masked.then(|| "*".repeat(text.chars().count()));
                let text = hidden.as_deref().unwrap_or(text);
                if json && syntax_line != Some(row.line) {
                    syntax_line = Some(row.line);
                    syntax = super::json_text::overlays(text);
                }
                let mut overlays = syntax.clone();
                overlays.extend(self.overlays(row.line, text, selection, theme.cursor));
                if body.width == 1 {
                    let index = render::char_index(text, row.start);
                    if text
                        .chars()
                        .nth(index)
                        .and_then(UnicodeWidthChar::width)
                        .is_some_and(|width| width > 1)
                    {
                        let style = overlays
                            .iter()
                            .filter(|(range, _)| range.contains(&index))
                            .fold(base, |style, (_, overlay)| style.patch(*overlay));
                        return Line::styled(NARROW_GLYPH, style);
                    }
                }
                render::Row {
                    text,
                    segments: None,
                    base,
                    fill: None,
                    overlays: &overlays,
                }
                .paint(row.start, row.span)
            })
            .collect::<Vec<Line<'static>>>();

        frame.render_widget(Paragraph::new(painted), body);
        self.scrollbar.draw(
            frame,
            if gutter { area } else { Rect::ZERO },
            rows.len().min(u32::MAX as usize) as u32,
            self.scroll.min(u32::MAX as usize) as u32,
        );
    }

    /// The selection on this line, then the caret over it, the same order the
    /// workbench paints a file's row in.
    fn overlays(
        &self,
        line: usize,
        text: &str,
        selection: Style,
        caret: Style,
    ) -> Vec<(Range<usize>, Style)> {
        let mut overlays = Vec::new();
        if let Some((from, to)) = self.buffer.selection()
            && (from.line..=to.line).contains(&line)
        {
            let start = if line == from.line { from.col } else { 0 };
            let end = match line == to.line {
                true => to.col,
                false => text.chars().count() + 1,
            };
            overlays.push((start..end, selection));
        }
        let cursor = self.buffer.cursor();
        if cursor.line == line {
            overlays.push((cursor.col..cursor.col + 1, caret));
        }
        overlays
    }

    fn reveal_cursor(&mut self, rows: &[VisualRow], height: usize) {
        let cursor = self.buffer.cursor();
        let column = render::display_column(self.buffer.line(cursor.line), cursor.col);
        let Some(at) = rows
            .iter()
            .rposition(|row| row.line == cursor.line && row.start <= column)
        else {
            return;
        };
        if at < self.scroll {
            self.scroll = at;
        } else if at >= self.scroll + height {
            self.scroll = at + 1 - height;
        }
    }

    /// Every visual row of the whole buffer. These documents are a note or a
    /// pasted blob rather than a file, so walking all of them per frame costs
    /// less than carrying a two-part scroll position.
    fn visual_rows(&self) -> Vec<VisualRow> {
        self.visual_rows_at(self.area.width)
    }

    fn visual_rows_at(&self, width: u16) -> Vec<VisualRow> {
        let width = usize::from(width.max(1));
        let mut rows = Vec::with_capacity(self.buffer.line_count());
        for line in 0..self.buffer.line_count() {
            let starts = render::wrap_columns(self.buffer.line(line), width);
            for (index, &start) in starts.iter().enumerate() {
                let span = starts
                    .get(index + 1)
                    .map_or(width, |next| (next - start).min(width));
                rows.push(VisualRow { line, start, span });
            }
        }
        rows
    }
}

#[derive(Clone, Copy)]
struct VisualRow {
    line: usize,
    /// Display column the row starts at.
    start: usize,
    /// Columns the row covers, which is short of the body width wherever a word
    /// was carried down whole.
    span: usize,
}

#[cfg(test)]
mod tests {
    use super::{EditorKey, EditorMouse, TextEditor};
    use crate::components::scrollbar;
    use caudra_workbench::scroll::SCROLLBAR_THUMB;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Rect;
    use ratatui::{Terminal, backend::TestBackend};
    use std::{env, process::Command};
    use test_case::test_case;
    use unicode_width::UnicodeWidthChar;

    const BODY: Rect = Rect::new(0, 0, 10, 4);
    const WRAPPED: &str = "hello world again";
    const WRAPPED_WORD_DELETED: &str = "hello world ";
    const DISABLED_GUTTER_TEST: &str = "CAUDRA_EDITOR_DISABLED_GUTTER_TEST";

    #[test_case("abcdefghijklmno", &["abcde", "fghij", "klmno"]; "unbroken_ascii")]
    #[test_case("abcd界efgh界ij", &["abcd ", "界efg", "h界ij"]; "wide_boundary")]
    fn gutter_preserves_every_content_column(text: &str, expected: &[&str]) {
        let mut editor = editor(text);
        let mut terminal = Terminal::new(TestBackend::new(6, 1)).unwrap();
        terminal
            .draw(|frame| editor.view(frame, frame.area()))
            .unwrap();
        assert_eq!(editor.area.width, 5);
        assert_eq!(editor.visual_rows().len(), expected.len());
        for (index, expected) in expected.iter().enumerate() {
            editor.scroll = index;
            editor.follow_cursor = false;
            terminal
                .draw(|frame| editor.view(frame, frame.area()))
                .unwrap();
            let shown = (0..5)
                .filter_map(|x| {
                    let cell = &terminal.backend().buffer()[(x, 0)];
                    if x > 0
                        && terminal.backend().buffer()[(x - 1, 0)]
                            .symbol()
                            .chars()
                            .next()
                            .is_some_and(|ch| ch.width() == Some(2))
                    {
                        None
                    } else {
                        Some(cell.symbol())
                    }
                })
                .collect::<String>();
            assert_eq!(shown, *expected);
            assert_eq!(
                terminal.backend().buffer()[(5, 0)].symbol(),
                SCROLLBAR_THUMB
            );
        }
        editor.scroll = 0;
        terminal
            .draw(|frame| editor.view(frame, frame.area()))
            .unwrap();
        press(&mut editor, 0, 0);
        for _ in 0..editor.visual_rows().len() {
            drag(&mut editor, 5, 1);
            terminal
                .draw(|frame| editor.view(frame, frame.area()))
                .unwrap();
        }
        assert_eq!(copied(release(&mut editor, 5, 1)).as_deref(), Some(text));
        assert_eq!(editor.text(), text);
    }

    #[test_case(0, 4; "wide_first_cell")]
    #[test_case(1, 4; "wide_second_cell")]
    #[test_case(2, 5; "after_wide_character")]
    #[test_case(4, 7; "last_content_column")]
    fn gutter_mouse_mapping_uses_the_rendered_unicode_wrap(column: u16, expected: usize) {
        let mut editor = editor("abcd界efgh界ij");
        let mut terminal = Terminal::new(TestBackend::new(6, 2)).unwrap();
        terminal
            .draw(|frame| editor.view(frame, frame.area()))
            .unwrap();
        assert_eq!(editor.area.width, 5);
        press(&mut editor, column, 1);
        assert_eq!(editor.buffer.cursor().col, expected);
        assert_eq!(editor.buffer.cursor().line, 0);
    }

    #[test_case(0; "zero")]
    #[test_case(1; "single_column")]
    #[test_case(2; "wide_character_minimum")]
    fn tiny_widths_keep_content_instead_of_spending_it_on_a_bar(width: u16) {
        let mut editor = editor("界\nx");
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        terminal
            .draw(|frame| editor.view(frame, frame.area()))
            .unwrap();
        assert_eq!(editor.area.width, width);
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| cell.symbol() != SCROLLBAR_THUMB)
        );
        if width > 0 {
            assert_eq!(
                terminal.backend().buffer()[(0, 0)].symbol(),
                if width == 1 {
                    super::NARROW_GLYPH
                } else {
                    "界"
                }
            );
            press(&mut editor, 0, 0);
            drag(&mut editor, width, 0);
            assert_eq!(
                copied(release(&mut editor, width, 0)).as_deref(),
                Some("界")
            );
        }
        assert_eq!(editor.text(), "界\nx");
    }

    #[test]
    fn fitting_text_does_not_create_self_sustaining_gutter_overflow() {
        let mut editor = editor("abcdefghij");
        let mut terminal = Terminal::new(TestBackend::new(5, 2)).unwrap();
        for _ in 0..3 {
            terminal
                .draw(|frame| editor.view(frame, frame.area()))
                .unwrap();
            assert_eq!(editor.area.width, 5);
            assert_eq!(editor.scroll, 0);
            assert_eq!(terminal.backend().buffer()[(4, 0)].symbol(), "e");
            assert_eq!(terminal.backend().buffer()[(4, 1)].symbol(), "j");
        }
        editor.handle_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        let EditorKey::Copy(text) =
            editor.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        else {
            panic!("copy selection")
        };
        assert_eq!(text, "abcdefghij");
    }

    #[test]
    fn single_column_drag_stops_before_the_next_wrapped_wide_character() {
        let mut editor = editor("a界");
        let mut terminal = Terminal::new(TestBackend::new(1, 1)).unwrap();
        terminal
            .draw(|frame| editor.view(frame, frame.area()))
            .unwrap();
        press(&mut editor, 0, 0);
        drag(&mut editor, 1, 0);
        assert_eq!(copied(release(&mut editor, 1, 0)).as_deref(), Some("a"));
    }

    #[test]
    fn preference_and_resize_reflow_preserve_source_anchor_and_exact_copy() {
        if env::var_os(DISABLED_GUTTER_TEST).is_none() {
            assert!(Command::new(env::current_exe().unwrap()).args(["--exact", "components::text_editor::tests::preference_and_resize_reflow_preserve_source_anchor_and_exact_copy"]).env(DISABLED_GUTTER_TEST, "1").status().unwrap().success());
            return;
        }
        const SOURCE: &str = "abcdefghij界klmnopqrstuv";
        let mut editor = editor(SOURCE);
        let mut terminal = Terminal::new(TestBackend::new(6, 2)).unwrap();
        terminal
            .draw(|frame| editor.view(frame, frame.area()))
            .unwrap();
        editor.scroll(-2);
        terminal
            .draw(|frame| editor.view(frame, frame.area()))
            .unwrap();
        let anchor = editor.visual_rows()[editor.scroll].start;
        scrollbar::set_enabled(false);
        terminal
            .draw(|frame| editor.view(frame, frame.area()))
            .unwrap();
        assert_eq!(editor.area.width, 6);
        let rows = editor.visual_rows();
        assert!(rows[editor.scroll].start <= anchor);
        assert!(
            rows.get(editor.scroll + 1)
                .is_none_or(|row| row.start > anchor)
        );
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| cell.symbol() != SCROLLBAR_THUMB)
        );
        editor.scroll(i32::MAX);
        terminal
            .draw(|frame| editor.view(frame, frame.area()))
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(5, 0)].symbol(), "f");
        press(&mut editor, 0, 0);
        for _ in 0..editor.visual_rows().len() {
            drag(&mut editor, 6, 2);
            terminal
                .draw(|frame| editor.view(frame, frame.area()))
                .unwrap();
        }
        assert_eq!(copied(release(&mut editor, 6, 2)).as_deref(), Some(SOURCE));
        scrollbar::set_enabled(true);
        terminal
            .draw(|frame| editor.view(frame, frame.area()))
            .unwrap();
        assert_eq!(editor.area.width, 5);
        let mut wide = Terminal::new(TestBackend::new(40, 2)).unwrap();
        wide.draw(|frame| editor.view(frame, frame.area())).unwrap();
        assert_eq!(editor.area.width, 40);
        assert_eq!(editor.scroll, 0);
        let EditorKey::Copy(text) =
            editor.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        else {
            panic!("copy selection")
        };
        assert_eq!(text, SOURCE);
    }

    #[test_case("abcd", KeyCode::Char('X'), KeyModifiers::SHIFT, false; "shift_at_bound")]
    #[test_case("abc", KeyCode::Char('é'), KeyModifiers::NONE, false; "multibyte_over_bound")]
    #[test_case("ab", KeyCode::Char('é'), KeyModifiers::NONE, true; "multibyte_exact_bound")]
    #[test_case("abcd", KeyCode::Enter, KeyModifiers::SHIFT, false; "shift_enter")]
    #[test_case("abcd", KeyCode::Tab, KeyModifiers::NONE, false; "indent")]
    fn bounded_key_admission(text: &str, code: KeyCode, modifiers: KeyModifiers, allowed: bool) {
        const LIMIT: usize = 4;
        let mut editor = editor(text);
        editor.move_to_end();
        let cursor = editor.buffer.cursor();
        assert_eq!(
            editor
                .handle_key_bounded(KeyEvent::new(code, modifiers), LIMIT)
                .is_ok(),
            allowed
        );
        assert!(editor.text().len() <= LIMIT);
        if !allowed {
            assert_eq!(editor.text(), text);
            assert_eq!(editor.buffer.cursor(), cursor);
        }
    }

    #[test_case(false; "terminal_paste")]
    #[test_case(true; "internal_clipboard")]
    fn bounded_paste_replaces_selection_without_losing_rejected_selection(internal: bool) {
        const TEXT: &str = "é界";
        let mut editor = editor(TEXT);
        editor.clipboard = TEXT.into();
        editor.move_to_end();
        let paste = |editor: &mut TextEditor| {
            if internal {
                editor
                    .handle_key_bounded(
                        KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
                        TEXT.len(),
                    )
                    .is_ok()
            } else {
                editor.handle_paste_bounded(TEXT, TEXT.len())
            }
        };
        assert!(!paste(&mut editor));
        assert_eq!(editor.text(), TEXT);
        editor.buffer.select_all();
        let selection = editor.buffer.selection();
        assert!(!editor.handle_paste_bounded("too long", TEXT.len()));
        assert_eq!(editor.buffer.selection(), selection);
        assert!(paste(&mut editor));
        assert_eq!(editor.text(), TEXT);
    }

    #[test_case(KeyCode::Enter; "auto_indent")]
    #[test_case(KeyCode::Tab; "selection_indent")]
    fn bounded_indentation_preserves_selection_and_history_when_rejected(code: KeyCode) {
        const TEXT: &str = "    a\n    b";
        let mut editor = editor(TEXT);
        editor.buffer.select_all();
        let selection = editor.buffer.selection();
        let limit = if code == KeyCode::Enter {
            1
        } else {
            TEXT.len()
        };
        assert!(
            editor
                .handle_key_bounded(KeyEvent::new(code, KeyModifiers::NONE), limit)
                .is_err()
        );
        assert_eq!(editor.text(), TEXT);
        assert_eq!(editor.buffer.selection(), selection);
        assert!(editor.history.undo().is_none());
    }

    #[test]
    fn bounded_editor_can_delete_and_undo_over_limit_text() {
        const TEXT: &str = "oversized";
        let mut editor = editor(TEXT);
        editor.buffer.select_all();
        assert!(
            editor
                .handle_key_bounded(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), 1)
                .is_ok()
        );
        assert_eq!(editor.text(), "");
        assert!(
            editor
                .handle_key_bounded(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL), 1)
                .is_ok()
        );
        assert_eq!(editor.text(), TEXT);
        editor.buffer.select_all();
        assert!(
            editor
                .handle_key_bounded(
                    KeyEvent::new(KeyCode::Char('é'), KeyModifiers::SHIFT),
                    'é'.len_utf8()
                )
                .is_ok()
        );
        assert_eq!(editor.text(), "é");
    }

    #[test]
    fn json_view_preserves_scrolling_and_unicode_selection() {
        use ratatui::{Terminal, backend::TestBackend};

        const JSON: &str = "Heading\n{\n  \"界\": true,\n  \"next\": null\n}\nEnd";
        let mut editor = editor(JSON);
        let mut terminal = Terminal::new(TestBackend::new(24, 3)).unwrap();
        terminal
            .draw(|frame| editor.view_json(frame, frame.area()))
            .unwrap();
        editor.scroll(-2);
        terminal
            .draw(|frame| editor.view_json(frame, frame.area()))
            .unwrap();
        assert_eq!(editor.scroll, 2);
        assert_eq!(
            terminal.backend().buffer()[(3, 0)].fg,
            crate::theme::current().accent.fg.unwrap()
        );
        press(&mut editor, 2, 0);
        drag(&mut editor, 6, 0);
        terminal
            .draw(|frame| editor.view_json(frame, frame.area()))
            .unwrap();
        assert!(
            terminal.backend().buffer()[(3, 0)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        assert_eq!(editor.text(), JSON);
    }

    fn editor(text: &str) -> TextEditor {
        let mut editor = TextEditor::new();
        editor.set_text(text.to_owned());
        editor.area = BODY;
        editor
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn press(editor: &mut TextEditor, column: u16, row: u16) -> EditorMouse {
        editor.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), column, row))
    }

    fn drag(editor: &mut TextEditor, column: u16, row: u16) -> EditorMouse {
        editor.handle_mouse(&mouse(MouseEventKind::Drag(MouseButton::Left), column, row))
    }

    fn release(editor: &mut TextEditor, column: u16, row: u16) -> EditorMouse {
        editor.handle_mouse(&mouse(MouseEventKind::Up(MouseButton::Left), column, row))
    }

    fn copied(outcome: EditorMouse) -> Option<String> {
        match outcome {
            EditorMouse::Copy(text) => Some(text),
            _ => None,
        }
    }

    #[test_case("abc", 1, &[(0, 0, 10)] ; "a lines last row spans the whole width")]
    #[test_case(WRAPPED, 1, &[(0, 0, 6), (0, 6, 6), (0, 12, 10)] ; "a long line wraps on its spaces")]
    #[test_case("a\nb", 2, &[(0, 0, 10), (1, 0, 10)] ; "each line starts its own row")]
    fn visual_rows_follow_the_wrap(text: &str, lines: usize, expected: &[(usize, usize, usize)]) {
        let editor = editor(text);
        assert_eq!(editor.buffer.line_count(), lines);
        let rows: Vec<(usize, usize, usize)> = editor
            .visual_rows()
            .iter()
            .map(|row| (row.line, row.start, row.span))
            .collect();
        assert_eq!(rows, expected);
    }

    #[test_case(0, 0, (0, 0) ; "first cell is the start")]
    #[test_case(3, 0, (0, 3) ; "a column inside the first row")]
    #[test_case(2, 1, (0, 8) ; "the second row is offset by its wrap")]
    #[test_case(4, 2, (0, 16) ; "past the end of the text clamps to it")]
    fn a_press_lands_the_caret_on_the_char_under_it(
        column: u16,
        row: u16,
        expected: (usize, usize),
    ) {
        let mut editor = editor(WRAPPED);
        press(&mut editor, column, row);
        let cursor = editor.buffer.cursor();
        assert_eq!((cursor.line, cursor.col), expected);
    }

    #[test]
    fn ctrl_w_deletes_the_word_before_the_caret() {
        let mut editor = editor(WRAPPED);
        editor.move_to_end();
        editor.handle_key(key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(editor.text(), WRAPPED_WORD_DELETED);
    }

    #[test]
    fn a_drag_selects_across_wrapped_rows() {
        let mut editor = editor(WRAPPED);
        press(&mut editor, 0, 0);
        drag(&mut editor, 5, 1);
        assert_eq!(
            editor.buffer.selected_text().as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn releasing_a_drag_hands_the_selection_over_to_be_copied() {
        let mut editor = editor(WRAPPED);
        press(&mut editor, 0, 0);
        drag(&mut editor, 5, 0);
        assert_eq!(copied(release(&mut editor, 5, 0)).as_deref(), Some("hello"));
    }

    #[test]
    fn a_press_that_selected_nothing_copies_nothing() {
        let mut editor = editor(WRAPPED);
        press(&mut editor, 2, 0);
        assert!(copied(release(&mut editor, 2, 0)).is_none());
    }

    #[test]
    fn a_second_press_on_the_same_cell_takes_the_word() {
        let mut editor = editor(WRAPPED);
        press(&mut editor, 2, 0);
        press(&mut editor, 2, 0);
        assert_eq!(editor.buffer.selected_text().as_deref(), Some("hello"));
    }

    #[test]
    fn a_third_press_takes_the_whole_line() {
        let mut editor = editor("one\ntwo");
        press(&mut editor, 1, 0);
        press(&mut editor, 1, 0);
        press(&mut editor, 1, 0);
        assert_eq!(editor.buffer.selected_text().as_deref(), Some("one\n"));
    }

    #[test_case(2, "hello"; "word")]
    #[test_case(3, "hello world again\n"; "source_line_not_soft_wrap")]
    fn multi_click_copies_on_release(clicks: usize, expected: &str) {
        let mut editor = editor(&format!("{WRAPPED}\nnext"));
        for _ in 1..clicks {
            press(&mut editor, 2, 0);
            release(&mut editor, 2, 0);
        }
        press(&mut editor, 2, 0);
        assert_eq!(
            copied(release(&mut editor, 2, 0)).as_deref(),
            Some(expected)
        );
        assert!(copied(release(&mut editor, 2, 0)).is_none());
    }

    #[test]
    fn scrollbar_only_drag_outside_masked_editor_never_selects_or_copies() {
        use ratatui::{Terminal, backend::TestBackend};

        const SECRET: &str = "masked-secret";
        let source = SECRET.repeat(100);
        let mut editor = editor(&source);
        let mut terminal = Terminal::new(TestBackend::new(BODY.width, BODY.height)).unwrap();
        terminal
            .draw(|frame| editor.view_masked(frame, BODY))
            .unwrap();
        for (kind, column, row) in [
            (
                MouseEventKind::Down(MouseButton::Left),
                BODY.right() - 1,
                BODY.y,
            ),
            (
                MouseEventKind::Drag(MouseButton::Left),
                BODY.right() + 2,
                BODY.bottom() + 2,
            ),
            (
                MouseEventKind::Up(MouseButton::Left),
                BODY.right() + 2,
                BODY.bottom() + 2,
            ),
        ] {
            assert!(editor.handle_scrollbar_mouse(&mouse(kind, column, row)));
        }
        assert!(editor.scroll > 0);
        assert!(editor.buffer.selection().is_none());
        assert!(editor.clipboard.is_empty());
        assert_eq!(editor.text(), source);
    }

    #[test]
    fn edge_drag_copies_offscreen_unicode_source_and_scroll_retains_selection() {
        use ratatui::{Terminal, backend::TestBackend};

        const LINE: &str = "界e\u{301}🙂 alpha beta gamma delta";
        const TAIL: &str = "tail";
        let source = format!("{LINE}\n{LINE}\n{TAIL}");
        let mut editor = editor(&source);
        let mut terminal = Terminal::new(TestBackend::new(BODY.width, BODY.height)).unwrap();
        terminal.draw(|frame| editor.view(frame, BODY)).unwrap();
        press(&mut editor, 0, 0);
        for _ in 0..editor.visual_rows().len() {
            drag(&mut editor, BODY.right() - 2, BODY.bottom());
            terminal.draw(|frame| editor.view(frame, BODY)).unwrap();
        }
        assert_eq!(
            copied(release(&mut editor, BODY.right() - 2, BODY.bottom())).as_deref(),
            Some(source.as_str())
        );
        editor.scroll(i32::MAX);
        terminal.draw(|frame| editor.view(frame, BODY)).unwrap();
        let EditorKey::Copy(text) =
            editor.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        else {
            panic!("selection lost on scroll");
        };
        assert_eq!(text, source);
        editor.cancel_selection();
        assert!(matches!(
            editor.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            EditorKey::Passthrough
        ));
    }

    #[test]
    fn copy_without_a_selection_is_left_for_the_host() {
        let mut editor = editor("hello");
        assert!(matches!(
            editor.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            EditorKey::Passthrough
        ));
    }

    #[test]
    fn copy_with_a_selection_hands_the_text_over() {
        let mut editor = editor("hello");
        editor.handle_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        let EditorKey::Copy(text) =
            editor.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        else {
            panic!("a selected buffer must answer Ctrl+C with its text");
        };
        assert_eq!(text, "hello");
    }

    #[test]
    fn cut_then_paste_restores_what_was_taken() {
        let mut editor = editor("hello");
        editor.handle_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        editor.handle_key(key(KeyCode::Delete, KeyModifiers::SHIFT));
        assert_eq!(editor.text(), "");
        editor.handle_key(key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert_eq!(editor.text(), "hello");
    }

    #[test]
    fn typing_over_a_selection_replaces_it() {
        let mut editor = editor("hello");
        editor.handle_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        editor.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(editor.text(), "x");
    }

    #[test]
    fn undo_takes_back_a_run_of_typing_and_redo_puts_it_back() {
        let mut editor = editor("");
        for ch in "abc".chars() {
            editor.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        editor.handle_key(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(editor.text(), "");
        editor.handle_key(key(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(editor.text(), "abc");
    }

    #[test]
    fn shift_arrow_extends_the_selection() {
        let mut editor = editor("hello");
        for _ in 0..2 {
            editor.handle_key(key(KeyCode::Right, KeyModifiers::SHIFT));
        }
        assert_eq!(editor.buffer.selected_text().as_deref(), Some("he"));
    }

    #[test]
    fn a_pasted_blob_keeps_its_line_breaks() {
        let mut editor = editor("");
        editor.handle_paste("one\ntwo");
        assert_eq!(editor.text(), "one\ntwo");
    }

    #[test]
    fn an_unclaimed_key_is_swallowed_rather_than_left_to_act_behind_the_modal() {
        let mut editor = editor("hello");
        assert!(matches!(
            editor.handle_key(key(KeyCode::Esc, KeyModifiers::NONE)),
            EditorKey::Consumed
        ));
    }
}
