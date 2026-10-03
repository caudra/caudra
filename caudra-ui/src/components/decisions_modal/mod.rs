//! `/decisions`: what the decision engine is set to, how it is doing, and
//! what it decided. Overview and Features read the configuration and the
//! service's cached health on every frame. Activity and Recent read
//! `decisions.db`, which a background task loads for every scope at once.
//! Nothing here changes the engine or contacts it.

mod lines;

use std::path::Path;

use arc_swap::ArcSwapOption;
use caudra_agent::decisions::{DecisionFeature, DecisionStatus};
use caudra_config::ClockFormat;
use caudra_config::decisions::DecisionsConfig;
use caudra_grab::grab_scope;
use caudra_storage::decision_log::{DecisionStats, LoggedDecision};
use caudra_storage::sessions::PermissionMode;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::components::command_text::ellipsize_spans;
use crate::components::keybindings::key;
use crate::components::modal::{ESC_LABEL, FooterHits, FooterLine, Modal, SEPARATOR};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::section_tabs::{SectionTab, tab_strip};
use crate::components::{ModalScroll, Overlay, PAN_STEP, bar_area, hover_style, now_secs};
use crate::repaint::{Dirty, Watch};
use crate::theme::{self, Theme};

pub(crate) const TITLE: &str = " Decisions ";
pub(crate) const COPY_LABEL: &str = "y";
pub(crate) const COPIED_DECISION: &str = "Copied decision";
pub(crate) const COPIED_SECTION: &str = "Copied section";
pub(crate) const NOTHING_TO_COPY: &str = "Nothing to copy";
/// The newest rows Recent lists per scope. Each row's state is capped, so the
/// whole list loads with the stats and a selection never waits on the disk.
pub(crate) const RECENT_LIMIT: usize = 200;
const COPY_KEY: char = COPY_LABEL.as_bytes()[0] as char;
const WIDTH_PERCENT: u16 = 90;
const MAX_HEIGHT_PERCENT: u16 = 85;
const H_PAD: u16 = 1;
const PANE_GAP: u16 = 1;
const LIST_PERCENT: u32 = 50;
const PERCENT: u32 = 100;
/// A list narrower than this cuts a feature's name, and a detail narrower
/// than this wraps every field, so a body too narrow for both shows one.
const LIST_MIN_COLS: u16 = 28;
const DETAIL_MIN_COLS: u16 = 40;
const SPLIT_MIN_COLS: u16 = LIST_MIN_COLS + PANE_GAP + DETAIL_MIN_COLS;
const CURSOR_MARK: &str = "\u{203a} ";
const NO_MARK: &str = "  ";
/// A blank column at the end of every list row, so the widest row never reads
/// on into the detail beside it.
const LIST_GUTTER: usize = 1;
const SCOPE_PREFIX: &str = "Scope: ";
const AS_OF_PREFIX: &str = "as of ";
const INFO_GAP: &str = "  ";
const SECTION_GAP: &str = "  ";
const KEY_GAP: &str = " ";
const FOOTER: [(&str, &str, FooterCommand); 5] = [
    (key::TAB.label, "Section", FooterCommand::Section),
    (key::SCOPE.label, "Scope", FooterCommand::Scope),
    (COPY_LABEL, "Copy", FooterCommand::Copy),
    (key::REFRESH.label, "Refresh", FooterCommand::Refresh),
    (ESC_LABEL, "Close", FooterCommand::Close),
];
/// Glossed, then keys alone, then keys packed: a footer too wide for its row
/// answers no clicks, so it gives up words before it gives up a key.
const FOOTER_RUNGS: [(bool, &str); 3] = [(true, SEPARATOR), (false, SECTION_GAP), (false, KEY_GAP)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Overview,
    Features,
    Activity,
    Recent,
}

impl SectionTab for Tab {
    const ALL: &'static [Self] = &[Self::Overview, Self::Features, Self::Activity, Self::Recent];

    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Features => "Features",
            Self::Activity => "Activity",
            Self::Recent => "Recent",
        }
    }
}

impl Tab {
    /// Whether the tab reads the log, and so answers to the scope.
    fn scoped(self) -> bool {
        self != Self::Overview
    }

    fn listed(self) -> bool {
        matches!(self, Self::Features | Self::Recent)
    }
}

/// Which rows of the log the scoped tabs count: this session's, this
/// project's, or every row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecisionsScope {
    #[default]
    Session,
    Project,
    All,
}

impl DecisionsScope {
    pub const ALL: [Self; 3] = [Self::Session, Self::Project, Self::All];

    fn next(self) -> Self {
        match self {
            Self::Session => Self::Project,
            Self::Project => Self::All,
            Self::All => Self::Session,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Session => "Session",
            Self::Project => "Project",
            Self::All => "All",
        }
    }

    fn empty(self) -> &'static str {
        match self {
            Self::Session => lines::NO_SESSION_DECISIONS,
            Self::Project => lines::NO_PROJECT_DECISIONS,
            Self::All => lines::NO_DECISIONS,
        }
    }

    /// Where the scope's rows sit in [`DecisionsSnapshot::scopes`].
    pub(crate) fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pane {
    List,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterCommand {
    Section,
    Scope,
    Copy,
    Refresh,
    Close,
}

/// What the background read of `decisions.db` produced.
pub enum DecisionsFetchState {
    Loading,
    /// Nothing has been logged on this machine yet.
    Missing,
    Ready(Box<DecisionsSnapshot>),
    Failed(String),
}

/// Every scope's rows, read together so `g` never waits on the disk and never
/// shows one scope's stats beside another's rows.
pub struct DecisionsSnapshot {
    pub scopes: [ScopeActivity; DecisionsScope::ALL.len()],
    /// The session the Session scope was read for.
    pub session: String,
    pub loaded_at: String,
}

#[derive(Default)]
pub struct ScopeActivity {
    pub stats: Vec<DecisionStats>,
    pub recent: Vec<LoggedDecision>,
}

/// What the log holds for the scope on screen.
#[derive(Clone, Copy)]
enum Logged<'a> {
    Loading,
    Missing,
    Failed(&'a str),
    Ready {
        activity: &'a ScopeActivity,
        session: &'a str,
    },
}

/// What the live tabs read on every frame. None of it touches the disk or
/// the endpoint.
pub struct DecisionsModalContext<'a> {
    pub config: &'a DecisionsConfig,
    pub status: &'a DecisionStatus,
    pub tainted: bool,
    pub mode: &'a PermissionMode,
    pub api_key_set: bool,
    pub log_path: &'a Path,
    pub clock: ClockFormat,
}

#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub enum DecisionsAction {
    Consumed,
    Close,
    /// Read the log again.
    Refresh,
    Copy {
        text: String,
        label: &'static str,
    },
    Flash(&'static str),
}

pub struct DecisionsModal {
    open: bool,
    tab: Tab,
    scope: DecisionsScope,
    pane: Pane,
    /// The feature the Features tab shows, by its place in
    /// [`DecisionFeature::ALL`].
    feature: usize,
    /// The decision Recent shows, by id, so a refresh that lands newer rows
    /// above it leaves it selected.
    selected: Option<i64>,
    list_scroll: ModalScroll,
    body_scroll: ModalScroll,
    list_bar: Scrollbar,
    body_bar: Scrollbar,
    /// Activity is a table as wide as its numbers, which owe nothing to the
    /// terminal, so a narrow modal pans to reach its right-hand columns.
    pan_bar: Scrollbar,
    popup: Rect,
    list_area: Rect,
    body_area: Rect,
    tab_hits: Vec<(Rect, Tab)>,
    /// Where the pointer last was, so each part answers for its own geometry
    /// on the frame it is drawn.
    pointer: Option<Position>,
    /// A key moved the list cursor, so the next draw scrolls it into view.
    reveal_cursor: bool,
    /// The body as last drawn, which is what `y` copies off every tab but
    /// Recent.
    drawn: Vec<Line<'static>>,
    footer_hits: FooterHits,
    fetch: Watch<DecisionsFetchState>,
}

impl DecisionsModal {
    pub fn new() -> Self {
        Self {
            open: false,
            tab: Tab::Overview,
            scope: DecisionsScope::default(),
            pane: Pane::List,
            feature: 0,
            selected: None,
            list_scroll: ModalScroll::new_top(),
            body_scroll: ModalScroll::new_top(),
            list_bar: Scrollbar::default(),
            body_bar: Scrollbar::default(),
            pan_bar: Scrollbar::horizontal(),
            popup: Rect::default(),
            list_area: Rect::default(),
            body_area: Rect::default(),
            tab_hits: Vec::new(),
            pointer: None,
            reveal_cursor: false,
            drawn: Vec::new(),
            footer_hits: FooterHits::default(),
            fetch: Watch::default(),
        }
    }

    pub fn open(&mut self) {
        *self = Self {
            open: true,
            ..Self::new()
        };
    }

    pub fn close(&mut self) {
        *self = Self::new();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    /// Picks up a finished read. Nothing wakes the loop when the task stores
    /// its result, so an unpolled modal sits on loading.
    pub fn poll(&mut self, slot: &ArcSwapOption<DecisionsFetchState>) -> Dirty {
        if !self.open {
            return Dirty::NO;
        }
        self.fetch.poll(slot.load_full())
    }

    /// Every key is answered here, so a stray one never closes the modal.
    pub fn handle_key(&mut self, key: KeyEvent) -> DecisionsAction {
        if key.code == KeyCode::Esc || key::QUIT.matches(key) {
            return DecisionsAction::Close;
        }
        if key::REFRESH.matches(key) {
            return DecisionsAction::Refresh;
        }
        if key::SCOPE.matches(key) {
            self.cycle_scope();
            return DecisionsAction::Consumed;
        }
        let plain = key.modifiers.is_empty();
        match key.code {
            KeyCode::Tab => self.set_tab(self.tab.step(1)),
            KeyCode::BackTab => self.set_tab(self.tab.step(-1)),
            KeyCode::Char(COPY_KEY) if plain => return self.copy(),
            KeyCode::Char(digit) if plain && digit.is_ascii_digit() => {
                if let Some(tab) = Tab::from_digit(digit) {
                    self.set_tab(tab);
                }
            }
            KeyCode::Left if plain => self.pane = Pane::List,
            KeyCode::Right if plain => self.pane = Pane::Detail,
            _ if self.tab.listed() && self.pane == Pane::List && self.step_by_key(key) => {}
            _ => {
                self.body_scroll.handle_key(key);
            }
        }
        DecisionsAction::Consumed
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> DecisionsAction {
        let pos = Position::new(event.column, event.row);
        self.pointer = Some(pos);
        if let Some(index) = self.footer_hits.handle_mouse(event) {
            return self.footer_command(index);
        }
        if let Some(moved) = bar_event(&mut self.list_bar, &event) {
            if let Some(top) = moved {
                self.list_scroll.scroll_to(top);
            }
            return DecisionsAction::Consumed;
        }
        if let Some(moved) = bar_event(&mut self.body_bar, &event) {
            if let Some(top) = moved {
                self.body_scroll.scroll_to(top);
            }
            return DecisionsAction::Consumed;
        }
        if let Some(moved) = bar_event(&mut self.pan_bar, &event) {
            if let Some(column) = moved {
                self.body_scroll.pan_to(column);
            }
            return DecisionsAction::Consumed;
        }
        match event.kind {
            MouseEventKind::ScrollLeft => self.pan(-PAN_STEP),
            MouseEventKind::ScrollRight => self.pan(PAN_STEP),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(&(_, tab)) = self.tab_hits.iter().find(|(hit, _)| hit.contains(pos)) {
                    self.set_tab(tab);
                } else if self.list_area.contains(pos) {
                    self.pane = Pane::List;
                    self.select(usize::from(
                        (pos.y - self.list_area.y).saturating_add(self.list_scroll.offset()),
                    ));
                } else if self.body_area.contains(pos) {
                    self.pane = Pane::Detail;
                }
            }
            _ => {}
        }
        DecisionsAction::Consumed
    }

    /// The wheel over the list walks the selection; anywhere else it scrolls
    /// the body.
    pub fn scroll_at(&mut self, pos: Position, delta: i32) {
        match self.list_area.contains(pos) {
            true => self.step(-(delta.signum() as isize)),
            false => self.body_scroll.scroll(delta),
        }
    }

    fn pan(&mut self, delta: i32) {
        self.body_scroll.pan_by(delta);
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect, ctx: &DecisionsModalContext) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("decisions_modal", area);
        let theme = theme::current();
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
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(padded);
        self.popup = popup;

        let fetch = self.fetch.held();
        let logged = logged(fetch.as_deref(), self.scope);
        let now = now_secs();
        if self.tab == Tab::Recent {
            self.settle_selection();
        }
        self.render_header(frame, header, fetch.as_deref(), &theme);
        let rows = self.list_rows(ctx.config, logged, now, &theme);
        let (list, detail) = self.panes(body, &rows);
        self.list_area = list;
        self.body_area = detail;
        match list.width {
            0 => self.list_bar = Scrollbar::default(),
            _ => self.render_list(frame, list, rows, logged, &theme),
        }
        let lines = self.body_lines(ctx, logged, now, &theme);
        match detail.width {
            0 => {
                self.body_bar = Scrollbar::default();
                self.pan_bar = Scrollbar::horizontal();
                self.drawn = lines;
            }
            _ => self.render_body(frame, detail, footer.bottom(), lines),
        }
        self.render_footer(frame, footer, &theme);
        popup
    }

    #[cfg(test)]
    pub(crate) fn tab(&self) -> Tab {
        self.tab
    }

    #[cfg(test)]
    pub(crate) fn scope(&self) -> DecisionsScope {
        self.scope
    }

    #[cfg(test)]
    pub(crate) fn pane(&self) -> Pane {
        self.pane
    }

    #[cfg(test)]
    pub(crate) fn is_loaded(&self) -> bool {
        matches!(self.fetch.get(), Some(DecisionsFetchState::Ready(_)))
    }

    #[cfg(test)]
    pub(crate) fn tab_hit(&self, tab: Tab) -> Option<Rect> {
        self.tab_hits
            .iter()
            .find(|(_, hit)| *hit == tab)
            .map(|(rect, _)| *rect)
    }

    #[cfg(test)]
    pub(crate) fn footer_hit(&self, index: usize) -> Rect {
        self.footer_hits.hit(index)
    }

    fn set_tab(&mut self, tab: Tab) {
        if tab == self.tab {
            return;
        }
        self.tab = tab;
        self.pane = Pane::List;
        self.list_scroll.reset();
        self.body_scroll.reset();
        self.reveal_cursor = true;
    }

    /// Instant, because every scope was read with the last refresh.
    fn cycle_scope(&mut self) {
        self.scope = self.scope.next();
        if self.tab.scoped() {
            self.body_scroll.reset();
            self.reveal_cursor = true;
        }
    }

    fn activity(&self) -> Option<&ScopeActivity> {
        match self.fetch.get()? {
            DecisionsFetchState::Ready(snapshot) => Some(&snapshot.scopes[self.scope.index()]),
            _ => None,
        }
    }

    fn recent(&self) -> &[LoggedDecision] {
        self.activity()
            .map(|activity| activity.recent.as_slice())
            .unwrap_or_default()
    }

    fn list_len(&self) -> usize {
        match self.tab {
            Tab::Features => DecisionFeature::ALL.len(),
            Tab::Recent => self.recent().len(),
            Tab::Overview | Tab::Activity => 0,
        }
    }

    fn cursor(&self) -> usize {
        match self.tab {
            Tab::Features => self.feature,
            Tab::Recent => self
                .selected
                .and_then(|id| self.recent().iter().position(|decision| decision.id == id))
                .unwrap_or_default(),
            Tab::Overview | Tab::Activity => 0,
        }
    }

    /// Keeps the selection on a row the scope lists, falling back to the
    /// newest, so the detail and the cursor always name the same decision.
    fn settle_selection(&mut self) {
        let recent = self.recent();
        let settled = self
            .selected
            .filter(|id| recent.iter().any(|decision| decision.id == *id))
            .or_else(|| recent.first().map(|decision| decision.id));
        if settled != self.selected {
            self.selected = settled;
            self.body_scroll.reset();
        }
    }

    fn select(&mut self, index: usize) {
        if index >= self.list_len() || index == self.cursor() {
            return;
        }
        match self.tab {
            Tab::Features => self.feature = index,
            Tab::Recent => self.selected = self.recent().get(index).map(|decision| decision.id),
            Tab::Overview | Tab::Activity => return,
        }
        self.body_scroll.reset();
    }

    fn step(&mut self, delta: isize) {
        let len = self.list_len();
        if len == 0 {
            return;
        }
        self.select(self.cursor().saturating_add_signed(delta).min(len - 1));
        self.reveal_cursor = true;
    }

    fn step_by_key(&mut self, key: KeyEvent) -> bool {
        let page = isize::try_from(self.list_area.height.max(1)).unwrap_or(isize::MAX);
        let delta = match key.code {
            KeyCode::Up => -1,
            KeyCode::Down => 1,
            KeyCode::PageUp => -page,
            KeyCode::PageDown => page,
            KeyCode::Home => isize::MIN,
            KeyCode::End => isize::MAX,
            _ => return false,
        };
        self.step(delta);
        true
    }

    fn copy(&self) -> DecisionsAction {
        if self.tab == Tab::Recent {
            return match self
                .recent()
                .get(self.cursor())
                .and_then(|decision| serde_json::to_string_pretty(decision).ok())
            {
                Some(text) => DecisionsAction::Copy {
                    text,
                    label: COPIED_DECISION,
                },
                None => DecisionsAction::Flash(NOTHING_TO_COPY),
            };
        }
        let text = plain_text(&self.drawn);
        match text.trim().is_empty() {
            true => DecisionsAction::Flash(NOTHING_TO_COPY),
            false => DecisionsAction::Copy {
                text,
                label: COPIED_SECTION,
            },
        }
    }

    fn footer_command(&mut self, index: usize) -> DecisionsAction {
        match FOOTER.get(index).map(|(_, _, command)| *command) {
            Some(FooterCommand::Section) => self.set_tab(self.tab.step(1)),
            Some(FooterCommand::Scope) => self.cycle_scope(),
            Some(FooterCommand::Copy) => return self.copy(),
            Some(FooterCommand::Refresh) => return DecisionsAction::Refresh,
            Some(FooterCommand::Close) => return DecisionsAction::Close,
            None => {}
        }
        DecisionsAction::Consumed
    }

    /// The list and the detail side by side, or the focused one alone when
    /// the body cannot hold both. A hidden pane keeps a zero area, so nothing
    /// draws into it and the pointer cannot land on it.
    fn panes(&self, body: Rect, rows: &[Vec<Span<'static>>]) -> (Rect, Rect) {
        if !self.tab.listed() {
            return (Rect::default(), body);
        }
        if body.width < SPLIT_MIN_COLS {
            return match self.pane {
                Pane::List => (body, Rect::default()),
                Pane::Detail => (Rect::default(), body),
            };
        }
        let natural = rows
            .iter()
            .map(|row| row.iter().map(Span::width).sum::<usize>())
            .max()
            .unwrap_or_default()
            .saturating_add(CURSOR_MARK.width() + LIST_GUTTER);
        let share =
            u16::try_from(u32::from(body.width) * LIST_PERCENT / PERCENT).unwrap_or(u16::MAX);
        let list_width = u16::try_from(natural)
            .unwrap_or(u16::MAX)
            .clamp(LIST_MIN_COLS, share.max(LIST_MIN_COLS));
        let [list, _, detail] = Layout::horizontal([
            Constraint::Length(list_width),
            Constraint::Length(PANE_GAP),
            Constraint::Fill(1),
        ])
        .areas(body);
        (list, detail)
    }

    fn list_rows(
        &self,
        config: &DecisionsConfig,
        logged: Logged<'_>,
        now: u64,
        theme: &Theme,
    ) -> Vec<Vec<Span<'static>>> {
        match (self.tab, logged) {
            (Tab::Features, _) => DecisionFeature::ALL
                .iter()
                .map(|feature| lines::feature_row(feature, config, theme))
                .collect(),
            (Tab::Recent, Logged::Ready { activity, .. }) => activity
                .recent
                .iter()
                .map(|decision| lines::recent_row(decision, now, theme))
                .collect(),
            _ => Vec::new(),
        }
    }

    fn render_header(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        fetch: Option<&DecisionsFetchState>,
        theme: &Theme,
    ) {
        let (strip, hits) = tab_strip(self.tab, self.pointer, area);
        self.tab_hits = hits;
        let room = usize::from(area.width).saturating_sub(strip.width() + INFO_GAP.width());
        frame.render_widget(Paragraph::new(strip), area);

        let scope = match self.tab.scoped() {
            true => vec![
                Span::styled(SCOPE_PREFIX, theme.tool_dim),
                Span::styled(self.scope.label(), theme.accent),
            ],
            false => Vec::new(),
        };
        let mut info = scope.clone();
        if let Some(DecisionsFetchState::Ready(snapshot)) = fetch {
            if !info.is_empty() {
                info.push(Span::raw(INFO_GAP));
            }
            info.push(Span::styled(
                format!("{AS_OF_PREFIX}{}", snapshot.loaded_at),
                theme.tool_dim,
            ));
        }
        let width = |spans: &[Span]| spans.iter().map(Span::width).sum::<usize>();
        if width(&info) > room {
            info = scope;
        }
        let cols = width(&info);
        if cols == 0 || cols > room {
            return;
        }
        let cols = u16::try_from(cols).unwrap_or(u16::MAX);
        frame.render_widget(
            Paragraph::new(Line::from(info)),
            Rect {
                x: area.right().saturating_sub(cols),
                width: cols,
                ..area
            },
        );
    }

    fn render_list(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        rows: Vec<Vec<Span<'static>>>,
        logged: Logged<'_>,
        theme: &Theme,
    ) {
        grab_scope!("decisions_modal_list", area);
        let cursor = self.cursor();
        let width = usize::from(area.width).saturating_sub(LIST_GUTTER);
        // The offset the last frame settled on is the one the pointer was
        // reported against.
        let hovered = self
            .pointer
            .filter(|at| area.contains(*at))
            .map(|at| usize::from((at.y - area.y).saturating_add(self.list_scroll.offset())));
        let lines: Vec<Line<'static>> = match rows.is_empty() {
            true => vec![
                lines::state_line(logged, theme)
                    .unwrap_or_else(|| lines::hint(self.scope.empty(), theme)),
            ],
            false => rows
                .into_iter()
                .enumerate()
                .map(|(index, row)| {
                    let selected = index == cursor;
                    let mark = match selected {
                        true => CURSOR_MARK,
                        false => NO_MARK,
                    };
                    let mut spans = vec![Span::styled(mark, theme.accent)];
                    spans.extend(row);
                    let style = match selected {
                        true => theme.item_selected,
                        false => Style::default(),
                    };
                    Line::from(ellipsize_spans(spans, width))
                        .style(hover_style(style, hovered == Some(index)))
                })
                .collect(),
        };
        let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        self.list_scroll.update_dimensions(total, area.height);
        if self.reveal_cursor {
            self.list_scroll
                .reveal(u16::try_from(cursor).unwrap_or(u16::MAX), 1);
            self.reveal_cursor = false;
        }
        let offset = self.list_scroll.offset();
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((offset, 0)),
            area,
        );
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

    fn body_lines(
        &self,
        ctx: &DecisionsModalContext,
        logged: Logged<'_>,
        now: u64,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        match self.tab {
            Tab::Overview => lines::overview(ctx, theme),
            Tab::Features => DecisionFeature::ALL
                .get(self.feature)
                .map(|feature| {
                    lines::feature_detail(feature, ctx.config, logged, self.scope, now, theme)
                })
                .unwrap_or_default(),
            Tab::Activity => lines::activity(ctx.config, logged, self.scope, now, theme),
            Tab::Recent => {
                let mut body = Vec::new();
                if !ctx.config.log {
                    body.push(Line::styled(lines::LOGGING_HINT, theme.tool_warning));
                    body.push(Line::default());
                }
                if let Logged::Ready { activity, session } = logged
                    && let Some(decision) = activity.recent.get(self.cursor())
                {
                    body.extend(lines::recent_detail(
                        decision, session, ctx.clock, now, theme,
                    ));
                }
                body
            }
        }
    }

    /// `bottom` is the row under the footer, the border the pan bar lies on.
    fn render_body(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        bottom: u16,
        lines: Vec<Line<'static>>,
    ) {
        grab_scope!("decisions_modal_body", area);
        // A table must not wrap: a folded column stops being a column, which
        // is what the pan bar exists to avoid.
        let table = self.tab == Tab::Activity;
        let mut paragraph = Paragraph::new(lines.clone());
        let (total, widest) = match table {
            true => (
                lines.len(),
                lines.iter().map(Line::width).max().unwrap_or_default(),
            ),
            false => {
                paragraph = paragraph.wrap(Wrap { trim: false });
                (paragraph.line_count(area.width.max(1)), 0)
            }
        };
        let total = u16::try_from(total).unwrap_or(u16::MAX);
        let widest = u16::try_from(widest).unwrap_or(u16::MAX);
        self.body_scroll.update_dimensions(total, area.height);
        self.body_scroll.fit_width(widest, area.width);
        let (offset, pan) = (self.body_scroll.offset(), self.body_scroll.pan());
        frame.render_widget(paragraph.scroll((offset, pan)), area);
        self.body_bar.draw(
            frame,
            Rect {
                width: area.width.saturating_add(H_PAD),
                ..area
            },
            total,
            offset,
        );
        match table {
            true => self.pan_bar.draw(
                frame,
                bar_area(Rect {
                    height: bottom.saturating_sub(area.y),
                    ..area
                }),
                widest,
                pan,
            ),
            false => self.pan_bar = Scrollbar::horizontal(),
        }
        self.drawn = lines;
    }

    fn render_footer(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let footer = self.footer_line(area.width, theme);
        self.footer_hits.set(footer.hits(area, 0, 1));
        frame.render_widget(
            Paragraph::new(footer.line(self.footer_hits.hovered())),
            area,
        );
    }

    fn footer_line(&self, width: u16, theme: &Theme) -> FooterLine {
        let [(glossed, gap), rest @ ..] = FOOTER_RUNGS;
        let mut footer = self.commands_footer(glossed, gap, theme);
        for (glossed, gap) in rest {
            if footer.fits(width) {
                break;
            }
            footer = self.commands_footer(glossed, gap, theme);
        }
        footer
    }

    fn commands_footer(&self, glossed: bool, gap: &'static str, theme: &Theme) -> FooterLine {
        let mut footer = FooterLine::default();
        for (index, (key, description, command)) in FOOTER.iter().enumerate() {
            if index > 0 {
                footer.text(gap, Style::default());
            }
            let enabled = match command {
                FooterCommand::Scope => self.tab.scoped(),
                FooterCommand::Copy => self.tab != Tab::Recent || !self.recent().is_empty(),
                FooterCommand::Section | FooterCommand::Refresh | FooterCommand::Close => true,
            };
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

impl Default for DecisionsModal {
    fn default() -> Self {
        Self::new()
    }
}

impl Overlay for DecisionsModal {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        self.close();
    }
}

fn logged(fetch: Option<&DecisionsFetchState>, scope: DecisionsScope) -> Logged<'_> {
    match fetch {
        None | Some(DecisionsFetchState::Loading) => Logged::Loading,
        Some(DecisionsFetchState::Missing) => Logged::Missing,
        Some(DecisionsFetchState::Failed(error)) => Logged::Failed(error),
        Some(DecisionsFetchState::Ready(snapshot)) => Logged::Ready {
            activity: &snapshot.scopes[scope.index()],
            session: &snapshot.session,
        },
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
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use caudra_config::decisions::{DecisionFeatures, FeatureMode};
    use caudra_storage::decision_log::{
        DecisionEffect, DecisionLabel, DecisionRecord, EndpointKind,
    };
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer, Cell};
    use serde_json::{Value, json};
    use test_case::test_case;

    use super::lines::{
        ACTIVITY_FOOTNOTE, ENGINE_CONFIGURED, ERROR_MARK, JSON_SECTIONS, LABEL_MARK, LOGGING_HINT,
        NO_DECISIONS, NO_SESSION_DECISIONS, READING, feature_label, recent_row,
    };
    use super::*;
    use crate::components::{buffer_text, escape_terminal_controls, key as press};

    const SESSION: &str = "session-a";
    const OTHER_SESSION: &str = "session-b";
    const PROJECT: &str = "/work/project";
    const MODEL: &str = "typesafe-test";
    const CONTROL_MODEL: &str = "test-model\n\u{1b}[2J";
    const BASE_URL: &str = "http://127.0.0.1:8080/typesafe";
    const LOG_PATH: &str = "/state/decisions.db";
    const LOADED_AT: &str = "12:34:56";
    const STATE_MARKER: &str = "state-marker";
    const LABEL_KEY: &str = "expected";
    const LABEL_SOURCE: &str = "user";
    const QUESTION_SET_VERSION: &str = "1";
    const FAILURE: &str = "decisions.db is unreadable";
    const ENGINE_ERROR: &str = "unreachable";
    const HIDDEN_URL_PARTS: [&str; 6] = [
        "private-user",
        "private-password",
        "private-path",
        "private-query",
        "private-fragment",
        "\u{1b}",
    ];
    const LOGGED_AT: u64 = 1_700_000_000;
    const LATENCY_MS: u64 = 120;
    const SESSION_ID: i64 = 2;
    const OTHER_ID: i64 = 3;
    const NEWER_ID: i64 = 4;
    const WIDE: u16 = 160;
    const NARROW: u16 = 80;
    const TALL: u16 = 60;
    const SHORT: u16 = 20;
    const WHEEL_DOWN: i32 = -3;
    const DESCRIPTION_PREFIX: usize = 24;
    const NO_HIT: &str = "every footer key must answer a click";
    const NOT_COPIED: &str = "y must copy";

    struct Fixture {
        config: DecisionsConfig,
        status: DecisionStatus,
        mode: PermissionMode,
    }

    impl Fixture {
        fn new(log: bool) -> Self {
            Self {
                config: DecisionsConfig {
                    base_url: Some(BASE_URL.parse().unwrap()),
                    model: MODEL.into(),
                    log,
                    features: DecisionFeatures {
                        permission_advice: FeatureMode::Advise,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                status: DecisionStatus::default(),
                mode: PermissionMode::Ask,
            }
        }

        fn ctx(&self) -> DecisionsModalContext<'_> {
            DecisionsModalContext {
                config: &self.config,
                status: &self.status,
                tainted: false,
                mode: &self.mode,
                api_key_set: false,
                log_path: Path::new(LOG_PATH),
                clock: ClockFormat::default(),
            }
        }
    }

    fn decision(id: i64, feature: &DecisionFeature, session: &str) -> LoggedDecision {
        LoggedDecision {
            id,
            record: DecisionRecord {
                timestamp: LOGGED_AT,
                session: Some(session.into()),
                project: Some(PROJECT.into()),
                feature: feature.name().into(),
                question_set_id: feature.name().into(),
                question_set_version: QUESTION_SET_VERSION.into(),
                endpoint_kind: EndpointKind::Local,
                model: MODEL.into(),
                state: json!({ "marker": STATE_MARKER }),
                questions: json!({ "flag": { "kind": "noul" } }),
                answers: Some(json!({ "flag": 0.2 })),
                error: None,
                latency_ms: LATENCY_MS,
                mode: FeatureMode::Advise.as_str().into(),
                effect: DecisionEffect::Escalated,
                meta: json!({}),
            },
            label: Some(DecisionLabel {
                expected: json!({ "flag": false }),
                source: LABEL_SOURCE.into(),
                timestamp: LOGGED_AT,
                meta: json!({}),
            }),
        }
    }

    fn stats(feature: &DecisionFeature) -> DecisionStats {
        DecisionStats {
            feature: feature.name().into(),
            count: 1,
            error_count: 0,
            error_rate: 0.0,
            acted_count: 1,
            labelled_count: 1,
            latency_p50_ms: LATENCY_MS,
            latency_p95_ms: LATENCY_MS,
            compared_labels: 1,
            agreeing_labels: 1,
            agreement_rate: Some(1.0),
            last_timestamp: LOGGED_AT,
        }
    }

    /// The session logged one permission decision; another session in the
    /// project logged a tool search after it.
    fn snapshot() -> DecisionsFetchState {
        let own = decision(SESSION_ID, &DecisionFeature::PermissionAdvice, SESSION);
        let other = decision(OTHER_ID, &DecisionFeature::ToolSearch, OTHER_SESSION);
        let project = || ScopeActivity {
            stats: vec![
                stats(&DecisionFeature::PermissionAdvice),
                stats(&DecisionFeature::ToolSearch),
            ],
            recent: vec![other.clone(), own.clone()],
        };
        DecisionsFetchState::Ready(Box::new(DecisionsSnapshot {
            scopes: [
                ScopeActivity {
                    stats: vec![stats(&DecisionFeature::PermissionAdvice)],
                    recent: vec![own.clone()],
                },
                project(),
                project(),
            ],
            session: SESSION.into(),
            loaded_at: LOADED_AT.into(),
        }))
    }

    fn empty_snapshot() -> DecisionsFetchState {
        DecisionsFetchState::Ready(Box::new(DecisionsSnapshot {
            scopes: Default::default(),
            session: SESSION.into(),
            loaded_at: LOADED_AT.into(),
        }))
    }

    fn deliver(modal: &mut DecisionsModal, fetch: DecisionsFetchState) {
        let _ = modal.poll(&ArcSwapOption::from(Some(Arc::new(fetch))));
    }

    fn opened(fetch: DecisionsFetchState) -> DecisionsModal {
        let mut modal = DecisionsModal::new();
        modal.open();
        deliver(&mut modal, fetch);
        modal
    }

    fn render(modal: &mut DecisionsModal, fixture: &Fixture, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area(), &fixture.ctx());
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn draw(modal: &mut DecisionsModal, fixture: &Fixture, width: u16, height: u16) -> String {
        buffer_text(&render(modal, fixture, width, height))
    }

    fn left_down(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn type_char(modal: &mut DecisionsModal, character: char) -> DecisionsAction {
        modal.handle_key(press(KeyCode::Char(character)))
    }

    #[test_case(KeyCode::Tab, Tab::Features; "tab_steps_forward")]
    #[test_case(KeyCode::BackTab, Tab::Recent; "shift_tab_wraps_back")]
    #[test_case(KeyCode::Char('3'), Tab::Activity; "a_digit_jumps")]
    #[test_case(KeyCode::Char('9'), Tab::Overview; "a_digit_past_the_tabs_stays")]
    fn keys_switch_tabs(code: KeyCode, expected: Tab) {
        let mut modal = opened(snapshot());

        assert_eq!(modal.handle_key(press(code)), DecisionsAction::Consumed);

        assert_eq!(modal.tab(), expected);
    }

    #[test]
    fn clicking_a_tab_opens_it() {
        let fixture = Fixture::new(true);
        let mut modal = opened(snapshot());
        draw(&mut modal, &fixture, WIDE, TALL);
        let hit = modal.tab_hit(Tab::Recent).unwrap();

        let _ = modal.handle_mouse(left_down(hit.x, hit.y));

        assert_eq!(modal.tab(), Tab::Recent);
        let screen = draw(&mut modal, &fixture, WIDE, TALL);
        assert!(screen.contains(STATE_MARKER), "{screen}");
    }

    #[test]
    fn the_scope_key_cycles_through_rows_already_read() {
        let fixture = Fixture::new(true);
        let mut modal = opened(snapshot());
        let _ = type_char(&mut modal, '3');
        let tool_search = feature_label(&DecisionFeature::ToolSearch);
        let session = draw(&mut modal, &fixture, WIDE, TALL);

        assert_eq!(type_char(&mut modal, 'g'), DecisionsAction::Consumed);

        let project = draw(&mut modal, &fixture, WIDE, TALL);
        assert!(!session.contains(tool_search), "{session}");
        assert!(project.contains(tool_search), "{project}");
        assert_eq!(modal.scope(), DecisionsScope::Project);
        let _ = type_char(&mut modal, 'g');
        let _ = type_char(&mut modal, 'g');
        assert_eq!(modal.scope(), DecisionsScope::Session);
    }

    #[test_case(Tab::Activity, feature_label(&DecisionFeature::PermissionAdvice); "activity")]
    #[test_case(Tab::Recent, STATE_MARKER; "recent")]
    fn with_logging_off_the_log_tabs_say_how_to_turn_it_on(tab: Tab, kept_row: &str) {
        let fixture = Fixture::new(false);
        let mut modal = opened(snapshot());
        let overview = draw(&mut modal, &fixture, WIDE, TALL);
        modal.set_tab(tab);

        let screen = draw(&mut modal, &fixture, WIDE, TALL);

        assert!(screen.contains(LOGGING_HINT), "{screen}");
        assert!(screen.contains(kept_row), "{screen}");
        assert!(overview.contains(ENGINE_CONFIGURED), "{overview}");
        assert!(overview.contains(MODEL), "{overview}");
    }

    #[test_case(DecisionsFetchState::Loading, READING; "loading")]
    #[test_case(DecisionsFetchState::Missing, NO_DECISIONS; "missing")]
    #[test_case(DecisionsFetchState::Failed(FAILURE.into()), FAILURE; "failed")]
    #[test_case(empty_snapshot(), NO_SESSION_DECISIONS; "empty_scope")]
    fn the_activity_tab_says_why_it_has_no_rows(fetch: DecisionsFetchState, expected: &str) {
        let fixture = Fixture::new(true);
        let mut modal = opened(fetch);
        let _ = type_char(&mut modal, '3');

        let screen = draw(&mut modal, &fixture, WIDE, TALL);

        assert!(screen.contains(expected), "{screen}");
    }

    #[test_case("https://private-user:private-password@example.com:8443/private-path?private-query#private-fragment", "https://example.com:8443"; "remote")]
    #[test_case("http://private-user:private-password@[::1]:8080/private-path?private-query#private-fragment", "http://[::1]:8080"; "ipv6")]
    fn the_overview_shows_only_the_endpoint_origin_and_escapes_the_model(
        base_url: &str,
        origin: &str,
    ) {
        let mut fixture = Fixture::new(true);
        fixture.config.base_url = Some(base_url.parse().unwrap());
        fixture.config.model = CONTROL_MODEL.into();
        let mut modal = opened(DecisionsFetchState::Loading);

        let screen = draw(&mut modal, &fixture, WIDE, TALL);

        assert!(screen.contains(origin), "{screen}");
        assert!(
            screen.contains(&escape_terminal_controls(CONTROL_MODEL)),
            "{screen}"
        );
        for hidden in HIDDEN_URL_PARTS {
            assert!(!screen.contains(hidden), "{hidden}: {screen}");
        }
    }

    #[test]
    fn at_eighty_columns_the_footer_fits_with_a_hit_for_every_key() {
        let fixture = Fixture::new(true);
        let mut modal = opened(snapshot());

        let screen = draw(&mut modal, &fixture, NARROW, TALL);

        for (index, (_, description, _)) in FOOTER.iter().enumerate() {
            assert!(modal.footer_hit(index).width > 0, "{NO_HIT}: {description}");
            assert!(screen.contains(description), "{screen}");
        }
    }

    /// Features ends on a workflow row as wide as the list's minimum, and the
    /// session's one decision carries a label mark at the end of its row.
    #[test_case(Tab::Features; "features")]
    #[test_case(Tab::Recent; "recent")]
    fn list_rows_stop_a_column_short_of_the_detail(tab: Tab) {
        let fixture = Fixture::new(true);
        let mut modal = opened(snapshot());
        modal.set_tab(tab);

        let buffer = render(&mut modal, &fixture, WIDE, TALL);

        let list = modal.list_area;
        assert!(list.width > 0, "{}", buffer_text(&buffer));
        for row in list.y..list.bottom() {
            let gutter = buffer.cell((list.right() - 1, row)).map(Cell::symbol);
            assert_eq!(gutter, Some(" "), "{}", buffer_text(&buffer));
        }
    }

    /// A ✗ is an error and a ✓ a label, so each keeps its own column on a row
    /// without the other.
    #[test]
    fn recent_marks_keep_their_columns() {
        let labelled = decision(SESSION_ID, &DecisionFeature::PermissionAdvice, SESSION);
        let mut failed = labelled.clone();
        failed.record.error = Some(ENGINE_ERROR.into());
        let mut bare = labelled.clone();
        bare.label = None;
        let theme = theme::current();

        let rows = [&labelled, &failed, &bare].map(|entry| {
            recent_row(entry, LOGGED_AT, &theme)
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        });

        assert!(
            rows.iter().all(|row| row.width() == rows[0].width()),
            "{rows:?}"
        );
        assert!(
            rows[..2].iter().all(|row| row.ends_with(LABEL_MARK)),
            "{rows:?}"
        );
        assert!(rows[1].contains(ERROR_MARK), "{rows:?}");
    }

    /// The table is wider than the modal here, so only lines that fit on
    /// their own can be read without panning.
    #[test]
    fn at_eighty_columns_the_activity_footnote_reads_whole() {
        let fixture = Fixture::new(true);
        let mut modal = opened(snapshot());
        let _ = type_char(&mut modal, '3');

        let screen = draw(&mut modal, &fixture, NARROW, TALL);

        for note in ACTIVITY_FOOTNOTE {
            assert!(screen.contains(note), "{note}: {screen}");
        }
    }

    #[test]
    fn a_footer_click_runs_its_command() {
        let fixture = Fixture::new(true);
        let mut modal = opened(snapshot());
        draw(&mut modal, &fixture, WIDE, TALL);
        let refresh = FOOTER
            .iter()
            .position(|(_, _, command)| *command == FooterCommand::Refresh)
            .unwrap();
        let hit = modal.footer_hit(refresh);

        let _ = modal.handle_mouse(left_down(hit.x, hit.y));
        let action = modal.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..left_down(hit.x, hit.y)
        });

        assert_eq!(action, DecisionsAction::Refresh);
    }

    #[test]
    fn a_narrow_body_shows_one_pane_and_the_arrows_switch_it() {
        let fixture = Fixture::new(true);
        let mut modal = opened(snapshot());
        let _ = type_char(&mut modal, '2');
        let description = &DecisionFeature::PermissionAdvice
            .setting()
            .unwrap()
            .description[..DESCRIPTION_PREFIX];
        let other = feature_label(&DecisionFeature::AutoScreening);

        let list = draw(&mut modal, &fixture, NARROW, TALL);
        let _ = modal.handle_key(press(KeyCode::Right));
        let detail = draw(&mut modal, &fixture, NARROW, TALL);
        let _ = modal.handle_key(press(KeyCode::Left));
        let back = draw(&mut modal, &fixture, NARROW, TALL);

        assert!(list.contains(other), "{list}");
        assert!(!list.contains(description), "{list}");
        assert!(detail.contains(description), "{detail}");
        assert!(!detail.contains(other), "{detail}");
        assert_eq!(modal.pane(), Pane::List);
        assert_eq!(back, list);
    }

    #[test]
    fn the_recent_detail_shows_every_json_section() {
        let fixture = Fixture::new(true);
        let mut modal = opened(snapshot());
        let _ = type_char(&mut modal, '4');

        let screen = draw(&mut modal, &fixture, WIDE, TALL);

        for section in JSON_SECTIONS {
            assert!(screen.contains(section), "{section}: {screen}");
        }
        assert!(screen.contains(STATE_MARKER), "{screen}");
        assert!(screen.contains(LABEL_KEY), "{screen}");
    }

    #[test]
    fn y_copies_the_selected_decision_as_json() {
        let fixture = Fixture::new(true);
        let mut modal = opened(snapshot());
        let _ = type_char(&mut modal, '4');
        draw(&mut modal, &fixture, WIDE, TALL);

        let DecisionsAction::Copy { text, label } = type_char(&mut modal, 'y') else {
            panic!("{NOT_COPIED}");
        };

        let copied: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(copied["id"], SESSION_ID);
        assert_eq!(label, COPIED_DECISION);
    }

    #[test]
    fn y_copies_the_visible_section_on_other_tabs() {
        let fixture = Fixture::new(true);
        let mut modal = opened(snapshot());
        draw(&mut modal, &fixture, WIDE, TALL);

        let DecisionsAction::Copy { text, label } = type_char(&mut modal, 'y') else {
            panic!("{NOT_COPIED}");
        };

        assert!(text.contains(MODEL), "{text}");
        assert_eq!(label, COPIED_SECTION);
    }

    #[test]
    fn a_refresh_keeps_the_selected_decision() {
        let fixture = Fixture::new(true);
        let mut modal = opened(snapshot());
        let _ = type_char(&mut modal, '4');
        let _ = type_char(&mut modal, 'g');
        draw(&mut modal, &fixture, WIDE, TALL);
        let _ = modal.handle_key(press(KeyCode::Down));
        let mut refreshed = snapshot();
        if let DecisionsFetchState::Ready(snapshot) = &mut refreshed {
            snapshot.scopes[DecisionsScope::Project.index()]
                .recent
                .insert(
                    0,
                    decision(NEWER_ID, &DecisionFeature::ToolSearch, OTHER_SESSION),
                );
        }

        deliver(&mut modal, refreshed);
        draw(&mut modal, &fixture, WIDE, TALL);

        assert_eq!(modal.selected, Some(SESSION_ID));
        assert_eq!(modal.cursor(), 2);
    }

    #[test_case(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), DecisionsAction::Close; "escape_closes")]
    #[test_case(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), DecisionsAction::Close; "ctrl_c_closes")]
    #[test_case(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL), DecisionsAction::Refresh; "ctrl_r_refreshes")]
    #[test_case(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), DecisionsAction::Consumed; "a_stray_key_is_consumed")]
    fn control_keys(key: KeyEvent, expected: DecisionsAction) {
        let mut modal = opened(snapshot());

        assert_eq!(modal.handle_key(key), expected);
    }

    #[test]
    fn the_wheel_scrolls_the_body_and_walks_the_list() {
        let fixture = Fixture::new(true);
        let mut modal = opened(snapshot());
        draw(&mut modal, &fixture, WIDE, SHORT);
        let body = modal.body_area;

        modal.scroll_at(Position::new(body.x, body.y), WHEEL_DOWN);

        assert!(modal.body_scroll.offset() > 0);
        let _ = type_char(&mut modal, '2');
        draw(&mut modal, &fixture, WIDE, SHORT);
        let list = modal.list_area;
        modal.scroll_at(Position::new(list.x, list.y), WHEEL_DOWN);
        assert_eq!(modal.feature, 1);
    }

    #[test]
    fn a_wide_activity_table_pans_by_key_wheel_and_bar() {
        let fixture = Fixture::new(true);
        let mut modal = opened(snapshot());
        let _ = type_char(&mut modal, '3');
        draw(&mut modal, &fixture, NARROW, TALL);
        let home = -i32::from(u16::MAX);

        let _ = modal.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT));
        let keyed = modal.body_scroll.pan();
        modal.pan(home);
        let _ = modal.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollRight,
            column: modal.body_area.x,
            row: modal.body_area.y,
            modifiers: KeyModifiers::NONE,
        });
        let wheeled = modal.body_scroll.pan();
        modal.pan(home);
        let _ = modal.handle_mouse(left_down(
            modal.body_area.right().saturating_sub(1),
            modal.popup.bottom().saturating_sub(1),
        ));

        assert!(keyed > 0);
        assert!(wheeled > 0);
        assert!(modal.body_scroll.pan() > 0);
    }

    #[test]
    fn clicking_a_row_selects_it() {
        let fixture = Fixture::new(true);
        let mut modal = opened(snapshot());
        let _ = type_char(&mut modal, '2');
        draw(&mut modal, &fixture, WIDE, TALL);
        let list = modal.list_area;

        let _ = modal.handle_mouse(left_down(list.x, list.y + 2));

        assert_eq!(modal.feature, 2);
        assert_eq!(modal.pane(), Pane::List);
    }

    #[test]
    fn a_closed_modal_ignores_finished_reads() {
        let mut modal = DecisionsModal::new();

        assert_eq!(
            modal.poll(&ArcSwapOption::from(Some(Arc::new(snapshot())))),
            Dirty::NO
        );
        assert!(!modal.is_loaded());
    }
}
