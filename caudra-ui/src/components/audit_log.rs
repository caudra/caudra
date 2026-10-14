//! What the autopilot and goal panels share: a Status tab beside a Log tab
//! listing what the session's audit log keeps, grouped and filtered, next to
//! the detail of the entry under the cursor. The log is read off the UI
//! thread each time the panel opens, and what the session writes while it
//! stays open lands in it as written.

use std::mem;

use caudra_config::ClockFormat;
use caudra_grab::grab_scope;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use smol::Task;
use unicode_width::UnicodeWidthStr;

use crate::components::command_text::ellipsize_spans;
use crate::components::modal::{FooterHits, FooterLine, Modal, SEPARATOR};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::section_tabs::{SectionTab, tab_strip};
use crate::components::{
    ModalScroll, Overlay, ascii_key, escape_terminal_controls, hover_style, plain_char,
};
use crate::repaint::{Cadence, Dirty};
use crate::theme::{self, Theme};

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
const SECTION_GAP: &str = "  ";
const LINE_BREAK: &str = "\n";
const KEY_GAP: &str = " ";
const INDENT: &str = "  ";
const LABEL_COLS: usize = 12;
const LIST_SCOPE: &str = "audit_log_list";
const BODY_SCOPE: &str = "audit_log_body";
pub(crate) const DATE_FORMAT: &str = "%Y-%m-%d ";
pub(crate) const FILTER_PREFIX: &str = "Filter: ";
pub(crate) const CLOSE_LABEL: &str = "q";
pub(crate) const FILTER_LABEL: &str = "f";
pub(crate) const COPY_LABEL: &str = "y";
pub(crate) const DOWN_LABEL: &str = "j";
pub(crate) const UP_LABEL: &str = "k";
pub(crate) const FILTER_KEY: char = ascii_key(FILTER_LABEL);
pub(crate) const COPY_KEY: char = ascii_key(COPY_LABEL);
pub(crate) const DOWN_KEY: char = ascii_key(DOWN_LABEL);
pub(crate) const UP_KEY: char = ascii_key(UP_LABEL);
const CLOSE_KEY: char = ascii_key(CLOSE_LABEL);
pub(crate) const SECTION_GLOSS: &str = "Section";
pub(crate) const FILTER_GLOSS: &str = "Filter";
pub(crate) const COPY_GLOSS: &str = "Copy";
pub(crate) const JUMP_GLOSS: &str = "Jump";
pub(crate) const CLOSE_GLOSS: &str = "Close";
pub(crate) const COPIED_ENTRY: &str = "Copied log entry";
pub(crate) const NOTHING_TO_COPY: &str = "Nothing to copy";
pub(crate) const NOTHING_MATCHES: &str = "Nothing matches this filter";
pub(crate) const ROW_NOT_LOADED: &str = "That entry's row is not in the loaded transcript";
/// Glossed, then keys alone, then keys packed: a footer too wide for its row
/// answers no clicks, so it gives up words before it gives up a key.
const FOOTER_RUNGS: [(bool, &str); 3] = [(true, SEPARATOR), (false, SECTION_GAP), (false, KEY_GAP)];

/// The read of a session's log a panel waits on.
pub(crate) type LogLoad<L> = Task<Result<L, String>>;
/// A footer command: its key, its gloss, and what it does. An empty gloss
/// leaves a key that names itself, such as a session command, unglossed.
pub(crate) type FooterEntry<C> = (&'static str, &'static str, FooterCommand<C>);

/// A session's audit log as a panel lists it.
pub(crate) trait AuditLog: Sized + Send + 'static {
    /// One change the session makes to the log.
    type Write: Clone;
    type Entry;
    type Filter: EntryFilter;
    /// What a header heads.
    type Group: Copy;
    const TITLE: &'static str;
    const SCOPE: &'static str;
    const READING: &'static str;
    const EMPTY: &'static str;

    /// Takes in what the session wrote, as the database does.
    fn apply(&mut self, write: &Self::Write);
    fn is_empty(&self) -> bool;
    /// The list under `filter`: each group's header, then its entries.
    fn rows(&self, filter: Self::Filter) -> Vec<LogRow<Self::Group>>;
    fn entry(&self, index: usize) -> Option<&Self::Entry>;
    /// The entry's seq, which finds it again and its transcript row.
    fn seq(entry: &Self::Entry) -> u64;
    /// `entries` counts the whole group, whatever the filter lists.
    fn header(&self, group: Self::Group, entries: usize, clock: ClockFormat) -> String;
    fn entry_spans(entry: &Self::Entry, clock: ClockFormat, theme: &Theme) -> Vec<Span<'static>>;
    /// Everything the log keeps about the entry, under its title.
    fn entry_detail(
        &self,
        entry: &Self::Entry,
        clock: ClockFormat,
        theme: &Theme,
    ) -> Vec<Line<'static>>;
}

/// Which entries a list shows, cycled by the filter key.
pub(crate) trait EntryFilter: Copy + Eq + Default {
    fn next(self) -> Self;
    fn label(self) -> &'static str;
}

/// One row of the list: a group's header, or an entry by its place in the
/// log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogRow<G> {
    Header { group: G, entries: usize },
    Entry(usize),
}

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

/// What a panel asks the host to do. `C` is what its own commands ask.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PanelAction<C> {
    /// Copy the text, flashing [`COPIED_ENTRY`].
    Copy(String),
    /// Close, and scroll the transcript to the row of the entry with this
    /// seq.
    Jump(u64),
    Flash(&'static str),
    Command(C),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FooterCommand<C> {
    Section,
    Filter,
    Copy,
    Jump,
    Close,
    Panel(C),
}

#[derive(Default)]
enum LogState<L: AuditLog> {
    #[default]
    Closed,
    /// Writes that land while the read is out are applied over what it
    /// returns, so one it missed is not lost.
    Loading {
        task: LogLoad<L>,
        pending: Vec<L::Write>,
    },
    Ready(L),
    Failed(String),
}

/// A tabbed panel over the log `L`, whose own footer commands are `C`. Each
/// panel adds its keys and its Status tab under its own name.
pub(crate) struct AuditPanel<L: AuditLog, C: 'static> {
    open: bool,
    tab: PanelTab,
    pane: Pane,
    filter: L::Filter,
    /// The entry the Log shows, by seq, so live rows landing above it leave
    /// it selected.
    selected: Option<u64>,
    log: LogState<L>,
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
    /// The footer last drawn. A click is answered from the table that
    /// produced its hits.
    footer: &'static [FooterEntry<C>],
}

impl<L: AuditLog, C> Default for AuditPanel<L, C> {
    fn default() -> Self {
        Self {
            open: false,
            tab: PanelTab::Status,
            pane: Pane::List,
            filter: L::Filter::default(),
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
            footer: &[],
        }
    }
}

impl<L: AuditLog, C: Copy> AuditPanel<L, C> {
    /// Opens on `tab`, with the session's log arriving from `load`, the
    /// changes it may not hold yet in `unwritten`, and its times read on
    /// `clock`.
    pub(crate) fn open(
        &mut self,
        tab: PanelTab,
        load: LogLoad<L>,
        unwritten: Vec<L::Write>,
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
    pub(crate) fn note(&mut self, write: &L::Write) {
        match &mut self.log {
            LogState::Loading { pending, .. } => pending.push(write.clone()),
            LogState::Ready(log) => log.apply(write),
            LogState::Closed | LogState::Failed(_) => {}
        }
    }

    pub(crate) fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    pub(crate) fn tab(&self) -> PanelTab {
        self.tab
    }

    /// The keys every panel shares. Every key is answered here, so a stray
    /// one never reaches the transcript under the panel.
    pub(crate) fn answer_key(&mut self, key: KeyEvent) -> Option<PanelAction<C>> {
        match (key.code, plain_char(&key)) {
            (KeyCode::Esc, _) => self.back(),
            (_, Some(CLOSE_KEY)) => self.close(),
            (KeyCode::Tab, _) => self.set_tab(self.tab.step(1)),
            (KeyCode::BackTab, _) => self.set_tab(self.tab.step(-1)),
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

    fn handle_log_key(&mut self, key: KeyEvent) -> Option<PanelAction<C>> {
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

    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> Option<PanelAction<C>> {
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

    /// Draws the panel with `status` as its Status tab and `footer` as the
    /// commands under the tab showing.
    pub(crate) fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        status: Vec<Line<'static>>,
        footer: &'static [FooterEntry<C>],
    ) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!(L::SCOPE, area);
        let theme = theme::current();
        let modal = Modal {
            title: L::TITLE,
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
        let [header, body, footer_area] = Layout::vertical([
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
                self.render_body(frame, body, status);
            }
            PanelTab::Log => self.render_log(frame, body, &theme),
        }
        self.footer = footer;
        self.render_footer(frame, footer_area, &theme);
        popup
    }

    #[cfg(test)]
    pub(crate) fn pane(&self) -> Pane {
        self.pane
    }

    #[cfg(test)]
    pub(crate) fn filter(&self) -> L::Filter {
        self.filter
    }

    #[cfg(test)]
    pub(crate) fn is_loaded(&self) -> bool {
        matches!(self.log, LogState::Ready(_))
    }

    #[cfg(test)]
    pub(crate) fn footer_hit(&self, command: FooterCommand<C>) -> Rect
    where
        C: PartialEq,
    {
        self.footer
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

    #[cfg(test)]
    pub(crate) fn body_offset(&self) -> u16 {
        self.body_scroll.offset()
    }

    /// The seqs the list shows, top to bottom.
    #[cfg(test)]
    pub(crate) fn listed(&self) -> Vec<u64> {
        self.rows()
            .into_iter()
            .filter_map(|row| self.entry(row).map(L::seq))
            .collect()
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

    fn ready(&self) -> Option<&L> {
        match &self.log {
            LogState::Ready(log) => Some(log),
            LogState::Closed | LogState::Loading { .. } | LogState::Failed(_) => None,
        }
    }

    fn rows(&self) -> Vec<LogRow<L::Group>> {
        self.ready()
            .map(|log| log.rows(self.filter))
            .unwrap_or_default()
    }

    /// The entry a row lists, `None` for a header.
    fn entry(&self, row: LogRow<L::Group>) -> Option<&L::Entry> {
        match row {
            LogRow::Entry(index) => self.ready()?.entry(index),
            LogRow::Header { .. } => None,
        }
    }

    /// The row the cursor is on: the selected entry while it is listed, the
    /// first entry otherwise. Never a header.
    fn cursor(&self, rows: &[LogRow<L::Group>]) -> Option<usize> {
        let mut entries = rows
            .iter()
            .enumerate()
            .filter_map(|(at, row)| self.entry(*row).map(|entry| (at, L::seq(entry))));
        let first = entries.next();
        first
            .into_iter()
            .chain(entries)
            .find(|(_, seq)| Some(*seq) == self.selected)
            .or(first)
            .map(|(at, _)| at)
    }

    pub(crate) fn selected_entry(&self) -> Option<&L::Entry> {
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
            .filter_map(|row| self.entry(*row).map(L::seq))
            .collect();
        let Some(last) = listed.len().checked_sub(1) else {
            return;
        };
        let current = self
            .cursor(&rows)
            .and_then(|row| self.entry(rows[row]))
            .and_then(|entry| listed.iter().position(|seq| *seq == L::seq(entry)))
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
            .map(L::seq);
        if let Some(seq) = seq {
            self.select(seq);
        }
    }

    /// A narrow list opens the entry's detail; otherwise the transcript is
    /// scrolled to its row.
    fn enter(&mut self) -> Option<PanelAction<C>> {
        let seq = L::seq(self.selected_entry()?);
        if self.single_pane() && self.pane == Pane::List {
            self.select(seq);
            self.pane = Pane::Detail;
            return None;
        }
        Some(PanelAction::Jump(seq))
    }

    /// The selected entry as plain text, its title first.
    fn copy(&self) -> PanelAction<C> {
        let Some((log, entry)) = self.ready().zip(self.selected_entry()) else {
            return PanelAction::Flash(NOTHING_TO_COPY);
        };
        let lines = log.entry_detail(entry, self.clock, &theme::current());
        PanelAction::Copy(plain_text(&lines))
    }

    fn footer_command(&mut self, index: usize) -> Option<PanelAction<C>> {
        match self.footer.get(index).map(|(_, _, command)| *command)? {
            FooterCommand::Section => self.set_tab(self.tab.step(1)),
            FooterCommand::Filter => self.cycle_filter(),
            FooterCommand::Copy => return Some(self.copy()),
            FooterCommand::Jump => {
                return self
                    .selected_entry()
                    .map(|entry| PanelAction::Jump(L::seq(entry)));
            }
            FooterCommand::Close => self.close(),
            FooterCommand::Panel(command) => return Some(PanelAction::Command(command)),
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
        let lines = self
            .ready()
            .zip(cursor.and_then(|row| self.entry(rows[row])))
            .map(|(log, entry)| log.entry_detail(entry, self.clock, theme))
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
        rows: &[LogRow<L::Group>],
        cursor: Option<usize>,
        theme: &Theme,
    ) {
        grab_scope!(LIST_SCOPE, area);
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
        rows: &[LogRow<L::Group>],
        cursor: Option<usize>,
        area: Rect,
        offset: u16,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let log = match &self.log {
            LogState::Ready(log) => log,
            LogState::Closed => return Vec::new(),
            LogState::Loading { .. } => return vec![hint(L::READING, theme)],
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
                    L::EMPTY
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
                LogRow::Header { group, entries } => Line::from(ellipsize_spans(
                    vec![Span::styled(
                        log.header(group, entries, self.clock),
                        theme.keybind_section,
                    )],
                    width,
                )),
                LogRow::Entry(index) => {
                    let selected = Some(at) == cursor;
                    let mark = if selected { CURSOR_MARK } else { NO_MARK };
                    let mut spans = vec![Span::styled(mark, theme.accent)];
                    spans.extend(
                        log.entry(index)
                            .map(|entry| L::entry_spans(entry, self.clock, theme))
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
        grab_scope!(BODY_SCOPE, area);
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
        for (index, (key, description, command)) in self.footer.iter().enumerate() {
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
            if glossed && !description.is_empty() {
                footer.describe(format!(" {description}"), theme.tool_dim);
            }
        }
        footer
    }
}

impl<L: AuditLog, C> Overlay for AuditPanel<L, C> {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        *self = Self::default();
    }

    /// The loop must keep ticking for [`AuditPanel::poll`] to see the read
    /// end.
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

/// A dim line standing in for what a list or field cannot show.
pub(crate) fn hint(text: &str, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::raw(INDENT),
        Span::styled(text.to_owned(), theme.tool_dim),
    ])
}

/// A detail section: a blank line, its heading, then `body`.
pub(crate) fn section(
    lines: &mut Vec<Line<'static>>,
    heading: &'static str,
    body: Vec<Line<'static>>,
    theme: &Theme,
) {
    lines.push(Line::default());
    lines.push(Line::styled(heading, theme.keybind_section));
    lines.extend(body);
}

/// Text that may run over several lines, each indented and escaped.
pub(crate) fn text_lines(text: &str, style: Style) -> Vec<Line<'static>> {
    text.lines()
        .map(|line| {
            Line::from(vec![
                Span::raw(INDENT),
                Span::styled(escape_terminal_controls(line), style),
            ])
        })
        .collect()
}

/// A detail field: its label in a column, then `value`.
pub(crate) fn field(label: &str, value: String, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{INDENT}{label:<LABEL_COLS$}"), theme.tool_dim),
        Span::raw(value),
    ])
}

/// Unix milliseconds in the local zone, by `format`.
pub(crate) fn local_time(at_ms: u64, format: &str) -> String {
    i64::try_from(at_ms)
        .ok()
        .and_then(|millis| Timestamp::from_millisecond(millis).ok())
        .map(|at| at.to_zoned(TimeZone::system()).strftime(format).to_string())
        .unwrap_or_else(|| at_ms.to_string())
}

/// `count` and the word for it, `one` when it is 1.
pub(crate) fn counted(count: impl Into<u64>, one: &str, many: &str) -> String {
    let count = count.into();
    format!("{count} {}", if count == 1 { one } else { many })
}
