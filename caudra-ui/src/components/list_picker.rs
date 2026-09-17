use std::cmp::Reverse;
use std::collections::HashMap;

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::animation::{animation_elapsed_ms, spinner_str};
use crate::components::keybindings::key;
use crate::components::modal::Modal;
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::{Hint, HintBar, Overlay};
use crate::repaint::Cadence;
use crate::text_buffer::{EditResult, TextBuffer};
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const NO_MATCHES: &str = "No matches";
const MIN_WIDTH_PERCENT: u16 = 65;
const MAX_HEIGHT_PERCENT: u16 = 80;
const SEARCH_ROW: u16 = 1;
const DETAIL_RIGHT_PAD: u16 = 1;
const DETAIL_DIM: f32 = 0.4;
pub(crate) const DISABLED_DIM: f32 = 0.45;

pub trait PickerItem {
    fn label(&self) -> &str;
    fn search_text(&self) -> &str {
        self.label()
    }
    fn suffix(&self) -> Option<&str> {
        None
    }
    fn detail(&self) -> Option<&str> {
        None
    }
    fn section(&self) -> Option<&str> {
        None
    }
    fn is_spinning(&self) -> bool {
        false
    }
    fn is_highlighted(&self) -> bool {
        false
    }
    /// Rendered dimmed to say "listed here, but inert in this context". The
    /// row stays selectable so whoever owns the action can explain the refusal.
    fn is_disabled(&self) -> bool {
        false
    }
}

impl PickerItem for String {
    fn label(&self) -> &str {
        self
    }
}

pub enum PickerAction<T> {
    Consumed,
    Select(T),
    Toggle(usize, bool),
    Close,
    /// A footer hint was clicked. The picker does not know what its owner's
    /// keys mean, so the owner feeds this back into its own `handle_key`.
    Key(KeyEvent),
}

/// The footer a picker draws under its search row. Built on every frame
/// because its owner's binds are consts and its hits are placed at draw time.
pub type FooterBuilder = fn() -> Vec<Hint>;

pub struct ListPicker<T> {
    state: Option<State<T>>,
    title: String,
    max_visible: Option<u16>,
    footer: Option<FooterBuilder>,
    error_text: Option<String>,
    info_text: Option<String>,
    empty_text: &'static str,
    width_percent: u16,
    relevance_order: bool,
}

struct State<T> {
    items: Vec<T>,
    filtered: Vec<usize>,
    selected: usize,
    search: TextBuffer,
    scroll_offset: usize,
    viewport_height: usize,
    popup_area: Rect,
    row_hits: Vec<PickerRowHit>,
    mouse_down: Option<usize>,
    scrollbar: Scrollbar,
    footer: HintBar,
    enabled: Option<Vec<bool>>,
    toggleable: Option<Vec<bool>>,
    matcher: Matcher,
    relevance_order: bool,
}

#[derive(Clone, Copy)]
struct PickerRowHit {
    area: Rect,
    filtered_index: usize,
    item_index: usize,
}

#[derive(Clone, Copy)]
struct RenderOptions<'a> {
    max_visible: Option<u16>,
    width_percent: u16,
    empty_text: &'a str,
}

#[derive(Clone, Copy)]
struct RenderContent<'a> {
    footer: Option<FooterBuilder>,
    error_text: Option<&'a str>,
    info_text: Option<&'a str>,
}

impl<T: PickerItem> State<T> {
    fn new(items: Vec<T>, relevance_order: bool) -> Self {
        let filtered = (0..items.len()).collect();
        Self {
            items,
            filtered,
            selected: 0,
            search: TextBuffer::new(String::new()),
            scroll_offset: 0,
            viewport_height: 20,
            popup_area: Rect::default(),
            row_hits: Vec::new(),
            mouse_down: None,
            scrollbar: Scrollbar::default(),
            footer: HintBar::default(),
            enabled: None,
            toggleable: None,
            matcher: Matcher::new(Config::DEFAULT),
            relevance_order,
        }
    }

    fn replace_items(&mut self, items: Vec<T>) {
        self.items = items;
        self.invalidate_mouse_geometry();
        self.rebuild_filter();
        self.clamp_selection();
    }

    fn invalidate_mouse_geometry(&mut self) {
        self.row_hits.clear();
        self.mouse_down = None;
    }

    fn rebuild_filter(&mut self) {
        let query = self.search.value();
        if query.is_empty() {
            self.filtered = (0..self.items.len()).collect();
            return;
        }
        let pattern = Pattern::new(
            &query,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );
        // Ranking needs every score attached to its own index. `match_list`
        // returns matched labels, and `ModelEntry` and `PermissionEntry` both
        // repeat labels, so a label cannot be mapped back to one index.
        let mut buf = Vec::new();
        let items = &self.items;
        let matcher = &mut self.matcher;
        let mut scored: Vec<(usize, u32)> = items
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| {
                pattern
                    .score(Utf32Str::new(item.search_text(), &mut buf), matcher)
                    .map(|score| (idx, score))
            })
            .collect();
        if self.relevance_order {
            rank_by_relevance(&mut scored, &self.items);
        }
        self.filtered = scored.into_iter().map(|(idx, _)| idx).collect();
    }

    fn clamp_selection(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
            self.scroll_offset = 0;
        } else {
            self.selected = self.selected.min(self.filtered.len() - 1);
            self.ensure_visible();
        }
    }

    fn update_search_and_clamp(&mut self) {
        self.invalidate_mouse_geometry();
        self.rebuild_filter();
        self.clamp_selection();
    }

    fn move_up(&mut self) {
        let len = self.filtered.len();
        if len == 0 {
            return;
        }
        self.selected = if self.selected == 0 {
            len - 1
        } else {
            self.selected - 1
        };
        self.ensure_visible();
    }

    fn page_up(&mut self) {
        let len = self.filtered.len();
        if len == 0 {
            return;
        }
        let step = self.viewport_height.max(1);
        self.selected = self.selected.saturating_sub(step);
        self.ensure_visible();
    }

    fn page_down(&mut self) {
        let len = self.filtered.len();
        if len == 0 {
            return;
        }
        let step = self.viewport_height.max(1);
        self.selected = (self.selected + step).min(len - 1);
        self.ensure_visible();
    }

    fn select_first(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = 0;
        self.ensure_visible();
    }

    fn select_last(&mut self) {
        let len = self.filtered.len();
        if len == 0 {
            return;
        }
        self.selected = len - 1;
        self.ensure_visible();
    }

    fn move_down(&mut self) {
        let len = self.filtered.len();
        if len == 0 {
            return;
        }
        self.selected = if self.selected == len - 1 {
            0
        } else {
            self.selected + 1
        };
        self.ensure_visible();
    }

    fn ensure_visible(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
        let visual = visual_rows_in_range(
            &self.filtered,
            &self.items,
            self.scroll_offset,
            self.selected + 1,
        );
        if visual > self.viewport_height {
            self.scroll_offset = find_scroll_offset_for(
                &self.filtered,
                &self.items,
                self.selected,
                self.viewport_height,
            );
        }
        let max_offset =
            find_scroll_offset_for_bottom(&self.filtered, &self.items, self.viewport_height);
        self.scroll_offset = self.scroll_offset.min(max_offset);
    }

    /// The bar counts visual rows, which section headers stretch past the item
    /// count, so a dragged row is walked back to the item that owns it.
    fn scroll_to_visual(&mut self, visual_offset: usize) {
        let max_offset =
            find_scroll_offset_for_bottom(&self.filtered, &self.items, self.viewport_height);
        let mut rows = 0;
        self.scroll_offset = max_offset;
        for offset in 0..self.filtered.len() {
            rows += 1 + section_gap(&self.filtered, &self.items, offset, 0);
            if rows > visual_offset {
                self.scroll_offset = offset.min(max_offset);
                break;
            }
        }
        self.invalidate_mouse_geometry();
    }

    fn selected_item_index(&self) -> Option<usize> {
        self.filtered.get(self.selected).copied()
    }
}

impl<T: PickerItem> ListPicker<T> {
    pub fn new() -> Self {
        Self {
            state: None,
            title: String::new(),
            max_visible: None,
            footer: None,
            error_text: None,
            info_text: None,
            empty_text: NO_MATCHES,
            width_percent: MIN_WIDTH_PERCENT,
            relevance_order: false,
        }
    }

    pub fn with_max_visible(mut self, max: u16) -> Self {
        self.max_visible = Some(max);
        self
    }

    /// Ranks matches by fuzzy score instead of leaving them in the order the
    /// items were supplied. Off by default: most pickers carry meaning in that
    /// order, such as recency, chronology, or a fixed menu.
    pub fn with_relevance_order(mut self) -> Self {
        self.relevance_order = true;
        self
    }

    pub fn with_width_percent(mut self, width_percent: u16) -> Self {
        self.width_percent = width_percent.clamp(1, 100);
        self
    }

    pub fn with_footer_builder(mut self, builder: FooterBuilder) -> Self {
        self.footer = Some(builder);
        self
    }

    pub fn set_footer_builder(&mut self, builder: FooterBuilder) {
        self.footer = Some(builder);
    }

    pub fn set_title(&mut self, title: impl Into<String>) {
        self.title = title.into();
    }

    pub fn open_selectively_toggleable(
        &mut self,
        items: Vec<T>,
        enabled: Vec<bool>,
        toggleable: Vec<bool>,
        title: impl Into<String>,
    ) {
        assert_eq!(
            items.len(),
            enabled.len(),
            "items and enabled must have same length"
        );
        assert_eq!(
            items.len(),
            toggleable.len(),
            "items and toggleable must have same length"
        );
        self.title = title.into();
        let mut state = State::new(items, self.relevance_order);
        state.enabled = Some(enabled);
        state.toggleable = Some(toggleable);
        self.state = Some(state);
    }

    pub fn open(&mut self, items: Vec<T>, title: impl Into<String>) {
        self.title = title.into();
        self.state = Some(State::new(items, self.relevance_order));
    }

    pub fn select(&mut self, index: usize) {
        if let Some(s) = self.state.as_mut() {
            s.invalidate_mouse_geometry();
            s.selected = index.min(s.filtered.len().saturating_sub(1));
            s.ensure_visible();
        }
    }

    pub fn select_item_by(&mut self, predicate: impl Fn(&T) -> bool) -> bool {
        let Some(s) = self.state.as_mut() else {
            return false;
        };
        s.invalidate_mouse_geometry();
        let Some(selected) = s
            .filtered
            .iter()
            .position(|&item_idx| predicate(&s.items[item_idx]))
        else {
            return false;
        };
        s.selected = selected;
        s.ensure_visible();
        true
    }

    pub fn set_error_text(&mut self, text: Option<String>) {
        self.error_text = text;
    }

    #[cfg(test)]
    pub fn error_text(&self) -> Option<&str> {
        self.error_text.as_deref()
    }

    pub fn set_info_text(&mut self, text: Option<String>) {
        self.info_text = text;
    }

    pub fn set_empty_text(&mut self, text: &'static str) {
        self.empty_text = text;
    }

    pub fn clear_search(&mut self) {
        self.set_search_text("");
    }

    pub fn search_text(&self) -> String {
        self.state
            .as_ref()
            .map(|state| state.search.value())
            .unwrap_or_default()
    }

    pub fn set_search_text(&mut self, text: &str) {
        if let Some(state) = self.state.as_mut() {
            state.search = TextBuffer::new(text.to_string());
            state.search.move_to_end();
            state.update_search_and_clamp();
        }
    }

    pub fn set_search_cursor(&mut self, offset: usize) {
        if let Some(state) = self.state.as_mut() {
            state.search.set_cursor_offset(offset);
        }
    }

    pub fn replace_items(&mut self, items: Vec<T>) {
        if let Some(s) = self.state.as_mut() {
            s.replace_items(items);
        }
    }

    pub fn replace_selectively_toggleable(
        &mut self,
        items: Vec<T>,
        enabled: Vec<bool>,
        toggleable: Vec<bool>,
    ) {
        assert_eq!(items.len(), enabled.len());
        assert_eq!(items.len(), toggleable.len());
        if let Some(s) = self.state.as_mut() {
            s.enabled = Some(enabled);
            s.toggleable = Some(toggleable);
            s.replace_items(items);
        }
    }

    pub fn is_open(&self) -> bool {
        self.state.is_some()
    }

    pub fn cadence(&self) -> Cadence {
        Cadence::when(
            self.state
                .as_ref()
                .is_some_and(|s| s.items.iter().any(PickerItem::is_spinning)),
            Cadence::SPINNER,
        )
    }

    pub fn close(&mut self) {
        self.state = None;
    }

    /// The whole popup, border included: the frame is part of the picker, so a
    /// press on it is a near miss rather than a press outside.
    pub fn contains(&self, pos: Position) -> bool {
        self.state
            .as_ref()
            .is_some_and(|s| s.popup_area.contains(pos))
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickerAction<T> {
        if self.state.is_none() {
            return PickerAction::Close;
        }
        self.handle_ready_key(key)
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> PickerAction<T> {
        let Some(state) = self.state.as_mut() else {
            return PickerAction::Close;
        };
        match state.scrollbar.handle(&event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return PickerAction::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                state.scroll_to_visual(top as usize);
                return PickerAction::Consumed;
            }
        }
        if let Some(key) = state.footer.handle_mouse(event) {
            return PickerAction::Key(key);
        }
        self.handle_list_mouse(event)
    }

    /// The footer alone. For an owner whose list is deaf for the moment, such
    /// as a rename in progress, but whose footer still names the keys that
    /// end it.
    pub fn handle_footer_mouse(&mut self, event: MouseEvent) -> Option<KeyEvent> {
        self.state.as_mut()?.footer.handle_mouse(event)
    }

    fn handle_list_mouse(&mut self, event: MouseEvent) -> PickerAction<T> {
        let Some(state) = self.state.as_mut() else {
            return PickerAction::Close;
        };
        let position = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                state.mouse_down = None;
                if let Some(hit) = state
                    .row_hits
                    .iter()
                    .find(|hit| hit.area.contains(position))
                    .copied()
                {
                    state.selected = hit.filtered_index;
                    state.mouse_down = Some(hit.item_index);
                }
                PickerAction::Consumed
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                state.mouse_down = None;
                PickerAction::Consumed
            }
            MouseEventKind::Moved => {
                if let Some(hit) = state
                    .row_hits
                    .iter()
                    .find(|hit| hit.area.contains(position))
                {
                    state.selected = hit.filtered_index;
                }
                PickerAction::Consumed
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(pressed) = state.mouse_down.take() else {
                    return PickerAction::Consumed;
                };
                let released = state
                    .row_hits
                    .iter()
                    .find(|hit| hit.area.contains(position))
                    .map(|hit| hit.item_index);
                if released != Some(pressed) {
                    return PickerAction::Consumed;
                }
                if let Some(enabled) = &mut state.enabled {
                    if state
                        .toggleable
                        .as_ref()
                        .is_some_and(|toggleable| !toggleable[pressed])
                    {
                        return PickerAction::Consumed;
                    }
                    enabled[pressed] = !enabled[pressed];
                    return PickerAction::Toggle(pressed, enabled[pressed]);
                }
                let mut state = self.state.take().expect("picker state disappeared");
                PickerAction::Select(state.items.swap_remove(pressed))
            }
            _ => PickerAction::Consumed,
        }
    }

    fn handle_ready_key(&mut self, key: KeyEvent) -> PickerAction<T> {
        let s = self
            .state
            .as_mut()
            .expect("handle_ready_key called without state");
        s.invalidate_mouse_geometry();

        if key::QUIT.matches(key) {
            self.state = None;
            return PickerAction::Close;
        }
        if key::SCROLL_HALF_UP.matches(key) {
            s.page_up();
            return PickerAction::Consumed;
        }
        match key.code {
            KeyCode::Up => {
                s.move_up();
                PickerAction::Consumed
            }
            KeyCode::Down => {
                s.move_down();
                PickerAction::Consumed
            }
            KeyCode::PageUp => {
                s.page_up();
                PickerAction::Consumed
            }
            KeyCode::PageDown => {
                s.page_down();
                PickerAction::Consumed
            }
            KeyCode::Home => {
                s.select_first();
                PickerAction::Consumed
            }
            KeyCode::End => {
                s.select_last();
                PickerAction::Consumed
            }
            KeyCode::Enter => {
                let idx = s.selected_item_index();
                if let (Some(enabled), Some(idx)) = (&mut s.enabled, idx) {
                    if s.toggleable
                        .as_ref()
                        .is_some_and(|toggleable| !toggleable[idx])
                    {
                        return PickerAction::Consumed;
                    }
                    enabled[idx] = !enabled[idx];
                    return PickerAction::Toggle(idx, enabled[idx]);
                }
                if s.enabled.is_some() {
                    return PickerAction::Consumed;
                }
                match idx {
                    Some(idx) => {
                        let mut state = self.state.take().unwrap();
                        PickerAction::Select(state.items.swap_remove(idx))
                    }
                    None => PickerAction::Consumed,
                }
            }
            KeyCode::Esc => {
                self.state = None;
                PickerAction::Close
            }
            // Everything the list itself does not claim edits the search
            // line, which owns the whole editing keymap.
            _ => {
                if s.search.handle_key(key) == EditResult::Changed {
                    s.update_search_and_clamp();
                }
                PickerAction::Consumed
            }
        }
    }

    pub fn selected_item(&self) -> Option<&T> {
        let s = self.state.as_ref()?;
        s.selected_item_index().map(|i| &s.items[i])
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.state.as_ref().and_then(|s| s.selected_item_index())
    }

    pub fn item(&self, idx: usize) -> Option<&T> {
        self.state.as_ref().and_then(|s| s.items.get(idx))
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        let Some(s) = self.state.as_mut() else {
            return false;
        };
        s.search.insert_text(text);
        s.update_search_and_clamp();
        true
    }

    pub fn scroll(&mut self, delta: i32) {
        let Some(s) = self.state.as_mut() else {
            return;
        };
        s.invalidate_mouse_geometry();
        if delta > 0 {
            s.scroll_offset = s.scroll_offset.saturating_sub(delta as usize);
        } else {
            let total_visual = visual_rows_in_range(&s.filtered, &s.items, 0, s.filtered.len());
            let max_offset = if total_visual <= s.viewport_height {
                0
            } else {
                find_scroll_offset_for_bottom(&s.filtered, &s.items, s.viewport_height)
            };
            s.scroll_offset = (s.scroll_offset + delta.unsigned_abs() as usize).min(max_offset);
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let footer = self.footer;
        match self.state.as_mut() {
            None => Rect::default(),
            Some(s) => render_ready(
                frame,
                area,
                s,
                &self.title,
                RenderOptions {
                    max_visible: self.max_visible,
                    width_percent: self.width_percent,
                    empty_text: self.empty_text,
                },
                RenderContent {
                    footer,
                    error_text: self.error_text.as_deref(),
                    info_text: self.info_text.as_deref(),
                },
            ),
        }
    }
}

impl<T: PickerItem> Overlay for ListPicker<T> {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }

    fn cadence(&self) -> Cadence {
        self.cadence()
    }
}

fn render_ready<T: PickerItem>(
    frame: &mut Frame,
    area: Rect,
    s: &mut State<T>,
    title: &str,
    options: RenderOptions<'_>,
    content: RenderContent<'_>,
) -> Rect {
    let RenderContent {
        footer,
        error_text,
        info_text,
    } = content;
    let footer_rows = if footer.is_some() { 1u16 } else { 0 };
    let content_rows = if s.filtered.is_empty() {
        1
    } else {
        let rows = visual_rows_in_range(&s.filtered, &s.items, 0, s.filtered.len()) as u16;
        match options.max_visible {
            Some(max) => rows.min(max),
            None => rows,
        }
    };
    let error_rows = error_text.is_some() as u16;
    let modal_inner_width = Modal::inner_width(area.width, options.width_percent);
    let requested_info_rows = info_text.map_or(0, |text| {
        Paragraph::new(text)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .line_count(modal_inner_width.max(1)) as u16
    });
    let modal = Modal {
        title,
        width_percent: options.width_percent,
        max_height_percent: MAX_HEIGHT_PERCENT,
    };
    let (popup, inner) = modal.render(
        frame,
        area,
        content_rows + SEARCH_ROW + footer_rows + error_rows + requested_info_rows,
    );
    let info_rows = requested_info_rows.min(
        inner
            .height
            .saturating_sub(error_rows + SEARCH_ROW + footer_rows + 1),
    );
    let viewport_h = inner
        .height
        .saturating_sub(error_rows + info_rows + SEARCH_ROW + footer_rows);
    let viewport_height = viewport_h as usize;
    if s.viewport_height != viewport_height {
        s.viewport_height = viewport_height;
        s.ensure_visible();
    }

    let mut constraints: Vec<Constraint> = Vec::with_capacity(
        3 + footer.is_some() as usize
            + error_text.is_some() as usize
            + info_text.is_some() as usize,
    );
    if error_text.is_some() {
        constraints.push(Constraint::Length(1)); // error line
    }
    if info_rows > 0 {
        constraints.push(Constraint::Length(info_rows));
    }
    constraints.push(Constraint::Min(1)); // list
    constraints.push(Constraint::Length(1)); // search
    if footer.is_some() {
        constraints.push(Constraint::Length(1));
    }

    let areas = Layout::vertical(constraints).split(inner);
    let mut area_idx = 0;

    if let Some(err) = error_text {
        let line = Line::from(Span::styled(
            format!("  Error: {err}"),
            theme::current().error,
        ));
        frame.render_widget(Paragraph::new(vec![line]), areas[area_idx]);
        area_idx += 1;
    }

    if let Some(info) = info_text
        && info_rows > 0
    {
        frame.render_widget(
            Paragraph::new(info)
                .style(theme::current().item_desc)
                .wrap(ratatui::widgets::Wrap { trim: false }),
            areas[area_idx],
        );
        area_idx += 1;
    }

    let list_area = areas[area_idx];
    area_idx += 1;

    let search_area = areas[area_idx];
    area_idx += 1;

    s.row_hits.clear();
    render_list(
        frame,
        list_area,
        &s.filtered,
        &s.items,
        s.selected,
        s.scroll_offset,
        s.viewport_height,
        s.enabled.as_deref(),
        &mut s.row_hits,
        options.empty_text,
    );
    render_search(frame, search_area, &s.search);

    if let Some(build) = footer {
        s.footer.draw(frame, areas[area_idx], build());
    }

    let total_visual = visual_rows_in_range(&s.filtered, &s.items, 0, s.filtered.len());
    let visual_offset = visual_rows_in_range(&s.filtered, &s.items, 0, s.scroll_offset);
    s.scrollbar
        .draw(frame, list_area, total_visual as u32, visual_offset as u32);

    s.popup_area = popup;
    popup
}

/// Orders matches by relevance while keeping each section contiguous, which
/// is what makes the section headers legible: a section is placed by its best
/// match, then its members by their own score. Equal scores fall back to the
/// original index, so duplicate labels keep their authored order.
fn rank_by_relevance<T: PickerItem>(scored: &mut [(usize, u32)], items: &[T]) {
    let mut best: HashMap<Option<&str>, (u32, usize)> = HashMap::new();
    for &(idx, score) in scored.iter() {
        let entry = best
            .entry(items[idx].section())
            .or_insert((score, usize::MAX));
        entry.0 = entry.0.max(score);
        entry.1 = entry.1.min(idx);
    }
    scored.sort_by_key(|&(idx, score)| {
        let (section_best, section_rank) = best[&items[idx].section()];
        (Reverse(section_best), section_rank, Reverse(score), idx)
    });
}

fn section_gap<T: PickerItem>(filtered: &[usize], items: &[T], idx: usize, start: usize) -> usize {
    let Some(sec) = items[filtered[idx]].section() else {
        return 0;
    };
    if idx == start {
        return 1;
    }
    let is_break = items[filtered[idx - 1]]
        .section()
        .is_none_or(|prev| prev != sec);
    if is_break { 2 } else { 0 }
}

fn visual_rows_in_range<T: PickerItem>(
    filtered: &[usize],
    items: &[T],
    start: usize,
    end: usize,
) -> usize {
    let item_count = end.saturating_sub(start);
    let section_rows: usize = (start..end)
        .map(|i| section_gap(filtered, items, i, start))
        .sum();
    item_count + section_rows
}

fn find_scroll_offset_for<T: PickerItem>(
    filtered: &[usize],
    items: &[T],
    target: usize,
    viewport_height: usize,
) -> usize {
    for start in (0..=target).rev() {
        let rows = visual_rows_in_range(filtered, items, start, target + 1);
        if rows > viewport_height {
            return (start + 1).min(target);
        }
    }
    0
}

fn find_scroll_offset_for_bottom<T: PickerItem>(
    filtered: &[usize],
    items: &[T],
    viewport_height: usize,
) -> usize {
    let len = filtered.len();
    if len == 0 {
        return 0;
    }
    find_scroll_offset_for(filtered, items, len - 1, viewport_height)
}

fn truncate_label(label: &str, max_width: usize) -> String {
    if label.width() <= max_width {
        return label.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let target = max_width.saturating_sub(1);
    let mut width = 0;
    let mut result = String::with_capacity(label.len());
    for ch in label.chars() {
        let cw = ch.width().unwrap_or(0);
        if width + cw > target {
            break;
        }
        width += cw;
        result.push(ch);
    }
    result.push('\u{2026}');
    result
}

#[allow(clippy::too_many_arguments)]
fn render_list<T: PickerItem>(
    frame: &mut Frame,
    area: Rect,
    filtered: &[usize],
    items: &[T],
    selected: usize,
    scroll_offset: usize,
    viewport_height: usize,
    enabled: Option<&[bool]>,
    row_hits: &mut Vec<PickerRowHit>,
    empty_text: &str,
) {
    if filtered.is_empty() {
        let line = Line::from(Span::styled(
            format!("  {empty_text}"),
            theme::current().item_desc,
        ));
        frame.render_widget(Paragraph::new(vec![line]), area);
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut i = scroll_offset;
    let mut last_section: Option<&str> = None;

    while lines.len() < viewport_height && i < filtered.len() {
        let item_idx = filtered[i];
        let item = &items[item_idx];

        match item.section() {
            // A section-less row ends the run, so the next sectioned row
            // re-emits its header. `section_gap` already counts it that way.
            None => last_section = None,
            Some(sec) if last_section.is_none_or(|prev| prev != sec) => {
                if !lines.is_empty() && lines.len() < viewport_height {
                    lines.push(Line::raw(""));
                }
                if lines.len() < viewport_height {
                    lines.push(Line::from(Span::styled(
                        format!("  {sec}"),
                        theme::current().keybind_section,
                    )));
                }
                last_section = Some(sec);
            }
            Some(_) => {}
        }

        if lines.len() >= viewport_height {
            break;
        }

        row_hits.push(PickerRowHit {
            area: Rect::new(area.x, area.y + lines.len() as u16, area.width, 1),
            filtered_index: i,
            item_index: item_idx,
        });

        let highlighted = item.is_highlighted();
        let t = theme::current();
        let (mut style, mut detail_style) = match (i == selected, highlighted) {
            (true, true) => {
                let s = t.item_match_selected;
                (s, theme::dim_style(s, DETAIL_DIM))
            }
            (true, false) => (t.item_selected, t.item_selected),
            (false, true) => (t.accent, theme::dim_style(t.accent, DETAIL_DIM)),
            (false, false) => (t.item, t.item_desc),
        };
        if item.is_disabled() {
            style = theme::dim_style(style, DISABLED_DIM);
            detail_style = theme::dim_style(detail_style, DISABLED_DIM);
        }
        let checkbox = enabled.map(|en| {
            let sym = if en[item_idx] { "✓ " } else { "✗ " };
            let sty = if i == selected {
                style
            } else if en[item_idx] {
                theme::current().item
            } else {
                theme::current().item_desc
            };
            Span::styled(sym, sty)
        });
        let label = format!("  {}", item.label());
        let suffix = item.suffix();
        let detail: Option<&str> = if item.is_spinning() {
            Some(spinner_str(animation_elapsed_ms()))
        } else {
            item.detail()
        };
        let suffix_gap = 2usize;
        let suffix_w = suffix.map(|s| s.width()).unwrap_or(0);
        let trailing_gap = suffix_w + if suffix_w > 0 { suffix_gap } else { 0 };
        let line = match detail {
            Some(detail) => {
                let max_label = area.width.saturating_sub(
                    detail.width() as u16 + trailing_gap as u16 + 1 + DETAIL_RIGHT_PAD,
                ) as usize;
                let label = truncate_label(&label, max_label);
                let pad = (area.width as usize).saturating_sub(
                    label.width() + trailing_gap + detail.width() + DETAIL_RIGHT_PAD as usize + 1,
                );
                let mut spans = Vec::with_capacity(7);
                if let Some(cb) = checkbox {
                    spans.push(cb);
                }
                spans.push(Span::styled(label, style));
                if let Some(s) = suffix {
                    spans.push(Span::styled(" ".repeat(suffix_gap), style));
                    spans.push(Span::styled(s.to_string(), theme::dim_style(style, 0.4)));
                }
                spans.push(Span::styled(" ".repeat(pad), style));
                spans.push(Span::styled(detail.to_string(), detail_style));
                spans.push(Span::styled(" ".repeat(DETAIL_RIGHT_PAD as usize), style));
                Line::from(spans)
            }
            None => {
                let mut spans: Vec<Span> = Vec::with_capacity(4);
                if let Some(cb) = checkbox {
                    spans.push(cb);
                }
                spans.push(Span::styled(label, style));
                if let Some(s) = suffix {
                    spans.push(Span::styled(" ".repeat(suffix_gap), style));
                    spans.push(Span::styled(s.to_string(), theme::dim_style(style, 0.4)));
                }
                Line::from(spans)
            }
        };
        lines.push(line);
        i += 1;
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_search(frame: &mut Frame, area: Rect, search: &TextBuffer) {
    let query = search.value();
    let cursor_x = search.x();
    let chars: Vec<char> = query.chars().collect();
    let before: String = chars[..cursor_x].iter().collect();
    let cursor_char = chars.get(cursor_x).copied().unwrap_or(' ');
    let after_start = cursor_x.saturating_add(1).min(chars.len());
    let after: String = chars[after_start..].iter().collect();

    let text = super::input_text_style();
    let line = Line::from(vec![
        super::chevron_span(),
        Span::styled(before, text),
        Span::styled(cursor_char.to_string(), theme::current().cursor),
        Span::styled(after, text),
    ]);
    frame.render_widget(Paragraph::new(vec![line]), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key;
    use crate::components::keybindings::key as kb;
    use crossterm::event::{KeyCode, KeyModifiers};
    use test_case::test_case;

    const SECTION_A: &str = "A";
    const SECTION_B: &str = "B";
    const BAR_LOST_THE_PRESS: &str = "the row under the scrollbar took the press";
    const QUERY_UNTOUCHED: &str = "the navigation key edited the filter line";
    /// More items than the 80x24 test terminal can show, so the bar has a
    /// track to press on.
    const OVERFLOWING_ITEMS: usize = 50;

    fn ready_state<T>(p: &ListPicker<T>) -> &State<T> {
        p.state.as_ref().expect("expected open state")
    }

    fn ready_state_mut<T>(p: &mut ListPicker<T>) -> &mut State<T> {
        p.state.as_mut().expect("expected open state")
    }

    fn mouse(kind: MouseEventKind, area: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn render<T: PickerItem>(picker: &mut ListPicker<T>) {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
    }

    struct Entry {
        label: String,
        detail: Option<String>,
        spinning: bool,
    }

    impl Entry {
        fn new(label: &str) -> Self {
            Self {
                label: label.into(),
                detail: None,
                spinning: false,
            }
        }
    }

    impl PickerItem for Entry {
        fn label(&self) -> &str {
            &self.label
        }
        fn detail(&self) -> Option<&str> {
            self.detail.as_deref()
        }
        fn is_spinning(&self) -> bool {
            self.spinning
        }
    }

    struct SearchEntry {
        label: &'static str,
        search_text: &'static str,
    }

    impl PickerItem for SearchEntry {
        fn label(&self) -> &str {
            self.label
        }

        fn search_text(&self) -> &str {
            self.search_text
        }
    }

    /// The running-task spinner is drawn here and nowhere else, so this is the
    /// only place that can tell the loop to keep painting it.
    #[test]
    fn a_spinning_item_animates_the_picker() {
        let mut p = ListPicker::new();
        p.open(entries(&["idle task"]), " Test ");
        assert_eq!(p.cadence(), Cadence::IDLE);

        let mut running = Entry::new("running task");
        running.spinning = true;
        p.replace_items(vec![running]);
        assert_eq!(p.cadence(), Cadence::SPINNER);

        p.close();
        assert_eq!(p.cadence(), Cadence::IDLE, "a closed picker draws nothing");
    }

    fn entries(names: &[&str]) -> Vec<Entry> {
        names.iter().map(|n| Entry::new(n)).collect()
    }

    #[test]
    fn select_item_by_uses_visible_row_and_preserves_filter() {
        let mut p = ListPicker::new();
        p.open(entries(&["Alpha", "Beta", "Alpine"]), " Test ");
        p.handle_key(key(KeyCode::Char('a')));
        p.handle_key(key(KeyCode::Char('l')));

        assert!(p.select_item_by(|entry| entry.label == "Alpine"));
        assert_eq!(p.selected_item().unwrap().label, "Alpine");
        assert_eq!(ready_state(&p).search.value(), "al");
        assert!(!p.select_item_by(|entry| entry.label == "Beta"));
        assert_eq!(p.selected_item().unwrap().label, "Alpine");
    }

    #[test]
    fn navigation_wraps_around() {
        let mut p = ListPicker::new();
        p.open(entries(&["A", "B", "C"]), " Test ");

        p.handle_key(key(KeyCode::Up));
        assert_eq!(ready_state(&p).selected, 2);

        p.handle_key(key(KeyCode::Down));
        assert_eq!(ready_state(&p).selected, 0);
    }

    #[test]
    fn page_down_advances_and_clamps() {
        let items: Vec<Entry> = (0..50).map(|i| Entry::new(&format!("Item {i}"))).collect();
        let mut p = ListPicker::new();
        p.open(items, " Test ");
        ready_state_mut(&mut p).viewport_height = 10;

        p.handle_key(key(KeyCode::PageDown));
        assert_eq!(ready_state(&p).selected, 10);

        for _ in 0..10 {
            p.handle_key(key(KeyCode::PageDown));
        }
        assert_eq!(ready_state(&p).selected, 49);
    }

    #[test]
    fn page_up_retreats_and_clamps() {
        let items: Vec<Entry> = (0..50).map(|i| Entry::new(&format!("Item {i}"))).collect();
        let mut p = ListPicker::new();
        p.open(items, " Test ");
        let s = ready_state_mut(&mut p);
        s.viewport_height = 10;
        s.selected = 25;

        p.handle_key(key(KeyCode::PageUp));
        assert_eq!(ready_state(&p).selected, 15);

        for _ in 0..5 {
            p.handle_key(key(KeyCode::PageUp));
        }
        assert_eq!(ready_state(&p).selected, 0);
    }

    /// The list owns the navigation keys, so the query they used to edit has
    /// to come back untouched.
    #[test_case(KeyCode::Home, 0  ; "home_selects_first")]
    #[test_case(KeyCode::End,  49 ; "end_selects_last")]
    fn home_and_end_reach_the_ends_of_the_list(code: KeyCode, expected: usize) {
        let items: Vec<Entry> = (0..50).map(|i| Entry::new(&format!("Item {i}"))).collect();
        let mut p = ListPicker::new();
        p.open(items, " Test ");
        let s = ready_state_mut(&mut p);
        s.viewport_height = 10;
        s.selected = 25;
        p.handle_key(key(KeyCode::Char('I')));

        p.handle_key(key(code));

        let s = ready_state(&p);
        assert_eq!(s.selected, expected);
        assert_eq!(s.search.value(), "I", "{QUERY_UNTOUCHED}");
    }

    #[test]
    fn home_and_end_on_an_empty_list_do_nothing() {
        let mut p = ListPicker::new();
        p.open(entries(&["A"]), " Test ");
        p.handle_key(key(KeyCode::Char('z')));
        assert!(ready_state(&p).filtered.is_empty());

        p.handle_key(key(KeyCode::Home));
        p.handle_key(key(KeyCode::End));
        assert_eq!(ready_state(&p).selected, 0);
    }

    #[test]
    fn ctrl_u_pages_like_the_page_keys() {
        let items: Vec<Entry> = (0..50).map(|i| Entry::new(&format!("Item {i}"))).collect();
        let mut p = ListPicker::new();
        p.open(items, " Test ");
        ready_state_mut(&mut p).viewport_height = 10;

        p.handle_key(key::PAGE_DOWN.to_key_event());
        assert_eq!(ready_state(&p).selected, 10);

        p.handle_key(key::SCROLL_HALF_UP.to_key_event());
        assert_eq!(ready_state(&p).selected, 0);
    }

    #[test]
    fn search_filters_progressively() {
        let mut p = ListPicker::new();
        p.open(entries(&["Alpha", "Beta"]), " Test ");
        assert_eq!(ready_state(&p).filtered, vec![0, 1]);

        p.handle_key(key(KeyCode::Char('a')));
        assert_eq!(ready_state(&p).filtered, vec![0, 1]);

        p.handle_key(key(KeyCode::Char('l')));
        assert_eq!(ready_state(&p).filtered, vec![0]);
    }

    #[test]
    fn search_uses_item_haystack_without_changing_its_label() {
        let mut p = ListPicker::new();
        p.open(
            vec![
                SearchEntry {
                    label: "shared-id",
                    search_text: "shared-id anthropic/shared-id Anthropic Best",
                },
                SearchEntry {
                    label: "shared-id",
                    search_text: "shared-id zai/shared-id Z.AI Fast",
                },
            ],
            " Test ",
        );

        p.handle_paste("zai/shared-id");

        assert_eq!(ready_state(&p).filtered, vec![1]);
        assert_eq!(p.selected_item().unwrap().label(), "shared-id");
    }

    /// Search lines used to hand-roll a keymap that dropped every control
    /// chord, so the editing keys advertised elsewhere did nothing here.
    #[test]
    fn ctrl_w_deletes_the_search_word_and_refilters() {
        let mut p = ListPicker::new();
        p.open(entries(&["Alpha", "Beta"]), " Test ");
        for c in "alx".chars() {
            p.handle_key(key(KeyCode::Char(c)));
        }
        assert!(ready_state(&p).filtered.is_empty());

        p.handle_key(kb::DELETE_WORD.to_key_event());
        assert_eq!(ready_state(&p).search.value(), "");
        assert_eq!(ready_state(&p).filtered, vec![0, 1]);
    }

    #[test]
    fn ctrl_left_moves_the_search_caret_by_word() {
        let mut p = ListPicker::new();
        p.open(entries(&["Alpha"]), " Test ");
        for c in "alpha beta".chars() {
            p.handle_key(key(KeyCode::Char(c)));
        }
        p.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(ready_state(&p).search.x(), "alpha ".len());
    }

    #[test]
    fn fuzzy_search_with_nucleo_matcher() {
        let mut p = ListPicker::new();
        p.open(
            entries(&["claude-sonnet", "claude-opus", "gemini-pro", "gpt-4"]),
            " Test ",
        );

        // Test fuzzy matching - should find "claude-sonnet" with "clu"
        p.handle_key(key(KeyCode::Char('c')));
        p.handle_key(key(KeyCode::Char('l')));
        p.handle_key(key(KeyCode::Char('u')));
        let filtered = ready_state(&p).filtered.clone();
        assert!(filtered.contains(&0)); // claude-sonnet should match
        assert!(filtered.contains(&1)); // claude-opus should match

        // Test that non-matching items are filtered out
        p.close();
        p.open(entries(&["claude-sonnet", "gemini-pro", "gpt-4"]), " Test ");
        p.handle_key(key(KeyCode::Char('c')));
        p.handle_key(key(KeyCode::Char('l')));
        p.handle_key(key(KeyCode::Char('u')));
        let filtered = ready_state(&p).filtered.clone();
        assert_eq!(filtered, vec![0]); // only claude-sonnet should match
    }

    #[test]
    fn enter_returns_selected_item() {
        let mut p = ListPicker::new();
        p.open(entries(&["A", "B", "C"]), " Test ");
        p.handle_key(key(KeyCode::Down));

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, PickerAction::Select(ref e) if e.label == "B"));
        assert!(!p.is_open());
    }

    #[test]
    fn clicking_item_returns_it() {
        let mut picker = ListPicker::new();
        picker.open(entries(&["A", "B", "C"]), " Test ");
        render(&mut picker);
        let hit = ready_state(&picker).row_hits[1];

        assert!(matches!(
            picker.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hit.area)),
            PickerAction::Consumed
        ));
        let action = picker.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), hit.area));

        assert!(matches!(action, PickerAction::Select(ref entry) if entry.label == "B"));
        assert!(!picker.is_open());
    }

    #[test]
    fn hovering_item_moves_visible_selection() {
        let mut picker = ListPicker::new();
        picker.open(entries(&["A", "B", "C"]), " Test ");
        render(&mut picker);
        let hit = ready_state(&picker).row_hits[2];

        picker.handle_mouse(mouse(MouseEventKind::Moved, hit.area));

        assert_eq!(ready_state(&picker).selected, 2);
    }

    #[test]
    fn mouse_release_on_another_item_does_not_select() {
        let mut picker = ListPicker::new();
        picker.open(entries(&["A", "B"]), " Test ");
        render(&mut picker);
        let first = ready_state(&picker).row_hits[0];
        let second = ready_state(&picker).row_hits[1];

        picker.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), first.area));
        let action = picker.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), second.area));

        assert!(matches!(action, PickerAction::Consumed));
        assert!(picker.is_open());
    }

    /// A highlighted row is painted over the bar's column, so the bar has to
    /// be offered the press before the row beneath it takes it.
    #[test]
    fn pressing_the_scrollbar_scrolls_instead_of_selecting() {
        let labels: Vec<String> = (0..OVERFLOWING_ITEMS)
            .map(|i| format!("item {i}"))
            .collect();
        let mut picker = ListPicker::new();
        picker.open(labels, " Test ");
        render(&mut picker);
        let last = *ready_state(&picker).row_hits.last().unwrap();
        let bar = Rect::new(last.area.right() - 1, last.area.y, 1, 1);

        picker.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), bar));

        let state = ready_state(&picker);
        assert_eq!(
            state.scroll_offset,
            OVERFLOWING_ITEMS - state.viewport_height,
            "{BAR_LOST_THE_PRESS}"
        );
        assert!(state.mouse_down.is_none(), "{BAR_LOST_THE_PRESS}");
    }

    #[test]
    fn filtering_invalidates_rendered_mouse_rows() {
        let mut picker = ListPicker::new();
        picker.open(entries(&["Alpha", "Beta"]), " Test ");
        render(&mut picker);
        let stale = ready_state(&picker).row_hits[0];

        picker.handle_key(key(KeyCode::Char('z')));
        picker.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), stale.area));
        let action = picker.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), stale.area));

        assert!(matches!(action, PickerAction::Consumed));
        assert!(picker.is_open());
    }

    #[test]
    fn scrolling_invalidates_rendered_mouse_rows() {
        let items: Vec<Entry> = (0..30).map(|i| Entry::new(&format!("Item {i}"))).collect();
        let mut picker = ListPicker::new();
        picker.open(items, " Test ");
        render(&mut picker);
        let stale = ready_state(&picker).row_hits[0];

        picker.scroll(-3);
        picker.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), stale.area));
        let action = picker.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), stale.area));

        assert!(matches!(action, PickerAction::Consumed));
        assert!(picker.is_open());
    }

    #[test]
    fn wheel_scroll_survives_repaint_after_hovering_item() {
        let items: Vec<Entry> = (0..30).map(|i| Entry::new(&format!("Item {i}"))).collect();
        let mut picker = ListPicker::new();
        picker.open(items, " Test ");
        render(&mut picker);

        let hovered = ready_state(&picker).row_hits[5];
        picker.handle_mouse(mouse(MouseEventKind::Moved, hovered.area));
        picker.scroll(-100);
        let bottom_offset = ready_state(&picker).scroll_offset;
        render(&mut picker);

        let state = ready_state(&picker);
        assert_eq!(state.scroll_offset, bottom_offset);
        assert_eq!(state.selected, hovered.filtered_index);
        assert_eq!(state.row_hits.last().map(|hit| hit.item_index), Some(29));

        let hovered = state.row_hits[state.row_hits.len() / 2];
        picker.handle_mouse(mouse(MouseEventKind::Moved, hovered.area));
        picker.scroll(100);
        render(&mut picker);

        let state = ready_state(&picker);
        assert_eq!(state.scroll_offset, 0);
        assert_eq!(state.selected, hovered.filtered_index);
        assert_eq!(state.row_hits.first().map(|hit| hit.item_index), Some(0));
    }

    #[test_case(key(KeyCode::Esc) ; "esc_returns_close")]
    #[test_case(kb::QUIT.to_key_event() ; "ctrl_c_returns_close")]
    fn cancel_returns_close(cancel_key: KeyEvent) {
        let mut p = ListPicker::new();
        p.open(entries(&["A", "B"]), " Test ");

        let action = p.handle_key(cancel_key);
        assert!(matches!(action, PickerAction::Close));
        assert!(!p.is_open());
    }

    #[test]
    fn enter_on_empty_results_consumed() {
        let mut p = ListPicker::new();
        p.open(entries(&["Alpha"]), " Test ");
        p.handle_key(key(KeyCode::Char('z')));

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, PickerAction::Consumed));
    }

    #[test_case(0, -3, 3  ; "scroll_down")]
    #[test_case(0, 100, 0  ; "clamp_at_top")]
    #[test_case(5, 3, 2    ; "scroll_up")]
    #[test_case(0, -100, 20 ; "clamp_at_bottom")]
    fn scroll_bounds(initial: usize, delta: i32, expected: usize) {
        let items: Vec<Entry> = (0..30).map(|i| Entry::new(&format!("Item {i}"))).collect();
        let mut p = ListPicker::new();
        p.open(items, " Test ");
        let s = ready_state_mut(&mut p);
        s.viewport_height = 10;
        s.scroll_offset = initial;

        p.scroll(delta);
        assert_eq!(ready_state(&p).scroll_offset, expected);
    }

    #[test]
    fn ctrl_w_deletes_word() {
        let mut p = ListPicker::new();
        p.open(entries(&["A", "B"]), " Test ");
        p.handle_key(key(KeyCode::Char('h')));
        p.handle_key(key(KeyCode::Char('i')));
        assert_eq!(ready_state(&p).search.value(), "hi");

        p.handle_key(kb::DELETE_WORD.to_key_event());
        assert_eq!(ready_state(&p).search.value(), "");
    }

    struct SectionEntry {
        label: String,
        section: Option<&'static str>,
    }

    impl PickerItem for SectionEntry {
        fn label(&self) -> &str {
            &self.label
        }
        fn section(&self) -> Option<&str> {
            self.section
        }
    }

    fn sectioned(label: &str, section: Option<&'static str>) -> SectionEntry {
        SectionEntry {
            label: label.into(),
            section,
        }
    }

    fn labels(p: &ListPicker<SectionEntry>) -> Vec<&str> {
        let s = ready_state(p);
        s.filtered.iter().map(|&i| s.items[i].label()).collect()
    }

    fn search(p: &mut ListPicker<SectionEntry>, query: &str) {
        for c in query.chars() {
            p.handle_key(key(KeyCode::Char(c)));
        }
    }

    /// The other pickers carry meaning in their item order, so ranking has to
    /// stay opt-in. `zview` scores worse than `view` yet is supplied first.
    #[test]
    fn source_order_is_kept_without_relevance_order() {
        let mut p = ListPicker::new();
        p.open(
            vec![
                sectioned("zview", Some(SECTION_A)),
                sectioned("view", Some(SECTION_A)),
            ],
            " Test ",
        );
        search(&mut p, "view");
        assert_eq!(labels(&p), vec!["zview", "view"]);
    }

    #[test]
    fn relevance_order_puts_the_better_match_first() {
        let mut p = ListPicker::new().with_relevance_order();
        p.open(
            vec![
                sectioned("zview", Some(SECTION_A)),
                sectioned("view", Some(SECTION_A)),
            ],
            " Test ",
        );
        search(&mut p, "view");
        assert_eq!(labels(&p), vec!["view", "zview"]);
    }

    /// Headers are only legible while every member of a section is adjacent,
    /// so ranking may reorder sections but never interleave them.
    #[test]
    fn relevance_order_keeps_sections_contiguous() {
        let mut p = ListPicker::new().with_relevance_order();
        p.open(
            vec![
                sectioned("zzview", Some(SECTION_A)),
                sectioned("view", Some(SECTION_B)),
                sectioned("vieww", Some(SECTION_A)),
                sectioned("zview", Some(SECTION_B)),
            ],
            " Test ",
        );
        search(&mut p, "view");

        let s = ready_state(&p);
        let sections: Vec<Option<&str>> =
            s.filtered.iter().map(|&i| s.items[i].section()).collect();
        let mut seen: Vec<Option<&str>> = Vec::new();
        for section in &sections {
            if seen.last() != Some(section) {
                assert!(
                    !seen.contains(section),
                    "section {section:?} was split: {sections:?}"
                );
                seen.push(*section);
            }
        }
    }

    /// Only B holds a match on the first character, so B is promoted even
    /// though A supplied the earlier items. Within each section the members
    /// sort by their own score, and A's equal scores keep the supplied order.
    #[test]
    fn relevance_order_ranks_sections_by_their_best_member() {
        let mut p = ListPicker::new().with_relevance_order();
        p.open(
            vec![
                sectioned("zview", Some(SECTION_A)),
                sectioned("xxxxview", Some(SECTION_A)),
                sectioned("preview", Some(SECTION_B)),
                sectioned("view", Some(SECTION_B)),
            ],
            " Test ",
        );
        search(&mut p, "view");
        assert_eq!(labels(&p), vec!["view", "preview", "zview", "xxxxview"]);
    }

    /// The model picker lists a model twice, once under `Recent`. Identical
    /// labels score identically, so only the supplied order can separate them.
    #[test]
    fn relevance_order_breaks_score_ties_by_original_index() {
        let mut p = ListPicker::new().with_relevance_order();
        p.open(
            vec![
                sectioned("view", Some(SECTION_A)),
                sectioned("view", Some(SECTION_A)),
                sectioned("view", Some(SECTION_A)),
            ],
            " Test ",
        );
        search(&mut p, "view");
        assert_eq!(ready_state(&p).filtered, vec![0, 1, 2]);
    }

    /// `section_gap` treats a section-less row as ending the run, so the next
    /// sectioned row re-emits its header. `render_list` has to agree or every
    /// consumer of `visual_rows_in_range` scrolls against the wrong height.
    #[test]
    fn a_section_repeats_its_header_after_a_sectionless_row() {
        let items = vec![
            sectioned("a1", Some(SECTION_A)),
            sectioned("loose", None),
            sectioned("a2", Some(SECTION_A)),
        ];
        let filtered: Vec<usize> = (0..items.len()).collect();
        assert_eq!(
            visual_rows_in_range(&filtered, &items, 0, items.len()),
            6,
            "3 items + first header + blank + repeated header"
        );

        let mut p = ListPicker::new();
        p.open(items, " Test ");
        let backend = ratatui::backend::TestBackend::new(60, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                p.view(f, f.area());
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert_eq!(
            text.matches(SECTION_A).count(),
            2,
            "header should be redrawn after the section-less row"
        );
    }

    fn section_entries() -> Vec<SectionEntry> {
        vec![
            sectioned("a1", Some(SECTION_A)),
            sectioned("a2", Some(SECTION_A)),
            sectioned("b1", Some(SECTION_B)),
        ]
    }

    #[test]
    fn section_headers_counted_in_visual_rows() {
        let items = section_entries();
        let filtered: Vec<usize> = (0..items.len()).collect();
        let rows = visual_rows_in_range(&filtered, &items, 0, items.len());
        assert_eq!(rows, 6);
    }

    #[test]
    fn section_navigation_accounts_for_headers() {
        let mut p = ListPicker::new();
        p.open(section_entries(), " Test ");
        let s = ready_state_mut(&mut p);
        s.viewport_height = 3;

        s.selected = 2;
        s.ensure_visible();
        assert_eq!(s.scroll_offset, 2);
    }

    #[test]
    fn section_headers_are_not_clickable() {
        let mut picker = ListPicker::new();
        picker.open(section_entries(), " Test ");
        render(&mut picker);
        let first_item = ready_state(&picker).row_hits[0].area;
        let header = Rect::new(first_item.x, first_item.y - 1, first_item.width, 1);

        picker.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), header));
        let action = picker.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), header));

        assert!(matches!(action, PickerAction::Consumed));
        assert!(picker.is_open());
    }

    #[test]
    fn filtering_clamps_scroll_offset_to_the_selection() {
        let mut p = ListPicker::new();
        let items: Vec<Entry> = (0..20).map(|i| Entry::new(&format!("Item {i}"))).collect();
        p.open(items, " Test ");
        let s = ready_state_mut(&mut p);
        s.viewport_height = 10;
        s.scroll_offset = 10;
        s.selected = 15;

        s.search.insert_text("0");
        s.update_search_and_clamp();
        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn toggle_mode_enter_flips_enabled() {
        let mut p = ListPicker::new();
        p.open_selectively_toggleable(
            entries(&["A", "B"]),
            vec![true, true],
            vec![true, true],
            " Test ",
        );
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, PickerAction::Toggle(0, false)));
        assert!(p.is_open());
    }

    #[test]
    fn toggle_mode_search_targets_correct_item() {
        let mut p = ListPicker::new();
        p.open_selectively_toggleable(
            entries(&["Alpha", "Beta"]),
            vec![true, true],
            vec![true, true],
            " Test ",
        );
        p.handle_key(key(KeyCode::Char('b')));
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, PickerAction::Toggle(1, false)));
    }

    #[test_case("short", 10 => "short" ; "no_truncation_needed")]
    #[test_case("abcdefghijklmno", 10 => "abcdefghi\u{2026}" ; "long_ascii_truncated")]
    #[test_case("ab\u{4e16}\u{754c}cde", 6 => "ab\u{4e16}\u{2026}" ; "wide_chars_truncated")]
    #[test_case("long", 0 => "" ; "zero_width_is_empty")]
    fn truncate_label_cases(label: &str, max_width: usize) -> String {
        truncate_label(label, max_width)
    }

    #[test]
    fn detail_right_edge_consistent_for_long_and_short_labels() {
        let width: u16 = 40;
        let detail = "2h ago";
        let suffix_gap = 2usize;

        let end_col = |label: &str, suffix_w: usize| -> usize {
            let trailing = suffix_w + if suffix_w > 0 { suffix_gap } else { 0 };
            let max_label = width
                .saturating_sub(detail.width() as u16 + trailing as u16 + 1 + DETAIL_RIGHT_PAD)
                as usize;
            let t = truncate_label(label, max_label);
            let pad = (width as usize).saturating_sub(
                t.width() + trailing + detail.width() + DETAIL_RIGHT_PAD as usize + 1,
            );
            t.width() + trailing + pad + detail.width() + DETAIL_RIGHT_PAD as usize
        };

        let long = "  ".to_string() + "x".repeat(60).as_str();
        assert_eq!(end_col(&long, 0), end_col("  hi", 0));
        assert!(end_col(&long, 0) <= width as usize);

        let sfx = "Anthropic".width();
        assert_eq!(end_col(&long, sfx), end_col("  hi", sfx));
        assert!(end_col(&long, sfx) <= width as usize);
    }
}
