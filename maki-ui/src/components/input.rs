use std::time::{SystemTime, UNIX_EPOCH};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::shell::parse_shell_prefix;
use crate::components::keybindings::key;
use crate::highlight;
use crate::input_document::{InputDocument, InputDraft, PasteId, should_summarize_paste};
use crate::text_buffer::{EditResult, TextBuffer, is_newline_key};
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent};
use maki_agent::PromptAdmission;
use maki_storage::input_history::InputHistory;
use std::mem;

use maki_providers::ImageSource;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph};

use super::scrollbar::render_vertical_scrollbar;
use super::{apply_scroll_delta, visual_line_count};
use crate::selection::LineBreaks;

const CHEVRON: &str = super::CHEVRON;
const NEWLINE_PAD: &str = "  ";
const PREFIX_WIDTH: u16 = 2;
const COMPOSER_RAIL_WIDTH: u16 = 1;
const COMPOSER_VERTICAL_PADDING: u16 = 1;
const ADMISSION_SEPARATOR: &str = "  ";
const ADMISSION_DESCRIPTION_GAP: &str = " ";
const PLACEHOLDER_SUGGESTIONS: &[&str] = &[
    "research how something works",
    "fix a bug",
    "add a feature",
    "add a database migration",
    "create a helm chart",
    "simplify some function",
    "remove trivial comments",
    "analyze data",
    "profile and improve performance",
    "add tests",
    "add benchmarks",
    "refactor a module",
    "remove dead code",
];
const QUEUE_PLACEHOLDER: &str = "Queue another prompt...";
const QUEUE_EDIT_PLACEHOLDER: &str = "Edit queued message...";
const STEER_PLACEHOLDER: &str = "Steer this task...";
const ASK_PREFIX: &str = "Ask maki to ";
const ASK_SUFFIX: &str = "...";
const BLANK_PLACEHOLDER: &str = " ";
const ADMISSION_OPTIONS: [(&str, &str, PromptAdmission); 3] = [
    ("Enter", "next", PromptAdmission::Queue),
    (key::STEER_PROMPT.label, "guide", PromptAdmission::Steer),
    (
        key::INTERRUPT_PROMPT.label,
        "replace",
        PromptAdmission::Interrupt,
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdmissionHit {
    pub area: Rect,
    pub admission: PromptAdmission,
}

#[derive(Clone, Copy)]
pub enum Placeholder {
    Suggestion,
    Blank,
    Queue,
    QueueEdit,
    Steer,
}

pub enum InputAction {
    Submit(Submission),
    EditPaste(PasteId),
    ContinueLine,
    PaletteSync(String),
    Passthrough(KeyEvent),
    None,
}

pub struct Submission {
    pub text: String,
    pub images: Vec<ImageSource>,
    pub(crate) draft: InputDraft,
}

pub(crate) struct InputState {
    draft: InputDraft,
    images: Vec<ImageSource>,
}

impl Submission {
    pub fn empty() -> Self {
        Self {
            text: String::new(),
            images: Vec::new(),
            draft: InputDraft::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.images.is_empty()
    }

    pub(crate) fn from_text(text: String) -> Self {
        Self {
            draft: InputDraft {
                text: text.clone(),
                paste_ranges: Vec::new(),
            },
            text,
            images: Vec::new(),
        }
    }
}

pub struct InputBox {
    pub(crate) buffer: InputDocument,
    history: InputHistory,
    history_index: Option<usize>,
    draft: InputDraft,
    scroll_y: u16,
    follow_cursor: bool,
    placeholder_hint: &'static str,
    pending_images: Vec<ImageSource>,
    max_input_lines: u16,
    last_total_lines: u16,
    last_content_height: u16,
    hovered_paste: Option<PasteId>,
}

impl InputBox {
    pub fn handle_key(&mut self, key: KeyEvent) -> InputAction {
        self.handle_key_inner(key, true)
    }

    pub(crate) fn handle_editor_key(&mut self, key: KeyEvent) -> InputAction {
        self.handle_key_inner(key, false)
    }

    fn handle_key_inner(&mut self, key: KeyEvent, record_history: bool) -> InputAction {
        self.follow_cursor = true;

        match key.code {
            KeyCode::Up if self.is_at_first_line() => {
                self.history_up();
                return InputAction::None;
            }
            KeyCode::Down if self.is_at_last_line() => {
                self.history_down();
                return InputAction::None;
            }
            KeyCode::Tab | KeyCode::Esc => return InputAction::Passthrough(key),
            _ if is_newline_key(&key) => {
                self.buffer.add_line();
                return InputAction::ContinueLine;
            }
            KeyCode::Enter if let Some(id) = self.buffer.focused_paste() => {
                return InputAction::EditPaste(id);
            }
            KeyCode::Enter if self.char_before_cursor_is_backslash() => {
                self.continue_line();
                return InputAction::ContinueLine;
            }
            KeyCode::Enter => {
                return match self.submit_inner(record_history) {
                    Some(sub) => InputAction::Submit(sub),
                    None => InputAction::Submit(Submission::empty()),
                };
            }
            _ => {}
        }

        match self.buffer.handle_key(key) {
            EditResult::Changed => InputAction::PaletteSync(self.buffer.palette_text()),
            EditResult::Moved | EditResult::Ignored => InputAction::None,
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> InputAction {
        self.follow_cursor = true;
        if should_summarize_paste(text) {
            self.buffer.insert_paste(text);
        } else {
            self.buffer.insert_text(text);
        }
        InputAction::PaletteSync(self.buffer.palette_text())
    }

    fn handle_plain_paste(&mut self, text: &str) -> InputAction {
        self.follow_cursor = true;
        self.buffer.insert_text(text);
        InputAction::PaletteSync(self.buffer.palette_text())
    }

    /// Inserting a file path mid-word looks broken ("read/tmp/x" instead of
    /// "read /tmp/x"). This adds spaces around the paste only when needed.
    pub fn handle_paste_with_spaces(&mut self, text: &str) -> InputAction {
        let line = &self.buffer.lines()[self.buffer.y()];
        let bx = TextBuffer::char_to_byte(line, self.buffer.x());

        let char_before = line[..bx].chars().next_back();
        let char_after = line[bx..].chars().next();

        let is_word_boundary =
            |c: char| -> bool { c.is_alphanumeric() || c == '_' || ")]}>".contains(c) };

        let needs_leading = char_before.is_some_and(&is_word_boundary) && !text.starts_with(' ');
        let needs_trailing = char_after.is_some_and(&is_word_boundary) && !text.ends_with(' ');

        if !needs_leading && !needs_trailing {
            return self.handle_plain_paste(text);
        }

        let mut spaced = String::with_capacity(
            text.len() + usize::from(needs_leading) + usize::from(needs_trailing),
        );

        if needs_leading {
            spaced.push(' ');
        }
        spaced.push_str(text);
        if needs_trailing {
            spaced.push(' ');
        }

        self.handle_plain_paste(&spaced)
    }

    pub fn new(history: InputHistory, max_input_lines: u32) -> Self {
        let max_input_lines = max_input_lines.clamp(1, u16::MAX as u32 - 2) as u16;
        Self {
            buffer: InputDocument::new(),
            history,
            history_index: None,
            draft: InputDraft::default(),
            scroll_y: 0,
            follow_cursor: true,
            placeholder_hint: random_placeholder_hint(),
            pending_images: Vec::new(),
            max_input_lines,
            last_total_lines: 1,
            last_content_height: 1,
            hovered_paste: None,
        }
    }

    pub fn copy_text(&self) -> String {
        self.buffer
            .lines()
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let prefix = if i == 0 { CHEVRON } else { NEWLINE_PAD };
                format!("{prefix}{l}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn line_breaks(&self, content_width: u16) -> LineBreaks {
        let ew = effective_width(content_width as usize);
        LineBreaks::from_heights(
            self.buffer
                .lines()
                .iter()
                .map(|line| visual_line_count(line.width(), ew) as u16),
        )
    }

    pub fn height(&self, width: u16) -> u16 {
        let ew = effective_width(composer_content_width(width) as usize);
        let mut visual_lines =
            total_visual_lines(&self.buffer, ew, self.buffer.focused_paste().is_none());
        if !self.pending_images.is_empty() {
            visual_lines += 1;
        }
        let capped = visual_lines.min(self.max_input_lines as usize);
        (capped + 2) as u16
    }

    pub fn is_at_first_line(&self) -> bool {
        self.buffer.y() == 0
    }

    pub fn is_at_last_line(&self) -> bool {
        self.buffer.y() == self.buffer.line_count().saturating_sub(1)
    }

    pub fn char_before_cursor_is_backslash(&self) -> bool {
        let line = &self.buffer.lines()[self.buffer.y()];
        let x = self.buffer.x();
        if x == 0 {
            return false;
        }
        let byte_idx = TextBuffer::char_to_byte(line, x - 1);
        line.as_bytes()[byte_idx] == b'\\'
    }

    pub fn continue_line(&mut self) {
        self.buffer.remove_char();
        self.buffer.add_line();
    }

    #[cfg(test)]
    pub fn submit(&mut self) -> Option<Submission> {
        self.submit_inner(true)
    }

    pub(crate) fn take_submission(&mut self) -> Option<Submission> {
        self.submit_inner(true)
    }

    fn submit_inner(&mut self, record_history: bool) -> Option<Submission> {
        let draft = self.buffer.draft();
        let text = draft.text.trim().to_string();
        let images = mem::take(&mut self.pending_images);
        if text.is_empty() && images.is_empty() {
            return None;
        }
        if record_history {
            self.history.push(text.clone());
        }
        self.discard();
        Some(Submission {
            text,
            images,
            draft,
        })
    }

    pub fn discard(&mut self) {
        self.pending_images.clear();
        self.history_index = None;
        self.draft = InputDraft::default();
        self.buffer.clear();
        self.scroll_y = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.value().trim().is_empty() && self.pending_images.is_empty()
    }

    pub fn attach_image(&mut self, source: ImageSource) {
        self.pending_images.push(source);
    }

    pub(crate) fn pending_images(&self) -> &[ImageSource] {
        &self.pending_images
    }

    pub fn set_input(&mut self, s: String) {
        self.buffer = InputDocument::from_plain(s);
    }

    pub(crate) fn draft(&self) -> InputDraft {
        self.buffer.draft()
    }

    pub(crate) fn set_draft(&mut self, draft: InputDraft) {
        self.buffer = InputDocument::from_draft(draft);
    }

    pub(crate) fn take_state(&mut self) -> InputState {
        let state = InputState {
            draft: self.draft(),
            images: mem::take(&mut self.pending_images),
        };
        self.discard();
        state
    }

    pub(crate) fn set_state(&mut self, state: InputState) {
        self.set_draft(state.draft);
        self.pending_images = state.images;
        self.move_to_end();
    }

    pub(crate) fn expanded_text(&self) -> String {
        self.buffer.expanded_text()
    }

    pub(crate) fn palette_text(&self) -> String {
        self.buffer.palette_text()
    }

    pub(crate) fn has_pastes(&self) -> bool {
        self.buffer.has_pastes()
    }

    pub(crate) fn paste_text(&self, id: PasteId) -> Option<&str> {
        self.buffer.paste_text(id)
    }

    pub(crate) fn update_paste(&mut self, id: PasteId, text: &str) -> bool {
        self.buffer.update_paste(id, text)
    }

    pub(crate) fn move_to_end(&mut self) {
        self.buffer.move_to_end();
    }

    pub(crate) fn expand_pastes(&mut self) {
        self.buffer.expand_pastes();
    }

    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let new_index = match self.history_index {
            None => {
                self.draft = self.buffer.draft();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.history_index = Some(new_index);
        let entry = self.history.get(new_index).unwrap().to_string();
        self.set_input(entry);
        self.buffer.move_to_end();
    }

    pub fn history_down(&mut self) {
        let Some(i) = self.history_index else {
            return;
        };
        if i + 1 < self.history.len() {
            self.history_index = Some(i + 1);
            let entry = self.history.get(i + 1).unwrap().to_string();
            self.set_input(entry);
        } else {
            self.history_index = None;
            let draft = mem::take(&mut self.draft);
            self.set_draft(draft);
        }
    }

    fn visual_cursor_y(&self, ew: usize) -> u16 {
        let lines_above: u16 = self
            .buffer
            .lines()
            .iter()
            .take(self.buffer.y())
            .map(|line| visual_line_count(line.width(), ew) as u16)
            .sum();

        let wrap_row = {
            let line = &self.buffer.lines()[self.buffer.y()];
            let cursor_col: usize = line
                .chars()
                .take(self.buffer.x())
                .map(|c| c.width().unwrap_or(1))
                .sum();
            cursor_col.checked_div(ew).unwrap_or(0) as u16
        };

        lines_above + wrap_row
    }

    pub fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        placeholder: Placeholder,
        border_style: Style,
        focused: bool,
        top_right_hint: Option<Line<'_>>,
    ) {
        let content_area = content_area(area);
        let content_height = content_area.height;
        let ew = effective_width(content_area.width as usize);
        let focused_paste = focused.then(|| self.buffer.focused_paste()).flatten();
        let cursor_visible = focused && focused_paste.is_none();

        if self.follow_cursor {
            let visual_cursor_y = self.visual_cursor_y(ew);
            if visual_cursor_y < self.scroll_y {
                self.scroll_y = visual_cursor_y;
            } else if visual_cursor_y >= self.scroll_y + content_height {
                self.scroll_y = visual_cursor_y - content_height + 1;
            }
        }

        let mut total_vl = total_visual_lines(&self.buffer, ew, cursor_visible) as u16;
        if !self.pending_images.is_empty() {
            total_vl += 1;
        }
        self.last_total_lines = total_vl;
        self.last_content_height = content_height.max(1);
        let max_scroll = self.max_scroll();
        self.scroll_y = self.scroll_y.min(max_scroll);

        let is_empty = self.buffer.display_text().is_empty();
        let mut styled_lines: Vec<Line> = if is_empty && self.pending_images.is_empty() {
            let base = theme::current().input_placeholder;
            let (head, tail) = match placeholder {
                Placeholder::Suggestion => (
                    ASK_PREFIX,
                    vec![
                        Span::styled(self.placeholder_hint, base.add_modifier(Modifier::ITALIC)),
                        Span::styled(ASK_SUFFIX, base),
                    ],
                ),
                Placeholder::Queue => (QUEUE_PLACEHOLDER, Vec::new()),
                Placeholder::QueueEdit => (QUEUE_EDIT_PLACEHOLDER, Vec::new()),
                Placeholder::Steer => (STEER_PLACEHOLDER, Vec::new()),
                Placeholder::Blank => (BLANK_PLACEHOLDER, Vec::new()),
            };
            let mut spans = vec![super::chevron_span()];
            spans.extend(cursor_on_first_char(head, base, focused));
            spans.extend(tail);
            vec![Line::from(spans)]
        } else {
            let cursor_y = self.buffer.y();
            let cursor_x = self.buffer.x();
            self.buffer
                .lines()
                .iter()
                .enumerate()
                .flat_map(|(i, line)| {
                    let paste_ranges = self.buffer.paste_ranges_on_line(i);
                    let is_cursor_line = i == cursor_y && focused && focused_paste.is_none();
                    let shell_spans = if i == 0 && paste_ranges.is_empty() {
                        shell_highlight_spans(line)
                    } else {
                        None
                    };
                    let styled_spans = if paste_ranges.is_empty() {
                        shell_spans
                    } else {
                        Some(paste_token_spans(
                            line,
                            &paste_ranges,
                            focused_paste,
                            self.hovered_paste,
                        ))
                    };
                    wrap_line(
                        line,
                        ew,
                        is_cursor_line,
                        cursor_x,
                        i == 0,
                        styled_spans.as_deref(),
                    )
                })
                .collect()
        };

        if !self.pending_images.is_empty() {
            let n = self.pending_images.len();
            let label = match n {
                1 => "1 image".to_string(),
                _ => format!("{n} images"),
            };
            styled_lines.push(Line::from(Span::styled(
                label,
                theme::current().input_placeholder,
            )));
        }

        frame.render_widget(
            Block::default().style(theme::current().element_style()),
            area,
        );
        let rail_style = border_style.patch(theme::current().element_style());
        for y in area.y..area.bottom() {
            if let Some(cell) = frame.buffer_mut().cell_mut((area.x, y)) {
                cell.set_char('┃').set_style(rail_style);
            }
        }
        if let Some(hint) = top_right_hint {
            let hint_area = top_right_hint_area(area);
            frame.render_widget(Paragraph::new(hint.right_aligned()), hint_area);
        }
        let text = Text::from(styled_lines);
        let paragraph = Paragraph::new(text)
            .style(Style::new().fg(theme::current().foreground))
            .scroll((self.scroll_y, 0));
        frame.render_widget(paragraph, content_area);

        if max_scroll > 0 {
            render_vertical_scrollbar(frame, content_area, total_vl, self.scroll_y);
        }
    }

    fn max_scroll(&self) -> u16 {
        self.last_total_lines
            .saturating_sub(self.last_content_height)
    }

    pub fn scroll_y(&self) -> u16 {
        self.scroll_y
    }

    pub fn history(&self) -> &InputHistory {
        &self.history
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll_y = apply_scroll_delta(self.scroll_y, delta).min(self.max_scroll());
        self.follow_cursor = false;
    }

    /// Move the text cursor to the position corresponding to a mouse click at
    /// the terminal coordinates (row, col) within the input content area.
    pub fn handle_click(
        &mut self,
        area: Rect,
        row: u16,
        col: u16,
        focused: bool,
    ) -> Option<PasteId> {
        let (y, x, text_hit) = self.click_position_with_hit(area, row, col, focused)?;
        if text_hit && let Some(id) = self.buffer.paste_at(y, x) {
            self.buffer.focus_paste(id);
            self.follow_cursor = true;
            return Some(id);
        }
        self.buffer.set_cursor(y, x);
        self.follow_cursor = true;
        None
    }

    pub(crate) fn update_paste_hover(&mut self, area: Rect, row: u16, col: u16, focused: bool) {
        self.hovered_paste = self
            .click_position_with_hit(area, row, col, focused)
            .and_then(|(y, x, text_hit)| text_hit.then(|| self.buffer.paste_at(y, x)).flatten());
    }

    pub(crate) fn clear_hover(&mut self) {
        self.hovered_paste = None;
    }

    /// Convert a mouse click at terminal (row, col) within the input content
    /// area into a (line_index, char_index) in the text buffer, accounting
    /// for scroll offset, word-wrap, and the chevron/padding prefix.
    #[cfg(test)]
    fn click_position(
        &self,
        area: Rect,
        row: u16,
        col: u16,
        focused: bool,
    ) -> Option<(usize, usize)> {
        self.click_position_with_hit(area, row, col, focused)
            .map(|(y, x, _)| (y, x))
    }

    fn click_position_with_hit(
        &self,
        area: Rect,
        row: u16,
        col: u16,
        focused: bool,
    ) -> Option<(usize, usize, bool)> {
        let content_y = row.checked_sub(area.y)?;
        let content_x = col.checked_sub(area.x)?;

        let ew = effective_width(area.width as usize);
        let visual_line = content_y as usize + self.scroll_y as usize;

        let cursor_line = self.buffer.y();
        let mut visual = 0usize;

        for (buf_line_idx, line) in self.buffer.lines().iter().enumerate() {
            let chars: Vec<char> = line.chars().collect();
            let widths: Vec<usize> = chars.iter().map(|c| c.width().unwrap_or(1)).collect();

            let is_cursor_line =
                buf_line_idx == cursor_line && focused && self.buffer.focused_paste().is_none();
            let ranges = wrap_ranges(&widths, ew, is_cursor_line);

            let n_visual_rows = ranges.len();

            if visual_line < visual + n_visual_rows {
                let wrap_row = visual_line - visual;
                let (row_char_start, row_char_end) = ranges[wrap_row];

                let row_display_width: usize = widths[row_char_start..row_char_end].iter().sum();

                // The first visual row of each buffer line has a 2-cell prefix
                // (chevron or continuation padding).  Wrapped rows have none.
                let in_prefix = wrap_row == 0 && content_x < PREFIX_WIDTH;
                let text_col = if wrap_row == 0 {
                    (content_x as usize).saturating_sub(PREFIX_WIDTH as usize)
                } else {
                    content_x as usize
                };
                let text_col = text_col.min(row_display_width);

                // Walk character widths to find which char the column hits.
                let mut accum = 0;
                let mut char_idx = row_char_start;
                for &w in &widths[row_char_start..row_char_end] {
                    if accum + w > text_col {
                        break;
                    }
                    accum += w;
                    char_idx += 1;
                }

                return Some((buf_line_idx, char_idx, !in_prefix));
            }
            visual += n_visual_rows;
        }

        None
    }
}

fn cursor_on_first_char(text: &'static str, base: Style, focused: bool) -> [Span<'static>; 2] {
    let (first, rest) = text.split_at(text.chars().next().map_or(0, char::len_utf8));
    let cursor = if focused { base.reversed() } else { base };
    [Span::styled(first, cursor), Span::styled(rest, base)]
}

fn random_placeholder_hint() -> &'static str {
    let idx = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as usize % PLACEHOLDER_SUGGESTIONS.len())
        .unwrap_or(0);
    PLACEHOLDER_SUGGESTIONS[idx]
}

fn effective_width(content_width: usize) -> usize {
    content_width.saturating_sub(PREFIX_WIDTH as usize)
}

pub(crate) fn content_area(area: Rect) -> Rect {
    let padding = composer_horizontal_padding(area.width);
    let left = COMPOSER_RAIL_WIDTH.saturating_add(padding).min(area.width);
    let right = padding.min(area.width.saturating_sub(left));
    let top = COMPOSER_VERTICAL_PADDING.min(area.height);
    let bottom = COMPOSER_VERTICAL_PADDING.min(area.height.saturating_sub(top));
    Rect::new(
        area.x.saturating_add(left),
        area.y.saturating_add(top),
        area.width.saturating_sub(left.saturating_add(right)),
        area.height.saturating_sub(top.saturating_add(bottom)),
    )
}

pub(crate) fn admission_hint(
    area: Rect,
    hovered: Option<PromptAdmission>,
) -> (Line<'static>, Vec<AdmissionHit>) {
    let theme = theme::current();
    let hint_area = top_right_hint_area(area);
    let full_width = ADMISSION_OPTIONS
        .iter()
        .map(|(key, description, _)| {
            ADMISSION_SEPARATOR.width()
                + key.width()
                + ADMISSION_DESCRIPTION_GAP.width()
                + description.width()
        })
        .sum::<usize>();
    let mut x = if full_width <= hint_area.width as usize {
        hint_area.right().saturating_sub(full_width as u16)
    } else {
        hint_area.x
    };
    let mut spans = Vec::with_capacity(ADMISSION_OPTIONS.len() * 3);
    let mut hits = Vec::with_capacity(ADMISSION_OPTIONS.len());

    for (key, description, admission) in ADMISSION_OPTIONS {
        spans.push(Span::raw(ADMISSION_SEPARATOR));
        x = x.saturating_add(ADMISSION_SEPARATOR.width() as u16);
        let key_width = key.width() as u16;
        let description_width = description.width() as u16;
        let control_width = key_width
            .saturating_add(ADMISSION_DESCRIPTION_GAP.width() as u16)
            .saturating_add(description_width);
        let is_hovered = hovered == Some(admission);
        let key_style = if is_hovered {
            theme.keybind_key.add_modifier(Modifier::REVERSED)
        } else {
            theme.keybind_key
        };
        let description_style = if is_hovered {
            theme.tool_dim.add_modifier(Modifier::REVERSED)
        } else {
            theme.tool_dim
        };
        spans.push(Span::styled(key, key_style));
        spans.push(Span::styled(
            format!("{ADMISSION_DESCRIPTION_GAP}{description}"),
            description_style,
        ));
        if hint_area.height > 0
            && control_width > 0
            && x >= hint_area.x
            && x.saturating_add(control_width) <= hint_area.right()
        {
            hits.push(AdmissionHit {
                area: Rect::new(x, hint_area.y, control_width, 1),
                admission,
            });
        }
        x = x.saturating_add(control_width);
    }

    (Line::from(spans), hits)
}

fn top_right_hint_area(area: Rect) -> Rect {
    let content_area = content_area(area);
    Rect::new(
        content_area.x,
        area.y,
        content_area.width,
        COMPOSER_VERTICAL_PADDING.min(area.height),
    )
}

fn composer_content_width(width: u16) -> u16 {
    let padding = composer_horizontal_padding(width);
    width.saturating_sub(
        COMPOSER_RAIL_WIDTH
            .saturating_add(padding)
            .saturating_add(padding),
    )
}

fn composer_horizontal_padding(width: u16) -> u16 {
    if width >= 32 {
        2
    } else if width >= 16 {
        1
    } else {
        0
    }
}

fn wrap_line(
    line: &str,
    ew: usize,
    is_cursor_line: bool,
    cursor_x: usize,
    is_first_line: bool,
    shell_spans: Option<&[Span<'static>]>,
) -> Vec<Line<'static>> {
    let chars: Vec<char> = line.chars().collect();
    let widths: Vec<usize> = chars.iter().map(|c| c.width().unwrap_or(1)).collect();

    wrap_ranges(&widths, ew, is_cursor_line)
        .into_iter()
        .enumerate()
        .map(|(row, (start, end))| {
            let prefix_span = if row == 0 && is_first_line {
                super::chevron_span()
            } else if row == 0 {
                Span::raw(NEWLINE_PAD)
            } else {
                Span::raw("")
            };
            let mut spans = vec![prefix_span];

            let chunk_spans = if let Some(styled) = &shell_spans {
                slice_styled_spans(styled, start, end)
            } else {
                let chunk_text: String = chars[start..end].iter().collect();
                vec![Span::raw(chunk_text)]
            };

            if is_cursor_line && cursor_x >= start && cursor_x <= end {
                let local_cursor = cursor_x.saturating_sub(start);
                spans.extend(overlay_cursor(chunk_spans, local_cursor));
            } else {
                spans.extend(chunk_spans);
            }

            Line::from(spans)
        })
        .collect()
}

/// Split a line (given per-char display widths) into wrapped row ranges of
/// char indices, exactly as it is rendered: an extra empty row is appended
/// when the cursor sits past a completely full last row.
fn wrap_ranges(widths: &[usize], ew: usize, is_cursor_line: bool) -> Vec<(usize, usize)> {
    let row_width = ew.max(1);
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut row_start = 0;
    let mut row_col = 0;
    for (i, &w) in widths.iter().enumerate() {
        if row_col + w > row_width && row_col > 0 {
            ranges.push((row_start, i));
            row_start = i;
            row_col = 0;
        }
        row_col += w;
    }
    if row_start < widths.len() || ranges.is_empty() {
        ranges.push((row_start, widths.len()));
    }
    if is_cursor_line && row_col + 1 > row_width {
        ranges.push((widths.len(), widths.len()));
    }
    ranges
}

fn shell_highlight_spans(line: &str) -> Option<Vec<Span<'static>>> {
    if !highlight::is_ready() {
        return None;
    }
    let parsed = parse_shell_prefix(line)?;
    let prefix = &line[..parsed.prefix_len];
    let command = &line[parsed.prefix_len..];
    let shell_style = theme::current().shell_prefix;
    let mut spans = vec![Span::styled(prefix.to_owned(), shell_style)];
    let mut hl = maki_highlight::Highlighter::for_token("bash");
    for span in highlight::highlight_line(&mut hl, command) {
        spans.push(span);
    }
    Some(spans)
}

fn slice_styled_spans(
    spans: &[Span<'static>],
    char_start: usize,
    char_end: usize,
) -> Vec<Span<'static>> {
    let mut result = Vec::new();
    let mut pos = 0;
    for span in spans {
        let span_len = span.content.chars().count();
        let span_end = pos + span_len;
        if span_end <= char_start || pos >= char_end {
            pos = span_end;
            continue;
        }
        let lo = char_start.saturating_sub(pos);
        let hi = (char_end - pos).min(span_len);
        let slice: String = span.content.chars().skip(lo).take(hi - lo).collect();
        if !slice.is_empty() {
            result.push(Span::styled(slice, span.style));
        }
        pos = span_end;
    }
    result
}

fn overlay_cursor(spans: Vec<Span<'static>>, cursor_char_pos: usize) -> Vec<Span<'static>> {
    let mut result = Vec::new();
    let mut pos = 0;
    let mut cursor_placed = false;
    for span in spans {
        let span_len = span.content.chars().count();
        if !cursor_placed && cursor_char_pos >= pos && cursor_char_pos < pos + span_len {
            let local = cursor_char_pos - pos;
            let byte_pos = TextBuffer::char_to_byte(&span.content, local);
            let (before, after) = span.content.split_at(byte_pos);
            if !before.is_empty() {
                result.push(Span::styled(before.to_string(), span.style));
            }
            let mut cs = after.chars();
            let Some(cursor_char) = cs.next() else {
                break;
            };
            result.push(Span::styled(cursor_char.to_string(), span.style.reversed()));
            let rest: String = cs.collect();
            if !rest.is_empty() {
                result.push(Span::styled(rest.to_string(), span.style));
            }
            cursor_placed = true;
        } else {
            result.push(span);
        }
        pos += span_len;
    }
    if !cursor_placed {
        result.push(Span::styled(" ", Style::new().reversed()));
    }
    result
}

fn total_visual_lines(buffer: &InputDocument, ew: usize, cursor_visible: bool) -> usize {
    let cursor_y = buffer.y();
    buffer
        .lines()
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let mut text_len = line.width();
            if cursor_visible && i == cursor_y {
                text_len += 1;
            }
            visual_line_count(text_len, ew)
        })
        .sum()
}

fn paste_token_spans(
    line: &str,
    pastes: &[(std::ops::Range<usize>, PasteId)],
    focused: Option<PasteId>,
    hovered: Option<PasteId>,
) -> Vec<Span<'static>> {
    let mut spans = Vec::with_capacity(pastes.len() * 2 + 1);
    let mut cursor = 0;
    for (range, id) in pastes {
        if cursor < range.start {
            spans.push(Span::raw(
                line.chars()
                    .skip(cursor)
                    .take(range.start - cursor)
                    .collect::<String>(),
            ));
        }
        let text: String = line.chars().skip(range.start).take(range.len()).collect();
        let style = if focused == Some(*id) {
            theme::current().item_selected
        } else if hovered == Some(*id) {
            theme::current().active.add_modifier(Modifier::REVERSED)
        } else {
            theme::current().active
        };
        spans.push(Span::styled(text, style));
        cursor = range.end;
    }
    if cursor < line.chars().count() {
        spans.push(Span::raw(line.chars().skip(cursor).collect::<String>()));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::scrollbar::SCROLLBAR_THUMB;
    use crossterm::event::KeyModifiers;
    use ratatui::layout::Rect;
    use test_case::test_case;

    fn type_text(input: &mut InputBox, text: &str) {
        for c in text.chars() {
            input.buffer.push_char(c);
        }
    }

    fn submit_text(input: &mut InputBox, text: &str) {
        type_text(input, text);
        input.submit();
    }

    #[test]
    fn submit() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        assert!(input.submit().is_none());

        type_text(&mut input, " ");
        assert!(input.submit().is_none());

        type_text(&mut input, " x ");
        let sub = input.submit().unwrap();
        assert_eq!(sub.text, "x");
        assert!(sub.images.is_empty());
        assert_eq!(input.buffer.value(), "");

        type_text(&mut input, "line1");
        input.buffer.add_line();
        type_text(&mut input, "line2");
        assert_eq!(input.submit().unwrap().text, "line1\nline2");
    }

    #[test]
    fn summarized_paste_opens_editor_and_submits_expanded_text() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.handle_paste("a\nb\nc");

        assert_eq!(input.buffer.display_text(), "[Pasted 3 lines] ");
        assert_eq!(input.buffer.line_count(), 1);
        input.buffer.move_left();
        input.buffer.move_left();
        assert!(matches!(
            input.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::EditPaste(_)
        ));

        let submission = input.submit().unwrap();
        assert_eq!(submission.text, "a\nb\nc");
        assert_eq!(submission.draft.paste_ranges.len(), 1);
        assert_eq!(submission.draft.paste_ranges.first(), Some(&(0..5)));
    }

    #[test]
    fn history_restores_summarized_draft() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        submit_text(&mut input, "history");
        input.handle_paste("a\nb\nc");

        input.history_up();
        assert_eq!(input.buffer.display_text(), "history");
        input.history_down();
        assert_eq!(input.buffer.display_text(), "[Pasted 3 lines] ");
        assert_eq!(input.buffer.expanded_text(), "a\nb\nc ");
    }

    #[test]
    fn clicking_summarized_paste_returns_its_id() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.handle_paste("a\nb\nc");
        let id = input
            .handle_click(Rect::new(0, 0, 80, 1), 0, 2, true)
            .expect("paste token should be clickable");
        assert_eq!(input.buffer.focused_paste(), Some(id));
    }

    #[test]
    fn clicking_input_prefix_does_not_open_paste() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.handle_paste("a\nb\nc");
        assert_eq!(input.handle_click(Rect::new(0, 0, 80, 1), 0, 0, true), None);
    }

    #[test]
    fn backslash_continuation() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "hello\\");
        assert!(input.char_before_cursor_is_backslash());
        input.continue_line();
        assert_eq!(input.buffer.lines(), &["hello", ""]);

        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "asd\\asd");
        for _ in 0..3 {
            input.buffer.move_left();
        }
        assert!(input.char_before_cursor_is_backslash());
        input.continue_line();
        assert_eq!(input.buffer.lines(), &["asd", "asd"]);
    }

    const TEST_WIDTH: u16 = 80;

    #[test]
    fn height_capped_at_max() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        let base = input.height(TEST_WIDTH);
        for _ in 0..20 {
            input.buffer.add_line();
        }
        assert!(input.height(TEST_WIDTH) > base);
        assert!(input.height(TEST_WIDTH) <= 20 + 2);
    }

    #[test]
    fn height_respects_configured_max() {
        let mut input = InputBox::new(InputHistory::default(), 3);
        for _ in 0..10 {
            input.buffer.add_line();
        }
        assert_eq!(input.height(TEST_WIDTH), 3 + 2);
    }

    #[test]
    fn first_last_line() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        assert!(input.is_at_first_line());
        assert!(input.is_at_last_line());

        input.buffer.add_line();
        assert!(!input.is_at_first_line());
        assert!(input.is_at_last_line());

        input.buffer.move_up();
        assert!(input.is_at_first_line());
        assert!(!input.is_at_last_line());
    }

    #[test]
    fn history() {
        let mut input = InputBox::new(InputHistory::default(), 20);

        input.history_up();
        input.history_down();
        assert_eq!(input.buffer.value(), "");

        submit_text(&mut input, "a");
        submit_text(&mut input, "b");
        type_text(&mut input, "draft");

        input.history_up();
        assert_eq!(input.buffer.value(), "b");
        input.history_up();
        assert_eq!(input.buffer.value(), "a");
        input.history_up();
        assert_eq!(input.buffer.value(), "a");

        input.history_down();
        assert_eq!(input.buffer.value(), "b");
        input.history_down();
        assert_eq!(input.buffer.value(), "draft");

        input.buffer.clear();
        type_text(&mut input, "line1");
        input.buffer.add_line();
        type_text(&mut input, "line2");
        assert!(input.is_at_last_line());
        input.history_up();
        input.history_down();
        assert_eq!(input.buffer.value(), "line1\nline2");
        assert!(input.is_at_first_line());

        input.submit();
        input.history_up();
        assert_eq!(input.buffer.value(), "line1\nline2");
        assert!(input.is_at_last_line());

        input.history_down();
        assert_eq!(input.buffer.value(), "");

        input.set_input("alpha\nbeta".into());
        input.submit();
        input.set_input("gamma\ndelta".into());
        input.submit();

        input.history_up();
        input.history_up();
        assert_eq!(input.buffer.value(), "alpha\nbeta");
        assert!(input.is_at_last_line());

        input.history_down();
        assert_eq!(input.buffer.value(), "gamma\ndelta");
        assert!(input.is_at_first_line());

        input.history_down();
        assert_eq!(input.buffer.value(), "");
    }

    #[test]
    fn cursor_adds_extra_wrap_row_at_boundary() {
        let width: u16 = 12;
        let ew = effective_width(composer_content_width(width) as usize);

        let mut at_boundary = InputBox::new(InputHistory::default(), 20);
        type_text(&mut at_boundary, &"x".repeat(ew));

        let mut before_boundary = InputBox::new(InputHistory::default(), 20);
        type_text(&mut before_boundary, &"x".repeat(ew - 1));

        assert_eq!(
            at_boundary.height(width),
            before_boundary.height(width) + 1,
            "cursor at boundary should cause one extra visual line"
        );
    }

    fn render_input_with(
        input: &mut InputBox,
        width: u16,
        height: u16,
        placeholder: Placeholder,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        let border_style = Style::new().fg(theme::current().mode_build);
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, height);
                input.view(frame, area, placeholder, border_style, true, None);
            })
            .unwrap();
        terminal
    }

    fn render_input(
        input: &mut InputBox,
        width: u16,
        height: u16,
    ) -> ratatui::Terminal<ratatui::backend::TestBackend> {
        render_input_with(input, width, height, Placeholder::Suggestion)
    }

    #[test]
    fn admission_hint_hits_exclude_separators_and_hover_only_the_control() {
        let area = Rect::new(0, 0, 80, 3);
        let (hint, hits) = admission_hint(area, Some(PromptAdmission::Steer));
        assert_eq!(hits.len(), ADMISSION_OPTIONS.len());
        assert!(hits.iter().all(|hit| {
            hit.area.width > 0 && hit.area.right() <= area.right() && hit.area.y == area.y
        }));

        let mut input = InputBox::new(InputHistory::default(), 20);
        let backend = ratatui::backend::TestBackend::new(area.width, area.height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                input.view(
                    frame,
                    area,
                    Placeholder::Queue,
                    Style::new(),
                    true,
                    Some(hint.clone()),
                );
            })
            .unwrap();
        let steer = hits
            .iter()
            .find(|hit| hit.admission == PromptAdmission::Steer)
            .unwrap();
        let buffer = terminal.backend().buffer();

        assert!(
            buffer
                .cell((steer.area.x, steer.area.y))
                .unwrap()
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !buffer
                .cell((steer.area.x - 1, steer.area.y))
                .unwrap()
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn paste_hover_reverses_only_the_summarized_token() {
        let area = Rect::new(0, 0, 60, 3);
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.handle_paste("a\nb\nc");
        let content = content_area(area);
        let token_x = content.x + PREFIX_WIDTH;
        input.update_paste_hover(content, content.y, token_x, true);
        assert!(input.hovered_paste.is_some());

        let terminal = render_input_with(&mut input, area.width, area.height, Placeholder::Blank);
        let buffer = terminal.backend().buffer();
        assert!(
            buffer
                .cell((token_x, content.y))
                .unwrap()
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !buffer
                .cell((token_x - 1, content.y))
                .unwrap()
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    fn has_scrollbar_thumb(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> bool {
        let buf = terminal.backend().buffer();
        (0..buf.area.height).any(|y| {
            (0..buf.area.width).any(|x| {
                buf.cell((x, y))
                    .is_some_and(|c| c.symbol() == SCROLLBAR_THUMB)
            })
        })
    }

    #[test_case(20, true  ; "visible_when_content_overflows")]
    #[test_case(0,  false ; "hidden_when_content_fits")]
    fn scrollbar_visibility(extra_lines: usize, expect_visible: bool) {
        let mut input = InputBox::new(InputHistory::default(), 20);
        for _ in 0..extra_lines {
            input.buffer.add_line();
        }
        let terminal = render_input(&mut input, 40, 20 + 2);
        assert_eq!(has_scrollbar_thumb(&terminal), expect_visible);
    }

    #[test]
    fn scroll_clamped_on_content_shrink() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        for _ in 0..20 {
            input.buffer.add_line();
        }
        let area_height = 5_u16;
        let _ = render_input(&mut input, 40, area_height);
        let scroll_before = input.scroll_y;
        assert!(scroll_before > 0);

        input.buffer = InputDocument::from_plain("short".into());
        let _ = render_input(&mut input, 40, area_height);
        assert_eq!(input.scroll_y, 0);
    }

    #[test]
    fn multibyte_input_renders_without_panic() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "● grep> hello");
        input.buffer.move_home();
        input.buffer.move_right();
        input.buffer.move_right();
        let _ = render_input(&mut input, 40, 5);
    }

    #[test_case("●\\", true  ; "after_multibyte")]
    #[test_case("●", false   ; "inside_multibyte_would_be_false")]
    fn char_before_cursor_backslash(input: &str, expected: bool) {
        let mut input_box = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input_box, input);
        assert_eq!(input_box.char_before_cursor_is_backslash(), expected);
    }

    fn rendered_row(
        terminal: &ratatui::Terminal<ratatui::backend::TestBackend>,
        row: u16,
    ) -> String {
        let buf = terminal.backend().buffer();
        let content = content_area(buf.area);
        (content.x..content.right())
            .map(|col| buf.cell((col, row)).unwrap().symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn prefix_on_single_line() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "hello");
        let terminal = render_input(&mut input, 20, 4);
        let row = rendered_row(&terminal, 1);
        assert!(row.starts_with(CHEVRON), "row: {row:?}");
        assert!(row.contains("hello"));
    }

    #[test]
    fn composer_uses_a_padded_panel_and_accent_rail() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "hello");
        let terminal = render_input(&mut input, 40, 5);
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer.cell((0, 1)).unwrap().symbol(), "┃");
        assert_eq!(buffer.cell((1, 1)).unwrap().symbol(), " ");
        assert_eq!(buffer.cell((2, 1)).unwrap().symbol(), " ");
        assert_eq!(buffer.cell((3, 1)).unwrap().symbol(), "❯");
        assert_eq!(
            buffer.cell((39, 1)).unwrap().style().bg,
            theme::current().element_style().bg
        );
    }

    #[test]
    fn prefix_on_multiline() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "aaa");
        input.buffer.add_line();
        type_text(&mut input, "bbb");
        let terminal = render_input(&mut input, 20, 5);
        let row0 = rendered_row(&terminal, 1);
        let row1 = rendered_row(&terminal, 2);
        assert!(row0.starts_with(CHEVRON), "row0: {row0:?}");
        assert!(row1.starts_with(NEWLINE_PAD), "row1: {row1:?}");
    }

    #[test]
    fn wrapped_line_gets_no_padding() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        let ew = effective_width(composer_content_width(14) as usize);
        type_text(&mut input, &"x".repeat(ew + 3));
        let terminal = render_input(&mut input, 14, 5);
        let row0 = rendered_row(&terminal, 1);
        let row1 = rendered_row(&terminal, 2);
        assert!(row0.starts_with(CHEVRON), "row0: {row0:?}");
        assert!(
            !row1.starts_with(CHEVRON) && !row1.starts_with(NEWLINE_PAD),
            "wrapped row should have no padding: {row1:?}"
        );
        assert!(
            row1.starts_with("x"),
            "wrapped row should start with content: {row1:?}"
        );
    }

    #[test]
    fn copy_text_includes_prefix() {
        let input = InputBox::new(InputHistory::default(), 20);
        assert_eq!(input.copy_text(), CHEVRON);

        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "line1");
        input.buffer.add_line();
        type_text(&mut input, "line2");
        assert_eq!(input.copy_text(), "❯ line1\n  line2");
    }

    #[test_case(Placeholder::Blank, "" ; "blank_shows_only_the_chevron")]
    #[test_case(Placeholder::Queue, QUEUE_PLACEHOLDER ; "queue_asks_for_another_prompt")]
    #[test_case(Placeholder::Steer, STEER_PLACEHOLDER ; "steer_targets_the_task")]
    fn placeholder_row(placeholder: Placeholder, expected: &str) {
        let mut input = InputBox::new(InputHistory::default(), 20);
        let terminal = render_input_with(&mut input, 40, 4, placeholder);
        assert_eq!(
            rendered_row(&terminal, 1),
            format!("{CHEVRON}{expected}").trim_end()
        );
    }

    #[test]
    fn suggestion_placeholder_shows_a_hint() {
        const HINT: &str = "fix a bug";
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.placeholder_hint = HINT;
        let terminal = render_input(&mut input, 40, 4);
        assert_eq!(
            rendered_row(&terminal, 1),
            format!("{CHEVRON}{ASK_PREFIX}{HINT}{ASK_SUFFIX}")
        );
    }

    fn test_image() -> ImageSource {
        use maki_providers::ImageMediaType;
        use std::sync::Arc;
        ImageSource::new(ImageMediaType::Png, Arc::from("dGVzdA=="))
    }

    #[test]
    fn submit_with_images() {
        let mut input = InputBox::new(InputHistory::default(), 20);

        input.attach_image(test_image());
        let sub = input.submit().unwrap();
        assert!(sub.text.is_empty());
        assert_eq!(sub.images.len(), 1);
        assert!(input.submit().is_none(), "images cleared after submit");

        type_text(&mut input, "describe this");
        input.attach_image(test_image());
        let sub = input.submit().unwrap();
        assert_eq!(sub.text, "describe this");
        assert_eq!(sub.images.len(), 1);
    }

    const IMAGE_LABEL: &str = "1 image";

    #[test]
    fn image_label_rendered() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.attach_image(test_image());
        let h = input.height(40);
        let terminal = render_input(&mut input, 40, h);
        let found = (0..h).any(|row| rendered_row(&terminal, row).contains(IMAGE_LABEL));
        assert!(found, "image label not found in rendered output");
    }

    #[test]
    fn height_accounts_for_pending_images() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        let base_height = input.height(TEST_WIDTH);
        input.attach_image(test_image());
        assert_eq!(input.height(TEST_WIDTH), base_height + 1);
    }

    #[test_case("read", "src/main.rs", " src/main.rs" ; "leading_after_ascii")]
    #[test_case("打开", "src/main.rs", " src/main.rs" ; "leading_after_unicode")]
    #[test_case("", "src/main.rs", "src/main.rs" ; "no_leading_at_start")]
    #[test_case("read ", "src/main.rs", "src/main.rs" ; "no_leading_after_space")]
    #[test_case("--file=", "src/main.rs", "src/main.rs" ; "no_leading_after_equals")]
    #[test_case("/", "src/main.rs", "src/main.rs" ; "no_leading_after_slash")]
    #[test_case("\"", "src/main.rs", "src/main.rs" ; "no_leading_after_quote")]
    #[test_case("'", "src/main.rs", "src/main.rs" ; "no_leading_after_squote")]
    #[test_case("foo_", "src/main.rs", " src/main.rs" ; "leading_after_underscore")]
    #[test_case("$(cmd)", "src/main.rs", " src/main.rs" ; "leading_after_closing_paren")]
    #[test_case("arr[0]", "src/main.rs", " src/main.rs" ; "leading_after_closing_bracket")]
    fn paste_with_spaces_leading(before: &str, paste: &str, expected_suffix: &str) {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, before);
        input.handle_paste_with_spaces(paste);
        assert_eq!(input.buffer.value(), format!("{before}{expected_suffix}"));
    }

    #[test_case("file", 0, "/tmp/foo", "/tmp/foo file" ; "trailing_before_ascii")]
    #[test_case("を読む", 0, "/tmp/foo", "/tmp/foo を読む" ; "trailing_before_unicode")]
    #[test_case("foobar", 3, "src/main.rs", "foo src/main.rs bar" ; "both_sides_mid_word")]
    #[test_case("in  between", 3, "file.rs", "in file.rs between" ; "neither_side_between_spaces")]
    #[test_case("read ''", 6, "src/main.rs", "read 'src/main.rs'" ; "neither_side_between_quotes")]
    fn paste_with_spaces_at_cursor(before: &str, cursor_at: usize, paste: &str, expected: &str) {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, before);
        let back = before.chars().count() - cursor_at;
        for _ in 0..back {
            input.buffer.move_left();
        }
        input.handle_paste_with_spaces(paste);
        assert_eq!(input.buffer.value(), expected);
    }

    #[test]
    fn paste_with_spaces_empty_line() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.handle_paste_with_spaces("file.rs");
        assert_eq!(input.buffer.value(), "file.rs");
    }

    #[test]
    fn file_picker_insertion_never_creates_paste_token() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        let path = format!("/tmp/{}", "x".repeat(151));
        input.handle_paste_with_spaces(&path);
        assert_eq!(input.buffer.display_text(), path);
        assert!(!input.has_pastes());
    }

    #[test]
    fn paste_with_spaces_text_has_leading_space() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "read");
        input.handle_paste_with_spaces(" file.rs");
        assert_eq!(input.buffer.value(), "read file.rs");
    }

    #[test]
    fn paste_with_spaces_text_has_trailing_space() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "file");
        for _ in 0..4 {
            input.buffer.move_left();
        }
        input.handle_paste_with_spaces("src/main.rs ");
        assert_eq!(input.buffer.value(), "src/main.rs file");
    }

    #[test]
    fn paste_with_spaces_multiline_buffer_cursor_on_second_line() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.handle_paste("first\nread");
        input.handle_paste_with_spaces("file.rs");
        assert_eq!(input.buffer.value(), "first\nread file.rs");
    }

    #[test]
    fn paste_with_spaces_cursor_at_end_no_trailing() {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "read");
        input.handle_paste_with_spaces("file.rs");
        assert_eq!(input.buffer.value(), "read file.rs");
    }

    // ew = area.width - PREFIX_WIDTH (2); with width 10, row_width is 8.
    fn area(width: u16) -> Rect {
        Rect::new(0, 0, width, 10)
    }

    fn single_line(text: &str) -> InputBox {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, text);
        input
    }

    #[test_case("abc", (0, 0) => Some((0, 0)); "click in chevron prefix of first row")]
    #[test_case("abc", (0, 1) => Some((0, 0)); "click on trailing part of prefix")]
    #[test_case("abc", (0, 2) => Some((0, 0)); "click on first text column")]
    #[test_case("abc", (0, 9) => Some((0, 3)); "click past end of line clamps to line end")]
    #[test_case("abc", (1, 0) => None; "click below content returns None")]
    fn click_position_prefix_and_clamp(
        text: &str,
        (row, col): (u16, u16),
    ) -> Option<(usize, usize)> {
        single_line(text).click_position(area(10), row, col, true)
    }

    #[test_case((1, 0) => Some((1, 0)); "second line col 0 is its start")]
    #[test_case((1, 1) => Some((1, 0)); "second line prefix occupies its first two cols")]
    #[test_case((1, 3) => Some((1, 1)); "second line text starts after the prefix")]
    #[test_case((1, 4) => Some((1, 2)); "second line maps last column")]
    fn click_position_second_line_prefix((row, col): (u16, u16)) -> Option<(usize, usize)> {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "abc");
        input.buffer.add_line();
        type_text(&mut input, "def");
        input.click_position(area(10), row, col, true)
    }

    #[test_case((1, 0) => Some((0, 8)); "continuation row first col maps to wrapped chunk start")]
    #[test_case((1, 1) => Some((0, 9)); "continuation row has no prefix offset")]
    fn click_position_wrapped_line_has_no_prefix((row, col): (u16, u16)) -> Option<(usize, usize)> {
        // width 10 -> ew 8 -> "abcdefghij" wraps as [0,8) then [8,10).
        single_line("abcdefghij").click_position(area(10), row, col, true)
    }

    #[test_case(true, (1, 2) => Some((0, 8)); "focused full cursor line gets an extra row")]
    #[test_case(false, (1, 2) => Some((1, 0)); "unfocused full cursor line has no extra row")]
    fn click_position_cursor_extra_row(
        focused: bool,
        (row, col): (u16, u16),
    ) -> Option<(usize, usize)> {
        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, "abcdefgh");
        input.buffer.add_line();
        type_text(&mut input, "xy");
        input.buffer.set_cursor(0, 8);
        input.click_position(area(10), row, col, focused)
    }
}
