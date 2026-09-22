use std::time::{SystemTime, UNIX_EPOCH};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::shell::parse_shell_prefix;
use crate::components::keybindings::leader;
use crate::highlight;
use crate::input_document::{InputDocument, InputDraft, PasteId, should_summarize_paste};
use crate::text_buffer::{EditResult, TextBuffer, is_newline_key};
use crate::theme;

use caudra_agent::PromptAdmission;
use caudra_agent::mentions::{self, Mention};
use caudra_grab::grab_scope;
use caudra_storage::input_history::InputHistory;
use caudra_workbench::buffer::Cursor;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use std::mem;
use std::ops::Range;
use std::path::PathBuf;

use caudra_providers::ImageSource;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph};

use super::scrollbar::{Scrollbar, ScrollbarMouse};
use super::{apply_scroll_delta, hover_style};

#[cfg(test)]
const CHEVRON: &str = super::CHEVRON;
const NEWLINE_PAD: &str = "  ";
const PREFIX_WIDTH: u16 = 2;
const SPACE: char = ' ';
const SELECTION: Style = Style::new().add_modifier(Modifier::REVERSED);
const COMPOSER_RAIL_WIDTH: u16 = 1;
const COMPOSER_VERTICAL_PADDING: u16 = 1;
const ADMISSION_SEPARATOR: &str = "  ";
const ADMISSION_DESCRIPTION_GAP: &str = " ";
const HINT_PAD: &str = " ";
const HINT_KEY_GAP: &str = " ";
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
const ASK_PREFIX: &str = "Ask caudra to ";
const ASK_SUFFIX: &str = "...";
const BLANK_PLACEHOLDER: &str = " ";
const ADMISSION_OPTIONS: [(&str, &str, PromptAdmission); 3] = [
    ("Enter", "next", PromptAdmission::Queue),
    (leader::STEER_PROMPT.label, "guide", PromptAdmission::Steer),
    (
        leader::INTERRUPT_PROMPT.label,
        "replace",
        PromptAdmission::Interrupt,
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdmissionHit {
    pub area: Rect,
    pub admission: PromptAdmission,
}

/// A chord the composer's top row advertises when nothing louder wants it.
/// The key travels with the target, so a click can never press something other
/// than what the hint reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChordHint {
    Tasks,
    PlanOrTodo,
}

impl ChordHint {
    pub(crate) fn key_label(self) -> &'static str {
        match self {
            Self::Tasks => leader::TASKS.label,
            Self::PlanOrTodo => leader::PLAN_TOGGLE.label,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChordHintHit {
    pub area: Rect,
    pub target: ChordHint,
}

#[derive(Clone, Copy)]
pub enum Placeholder {
    Suggestion,
    Blank,
    Queue,
    QueueEdit,
    Steer,
}

/// What a click or the pointer landed on in the composer, when it landed on
/// anything. A mention carries the char range it occupies as well as the
/// mention itself, because marking it needs the glyphs rather than the path.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InputHit {
    Paste(PasteId),
    Mention {
        range: Range<usize>,
        mention: Mention,
    },
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
    pub mentions: Vec<Mention>,
    pub(crate) draft: InputDraft,
}

pub(crate) struct InputState {
    draft: InputDraft,
    images: Vec<ImageSource>,
}

impl InputState {
    pub(crate) fn new(draft: InputDraft, images: Vec<ImageSource>) -> Self {
        Self { draft, images }
    }

    pub(crate) fn into_parts(self) -> (InputDraft, Vec<ImageSource>) {
        (self.draft, self.images)
    }
}

impl Submission {
    pub fn empty() -> Self {
        Self {
            text: String::new(),
            images: Vec::new(),
            mentions: Vec::new(),
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
            mentions: Vec::new(),
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
    scrollbar: Scrollbar,
    hover: Option<InputHit>,
    /// Resolves the relative paths a mention names. Mentions stay inert until
    /// the app hands the composer the session's working directory.
    cwd: PathBuf,
    remote_workspace: bool,
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

        // Shift means the caret is growing a selection, so the edge lines stay
        // edges instead of handing the draft over to history recall.
        let extending = key.modifiers.contains(KeyModifiers::SHIFT);

        match key.code {
            KeyCode::Up if !extending && self.is_at_first_line() => {
                self.history_up();
                return InputAction::None;
            }
            KeyCode::Down if !extending && self.is_at_last_line() => {
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
            scrollbar: Scrollbar::default(),
            hover: None,
            cwd: PathBuf::new(),
            remote_workspace: false,
        }
    }

    pub(crate) fn set_cwd(&mut self, cwd: impl Into<PathBuf>) {
        self.cwd = cwd.into();
        self.remote_workspace = false;
    }

    pub(crate) fn set_remote_cwd(&mut self) {
        self.cwd.clear();
        self.remote_workspace = true;
    }

    /// The mentions the composer text resolves, as char ranges into it.
    ///
    /// Derived on demand rather than stored as spans: a mention's display form
    /// *is* its text, so there is nothing a span would preserve, and leaving it
    /// as ordinary text keeps it hand-editable — retyping `L12` as `L13` is a
    /// normal edit rather than a token replacement. Scanning the display text
    /// rather than the expanded text also means an `@path` sitting inside
    /// pasted content stays data instead of becoming a request to read a file.
    pub(crate) fn mentions(&self) -> Vec<(Range<usize>, Mention)> {
        if self.remote_workspace {
            mentions::scan_remote(&self.buffer.display_text())
        } else {
            mentions::scan_in(&self.buffer.display_text(), &self.cwd)
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        self.buffer.selected_text()
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
        let mut mentions: Vec<Mention> = self.mentions().into_iter().map(|(_, m)| m).collect();
        mentions.dedup_by(|left, right| left.target == right.target && left.lines == right.lines);
        if record_history {
            self.history.push(text.clone());
        }
        self.discard();
        Some(Submission {
            text,
            images,
            mentions,
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

    fn visual_cursor_y(&self, ew: usize, cursor_visible: bool) -> u16 {
        let lines = self.buffer.lines();
        let rows_above: usize = lines
            .iter()
            .take(self.buffer.y())
            .map(|line| wrap_row_count(line, ew, false))
            .sum();

        let chars: Vec<char> = lines[self.buffer.y()].chars().collect();
        let wrap_row = wrap_ranges(&chars, ew, cursor_visible)
            .iter()
            .rposition(|&(start, _)| self.buffer.x() >= start)
            .unwrap_or(0);

        (rows_above + wrap_row) as u16
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
        grab_scope!("input", area);
        let content_area = content_area(area);
        let content_height = content_area.height;
        let ew = effective_width(content_area.width as usize);
        let focused_paste = focused.then(|| self.buffer.focused_paste()).flatten();
        let cursor_visible = focused && focused_paste.is_none();

        if self.follow_cursor {
            let visual_cursor_y = self.visual_cursor_y(ew, cursor_visible);
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
        let mention_ranges: Vec<Range<usize>> = self
            .mentions()
            .into_iter()
            .map(|(range, _)| range)
            .collect();
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
                    let tokens = token_ranges_on_line(
                        &self.buffer,
                        i,
                        &mention_ranges,
                        focused_paste,
                        self.hover.as_ref(),
                    );
                    let is_cursor_line = i == cursor_y && focused && focused_paste.is_none();
                    let shell_spans = if i == 0 && tokens.is_empty() {
                        shell_highlight_spans(line)
                    } else {
                        None
                    };
                    let mut styled_spans = match tokens.is_empty() {
                        true => shell_spans,
                        false => Some(token_spans(line, &tokens)),
                    };
                    if let Some(range) = self.buffer.selection_on_line(i) {
                        let spans = styled_spans.unwrap_or_else(|| vec![Span::raw(line.clone())]);
                        styled_spans = Some(apply_selection(spans, &range));
                    }
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

        self.scrollbar
            .draw(frame, content_area, total_vl, self.scroll_y);
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

    /// The composer routes the rest of the pointer through `handle_click`, so
    /// the bar is offered on its own and ahead of it: a drag on the bar must
    /// not move the caret.
    pub fn handle_scrollbar(&mut self, event: &MouseEvent) -> bool {
        match self.scrollbar.handle(event) {
            ScrollbarMouse::Ignored => false,
            ScrollbarMouse::Consumed => true,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll_y = top as u16;
                true
            }
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll_y = apply_scroll_delta(self.scroll_y, delta).min(self.max_scroll());
        self.follow_cursor = false;
    }

    /// Pages a draft too tall for its viewport, and reports whether it claimed
    /// the key. A draft that fits leaves the navigation keys to the transcript
    /// rather than eating them to do nothing.
    pub fn page(&mut self, up: bool) -> bool {
        if self.max_scroll() == 0 {
            return false;
        }
        let half = (self.last_content_height / 2).max(1) as i32;
        self.scroll(if up { half } else { -half });
        true
    }

    /// Move the text cursor to the position corresponding to a mouse click at
    /// the terminal coordinates (row, col) within the input content area.
    pub fn handle_click(
        &mut self,
        area: Rect,
        row: u16,
        col: u16,
        focused: bool,
    ) -> Option<InputHit> {
        let (y, x, text_hit) = self.click_position_with_hit(area, row, col, focused)?;
        let hit = text_hit.then(|| self.hit_at(y, x)).flatten();
        match hit {
            Some(InputHit::Paste(id)) => {
                self.buffer.focus_paste(id);
                self.follow_cursor = true;
                Some(InputHit::Paste(id))
            }
            hit => {
                self.buffer.set_cursor(y, x);
                self.follow_cursor = true;
                hit
            }
        }
    }

    /// Extends the selection to the pointer. The press already dropped the
    /// anchor, so a sweep only ever moves the far end.
    pub fn handle_drag(&mut self, area: Rect, row: u16, col: u16, focused: bool) {
        let Some((y, x, _)) = self.click_position_with_hit(area, row, col, focused) else {
            return;
        };
        self.buffer.set_caret(Cursor::new(y, x), true);
        self.follow_cursor = true;
    }

    /// A mention keeps the text cursor where it was clicked, unlike a paste
    /// label, which the cursor may not sit inside.
    fn hit_at(&self, y: usize, x: usize) -> Option<InputHit> {
        if let Some(id) = self.buffer.paste_at(y, x) {
            return Some(InputHit::Paste(id));
        }
        let offset = self.buffer.line_offset(y)? + x;
        self.mentions()
            .into_iter()
            .find(|(range, _)| range.contains(&offset))
            .map(|(range, mention)| InputHit::Mention { range, mention })
    }

    pub(crate) fn update_hover(&mut self, area: Rect, row: u16, col: u16, focused: bool) {
        self.hover = self
            .click_position_with_hit(area, row, col, focused)
            .and_then(|(y, x, text_hit)| text_hit.then(|| self.hit_at(y, x)).flatten());
    }

    pub(crate) fn clear_hover(&mut self) {
        self.hover = None;
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
            let ranges = wrap_ranges(&chars, ew, is_cursor_line);

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
        let key_style = hover_style(theme.keybind_key, is_hovered);
        let description_style = hover_style(theme.tool_dim, is_hovered);
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

/// A `text key` control right-aligned into [`top_right_hint_area`], and the
/// cells it occupies. The padding on either side is separator rather than
/// control, so it neither takes the click nor lights up under the pointer. A
/// region too narrow to hold the control leaves a zero-width rect, which no
/// pointer can be inside.
pub(crate) fn chord_hint(
    area: Rect,
    text: &str,
    target: ChordHint,
    hovered: bool,
) -> (Line<'static>, ChordHintHit) {
    let theme = theme::current();
    let key_label = target.key_label();
    let control_width = text.width() + HINT_KEY_GAP.width() + key_label.width();
    let line = Line::from(vec![
        Span::raw(HINT_PAD),
        Span::styled(
            text.to_string(),
            hover_style(Style::new().fg(theme.foreground), hovered),
        ),
        Span::styled(
            format!("{HINT_KEY_GAP}{key_label}"),
            hover_style(theme.keybind_key, hovered),
        ),
        Span::raw(HINT_PAD),
    ]);

    let hint_area = top_right_hint_area(area);
    let full_width = control_width + HINT_PAD.width() * 2;
    let left = if full_width <= hint_area.width as usize {
        hint_area.right().saturating_sub(full_width as u16)
    } else {
        hint_area.x
    };
    let x = left.saturating_add(HINT_PAD.width() as u16);
    let hit = ChordHintHit {
        area: Rect::new(
            x,
            hint_area.y,
            (control_width as u16).min(hint_area.right().saturating_sub(x)),
            hint_area.height.min(1),
        ),
        target,
    };
    (line, hit)
}

pub(crate) fn top_right_hint_area(area: Rect) -> Rect {
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

    wrap_ranges(&chars, ew, is_cursor_line)
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

/// Split a line into wrapped row ranges of char indices, exactly as it is
/// rendered: a row breaks after the last space that fits so a word is never
/// split, a run too long for any row breaks on a char boundary, and an extra
/// empty row is appended when the cursor sits past a completely full last row.
///
/// A trailing space is allowed past the right margin rather than pushed onto
/// the next row, so a wrapped word starts flush instead of indented. Ranges
/// tile the line contiguously, which is what lets the click hit-test and the
/// cursor's visual row read this one function.
fn wrap_ranges(chars: &[char], ew: usize, is_cursor_line: bool) -> Vec<(usize, usize)> {
    let row_width = ew.max(1);
    let widths: Vec<usize> = chars.iter().map(|c| c.width().unwrap_or(1)).collect();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut row_start = 0;
    let mut row_col = 0;
    let mut after_space = None;
    for (i, &w) in widths.iter().enumerate() {
        let is_space = chars[i] == SPACE;
        if !is_space && row_col + w > row_width && row_col > 0 {
            let end = after_space.filter(|&end| end > row_start).unwrap_or(i);
            ranges.push((row_start, end));
            row_start = end;
            row_col = widths[end..i].iter().sum();
            after_space = None;
        }
        if is_space {
            after_space = Some(i + 1);
        }
        row_col += w;
    }
    if row_start < chars.len() || ranges.is_empty() {
        ranges.push((row_start, chars.len()));
    }
    if is_cursor_line && row_col + 1 > row_width {
        ranges.push((chars.len(), chars.len()));
    }
    ranges
}

fn wrap_row_count(line: &str, ew: usize, is_cursor_line: bool) -> usize {
    let chars: Vec<char> = line.chars().collect();
    wrap_ranges(&chars, ew, is_cursor_line).len()
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
    let mut hl = caudra_highlight::Highlighter::for_token("bash");
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

/// Repaints `range` as selected, splitting spans at its edges. Selection is a
/// modifier on top of whatever colour a span already carries, so a selected
/// paste chip or shell keyword still reads as one.
fn apply_selection(spans: Vec<Span<'static>>, range: &Range<usize>) -> Vec<Span<'static>> {
    let mut result = Vec::with_capacity(spans.len() + 2);
    let mut pos = 0;
    for span in spans {
        let len = span.content.chars().count();
        let end = pos + len;
        if end <= range.start || pos >= range.end {
            result.push(span);
            pos = end;
            continue;
        }
        let lo = range.start.saturating_sub(pos).min(len);
        let hi = (range.end - pos).min(len);
        for (from, to, selected) in [(0, lo, false), (lo, hi, true), (hi, len, false)] {
            if from >= to {
                continue;
            }
            let text: String = span.content.chars().skip(from).take(to - from).collect();
            let style = match selected {
                true => span.style.patch(SELECTION),
                false => span.style,
            };
            result.push(Span::styled(text, style));
        }
        pos = end;
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
        .map(|(i, line)| wrap_row_count(line, ew, cursor_visible && i == cursor_y))
        .sum()
}

/// Line-local styled ranges for both kinds of composer token: paste labels,
/// which stand in for text that is not shown, and mentions, which are ordinary
/// text the model will be handed the contents of. Sorted so [`token_spans`] can
/// walk them in one pass; the two kinds cannot overlap, because a mention is
/// scanned out of the display text a paste label already occupies.
fn token_ranges_on_line(
    buffer: &InputDocument,
    y: usize,
    mentions: &[Range<usize>],
    focused: Option<PasteId>,
    hovered: Option<&InputHit>,
) -> Vec<(Range<usize>, Style)> {
    let mut tokens: Vec<(Range<usize>, Style)> = buffer
        .paste_ranges_on_line(y)
        .into_iter()
        .map(|(range, id)| {
            let on = matches!(hovered, Some(InputHit::Paste(other)) if *other == id);
            let style = match focused == Some(id) {
                true => theme::current().item_selected,
                false => hover_style(theme::current().active, on),
            };
            (range, style)
        })
        .collect();
    if let Some(start) = buffer.line_offset(y) {
        let end = start + buffer.lines()[y].chars().count();
        tokens.extend(
            mentions
                .iter()
                .filter(|range| range.start >= start && range.end <= end)
                .map(|range| {
                    let on = matches!(hovered, Some(InputHit::Mention { range: other, .. }) if other == range);
                    (
                        range.start - start..range.end - start,
                        hover_style(theme::current().mention, on),
                    )
                }),
        );
    }
    tokens.sort_by_key(|(range, _)| range.start);
    tokens
}

fn token_spans(line: &str, tokens: &[(Range<usize>, Style)]) -> Vec<Span<'static>> {
    let mut spans = Vec::with_capacity(tokens.len() * 2 + 1);
    let mut cursor = 0;
    for (range, style) in tokens {
        if cursor < range.start {
            spans.push(Span::raw(
                line.chars()
                    .skip(cursor)
                    .take(range.start - cursor)
                    .collect::<String>(),
            ));
        }
        let text: String = line.chars().skip(range.start).take(range.len()).collect();
        spans.push(Span::styled(text, *style));
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
    use caudra_workbench::scroll::SCROLLBAR_THUMB;
    use crossterm::event::KeyModifiers;
    use ratatui::layout::Rect;
    use test_case::test_case;

    const EXPECT_PASTE_HIT: &str = "a click inside a paste label reports the paste, not a mention";
    const EXPECT_MENTION_HIT: &str = "a click inside a mention reports the file it names";
    const NOT_HOVERED: &str = "the pointer sat on the token without the composer noticing";
    const TOKEN_UNMARKED: &str = "the token under the pointer was drawn unmarked";
    const PROSE_MARKED: &str = "hover reached text outside the token";

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
        let InputHit::Paste(id) = input
            .handle_click(Rect::new(0, 0, 80, 1), 0, 2, true)
            .expect("paste token should be clickable")
        else {
            panic!("{EXPECT_PASTE_HIT}");
        };
        assert_eq!(input.buffer.focused_paste(), Some(id));
    }

    /// The composer resolves mentions against its own working directory, so a
    /// test can point it at a real tree and type a path that exists there.
    fn composer_at_crate_root(text: &str) -> InputBox {
        let mut input = InputBox::new(InputHistory::default(), 20);
        input.set_cwd(env!("CARGO_MANIFEST_DIR"));
        type_text(&mut input, text);
        input
    }

    #[test_case("@src/lib.rs", Some(None) ; "whole_file")]
    #[test_case("@src/lib.rs:L3-L9", Some(Some(3..=9)) ; "line_range")]
    #[test_case("look at @src/lib.rs now", Some(None) ; "mid_sentence")]
    #[test_case("@src/does_not_exist.rs", None ; "unresolved_path")]
    #[test_case("@dataclass", None ; "decorator")]
    fn the_composer_resolves_only_paths_that_exist(
        text: &str,
        expected: Option<Option<std::ops::RangeInclusive<usize>>>,
    ) {
        let mentions = composer_at_crate_root(text).mentions();
        assert_eq!(mentions.len(), usize::from(expected.is_some()), "{text}");
        if let Some(lines) = expected {
            assert_eq!(mentions[0].1.lines, lines);
        }
    }

    #[test]
    fn a_mention_stays_editable_rather_than_deleting_whole() {
        let mut input = composer_at_crate_root("@src/lib.rs");
        input.buffer.remove_char();
        assert_eq!(input.buffer.display_text(), "@src/lib.r");
        assert!(input.mentions().is_empty(), "the shortened path is prose");
    }

    #[test]
    fn clicking_a_mention_reports_the_file_it_names() {
        let mut input = composer_at_crate_root("@src/lib.rs");
        let hit = input
            .handle_click(Rect::new(0, 0, 80, 1), 0, 4, true)
            .expect("a mention is clickable");
        let InputHit::Mention { mention, .. } = hit else {
            panic!("{EXPECT_MENTION_HIT}");
        };
        assert_eq!(
            mention.local_path(),
            Some(std::path::Path::new("src/lib.rs"))
        );
    }

    #[test]
    fn a_submission_carries_its_mentions_once_each() {
        let mut input = composer_at_crate_root("@src/lib.rs and @src/lib.rs again");
        let submission = input.take_submission().expect("non-empty submission");
        assert_eq!(submission.mentions.len(), 1);
        assert_eq!(
            submission.mentions[0].local_path(),
            Some(std::path::Path::new("src/lib.rs"))
        );
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
        input.update_hover(content, content.y, token_x, true);
        assert!(input.hover.is_some(), "{NOT_HOVERED}");

        let terminal = render_input_with(&mut input, area.width, area.height, Placeholder::Blank);
        let buffer = terminal.backend().buffer();
        assert!(reversed(buffer, token_x, content.y), "{TOKEN_UNMARKED}");
        assert!(!reversed(buffer, token_x - 1, content.y), "{PROSE_MARKED}");
    }

    /// The mention is ordinary text the reader can edit, so only the glyphs
    /// the pointer is actually over are marked.
    #[test]
    fn mention_hover_reverses_only_the_path_it_names() {
        const PROSE: &str = "see ";
        let area = Rect::new(0, 0, 60, 3);
        let mut input = composer_at_crate_root(&format!("{PROSE}@src/lib.rs"));
        let content = content_area(area);
        let mention_x = content.x + PREFIX_WIDTH + PROSE.len() as u16;

        input.update_hover(content, content.y, mention_x, true);
        assert!(input.hover.is_some(), "{NOT_HOVERED}");

        let terminal = render_input_with(&mut input, area.width, area.height, Placeholder::Blank);
        let buffer = terminal.backend().buffer();
        assert!(reversed(buffer, mention_x, content.y), "{TOKEN_UNMARKED}");
        assert!(
            !reversed(buffer, mention_x - 1, content.y),
            "{PROSE_MARKED}"
        );
    }

    fn reversed(buffer: &ratatui::buffer::Buffer, x: u16, y: u16) -> bool {
        buffer
            .cell((x, y))
            .unwrap()
            .style()
            .add_modifier
            .contains(Modifier::REVERSED)
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
        use caudra_providers::ImageMediaType;
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

    const ROW_SEPARATOR: &str = "|";
    const BORDER_ROWS: u16 = 2;
    const HEIGHT_MSG: &str = "the box must be as tall as the rows the wrap produces";
    const FIXTURE_MSG: &str = "the fixture must wrap into more rows than a plain fill would";

    fn wrapped_rows(text: &str, ew: usize) -> String {
        let chars: Vec<char> = text.chars().collect();
        wrap_ranges(&chars, ew, false)
            .into_iter()
            .map(|(start, end)| chars[start..end].iter().collect::<String>())
            .collect::<Vec<_>>()
            .join(ROW_SEPARATOR)
    }

    #[test_case("hello", 8 => "hello"; "a line that fits keeps one row")]
    #[test_case("", 8 => ""; "an empty line still has one row")]
    #[test_case("hello world", 8 => "hello |world"; "a word moves whole onto the next row")]
    #[test_case("one two three", 8 => "one two |three"; "the break is the last space that fits")]
    #[test_case("abcdefghij", 8 => "abcdefgh|ij"; "a run too long for a row breaks on a char")]
    #[test_case("ab cdefghijkl", 8 => "ab |cdefghij|kl"; "an over-long word breaks after the space before it")]
    fn wrap_ranges_breaks_on_words(text: &str, ew: usize) -> String {
        wrapped_rows(text, ew)
    }

    #[test]
    fn height_matches_the_rows_the_wrap_produces() {
        let width: u16 = 24;
        let ew = effective_width(composer_content_width(width) as usize);
        let word = "a".repeat(ew / 2 + 1);
        let text = [word.as_str(); 4].join(" ");

        let mut input = InputBox::new(InputHistory::default(), 20);
        type_text(&mut input, &text);

        let rows = wrap_row_count(&text, ew, true);
        assert!(rows > text.len().div_ceil(ew), "{FIXTURE_MSG}");
        assert_eq!(
            input.height(width),
            rows as u16 + BORDER_ROWS,
            "{HEIGHT_MSG}"
        );
    }

    #[test_case((1, 0) => Some((0, 6)); "continuation row starts at the wrapped word")]
    #[test_case((1, 4) => Some((0, 10)); "the last char of the wrapped word is reachable")]
    fn click_position_word_wrapped_line((row, col): (u16, u16)) -> Option<(usize, usize)> {
        // ew 8: "hello world" wraps as "hello " then "world".
        single_line("hello world").click_position(area(10), row, col, true)
    }

    #[test_case(5 => 0; "a cursor before the break stays on the first row")]
    #[test_case(6 => 1; "a cursor on the wrapped word sits on its row")]
    fn visual_cursor_y_follows_the_wrapped_word(cursor_x: usize) -> u16 {
        let mut input = single_line("hello world");
        input.buffer.set_cursor(0, cursor_x);
        input.visual_cursor_y(8, true)
    }
}
