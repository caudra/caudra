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

use caudra_markdown::render::SpanSource;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use super::messages::{ASSISTANT_LABEL, ReviewTarget};
use super::modal::Modal;
use super::scrollbar::{Scrollbar, ScrollbarMouse};
use super::{DisplaySource, Overlay, hint_line};
use crate::markdown;
use crate::provenance::{LineProvenance, Provenance};
use crate::selection::{self, LineBreaks, ScreenSelection, line_chars, wrap_breaks};
use crate::text_buffer::TextBuffer;
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

enum Mode {
    Passage {
        cursor: u16,
        anchor: Option<u16>,
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
    Close,
}

pub(crate) struct ReviewModal {
    target: Option<Target>,
    notes: Vec<ReviewNote>,
    mode: Mode,
    buffer: TextBuffer,
    scroll: u16,
    scrollbar: Scrollbar,
    rows_total: u16,
    width: u16,
    content: Rect,
    popup: Rect,
}

impl ReviewModal {
    pub fn new() -> Self {
        Self {
            target: None,
            notes: Vec::new(),
            mode: Mode::Passage {
                cursor: 0,
                anchor: None,
            },
            buffer: TextBuffer::new(String::new()),
            scroll: 0,
            scrollbar: Scrollbar::default(),
            rows_total: 0,
            width: 0,
            content: Rect::default(),
            popup: Rect::default(),
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
            cursor: 0,
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
        self.buffer.insert_text(text);
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

    pub fn handle_mouse(&mut self, event: MouseEvent) -> ReviewAction {
        if self.target.is_none() {
            return ReviewAction::Passthrough;
        }
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
        let Mode::Passage { .. } = self.mode else {
            return ReviewAction::Passthrough;
        };
        let inside = self
            .content
            .contains(Position::new(event.column, event.row));
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if inside => {
                let row = self.row_at(event.row);
                self.mode = Mode::Passage {
                    cursor: row,
                    anchor: Some(row),
                };
                ReviewAction::Consumed
            }
            MouseEventKind::Drag(MouseButton::Left) if inside => {
                let row = self.row_at(event.row);
                if let Mode::Passage { cursor, .. } = &mut self.mode {
                    *cursor = row;
                }
                ReviewAction::Consumed
            }
            MouseEventKind::Up(MouseButton::Left) if inside => ReviewAction::Consumed,
            _ => ReviewAction::Passthrough,
        }
    }

    fn passage_key(&mut self, key: KeyEvent, cursor: u16, anchor: Option<u16>) -> ReviewAction {
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
            KeyCode::Enter => {
                let rows = ordered(cursor, anchor);
                let quote = self.quote(rows);
                if quote.trim().is_empty() {
                    return ReviewAction::Consumed;
                }
                self.buffer = TextBuffer::new(String::new());
                self.mode = Mode::Note {
                    rows,
                    quote,
                    editing: None,
                };
                return ReviewAction::Consumed;
            }
            KeyCode::Char('v') => {
                self.mode = Mode::Passage {
                    cursor,
                    anchor: anchor.xor(Some(cursor)),
                };
                return ReviewAction::Consumed;
            }
            KeyCode::Char('d') => {
                self.delete_note_at(cursor);
                return ReviewAction::Consumed;
            }
            KeyCode::Char('e') => {
                self.edit_note_at(cursor);
                return ReviewAction::Consumed;
            }
            KeyCode::Char('n') => return self.jump_note(cursor, true),
            KeyCode::Char('p') => return self.jump_note(cursor, false),
            _ => {}
        }

        let last = self.rows_total.saturating_sub(1);
        let moved = match key.code {
            KeyCode::Char('j') | KeyCode::Down => cursor.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => cursor.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => 0,
            KeyCode::Char('G') | KeyCode::End => last,
            KeyCode::PageDown => cursor.saturating_add(page),
            KeyCode::PageUp => cursor.saturating_sub(page),
            _ => return ReviewAction::Consumed,
        };
        self.set_cursor(moved.min(last), anchor);
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
        if key.code == KeyCode::Enter {
            self.buffer.add_line();
        } else {
            self.buffer.handle_key(key);
        }
        ReviewAction::Consumed
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
        let comment = self.buffer.value();
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
        let cursor = match self.mode {
            Mode::Note { rows, .. } => rows.0,
            Mode::Passage { cursor, .. } => cursor,
        };
        self.buffer.clear();
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
        self.buffer = TextBuffer::new(note.comment.clone());
        self.buffer.move_to_end();
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
            self.set_cursor(row, None);
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

    fn set_cursor(&mut self, cursor: u16, anchor: Option<u16>) {
        self.mode = Mode::Passage { cursor, anchor };
        self.follow_cursor(cursor);
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

    fn row_at(&self, screen_row: u16) -> u16 {
        let offset = screen_row.saturating_sub(self.content.y);
        (self.scroll + offset).min(self.rows_total.saturating_sub(1))
    }

    /// Markdown behind display rows `rows.0..=rows.1`, preferring the source
    /// text provenance recorded when the segment was painted.
    fn quote(&self, rows: (u16, u16)) -> String {
        let Some(target) = &self.target else {
            return String::new();
        };
        let width = self.width;
        if width == 0 || self.rows_total == 0 {
            return String::new();
        }
        let (start, end) = rows;
        let end = end.min(self.rows_total.saturating_sub(1));
        let sel = ScreenSelection {
            start_row: start,
            start_col: 0,
            end_row: end,
            end_col: width.saturating_sub(1),
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
        match self.mode {
            Mode::Passage { .. } => self.view_passage(frame, area),
            Mode::Note { .. } => self.view_note(frame, area),
        }
    }

    fn view_passage(&mut self, frame: &mut Frame, area: Rect) -> Rect {
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
        let cursor = self.cursor();
        self.follow_cursor(cursor.min(self.rows_total.saturating_sub(1)));

        self.render_meta(frame, meta_area);
        self.render_gutter(frame, gutter);
        self.render_passage(frame, text);
        frame.render_widget(
            Paragraph::new(hint_line(&[
                ("j/k", "move"),
                ("v", "select"),
                ("Enter", "note"),
                ("e/d", "edit/delete"),
                ("Ctrl+S", "send"),
                ("Esc", "close"),
            ])),
            hint_area,
        );
        popup
    }

    fn render_meta(&self, frame: &mut Frame, area: Rect) {
        let theme = theme::current();
        let notes = self.notes.len();
        let (start, end) = ordered(self.cursor(), self.anchor());
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
        let theme = theme::current();
        let (start, end) = ordered(self.cursor(), self.anchor());
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
        frame.render_widget(
            Paragraph::new(target.lines.clone())
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            area,
        );

        let (start, end) = ordered(self.cursor(), self.anchor());
        if start < self.scroll + area.height && end >= self.scroll {
            let top = area.y + start.saturating_sub(self.scroll);
            let bottom = area.y + (end - self.scroll).min(area.height.saturating_sub(1));
            let sel = ScreenSelection {
                start_row: top.max(area.y),
                start_col: area.x,
                end_row: bottom,
                end_col: area.right().saturating_sub(1),
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
        frame.render_widget(
            Paragraph::new(editor_lines(&self.buffer))
                .wrap(Wrap { trim: false })
                .style(Style::new().fg(theme.foreground)),
            editor_area,
        );
        frame.render_widget(
            Paragraph::new(hint_line(&[("Ctrl+S", "save"), ("Esc", "back")])),
            hint_area,
        );
        popup
    }

    fn target_lines(&self) -> &[Line<'static>] {
        self.target.as_ref().map_or(&[], |t| t.lines.as_slice())
    }

    fn cursor(&self) -> u16 {
        match self.mode {
            Mode::Passage { cursor, .. } => cursor,
            Mode::Note { rows, .. } => rows.0,
        }
    }

    fn anchor(&self) -> Option<u16> {
        match self.mode {
            Mode::Passage { anchor, .. } => anchor,
            Mode::Note { rows, .. } => Some(rows.1),
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
        self.buffer.clear();
        self.mode = Mode::Passage {
            cursor: 0,
            anchor: None,
        };
        self.scroll = 0;
        self.rows_total = 0;
        self.content = Rect::default();
        self.popup = Rect::default();
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

fn ordered(cursor: u16, anchor: Option<u16>) -> (u16, u16) {
    match anchor {
        Some(anchor) if anchor < cursor => (anchor, cursor),
        Some(anchor) => (cursor, anchor),
        None => (cursor, cursor),
    }
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

fn editor_lines(buffer: &TextBuffer) -> Vec<Line<'static>> {
    let theme = theme::current();
    buffer
        .lines()
        .iter()
        .enumerate()
        .map(|(y, line)| {
            if y != buffer.y() {
                return Line::raw(line.clone());
            }
            let byte = TextBuffer::char_to_byte(line, buffer.x());
            let (before, after) = line.split_at(byte);
            let mut chars = after.chars();
            let cursor = chars.next().unwrap_or(' ');
            Line::from(vec![
                Span::raw(before.to_owned()),
                Span::styled(cursor.to_string(), theme.cursor),
                Span::raw(chars.collect::<String>()),
            ])
        })
        .collect()
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

    #[test_case(3, None, (3, 3) ; "collapsed_range_is_the_cursor_row")]
    #[test_case(3, Some(7), (3, 7) ; "anchor_below_cursor")]
    #[test_case(7, Some(3), (3, 7) ; "anchor_above_cursor")]
    fn orders_the_row_range(cursor: u16, anchor: Option<u16>, expected: (u16, u16)) {
        assert_eq!(ordered(cursor, anchor), expected);
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

    #[test]
    fn cursor_stops_at_both_ends() {
        let mut modal = modal_with_rows(3);
        modal.handle_key(press(KeyCode::Char('k')));
        assert_eq!(modal.cursor(), 0);
        for _ in 0..5 {
            modal.handle_key(press(KeyCode::Char('j')));
        }
        assert_eq!(modal.cursor(), 2);
    }

    #[test]
    fn escape_clears_the_range_before_closing() {
        let mut modal = modal_with_rows(3);
        modal.handle_key(press(KeyCode::Char('v')));
        modal.handle_key(press(KeyCode::Char('j')));
        assert_eq!(ordered(modal.cursor(), modal.anchor()), (0, 1));

        assert!(matches!(
            modal.handle_key(press(KeyCode::Esc)),
            ReviewAction::Consumed
        ));
        assert_eq!(modal.anchor(), None);
        assert!(matches!(
            modal.handle_key(press(KeyCode::Esc)),
            ReviewAction::Close
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

        modal.handle_key(press(KeyCode::Char('j')));
        modal.handle_key(press(KeyCode::Char('d')));
        assert_eq!(modal.notes_pending(), 0);
    }

    #[test]
    fn quote_falls_back_to_scraping_without_provenance() {
        let mut modal = modal_with_rows(3);
        assert_eq!(modal.quote((0, 1)), "line 0\nline 1");
        modal.handle_key(press(KeyCode::Char('v')));
        modal.handle_key(press(KeyCode::Char('j')));
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
