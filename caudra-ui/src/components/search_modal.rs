use std::cmp::Reverse;

use crate::components::Overlay;
use crate::components::keybindings::key;
use crate::components::modal::Modal;
use crate::components::scrollbar::render_vertical_scrollbar;
use crate::text_buffer::TextBuffer;
use crate::theme;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Modifier;
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
    Close(Option<(u32, bool)>),
}

pub struct SearchModal {
    search: TextBuffer,
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
}

impl SearchModal {
    pub fn new() -> Self {
        Self {
            search: TextBuffer::new(String::new()),
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
        self.search.insert_text(text);
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
            _ => {
                if key::DELETE_WORD.matches(key) {
                    self.search.remove_word_before_cursor();
                } else {
                    self.search.handle_key(key);
                }
                self.invalidate_mouse_geometry();
                SearchAction::QueryChanged
            }
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> SearchAction {
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
        let query = self.search.value();
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

        let content_rows = if self.matches.is_empty() && !self.search.value().is_empty() {
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

        let total = self.matches.len() as u16;
        if total > viewport_h as u16 {
            render_vertical_scrollbar(frame, list_area, total, self.scroll_offset as u16);
        }

        popup
    }

    fn invalidate_mouse_geometry(&mut self) {
        self.row_hits.clear();
        self.mouse_down = None;
    }

    fn render_list(&self, frame: &mut Frame, area: Rect, viewport_height: usize) {
        let t = theme::current();

        if self.matches.is_empty() {
            if !self.search.value().is_empty() {
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
        let t = theme::current();
        let query = self.search.value();
        let cursor_byte = TextBuffer::char_to_byte(&query, self.search.x());
        let (before, rest) = query.split_at(cursor_byte);
        let mut chars = rest.chars();
        let cursor_char = chars.next().unwrap_or(' ');
        let after = chars.as_str();

        let text = super::input_text_style();
        let line = Line::from(vec![
            Span::styled(SEARCH_PREFIX, t.tool_dim),
            Span::styled(before.to_owned(), text),
            Span::styled(cursor_char.to_string(), t.cursor),
            Span::styled(after.to_owned(), text),
        ]);
        frame.render_widget(Paragraph::new(vec![line]), area);
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
    let index_set: std::collections::HashSet<u32> = indices.iter().copied().collect();
    let base_style = if is_selected { t.item_selected } else { t.item };
    let match_style = base_style
        .fg(t.accent.fg.unwrap_or(t.foreground))
        .add_modifier(Modifier::BOLD);

    let mut spans = vec![Span::styled(LABEL_INDENT, base_style)];
    let mut current_highlighted = false;
    let mut run = String::new();

    for (char_pos, ch) in text.chars().enumerate().take(max_width) {
        let is_match = index_set.contains(&(char_pos as u32));

        if is_match != current_highlighted && !run.is_empty() {
            let style = if current_highlighted {
                match_style
            } else {
                base_style
            };
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        current_highlighted = is_match;
        run.push(ch);
    }

    if !run.is_empty() {
        let style = if current_highlighted {
            match_style
        } else {
            base_style
        };
        spans.push(Span::styled(run, style));
    }

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
    use test_case::test_case;

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
        modal.search = TextBuffer::new(query.into());
        modal.update_matches(texts);
        modal
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
        assert!(modal.search.value().is_empty());
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
