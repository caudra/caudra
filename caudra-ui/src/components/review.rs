//! Passage review: mark ranges of a rendered message and attach notes, then
//! hand the whole batch back to the model as one prompt.
//!
//! The modal never re-renders the message. It borrows the painted lines and
//! the `Provenance` the transcript already built, then lays them out at its
//! own width. Confirming a range asks provenance for the markdown that
//! produced those rows, so a quote is the original source rather than the
//! glyphs on screen. Segments with nothing markdown behind them (tool
//! buffers, images) report no provenance, and those fall back to scraping a
//! throwaway buffer the same way transcript copy does.
//!
//! Row indices are display rows at the width of the last render, so they go
//! stale when the terminal resizes. Notes record that width and only draw
//! their gutter marker while it still matches; the quote they captured stays
//! correct either way.

use std::fmt::Write as _;
use std::ops::Range;

use caudra_grab::grab_scope;
use caudra_markdown::render::SpanSource;
use caudra_workbench::Clicks;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use unicode_width::UnicodeWidthChar;

use super::keybindings::key;
use super::messages::{ASSISTANT_LABEL, ReviewTarget};
use super::modal::Modal;
use super::scrollbar::{Scrollbar, ScrollbarMouse};
use super::text_editor::{EditorKey, EditorMouse, TextEditor};
use super::{DisplaySource, Hint, HintBar, Overlay};
use crate::markdown;
use crate::provenance::{LineProvenance, Provenance};
use crate::selection::{self, LineBreaks, ScreenSelection, line_chars, wrap_breaks};
use crate::theme;

const PASSAGE_TITLE: &str = " Review reply ";
const NOTE_TITLE: &str = " Review note ";
const MODAL_WIDTH_PERCENT: u16 = 80;
const MODAL_MAX_HEIGHT_PERCENT: u16 = 80;
const META_ROWS: u16 = 1;
const HINT_ROWS: u16 = 1;
const MIN_CONTENT_ROWS: u16 = 5;
const MIN_EDITOR_ROWS: u16 = 3;
const GUTTER_WIDTH: u16 = 2;
const QUOTE_ROWS: u16 = 2;
const RANGE_MARKER: &str = "▌ ";
const NOTE_MARKER: &str = "● ";
const BLANK_MARKER: &str = "  ";

const REVIEW_OPEN: &str = "<review>";
const REVIEW_CLOSE: &str = "</review>";
const REVIEW_PREAMBLE: &str = "Address each note on my previous message.";
const NOTE_OPEN: &str = "<note";
const NOTE_CLOSE: &str = "</note>";
const QUOTE_PREFIX: &str = "> ";
const SURFACE_ATTR: &str = " surface=\"";
const CARD_TITLE: &str = "Review";
const CARD_BAR: &str = "▏ ";

pub(crate) struct ReviewNote {
    source: DisplaySource,
    label: &'static str,
    rows: (u16, u16),
    width: u16,
    quote: String,
    comment: String,
}

struct Target {
    source: DisplaySource,
    label: &'static str,
    lines: Vec<Line<'static>>,
    provenance: Option<Provenance>,
}

/// A cell of the passage: a display row, and a column inside the content width.
/// Ordering is reading order, which is what makes a span of two of them a
/// selection.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Cell {
    row: u16,
    col: u16,
}

enum Mode {
    Passage {
        cursor: Cell,
        /// Where a selection started. `None`, or an anchor the cursor never
        /// left, means the whole row is taken rather than one character: a
        /// click and a bare caret both mean "this line".
        anchor: Option<Cell>,
    },
    Note {
        rows: (u16, u16),
        quote: String,
        editing: Option<usize>,
    },
}

pub(crate) enum ReviewAction {
    Consumed,
    Passthrough,
    Submit(String),
    /// Text the host should put on the system clipboard.
    Copy(String),
    Close,
}

pub(crate) struct ReviewModal {
    target: Option<Target>,
    notes: Vec<ReviewNote>,
    mode: Mode,
    note: TextEditor,
    clicks: Clicks,
    scroll: u16,
    scrollbar: Scrollbar,
    rows_total: u16,
    width: u16,
    content: Rect,
    popup: Rect,
    hints: HintBar,
}

impl ReviewModal {
    pub fn new() -> Self {
        Self {
            target: None,
            notes: Vec::new(),
            mode: Mode::Passage {
                cursor: Cell::default(),
                anchor: None,
            },
            note: TextEditor::new(),
            clicks: Clicks::default(),
            scroll: 0,
            scrollbar: Scrollbar::default(),
            rows_total: 0,
            width: 0,
            content: Rect::default(),
            popup: Rect::default(),
            hints: HintBar::default(),
        }
    }

    pub fn open(&mut self, source: DisplaySource, target: ReviewTarget) {
        self.target = Some(Target {
            source,
            label: target.label,
            lines: target.lines,
            provenance: target.provenance,
        });
        self.mode = Mode::Passage {
            cursor: Cell::default(),
            anchor: None,
        };
        self.scroll = 0;
        self.rows_total = 0;
    }

    pub fn notes_pending(&self) -> usize {
        self.notes.len()
    }

    /// Drops notes along with the target. Reserved for transcript resets,
    /// where the sources the notes point at no longer exist.
    pub fn discard(&mut self) {
        self.notes.clear();
        self.close();
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.popup.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll = super::apply_scroll_delta(self.scroll, delta);
        self.clamp_scroll();
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        if !matches!(self.mode, Mode::Note { .. }) {
            return false;
        }
        self.note.handle_paste(text);
        true
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ReviewAction {
        if self.target.is_none() {
            return ReviewAction::Close;
        }
        match self.mode {
            Mode::Passage { cursor, anchor } => self.passage_key(key, cursor, anchor),
            Mode::Note { .. } => self.note_key(key),
        }
    }

    pub(crate) fn text_input_active(&self) -> bool {
        self.target.is_some() && matches!(self.mode, Mode::Note { .. })
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> ReviewAction {
        if self.target.is_none() {
            return ReviewAction::Passthrough;
        }
        // The hint row sits outside both the passage and the note editor, so
        // it is asked before either claims the pointer.
        if let Some(key) = self.hints.handle_mouse(event) {
            return self.handle_key(key);
        }
        // The note editor owns the whole pointer while it is up, bar and wheel
        // included: the passage behind it is not what the pointer is on.
        let Mode::Passage { .. } = self.mode else {
            return match self.note.handle_mouse(&event) {
                EditorMouse::Consumed => ReviewAction::Consumed,
                EditorMouse::Copy(text) => ReviewAction::Copy(text),
                EditorMouse::Passthrough => ReviewAction::Passthrough,
            };
        };
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return ReviewAction::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll = top as u16;
                return ReviewAction::Consumed;
            }
        }
        if let MouseEventKind::ScrollUp | MouseEventKind::ScrollDown = event.kind {
            let delta = if event.kind == MouseEventKind::ScrollUp {
                1
            } else {
                -1
            };
            self.scroll(delta);
            return ReviewAction::Consumed;
        }
        let inside = self
            .content
            .contains(Position::new(event.column, event.row));
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if inside => {
                self.press(event.column, event.row);
                ReviewAction::Consumed
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let at = self.cell_at(event.column, event.row);
                if let Mode::Passage { cursor, anchor } = &mut self.mode {
                    anchor.get_or_insert(*cursor);
                    *cursor = at;
                }
                ReviewAction::Consumed
            }
            MouseEventKind::Up(MouseButton::Left) if inside => match self.selected_text() {
                Some(text) => ReviewAction::Copy(text),
                None => ReviewAction::Consumed,
            },
            _ => ReviewAction::Passthrough,
        }
    }

    /// One press drops the caret and takes the row, two take the word under it,
    /// three take the row again, which is what a triple click means everywhere
    /// else and what the passage was already doing before it could select a
    /// word at all.
    fn press(&mut self, column: u16, row: u16) {
        let at = self.cell_at(column, row);
        let anchor = match self.clicks.press((column, row), std::time::Instant::now()) {
            2 => self.word_at(at),
            _ => None,
        };
        self.mode = match anchor {
            Some((start, end)) => Mode::Passage {
                cursor: end,
                anchor: Some(start),
            },
            None => Mode::Passage {
                cursor: at,
                anchor: Some(at),
            },
        };
    }

    fn passage_key(&mut self, key: KeyEvent, cursor: Cell, anchor: Option<Cell>) -> ReviewAction {
        let page = (self.content.height / 2).max(1);
        match key.code {
            KeyCode::Esc => {
                if anchor.is_some() {
                    self.mode = Mode::Passage {
                        cursor,
                        anchor: None,
                    };
                    return ReviewAction::Consumed;
                }
                return ReviewAction::Close;
            }
            KeyCode::Char('s') if key.modifiers == KeyModifiers::CONTROL => {
                if self.notes.is_empty() {
                    return ReviewAction::Consumed;
                }
                let compiled = compile(&self.notes);
                self.notes.clear();
                self.close();
                return ReviewAction::Submit(compiled);
            }
            // Without a selection there is nothing to copy, and the chord is
            // the way out of the session before it is a copy.
            KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                return match self.selected_text() {
                    Some(text) => ReviewAction::Copy(text),
                    None => ReviewAction::Passthrough,
                };
            }
            KeyCode::Char('a') if key.modifiers == KeyModifiers::CONTROL => {
                self.mode = Mode::Passage {
                    cursor: self.last_cell(),
                    anchor: Some(Cell::default()),
                };
                return ReviewAction::Consumed;
            }
            KeyCode::Enter => {
                let rows = self.selected_rows();
                let quote = self.quote(self.selection());
                if quote.trim().is_empty() {
                    return ReviewAction::Consumed;
                }
                self.note.set_text(String::new());
                self.mode = Mode::Note {
                    rows,
                    quote,
                    editing: None,
                };
                return ReviewAction::Consumed;
            }
            KeyCode::Char('d') => {
                self.delete_note_at(cursor.row);
                return ReviewAction::Consumed;
            }
            KeyCode::Char('e') => {
                self.edit_note_at(cursor.row);
                return ReviewAction::Consumed;
            }
            KeyCode::Char('n') => return self.jump_note(cursor.row, true),
            KeyCode::Char('p') => return self.jump_note(cursor.row, false),
            _ => {}
        }

        let last = self.rows_total.saturating_sub(1);
        let right = self.width.saturating_sub(1);
        let by_word = key.modifiers.contains(KeyModifiers::CONTROL);
        let extend = key.modifiers.contains(KeyModifiers::SHIFT);
        // A vertical extension takes whole rows: a reader picking out prose
        // means lines, and carrying the column down would clip the row it just
        // took. `seed` is where the anchor lands when the extension starts, so
        // the row the caret was on is taken whole too.
        let vertical = |rows: u16, down: bool| {
            let moved = Cell {
                row: match down {
                    true => cursor.row.saturating_add(rows),
                    false => cursor.row.saturating_sub(rows),
                },
                col: match (extend, down) {
                    (true, true) => right,
                    (true, false) => 0,
                    (false, _) => cursor.col,
                },
            };
            let seed = Cell {
                col: if down { 0 } else { right },
                ..cursor
            };
            (moved, seed)
        };
        let (moved, seed) = match key.code {
            KeyCode::Down => vertical(1, true),
            KeyCode::Up => vertical(1, false),
            KeyCode::PageDown => vertical(page, true),
            KeyCode::PageUp => vertical(page, false),
            // Stepping off either end of a row carries on to the next, the way
            // a caret crosses a line break in any editor.
            KeyCode::Right if cursor.col >= right => (
                Cell {
                    row: cursor.row.saturating_add(1),
                    col: 0,
                },
                cursor,
            ),
            KeyCode::Right => (
                Cell {
                    col: cursor.col + 1,
                    ..cursor
                },
                cursor,
            ),
            KeyCode::Left if cursor.col == 0 => (
                Cell {
                    row: cursor.row.saturating_sub(1),
                    col: if cursor.row == 0 { 0 } else { right },
                },
                cursor,
            ),
            KeyCode::Left => (
                Cell {
                    col: cursor.col - 1,
                    ..cursor
                },
                cursor,
            ),
            KeyCode::Home if by_word => (Cell::default(), cursor),
            KeyCode::Home => (Cell { col: 0, ..cursor }, cursor),
            KeyCode::End if by_word => (self.last_cell(), cursor),
            KeyCode::End => (
                Cell {
                    col: right,
                    ..cursor
                },
                cursor,
            ),
            _ => return ReviewAction::Consumed,
        };
        let moved = Cell {
            row: moved.row.min(last),
            col: moved.col.min(right),
        };
        // Shift keeps the anchor and grows the span; a bare motion drops it, so
        // the caret goes back to meaning one row.
        let anchor = extend.then(|| anchor.unwrap_or(seed));
        self.set_cursor(moved, anchor);
        ReviewAction::Consumed
    }

    fn note_key(&mut self, key: KeyEvent) -> ReviewAction {
        if key.code == KeyCode::Esc {
            self.back_to_passage();
            return ReviewAction::Consumed;
        }
        if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
            self.save_note();
            return ReviewAction::Consumed;
        }
        match self.note.handle_key(key) {
            EditorKey::Consumed => ReviewAction::Consumed,
            EditorKey::Copy(text) => ReviewAction::Copy(text),
            EditorKey::Passthrough => ReviewAction::Passthrough,
        }
    }

    fn save_note(&mut self) {
        let Mode::Note {
            rows,
            ref quote,
            editing,
        } = self.mode
        else {
            return;
        };
        let comment = self.note.text();
        if comment.trim().is_empty() {
            return;
        }
        let Some(target) = &self.target else { return };
        let note = ReviewNote {
            source: target.source,
            label: target.label,
            rows,
            width: self.width,
            quote: quote.clone(),
            comment,
        };
        match editing {
            Some(index) if index < self.notes.len() => self.notes[index] = note,
            _ => self.notes.push(note),
        }
        self.back_to_passage();
    }

    fn back_to_passage(&mut self) {
        let cursor = self.caret();
        self.note.set_text(String::new());
        self.mode = Mode::Passage {
            cursor,
            anchor: None,
        };
    }

    fn edit_note_at(&mut self, cursor: u16) {
        let Some(index) = self.note_index_at(cursor) else {
            return;
        };
        let note = &self.notes[index];
        self.note.set_text(note.comment.clone());
        self.note.move_to_end();
        self.mode = Mode::Note {
            rows: note.rows,
            quote: note.quote.clone(),
            editing: Some(index),
        };
    }

    fn delete_note_at(&mut self, cursor: u16) {
        if let Some(index) = self.note_index_at(cursor) {
            self.notes.remove(index);
        }
    }

    fn jump_note(&mut self, cursor: u16, forward: bool) -> ReviewAction {
        let mut rows: Vec<u16> = self.visible_notes().map(|note| note.rows.0).collect();
        rows.sort_unstable();
        let next = if forward {
            rows.iter().find(|&&row| row > cursor).copied()
        } else {
            rows.iter().rev().find(|&&row| row < cursor).copied()
        };
        if let Some(row) = next.or_else(|| rows.first().copied()) {
            self.set_cursor(Cell { row, col: 0 }, None);
        }
        ReviewAction::Consumed
    }

    fn note_index_at(&self, cursor: u16) -> Option<usize> {
        let (source, width) = self.target.as_ref().map(|t| (t.source, self.width))?;
        self.notes.iter().position(|note| {
            note.source == source
                && note.width == width
                && note.rows.0 <= cursor
                && cursor <= note.rows.1
        })
    }

    fn visible_notes(&self) -> impl Iterator<Item = &ReviewNote> {
        let source = self.target.as_ref().map(|t| t.source);
        let width = self.width;
        self.notes
            .iter()
            .filter(move |note| Some(note.source) == source && note.width == width)
    }

    fn set_cursor(&mut self, cursor: Cell, anchor: Option<Cell>) {
        self.mode = Mode::Passage { cursor, anchor };
        self.follow_cursor(cursor.row);
    }

    fn follow_cursor(&mut self, cursor: u16) {
        let height = self.content.height.max(1);
        if cursor < self.scroll {
            self.scroll = cursor;
        } else if cursor >= self.scroll + height {
            self.scroll = cursor + 1 - height;
        }
        self.clamp_scroll();
    }

    fn clamp_scroll(&mut self) {
        let max = self.rows_total.saturating_sub(self.content.height);
        self.scroll = self.scroll.min(max);
    }

    /// The cell under a screen position, which a drag may have carried outside
    /// the content rect entirely.
    fn cell_at(&self, screen_col: u16, screen_row: u16) -> Cell {
        let row = self.scroll + screen_row.saturating_sub(self.content.y);
        Cell {
            row: row.min(self.rows_total.saturating_sub(1)),
            col: screen_col
                .saturating_sub(self.content.x)
                .min(self.width.saturating_sub(1)),
        }
    }

    fn last_cell(&self) -> Cell {
        Cell {
            row: self.rows_total.saturating_sub(1),
            col: self.width.saturating_sub(1),
        }
    }

    /// The span the passage will quote and highlight, in display rows and
    /// content columns. A caret that never left its anchor takes the whole row,
    /// which is what makes a click and a bare motion mean "this line".
    fn selection(&self) -> ScreenSelection {
        let Mode::Passage { cursor, anchor } = self.mode else {
            let (start, end) = self.selected_rows();
            return self.whole_rows(start, end);
        };
        let (start, end) = match anchor {
            Some(anchor) if anchor < cursor => (anchor, cursor),
            Some(anchor) => (cursor, anchor),
            None => (cursor, cursor),
        };
        if start == end {
            return self.whole_rows(start.row, end.row);
        }
        ScreenSelection {
            start_row: start.row,
            start_col: start.col,
            end_row: end.row,
            end_col: end.col,
        }
    }

    fn whole_rows(&self, start: u16, end: u16) -> ScreenSelection {
        ScreenSelection {
            start_row: start,
            start_col: 0,
            end_row: end,
            end_col: self.width.saturating_sub(1),
        }
    }

    /// The display rows the selection touches. Notes are keyed on rows, so this
    /// is what a note records however narrow the span inside them was.
    fn selected_rows(&self) -> (u16, u16) {
        let selection = self.selection();
        (selection.start_row, selection.end_row)
    }

    /// The selected text, or `None` when the caret never left its anchor. A
    /// bare caret selects a row for the purpose of quoting it, but copying it
    /// would be a copy the reader never asked for.
    fn selected_text(&self) -> Option<String> {
        let Mode::Passage { cursor, anchor } = self.mode else {
            return None;
        };
        if anchor.is_none_or(|anchor| anchor == cursor) {
            return None;
        }
        let text = self.quote(self.selection());
        (!text.trim().is_empty()).then_some(text)
    }

    /// The word around `at`, as an inclusive cell span. `None` where the row
    /// has no text under the pointer, which leaves a double click behaving like
    /// a single one.
    fn word_at(&self, at: Cell) -> Option<(Cell, Cell)> {
        let (chars, range) = self.row_chars(at.row)?;
        let row = &chars[range];
        let mut columns = Vec::with_capacity(row.len());
        let mut column = 0usize;
        for ch in row {
            columns.push(column);
            column += ch.width().unwrap_or(0).max(1);
        }
        let index = columns.partition_point(|&start| start <= usize::from(at.col));
        let index = index.checked_sub(1)?;
        let wanted = is_word(row[index]);
        let start = row[..index]
            .iter()
            .rposition(|ch| is_word(*ch) != wanted)
            .map_or(0, |found| found + 1);
        let end = row[index..]
            .iter()
            .position(|ch| is_word(*ch) != wanted)
            .map_or(row.len(), |offset| index + offset);
        let last = end.checked_sub(1)?;
        Some((
            Cell {
                row: at.row,
                col: u16::try_from(columns[start]).unwrap_or(u16::MAX),
            },
            Cell {
                row: at.row,
                col: u16::try_from(columns[last]).unwrap_or(u16::MAX),
            },
        ))
    }

    /// The chars of the logical line display row `row` belongs to, and the
    /// range of them that row shows. Replays the same wrap the frame was
    /// painted with, so a press cannot land on a different half of a line than
    /// it points at.
    fn row_chars(&self, row: u16) -> Option<(Vec<char>, Range<usize>)> {
        if self.width == 0 {
            return None;
        }
        let mut seen = 0u16;
        for line in self.target_lines() {
            let chars = line_chars(line);
            let mut starts = vec![0usize];
            starts.extend(wrap_breaks(&chars, self.width).into_iter().map(|b| b.start));
            let index = row
                .checked_sub(seen)
                .map(usize::from)
                .filter(|index| *index < starts.len());
            if let Some(index) = index {
                let start = starts[index];
                let end = starts.get(index + 1).copied().unwrap_or(chars.len());
                return Some((chars, start..end));
            }
            seen = seen.saturating_add(u16::try_from(starts.len()).unwrap_or(u16::MAX));
        }
        None
    }

    /// Markdown behind `sel`, preferring the source text provenance recorded
    /// when the segment was painted. Partial first and last rows are carried
    /// through: provenance narrows a covered row to the chars the selection
    /// touched, so a span inside a line quotes that span.
    fn quote(&self, sel: ScreenSelection) -> String {
        let Some(target) = &self.target else {
            return String::new();
        };
        let width = self.width;
        if width == 0 || self.rows_total == 0 {
            return String::new();
        }
        let start = sel.start_row;
        let end = sel.end_row.min(self.rows_total.saturating_sub(1));
        let sel = ScreenSelection {
            end_row: end,
            ..sel
        };
        if let Some(text) = target
            .provenance
            .as_ref()
            .and_then(|p| p.extract(&target.lines, width, &sel, start, end + 1))
        {
            return text;
        }

        let area = Rect::new(0, 0, width, end.saturating_add(1));
        let mut buffer = Buffer::empty(area);
        Paragraph::new(target.lines.clone())
            .wrap(Wrap { trim: false })
            .render(area, &mut buffer);
        let breaks = LineBreaks::from_lines(&target.lines, width);
        let mut out = String::new();
        selection::append_rows(&buffer, area, &sel, start, end + 1, &mut out, &breaks);
        out
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if self.target.is_none() {
            return Rect::default();
        }
        grab_scope!("review", area);
        match self.mode {
            Mode::Passage { .. } => self.view_passage(frame, area),
            Mode::Note { .. } => self.view_note(frame, area),
        }
    }

    fn view_passage(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        grab_scope!("review_passage", area);
        let modal = Modal {
            title: PASSAGE_TITLE,
            width_percent: MODAL_WIDTH_PERCENT,
            max_height_percent: MODAL_MAX_HEIGHT_PERCENT,
        };
        let desired = modal_body_rows(area, MIN_CONTENT_ROWS);
        let (popup, inner) = modal.render(frame, area, desired);
        self.popup = popup;

        let [meta_area, body, hint_area] = Layout::vertical([
            Constraint::Length(META_ROWS),
            Constraint::Min(MIN_CONTENT_ROWS),
            Constraint::Length(HINT_ROWS),
        ])
        .areas(inner);
        let [gutter, text] =
            Layout::horizontal([Constraint::Length(GUTTER_WIDTH), Constraint::Min(1)]).areas(body);

        self.content = text;
        self.width = text.width;
        self.rows_total = total_rows(self.target_lines(), text.width);
        let cursor = self.caret();
        self.follow_cursor(cursor.row.min(self.rows_total.saturating_sub(1)));

        self.render_meta(frame, meta_area);
        self.render_gutter(frame, gutter);
        self.render_passage(frame, text);
        self.hints.draw(
            frame,
            hint_area,
            vec![
                Hint::inert("Shift+↑↓", "select"),
                Hint::bind(key::ENTER, "note"),
                Hint::inert("e/d", "edit/delete"),
                Hint::inert("n/p", "jump"),
                Hint::bind(key::SAVE, "send"),
                Hint::bind(key::ESC, "close"),
            ],
        );
        popup
    }

    fn render_meta(&self, frame: &mut Frame, area: Rect) {
        grab_scope!("review_meta", area);
        let theme = theme::current();
        let notes = self.notes.len();
        let (start, end) = self.selected_rows();
        let rows = if start == end {
            format!("row {}", start + 1)
        } else {
            format!("rows {}-{}", start + 1, end + 1)
        };
        let label = self.target.as_ref().map_or("", |t| t.label);
        let line = Line::from(vec![
            Span::styled(format!(" {label} "), theme.active),
            Span::styled(
                format!("{notes} note{} · {rows}", plural(notes)),
                theme.item_desc,
            ),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_gutter(&self, frame: &mut Frame, area: Rect) {
        grab_scope!("review_gutter", area);
        let theme = theme::current();
        let (start, end) = self.selected_rows();
        let noted: Vec<(u16, u16)> = self.visible_notes().map(|note| note.rows).collect();
        let lines = (0..area.height)
            .map(|offset| {
                let row = self.scroll + offset;
                if row >= self.rows_total {
                    return Line::raw(BLANK_MARKER);
                }
                if row >= start && row <= end {
                    return Line::styled(RANGE_MARKER, theme.accent);
                }
                if noted.iter().any(|&(a, b)| row >= a && row <= b) {
                    return Line::styled(NOTE_MARKER, theme.item_desc);
                }
                Line::raw(BLANK_MARKER)
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_passage(&mut self, frame: &mut Frame, area: Rect) {
        let Some(target) = &self.target else { return };
        grab_scope!("review_passage_text", area);
        frame.render_widget(
            Paragraph::new(target.lines.clone())
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            area,
        );

        // The selection is kept in display rows and content columns; the
        // highlight wants screen cells, so it is offset by the window here and
        // nowhere else.
        let selection = self.selection();
        let (start, end) = (selection.start_row, selection.end_row);
        if start < self.scroll + area.height && end >= self.scroll {
            let top = area.y + start.saturating_sub(self.scroll);
            let bottom = area.y + (end - self.scroll).min(area.height.saturating_sub(1));
            let sel = ScreenSelection {
                start_row: top.max(area.y),
                start_col: area.x.saturating_add(selection.start_col),
                end_row: bottom,
                end_col: area
                    .x
                    .saturating_add(selection.end_col)
                    .min(area.right().saturating_sub(1)),
            };
            selection::apply_highlight(frame.buffer_mut(), area, &sel);
        }

        self.scrollbar
            .draw(frame, area, self.rows_total, self.scroll);
    }

    fn view_note(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let Mode::Note { ref quote, .. } = self.mode else {
            return Rect::default();
        };
        grab_scope!("review_note", area);
        let modal = Modal {
            title: NOTE_TITLE,
            width_percent: MODAL_WIDTH_PERCENT,
            max_height_percent: MODAL_MAX_HEIGHT_PERCENT,
        };
        let desired = modal_body_rows(area, MIN_EDITOR_ROWS + QUOTE_ROWS);
        let (popup, inner) = modal.render(frame, area, desired);
        self.popup = popup;

        let [quote_area, editor_area, hint_area] = Layout::vertical([
            Constraint::Length(QUOTE_ROWS),
            Constraint::Min(MIN_EDITOR_ROWS),
            Constraint::Length(HINT_ROWS),
        ])
        .areas(inner);

        let theme = theme::current();
        frame.render_widget(
            Paragraph::new(quote.as_str())
                .wrap(Wrap { trim: true })
                .style(theme.item_desc),
            quote_area,
        );
        self.note.view(frame, editor_area);
        self.hints.draw(
            frame,
            hint_area,
            vec![
                Hint::bind(key::SAVE, "save"),
                Hint::bind(key::SELECT_ALL, "select all"),
                Hint::bind(key::UNDO, "undo"),
                Hint::bind(key::ESC, "back"),
            ],
        );
        popup
    }

    fn target_lines(&self) -> &[Line<'static>] {
        self.target.as_ref().map_or(&[], |t| t.lines.as_slice())
    }

    /// Where the caret sits. A note being written puts it back on the first row
    /// of the passage it is about, which is where the reader left it.
    fn caret(&self) -> Cell {
        match self.mode {
            Mode::Passage { cursor, .. } => cursor,
            Mode::Note { rows, .. } => Cell {
                row: rows.0,
                col: 0,
            },
        }
    }
}

impl Overlay for ReviewModal {
    fn is_open(&self) -> bool {
        self.target.is_some()
    }

    /// Keeps the notes. Closing is how a user steps out to review a different
    /// message, and cancelling a run must not throw the batch away.
    fn close(&mut self) {
        self.target = None;
        self.note.set_text(String::new());
        self.mode = Mode::Passage {
            cursor: Cell::default(),
            anchor: None,
        };
        self.scroll = 0;
        self.rows_total = 0;
        self.content = Rect::default();
        self.popup = Rect::default();
        self.hints.reset();
    }
}

/// One `<note>` per entry, blockquoted passage first, comment second. The
/// surface attribute is omitted for plain assistant text, which is the common
/// case and needs no explaining.
pub(crate) fn compile(notes: &[ReviewNote]) -> String {
    let mut out = String::from(REVIEW_OPEN);
    out.push('\n');
    out.push_str(REVIEW_PREAMBLE);
    for note in notes {
        out.push_str("\n\n<note");
        if note.label != ASSISTANT_LABEL {
            let _ = write!(out, " surface=\"{}\"", note.label);
        }
        out.push_str(">\n");
        for line in note.quote.lines() {
            out.push_str(format!("{QUOTE_PREFIX}{line}").trim_end());
            out.push('\n');
        }
        out.push_str(note.comment.trim());
        out.push('\n');
        out.push_str(NOTE_CLOSE);
    }
    out.push('\n');
    out.push_str(REVIEW_CLOSE);
    out
}

/// A source line of a note. `range` covers the whole line in the compiled
/// text, including any `> ` prefix, so a copy returns what was really sent.
/// `body` is the offset the display text starts at, which is where the
/// markdown provenance of that display text has to be rebased.
pub(crate) struct ParsedLine {
    text: String,
    range: Range<u32>,
    body: u32,
}

/// One note recovered from a compiled block.
pub(crate) struct ParsedNote {
    pub surface: Option<String>,
    quote: Vec<ParsedLine>,
    comment: Vec<ParsedLine>,
}

impl ParsedNote {
    pub fn search_text(&self) -> String {
        format!("{}\n{}", join(&self.quote), join(&self.comment))
    }

    #[cfg(test)]
    fn quote_text(&self) -> String {
        join(&self.quote)
    }

    #[cfg(test)]
    fn comment_text(&self) -> String {
        join(&self.comment)
    }
}

fn join(lines: &[ParsedLine]) -> String {
    lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Recognises a prompt this module compiled, so the transcript can draw a card
/// instead of the raw tags. Anything that does not match the exact shape
/// `compile` writes is left alone and rendered as ordinary markdown.
pub(crate) fn parse(text: &str) -> Option<Vec<ParsedNote>> {
    let trimmed = text.trim();
    let body = trimmed
        .strip_prefix(REVIEW_OPEN)?
        .strip_suffix(REVIEW_CLOSE)?
        .trim()
        .strip_prefix(REVIEW_PREAMBLE)?;

    let mut notes = Vec::new();
    for chunk in body.split(NOTE_OPEN).skip(1) {
        let (head, rest) = chunk.split_once('>')?;
        let surface = head
            .strip_prefix(SURFACE_ATTR)
            .and_then(|value| value.strip_suffix('"'))
            .map(str::to_owned);
        if surface.is_none() && !head.is_empty() {
            return None;
        }
        let inner = rest.split_once(NOTE_CLOSE)?.0.trim_matches('\n');

        let mut quote = Vec::new();
        let mut comment = Vec::new();
        for line in inner.lines() {
            let start = offset_in(text, line);
            let quoted = line
                .strip_prefix(QUOTE_PREFIX)
                .or_else(|| line.strip_prefix(QUOTE_PREFIX.trim_end()));
            let range = start..start + u32::try_from(line.len()).unwrap_or(0);
            match quoted {
                Some(body) if comment.is_empty() => quote.push(ParsedLine {
                    body: start + u32::try_from(line.len() - body.len()).unwrap_or(0),
                    text: body.to_owned(),
                    range,
                }),
                _ => comment.push(ParsedLine {
                    body: start,
                    text: line.to_owned(),
                    range,
                }),
            }
        }
        if comment.iter().all(|line| line.text.trim().is_empty()) {
            return None;
        }
        notes.push(ParsedNote {
            surface,
            quote,
            comment,
        });
    }
    (!notes.is_empty()).then_some(notes)
}

/// Byte offset of a subslice inside the string it was sliced from.
fn offset_in(haystack: &str, part: &str) -> u32 {
    let base = haystack.as_ptr() as usize;
    let at = part.as_ptr() as usize;
    u32::try_from(at.saturating_sub(base)).unwrap_or(0)
}

/// Header, then each note as a barred quote followed by its comment. The
/// quote keeps its markdown so lists and emphasis survive the round trip.
///
/// Every painted row carries the source range behind it, so selecting the card
/// copies the `<review>` block the model received rather than the glyphs.
pub(crate) fn card_lines(
    notes: &[ParsedNote],
    width: u16,
    text_style: Style,
) -> (Vec<Line<'static>>, Vec<LineProvenance>, markdown::LinkMap) {
    let theme = theme::current();
    let bar_width = u16::try_from(CARD_BAR.chars().count()).unwrap_or(2);
    let quote_width = width.saturating_sub(bar_width).max(1);

    let mut lines = vec![Line::from(vec![
        Span::styled(CARD_TITLE, theme.accent),
        Span::styled(
            format!(" · {} note{}", notes.len(), plural(notes.len())),
            theme.item_desc,
        ),
    ])];
    let mut provenance = vec![LineProvenance::chrome(lines[0].spans.len())];
    let mut links = markdown::LinkMap::none_for(&lines);

    for note in notes {
        lines.push(Line::default());
        provenance.push(LineProvenance::chrome(0));
        links.rows.push(Vec::new());
        if let Some(surface) = &note.surface {
            lines.push(Line::from(Span::styled(
                format!("{CARD_BAR}{surface}"),
                theme.item_desc,
            )));
            provenance.push(LineProvenance::chrome(1));
            links.rows.push(vec![None]);
        }
        for line in &note.quote {
            push_source_line(
                &mut lines,
                &mut provenance,
                &mut links,
                line,
                quote_width,
                theme.tool_dim,
                Some(Span::styled(CARD_BAR, theme.subtle_border_style())),
            );
        }
        for line in &note.comment {
            push_source_line(
                &mut lines,
                &mut provenance,
                &mut links,
                line,
                width,
                text_style,
                None,
            );
        }
    }
    (lines, provenance, links)
}

/// Renders one source line and rebases its span provenance onto the compiled
/// text. A line normally paints as one row; anything that splits repeats the
/// same source range so a full-row selection still copies the whole line.
fn push_source_line(
    lines: &mut Vec<Line<'static>>,
    provenance: &mut Vec<LineProvenance>,
    links: &mut markdown::LinkMap,
    source: &ParsedLine,
    width: u16,
    style: Style,
    bar: Option<Span<'static>>,
) {
    let painted = markdown::text_to_painted_at(&source.text, style, width, source.body);
    for ((mut line, mut line_provenance), mut line_links) in painted
        .lines
        .into_iter()
        .zip(painted.provenance)
        .zip(painted.links.rows)
    {
        if let Some(bar) = bar.clone() {
            line.spans.insert(0, bar);
            line_provenance.spans.insert(0, SpanSource::Chrome);
            line_links.insert(0, None);
        }
        line_provenance.line = Some(source.range.clone());
        lines.push(line);
        provenance.push(line_provenance);
        links.rows.push(line_links);
    }
}

fn modal_body_rows(area: Rect, minimum: u16) -> u16 {
    area.height
        .saturating_mul(MODAL_MAX_HEIGHT_PERCENT)
        .div_ceil(100)
        .saturating_sub(super::modal::CHROME_LINES)
        .max(minimum + META_ROWS + HINT_ROWS)
}

/// Word characters for the passage's double click, matching the workbench's
/// own rule so a word means the same thing in both.
fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn total_rows(lines: &[Line<'_>], width: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    lines.iter().fold(0u16, |total, line| {
        let rows = wrap_breaks(&line_chars(line), width)
            .len()
            .saturating_add(1);
        total.saturating_add(u16::try_from(rows).unwrap_or(u16::MAX))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_providers::CaudraId;
    use test_case::test_case;

    const TOOL_LABEL: &str = "tool result";

    fn note(quote: &str, comment: &str, label: &'static str) -> ReviewNote {
        ReviewNote {
            source: DisplaySource::AssistantText(CaudraId::generate()),
            label,
            rows: (0, 0),
            width: 40,
            quote: quote.to_owned(),
            comment: comment.to_owned(),
        }
    }

    #[test]
    fn compiles_a_single_note_without_a_surface() {
        let compiled = compile(&[note("HNSW always wins.", "Overstated.", ASSISTANT_LABEL)]);
        assert_eq!(
            compiled,
            "<review>\nAddress each note on my previous message.\n\n\
             <note>\n> HNSW always wins.\nOverstated.\n</note>\n</review>"
        );
    }

    #[test]
    fn compiles_a_non_assistant_surface_as_an_attribute() {
        let compiled = compile(&[note("{\"ms\": 12}", "Source?", TOOL_LABEL)]);
        assert!(compiled.contains("<note surface=\"tool result\">"));
    }

    #[test]
    fn blockquotes_every_line_and_trims_blank_ones() {
        let compiled = compile(&[note("first\n\nsecond", "why", ASSISTANT_LABEL)]);
        assert!(compiled.contains("> first\n>\n> second\n"));
    }

    #[test]
    fn keeps_notes_in_order() {
        let compiled = compile(&[
            note("a", "first", ASSISTANT_LABEL),
            note("b", "second", ASSISTANT_LABEL),
        ]);
        let first = compiled.find("first").expect("first comment");
        let second = compiled.find("second").expect("second comment");
        assert!(first < second);
        assert_eq!(compiled.matches(NOTE_CLOSE).count(), 2);
    }

    fn modal_with_rows(rows: u16) -> ReviewModal {
        let mut modal = ReviewModal::new();
        modal.open(
            DisplaySource::AssistantText(CaudraId::generate()),
            ReviewTarget {
                lines: (0..rows).map(|i| Line::raw(format!("line {i}"))).collect(),
                provenance: None,
                label: ASSISTANT_LABEL,
            },
        );
        modal.width = 40;
        modal.rows_total = rows;
        modal.content = Rect::new(0, 0, 40, rows);
        modal
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click(modal: &mut ReviewModal, column: u16, row: u16) {
        modal.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), column, row));
    }

    #[test]
    fn the_caret_stops_at_both_ends() {
        let mut modal = modal_with_rows(3);
        modal.handle_key(press(KeyCode::Up));
        assert_eq!(modal.caret().row, 0);
        for _ in 0..5 {
            modal.handle_key(press(KeyCode::Down));
        }
        assert_eq!(modal.caret().row, 2);
    }

    #[test]
    fn a_bare_caret_takes_the_whole_row() {
        let mut modal = modal_with_rows(3);
        modal.handle_key(press(KeyCode::Right));
        assert_eq!(modal.selected_rows(), (0, 0));
        assert_eq!(modal.selection().start_col, 0);
        assert_eq!(modal.selection().end_col, modal.width - 1);
    }

    #[test]
    fn shift_extends_the_selection_and_a_bare_motion_drops_it() {
        let mut modal = modal_with_rows(3);
        modal.handle_key(shift(KeyCode::Down));
        assert_eq!(modal.selected_rows(), (0, 1));

        modal.handle_key(press(KeyCode::Down));
        assert_eq!(modal.selected_rows(), (2, 2));
    }

    #[test]
    fn shift_right_selects_inside_one_row() {
        let mut modal = modal_with_rows(3);
        for _ in 0..4 {
            modal.handle_key(shift(KeyCode::Right));
        }
        let selection = modal.selection();
        assert_eq!((selection.start_row, selection.end_row), (0, 0));
        assert_eq!((selection.start_col, selection.end_col), (0, 4));
        assert_eq!(modal.quote(selection), "line");
    }

    #[test]
    fn escape_clears_the_range_before_closing() {
        let mut modal = modal_with_rows(3);
        modal.handle_key(shift(KeyCode::Down));
        assert_eq!(modal.selected_rows(), (0, 1));

        assert!(matches!(
            modal.handle_key(press(KeyCode::Esc)),
            ReviewAction::Consumed
        ));
        assert_eq!(modal.selected_rows(), (1, 1));
        assert!(matches!(
            modal.handle_key(press(KeyCode::Esc)),
            ReviewAction::Close
        ));
    }

    #[test]
    fn a_drag_selects_across_rows_and_copies_on_release() {
        let mut modal = modal_with_rows(3);
        click(&mut modal, 0, 0);
        modal.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 5, 1));
        let action = modal.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 5, 1));
        assert!(matches!(action, ReviewAction::Copy(text) if text == "line 0\nline 1"));
    }

    #[test]
    fn a_click_that_selected_nothing_copies_nothing() {
        let mut modal = modal_with_rows(3);
        click(&mut modal, 2, 0);
        let action = modal.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 2, 0));
        assert!(matches!(action, ReviewAction::Consumed));
    }

    #[test]
    fn a_second_click_on_the_same_cell_takes_the_word() {
        let mut modal = modal_with_rows(3);
        click(&mut modal, 2, 0);
        click(&mut modal, 2, 0);
        assert_eq!(modal.quote(modal.selection()), "line");
    }

    #[test]
    fn copy_without_a_selection_is_left_to_the_app() {
        let mut modal = modal_with_rows(3);
        assert!(matches!(
            modal.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            ReviewAction::Passthrough
        ));
    }

    #[test]
    fn select_all_takes_the_whole_passage() {
        let mut modal = modal_with_rows(3);
        modal.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(modal.selected_rows(), (0, 2));
        assert!(matches!(
            modal.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            ReviewAction::Copy(text) if text == "line 0\nline 1\nline 2"
        ));
    }

    #[test]
    fn saving_a_note_returns_to_passage_mode() {
        let mut modal = modal_with_rows(3);
        modal.handle_key(press(KeyCode::Enter));
        assert!(matches!(modal.mode, Mode::Note { .. }));

        for character in "needs a caveat".chars() {
            modal.handle_key(press(KeyCode::Char(character)));
        }
        modal.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));

        assert!(matches!(modal.mode, Mode::Passage { .. }));
        assert_eq!(modal.notes_pending(), 1);
        assert_eq!(modal.notes[0].comment, "needs a caveat");
    }

    #[test]
    fn an_empty_comment_does_not_save() {
        let mut modal = modal_with_rows(3);
        modal.handle_key(press(KeyCode::Enter));
        modal.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(matches!(modal.mode, Mode::Note { .. }));
        assert_eq!(modal.notes_pending(), 0);
    }

    #[test]
    fn submitting_without_notes_is_inert() {
        let mut modal = modal_with_rows(3);
        assert!(matches!(
            modal.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            ReviewAction::Consumed
        ));
        assert!(modal.is_open());
    }

    #[test]
    fn submitting_clears_the_batch_and_closes() {
        let mut modal = modal_with_rows(3);
        modal.notes.push(note("a", "b", ASSISTANT_LABEL));
        let action = modal.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));

        assert!(matches!(action, ReviewAction::Submit(text) if text.contains("<review>")));
        assert_eq!(modal.notes_pending(), 0);
        assert!(!modal.is_open());
    }

    #[test]
    fn closing_keeps_notes_and_discarding_drops_them() {
        let mut modal = modal_with_rows(3);
        modal.notes.push(note("a", "b", ASSISTANT_LABEL));

        modal.close();
        assert_eq!(modal.notes_pending(), 1);

        modal.discard();
        assert_eq!(modal.notes_pending(), 0);
    }

    #[test]
    fn deleting_removes_the_note_under_the_cursor() {
        let mut modal = modal_with_rows(3);
        let source = modal.target.as_ref().expect("target").source;
        modal.notes.push(ReviewNote {
            source,
            label: ASSISTANT_LABEL,
            rows: (1, 2),
            width: 40,
            quote: "a".into(),
            comment: "b".into(),
        });

        modal.handle_key(press(KeyCode::Char('d')));
        assert_eq!(modal.notes_pending(), 1);

        modal.handle_key(press(KeyCode::Down));
        modal.handle_key(press(KeyCode::Char('d')));
        assert_eq!(modal.notes_pending(), 0);
    }

    #[test]
    fn quote_falls_back_to_scraping_without_provenance() {
        let mut modal = modal_with_rows(3);
        assert_eq!(modal.quote(modal.whole_rows(0, 1)), "line 0\nline 1");
        modal.handle_key(shift(KeyCode::Down));
        modal.handle_key(press(KeyCode::Enter));
        assert!(matches!(modal.mode, Mode::Note { ref quote, .. } if quote == "line 0\nline 1"));
    }

    #[test]
    fn editing_a_note_reuses_its_slot() {
        let mut modal = modal_with_rows(3);
        modal.handle_key(press(KeyCode::Enter));
        for character in "first".chars() {
            modal.handle_key(press(KeyCode::Char(character)));
        }
        modal.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));

        modal.handle_key(press(KeyCode::Char('e')));
        modal.handle_key(press(KeyCode::Char('!')));
        modal.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));

        assert_eq!(modal.notes_pending(), 1);
        assert_eq!(modal.notes[0].comment, "first!");
    }

    #[test]
    fn parses_back_what_it_compiled() {
        let notes = vec![
            note("first passage", "first comment", ASSISTANT_LABEL),
            note("{\"ms\": 12}", "second comment", TOOL_LABEL),
        ];
        let parsed = parse(&compile(&notes)).expect("round trip");

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].surface, None);
        assert_eq!(parsed[0].quote_text(), "first passage");
        assert_eq!(parsed[0].comment_text(), "first comment");
        assert_eq!(parsed[1].surface.as_deref(), Some(TOOL_LABEL));
        assert_eq!(parsed[1].quote_text(), "{\"ms\": 12}");
        assert_eq!(parsed[1].comment_text(), "second comment");
    }

    #[test]
    fn parses_a_multiline_quote_and_comment() {
        let compiled = compile(&[note(
            "- **Independent lap controls**\n- **Race-distance guidance**",
            "cool features\nship it",
            ASSISTANT_LABEL,
        )]);
        let parsed = parse(&compiled).expect("round trip");

        assert_eq!(
            parsed[0].quote_text(),
            "- **Independent lap controls**\n- **Race-distance guidance**"
        );
        assert_eq!(parsed[0].comment_text(), "cool features\nship it");
    }

    #[test]
    fn keeps_blank_lines_inside_a_quote() {
        let parsed =
            parse(&compile(&[note("a\n\nb", "why", ASSISTANT_LABEL)])).expect("round trip");
        assert_eq!(parsed[0].quote_text(), "a\n\nb");
    }

    #[test_case("plain user text" ; "prose")]
    #[test_case("<review>\nmissing preamble\n</review>" ; "wrong_preamble")]
    #[test_case("<review>\nAddress each note on my previous message.\n</review>" ; "no_notes")]
    #[test_case("<review>\nAddress each note on my previous message.\n\n<note>\n> quote\n</note>\n</review>" ; "no_comment")]
    fn leaves_everything_else_alone(text: &str) {
        assert!(parse(text).is_none());
    }

    #[test]
    fn card_shows_a_header_and_every_comment() {
        let notes = parse(&compile(&[
            note("a", "first comment", ASSISTANT_LABEL),
            note("b", "second comment", TOOL_LABEL),
        ]))
        .expect("round trip");
        let (lines, provenance, links) = card_lines(&notes, 40, Style::default());
        assert_eq!(lines.len(), provenance.len());
        assert!(links.is_aligned(&lines));
        let rendered = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.starts_with("Review · 2 notes"));
        assert!(rendered.contains("first comment"));
        assert!(rendered.contains("second comment"));
        assert!(rendered.contains(TOOL_LABEL));
        assert!(!rendered.contains(REVIEW_OPEN));
    }

    #[test]
    fn card_keeps_markdown_link_targets_aligned() {
        let notes = parse(&compile(&[note(
            "[docs](https://example.com)",
            "see [guide](https://example.com/guide)",
            ASSISTANT_LABEL,
        )]))
        .expect("round trip");
        let (lines, _, links) = card_lines(&notes, 40, Style::default());

        assert!(links.is_aligned(&lines));
        assert!(
            links
                .rows
                .iter()
                .flatten()
                .any(|target| target.as_deref() == Some("https://example.com/guide"))
        );
    }

    #[test]
    fn total_rows_counts_wrapped_lines() {
        let lines = vec![Line::raw("aaaaaaaa"), Line::raw("b"), Line::raw("")];
        assert_eq!(total_rows(&lines, 4), 4);
        assert_eq!(total_rows(&lines, 0), 0);
    }
}
