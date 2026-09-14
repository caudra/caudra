//! Live view of the structured log file.
//!
//! The modal never materializes the whole file. [`LogTail`] keeps a window of
//! at most `viewport + 2 * overscan` entries, and this component owns a cursor
//! into that window, asking the tail for more only when the cursor nears an
//! edge. Following polls a process-global write counter first, so a quiet log
//! costs one atomic load per tick and no syscalls.

use std::ops::Range;
use std::path::Path;
use std::time::Duration;

use caudra_storage::log::record::{Entry, Filter, Level, Record};
use caudra_storage::log::tail::{LogTail, ScanOutcome};
use caudra_storage::log::{self, DEFAULT_MAX_FILES};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::components::keybindings::key;
use crate::components::modal::Modal;
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::{
    Overlay, bar_area, escape_terminal_controls, hint_line, hover_style, input_line_with_cursor,
    is_ctrl,
};
use crate::repaint::{Cadence, Dirty};
use crate::selection::wrap_breaks;
use crate::text_buffer::TextBuffer;
use crate::theme::Theme;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) const TITLE: &str = " Logs ";
const WIDTH_PERCENT: u16 = 92;
const MAX_HEIGHT_PERCENT: u16 = 88;
const H_PAD: u16 = 1;

/// How often a following modal looks for new records.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Extra entries kept on each side of the viewport so ordinary scrolling never
/// touches the filesystem.
const MIN_OVERSCAN: usize = 64;

const LEVEL_WIDTH: usize = 5;
const COLUMN_GAP: &str = "  ";
const FIELD_GAP: &str = " ";
const KV_SEPARATOR: &str = "=";
const EXPAND_INDENT: &str = "    ";
const EXPAND_ARROW: &str = "> ";
const RAW_JSON_LABEL: &str = "raw";
const LEVEL_PREFIX: &str = ">=";

const NO_TAIL: &str = "No log file yet.";
const NO_TAIL_HINT: &str = "One appears once caudra writes its first record.";
const NO_MATCHES: &str = "No records match the current filter.";
const SCAN_LIMIT_NOTE: &str = "scan limit reached, keep scrolling";
const OLDEST_NOTE: &str = "oldest retained record";
const FOLLOWING: &str = "following";
const PAUSED: &str = "paused";
const COPIED: &str = "Copied log line";
const COPIED_RAW: &str = "Copied raw record";

/// Span fields worth carrying on the one-line view.
const CORRELATION_FIELDS: &[&str] = &["session_id", "turn_id", "request_id", "tool_use_id"];
/// Most specific first. `turn_id` is absent on purpose: it counts from one per
/// session, so filtering on it alone would pull in every session's fourth turn.
const FOCUS_FIELDS: &[&str] = &["tool_use_id", "request_id", "session_id"];

/// The search row and the hint row, which are always drawn. The status row is
/// part of the footer that reports the file.
const CHROME_ROWS: u16 = 2;
const HINT_KEY_MOVE: &str = "\u{2191}\u{2193}";
const HINT_KEY_ENTER: &str = "enter";
const HINT_KEY_ESC: &str = "esc";
const HINT_MOVE: &str = "select";
const HINT_EXPAND: &str = "expand";
const HINT_FOCUS: &str = "only this id";
const HINT_FILTER: &str = "filter";
const HINT_LEVEL: &str = "level";
const HINT_COPY: &str = "copy / raw";
const HINT_CLOSE: &str = "close";
const HINT_APPLY: &str = "done";
const HINT_CLEAR: &str = "clear";
const HINT_KEY_PAN: &str = "\u{2190}\u{2192}";
const HINT_KEY_FOCUS: &str = "tab";
const HINT_KEY_FILTER: &str = "/";
const HINT_PAN: &str = "pan";
const HINT_WRAP: &str = "wrap";
const HINT_COLLAPSE: &str = "collapse";
const HINT_MOVE_KEYS: (&str, &str) = (HINT_KEY_MOVE, HINT_MOVE);
const HINT_TAIL: [(&str, &str); 4] = [
    ("l", HINT_LEVEL),
    ("w", HINT_WRAP),
    ("y/Y", HINT_COPY),
    (HINT_KEY_ESC, HINT_CLOSE),
];

const NO_FOCUS: &str = "That record carries no id to filter on";
const WRAPPED: &str = "wrapped";
const PAN_MARK: &str = "col ";
/// Log lines are wide, so panning a character at a time would take all day.
const PAN_STEP: isize = 8;

pub enum LogsAction {
    Consumed,
    Close,
    Flash(&'static str),
    Copy { text: String, label: &'static str },
}

pub struct LogsModal {
    open: bool,
    max_files: u32,
    tail: Option<LogTail>,
    follow: bool,
    filter: Filter,
    query: TextBuffer,
    query_focused: bool,
    /// Index into the tail window, not into the file.
    selected: usize,
    view_top: usize,
    expanded: bool,
    wrap: bool,
    /// Display columns scrolled off the left. Always zero while wrapping,
    /// since a wrapped row has nothing past the right margin to reach.
    pan: usize,
    max_pan: usize,
    outcome: ScanOutcome,
    last_sequence: u64,
    viewport_h: usize,
    popup: Rect,
    body: Rect,
    /// Window index per body row, rebuilt each frame.
    rows: Vec<usize>,
    /// Where the level chip sits, so a click can cycle it and a move can light
    /// it up. Recomputed each frame because the footer reflows with the path.
    level_hit: Rect,
    level_hovered: bool,
    scrollbar: Scrollbar,
    pan_bar: Scrollbar,
}

impl LogsModal {
    pub fn new(max_log_files: u32) -> Self {
        Self {
            open: false,
            max_files: max_files_or_default(max_log_files),
            tail: None,
            follow: true,
            filter: Filter::default(),
            query: TextBuffer::new(String::new()),
            query_focused: false,
            selected: 0,
            view_top: 0,
            expanded: false,
            wrap: false,
            pan: 0,
            max_pan: 0,
            outcome: ScanOutcome::Filled,
            last_sequence: 0,
            viewport_h: 0,
            popup: Rect::default(),
            body: Rect::default(),
            rows: Vec::new(),
            level_hit: Rect::default(),
            level_hovered: false,
            scrollbar: Scrollbar::default(),
            pan_bar: Scrollbar::horizontal(),
        }
    }

    pub fn open(&mut self) {
        match log::tail_dir() {
            Some(dir) => self.open_dir(&dir),
            None => {
                self.reset();
                self.tail = None;
            }
        }
    }

    fn open_dir(&mut self, dir: &Path) {
        self.reset();
        self.tail = LogTail::open(dir, self.max_files, self.capacity()).ok();
        self.reload();
    }

    fn reset(&mut self) {
        self.open = true;
        self.follow = true;
        self.expanded = false;
        self.query.clear();
        self.query_focused = false;
        self.pan = 0;
        self.filter = Filter::default();
    }

    pub fn close(&mut self) {
        self.open = false;
        // The window and its open file handle are the expensive part, and a
        // closed modal must not hold either.
        self.tail = None;
        self.popup = Rect::default();
        self.body = Rect::default();
        self.rows = Vec::new();
        self.level_hit = Rect::default();
        self.level_hovered = false;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    fn capacity(&self) -> usize {
        self.viewport_h.max(MIN_OVERSCAN) * 2 + self.viewport_h
    }

    fn window_len(&self) -> usize {
        self.tail.as_ref().map_or(0, |tail| tail.window().len())
    }

    /// Re-anchors at the newest matching record. Used on open and whenever the
    /// filter changes, since the old window was built under a different one.
    fn reload(&mut self) {
        let filter = self.filter.clone();
        if let Some(tail) = &mut self.tail
            && let Ok(outcome) = tail.tail(&filter)
        {
            self.outcome = outcome;
        }
        self.follow = true;
        self.expanded = false;
        self.last_sequence = log::write_sequence();
        self.snap_to_end();
    }

    fn snap_to_end(&mut self) {
        let len = self.window_len();
        self.selected = len.saturating_sub(1);
        self.view_top = len.saturating_sub(self.viewport_h.max(1));
    }

    /// Returns whether anything changed, so `App::tick` can decide to repaint.
    pub fn poll(&mut self) -> Dirty {
        if !self.open || !self.follow {
            return Dirty::NO;
        }
        let sequence = log::write_sequence();
        if sequence == self.last_sequence {
            return Dirty::NO;
        }
        self.last_sequence = sequence;
        let filter = self.filter.clone();
        let added = self
            .tail
            .as_mut()
            .and_then(|tail| tail.poll(&filter).ok())
            .unwrap_or(0);
        if added == 0 {
            return Dirty::NO;
        }
        self.snap_to_end();
        Dirty::YES
    }

    pub fn handle_key(&mut self, event: KeyEvent) -> LogsAction {
        if self.query_focused {
            return self.handle_query_key(event);
        }
        match event.code {
            KeyCode::Esc => return LogsAction::Close,
            KeyCode::Char('q') if !is_ctrl(&event) => return LogsAction::Close,
            _ if key::QUIT.matches(event) => return LogsAction::Close,
            KeyCode::Char('/') => {
                self.query_focused = true;
                return LogsAction::Consumed;
            }
            KeyCode::Char('f') => self.follow = !self.follow,
            KeyCode::Char('l') => self.cycle_level(),
            KeyCode::Char('w') => self.toggle_wrap(),
            KeyCode::Left => self.pan(-PAN_STEP),
            KeyCode::Right => self.pan(PAN_STEP),
            KeyCode::Home => self.pan = 0,
            KeyCode::Tab => return self.focus_correlation(),
            KeyCode::Enter | KeyCode::Char(' ') => self.expanded = !self.expanded,
            KeyCode::Char('y') => return self.copy(false),
            KeyCode::Char('Y') => return self.copy(true),
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-self.half_page()),
            KeyCode::PageDown => self.move_cursor(self.half_page()),
            _ if key::SCROLL_HALF_UP.matches(event) => self.move_cursor(-self.half_page()),
            _ if key::SCROLL_LINE_UP.matches(event) => self.move_cursor(-1),
            _ if key::SCROLL_LINE_DOWN.matches(event) => self.move_cursor(1),
            _ if key::SCROLL_TOP.matches(event) => self.move_cursor(-(MIN_OVERSCAN as isize)),
            _ if key::SCROLL_BOTTOM.matches(event) || key::DOC_BOTTOM.matches(event) => {
                self.reload();
            }
            _ => {}
        }
        LogsAction::Consumed
    }

    fn handle_query_key(&mut self, event: KeyEvent) -> LogsAction {
        match event.code {
            KeyCode::Esc => {
                self.query_focused = false;
                if !self.query.value().is_empty() {
                    self.query.clear();
                    self.apply_query();
                }
            }
            KeyCode::Enter => self.query_focused = false,
            _ => {
                let before = self.query.value().to_owned();
                self.query.handle_key(event);
                if self.query.value() != before {
                    self.apply_query();
                }
            }
        }
        LogsAction::Consumed
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.query_focused {
            self.query.insert_text(text);
            self.apply_query();
        }
    }

    fn apply_query(&mut self) {
        self.filter = Filter::new(self.filter.min_level, &self.query.value());
        self.reload();
    }

    /// Narrows to everything sharing the selected record's most specific id.
    /// A turn or a tool call spans many records, and reading it any other way
    /// means eyeballing an id across a scrolling file.
    fn focus_correlation(&mut self) -> LogsAction {
        let Some(id) = self.current().and_then(|line| correlation_id(&line.entry)) else {
            return LogsAction::Flash(NO_FOCUS);
        };
        self.query = TextBuffer::new(id);
        self.query_focused = false;
        self.apply_query();
        LogsAction::Consumed
    }

    /// Wrapping and panning are two answers to the same question, so turning
    /// one on puts the other back at the left margin.
    fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
        self.pan = 0;
    }

    fn pan(&mut self, delta: isize) {
        if self.wrap {
            return;
        }
        self.pan = self.pan.saturating_add_signed(delta).min(self.max_pan);
    }

    fn cycle_level(&mut self) {
        self.filter = Filter::new(self.filter.min_level.next(), &self.query.value());
        self.reload();
    }

    fn copy(&self, raw: bool) -> LogsAction {
        let Some(line) = self.current() else {
            return LogsAction::Consumed;
        };
        if raw {
            LogsAction::Copy {
                text: line.raw.clone(),
                label: COPIED_RAW,
            }
        } else {
            LogsAction::Copy {
                text: plain_text(&line.entry),
                label: COPIED,
            }
        }
    }

    fn current(&self) -> Option<&caudra_storage::log::tail::Line> {
        self.tail.as_ref()?.window().get(self.selected)
    }

    fn half_page(&self) -> isize {
        (self.viewport_h / 2).max(1) as isize
    }

    /// The wheel moves the page. Dragging the selection along would make the
    /// view jump back the moment the cursor left the viewport, which is the
    /// opposite of what a wheel is for.
    pub fn scroll(&mut self, delta: i32) {
        self.scroll_view(-delta as isize);
    }

    fn max_view_top(&self) -> usize {
        self.window_len().saturating_sub(self.viewport_h.max(1))
    }

    fn scroll_view(&mut self, delta: isize) {
        if delta == 0 {
            return;
        }
        self.expanded = false;
        if delta < 0 {
            let want = delta.unsigned_abs();
            if self.view_top < want {
                self.extend_back(want.max(MIN_OVERSCAN));
            }
            self.view_top = self.view_top.saturating_sub(want);
            self.follow = false;
        } else {
            let want = delta as usize;
            if self.view_top + want >= self.max_view_top() {
                self.extend_forward();
            }
            self.view_top = (self.view_top + want).min(self.max_view_top());
            self.follow = self.view_top >= self.max_view_top() && self.at_end();
        }
        self.clamp_selection();
    }

    /// The bar names the row it wants at the top; the window only knows how to
    /// travel, since a distance is what decides whether it has to read further
    /// back in the file.
    fn scroll_view_to(&mut self, top: usize) {
        self.scroll_view(top as isize - self.view_top as isize);
    }

    /// Keeps the cursor on a visible row, so copying or expanding after a
    /// wheel acts on something the reader can see.
    fn clamp_selection(&mut self) {
        let last = self.window_len().saturating_sub(1);
        let bottom = (self.view_top + self.viewport_h.max(1) - 1).min(last);
        self.selected = self.selected.clamp(self.view_top.min(bottom), bottom);
    }

    /// Positive moves toward newer records. Extending the window is the only
    /// place that reads the file, and only when the cursor reaches an edge.
    fn move_cursor(&mut self, delta: isize) {
        if delta == 0 {
            return;
        }
        self.expanded = false;
        if delta < 0 {
            let want = delta.unsigned_abs();
            if self.selected < want {
                let deficit = want - self.selected;
                self.extend_back(deficit.max(MIN_OVERSCAN));
            }
            self.selected = self.selected.saturating_sub(want);
            self.follow = false;
        } else {
            let want = delta as usize;
            let len = self.window_len();
            if self.selected + want >= len.saturating_sub(1) {
                self.extend_forward();
            }
            self.selected = (self.selected + want).min(self.window_len().saturating_sub(1));
            self.follow = self.selected + 1 >= self.window_len() && self.at_end();
        }
    }

    fn at_end(&self) -> bool {
        self.tail.as_ref().is_some_and(LogTail::at_end)
    }

    fn extend_back(&mut self, want: usize) {
        let filter = self.filter.clone();
        let Some(tail) = &mut self.tail else {
            return;
        };
        let Ok(advance) = tail.scroll_back(want, &filter) else {
            return;
        };
        self.outcome = advance.outcome;
        self.selected += advance.added;
        self.view_top += advance.added;
        let len = tail.window().len();
        self.selected = self.selected.min(len.saturating_sub(1));
        self.view_top = self.view_top.min(len.saturating_sub(1));
    }

    fn extend_forward(&mut self) {
        let filter = self.filter.clone();
        let Some(tail) = &mut self.tail else {
            return;
        };
        let before = tail.window().len();
        let Ok(added) = tail.scroll_forward(MIN_OVERSCAN, &filter) else {
            return;
        };
        // A full window evicts from the front by however much it grew past
        // capacity, which shifts every index the cursor holds.
        let evicted = (before + added).saturating_sub(tail.window().len());
        self.selected = self.selected.saturating_sub(evicted);
        self.view_top = self.view_top.saturating_sub(evicted);
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> LogsAction {
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return LogsAction::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll_view_to(top as usize);
                return LogsAction::Consumed;
            }
        }
        let pos = Position::new(event.column, event.row);
        // Under touch the bar's hit margin reaches up off its border row and over
        // the footer, so the level chip is asked first for the cells it drew on.
        if !self.level_hit.contains(pos) {
            match self.pan_bar.handle(&event) {
                ScrollbarMouse::Ignored => {}
                ScrollbarMouse::Consumed => return LogsAction::Consumed,
                ScrollbarMouse::ScrollTo(column) => {
                    self.pan = (column as usize).min(self.max_pan);
                    return LogsAction::Consumed;
                }
            }
        }
        match event.kind {
            MouseEventKind::ScrollUp => self.scroll_view(-1),
            MouseEventKind::ScrollDown => self.scroll_view(1),
            MouseEventKind::ScrollLeft => self.pan(-PAN_STEP),
            MouseEventKind::ScrollRight => self.pan(PAN_STEP),
            // Hover is read on the next frame, so it is recorded even where the
            // move itself is nothing the modal needs to act on.
            MouseEventKind::Moved => self.level_hovered = self.level_hit.contains(pos),
            MouseEventKind::Down(MouseButton::Left) if self.level_hit.contains(pos) => {
                self.cycle_level();
            }
            MouseEventKind::Down(MouseButton::Left) if self.body.contains(pos) => {
                if let Some(&index) = self.rows.get(usize::from(event.row - self.body.y)) {
                    self.selected = index;
                    self.follow = false;
                }
            }
            _ => {}
        }
        LogsAction::Consumed
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) -> Rect {
        if !self.open {
            return Rect::default();
        }
        let modal = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, area.height);
        let padded = Rect {
            x: inner.x.saturating_add(H_PAD),
            width: inner.width.saturating_sub(H_PAD.saturating_mul(2)),
            ..inner
        };
        let searching = self.query_focused || !self.query.value().is_empty();
        let chrome = CHROME_ROWS + u16::from(searching);
        let body_height = padded.height.saturating_sub(chrome);
        self.resize(usize::from(body_height));

        self.body = Rect {
            height: body_height,
            ..padded
        };
        let width = usize::from(padded.width);
        self.anchor(width, theme);
        let widest = self.widest_row(theme);
        self.max_pan = widest.saturating_sub(width);
        self.pan = self.pan.min(self.max_pan);
        let (lines, rows) = self.body_lines(width, theme);
        self.rows = rows;
        frame.render_widget(
            Paragraph::new(lines).style(Style::new().fg(theme.foreground)),
            self.body,
        );

        let mut row = padded.y.saturating_add(body_height);
        let mut next_row = || {
            let area = Rect {
                y: row,
                height: 1,
                ..padded
            };
            row = row.saturating_add(1);
            area
        };
        if searching {
            frame.render_widget(
                Paragraph::new(input_line_with_cursor(&self.query)),
                next_row(),
            );
        }
        frame.render_widget(Paragraph::new(hint_line(&self.hints())), next_row());
        let footer = next_row();
        let (line, level) = self.footer_line(theme);
        self.level_hit = hit_rect(footer, level);
        frame.render_widget(Paragraph::new(line), footer);
        let len = u16::try_from(self.window_len()).unwrap_or(u16::MAX);
        let offset = u16::try_from(self.view_top).unwrap_or(u16::MAX);
        // Against the body, not the whole modal: the search, hint, and status
        // rows do not scroll and must not wear a track.
        self.scrollbar.draw(
            frame,
            Rect {
                height: body_height,
                ..inner
            },
            len,
            offset,
        );
        // The bottom border row, the same place every panning modal puts it, and
        // the rows over it for a fingertip to aim at. A wrapped pane has nothing
        // off screen, so it reports no width to pan.
        let pannable = match self.wrap {
            true => 0,
            false => u16::try_from(widest).unwrap_or(u16::MAX),
        };
        self.pan_bar.draw(
            frame,
            bar_area(inner),
            pannable,
            u16::try_from(self.pan).unwrap_or(u16::MAX),
        );

        self.popup = popup;
        popup
    }

    /// Record indices cannot say where the top of the pane goes once a record
    /// can be several rows tall, so the anchor is measured every frame against
    /// the rows the pane will actually paint.
    fn anchor(&mut self, width: usize, theme: &Theme) {
        let len = self.window_len();
        if len == 0 || self.viewport_h == 0 {
            return;
        }
        let last = match self.follow {
            true => len - 1,
            false => self.selected.min(len - 1),
        };
        if !self.follow && self.selected < self.view_top {
            self.view_top = self.selected;
            return;
        }
        let top = self.top_for(last, width, theme);
        if self.follow || top > self.view_top {
            self.view_top = top;
        }
    }

    /// A resize changes how much the window has to hold, and following has to
    /// stay pinned to the newest row across it.
    fn resize(&mut self, body_height: usize) {
        if self.viewport_h == body_height {
            return;
        }
        self.viewport_h = body_height;
        let capacity = self.capacity();
        if let Some(tail) = &mut self.tail {
            tail.set_capacity(capacity);
        }
        if self.follow {
            self.snap_to_end();
        }
    }

    /// One record before the pane is applied: the summary row, plus its
    /// expansion when it is the selected record.
    fn logical_lines(&self, index: usize, theme: &Theme) -> Vec<Line<'static>> {
        let Some(line) = self.tail.as_ref().and_then(|t| t.window().get(index)) else {
            return Vec::new();
        };
        let selected = index == self.selected;
        let mut out = vec![entry_line(&line.entry, selected, theme)];
        if selected && self.expanded {
            out.extend(expansion_lines(line, theme));
        }
        out
    }

    /// Every row one record paints. Measuring and drawing both go through this,
    /// so the two can never disagree about how tall a wrapped record is.
    fn record_rows(&self, index: usize, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        self.logical_lines(index, theme)
            .iter()
            .flat_map(|line| lay_out(line, width, self.pan, self.wrap))
            .collect()
    }

    /// The widest row on screen, measured before panning, which is how far
    /// panning is allowed to go. Without it the arrows would walk a short page
    /// off into blank columns with no way to tell how far back to come.
    fn widest_row(&self, theme: &Theme) -> usize {
        (self.view_top..self.window_len())
            .take(self.viewport_h.max(1))
            .flat_map(|index| self.logical_lines(index, theme))
            .map(|line| display_width(&line))
            .max()
            .unwrap_or(0)
    }

    /// The topmost record that still leaves `last` on screen. Wrapping makes a
    /// record several rows tall, so this has to be measured rather than counted.
    fn top_for(&self, last: usize, width: usize, theme: &Theme) -> usize {
        let mut used = 0;
        let mut top = last;
        for index in (0..=last).rev() {
            let height = self.record_rows(index, width, theme).len().max(1);
            if used + height > self.viewport_h && used > 0 {
                break;
            }
            used += height;
            top = index;
        }
        top
    }

    /// Also reports the window index behind each screen row, so a click lands
    /// on the record the reader pointed at even when an expansion pushed the
    /// rows below it down.
    fn body_lines(&self, width: usize, theme: &Theme) -> (Vec<Line<'static>>, Vec<usize>) {
        let Some(tail) = &self.tail else {
            return (
                vec![
                    Line::from(Span::styled(NO_TAIL, theme.status_dim)),
                    Line::from(Span::styled(NO_TAIL_HINT, theme.tool_dim)),
                ],
                Vec::new(),
            );
        };
        if tail.window().is_empty() {
            return (
                vec![Line::from(Span::styled(NO_MATCHES, theme.status_dim))],
                Vec::new(),
            );
        }

        let mut lines = Vec::with_capacity(self.viewport_h);
        let mut rows = Vec::with_capacity(self.viewport_h);
        for index in self.view_top..self.window_len() {
            if lines.len() >= self.viewport_h {
                break;
            }
            for row in self.record_rows(index, width, theme) {
                lines.push(row);
                rows.push(index);
            }
        }
        lines.truncate(self.viewport_h);
        rows.truncate(self.viewport_h);
        (lines, rows)
    }

    /// The actions a reader can reach from here. Without this row the only way
    /// to learn that a record expands or that a turn can be isolated is to open
    /// the keybinding reference.
    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.query_focused {
            return vec![(HINT_KEY_ENTER, HINT_APPLY), (HINT_KEY_ESC, HINT_CLEAR)];
        }
        let mut out = vec![HINT_MOVE_KEYS];
        // Panning is offered only where it can do something, so a wrapped or a
        // narrow pane does not advertise a key that would be ignored.
        if self.max_pan > 0 && !self.wrap {
            out.push((HINT_KEY_PAN, HINT_PAN));
        }
        out.push((
            HINT_KEY_ENTER,
            match self.expanded {
                true => HINT_COLLAPSE,
                false => HINT_EXPAND,
            },
        ));
        out.push((HINT_KEY_FOCUS, HINT_FOCUS));
        out.push((HINT_KEY_FILTER, HINT_FILTER));
        out.extend(HINT_TAIL);
        out
    }

    /// Also reports the level chip's column range, which is what keeps the
    /// footer and the hit test from drifting apart.
    fn footer_line(&self, theme: &Theme) -> (Line<'static>, Range<usize>) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut push = |text: String, style: Style| {
            if !spans.is_empty() {
                spans.push(Span::styled(COLUMN_GAP.to_owned(), theme.tool_dim));
            }
            let start: usize = spans.iter().map(|s| s.content.chars().count()).sum();
            let end = start + text.chars().count();
            spans.push(Span::styled(text, style));
            start..end
        };

        match &self.tail {
            Some(tail) => {
                push(
                    escape_terminal_controls(&tail.path().display().to_string()),
                    theme.tool_path,
                );
                push(format_bytes(tail.size()), theme.tool_dim);
            }
            None => {
                push(NO_TAIL.to_owned(), theme.status_dim);
            }
        }
        let level = push(
            format!("{LEVEL_PREFIX}{}", self.filter.min_level),
            hover_style(
                level_style(self.filter.min_level, theme),
                self.level_hovered,
            ),
        );
        push(
            if self.follow { FOLLOWING } else { PAUSED }.to_owned(),
            if self.follow {
                theme.tool_success
            } else {
                theme.status_dim
            },
        );
        if self.wrap {
            push(WRAPPED.to_owned(), theme.tool_annotation);
        } else if self.pan > 0 {
            push(format!("{PAN_MARK}{}", self.pan), theme.tool_annotation);
        }
        if self.outcome == ScanOutcome::ScanLimit {
            push(SCAN_LIMIT_NOTE.to_owned(), theme.tool_warning);
        } else if self.outcome == ScanOutcome::Exhausted && self.view_top == 0 {
            push(OLDEST_NOTE.to_owned(), theme.tool_dim);
        }
        (Line::from(spans), level)
    }
}

/// A footer chip clipped to the row it was drawn on. A range past the right
/// edge collapses to zero width, so it can never be clicked.
fn hit_rect(row: Rect, columns: Range<usize>) -> Rect {
    let start = u16::try_from(columns.start)
        .unwrap_or(u16::MAX)
        .min(row.width);
    let end = u16::try_from(columns.end)
        .unwrap_or(u16::MAX)
        .min(row.width);
    Rect {
        x: row.x + start,
        y: row.y,
        width: end - start,
        height: row.height,
    }
}

impl Default for LogsModal {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FILES)
    }
}

impl Overlay for LogsModal {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        self.close();
    }

    fn cadence(&self) -> Cadence {
        Cadence::when(self.open && self.follow, Cadence::polling(POLL_INTERVAL))
    }
}

/// The spans covering `range` of the line's concatenated characters, styles
/// intact, so a field value cut by a wrap or a pan keeps its colour.
fn slice_spans(spans: &[Span<'static>], range: Range<usize>) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut at = 0;
    for span in spans {
        let len = span.content.chars().count();
        let start = range.start.saturating_sub(at).min(len);
        let end = range.end.saturating_sub(at).min(len);
        if start < end {
            let text: String = span.content.chars().skip(start).take(end - start).collect();
            out.push(Span::styled(text, span.style));
        }
        at += len;
        if at >= range.end {
            break;
        }
    }
    out
}

/// The first character at or past display column `column`. Panning counts
/// columns rather than characters so a wide glyph cannot shear a line.
fn char_at_column(chars: &[char], column: usize) -> usize {
    let mut used = 0;
    for (index, ch) in chars.iter().enumerate() {
        if used >= column {
            return index;
        }
        used += ch.width().unwrap_or(0);
    }
    chars.len()
}

fn display_width(line: &Line<'static>) -> usize {
    line.spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// One logical record laid out for the pane: broken onto as many rows as it
/// needs, or kept on one row panned `pan` columns to the right. Wrapping uses
/// the shared break walk, so these rows fall where every other wrapped surface
/// in the UI puts them.
fn lay_out(line: &Line<'static>, width: usize, pan: usize, wrap: bool) -> Vec<Line<'static>> {
    if !wrap {
        if pan == 0 {
            return vec![line.clone()];
        }
        let chars: Vec<char> = line.spans.iter().flat_map(|s| s.content.chars()).collect();
        let start = char_at_column(&chars, pan);
        return vec![Line::from(slice_spans(&line.spans, start..chars.len()))];
    }

    let chars: Vec<char> = line.spans.iter().flat_map(|s| s.content.chars()).collect();
    let mut starts = vec![0];
    starts.extend(
        wrap_breaks(&chars, u16::try_from(width).unwrap_or(u16::MAX).max(1))
            .into_iter()
            .map(|brk| brk.start),
    );
    starts
        .iter()
        .enumerate()
        .map(|(row, &start)| {
            let end = starts.get(row + 1).copied().unwrap_or(chars.len());
            Line::from(slice_spans(&line.spans, start..end))
        })
        .collect()
}

fn level_style(level: Level, theme: &Theme) -> Style {
    match level {
        Level::Error => theme.error,
        Level::Warn => theme.tool_warning,
        Level::Info => theme.tool_success,
        Level::Debug => theme.status_dim,
        Level::Trace => theme.tool_dim,
    }
}

/// Everything rendered here can carry text the model, a tool, or an MCP server
/// produced, so nothing reaches the terminal unescaped.
fn entry_line(entry: &Entry, selected: bool, theme: &Theme) -> Line<'static> {
    let line = match entry {
        Entry::Raw(raw) => Line::from(Span::styled(escape_terminal_controls(raw), theme.tool_dim)),
        Entry::Record(record) => record_line(record, theme),
    };
    if selected {
        line.style(theme.item_selected)
    } else {
        line
    }
}

/// Every field, however wide that runs. What does not fit is reached by
/// wrapping or panning rather than being dropped, so no record can hide a field
/// the reader has no way to ask for.
fn record_line(record: &Record, theme: &Theme) -> Line<'static> {
    let mut spans = vec![
        Span::styled(record.time_of_day().to_owned(), theme.timestamp),
        Span::raw(COLUMN_GAP),
        Span::styled(
            format!("{:LEVEL_WIDTH$}", record.level.as_str()),
            level_style(record.level, theme),
        ),
        Span::raw(COLUMN_GAP),
        Span::styled(escape_terminal_controls(&record.target), theme.tool_dim),
        Span::raw(COLUMN_GAP),
        Span::styled(
            escape_terminal_controls(&record.message),
            Style::new().fg(theme.foreground),
        ),
    ];

    for (key, value) in correlation_fields(record) {
        push_field(&mut spans, &key, &value, theme.tool_dim, theme.accent);
    }
    for (key, value) in &record.fields {
        push_field(&mut spans, key, value, theme.tool_annotation, theme.accent);
    }
    Line::from(spans)
}

/// The narrowest id the record carries. Span fields are searched first because
/// a tool call nests inside the turn that made it.
fn correlation_id(entry: &Entry) -> Option<String> {
    let Entry::Record(record) = entry else {
        return None;
    };
    FOCUS_FIELDS.iter().find_map(|wanted| {
        record
            .spans
            .iter()
            .flat_map(|span| span.fields.iter())
            .chain(record.fields.iter())
            .find(|(key, _)| key == wanted)
            .map(|(_, value)| value.clone())
    })
}

/// Correlation lives on the spans, so it is worth a place on the one-line view
/// even though it is not an event field.
fn correlation_fields(record: &Record) -> Vec<(String, String)> {
    record
        .spans
        .iter()
        .flat_map(|span| span.fields.iter())
        .filter(|(key, _)| CORRELATION_FIELDS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn push_field(
    spans: &mut Vec<Span<'static>>,
    key: &str,
    value: &str,
    key_style: Style,
    value_style: Style,
) {
    spans.push(Span::raw(FIELD_GAP));
    spans.push(Span::styled(escape_terminal_controls(key), key_style));
    spans.push(Span::styled(KV_SEPARATOR.to_owned(), key_style));
    spans.push(Span::styled(escape_terminal_controls(value), value_style));
}

fn expansion_lines(line: &caudra_storage::log::tail::Line, theme: &Theme) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if let Entry::Record(record) = &line.entry {
        out.push(detail(RECORD_TIME, &record.timestamp, theme));
        for span in &record.spans {
            for (key, value) in &span.fields {
                out.push(detail(&format!("{}.{key}", span.name), value, theme));
            }
        }
        for (key, value) in &record.fields {
            out.push(detail(key, value, theme));
        }
    }
    out.push(detail(RAW_JSON_LABEL, &line.raw, theme));
    out
}

const RECORD_TIME: &str = "timestamp";

fn detail(key: &str, value: &str, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(EXPAND_INDENT.to_owned(), theme.tool_dim),
        Span::styled(EXPAND_ARROW.to_owned(), theme.tool_dim),
        Span::styled(escape_terminal_controls(key), theme.tool_annotation),
        Span::styled(KV_SEPARATOR.to_owned(), theme.tool_dim),
        Span::styled(escape_terminal_controls(value), theme.accent),
    ])
}

/// What a copy of the rendered line should contain: the same information, with
/// no styling and no width cap.
fn plain_text(entry: &Entry) -> String {
    match entry {
        Entry::Raw(raw) => raw.clone(),
        Entry::Record(record) => {
            let mut out = format!(
                "{} {:LEVEL_WIDTH$} {} {}",
                record.timestamp, record.level, record.target, record.message
            );
            for (key, value) in record
                .spans
                .iter()
                .flat_map(|span| span.fields.iter())
                .chain(record.fields.iter())
            {
                out.push_str(&format!("{FIELD_GAP}{key}{KV_SEPARATOR}{value}"));
            }
            out
        }
    }
}

const BYTE_UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
const BYTE_STEP: u64 = 1024;

fn format_bytes(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= BYTE_STEP as f64 && unit + 1 < BYTE_UNITS.len() {
        value /= BYTE_STEP as f64;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", BYTE_UNITS[0])
    } else {
        format!("{value:.1} {}", BYTE_UNITS[unit])
    }
}

/// The modal opens against the configured retention, so a user who keeps twenty
/// files can scroll into all twenty.
pub(crate) fn max_files_or_default(configured: u32) -> u32 {
    if configured == 0 {
        DEFAULT_MAX_FILES
    } else {
        configured
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_workbench::scroll::SCROLLBAR_THUMB_HORIZONTAL;

    use crate::components::{buffer_text, key};
    use crate::theme;
    use test_case::test_case;

    const EVENT: &str = r#"{"timestamp":"2026-09-09T14:22:07.418123Z","level":"WARN","fields":{"message":"retryable, will retry","attempt":3},"target":"caudra::provider"}"#;
    const WITH_SPAN: &str = r#"{"timestamp":"2026-09-09T14:22:07.418123Z","level":"INFO","fields":{"message":"tool result"},"target":"caudra.tool_result","spans":[{"name":"turn","session_id":"s-1","turn_id":4}]}"#;
    const CONTROL_CHARS: &str = r#"{"timestamp":"2026-09-09T14:22:07.418123Z","level":"ERROR","fields":{"message":"a\u001b[31mb"},"target":"caudra::mcp"}"#;
    const NOT_JSON: &str = "thread 'main' panicked at src/main.rs:1:1";
    const WIDE: usize = 200;
    const ESCAPE: char = '\u{1b}';

    fn rendered(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn render(raw: &str, width: usize) -> String {
        let theme = theme::current();
        rendered(
            &lay_out(
                &entry_line(&Entry::parse(raw), false, &theme),
                width,
                0,
                false,
            )[0],
        )
    }

    #[test]
    fn a_record_renders_time_level_target_message_and_fields() {
        let out = render(EVENT, WIDE);
        assert!(out.contains("14:22:07.418"), "{out}");
        assert!(out.contains("WARN"), "{out}");
        assert!(out.contains("caudra::provider"), "{out}");
        assert!(out.contains("retryable, will retry"), "{out}");
        assert!(out.contains("attempt=3"), "{out}");
    }

    #[test]
    fn correlation_from_spans_reaches_the_one_line_view() {
        let out = render(WITH_SPAN, WIDE);
        assert!(out.contains("session_id=s-1"), "{out}");
        assert!(out.contains("turn_id=4"), "{out}");
    }

    #[test]
    fn an_unparseable_line_renders_verbatim() {
        assert_eq!(render(NOT_JSON, WIDE), NOT_JSON);
    }

    #[test]
    fn control_sequences_never_reach_the_terminal() {
        let out = render(CONTROL_CHARS, WIDE);
        assert!(!out.contains(ESCAPE), "{out}");
    }

    /// A narrow pane clips rather than dropping, so no field is unreachable.
    /// The row is longer than the pane and panning is what brings it into view.
    #[test]
    fn a_narrow_pane_keeps_every_field_on_a_row_that_runs_past_it() {
        const NARROW: usize = 60;
        let theme = theme::current();
        let full = entry_line(&Entry::parse(EVENT), false, &theme);

        assert!(rendered(&full).contains("attempt=3"));
        assert!(display_width(&full) > NARROW);

        let panned = &lay_out(&full, NARROW, 20, false)[0];
        assert!(
            !rendered(panned).contains("14:22:07"),
            "the pan never moved"
        );
        assert!(rendered(panned).contains("attempt=3"));
    }

    #[test]
    fn wrapping_puts_the_whole_record_on_screen_across_several_rows() {
        let theme = theme::current();
        let full = entry_line(&Entry::parse(EVENT), false, &theme);
        let rows = lay_out(&full, 40, 0, true);

        assert!(rows.len() > 1);
        assert!(rows.iter().all(|row| display_width(row) <= 40));
        let joined: String = rows.iter().map(rendered).collect();
        assert!(joined.contains("attempt=3"), "{joined}");
    }

    #[test]
    fn a_cut_row_keeps_the_styles_of_the_spans_it_came_from() {
        let theme = theme::current();
        let full = entry_line(&Entry::parse(EVENT), false, &theme);
        let rows = lay_out(&full, 40, 0, true);

        let styles: Vec<_> = rows
            .iter()
            .flat_map(|r| r.spans.iter().map(|s| s.style))
            .collect();
        assert!(
            styles
                .iter()
                .any(|style| *style == level_style(Level::Warn, &theme)),
            "the level lost its colour on the way through the wrap"
        );
    }

    #[test]
    fn expanding_shows_every_field_and_the_raw_record() {
        let theme = theme::current();
        let line = caudra_storage::log::tail::Line {
            raw: WITH_SPAN.to_owned(),
            entry: Entry::parse(WITH_SPAN),
        };
        let out: String = expansion_lines(&line, &theme)
            .iter()
            .map(rendered)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(out.contains("turn.session_id=s-1"), "{out}");
        assert!(out.contains("turn.turn_id=4"), "{out}");
        assert!(out.contains("2026-09-09T14:22:07.418123Z"), "{out}");
        assert!(out.contains(RAW_JSON_LABEL), "{out}");
    }

    #[test]
    fn a_copied_line_keeps_every_field_without_a_width_cap() {
        let out = plain_text(&Entry::parse(WITH_SPAN));
        assert!(out.contains("session_id=s-1"), "{out}");
        assert!(out.contains("caudra.tool_result"), "{out}");
    }

    #[test_case(0, "0 B" ; "zero")]
    #[test_case(512, "512 B" ; "bytes")]
    #[test_case(2048, "2.0 KB" ; "kilobytes")]
    #[test_case(5 * 1024 * 1024, "5.0 MB" ; "megabytes")]
    fn sizes_read_as_units(bytes: u64, expected: &str) {
        assert_eq!(format_bytes(bytes), expected);
    }

    #[test_case(0, DEFAULT_MAX_FILES ; "zero falls back")]
    #[test_case(3, 3 ; "configured wins")]
    fn retention_drives_how_far_back_scrolling_reaches(configured: u32, expected: u32) {
        assert_eq!(max_files_or_default(configured), expected);
    }

    const VIEWPORT: usize = 8;
    const SEEDED: usize = 60;
    const OFF_CHIP: u16 = 250;
    const OFF_SCREEN: &str = "the cursor must stay on a visible row";
    const BAR_IGNORED: &str = "a press on the bar's column must scroll the pane";
    const OVERFLOWING_MESSAGE: usize = 200;
    const NO_PAN_BAR: &str = "the rows must run past the modal for the bar to exist";
    const NO_CHIP: &str = "the footer must have drawn the chip for the press to land on";
    const CHIP_LOST: &str = "the pan bar's touch margin swallowed the level chip";

    /// Seeds a log directory and opens against it, so the tests never touch the
    /// real one and never depend on what a previous run happened to write.
    fn seeded_modal(dir: &std::path::Path) -> LogsModal {
        seeded_with(
            dir,
            &(0..SEEDED).map(|i| format!("line-{i}")).collect::<Vec<_>>(),
        )
    }

    /// The same seed with a last record too wide for any modal, which is what
    /// puts a pan bar on the row the level chip shares.
    fn seeded_modal_overflowing(dir: &std::path::Path) -> LogsModal {
        let mut messages: Vec<String> = (0..SEEDED).map(|i| format!("line-{i}")).collect();
        messages.push("x".repeat(OVERFLOWING_MESSAGE));
        seeded_with(dir, &messages)
    }

    fn seeded_with(dir: &std::path::Path, messages: &[String]) -> LogsModal {
        let lines: Vec<String> = messages
            .iter()
            .map(|message| {
                format!(
                    r#"{{"timestamp":"2026-09-09T14:22:07.418123Z","level":"INFO","fields":{{"message":"{message}"}},"target":"caudra::agent"}}"#
                )
            })
            .collect();
        std::fs::write(
            caudra_storage::log::file_path(dir, 0),
            format!("{}\n", lines.join("\n")),
        )
        .unwrap();

        let mut modal = LogsModal::new(DEFAULT_MAX_FILES);
        modal.viewport_h = VIEWPORT;
        modal.open_dir(dir);
        modal
    }

    fn wheel(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    /// The wheel is a viewport control. The cursor comes along only as far as
    /// staying on screen requires, and never drives the view itself.
    #[test]
    fn the_wheel_moves_the_page_by_exactly_what_it_was_given() {
        let tmp = tempfile::tempdir().unwrap();
        let mut modal = seeded_modal(tmp.path());
        let top = modal.view_top;
        assert_eq!(modal.selected, top + VIEWPORT - 1);

        modal.scroll(3);

        assert_eq!(modal.view_top, top - 3);
        assert_eq!(
            modal.selected,
            modal.view_top + VIEWPORT - 1,
            "{OFF_SCREEN}"
        );
    }

    /// A cursor already inside the new page is left where it was, which is the
    /// difference between scrolling the view and moving the selection.
    #[test]
    fn the_wheel_leaves_a_cursor_that_is_still_on_screen_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let mut modal = seeded_modal(tmp.path());
        modal.move_cursor(-(VIEWPORT as isize - 1));
        let selected = modal.selected;

        modal.scroll(-1);

        assert_eq!(modal.selected, selected);
    }

    #[test]
    fn scrolling_up_pauses_following_and_scrolling_back_to_the_end_resumes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let mut modal = seeded_modal(tmp.path());
        assert!(modal.follow);

        modal.scroll(3);
        assert!(!modal.follow);

        modal.scroll(-3);
        assert!(modal.follow);
    }

    #[test]
    fn clicking_the_level_chip_cycles_the_minimum_level() {
        let tmp = tempfile::tempdir().unwrap();
        let mut modal = seeded_modal(tmp.path());
        let before = modal.filter.min_level;
        modal.level_hit = Rect::new(4, 9, 6, 1);

        modal.handle_mouse(wheel(MouseEventKind::Down(MouseButton::Left), 5, 9));

        assert_eq!(modal.filter.min_level, before.next());
    }

    /// The pan bar's touch margin reaches off its border row and over the footer
    /// the chip is drawn on, so the two want the same cells. The chip is the
    /// smaller target and the one the reader aimed at.
    #[test]
    fn a_touch_on_the_level_chip_cycles_it_rather_than_panning() {
        caudra_workbench::scroll::set_touch(true);
        let tmp = tempfile::tempdir().unwrap();
        let mut modal = seeded_modal_overflowing(tmp.path());
        let before = modal.filter.min_level;
        let out = drawn(&mut modal);
        assert!(out.contains(SCROLLBAR_THUMB_HORIZONTAL), "{NO_PAN_BAR}");
        assert!(modal.level_hit.width > 0, "{NO_CHIP}");

        modal.handle_mouse(wheel(
            MouseEventKind::Down(MouseButton::Left),
            modal.level_hit.x,
            modal.level_hit.y,
        ));

        let level = modal.filter.min_level;
        let pan = modal.pan;
        caudra_workbench::scroll::set_touch(false);
        assert_eq!(level, before.next(), "{CHIP_LOST}");
        assert_eq!(pan, 0, "{CHIP_LOST}");
    }

    #[test]
    fn the_level_chip_tracks_the_pointer() {
        let mut modal = LogsModal::new(DEFAULT_MAX_FILES);
        modal.level_hit = Rect::new(4, 9, 6, 1);

        modal.handle_mouse(wheel(MouseEventKind::Moved, 5, 9));
        assert!(modal.level_hovered);

        modal.handle_mouse(wheel(MouseEventKind::Moved, OFF_CHIP, 9));
        assert!(!modal.level_hovered);
    }

    #[test]
    fn the_footer_reports_where_it_drew_the_level_chip() {
        let theme = theme::current();
        let modal = LogsModal::new(DEFAULT_MAX_FILES);
        let (line, level) = modal.footer_line(&theme);

        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let chars: String = text.chars().skip(level.start).take(level.len()).collect();
        assert_eq!(chars, format!("{LEVEL_PREFIX}{}", modal.filter.min_level));
    }

    #[test]
    fn a_click_lands_on_the_record_under_the_pointer_even_below_an_expansion() {
        let tmp = tempfile::tempdir().unwrap();
        let mut modal = seeded_modal(tmp.path());
        modal.body = Rect::new(0, 2, 80, VIEWPORT as u16);
        // Two screen rows for one record, so plain arithmetic would be off by
        // the height of the expansion.
        modal.rows = vec![10, 10, 11, 12];

        modal.handle_mouse(wheel(MouseEventKind::Down(MouseButton::Left), 1, 4));

        assert_eq!(modal.selected, 11);
    }

    #[test]
    fn a_press_on_the_bar_scrolls_the_pane() {
        let tmp = tempfile::tempdir().unwrap();
        let mut modal = seeded_modal(tmp.path());
        let _ = drawn(&mut modal);
        assert!(modal.view_top > 0);

        // The first row of a track is the top of the document by definition, so
        // the press lands there wherever rounding painted the thumb.
        let column = modal.body.right() + H_PAD - 1;
        modal.handle_mouse(wheel(
            MouseEventKind::Down(MouseButton::Left),
            column,
            modal.body.y,
        ));

        assert_eq!(modal.view_top, 0, "{BAR_IGNORED}");
    }

    const TOOL_RECORD: &str = r#"{"timestamp":"2026-09-09T14:22:07.418123Z","level":"INFO","fields":{"message":"tool result"},"target":"caudra::tool","spans":[{"name":"turn","session_id":"s-1","turn_id":4},{"name":"tool","tool_use_id":"tu-9"}]}"#;
    const NO_ID: &str = r#"{"timestamp":"2026-09-09T14:22:07.418123Z","level":"INFO","fields":{"message":"hi"},"target":"caudra::agent"}"#;
    const FRAME_W: u16 = 120;
    const FRAME_H: u16 = 30;
    const NOT_DRAWN: &str = "the row never reached the frame";

    fn drawn(modal: &mut LogsModal) -> String {
        let backend = ratatui::backend::TestBackend::new(FRAME_W, FRAME_H);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let theme = theme::current();
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area(), &theme);
            })
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn the_hint_row_names_every_action_the_selection_has() {
        let tmp = tempfile::tempdir().unwrap();
        let mut modal = seeded_modal(tmp.path());
        let out = drawn(&mut modal);

        for (_, action) in modal.hints() {
            assert!(out.contains(action), "{action} missing from {out}");
        }
    }

    #[test]
    fn the_hint_row_follows_what_the_key_will_do_next() {
        let tmp = tempfile::tempdir().unwrap();
        let mut modal = seeded_modal(tmp.path());
        assert!(drawn(&mut modal).contains(HINT_EXPAND));

        modal.handle_key(key(KeyCode::Enter));
        assert!(drawn(&mut modal).contains(HINT_COLLAPSE));
    }

    #[test]
    fn pressing_slash_opens_a_field_with_a_cursor_in_it() {
        let tmp = tempfile::tempdir().unwrap();
        let mut modal = seeded_modal(tmp.path());
        let before = drawn(&mut modal);
        assert!(!before.contains(HINT_APPLY));

        modal.handle_key(key(KeyCode::Char('/')));
        modal.handle_key(key(KeyCode::Char('l')));
        modal.handle_key(key(KeyCode::Char('n')));
        let out = drawn(&mut modal);

        assert!(out.contains(HINT_APPLY), "{NOT_DRAWN}: {out}");
        assert!(out.contains(HINT_CLEAR), "{NOT_DRAWN}: {out}");
        assert!(out.contains("ln"), "typed text never appeared: {out}");
    }

    #[test]
    fn the_search_row_only_takes_a_line_while_it_is_in_use() {
        let tmp = tempfile::tempdir().unwrap();
        let mut modal = seeded_modal(tmp.path());
        let _ = drawn(&mut modal);
        let quiet = modal.viewport_h;

        modal.handle_key(key(KeyCode::Char('/')));
        let _ = drawn(&mut modal);
        assert_eq!(modal.viewport_h, quiet - 1);

        modal.handle_key(key(KeyCode::Esc));
        let _ = drawn(&mut modal);
        assert_eq!(modal.viewport_h, quiet);
    }

    #[test_case(TOOL_RECORD, "tu-9" ; "the tool call over the turn that made it")]
    #[test_case(WITH_SPAN, "s-1" ; "the session when nothing narrower is there")]
    fn focus_narrows_to_the_records_most_specific_id(raw: &str, expected: &str) {
        assert_eq!(
            correlation_id(&Entry::parse(raw)).as_deref(),
            Some(expected)
        );
    }

    #[test]
    fn a_record_with_no_id_says_so_rather_than_filtering_to_nothing() {
        assert!(correlation_id(&Entry::parse(NO_ID)).is_none());
        assert!(correlation_id(&Entry::parse(NOT_JSON)).is_none());
    }

    #[test]
    fn focus_puts_the_id_in_the_field_so_it_can_be_edited_or_cleared() {
        let tmp = tempfile::tempdir().unwrap();
        let mut modal = seeded_modal(tmp.path());
        std::fs::write(
            caudra_storage::log::file_path(tmp.path(), 0),
            format!("{TOOL_RECORD}\n"),
        )
        .unwrap();
        modal.reload();

        assert!(matches!(
            modal.handle_key(key(KeyCode::Tab)),
            LogsAction::Consumed
        ));
        assert_eq!(modal.query.value(), "tu-9");
    }

    #[test]
    fn focus_on_a_record_without_an_id_flashes_instead() {
        let tmp = tempfile::tempdir().unwrap();
        let mut modal = seeded_modal(tmp.path());

        assert!(matches!(
            modal.handle_key(key(KeyCode::Tab)),
            LogsAction::Flash(NO_FOCUS)
        ));
    }

    #[test]
    fn wrapping_and_panning_are_one_question_so_each_undoes_the_other() {
        let tmp = tempfile::tempdir().unwrap();
        let mut modal = seeded_modal(tmp.path());
        modal.max_pan = 40;

        modal.handle_key(key(KeyCode::Right));
        assert_eq!(modal.pan, PAN_STEP as usize);

        modal.handle_key(key(KeyCode::Char('w')));
        assert!(modal.wrap);
        assert_eq!(
            modal.pan, 0,
            "wrapping has nothing past the margin to pan to"
        );

        modal.handle_key(key(KeyCode::Right));
        assert_eq!(modal.pan, 0, "a wrapped pane must ignore the pan keys");
    }

    #[test]
    fn panning_stops_at_the_widest_row_on_screen() {
        let tmp = tempfile::tempdir().unwrap();
        let mut modal = seeded_modal(tmp.path());
        modal.max_pan = 5;

        for _ in 0..10 {
            modal.handle_key(key(KeyCode::Right));
        }
        assert_eq!(modal.pan, 5);

        modal.handle_key(key(KeyCode::Home));
        assert_eq!(modal.pan, 0);
    }

    #[test]
    fn a_wrapped_record_keeps_the_newest_one_on_screen() {
        let tmp = tempfile::tempdir().unwrap();
        let mut modal = seeded_modal(tmp.path());
        modal.handle_key(key(KeyCode::Char('w')));
        let out = drawn(&mut modal);

        assert!(
            out.contains(&format!("line-{}", SEEDED - 1)),
            "following lost the newest record to the fold: {out}"
        );
    }

    #[test]
    fn a_closed_modal_polls_nothing() {
        let mut modal = LogsModal::new(DEFAULT_MAX_FILES);
        assert_eq!(modal.poll(), Dirty::NO);
        assert_eq!(modal.cadence(), Cadence::IDLE);
    }
}
