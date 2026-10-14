//! The panel itself: a Status tab for the engagement, and a Log tab listing
//! what the session's log keeps beside the detail of the entry under the
//! cursor. The log is read off the UI thread each time the panel opens, and
//! what the session writes while it stays open lands in it as written.

use std::mem;

use caudra_config::ClockFormat;
use caudra_grab::grab_scope;
use caudra_storage::autopilot::StoredAutopilotDecision;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use smol::Task;
use unicode_width::UnicodeWidthStr;

use super::log::{AutopilotLog, LogFilter, LogRow, entry_detail, entry_spans, hint};
use super::status::{PanelContent, status_lines};
use crate::components::command_text::ellipsize_spans;
use crate::components::keybindings::key;
use crate::components::modal::{ESC_LABEL, FooterHits, FooterLine, Modal, SEPARATOR};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::section_tabs::{SectionTab, tab_strip};
use crate::components::{
    ModalScroll, Overlay, ascii_key, escape_terminal_controls, hover_style, plain_char,
};
use crate::repaint::{Cadence, Dirty};
use crate::storage_writer::AutopilotWrite;
use crate::theme::{self, Theme};

const TITLE: &str = " Autopilot ";
const WIDTH_PERCENT: u16 = 86;
const MAX_HEIGHT_PERCENT: u16 = 80;
const H_PAD: u16 = 1;
const PANE_GAP: u16 = 1;
const LIST_PERCENT: u32 = 45;
const PERCENT: u32 = 100;
/// A list narrower than this cuts a row before its subject, and a detail
/// narrower than this wraps every field, so a body too narrow for both shows
/// one.
const LIST_MIN_COLS: u16 = 28;
const DETAIL_MIN_COLS: u16 = 40;
const SPLIT_MIN_COLS: u16 = LIST_MIN_COLS + PANE_GAP + DETAIL_MIN_COLS;
const CURSOR_MARK: &str = "\u{203a} ";
const NO_MARK: &str = "  ";
/// A blank column at the end of every list row, so the widest row never reads
/// on into the detail beside it.
const LIST_GUTTER: usize = 1;
pub(crate) const FILTER_PREFIX: &str = "Filter: ";
const SECTION_GAP: &str = "  ";
const LINE_BREAK: &str = "\n";
const KEY_GAP: &str = " ";
pub(crate) const STOP_LABEL: &str = "s";
pub(crate) const MODEL_LABEL_KEY: &str = "m";
pub(crate) const CLOSE_LABEL: &str = "q";
pub(crate) const FILTER_LABEL: &str = "f";
pub(crate) const COPY_LABEL: &str = "y";
pub(crate) const DOWN_LABEL: &str = "j";
pub(crate) const UP_LABEL: &str = "k";
pub(crate) const STOP_KEY: char = ascii_key(STOP_LABEL);
pub(crate) const MODEL_KEY: char = ascii_key(MODEL_LABEL_KEY);
const CLOSE_KEY: char = ascii_key(CLOSE_LABEL);
const FILTER_KEY: char = ascii_key(FILTER_LABEL);
const COPY_KEY: char = ascii_key(COPY_LABEL);
const DOWN_KEY: char = ascii_key(DOWN_LABEL);
const UP_KEY: char = ascii_key(UP_LABEL);
pub(crate) const COPIED_ENTRY: &str = "Copied log entry";
pub(crate) const NOTHING_TO_COPY: &str = "Nothing to copy";
pub(crate) const READING_LOG: &str = "Reading the autopilot log\u{2026}";
pub(crate) const NO_ENTRIES: &str = "No autopilot log for this session yet";
pub(crate) const NOTHING_MATCHES: &str = "Nothing matches this filter";
const STATUS_FOOTER: &[(&str, &str, FooterCommand)] = &[
    (key::TAB.label, "Section", FooterCommand::Section),
    (STOP_LABEL, "Stop", FooterCommand::Stop),
    (MODEL_LABEL_KEY, "Model", FooterCommand::Model),
    (ESC_LABEL, "Close", FooterCommand::Close),
];
const LOG_FOOTER: &[(&str, &str, FooterCommand)] = &[
    (key::TAB.label, "Section", FooterCommand::Section),
    (FILTER_LABEL, "Filter", FooterCommand::Filter),
    (COPY_LABEL, "Copy", FooterCommand::Copy),
    (key::ENTER.label, "Jump", FooterCommand::Jump),
    (STOP_LABEL, "Stop", FooterCommand::Stop),
    (MODEL_LABEL_KEY, "Model", FooterCommand::Model),
    (ESC_LABEL, "Close", FooterCommand::Close),
];
/// Glossed, then keys alone, then keys packed: a footer too wide for its row
/// answers no clicks, so it gives up words before it gives up a key.
const FOOTER_RUNGS: [(bool, &str); 3] = [(true, SEPARATOR), (false, SECTION_GAP), (false, KEY_GAP)];

/// The read of a session's log the panel waits on.
pub(crate) type LogLoad = Task<Result<AutopilotLog, String>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PanelTab {
    Status,
    Log,
}

impl SectionTab for PanelTab {
    const ALL: &'static [Self] = &[Self::Status, Self::Log];

    fn label(self) -> &'static str {
        match self {
            Self::Status => "Status",
            Self::Log => "Log",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pane {
    List,
    Detail,
}

/// What the panel asks the host to do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PanelAction {
    Stop,
    Model,
    /// Copy the text, flashing [`COPIED_ENTRY`].
    Copy(String),
    /// Close, and scroll the transcript to the row of the decision with this
    /// seq.
    Jump(u64),
    Flash(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FooterCommand {
    Section,
    Filter,
    Copy,
    Jump,
    Stop,
    Model,
    Close,
}

#[derive(Default)]
enum LogState {
    #[default]
    Closed,
    /// Writes that land while the read is out are applied over what it
    /// returns, so one it missed is not lost.
    Loading {
        task: LogLoad,
        pending: Vec<AutopilotWrite>,
    },
    Ready(AutopilotLog),
    Failed(String),
}

pub struct AutopilotPanel {
    open: bool,
    tab: PanelTab,
    pane: Pane,
    filter: LogFilter,
    /// The entry the Log shows, by seq, so live rows landing above it leave
    /// it selected.
    selected: Option<u64>,
    log: LogState,
    clock: ClockFormat,
    list_scroll: ModalScroll,
    body_scroll: ModalScroll,
    list_bar: Scrollbar,
    body_bar: Scrollbar,
    popup: Rect,
    list_area: Rect,
    body_area: Rect,
    tab_hits: Vec<(Rect, PanelTab)>,
    /// Where the pointer last was, so each part answers for its own geometry
    /// on the frame it is drawn.
    pointer: Option<Position>,
    /// A key moved the list cursor, so the next draw scrolls it into view.
    reveal_cursor: bool,
    footer_hits: FooterHits,
}

impl Default for AutopilotPanel {
    fn default() -> Self {
        Self {
            open: false,
            tab: PanelTab::Status,
            pane: Pane::List,
            filter: LogFilter::default(),
            selected: None,
            log: LogState::default(),
            clock: ClockFormat::default(),
            list_scroll: ModalScroll::new_top(),
            body_scroll: ModalScroll::new_top(),
            list_bar: Scrollbar::default(),
            body_bar: Scrollbar::default(),
            popup: Rect::default(),
            list_area: Rect::default(),
            body_area: Rect::default(),
            tab_hits: Vec::new(),
            pointer: None,
            reveal_cursor: false,
            footer_hits: FooterHits::default(),
        }
    }
}

impl AutopilotPanel {
    /// Opens on `tab`, with the session's log arriving from `load`, the
    /// changes it may not hold yet in `unwritten`, and its times read on
    /// `clock`.
    pub(crate) fn open(
        &mut self,
        tab: PanelTab,
        load: LogLoad,
        unwritten: Vec<AutopilotWrite>,
        clock: ClockFormat,
    ) {
        *self = Self {
            open: true,
            tab,
            clock,
            log: LogState::Loading {
                task: load,
                pending: unwritten,
            },
            reveal_cursor: true,
            ..Self::default()
        };
    }

    /// Picks up a finished read. Nothing wakes the loop when the read ends,
    /// so an unpolled panel sits on loading.
    pub(crate) fn poll(&mut self) -> Dirty {
        if !matches!(&self.log, LogState::Loading { task, .. } if task.is_finished()) {
            return Dirty::NO;
        }
        self.land();
        Dirty::YES
    }

    /// Waits out the read, for a test that cannot wait for a tick.
    #[cfg(test)]
    pub(crate) fn finish_loading(&mut self) {
        self.land();
    }

    fn land(&mut self) {
        let LogState::Loading { task, pending } = mem::take(&mut self.log) else {
            return;
        };
        self.log = match smol::block_on(task) {
            Ok(mut log) => {
                for write in &pending {
                    log.apply(write);
                }
                LogState::Ready(log)
            }
            Err(error) => LogState::Failed(error),
        };
    }

    /// Takes in what the session just wrote to its log.
    pub(crate) fn note(&mut self, write: &AutopilotWrite) {
        match &mut self.log {
            LogState::Loading { pending, .. } => pending.push(write.clone()),
            LogState::Ready(log) => log.apply(write),
            LogState::Closed | LogState::Failed(_) => {}
        }
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    /// Every key is answered here, so a stray one never reaches the
    /// transcript under the panel.
    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<PanelAction> {
        match (key.code, plain_char(&key)) {
            (KeyCode::Esc, _) => self.back(),
            (_, Some(CLOSE_KEY)) => self.close(),
            (KeyCode::Tab, _) => self.set_tab(self.tab.step(1)),
            (KeyCode::BackTab, _) => self.set_tab(self.tab.step(-1)),
            (_, Some(STOP_KEY)) => return Some(PanelAction::Stop),
            (_, Some(MODEL_KEY)) => return Some(PanelAction::Model),
            (_, Some(digit)) if digit.is_ascii_digit() => {
                if let Some(tab) = PanelTab::from_digit(digit) {
                    self.set_tab(tab);
                }
            }
            _ if self.tab == PanelTab::Log => return self.handle_log_key(key),
            _ => {
                self.body_scroll.handle_key(key);
            }
        }
        None
    }

    fn handle_log_key(&mut self, key: KeyEvent) -> Option<PanelAction> {
        if self.pane == Pane::List
            && let Some(delta) = self.list_scroll.list_jump(key)
        {
            self.step(delta);
            return None;
        }
        let list = self.pane == Pane::List;
        match (key.code, plain_char(&key)) {
            (_, Some(FILTER_KEY)) => self.cycle_filter(),
            (_, Some(COPY_KEY)) => return Some(self.copy()),
            (KeyCode::Enter, _) => return self.enter(),
            (KeyCode::Left, _) => self.pane = Pane::List,
            (KeyCode::Right, _) => self.pane = Pane::Detail,
            (KeyCode::Up, _) | (_, Some(UP_KEY)) if list => self.step(-1),
            (KeyCode::Down, _) | (_, Some(DOWN_KEY)) if list => self.step(1),
            _ => {
                self.body_scroll.handle_key(key);
            }
        }
        None
    }

    /// The wheel scrolls whichever pane is under the pointer. Over the list it
    /// moves the view and leaves the selection where it is.
    pub(crate) fn scroll_at(&mut self, pos: Position, delta: i32) {
        match self.list_area.contains(pos) {
            true => self.list_scroll.scroll(delta),
            false => self.body_scroll.scroll(delta),
        }
    }

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> Option<PanelAction> {
        let pos = Position::new(event.column, event.row);
        self.pointer = Some(pos);
        if let Some(index) = self.footer_hits.handle_mouse(event) {
            return self.footer_command(index);
        }
        if let Some(moved) = bar_event(&mut self.list_bar, &event) {
            if let Some(top) = moved {
                self.list_scroll.scroll_to(top);
            }
            return None;
        }
        if let Some(moved) = bar_event(&mut self.body_bar, &event) {
            if let Some(top) = moved {
                self.body_scroll.scroll_to(top);
            }
            return None;
        }
        if event.kind != MouseEventKind::Down(MouseButton::Left) {
            return None;
        }
        if let Some(&(_, tab)) = self.tab_hits.iter().find(|(hit, _)| hit.contains(pos)) {
            self.set_tab(tab);
        } else if self.list_area.contains(pos) {
            self.pane = Pane::List;
            self.click_row(
                usize::from(pos.y - self.list_area.y) + usize::from(self.list_scroll.offset()),
            );
        } else if self.body_area.contains(pos) && self.tab == PanelTab::Log {
            self.pane = Pane::Detail;
        }
        None
    }

    pub(crate) fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        content: &PanelContent<'_>,
    ) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("autopilot_panel", area);
        let theme = theme::current();
        let modal = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, area.height);
        self.popup = popup;
        let padded = Rect {
            x: inner.x.saturating_add(H_PAD),
            width: inner.width.saturating_sub(H_PAD.saturating_mul(2)),
            ..inner
        };
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(padded);
        self.render_header(frame, header, &theme);
        match self.tab {
            PanelTab::Status => {
                self.list_area = Rect::default();
                self.list_bar = Scrollbar::default();
                self.body_area = body;
                self.render_body(frame, body, status_lines(content, &theme));
            }
            PanelTab::Log => self.render_log(frame, body, &theme),
        }
        self.render_footer(frame, footer, &theme);
        popup
    }

    #[cfg(test)]
    pub(crate) fn tab(&self) -> PanelTab {
        self.tab
    }

    #[cfg(test)]
    pub(crate) fn pane(&self) -> Pane {
        self.pane
    }

    #[cfg(test)]
    pub(crate) fn filter(&self) -> LogFilter {
        self.filter
    }

    #[cfg(test)]
    pub(crate) fn is_loaded(&self) -> bool {
        matches!(self.log, LogState::Ready(_))
    }

    #[cfg(test)]
    pub(crate) fn footer_hit(&self, command: FooterCommand) -> Rect {
        self.footer()
            .iter()
            .position(|(_, _, candidate)| *candidate == command)
            .map(|index| self.footer_hits.hit(index))
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn list_area(&self) -> Rect {
        self.list_area
    }

    #[cfg(test)]
    pub(crate) fn body_area(&self) -> Rect {
        self.body_area
    }

    fn back(&mut self) {
        if self.single_pane() && self.pane == Pane::Detail {
            self.pane = Pane::List;
        } else {
            self.close();
        }
    }

    /// One pane shows at a time, the list or the detail, as last drawn.
    fn single_pane(&self) -> bool {
        self.tab == PanelTab::Log && (self.list_area.width == 0 || self.body_area.width == 0)
    }

    fn set_tab(&mut self, tab: PanelTab) {
        if tab == self.tab {
            return;
        }
        self.tab = tab;
        self.pane = Pane::List;
        self.list_scroll.reset();
        self.body_scroll.reset();
        self.reveal_cursor = true;
    }

    fn cycle_filter(&mut self) {
        self.filter = self.filter.next();
        self.list_scroll.reset();
        self.body_scroll.reset();
        self.reveal_cursor = true;
    }

    fn ready(&self) -> Option<&AutopilotLog> {
        match &self.log {
            LogState::Ready(log) => Some(log),
            LogState::Closed | LogState::Loading { .. } | LogState::Failed(_) => None,
        }
    }

    fn rows(&self) -> Vec<LogRow> {
        self.ready()
            .map(|log| log.rows(self.filter))
            .unwrap_or_default()
    }

    /// The decision a row lists, `None` for a header.
    fn entry(&self, row: LogRow) -> Option<&StoredAutopilotDecision> {
        match row {
            LogRow::Entry(index) => self.ready()?.decision(index),
            LogRow::Header { .. } => None,
        }
    }

    /// The row the cursor is on: the selected entry while it is listed, the
    /// first entry otherwise. Never a header.
    fn cursor(&self, rows: &[LogRow]) -> Option<usize> {
        let mut entries = rows
            .iter()
            .enumerate()
            .filter_map(|(at, row)| self.entry(*row).map(|decision| (at, decision.seq)));
        let first = entries.next();
        first
            .into_iter()
            .chain(entries)
            .find(|(_, seq)| Some(*seq) == self.selected)
            .or(first)
            .map(|(at, _)| at)
    }

    fn selected_entry(&self) -> Option<&StoredAutopilotDecision> {
        let rows = self.rows();
        let row = *rows.get(self.cursor(&rows)?)?;
        self.entry(row)
    }

    fn select(&mut self, seq: u64) {
        if self.selected != Some(seq) {
            self.selected = Some(seq);
            self.body_scroll.reset();
        }
    }

    /// Moves the cursor `delta` entries, stepping over the headers between.
    fn step(&mut self, delta: isize) {
        let rows = self.rows();
        let listed: Vec<u64> = rows
            .iter()
            .filter_map(|row| self.entry(*row).map(|decision| decision.seq))
            .collect();
        let Some(last) = listed.len().checked_sub(1) else {
            return;
        };
        let current = self
            .cursor(&rows)
            .and_then(|row| self.entry(rows[row]))
            .and_then(|decision| listed.iter().position(|seq| *seq == decision.seq))
            .unwrap_or_default();
        self.select(listed[current.saturating_add_signed(delta).min(last)]);
        self.reveal_cursor = true;
    }

    /// A click on a header leaves the cursor where it is.
    fn click_row(&mut self, row: usize) {
        let seq = self
            .rows()
            .get(row)
            .and_then(|row| self.entry(*row))
            .map(|decision| decision.seq);
        if let Some(seq) = seq {
            self.select(seq);
        }
    }

    /// A narrow list opens the entry's detail; otherwise the transcript is
    /// scrolled to its row.
    fn enter(&mut self) -> Option<PanelAction> {
        let seq = self.selected_entry()?.seq;
        if self.single_pane() && self.pane == Pane::List {
            self.select(seq);
            self.pane = Pane::Detail;
            return None;
        }
        Some(PanelAction::Jump(seq))
    }

    /// The selected entry as plain text, its title first.
    fn copy(&self) -> PanelAction {
        match self.selected_entry() {
            Some(decision) => {
                let lines = entry_detail(decision, self.clock, &theme::current());
                PanelAction::Copy(plain_text(&lines))
            }
            None => PanelAction::Flash(NOTHING_TO_COPY),
        }
    }

    fn footer(&self) -> &'static [(&'static str, &'static str, FooterCommand)] {
        match self.tab {
            PanelTab::Status => STATUS_FOOTER,
            PanelTab::Log => LOG_FOOTER,
        }
    }

    fn footer_command(&mut self, index: usize) -> Option<PanelAction> {
        match self.footer().get(index).map(|(_, _, command)| *command)? {
            FooterCommand::Section => self.set_tab(self.tab.step(1)),
            FooterCommand::Filter => self.cycle_filter(),
            FooterCommand::Copy => return Some(self.copy()),
            FooterCommand::Jump => {
                return self
                    .selected_entry()
                    .map(|decision| PanelAction::Jump(decision.seq));
            }
            FooterCommand::Stop => return Some(PanelAction::Stop),
            FooterCommand::Model => return Some(PanelAction::Model),
            FooterCommand::Close => self.close(),
        }
        None
    }

    /// The list and the detail side by side, or the focused one alone when
    /// the body cannot hold both. A hidden pane keeps a zero area, so nothing
    /// draws into it and the pointer cannot land on it.
    fn panes(&self, body: Rect) -> (Rect, Rect) {
        if body.width < SPLIT_MIN_COLS {
            return match self.pane {
                Pane::List => (body, Rect::default()),
                Pane::Detail => (Rect::default(), body),
            };
        }
        let share = u16::try_from(u32::from(body.width) * LIST_PERCENT / PERCENT)
            .unwrap_or(u16::MAX)
            .min(body.width - PANE_GAP - DETAIL_MIN_COLS)
            .max(LIST_MIN_COLS);
        let [list, _, detail] = Layout::horizontal([
            Constraint::Length(share),
            Constraint::Length(PANE_GAP),
            Constraint::Fill(1),
        ])
        .areas(body);
        (list, detail)
    }

    fn render_header(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let (strip, hits) = tab_strip(self.tab, self.pointer, area);
        self.tab_hits = hits;
        let room = usize::from(area.width).saturating_sub(strip.width() + SECTION_GAP.width());
        frame.render_widget(Paragraph::new(strip), area);
        if self.tab != PanelTab::Log {
            return;
        }
        let info = Line::from(vec![
            Span::styled(FILTER_PREFIX, theme.tool_dim),
            Span::styled(self.filter.label(), theme.accent),
        ]);
        let cols = info.width();
        if cols > room {
            return;
        }
        let cols = u16::try_from(cols).unwrap_or(u16::MAX);
        frame.render_widget(
            Paragraph::new(info),
            Rect {
                x: area.right().saturating_sub(cols),
                width: cols,
                ..area
            },
        );
    }

    fn render_log(&mut self, frame: &mut Frame, body: Rect, theme: &Theme) {
        let rows = self.rows();
        let cursor = self.cursor(&rows);
        let (list, detail) = self.panes(body);
        self.list_area = list;
        self.body_area = detail;
        match list.width {
            0 => self.list_bar = Scrollbar::default(),
            _ => self.render_list(frame, list, &rows, cursor, theme),
        }
        let lines = cursor
            .and_then(|row| self.entry(rows[row]))
            .map(|decision| entry_detail(decision, self.clock, theme))
            .unwrap_or_default();
        match detail.width {
            0 => self.body_bar = Scrollbar::default(),
            _ => self.render_body(frame, detail, lines),
        }
    }

    fn render_list(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        rows: &[LogRow],
        cursor: Option<usize>,
        theme: &Theme,
    ) {
        grab_scope!("autopilot_panel_list", area);
        let total = u16::try_from(rows.len().max(1)).unwrap_or(u16::MAX);
        self.list_scroll.update_dimensions(total, area.height);
        if self.reveal_cursor {
            if let Some(cursor) = cursor {
                self.list_scroll
                    .reveal_and_hold(u16::try_from(cursor).unwrap_or(u16::MAX), 1);
            }
            self.reveal_cursor = false;
        }
        let offset = self.list_scroll.offset();
        let lines = self.list_lines(rows, cursor, area, offset, theme);
        frame.render_widget(Paragraph::new(lines), area);
        self.list_bar.draw(
            frame,
            Rect {
                width: area.width.saturating_add(PANE_GAP),
                ..area
            },
            total,
            offset,
        );
    }

    /// The rows on screen, from `offset`, or what stands in for rows the log
    /// cannot list.
    fn list_lines(
        &self,
        rows: &[LogRow],
        cursor: Option<usize>,
        area: Rect,
        offset: u16,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let log = match &self.log {
            LogState::Ready(log) => log,
            LogState::Closed => return Vec::new(),
            LogState::Loading { .. } => return vec![hint(READING_LOG, theme)],
            LogState::Failed(error) => {
                return vec![Line::styled(
                    escape_terminal_controls(error),
                    theme.tool_error,
                )];
            }
        };
        if rows.is_empty() {
            return vec![hint(
                if log.is_empty() {
                    NO_ENTRIES
                } else {
                    NOTHING_MATCHES
                },
                theme,
            )];
        }
        let width = usize::from(area.width).saturating_sub(LIST_GUTTER);
        // The offset the last frame settled on is the one the pointer was
        // reported against.
        let hovered = self
            .pointer
            .filter(|at| area.contains(*at))
            .map(|at| usize::from(at.y - area.y) + usize::from(offset));
        rows.iter()
            .enumerate()
            .skip(usize::from(offset))
            .take(usize::from(area.height))
            .map(|(at, row)| match *row {
                LogRow::Header { group, decisions } => Line::from(ellipsize_spans(
                    vec![Span::styled(
                        log.header(group, decisions, self.clock),
                        theme.keybind_section,
                    )],
                    width,
                )),
                LogRow::Entry(index) => {
                    let selected = Some(at) == cursor;
                    let mark = if selected { CURSOR_MARK } else { NO_MARK };
                    let mut spans = vec![Span::styled(mark, theme.accent)];
                    spans.extend(
                        log.decision(index)
                            .map(|decision| entry_spans(decision, self.clock, theme))
                            .unwrap_or_default(),
                    );
                    let style = if selected {
                        theme.item_selected
                    } else {
                        Style::default()
                    };
                    Line::from(ellipsize_spans(spans, width))
                        .style(hover_style(style, hovered == Some(at)))
                }
            })
            .collect()
    }

    fn render_body(&mut self, frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
        grab_scope!("autopilot_panel_body", area);
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let total = u16::try_from(paragraph.line_count(area.width.max(1))).unwrap_or(u16::MAX);
        self.body_scroll.update_dimensions(total, area.height);
        let offset = self.body_scroll.offset();
        frame.render_widget(paragraph.scroll((offset, 0)), area);
        self.body_bar.draw(
            frame,
            Rect {
                width: area.width.saturating_add(H_PAD),
                ..area
            },
            total,
            offset,
        );
    }

    fn render_footer(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let selected = self.selected_entry().is_some();
        let [(glossed, gap), rest @ ..] = FOOTER_RUNGS;
        let mut footer = self.commands_footer(glossed, gap, selected, theme);
        for (glossed, gap) in rest {
            if footer.fits(area.width) {
                break;
            }
            footer = self.commands_footer(glossed, gap, selected, theme);
        }
        self.footer_hits.set(footer.hits(area, 0, 1));
        frame.render_widget(
            Paragraph::new(footer.line(self.footer_hits.hovered())),
            area,
        );
    }

    /// Copy and Jump read dim while no entry is selected.
    fn commands_footer(
        &self,
        glossed: bool,
        gap: &'static str,
        selected: bool,
        theme: &Theme,
    ) -> FooterLine {
        let mut footer = FooterLine::default();
        for (index, (key, description, command)) in self.footer().iter().enumerate() {
            if index > 0 {
                footer.text(gap, Style::default());
            }
            let enabled = selected || !matches!(command, FooterCommand::Copy | FooterCommand::Jump);
            footer.command(
                key,
                match enabled {
                    true => theme.keybind_key,
                    false => theme.tool_dim,
                },
            );
            if glossed {
                footer.describe(format!(" {description}"), theme.tool_dim);
            }
        }
        footer
    }
}

impl Overlay for AutopilotPanel {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        *self = Self::default();
    }

    /// The loop must keep ticking for [`AutopilotPanel::poll`] to see the
    /// read end.
    fn cadence(&self) -> Cadence {
        Cadence::when(
            matches!(self.log, LogState::Loading { .. }),
            Cadence::PENDING,
        )
    }
}

/// `None` when the bar let the event through, else where it moved the view,
/// if anywhere.
fn bar_event(bar: &mut Scrollbar, event: &MouseEvent) -> Option<Option<u16>> {
    match bar.handle(event) {
        ScrollbarMouse::Ignored => None,
        ScrollbarMouse::Consumed => Some(None),
        ScrollbarMouse::ScrollTo(at) => Some(Some(u16::try_from(at).unwrap_or(u16::MAX))),
    }
}

fn plain_text(lines: &[Line<'static>]) -> String {
    lines
        .iter()
        .map(|line| line.to_string().trim_end().to_owned())
        .collect::<Vec<_>>()
        .join(LINE_BREAK)
}

#[cfg(test)]
mod tests {
    use caudra_agent::autopilot::ActionName;
    use caudra_config::AutopilotConfig;
    use caudra_storage::autopilot::{DecisionOutcome, StoredAutopilotEngagement};
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use test_case::test_case;

    use super::super::log::REASON_HEADING;
    use super::super::text::fixtures::{REASON, SUBJECT, decision, engagement};
    use super::*;
    use crate::components::{buffer_text, key};

    const WIDE: u16 = 160;
    const NARROW: u16 = 60;
    const HEIGHT: u16 = 30;
    const FIRST: u32 = 1;
    const SECOND: u32 = 2;
    const OLDER: u64 = 0;
    const NEWER: u64 = 1;
    const LANDED: u64 = 2;
    const LANDED_LATER: u64 = 3;
    const HEADERS_SKIPPED: &str = "the cursor moves between entries, never onto a header";
    const ONE_PANE: &str = "a body too narrow for both panes shows one";

    fn logged(seq: u64, engagement: u32) -> StoredAutopilotDecision {
        StoredAutopilotDecision {
            seq,
            engagement: Some(engagement),
            ..decision().into()
        }
    }

    fn engaged(number: u32) -> StoredAutopilotEngagement {
        StoredAutopilotEngagement {
            engagement: number,
            ..engagement(0, 0.0, 0.0, 0, 0)
        }
    }

    /// Two engagements of one decision each, so a header sits between them.
    fn log() -> AutopilotLog {
        AutopilotLog::new(
            vec![logged(OLDER, FIRST), logged(NEWER, SECOND)],
            vec![engaged(FIRST), engaged(SECOND)],
        )
    }

    fn opened(tab: PanelTab, unwritten: Vec<AutopilotWrite>) -> AutopilotPanel {
        let mut panel = AutopilotPanel::default();
        panel.open(
            tab,
            smol::unblock(|| Ok(log())),
            unwritten,
            ClockFormat::default(),
        );
        panel
    }

    fn loaded() -> AutopilotPanel {
        let mut panel = opened(PanelTab::Log, Vec::new());
        panel.finish_loading();
        panel
    }

    fn draw(panel: &mut AutopilotPanel, width: u16) -> String {
        let limits = AutopilotConfig::default();
        let content = PanelContent {
            stored: None,
            limits: &limits,
            goal: None,
            model: None,
            deciding: false,
            escalations: &[],
        };
        let mut terminal = Terminal::new(TestBackend::new(width, HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                panel.view(frame, frame.area(), &content);
            })
            .unwrap();
        buffer_text(terminal.backend().buffer())
    }

    fn press(panel: &mut AutopilotPanel, code: KeyCode) -> Option<PanelAction> {
        panel.handle_key(key(code))
    }

    fn selected(panel: &AutopilotPanel) -> Option<u64> {
        panel.selected_entry().map(|decision| decision.seq)
    }

    fn listed(panel: &AutopilotPanel) -> Vec<u64> {
        panel
            .rows()
            .into_iter()
            .filter_map(|row| panel.entry(row).map(|decision| decision.seq))
            .collect()
    }

    fn written(seq: u64) -> AutopilotWrite {
        AutopilotWrite::Decision(Box::new(logged(seq, SECOND)))
    }

    #[test_case(&[], NEWER ; "the_newest_entry_first")]
    #[test_case(&[KeyCode::Char(DOWN_KEY)], OLDER ; "j_steps_over_the_header")]
    #[test_case(&[KeyCode::Down, KeyCode::Down], OLDER ; "down_stops_at_the_last")]
    #[test_case(&[KeyCode::Down, KeyCode::Char(UP_KEY)], NEWER ; "k_steps_back")]
    #[test_case(&[KeyCode::End], OLDER ; "end")]
    #[test_case(&[KeyCode::End, KeyCode::Home], NEWER ; "home")]
    fn the_cursor_moves_between_entries(keys: &[KeyCode], expected: u64) {
        let mut panel = loaded();
        draw(&mut panel, WIDE);

        for code in keys {
            press(&mut panel, *code);
        }

        assert_eq!(selected(&panel), Some(expected), "{HEADERS_SKIPPED}");
    }

    #[test]
    fn tab_and_digits_switch_tabs() {
        let mut panel = opened(PanelTab::Status, Vec::new());

        press(&mut panel, KeyCode::Tab);
        assert_eq!(panel.tab(), PanelTab::Log);
        press(&mut panel, KeyCode::Tab);
        assert_eq!(panel.tab(), PanelTab::Status);
        press(&mut panel, KeyCode::Char('2'));
        assert_eq!(panel.tab(), PanelTab::Log);
        press(&mut panel, KeyCode::BackTab);
        assert_eq!(panel.tab(), PanelTab::Status);
    }

    #[test]
    fn a_narrow_panel_shows_the_list_then_the_detail() {
        let mut panel = loaded();
        draw(&mut panel, NARROW);
        assert!(panel.list_area().width > 0, "{ONE_PANE}");
        assert_eq!(panel.body_area().width, 0, "{ONE_PANE}");

        assert_eq!(press(&mut panel, KeyCode::Enter), None);
        let shown = draw(&mut panel, NARROW);
        assert_eq!(panel.pane(), Pane::Detail);
        assert_eq!(panel.list_area().width, 0, "{ONE_PANE}");
        assert!(shown.contains(REASON_HEADING), "{shown}");

        press(&mut panel, KeyCode::Esc);
        assert_eq!(panel.pane(), Pane::List);
        assert!(panel.is_open());
        press(&mut panel, KeyCode::Esc);
        assert!(!panel.is_open());
    }

    #[test]
    fn enter_jumps_when_both_panes_show() {
        let mut panel = loaded();
        let shown = draw(&mut panel, WIDE);

        assert!(panel.list_area().width > 0 && panel.body_area().width > 0);
        assert!(shown.contains(REASON_HEADING), "{shown}");
        assert_eq!(
            press(&mut panel, KeyCode::Enter),
            Some(PanelAction::Jump(NEWER))
        );
    }

    #[test]
    fn copy_takes_the_selected_entry_as_text() {
        let mut panel = loaded();

        let Some(PanelAction::Copy(text)) = press(&mut panel, KeyCode::Char(COPY_KEY)) else {
            panic!("a selected entry is copied");
        };

        assert!(text.contains(SUBJECT) && text.contains(REASON), "{text}");
    }

    #[test]
    fn copy_with_nothing_loaded_says_so() {
        let mut panel = opened(PanelTab::Log, Vec::new());

        assert_eq!(
            press(&mut panel, KeyCode::Char(COPY_KEY)),
            Some(PanelAction::Flash(NOTHING_TO_COPY))
        );
    }

    #[test]
    fn writes_before_during_and_after_the_read_all_land() {
        let mut panel = opened(PanelTab::Log, vec![written(LANDED)]);
        panel.note(&written(LANDED_LATER));
        panel.finish_loading();
        panel.note(&AutopilotWrite::Settle {
            seq: LANDED_LATER,
            outcome: DecisionOutcome::TakenOver,
        });

        assert_eq!(listed(&panel), [LANDED_LATER, LANDED, NEWER, OLDER]);
        assert_eq!(
            panel
                .selected_entry()
                .and_then(|decision| decision.outcome.clone()),
            Some(DecisionOutcome::TakenOver)
        );
    }

    #[test]
    fn a_row_landing_above_keeps_the_chosen_entry_selected() {
        let mut panel = loaded();
        press(&mut panel, KeyCode::Down);

        panel.note(&written(LANDED));

        assert_eq!(selected(&panel), Some(OLDER));
    }

    #[test]
    fn a_filter_with_nothing_to_list_says_so() {
        let mut panel = loaded();

        press(&mut panel, KeyCode::Char(FILTER_KEY));
        let shown = draw(&mut panel, WIDE);

        assert_eq!(panel.filter(), LogFilter::LeftToYou);
        assert!(shown.contains(NOTHING_MATCHES), "{shown}");
        assert_eq!(selected(&panel), None);
    }

    #[test]
    fn an_escalation_is_listed_under_left_to_you() {
        let escalated = StoredAutopilotDecision {
            action: Some(ActionName::Escalate.name().to_owned()),
            ..logged(LANDED, SECOND)
        };
        let mut panel = loaded();
        panel.note(&AutopilotWrite::Decision(Box::new(escalated)));

        press(&mut panel, KeyCode::Char(FILTER_KEY));

        assert_eq!(listed(&panel), [LANDED]);
    }

    #[test_case(FooterCommand::Filter, PanelTab::Log, LogFilter::LeftToYou ; "filter")]
    #[test_case(FooterCommand::Section, PanelTab::Status, LogFilter::All ; "section")]
    fn a_footer_click_runs_its_command(command: FooterCommand, tab: PanelTab, filter: LogFilter) {
        let mut panel = loaded();
        draw(&mut panel, WIDE);
        let hit = panel.footer_hit(command);

        let actions = [MouseEventKind::Down, MouseEventKind::Up].map(|kind| {
            panel.handle_mouse(MouseEvent {
                kind: kind(MouseButton::Left),
                column: hit.x,
                row: hit.y,
                modifiers: KeyModifiers::NONE,
            })
        });

        assert_eq!(actions, [None, None]);
        assert_eq!((panel.tab(), panel.filter()), (tab, filter));
    }

    #[test]
    fn the_loading_panel_keeps_the_loop_ticking() {
        let mut panel = opened(PanelTab::Status, Vec::new());
        assert_ne!(panel.cadence(), Cadence::IDLE);

        panel.finish_loading();

        assert_eq!(panel.cadence(), Cadence::IDLE);
    }
}
