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
use crate::components::scrollbar::render_vertical_scrollbar;
use crate::components::{Overlay, escape_terminal_controls, hover_style, is_ctrl};
use crate::repaint::{Cadence, Dirty};
use crate::text_buffer::TextBuffer;
use crate::theme::Theme;

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
const OVERFLOW_PREFIX: &str = "+";
const FILTER_PREFIX: &str = "/";
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

/// Fields shown inline. The rest live behind the expand key, so one slow tool
/// call with twenty fields cannot push every other row off the screen.
const INLINE_FIELD_LIMIT: usize = 6;
/// Span fields worth carrying on the one-line view.
const CORRELATION_FIELDS: &[&str] = &["session_id", "turn_id", "request_id", "tool_use_id"];

pub enum LogsAction {
    Consumed,
    Close,
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
            outcome: ScanOutcome::Filled,
            last_sequence: 0,
            viewport_h: 0,
            popup: Rect::default(),
            body: Rect::default(),
            rows: Vec::new(),
            level_hit: Rect::default(),
            level_hovered: false,
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
            KeyCode::Enter | KeyCode::Char(' ') => self.expanded = !self.expanded,
            KeyCode::Char('y') => return self.copy(false),
            KeyCode::Char('Y') => return self.copy(true),
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-self.half_page()),
            KeyCode::PageDown => self.move_cursor(self.half_page()),
            _ if key::SCROLL_HALF_UP.matches(event) || key::SCROLL_HALF_UP_ALT.matches(event) => {
                self.move_cursor(-self.half_page());
            }
            _ if key::SCROLL_HALF_DOWN.matches(event) => self.move_cursor(self.half_page()),
            _ if key::SCROLL_LINE_UP.matches(event) => self.move_cursor(-1),
            _ if key::SCROLL_LINE_DOWN.matches(event) => self.move_cursor(1),
            _ if key::SCROLL_TOP.matches(event) || key::SCROLL_TOP_ALT.matches(event) => {
                self.move_cursor(-(MIN_OVERSCAN as isize));
            }
            _ if key::SCROLL_BOTTOM.matches(event) || key::SCROLL_BOTTOM_ALT.matches(event) => {
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
        self.reveal();
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

    fn reveal(&mut self) {
        let height = self.viewport_h.max(1);
        if self.selected < self.view_top {
            self.view_top = self.selected;
        } else if self.selected >= self.view_top + height {
            self.view_top = self.selected + 1 - height;
        }
        self.view_top = self.view_top.min(self.window_len().saturating_sub(1));
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> LogsAction {
        let pos = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::ScrollUp => self.scroll_view(-1),
            MouseEventKind::ScrollDown => self.scroll_view(1),
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
        // One row is the footer, which never scrolls.
        let body_height = padded.height.saturating_sub(1);
        self.resize(usize::from(body_height));

        self.body = Rect {
            height: body_height,
            ..padded
        };
        let (lines, rows) = self.body_lines(usize::from(padded.width), theme);
        self.rows = rows;
        frame.render_widget(
            Paragraph::new(lines).style(Style::new().fg(theme.foreground)),
            self.body,
        );
        let footer = Rect {
            y: padded.y.saturating_add(body_height),
            height: 1,
            ..padded
        };
        let (line, level) = self.footer_line(theme);
        self.level_hit = hit_rect(footer, level);
        frame.render_widget(Paragraph::new(line), footer);
        let len = u16::try_from(self.window_len()).unwrap_or(u16::MAX);
        if len > body_height {
            let offset = u16::try_from(self.view_top).unwrap_or(u16::MAX);
            render_vertical_scrollbar(frame, inner, len, offset);
        }

        self.popup = popup;
        popup
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
        } else {
            self.reveal();
        }
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
        for (index, line) in tail
            .window()
            .iter()
            .enumerate()
            .skip(self.view_top)
            .take(self.viewport_h)
        {
            let selected = index == self.selected;
            lines.push(entry_line(&line.entry, width, selected, theme));
            rows.push(index);
            if selected && self.expanded {
                for extra in expansion_lines(line, width, theme) {
                    lines.push(extra);
                    rows.push(index);
                }
            }
        }
        lines.truncate(self.viewport_h);
        rows.truncate(self.viewport_h);
        (lines, rows)
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
        let query = self.query.value();
        if self.query_focused || !query.is_empty() {
            push(
                format!("{FILTER_PREFIX}{}", escape_terminal_controls(&query)),
                if self.query_focused {
                    theme.cursor
                } else {
                    theme.item_match
                },
            );
        }
        push(
            if self.follow { FOLLOWING } else { PAUSED }.to_owned(),
            if self.follow {
                theme.tool_success
            } else {
                theme.status_dim
            },
        );
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
fn entry_line(entry: &Entry, width: usize, selected: bool, theme: &Theme) -> Line<'static> {
    let line = match entry {
        Entry::Raw(raw) => Line::from(Span::styled(escape_terminal_controls(raw), theme.tool_dim)),
        Entry::Record(record) => record_line(record, width, theme),
    };
    if selected {
        line.style(theme.item_selected)
    } else {
        line
    }
}

fn record_line(record: &Record, width: usize, theme: &Theme) -> Line<'static> {
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

    let mut used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    for (key, value) in correlation_fields(record) {
        push_field(
            &mut spans,
            &mut used,
            width,
            &key,
            &value,
            theme.tool_dim,
            theme.accent,
        );
    }
    let mut shown = 0;
    for (key, value) in &record.fields {
        if shown >= INLINE_FIELD_LIMIT {
            break;
        }
        if push_field(
            &mut spans,
            &mut used,
            width,
            key,
            value,
            theme.tool_annotation,
            theme.accent,
        ) {
            shown += 1;
        }
    }
    let hidden = record.fields.len().saturating_sub(shown);
    if hidden > 0 {
        spans.push(Span::styled(
            format!("{FIELD_GAP}{OVERFLOW_PREFIX}{hidden}"),
            theme.tool_dim,
        ));
    }
    Line::from(spans)
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

/// Returns whether the field fit. Once one does not, no later one will either,
/// and the caller counts it toward the hidden total.
fn push_field(
    spans: &mut Vec<Span<'static>>,
    used: &mut usize,
    width: usize,
    key: &str,
    value: &str,
    key_style: Style,
    value_style: Style,
) -> bool {
    let key = escape_terminal_controls(key);
    let value = escape_terminal_controls(value);
    let cost = key.chars().count() + value.chars().count() + FIELD_GAP.len() + KV_SEPARATOR.len();
    if *used + cost > width {
        return false;
    }
    *used += cost;
    spans.push(Span::raw(FIELD_GAP));
    spans.push(Span::styled(key, key_style));
    spans.push(Span::styled(KV_SEPARATOR.to_owned(), key_style));
    spans.push(Span::styled(value, value_style));
    true
}

fn expansion_lines(
    line: &caudra_storage::log::tail::Line,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
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
    out.push(detail(RAW_JSON_LABEL, &truncate(&line.raw, width), theme));
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

fn truncate(text: &str, width: usize) -> String {
    let budget =
        width.saturating_sub(EXPAND_INDENT.len() + EXPAND_ARROW.len() + RAW_JSON_LABEL.len());
    match text.char_indices().nth(budget) {
        Some((at, _)) => text[..at].to_owned(),
        None => text.to_owned(),
    }
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
        rendered(&entry_line(&Entry::parse(raw), width, false, &theme))
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

    #[test]
    fn a_narrow_viewport_drops_fields_and_counts_them() {
        let narrow = render(EVENT, 60);
        assert!(!narrow.contains("attempt=3"), "{narrow}");
        assert!(narrow.contains("+1"), "{narrow}");
    }

    #[test]
    fn expanding_shows_every_field_and_the_raw_record() {
        let theme = theme::current();
        let line = caudra_storage::log::tail::Line {
            raw: WITH_SPAN.to_owned(),
            entry: Entry::parse(WITH_SPAN),
        };
        let out: String = expansion_lines(&line, WIDE, &theme)
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

    /// Seeds a log directory and opens against it, so the tests never touch the
    /// real one and never depend on what a previous run happened to write.
    fn seeded_modal(dir: &std::path::Path) -> LogsModal {
        let lines: Vec<String> = (0..SEEDED)
            .map(|i| {
                format!(
                    r#"{{"timestamp":"2026-09-09T14:22:07.418123Z","level":"INFO","fields":{{"message":"line-{i}"}},"target":"caudra::agent"}}"#
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
    fn a_closed_modal_polls_nothing() {
        let mut modal = LogsModal::new(DEFAULT_MAX_FILES);
        assert_eq!(modal.poll(), Dirty::NO);
        assert_eq!(modal.cadence(), Cadence::IDLE);
    }
}
