//! The search modal's past-sessions tabs: keyword and reranked hits from the stored history on
//! the left, the selected hit in its context on the right.

use std::ops::Range;
use std::time::{SystemTime, UNIX_EPOCH};

use caudra_agent::history::Ranking;
use caudra_grab::grab_scope;
use caudra_storage::history_search::{
    HistoryEntry, HistoryHit, HistoryKind, HistoryRef, HistorySort, HistoryStream, NeighbourPreview,
};
use caudra_storage::id::CaudraId;
use caudra_workbench::keys::{LIST_FIRST, LIST_LAST};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::components::keybindings::key;
use crate::components::list_picker::truncate_label;
use crate::components::scrollbar::{ScrollHint, Scrollbar, ScrollbarMouse};
use crate::components::session_picker::age;
use crate::components::status_bar::collapse_home;
use crate::components::{match_spans, now_secs};
use crate::theme::{self, Theme};

const ROWS_PER_HIT: usize = 2;
const LIST_PERCENT: u32 = 45;
const PERCENT: u32 = 100;
const LIST_MIN_COLS: u16 = 28;
const PREVIEW_MIN_COLS: u16 = 40;
const PANE_GAP: u16 = 1;
const SNIPPET_INDENT: &str = "   ";
const ELLIPSIS: &str = "…";
/// Context kept ahead of a row's first highlight when the snippet has to be cut.
const SNIPPET_LEAD_COLS: usize = 12;
const MILLIS_PER_SECOND: u64 = 1_000;
const PREVIEW_SCROLL_STEP: u16 = 3;
/// Rows of context the preview keeps above the first match it scrolls to.
const PREVIEW_LEAD_ROWS: u16 = 2;
const LOCAL_TIME_FORMAT: &str = "%Y-%m-%d %H:%M";
const SEPARATOR: &str = " · ";
const UNTITLED: &str = "Untitled";
const THIS_SESSION: &str = " (this session)";
const SUBAGENT: &str = "subagent";
const SPINNER_FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
const SPINNER_FRAME_MS: u128 = 120;
pub(super) const EMPTY_PROJECT: &str = "Type to search this project's past sessions";
pub(super) const EMPTY_ALL: &str = "Type to search every project's past sessions";
pub(super) const NO_MATCHES: &str = "No matches. Try fewer words, or every project.";
pub(super) const SEARCHING: &str = "Searching…";
pub(super) const RERANKING: &str = "Reranking…";
pub(super) const PREVIEW_LOADING: &str = "Loading…";
const FILTER_HINT: &str = "Filters: kind: tool: since:7d until:2026-10-01";
const RANKED_BY_RELEVANCE: &str = "best first";
const RANKED_BY_TIME: &str = "newest first";

/// What the last search found, as the status line reports it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HistoryStatus {
    Empty,
    Searching,
    Ready {
        ranking: Ranking,
        judged_unrelated: usize,
        unindexed_sessions: usize,
        note: Option<String>,
    },
    Failed(String),
    Unavailable(String),
}

/// The selected hit in full, as the preview draws it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HistoryPreview {
    pub entry: HistoryEntry,
    pub highlights: Vec<Range<usize>>,
}

pub(super) enum PaneKey {
    Ignored,
    Navigate,
    Open,
    Rerun,
    Consumed,
}

pub(super) enum PaneMouse {
    Ignored,
    Navigate,
    Open,
    Consumed,
}

pub(crate) struct HistoryPane {
    hits: Vec<HistoryHit>,
    more: bool,
    selected: usize,
    scroll: usize,
    viewport_hits: usize,
    pub(crate) status: HistoryStatus,
    reranking: bool,
    sort: HistorySort,
    rerank: bool,
    rerank_available: bool,
    preview: Option<HistoryPreview>,
    preview_scroll: u16,
    preview_rows: u16,
    preview_height: u16,
    preview_anchored: bool,
    this_session: Option<CaudraId>,
    this_project: String,
    list_area: Rect,
    preview_area: Rect,
    row_hits: Vec<(Rect, usize)>,
    mouse_down: Option<usize>,
    scrollbar: Scrollbar,
    preview_scrollbar: Scrollbar,
}

impl HistoryPane {
    pub(super) fn new() -> Self {
        Self {
            hits: Vec::new(),
            more: false,
            selected: 0,
            scroll: 0,
            viewport_hits: 0,
            status: HistoryStatus::Empty,
            reranking: false,
            sort: HistorySort::Relevance,
            rerank: true,
            rerank_available: false,
            preview: None,
            preview_scroll: 0,
            preview_rows: 0,
            preview_height: 0,
            preview_anchored: false,
            this_session: None,
            this_project: String::new(),
            list_area: Rect::default(),
            preview_area: Rect::default(),
            row_hits: Vec::new(),
            mouse_down: None,
            scrollbar: Scrollbar::default(),
            preview_scrollbar: Scrollbar::default(),
        }
    }

    /// Where the search runs from: hits in this session are marked, and hits in this project
    /// leave their project out.
    pub(super) fn bind(&mut self, session: Option<CaudraId>, project: String, rerank: bool) {
        self.this_session = session;
        self.this_project = project;
        self.rerank_available = rerank;
    }

    pub(super) fn reset(&mut self) {
        let (session, project, rerank) = (
            self.this_session,
            std::mem::take(&mut self.this_project),
            self.rerank_available,
        );
        *self = Self::new();
        self.bind(session, project, rerank);
    }

    pub(crate) fn sort(&self) -> HistorySort {
        self.sort
    }

    /// Whether the user wants the decision engine to rerank, and it can.
    pub(crate) fn wants_rerank(&self) -> bool {
        self.rerank && self.rerank_available
    }

    pub(crate) fn hits(&self) -> &[HistoryHit] {
        &self.hits
    }

    pub(crate) fn more(&self) -> bool {
        self.more
    }

    pub(super) fn selected(&self) -> usize {
        self.selected
    }

    #[cfg(test)]
    pub(super) fn scroll(&self) -> usize {
        self.scroll
    }

    #[cfg(test)]
    pub(super) fn list_area(&self) -> Rect {
        self.list_area
    }

    pub(super) fn bar_claims(&self, event: &MouseEvent) -> bool {
        self.scrollbar.claims(event) || self.preview_scrollbar.claims(event)
    }

    /// The text of the list and of the preview, each selected on its own, without their bars.
    pub(super) fn text_panes(&self) -> [Rect; 2] {
        [self.list_area, self.preview_area].map(without_bar)
    }

    pub(crate) fn selected_hit(&self) -> Option<&HistoryHit> {
        self.hits.get(self.selected)
    }

    /// Whether the selection sits on the last hit, where the next page is due.
    pub(crate) fn at_end(&self) -> bool {
        !self.hits.is_empty() && self.selected + 1 >= self.hits.len()
    }

    pub(crate) fn preview_ref(&self) -> Option<&HistoryRef> {
        self.preview
            .as_ref()
            .map(|preview| &preview.entry.reference)
    }

    pub(crate) fn set_status(&mut self, status: HistoryStatus) {
        if !matches!(
            status,
            HistoryStatus::Ready { .. } | HistoryStatus::Searching
        ) {
            self.hits.clear();
            self.more = false;
            self.selected = 0;
            self.scroll = 0;
            self.preview = None;
            self.invalidate_mouse_geometry();
        }
        self.status = status;
    }

    pub(crate) fn set_reranking(&mut self, reranking: bool) {
        self.reranking = reranking;
    }

    /// Whether a search or a rerank is on its way, which keeps the spinner turning.
    pub(super) fn busy(&self) -> bool {
        self.reranking || self.status == HistoryStatus::Searching
    }

    pub(super) fn rerank_offered(&self) -> bool {
        self.rerank_available
    }

    /// Shows `hits`, a fresh answer or, with `append`, the next page of the last one. The
    /// selection stays on the hit it was on when that hit is still listed.
    pub(crate) fn set_hits(
        &mut self,
        hits: Vec<HistoryHit>,
        more: bool,
        append: bool,
        status: HistoryStatus,
    ) {
        let kept = self.selected_hit().map(|hit| hit.reference.clone());
        match append {
            true => self.hits.extend(hits),
            false => self.hits = hits,
        }
        self.more = more;
        self.status = status;
        self.selected = kept
            .and_then(|reference| self.hits.iter().position(|hit| hit.reference == reference))
            .unwrap_or(if append { self.selected } else { 0 });
        if !append && self.selected == 0 {
            self.scroll = 0;
        }
        self.ensure_visible();
        self.invalidate_mouse_geometry();
    }

    pub(crate) fn set_preview(&mut self, preview: Option<HistoryPreview>) {
        self.preview = preview;
        self.preview_scroll = 0;
        self.preview_anchored = false;
    }

    pub(super) fn handle_key(&mut self, key_event: KeyEvent) -> PaneKey {
        let shift = key_event.modifiers == KeyModifiers::SHIFT;
        match key_event.code {
            KeyCode::Enter if !self.hits.is_empty() => return PaneKey::Open,
            KeyCode::Up if shift => self.scroll_preview(-i32::from(PREVIEW_SCROLL_STEP)),
            KeyCode::Down if shift => self.scroll_preview(i32::from(PREVIEW_SCROLL_STEP)),
            KeyCode::Up => self.select(self.selected.checked_sub(1).unwrap_or(self.last())),
            KeyCode::Down => self.select(match self.at_end() {
                true => 0,
                false => self.selected + 1,
            }),
            KeyCode::PageUp => self.select(self.selected.saturating_sub(self.page())),
            KeyCode::PageDown => self.select((self.selected + self.page()).min(self.last())),
            _ if LIST_FIRST.matches(key_event) => self.select(0),
            _ if LIST_LAST.matches(key_event) => self.select(self.last()),
            _ if key::SEARCH_SORT.matches(key_event) => {
                self.sort = match self.sort {
                    HistorySort::Relevance => HistorySort::Newest,
                    HistorySort::Newest => HistorySort::Relevance,
                };
                return PaneKey::Rerun;
            }
            _ if key::SEARCH_RERANK.matches(key_event) && self.rerank_available => {
                self.rerank = !self.rerank;
                return PaneKey::Rerun;
            }
            _ => return PaneKey::Ignored,
        }
        match shift {
            true => PaneKey::Consumed,
            false => PaneKey::Navigate,
        }
    }

    pub(super) fn handle_mouse(&mut self, event: MouseEvent) -> PaneMouse {
        match self.preview_scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return PaneMouse::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.preview_scroll = u16::try_from(top).unwrap_or(u16::MAX);
                self.preview_anchored = true;
                return PaneMouse::Consumed;
            }
        }
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return PaneMouse::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll = (top as usize / ROWS_PER_HIT).min(self.max_scroll());
                self.invalidate_mouse_geometry();
                return PaneMouse::Consumed;
            }
        }
        let position = Position::new(event.column, event.row);
        let under = self
            .row_hits
            .iter()
            .find(|(area, _)| area.contains(position))
            .map(|(_, index)| *index);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.mouse_down = under;
                match under {
                    Some(index) => {
                        self.selected = index;
                        PaneMouse::Navigate
                    }
                    None => PaneMouse::Ignored,
                }
            }
            MouseEventKind::Up(MouseButton::Left) => match self.mouse_down.take() {
                Some(pressed) if under == Some(pressed) => PaneMouse::Open,
                Some(_) => PaneMouse::Consumed,
                None => PaneMouse::Ignored,
            },
            MouseEventKind::Drag(MouseButton::Left) => {
                self.mouse_down = None;
                PaneMouse::Consumed
            }
            _ => PaneMouse::Ignored,
        }
    }

    /// The wheel moves whichever pane the pointer is over, and never the selection.
    pub(super) fn scroll_at(&mut self, position: Position, delta: i32) {
        if self.preview_area.contains(position) {
            self.scroll_preview(-delta);
            return;
        }
        let scroll = match delta > 0 {
            true => self.scroll.saturating_sub(delta.unsigned_abs() as usize),
            false => self.scroll.saturating_add(delta.unsigned_abs() as usize),
        };
        self.scroll = scroll.min(self.max_scroll());
        self.invalidate_mouse_geometry();
    }

    fn scroll_preview(&mut self, delta: i32) {
        let limit = self.preview_rows.saturating_sub(self.preview_height);
        self.preview_scroll = self
            .preview_scroll
            .saturating_add_signed(i16::try_from(delta).unwrap_or_default())
            .min(limit);
        self.preview_anchored = true;
    }

    fn last(&self) -> usize {
        self.hits.len().saturating_sub(1)
    }

    fn page(&self) -> usize {
        self.viewport_hits.max(1)
    }

    fn max_scroll(&self) -> usize {
        self.hits.len().saturating_sub(self.viewport_hits)
    }

    fn select(&mut self, index: usize) {
        if self.hits.is_empty() {
            return;
        }
        self.selected = index.min(self.last());
        self.ensure_visible();
    }

    fn ensure_visible(&mut self) {
        if self.viewport_hits == 0 {
            return;
        }
        let before = self.scroll;
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + self.viewport_hits {
            self.scroll = self.selected + 1 - self.viewport_hits;
        }
        if self.scroll != before {
            self.invalidate_mouse_geometry();
        }
    }

    pub(super) fn invalidate_mouse_geometry(&mut self) {
        self.row_hits.clear();
        self.mouse_down = None;
    }

    pub(super) fn status_line(&self, query_blank: bool, all_projects: bool) -> Line<'static> {
        let t = theme::current();
        let order = match self.sort {
            HistorySort::Relevance => RANKED_BY_RELEVANCE,
            HistorySort::Newest => RANKED_BY_TIME,
        };
        let text = match &self.status {
            HistoryStatus::Empty if query_blank => match all_projects {
                true => format!("{EMPTY_ALL}. {FILTER_HINT}"),
                false => format!("{EMPTY_PROJECT}. {FILTER_HINT}"),
            },
            HistoryStatus::Empty | HistoryStatus::Searching => SEARCHING.to_owned(),
            HistoryStatus::Failed(error) => return Line::styled(error.clone(), t.error),
            HistoryStatus::Unavailable(why) => why.clone(),
            HistoryStatus::Ready { .. } if self.hits.is_empty() => NO_MATCHES.to_owned(),
            HistoryStatus::Ready {
                ranking,
                judged_unrelated,
                unindexed_sessions,
                note,
            } => {
                let mut parts = vec![match ranking {
                    Ranking::Reranked { pool, kept } => {
                        format!("{kept} of {pool} matches reranked, {order}")
                    }
                    Ranking::Keyword { matched } => {
                        let plus = if self.more { "+" } else { "" };
                        format!("{matched}{plus} matches, keyword order, {order}")
                    }
                }];
                if *judged_unrelated > 0 {
                    parts.push(format!(
                        "{judged_unrelated} judged unrelated ({} for keyword order)",
                        key::SEARCH_RERANK.label
                    ));
                }
                if *unindexed_sessions > 0 {
                    parts.push(format!("{unindexed_sessions} sessions not indexed yet"));
                }
                if let Some(note) = note {
                    parts.push(note.clone());
                }
                parts.join(SEPARATOR)
            }
        };
        let mut spans = Vec::new();
        if self.reranking {
            spans.push(Span::styled(
                format!("{} {RERANKING} ", spinner_frame()),
                t.spinner,
            ));
        }
        spans.push(Span::styled(text, t.item_desc));
        Line::from(spans)
    }

    pub(super) fn view(&mut self, frame: &mut Frame, area: Rect) {
        grab_scope!("search_modal_history", area);
        let (list, preview) = panes(area);
        self.list_area = list;
        self.preview_area = preview;
        let viewport_hits = usize::from(list.height) / ROWS_PER_HIT;
        if viewport_hits != self.viewport_hits {
            self.viewport_hits = viewport_hits;
            self.ensure_visible();
        }
        self.scroll = self.scroll.min(self.max_scroll());
        self.render_list(frame, list);
        match preview.width > 0 {
            true => self.render_preview(frame, preview),
            false => self.preview_scrollbar.draw(frame, preview, 0u32, 0u32),
        }
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect) {
        let t = theme::current();
        let width = usize::from(area.width.saturating_sub(1));
        let now = now_secs();
        let end = (self.scroll + self.viewport_hits).min(self.hits.len());
        let mut lines = Vec::with_capacity((end - self.scroll) * ROWS_PER_HIT);
        self.row_hits.clear();
        for index in self.scroll..end {
            let hit = &self.hits[index];
            let selected = index == self.selected;
            let row = u16::try_from((index - self.scroll) * ROWS_PER_HIT).unwrap_or(u16::MAX);
            self.row_hits.push((
                Rect::new(
                    area.x,
                    area.y.saturating_add(row),
                    area.width.saturating_sub(1),
                    ROWS_PER_HIT as u16,
                ),
                index,
            ));
            lines.push(self.title_line(hit, selected, width, now, &t));
            lines.push(snippet_line(hit, selected, width, &t));
        }
        frame.render_widget(Paragraph::new(lines), area);
        self.scrollbar.set_hint(ScrollHint::matches(
            u32::try_from(self.scroll + 1).unwrap_or(u32::MAX),
            u32::try_from(self.hits.len()).unwrap_or(u32::MAX),
        ));
        self.scrollbar.draw(
            frame,
            area,
            u32::try_from(self.hits.len() * ROWS_PER_HIT).unwrap_or(u32::MAX),
            u32::try_from(self.scroll * ROWS_PER_HIT).unwrap_or(u32::MAX),
        );
    }

    fn title_line(
        &self,
        hit: &HistoryHit,
        selected: bool,
        width: usize,
        now: u64,
        t: &Theme,
    ) -> Line<'static> {
        let (base, dim) = match selected {
            true => (t.item_selected, t.item_selected),
            false => (t.item, t.item_desc),
        };
        let when = age(now.saturating_sub(hit.at_ms / MILLIS_PER_SECOND));
        let mut detail = String::new();
        if self.this_session == Some(hit.reference.session) {
            detail.push_str(THIS_SESSION);
        }
        if hit.project != self.this_project {
            detail.push_str(SEPARATOR);
            detail.push_str(&collapse_home(&hit.project));
        }
        let glyph = format!("{} ", kind_glyph(hit.kind));
        let title = title_of(&hit.session_title);
        let room = width.saturating_sub(glyph.width() + when.width() + 1);
        let detail = truncate_label(&detail, room.saturating_sub(title.width().min(room / 2)));
        let title = truncate_label(title, room.saturating_sub(detail.width()));
        let pad = room.saturating_sub(title.width() + detail.width());
        Line::from(vec![
            Span::styled(glyph, dim),
            Span::styled(title, base),
            Span::styled(detail, dim),
            Span::styled(format!("{} {when}", " ".repeat(pad)), dim),
        ])
    }

    fn render_preview(&mut self, frame: &mut Frame, area: Rect) {
        grab_scope!("search_modal_preview", area);
        let t = theme::current();
        let Some(preview) = &self.preview else {
            let line = Line::styled(
                match self.hits.is_empty() {
                    true => "",
                    false => PREVIEW_LOADING,
                },
                t.item_desc,
            );
            frame.render_widget(Paragraph::new(line), area);
            self.preview_rows = 0;
            self.preview_scrollbar.draw(frame, area, 0u32, 0u32);
            return;
        };
        let width = usize::from(area.width.saturating_sub(1));
        let mut head = preview_header(&preview.entry, &self.this_project, width, &t);
        head.extend(
            preview
                .entry
                .before
                .iter()
                .map(|neighbour| neighbour_line(neighbour, width, &t)),
        );
        let lead = head.len();
        let body = highlighted_lines(&preview.entry, &preview.highlights, &t);
        let first_match = first_highlight_line(&preview.entry.text, &preview.highlights);
        let mut lines = head;
        lines.extend(body);
        if let Some(next) = preview.entry.next_offset {
            lines.push(Line::styled(
                format!(
                    "{ELLIPSIS} {} more bytes",
                    preview.entry.total_bytes.saturating_sub(next)
                ),
                t.item_desc,
            ));
        }
        lines.extend(
            preview
                .entry
                .after
                .iter()
                .map(|neighbour| neighbour_line(neighbour, width, &t)),
        );
        let wrap_width = area.width.saturating_sub(1).max(1);
        if !self.preview_anchored {
            self.preview_anchored = true;
            let before_match = Paragraph::new(lines[..lead + first_match].to_vec())
                .wrap(Wrap { trim: false })
                .line_count(wrap_width);
            let before_match = u16::try_from(before_match).unwrap_or(u16::MAX);
            self.preview_scroll = match before_match + PREVIEW_LEAD_ROWS < area.height {
                true => 0,
                false => before_match.saturating_sub(PREVIEW_LEAD_ROWS),
            };
        }
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        self.preview_rows = u16::try_from(paragraph.line_count(wrap_width)).unwrap_or(u16::MAX);
        self.preview_height = area.height;
        self.preview_scroll = self
            .preview_scroll
            .min(self.preview_rows.saturating_sub(area.height));
        let text_area = Rect {
            width: wrap_width,
            ..area
        };
        frame.render_widget(paragraph.scroll((self.preview_scroll, 0)), text_area);
        self.preview_scrollbar.set_hint(ScrollHint::lines(
            u32::from(self.preview_scroll) + 1,
            u32::from(self.preview_rows),
        ));
        self.preview_scrollbar
            .draw(frame, area, self.preview_rows, self.preview_scroll);
    }
}

/// `area` without the column its scrollbar is drawn in.
pub(super) fn without_bar(area: Rect) -> Rect {
    Rect {
        width: area.width.saturating_sub(1),
        ..area
    }
}

/// The list and the preview beside it, or the list alone when both would not fit.
fn panes(area: Rect) -> (Rect, Rect) {
    if area.width < LIST_MIN_COLS + PANE_GAP + PREVIEW_MIN_COLS {
        return (area, Rect::default());
    }
    let share = u16::try_from(u32::from(area.width) * LIST_PERCENT / PERCENT).unwrap_or(u16::MAX);
    let list = share.clamp(LIST_MIN_COLS, area.width - PANE_GAP - PREVIEW_MIN_COLS);
    let [list, _, preview] = Layout::horizontal([
        Constraint::Length(list),
        Constraint::Length(PANE_GAP),
        Constraint::Min(PREVIEW_MIN_COLS),
    ])
    .areas(area);
    (list, preview)
}

fn kind_glyph(kind: HistoryKind) -> &'static str {
    match kind {
        HistoryKind::User => "❯",
        HistoryKind::Assistant => "●",
        HistoryKind::Summary => "≡",
        HistoryKind::Reasoning => "∴",
        HistoryKind::ToolCall => "⚙",
        HistoryKind::ToolResult => "↳",
    }
}

fn kind_label(kind: HistoryKind, tool: Option<&str>) -> String {
    match (kind, tool) {
        (HistoryKind::ToolCall, Some(tool)) => format!("{tool} call"),
        (HistoryKind::ToolResult, Some(tool)) => format!("{tool} result"),
        (HistoryKind::ToolCall, None) => "tool call".to_owned(),
        (HistoryKind::ToolResult, None) => "tool result".to_owned(),
        (HistoryKind::User, _) => "you".to_owned(),
        (HistoryKind::Assistant, _) => "reply".to_owned(),
        (HistoryKind::Summary, _) => "compaction summary".to_owned(),
        (HistoryKind::Reasoning, _) => "reasoning".to_owned(),
    }
}

fn title_of(title: &str) -> &str {
    match title.trim().is_empty() {
        true => UNTITLED,
        false => title,
    }
}

/// `text` with every control character, newlines included, as spaces of the same byte length,
/// so highlight ranges still land on it.
pub(super) fn one_line(text: &str) -> String {
    text.chars()
        .map(|character| match character.is_control() {
            true => " ".repeat(character.len_utf8()),
            false => character.to_string(),
        })
        .collect()
}

/// The snippet on one row, cut to show its first highlight with a little context before it.
fn snippet_line(hit: &HistoryHit, selected: bool, width: usize, t: &Theme) -> Line<'static> {
    let (base, matched) = match selected {
        true => (t.item_selected, t.item_match_selected),
        false => (t.item_desc, t.item_match),
    };
    let text = one_line(&hit.snippet.text);
    let first = hit
        .snippet
        .highlights
        .first()
        .map_or(0, |range| range.start);
    let lead_bytes = text[..first]
        .char_indices()
        .rev()
        .nth(SNIPPET_LEAD_COLS)
        .map_or(0, |(at, _)| at);
    let room = width.saturating_sub(SNIPPET_INDENT.width());
    let start = match text[..first].width() + SNIPPET_LEAD_COLS > room {
        true => lead_bytes,
        false => 0,
    };
    let shown = text[start..].trim_start();
    let start = text.len() - shown.len();
    let prefix = if start > 0 { ELLIPSIS } else { "" };
    let indices = char_indices(shown, &hit.snippet.highlights, start);
    let mut spans = vec![Span::styled(format!("{SNIPPET_INDENT}{prefix}"), base)];
    spans.extend(match_spans(
        shown,
        &indices,
        base,
        matched,
        Some(room.saturating_sub(prefix.width())),
    ));
    Line::from(spans)
}

/// The char positions in `text`, which starts `offset` bytes into the text `ranges` index, that
/// any range covers.
fn char_indices(text: &str, ranges: &[Range<usize>], offset: usize) -> Vec<u32> {
    text.char_indices()
        .enumerate()
        .filter(|(_, (at, _))| ranges.iter().any(|range| range.contains(&(at + offset))))
        .map(|(index, _)| u32::try_from(index).unwrap_or(u32::MAX))
        .collect()
}

fn preview_header(
    entry: &HistoryEntry,
    this_project: &str,
    width: usize,
    t: &Theme,
) -> Vec<Line<'static>> {
    let mut detail = vec![kind_label(entry.kind, entry.tool.as_deref())];
    if matches!(entry.reference.stream, HistoryStream::Subagent(_)) {
        detail.push(SUBAGENT.to_owned());
    }
    detail.push(local_time(entry.at_ms));
    if entry.project != this_project {
        detail.push(collapse_home(&entry.project));
    }
    vec![
        Line::styled(
            truncate_label(title_of(&entry.session_title), width),
            t.panel_title,
        ),
        Line::styled(truncate_label(&detail.join(SEPARATOR), width), t.item_desc),
        Line::default(),
    ]
}

fn neighbour_line(neighbour: &NeighbourPreview, width: usize, t: &Theme) -> Line<'static> {
    let text = format!(
        "{} {}",
        kind_glyph(neighbour.kind),
        one_line(&neighbour.preview).trim()
    );
    Line::styled(truncate_label(&text, width), t.tool_dim)
}

fn highlighted_lines(
    entry: &HistoryEntry,
    highlights: &[Range<usize>],
    t: &Theme,
) -> Vec<Line<'static>> {
    let base = match entry.kind {
        HistoryKind::User => t.user,
        HistoryKind::Reasoning => t.thinking,
        HistoryKind::ToolCall | HistoryKind::ToolResult => t.tool,
        HistoryKind::Assistant | HistoryKind::Summary => t.assistant,
    };
    let mut lines = Vec::new();
    let mut at = 0;
    for raw in entry.text.split('\n') {
        lines.push(spans_with(raw, at, highlights, base, t.item_match));
        at += raw.len() + 1;
    }
    lines
}

/// `line`, which starts `offset` bytes into the text `ranges` index, with the ranges marked.
fn spans_with(
    line: &str,
    offset: usize,
    ranges: &[Range<usize>],
    base: Style,
    matched: Style,
) -> Line<'static> {
    let line_text = one_line(line);
    let end = offset + line.len();
    let mut spans = Vec::new();
    let mut cursor = 0;
    for range in ranges
        .iter()
        .filter(|range| range.end > offset && range.start < end)
    {
        let start = range.start.max(offset) - offset;
        let stop = range.end.min(end) - offset;
        if start > cursor {
            spans.push(Span::styled(line_text[cursor..start].to_owned(), base));
        }
        spans.push(Span::styled(line_text[start..stop].to_owned(), matched));
        cursor = stop;
    }
    if cursor < line_text.len() {
        spans.push(Span::styled(line_text[cursor..].to_owned(), base));
    }
    Line::from(spans)
}

/// Which line of `text` holds the first highlight.
fn first_highlight_line(text: &str, highlights: &[Range<usize>]) -> usize {
    highlights.first().map_or(0, |range| {
        text[..range.start.min(text.len())].matches('\n').count()
    })
}

fn local_time(at_ms: u64) -> String {
    i64::try_from(at_ms)
        .ok()
        .and_then(|ms| Timestamp::from_millisecond(ms).ok())
        .map(|at| {
            at.to_zoned(TimeZone::system())
                .strftime(LOCAL_TIME_FORMAT)
                .to_string()
        })
        .unwrap_or_default()
}

fn spinner_frame() -> &'static str {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    SPINNER_FRAMES[(elapsed / SPINNER_FRAME_MS) as usize % SPINNER_FRAMES.len()]
}
