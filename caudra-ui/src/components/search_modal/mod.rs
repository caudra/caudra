//! One search for the open chat and for past sessions. *This chat* fuzzy-matches what the
//! transcript draws and previews each match in the transcript behind the modal; *Project* and
//! *All projects* search the stored history and preview each hit in a pane of their own.

mod history;

use std::cmp::Reverse;

use caudra_grab::grab_scope;
use caudra_storage::history_search::HistoryHit;
use caudra_storage::id::CaudraId;
use caudra_workbench::keys::{LIST_FIRST, LIST_LAST};
use caudra_workbench::text_field::{FieldKind, TextField, TextKey};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::components::keybindings::key;
use crate::components::modal::{CLOSE_HINT, ESC_LABEL, FooterHits, FooterLine, Modal, SEPARATOR};
use crate::components::scrollbar::{ScrollHint, Scrollbar, ScrollbarMouse};
use crate::components::section_tabs::{SectionTab, tab_strip};
use crate::components::{Overlay, field_styles, input_text_style, match_spans};
use crate::repaint::Cadence;
use crate::theme;
use history::{PaneKey, PaneMouse};

pub(crate) use history::{HistoryPane, HistoryPreview, HistoryStatus};

const MODAL_TITLE: &str = " Search ";
const MODAL_WIDTH_PERCENT: u16 = 50;
const MODAL_MAX_HEIGHT_PERCENT: u16 = 60;
const HISTORY_WIDTH_PERCENT: u16 = 90;
const HISTORY_MAX_HEIGHT_PERCENT: u16 = 85;
/// The tab strip, the query and the footer around the chat matches.
const CHAT_CHROME_ROWS: u16 = 3;
const SEARCH_PREFIX: &str = "/ ";
const NO_MATCHES: &str = "  No matches";
const LABEL_INDENT: &str = "  ";
const ENTER_LABEL: &str = "Enter";
const TAB_LABEL: &str = "Tab";
const GO_TO_HINT: &str = " go to";
const OPEN_HINT: &str = " open";
const PAST_SESSIONS_HINT: &str = " past sessions";
const SCOPE_HINT: &str = " scope";
const SORT_HINT: &str = " sort";
const RERANK_ON_HINT: &str = " rerank: on";
const RERANK_OFF_HINT: &str = " rerank: off";
/// After the count of listed matches when another page of them can still load.
const MORE_MARK: &str = "+";

/// Where the modal searches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SearchTab {
    #[default]
    ThisChat,
    Project,
    AllProjects,
}

impl SectionTab for SearchTab {
    const ALL: &'static [Self] = &[Self::ThisChat, Self::Project, Self::AllProjects];

    fn label(self) -> &'static str {
        match self {
            Self::ThisChat => "This chat",
            Self::Project => "Project",
            Self::AllProjects => "All projects",
        }
    }
}

impl SearchTab {
    pub(crate) fn searches_history(self) -> bool {
        self != Self::ThisChat
    }
}

/// A footer control, in the order the footer draws them.
#[derive(Clone, Copy)]
enum FooterCommand {
    Enter,
    Tab,
    Sort,
    Rerank,
    Close,
}

struct SearchMatch {
    segment_index: usize,
    score: u16,
    display_indices: Vec<u32>,
    display_line: String,
}

#[derive(Clone, Copy)]
struct SearchRowHit {
    area: Rect,
    match_index: usize,
    segment_index: usize,
}

pub enum SearchAction {
    Consumed,
    QueryChanged,
    Navigate,
    Select(usize),
    Copy(String),
    /// The selection came out of the query: it goes on the clipboard, and the
    /// matches are stale.
    Cut(String),
    Close(Option<(u32, bool)>),
    /// Another tab is showing. Leaving *This chat* owes the transcript its scroll position.
    TabChanged(Option<(u32, bool)>),
    /// Go to a hit from a past session.
    Open(Box<HistoryHit>),
}

pub struct SearchModal {
    tab: SearchTab,
    search: TextField,
    matches: Vec<SearchMatch>,
    selected: usize,
    scroll_offset: usize,
    viewport_height: usize,
    open: bool,
    saved_scroll: Option<(u32, bool)>,
    matcher: Matcher,
    /// Where the modal last drew, so a wheel event can tell whether it landed
    /// on the results or on the transcript behind them.
    popup: Rect,
    chat_area: Rect,
    row_hits: Vec<SearchRowHit>,
    mouse_down: Option<usize>,
    scrollbar: Scrollbar,
    history: HistoryPane,
    tab_hits: Vec<(Rect, SearchTab)>,
    footer: Vec<FooterCommand>,
    footer_hits: FooterHits,
    pointer: Option<Position>,
}

impl SearchModal {
    pub fn new() -> Self {
        Self {
            tab: SearchTab::default(),
            search: TextField::new(FieldKind::Line),
            matches: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            viewport_height: 0,
            open: false,
            saved_scroll: None,
            matcher: Matcher::new(Config::DEFAULT),
            popup: Rect::default(),
            chat_area: Rect::default(),
            row_hits: Vec::new(),
            mouse_down: None,
            scrollbar: Scrollbar::default(),
            history: HistoryPane::new(),
            tab_hits: Vec::new(),
            footer: Vec::new(),
            footer_hits: FooterHits::default(),
            pointer: None,
        }
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.popup.contains(pos)
    }

    pub(crate) fn area(&self) -> Rect {
        self.popup
    }

    /// Where a drag selects text, one pane at a time so a sweep never runs from the hits into
    /// the preview. An empty `Rect` is a pane the tab does not draw.
    pub(crate) fn text_panes(&self) -> [Rect; 2] {
        match self.tab.searches_history() {
            true => self.history.text_panes(),
            false => [history::without_bar(self.chat_area), Rect::default()],
        }
    }

    /// Whether `event` belongs to one of the tab's scrollbars, which no text selection may take.
    pub(crate) fn bar_claims(&self, event: &MouseEvent) -> bool {
        match self.tab.searches_history() {
            true => self.history.bar_claims(event),
            false => self.scrollbar.claims(event),
        }
    }

    /// Moves the viewport without moving the selection, so `ensure_visible`
    /// stays out of it: pulling the list back to the cursor on the next
    /// keystroke is the point, doing it on the wheel is not.
    pub fn scroll(&mut self, delta: i32) {
        let max_offset = self.matches.len().saturating_sub(self.viewport_height);
        let offset = if delta > 0 {
            self.scroll_offset.saturating_sub(delta as usize)
        } else {
            self.scroll_offset
                .saturating_add(delta.unsigned_abs() as usize)
        };
        let offset = offset.min(max_offset);
        if offset != self.scroll_offset {
            self.scroll_offset = offset;
            self.invalidate_mouse_geometry();
        }
    }

    /// The wheel at `position`: the chat matches, the past-session hits or their preview.
    pub fn scroll_at(&mut self, position: Position, delta: i32) {
        match self.tab.searches_history() {
            true => self.history.scroll_at(position, delta),
            false => self.scroll(delta),
        }
    }

    #[cfg(test)]
    pub fn open(&mut self, scroll_top: u32, auto_scroll: bool) {
        self.open_on(SearchTab::ThisChat, "", scroll_top, auto_scroll);
    }

    pub fn open_on(&mut self, tab: SearchTab, query: &str, scroll_top: u32, auto_scroll: bool) {
        self.reset();
        self.open = true;
        self.tab = tab;
        self.search = TextField::with_text(FieldKind::Line, query);
        self.saved_scroll = Some((scroll_top, auto_scroll));
    }

    /// Where past-session searches run from, and whether the decision engine can rerank them.
    pub(crate) fn bind_history(
        &mut self,
        session: Option<CaudraId>,
        project: String,
        rerank: bool,
    ) {
        self.history.bind(session, project, rerank);
    }

    pub fn close(&mut self) {
        self.reset();
    }

    fn reset(&mut self) {
        self.open = false;
        self.tab = SearchTab::default();
        self.search.clear();
        self.matches.clear();
        self.selected = 0;
        self.scroll_offset = 0;
        self.saved_scroll = None;
        self.history.reset();
        self.footer_hits.reset();
        self.invalidate_mouse_geometry();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn tab(&self) -> SearchTab {
        self.tab
    }

    pub(crate) fn query(&self) -> String {
        self.search.text()
    }

    pub(crate) fn history(&self) -> &HistoryPane {
        &self.history
    }

    pub(crate) fn history_mut(&mut self) -> &mut HistoryPane {
        &mut self.history
    }

    pub fn handle_paste(&mut self, text: &str) {
        self.handle_text(text, true)
    }

    pub fn handle_text(&mut self, text: &str, paste: bool) {
        self.search.handle_text(text, paste);
        self.invalidate_mouse_geometry();
    }

    /// Leaving the search behind without taking a match, which owes the
    /// transcript the scroll position it was opened from.
    pub fn cancel(&mut self) -> SearchAction {
        SearchAction::Close(self.saved_scroll.take())
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> SearchAction {
        if key_event.code == KeyCode::Esc {
            return self.cancel();
        }
        if let Some(tab) = tab_key(self.tab, key_event) {
            return self.switch_tab(tab);
        }
        if self.tab.searches_history() {
            match self.history.handle_key(key_event) {
                PaneKey::Ignored => {}
                PaneKey::Navigate => return SearchAction::Navigate,
                PaneKey::Rerun => return SearchAction::QueryChanged,
                PaneKey::Consumed => return SearchAction::Consumed,
                PaneKey::Open => return self.open_selected(),
            }
            return self.edit_query(key_event);
        }
        match key_event.code {
            KeyCode::Enter => {
                if let Some(m) = self.matches.get(self.selected) {
                    SearchAction::Select(m.segment_index)
                } else {
                    SearchAction::Close(self.saved_scroll.take())
                }
            }
            KeyCode::Up => {
                self.move_up();
                SearchAction::Navigate
            }
            KeyCode::Down => {
                self.move_down();
                SearchAction::Navigate
            }
            KeyCode::PageUp => {
                self.page(-1);
                SearchAction::Navigate
            }
            KeyCode::PageDown => {
                self.page(1);
                SearchAction::Navigate
            }
            _ if LIST_FIRST.matches(key_event) => {
                self.select(0);
                SearchAction::Navigate
            }
            _ if LIST_LAST.matches(key_event) => {
                self.select(self.matches.len().saturating_sub(1));
                SearchAction::Navigate
            }
            _ => self.edit_query(key_event),
        }
    }

    fn edit_query(&mut self, key_event: KeyEvent) -> SearchAction {
        let edit = self.search.handle_key(key_event);
        if edit.changed() {
            self.invalidate_mouse_geometry();
        }
        match edit {
            TextKey::Changed => SearchAction::QueryChanged,
            TextKey::Copy(text) => SearchAction::Copy(text),
            TextKey::Cut(text) => SearchAction::Cut(text),
            TextKey::Handled | TextKey::Refused | TextKey::Ignored => SearchAction::Consumed,
        }
    }

    fn switch_tab(&mut self, tab: SearchTab) -> SearchAction {
        if tab == self.tab {
            return SearchAction::Consumed;
        }
        let leaving_chat = !self.tab.searches_history();
        self.tab = tab;
        self.matches.clear();
        self.selected = 0;
        self.scroll_offset = 0;
        self.history.set_status(HistoryStatus::Empty);
        self.footer_hits.clear();
        self.invalidate_mouse_geometry();
        SearchAction::TabChanged(leaving_chat.then_some(self.saved_scroll).flatten())
    }

    fn open_selected(&self) -> SearchAction {
        self.history
            .selected_hit()
            .map_or(SearchAction::Consumed, |hit| {
                SearchAction::Open(Box::new(hit.clone()))
            })
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> SearchAction {
        let position = Position::new(event.column, event.row);
        self.pointer = Some(position);
        if let Some(index) = self.footer_hits.handle_mouse(event) {
            return self.footer_command(index);
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some((_, tab)) = self
                .tab_hits
                .iter()
                .find(|(area, _)| area.contains(position))
        {
            return self.switch_tab(*tab);
        }
        if self.tab.searches_history() {
            return match self.history.handle_mouse(event) {
                PaneMouse::Ignored | PaneMouse::Consumed => SearchAction::Consumed,
                PaneMouse::Navigate => SearchAction::Navigate,
                PaneMouse::Open => self.open_selected(),
            };
        }
        self.chat_mouse(event, position)
    }

    fn footer_command(&mut self, index: usize) -> SearchAction {
        let Some(command) = self.footer.get(index).copied() else {
            return SearchAction::Consumed;
        };
        let press = |code: KeyCode, modifiers: KeyModifiers| KeyEvent::new(code, modifiers);
        let event = match command {
            FooterCommand::Enter => press(KeyCode::Enter, KeyModifiers::NONE),
            FooterCommand::Tab => press(KeyCode::Tab, KeyModifiers::NONE),
            FooterCommand::Sort => key::SEARCH_SORT.to_key_event(),
            FooterCommand::Rerank => key::SEARCH_RERANK.to_key_event(),
            FooterCommand::Close => press(KeyCode::Esc, KeyModifiers::NONE),
        };
        self.handle_key(event)
    }

    fn chat_mouse(&mut self, event: MouseEvent, position: Position) -> SearchAction {
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return SearchAction::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll_offset = top as usize;
                self.invalidate_mouse_geometry();
                return SearchAction::Consumed;
            }
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.mouse_down = None;
                if let Some(hit) = self
                    .row_hits
                    .iter()
                    .find(|hit| hit.area.contains(position))
                    .copied()
                {
                    self.selected = hit.match_index;
                    self.mouse_down = Some(hit.segment_index);
                    return SearchAction::Navigate;
                }
                SearchAction::Consumed
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.mouse_down = None;
                SearchAction::Consumed
            }
            MouseEventKind::Moved => {
                if let Some(hit) = self.row_hits.iter().find(|hit| hit.area.contains(position)) {
                    self.selected = hit.match_index;
                    SearchAction::Navigate
                } else {
                    SearchAction::Consumed
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(pressed_segment) = self.mouse_down.take() else {
                    return SearchAction::Consumed;
                };
                let released_segment = self
                    .row_hits
                    .iter()
                    .find(|hit| hit.area.contains(position))
                    .map(|hit| hit.segment_index);
                if released_segment == Some(pressed_segment) {
                    SearchAction::Select(pressed_segment)
                } else {
                    SearchAction::Consumed
                }
            }
            _ => SearchAction::Consumed,
        }
    }

    fn move_up(&mut self) {
        if !self.matches.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.matches.len() - 1);
            self.ensure_visible();
        }
    }

    fn move_down(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
            self.ensure_visible();
        }
    }

    /// Clamps where the arrows wrap: a page is a jump rather than a step, so
    /// running off one end and reappearing at the other reads as a mistake.
    fn page(&mut self, direction: isize) {
        let step = self.viewport_height.max(1) as isize;
        let target = (self.selected as isize + direction * step).max(0);
        self.select(target as usize);
    }

    fn select(&mut self, index: usize) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = index.min(self.matches.len() - 1);
        self.ensure_visible();
    }

    fn ensure_visible(&mut self) {
        if self.viewport_height == 0 {
            return;
        }
        let previous_offset = self.scroll_offset;
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + self.viewport_height {
            self.scroll_offset = self.selected + 1 - self.viewport_height;
        }
        if self.scroll_offset != previous_offset {
            self.invalidate_mouse_geometry();
        }
    }

    pub fn update_matches(&mut self, segment_texts: &[&str]) {
        self.invalidate_mouse_geometry();
        let query = self.search.text();
        self.matches.clear();
        self.selected = 0;
        self.scroll_offset = 0;

        if query.trim().is_empty() {
            return;
        }

        let atom = Atom::new(
            &query,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Fuzzy,
            false,
        );

        let mut buf = Vec::new();
        let mut indices = Vec::new();
        for (idx, text) in segment_texts.iter().enumerate() {
            if text.is_empty() {
                continue;
            }
            buf.clear();
            indices.clear();
            let haystack = Utf32Str::new(text, &mut buf);
            if let Some(score) = atom.indices(haystack, &mut self.matcher, &mut indices) {
                let (display_line, display_indices) = pick_display_line(text, &indices);
                self.matches.push(SearchMatch {
                    segment_index: idx,
                    score,
                    display_indices,
                    display_line,
                });
            }
        }

        self.matches.sort_by_key(|m| Reverse(m.score));
    }

    pub fn current_segment_index(&self) -> Option<usize> {
        match self.tab {
            SearchTab::ThisChat => self.matches.get(self.selected).map(|m| m.segment_index),
            SearchTab::Project | SearchTab::AllProjects => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn row_area(&self, index: usize) -> Option<Rect> {
        self.row_hits.get(index).map(|hit| hit.area)
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("search_modal", area);
        let history = self.tab.searches_history();
        let modal = match history {
            true => Modal {
                title: MODAL_TITLE,
                width_percent: HISTORY_WIDTH_PERCENT,
                max_height_percent: HISTORY_MAX_HEIGHT_PERCENT,
            },
            false => Modal {
                title: MODAL_TITLE,
                width_percent: MODAL_WIDTH_PERCENT,
                max_height_percent: MODAL_MAX_HEIGHT_PERCENT,
            },
        };
        let content_rows = match history {
            true => u16::MAX,
            false if self.matches.is_empty() && !self.search.is_empty() => 1,
            false => u16::try_from(self.matches.len()).unwrap_or(u16::MAX),
        };
        let (popup, inner) = modal.render(
            frame,
            area,
            content_rows.saturating_add(CHAT_CHROME_ROWS + 1),
        );
        self.popup = popup;
        let status_rows = u16::from(history);
        let [tabs_area, search_area, status_area, body, footer_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(status_rows),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        let (strip, hits) = tab_strip(self.tab, self.pointer, tabs_area);
        frame.render_widget(Paragraph::new(strip), tabs_area);
        self.tab_hits = hits;
        self.render_search(frame, search_area);
        if history {
            let status = self.history.status_line(
                self.search.text().trim().is_empty(),
                self.tab == SearchTab::AllProjects,
            );
            frame.render_widget(Paragraph::new(status), status_area);
            self.history.view(frame, body);
        } else {
            self.render_chat(frame, body);
        }
        self.render_footer(frame, footer_area);
        popup
    }

    fn render_chat(&mut self, frame: &mut Frame, list_area: Rect) {
        self.chat_area = list_area;
        let viewport_h = usize::from(list_area.height);
        self.viewport_height = viewport_h;
        self.row_hits.clear();
        let end = (self.scroll_offset + viewport_h).min(self.matches.len());
        self.row_hits.extend(
            self.matches[self.scroll_offset.min(end)..end]
                .iter()
                .enumerate()
                .map(|(row, m)| SearchRowHit {
                    area: Rect::new(list_area.x, list_area.y + row as u16, list_area.width, 1),
                    match_index: self.scroll_offset + row,
                    segment_index: m.segment_index,
                }),
        );
        self.render_list(frame, list_area, viewport_h);
        self.scrollbar.set_hint(ScrollHint::matches(
            u32::try_from(self.scroll_offset + 1).unwrap_or(u32::MAX),
            u32::try_from(self.matches.len()).unwrap_or(u32::MAX),
        ));
        self.scrollbar.draw(
            frame,
            list_area,
            self.matches.len() as u32,
            self.scroll_offset as u32,
        );
    }

    fn render_footer(&mut self, frame: &mut Frame, area: Rect) {
        let t = theme::current();
        let mut line = FooterLine::default();
        self.footer.clear();
        let mut command =
            |line: &mut FooterLine, command: FooterCommand, key: &'static str, hint| {
                if !self.footer.is_empty() {
                    line.text(SEPARATOR, t.item_desc);
                }
                line.command(key, t.keybind_key);
                line.describe(hint, t.keybind_desc);
                self.footer.push(command);
            };
        match self.tab.searches_history() {
            true => {
                command(&mut line, FooterCommand::Enter, ENTER_LABEL, OPEN_HINT);
                command(&mut line, FooterCommand::Tab, TAB_LABEL, SCOPE_HINT);
                command(
                    &mut line,
                    FooterCommand::Sort,
                    key::SEARCH_SORT.label,
                    SORT_HINT,
                );
                if self.history.rerank_offered() {
                    let hint = match self.history.wants_rerank() {
                        true => RERANK_ON_HINT,
                        false => RERANK_OFF_HINT,
                    };
                    command(
                        &mut line,
                        FooterCommand::Rerank,
                        key::SEARCH_RERANK.label,
                        hint,
                    );
                }
            }
            false => {
                command(&mut line, FooterCommand::Enter, ENTER_LABEL, GO_TO_HINT);
                command(&mut line, FooterCommand::Tab, TAB_LABEL, PAST_SESSIONS_HINT);
            }
        }
        command(&mut line, FooterCommand::Close, ESC_LABEL, CLOSE_HINT);
        self.footer_hits.set(line.hits(area, 0, 1));
        frame.render_widget(Paragraph::new(line.line(self.footer_hits.hovered())), area);
    }

    fn invalidate_mouse_geometry(&mut self) {
        self.row_hits.clear();
        self.mouse_down = None;
        self.history.invalidate_mouse_geometry();
    }

    fn render_list(&self, frame: &mut Frame, area: Rect, viewport_height: usize) {
        grab_scope!("search_modal_list", area);
        let t = theme::current();

        if self.matches.is_empty() {
            if !self.search.is_empty() {
                let line = Line::from(Span::styled(NO_MATCHES, t.item_desc));
                frame.render_widget(Paragraph::new(vec![line]), area);
            }
            return;
        }

        let max_label_width = area.width.saturating_sub(LABEL_INDENT.len() as u16) as usize;
        let mut lines: Vec<Line> = Vec::new();
        let end = (self.scroll_offset + viewport_height).min(self.matches.len());

        for i in self.scroll_offset..end {
            let m = &self.matches[i];
            let is_selected = i == self.selected;
            let line = build_highlighted_line(
                &m.display_line,
                &m.display_indices,
                max_label_width,
                is_selected,
                &t,
            );
            lines.push(line);
        }

        frame.render_widget(Paragraph::new(lines), area);
    }

    /// The query, with which match is selected out of how many at the right edge.
    fn render_search(&self, frame: &mut Frame, area: Rect) {
        grab_scope!("search_modal_search", area);
        let t = theme::current();
        let prefix = Span::styled(SEARCH_PREFIX, t.tool_dim);
        let position = self.position().map(|(current, total, more)| {
            let plus = if more { MORE_MARK } else { "" };
            Span::styled(format!(" {current}/{total}{plus}"), t.item_desc)
        });
        let counter = position.as_ref().map_or(0, Span::width);
        let width = usize::from(area.width).saturating_sub(prefix.width() + counter);
        let mut line = self
            .search
            .paint(width, &field_styles(input_text_style()), true, "");
        line.spans.insert(0, prefix);
        frame.render_widget(Paragraph::new(line), area);
        if let Some(position) = position {
            frame.render_widget(Paragraph::new(Line::from(position).right_aligned()), area);
        }
    }

    /// The selected match counted from one, how many are listed, and whether more follow.
    fn position(&self) -> Option<(usize, usize, bool)> {
        match self.tab.searches_history() {
            true => self.history.selected_hit().map(|_| {
                let pane = &self.history;
                (pane.selected() + 1, pane.hits().len(), pane.more())
            }),
            false => {
                (!self.matches.is_empty()).then(|| (self.selected + 1, self.matches.len(), false))
            }
        }
    }
}

impl Overlay for SearchModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }

    fn cadence(&self) -> Cadence {
        Cadence::when(
            self.open && self.tab.searches_history() && self.history.busy(),
            Cadence::SPINNER,
        )
    }
}

/// The tab a key asks for: Tab and Shift+Tab step, Alt and a digit jump.
fn tab_key(current: SearchTab, key_event: KeyEvent) -> Option<SearchTab> {
    match key_event.code {
        KeyCode::BackTab => Some(current.step(-1)),
        KeyCode::Tab if key_event.modifiers == KeyModifiers::SHIFT => Some(current.step(-1)),
        KeyCode::Tab if key_event.modifiers.is_empty() => Some(current.step(1)),
        KeyCode::Char(digit) if key_event.modifiers == KeyModifiers::ALT => {
            SearchTab::from_digit(digit)
        }
        _ => None,
    }
}

fn pick_display_line(text: &str, indices: &[u32]) -> (String, Vec<u32>) {
    let first_idx = indices.iter().copied().min().unwrap_or(0);
    let mut char_offset = 0u32;
    for line in text.lines() {
        let line_char_count = line.chars().count() as u32;
        if first_idx < char_offset + line_char_count {
            let remapped: Vec<u32> = indices
                .iter()
                .filter(|&&i| i >= char_offset && i < char_offset + line_char_count)
                .map(|&i| i - char_offset)
                .collect();
            return (line.to_string(), remapped);
        }
        char_offset += line_char_count + 1;
    }
    let first_line = text.lines().next().unwrap_or("").to_string();
    (first_line, Vec::new())
}

fn build_highlighted_line<'a>(
    text: &str,
    indices: &[u32],
    max_width: usize,
    is_selected: bool,
    t: &'a theme::Theme,
) -> Line<'a> {
    let (base, matched) = match is_selected {
        true => (t.item_selected, t.item_match_selected),
        false => (t.item, t.item_match),
    };

    let mut spans = vec![Span::styled(LABEL_INDENT, base)];
    spans.extend(match_spans(text, indices, base, matched, Some(max_width)));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::keybindings::key;
    use caudra_agent::history::Ranking;
    use caudra_storage::history_search::{HistoryKind, HistoryRef, HistoryStream, Snippet};
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
    use std::ops::Range;
    use test_case::test_case;

    const BAR_LOST_THE_PRESS: &str = "the row under the scrollbar took the press";
    const QUERY_UNTOUCHED: &str = "the navigation key edited the query line";
    const EXPECT_NAVIGATE: &str = "moving the selection has to re-sync the transcript highlight";
    const REFILTERED: &str = "a caret motion refiltered the results";
    const NOT_COPIED: &str = "the selected query never reached the clipboard";
    const ITEM: &str = "item";
    const ITEM_COUNT: usize = 20;
    /// More matches than the 80x24 test terminal can show, so the bar has a
    /// track to press on.
    const OVERFLOWING_MATCHES: usize = 50;

    fn key_event(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn mouse(kind: MouseEventKind, area: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn render(modal: &mut SearchModal) {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area());
            })
            .unwrap();
    }

    fn modal_with_query(query: &str, texts: &[&str]) -> SearchModal {
        let mut modal = SearchModal::new();
        modal.open(0, true);
        modal.search = TextField::with_text(FieldKind::Line, query);
        modal.update_matches(texts);
        modal
    }

    /// `ITEM` matching every one of `ITEM_COUNT` results.
    fn modal_with_items() -> SearchModal {
        let texts: Vec<String> = (0..ITEM_COUNT).map(|i| format!("{ITEM} {i}")).collect();
        let borrowed: Vec<&str> = texts.iter().map(String::as_str).collect();
        modal_with_query(ITEM, &borrowed)
    }

    #[test]
    fn matching_finds_correct_segments() {
        let modal = modal_with_query("hello", &["hello world", "foo bar", "say hello"]);
        assert_eq!(modal.matches.len(), 2);
        assert!(modal.matches.iter().all(|m| !m.display_indices.is_empty()));
        let seg: Vec<usize> = modal.matches.iter().map(|m| m.segment_index).collect();
        assert!(seg.contains(&0));
        assert!(seg.contains(&2));
    }

    #[test]
    fn matches_sorted_by_score_descending() {
        let modal = modal_with_query("fb", &["foobar", "fb", "f---b"]);
        assert!(modal.matches.len() >= 2);
        for w in modal.matches.windows(2) {
            assert!(w[0].score >= w[1].score);
        }
    }

    #[test]
    fn navigation_wraps_around() {
        let mut modal = modal_with_query("item", &["item a", "item b", "item c"]);
        assert_eq!(modal.selected, 0);

        modal.handle_key(key_event(KeyCode::Down));
        assert_eq!(modal.selected, 1);
        modal.handle_key(key_event(KeyCode::Down));
        assert_eq!(modal.selected, 2);
        modal.handle_key(key_event(KeyCode::Down));
        assert_eq!(modal.selected, 0);

        modal.handle_key(key_event(KeyCode::Up));
        assert_eq!(modal.selected, 2);
    }

    /// A page is a jump, so it clamps where the arrows wrap, and all four
    /// navigation keys belong to the result list rather than to the query.
    #[test_case(LIST_FIRST.to_key_event(),   0  ; "ctrl_home_selects_first")]
    #[test_case(LIST_LAST.to_key_event(),    19 ; "ctrl_end_selects_last")]
    #[test_case(key_event(KeyCode::PageUp),   5  ; "page_up_retreats_a_page")]
    #[test_case(key_event(KeyCode::PageDown), 15 ; "page_down_advances_a_page")]
    fn navigation_keys_move_the_result_list(event: KeyEvent, expected: usize) {
        let mut modal = modal_with_items();
        modal.viewport_height = 5;
        modal.selected = 10;

        let action = modal.handle_key(event);

        assert!(
            matches!(action, SearchAction::Navigate),
            "{EXPECT_NAVIGATE}"
        );
        assert_eq!(modal.selected, expected);
        assert_eq!(modal.search.text(), ITEM, "{QUERY_UNTOUCHED}");
    }

    /// Home and End move the caret without refiltering, which would throw the
    /// selection back to the top.
    #[test_case(KeyCode::Home, 0          ; "home")]
    #[test_case(KeyCode::End,  ITEM.len() ; "end")]
    fn home_and_end_move_the_query_caret(code: KeyCode, caret: usize) {
        let mut modal = modal_with_items();
        modal.search.set_cursor_offset(1);
        modal.selected = 10;

        let action = modal.handle_key(key_event(code));

        assert!(matches!(action, SearchAction::Consumed), "{REFILTERED}");
        assert_eq!(modal.search.cursor_offset(), caret);
        assert_eq!(modal.selected, 10, "{REFILTERED}");
    }

    #[test_case(key::QUIT.to_key_event(), false ; "ctrl_c_copies")]
    #[test_case(key::CUT.to_key_event(),  true  ; "shift_delete_cuts")]
    fn clipboard_chords_hand_the_selected_query_back(event: KeyEvent, cut: bool) {
        let mut modal = modal_with_items();
        modal.handle_key(key::SELECT_ALL.to_key_event());

        let copied = match modal.handle_key(event) {
            SearchAction::Copy(text) if !cut => text,
            SearchAction::Cut(text) if cut => text,
            _ => panic!("{NOT_COPIED}"),
        };

        assert_eq!(copied, ITEM);
        assert_eq!(modal.search.is_empty(), cut);
    }

    #[test]
    fn enter_selects_current_match() {
        let mut modal = modal_with_query("hello", &["hello world", "foo bar", "say hello"]);
        modal.handle_key(key_event(KeyCode::Down));
        let expected_seg = modal.matches[1].segment_index;

        match modal.handle_key(key_event(KeyCode::Enter)) {
            SearchAction::Select(idx) => assert_eq!(idx, expected_seg),
            other => panic!("expected Select, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn enter_on_no_matches_closes() {
        let mut modal = modal_with_query("zzz", &["hello", "world"]);
        assert!(matches!(
            modal.handle_key(key_event(KeyCode::Enter)),
            SearchAction::Close(_)
        ));
    }

    #[test]
    fn close_clears_state() {
        let mut modal = modal_with_query("hello", &["hello world"]);
        assert!(!modal.matches.is_empty());
        modal.close();
        assert!(modal.matches.is_empty());
        assert!(modal.search.is_empty());
        assert!(!modal.is_open());
    }

    #[test_case("hello", "hello world\nsecond line", "hello world" ; "match_on_first_line")]
    #[test_case("second", "header\nsecond line\nthird", "second line" ; "match_on_middle_line")]
    fn display_line_picks_matched_line(query: &str, text: &str, expected: &str) {
        let modal = modal_with_query(query, &[text]);
        assert_eq!(modal.matches.len(), 1);
        assert_eq!(modal.matches[0].display_line, expected);
        assert!(
            modal.matches[0]
                .display_indices
                .iter()
                .all(|&i| i < expected.len() as u32)
        );
    }

    #[test_case("thinking>", &["hello", "world", "thinking> hmm"], 2 ; "thinking_prefix")]
    #[test_case("bash>",     &["request", "response", "bash> output"], 2 ; "tool_prefix")]
    fn search_non_author_prefix_matches(query: &str, texts: &[&str], expected_idx: usize) {
        let modal = modal_with_query(query, texts);
        assert_eq!(modal.matches.len(), 1);
        assert_eq!(modal.matches[0].segment_index, expected_idx);
    }

    #[test]
    fn hovering_search_result_navigates_to_it() {
        let mut modal = modal_with_query("item", &["item a", "item b", "item c"]);
        render(&mut modal);
        let hit = modal.row_hits[2];

        assert!(matches!(
            modal.handle_mouse(mouse(MouseEventKind::Moved, hit.area)),
            SearchAction::Navigate
        ));
        assert_eq!(modal.selected, hit.match_index);
    }

    #[test]
    fn pressing_the_scrollbar_scrolls_instead_of_navigating() {
        let texts: Vec<String> = (0..OVERFLOWING_MATCHES)
            .map(|i| format!("item {i}"))
            .collect();
        let borrowed: Vec<&str> = texts.iter().map(String::as_str).collect();
        let mut modal = modal_with_query("item", &borrowed);
        render(&mut modal);
        let last = *modal.row_hits.last().unwrap();
        let bar = Rect::new(last.area.right() - 1, last.area.y, 1, 1);

        let action = modal.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), bar));

        assert!(
            matches!(action, SearchAction::Consumed),
            "{BAR_LOST_THE_PRESS}"
        );
        assert_eq!(
            modal.scroll_offset,
            OVERFLOWING_MATCHES - modal.viewport_height,
            "{BAR_LOST_THE_PRESS}"
        );
    }

    #[test]
    fn clicking_search_result_selects_its_segment() {
        let mut modal = modal_with_query("item", &["item a", "item b", "item c"]);
        render(&mut modal);
        let hit = modal.row_hits[1];

        modal.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hit.area));
        let action = modal.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hit.area));

        assert!(matches!(
            action,
            SearchAction::Select(index) if index == hit.segment_index
        ));
    }

    #[test]
    fn releasing_on_another_search_result_does_not_select() {
        let mut modal = modal_with_query("item", &["item a", "item b"]);
        render(&mut modal);
        let first = modal.row_hits[0];
        let second = modal.row_hits[1];

        modal.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), first.area));
        let action = modal.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), second.area));

        assert!(matches!(action, SearchAction::Consumed));
    }

    #[test]
    fn dragging_search_result_cancels_click() {
        let mut modal = modal_with_query("item", &["item a", "item b"]);
        render(&mut modal);
        let hit = modal.row_hits[0];

        modal.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hit.area));
        modal.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), hit.area));
        let action = modal.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hit.area));

        assert!(matches!(action, SearchAction::Consumed));
    }

    #[test]
    fn replacing_search_matches_invalidates_armed_row() {
        let mut modal = modal_with_query("item", &["item a", "item b"]);
        render(&mut modal);
        let stale = modal.row_hits[0];
        modal.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), stale.area));

        modal.update_matches(&["replacement item"]);
        let action = modal.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), stale.area));

        assert!(matches!(action, SearchAction::Consumed));
        assert!(modal.row_hits.is_empty());
        assert!(modal.mouse_down.is_none());
    }

    const QUERY: &str = "retry loop";
    /// Where `QUERY` starts in each hit's snippet.
    const QUERY_AT: usize = "we talked about the ".len();
    const PROJECT: &str = "/work/project";
    const TITLES: [&str; 3] = ["Fix the retry loop", "Ledger cleanup", "Release prep"];
    const WIDE: (u16, u16) = (140, 40);
    const SAVED_TOP: u32 = 7;
    const QUERY_LOST: &str = "switching tabs dropped the query";
    const SELECTION_LOST: &str = "the selection left the hit it was on";
    const SNAPPED_BACK: &str = "the redraw pulled the list back to the selected hit";
    const WHEEL_STEP: i32 = 3;

    fn hit(title: &str) -> HistoryHit {
        HistoryHit {
            reference: HistoryRef {
                session: CaudraId::generate(),
                stream: HistoryStream::Main,
                item: CaudraId::generate(),
            },
            session_title: title.to_owned(),
            project: PROJECT.to_owned(),
            kind: HistoryKind::User,
            tool: None,
            at_ms: 0,
            score: 1.0,
            snippet: Snippet {
                text: format!("we talked about the {QUERY}"),
                highlights: Vec::from([Range {
                    start: QUERY_AT,
                    end: QUERY_AT + QUERY.len(),
                }]),
            },
        }
    }

    fn ready() -> HistoryStatus {
        HistoryStatus::Ready {
            ranking: Ranking::Keyword {
                matched: TITLES.len(),
            },
            judged_unrelated: 0,
            unindexed_sessions: 0,
            note: None,
        }
    }

    fn history_modal(tab: SearchTab, rerank: bool) -> SearchModal {
        let mut modal = SearchModal::new();
        modal.open_on(tab, QUERY, SAVED_TOP, false);
        modal.bind_history(None, PROJECT.to_owned(), rerank);
        let hits = TITLES.iter().map(|title| hit(title)).collect();
        modal.history_mut().set_hits(hits, false, false, ready());
        modal
    }

    fn screen(modal: &mut SearchModal, (width, height): (u16, u16)) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                modal.view(frame, frame.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test_case(with(KeyCode::Tab, KeyModifiers::NONE),       SearchTab::Project     ; "tab_steps_forward")]
    #[test_case(with(KeyCode::BackTab, KeyModifiers::SHIFT),  SearchTab::AllProjects ; "back_tab_wraps_back")]
    #[test_case(with(KeyCode::Char('3'), KeyModifiers::ALT),  SearchTab::AllProjects ; "alt_digit_jumps")]
    fn leaving_this_chat_keeps_the_query_and_returns_the_scroll(
        event: KeyEvent,
        expected: SearchTab,
    ) {
        let mut modal = SearchModal::new();
        modal.open_on(SearchTab::ThisChat, QUERY, SAVED_TOP, false);

        let action = modal.handle_key(event);

        assert_eq!(modal.tab(), expected);
        assert_eq!(modal.query(), QUERY, "{QUERY_LOST}");
        assert!(matches!(
            action,
            SearchAction::TabChanged(Some((SAVED_TOP, false)))
        ));
    }

    #[test]
    fn coming_back_to_this_chat_previews_again() {
        let mut modal = history_modal(SearchTab::Project, false);

        let action = modal.handle_key(with(KeyCode::Char('1'), KeyModifiers::ALT));

        assert_eq!(modal.tab(), SearchTab::ThisChat);
        assert!(matches!(action, SearchAction::TabChanged(None)));
        assert!(matches!(
            modal.cancel(),
            SearchAction::Close(Some((SAVED_TOP, false)))
        ));
    }

    #[test]
    fn selection_follows_its_hit_through_a_reorder() {
        let mut modal = history_modal(SearchTab::Project, false);
        modal.handle_key(key_event(KeyCode::Down));
        let chosen = modal.history().selected_hit().unwrap().clone();
        let mut reordered = modal.history().hits().to_vec();
        reordered.reverse();

        modal
            .history_mut()
            .set_hits(reordered, false, false, ready());

        assert_eq!(
            modal.history().selected_hit().map(|hit| &hit.reference),
            Some(&chosen.reference),
            "{SELECTION_LOST}"
        );
    }

    #[test]
    fn enter_opens_the_selected_hit() {
        let mut modal = history_modal(SearchTab::AllProjects, false);
        modal.handle_key(key_event(KeyCode::Down));
        let chosen = modal.history().selected_hit().unwrap().reference.clone();

        let action = modal.handle_key(key_event(KeyCode::Enter));

        assert!(matches!(action, SearchAction::Open(hit) if hit.reference == chosen));
    }

    #[test_case(true,  true  ; "rerank_toggles_when_offered")]
    #[test_case(false, false ; "rerank_is_text_when_not_offered")]
    fn rerank_key_reruns_only_when_offered(offered: bool, reruns: bool) {
        let mut modal = history_modal(SearchTab::Project, offered);
        let before = modal.history().wants_rerank();

        let action = modal.handle_key(key::SEARCH_RERANK.to_key_event());

        assert_eq!(matches!(action, SearchAction::QueryChanged), reruns);
        assert_eq!(modal.history().wants_rerank() != before, reruns);
    }

    #[test]
    fn sort_key_flips_the_order_and_reruns() {
        let mut modal = history_modal(SearchTab::Project, false);
        let before = modal.history().sort();

        let action = modal.handle_key(key::SEARCH_SORT.to_key_event());

        assert!(matches!(action, SearchAction::QueryChanged));
        assert_ne!(modal.history().sort(), before);
        assert_eq!(modal.query(), QUERY, "{QUERY_LOST}");
    }

    #[test]
    fn history_tab_draws_hits_scopes_and_hints() {
        let mut modal = history_modal(SearchTab::Project, true);

        let screen = screen(&mut modal, WIDE);

        for expected in TITLES
            .into_iter()
            .chain(SearchTab::ALL.iter().map(|tab| tab.label()))
            .chain([SORT_HINT.trim(), RERANK_ON_HINT.trim(), SCOPE_HINT.trim()])
        {
            assert!(screen.contains(expected), "{expected} missing:\n{screen}");
        }
    }

    fn many_hits_modal(count: usize) -> SearchModal {
        let mut modal = history_modal(SearchTab::Project, false);
        let hits = (0..count)
            .map(|index| hit(&format!("{ITEM} {index}")))
            .collect();
        modal.history_mut().set_hits(hits, true, false, ready());
        modal
    }

    #[test]
    fn the_wheel_moves_the_hits_past_the_selection_and_a_redraw_keeps_them_there() {
        let mut modal = many_hits_modal(OVERFLOWING_MATCHES);
        let _ = screen(&mut modal, WIDE);
        let list = modal.history.list_area();

        modal.scroll_at(Position::new(list.x, list.y), -WHEEL_STEP);
        let _ = screen(&mut modal, WIDE);
        modal.scroll_at(Position::new(list.x, list.y), -WHEEL_STEP);
        let after = screen(&mut modal, WIDE);

        assert_eq!(
            modal.history().selected(),
            0,
            "the wheel moved the selection"
        );
        assert_eq!(
            modal.history.scroll(),
            2 * WHEEL_STEP as usize,
            "{SNAPPED_BACK}"
        );
        assert!(
            !after.contains(&format!("{ITEM} 0 ")),
            "{SNAPPED_BACK}:\n{after}"
        );
    }

    #[test]
    fn pressing_the_hit_scrollbar_moves_the_list() {
        let mut modal = many_hits_modal(OVERFLOWING_MATCHES);
        let _ = screen(&mut modal, WIDE);
        let list = modal.history.list_area();
        let bottom = mouse(
            MouseEventKind::Down(MouseButton::Left),
            Rect::new(list.right() - 1, list.bottom() - 1, 1, 1),
        );

        let action = modal.handle_mouse(bottom);
        let _ = screen(&mut modal, WIDE);

        assert!(
            matches!(action, SearchAction::Consumed),
            "{BAR_LOST_THE_PRESS}"
        );
        assert!(
            modal.history.scroll() > 0,
            "the press on the bar left the list at the top"
        );
        assert_eq!(modal.history().selected(), 0);
    }

    #[test_case(SearchTab::ThisChat ; "this_chat")]
    #[test_case(SearchTab::Project  ; "project")]
    fn the_query_row_counts_the_selected_match(tab: SearchTab) {
        let mut modal = match tab {
            SearchTab::ThisChat => modal_with_items(),
            _ => many_hits_modal(ITEM_COUNT),
        };
        modal.handle_key(key_event(KeyCode::Down));
        let more = if tab.searches_history() {
            MORE_MARK
        } else {
            ""
        };

        let screen = screen(&mut modal, WIDE);

        let counter = format!("2/{ITEM_COUNT}{more}");
        assert!(screen.contains(&counter), "{counter} missing:\n{screen}");
    }

    #[test_case(WIDE,     true  ; "wide_shows_the_preview")]
    #[test_case((60, 40), false ; "narrow_drops_the_preview")]
    fn preview_pane_needs_room(size: (u16, u16), shown: bool) {
        let mut modal = history_modal(SearchTab::Project, false);

        let screen = screen(&mut modal, size);

        assert_eq!(screen.contains(history::PREVIEW_LOADING), shown, "{screen}");
    }
}
