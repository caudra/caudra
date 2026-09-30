use std::cmp::Reverse;

use crate::components::modal::Modal;
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::{Overlay, field_styles, input_text_style, match_spans};
use crate::theme;
use caudra_grab::grab_scope;
use caudra_workbench::keys::{LIST_FIRST, LIST_LAST};
use caudra_workbench::text_field::{FieldKind, TextField, TextKey};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

const MODAL_TITLE: &str = " Search ";
const MODAL_WIDTH_PERCENT: u16 = 50;
const MODAL_MAX_HEIGHT_PERCENT: u16 = 60;
const SEARCH_ROW: u16 = 1;
const SEARCH_PREFIX: &str = "/ ";
const NO_MATCHES: &str = "  No matches";
const LABEL_INDENT: &str = "  ";

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
}

pub struct SearchModal {
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
    row_hits: Vec<SearchRowHit>,
    mouse_down: Option<usize>,
    scrollbar: Scrollbar,
}

impl SearchModal {
    pub fn new() -> Self {
        Self {
            search: TextField::new(FieldKind::Line),
            matches: Vec::new(),
            selected: 0,
            scroll_offset: 0,
            viewport_height: 0,
            open: false,
            saved_scroll: None,
            matcher: Matcher::new(Config::DEFAULT),
            popup: Rect::default(),
            row_hits: Vec::new(),
            mouse_down: None,
            scrollbar: Scrollbar::default(),
        }
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.popup.contains(pos)
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

    pub fn open(&mut self, scroll_top: u32, auto_scroll: bool) {
        self.reset();
        self.open = true;
        self.saved_scroll = Some((scroll_top, auto_scroll));
    }

    pub fn close(&mut self) {
        self.reset();
    }

    fn reset(&mut self) {
        self.open = false;
        self.search.clear();
        self.matches.clear();
        self.selected = 0;
        self.scroll_offset = 0;
        self.saved_scroll = None;
        self.invalidate_mouse_geometry();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn handle_paste(&mut self, text: &str) {
        self.search.paste(text);
        self.invalidate_mouse_geometry();
    }

    /// Leaving the search behind without taking a match, which owes the
    /// transcript the scroll position it was opened from.
    pub fn cancel(&mut self) -> SearchAction {
        SearchAction::Close(self.saved_scroll.take())
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> SearchAction {
        match key.code {
            KeyCode::Esc => self.cancel(),
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
            _ if LIST_FIRST.matches(key) => {
                self.select(0);
                SearchAction::Navigate
            }
            _ if LIST_LAST.matches(key) => {
                self.select(self.matches.len().saturating_sub(1));
                SearchAction::Navigate
            }
            _ => {
                let edit = self.search.handle_key(key);
                if edit.changed() {
                    self.invalidate_mouse_geometry();
                }
                match edit {
                    TextKey::Changed => SearchAction::QueryChanged,
                    TextKey::Copy(text) => SearchAction::Copy(text),
                    TextKey::Cut(text) => SearchAction::Cut(text),
                    TextKey::Handled | TextKey::Refused | TextKey::Ignored => {
                        SearchAction::Consumed
                    }
                }
            }
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> SearchAction {
        match self.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return SearchAction::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll_offset = top as usize;
                self.invalidate_mouse_geometry();
                return SearchAction::Consumed;
            }
        }
        let position = Position::new(event.column, event.row);
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
        self.matches.get(self.selected).map(|m| m.segment_index)
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

        let content_rows = if self.matches.is_empty() && !self.search.is_empty() {
            1
        } else {
            self.matches.len() as u16
        };

        let modal = Modal {
            title: MODAL_TITLE,
            width_percent: MODAL_WIDTH_PERCENT,
            max_height_percent: MODAL_MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, content_rows + SEARCH_ROW);
        let viewport_h = inner.height.saturating_sub(SEARCH_ROW) as usize;
        self.viewport_height = viewport_h;
        self.popup = popup;

        let [list_area, search_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

        self.row_hits.clear();
        let end = (self.scroll_offset + viewport_h).min(self.matches.len());
        self.row_hits.extend(
            self.matches[self.scroll_offset..end]
                .iter()
                .enumerate()
                .map(|(row, m)| SearchRowHit {
                    area: Rect::new(list_area.x, list_area.y + row as u16, list_area.width, 1),
                    match_index: self.scroll_offset + row,
                    segment_index: m.segment_index,
                }),
        );
        self.render_list(frame, list_area, viewport_h);
        self.render_search(frame, search_area);

        self.scrollbar.draw(
            frame,
            list_area,
            self.matches.len() as u32,
            self.scroll_offset as u32,
        );

        popup
    }

    fn invalidate_mouse_geometry(&mut self) {
        self.row_hits.clear();
        self.mouse_down = None;
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

    fn render_search(&self, frame: &mut Frame, area: Rect) {
        grab_scope!("search_modal_search", area);
        let prefix = Span::styled(SEARCH_PREFIX, theme::current().tool_dim);
        let width = usize::from(area.width).saturating_sub(prefix.width());
        let mut line = self
            .search
            .paint(width, &field_styles(input_text_style()), true, "");
        line.spans.insert(0, prefix);
        frame.render_widget(Paragraph::new(line), area);
    }
}

impl Overlay for SearchModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
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
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
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
}
