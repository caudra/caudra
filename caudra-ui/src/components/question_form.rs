//! The form behind the `question` tool: one tab per question, an option list,
//! and a free-text answer the user can type instead.
//!
//! The form holds only what the user has picked. Everything on screen is
//! derived from that plus the questions, so a resize or a theme switch can
//! never disagree with what will be submitted.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use caudra_agent::types::AskedQuestion;

use super::Overlay;
use super::form::render_form;
use crate::components::hint_line;
use crate::repaint::Cadence;
use crate::text_buffer::TextBuffer;
use crate::theme;

const TITLE: &str = " Question ";
const CUSTOM_OPTION: &str = "Type your own answer";
const REVIEW_TAB: &str = " Review ";
const TAB_SEPARATOR: &str = "│";
const ANSWERED_MARK: &str = " ✓ ";
const MULTI_HINT: &str = "  (multiple answers)";
const SINGLE_HINT: &str = "  (single answer)";
const NO_ANSWER: &str = "(no answer)";
const REVIEW_HEADING: &str = " Review your answers:";
const DESC_SEPARATOR: &str = " — ";
const ANSWER_ARROW: &str = "    → ";
const CUSTOM_PROMPT: &str = "  ❯ ";
/// Two borders plus the hint row.
const CHROME_ROWS: u16 = 3;
const MAX_HEIGHT_PERCENT: u16 = 75;

pub enum QuestionFormAction {
    Consumed,
    /// One label list per question, in question order. An empty list is a
    /// question the user skipped.
    Submit(Vec<Vec<String>>),
    Dismiss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Selecting,
    EditingCustom,
    Confirming,
}

pub struct QuestionForm {
    questions: Vec<AskedQuestion>,
    mode: Mode,
    tab: usize,
    cursor: usize,
    answers: Vec<Vec<String>>,
    custom: TextBuffer,
    scroll: u16,
    /// Where the form last drew, so a wheel event can tell whether it landed
    /// on the form or on the transcript behind it.
    area: Rect,
    row_hits: Vec<RowHit>,
    /// The row a left press landed on, so a release somewhere else is a drag
    /// rather than a click on whatever it ended up over.
    mouse_down: Option<usize>,
}

#[derive(Clone, Copy)]
struct RowHit {
    area: Rect,
    index: usize,
}

impl QuestionForm {
    pub fn new() -> Self {
        Self {
            questions: Vec::new(),
            mode: Mode::Selecting,
            tab: 0,
            cursor: 0,
            answers: Vec::new(),
            custom: TextBuffer::new(String::new()),
            scroll: 0,
            area: Rect::default(),
            row_hits: Vec::new(),
            mouse_down: None,
        }
    }

    pub fn open(&mut self, questions: Vec<AskedQuestion>) {
        self.answers = vec![Vec::new(); questions.len()];
        self.questions = questions;
        self.mode = Mode::Selecting;
        self.tab = 0;
        self.cursor = 0;
        self.custom = TextBuffer::new(String::new());
        self.scroll = 0;
        self.row_hits.clear();
        self.mouse_down = None;
    }

    pub fn is_open(&self) -> bool {
        !self.questions.is_empty()
    }

    pub fn close(&mut self) {
        self.questions.clear();
        self.answers.clear();
        self.row_hits.clear();
        self.mouse_down = None;
        self.area = Rect::default();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> QuestionFormAction {
        if !self.is_open() {
            return QuestionFormAction::Consumed;
        }
        match self.mode {
            Mode::Selecting => self.key_selecting(key),
            Mode::EditingCustom => self.key_editing(key),
            Mode::Confirming => self.key_confirming(key),
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        if self.mode != Mode::EditingCustom {
            return false;
        }
        self.custom.insert_text(text);
        true
    }

    pub fn contains(&self, pos: ratatui::layout::Position) -> bool {
        self.area.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll = self.scroll.saturating_add_signed(delta as i16);
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let width = area.width.saturating_sub(2);
        let lines = self.lines(width);
        let rows = visual_rows(&lines, width);
        let height = (rows.total + CHROME_ROWS)
            .min(area.height * MAX_HEIGHT_PERCENT / 100)
            .max(CHROME_ROWS + 1);
        let form = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(height),
            width: area.width,
            height,
        };
        self.area = form;
        // The cursor may sit below the fold on a long option list, so the
        // viewport follows it rather than staying where the user left it.
        let visible = height.saturating_sub(CHROME_ROWS);
        let focus = rows.row_of(self.focus_line());
        self.scroll = self
            .scroll
            .min(focus)
            .max(focus.saturating_sub(visible - 1));
        let t = theme::current();
        // The form floats over the transcript, so the cells behind it have to
        // go before anything is drawn: a border alone leaves the old text
        // showing through the gaps.
        frame.render_widget(Clear, form);
        frame.render_widget(Block::default().style(t.surface_style()), form);
        render_form(&t, TITLE, frame, form, lines, (self.scroll, 0));
        self.record_row_hits(&rows, form, visible);
        // `CHROME_ROWS` reserves the hint a content row of its own. Drawing it
        // one lower puts it on the bottom border, which then carries on to the
        // right of the text.
        let hint = Rect {
            x: form.x + 1,
            y: form.y + form.height.saturating_sub(2),
            width: form.width.saturating_sub(2),
            height: 1,
        };
        frame.render_widget(ratatui::widgets::Paragraph::new(self.hint()), hint);
        form
    }

    /// Where each option landed on screen, so a click can name the row it hit.
    /// Only the rows inside the viewport are recorded: one scrolled out of
    /// sight must not be clickable through whatever is drawn over it.
    fn record_row_hits(&mut self, rows: &VisualRows, form: Rect, visible: u16) {
        self.row_hits.clear();
        if self.mode != Mode::Selecting {
            return;
        }
        let first = self.first_option_line();
        let top = form.y + 1;
        for index in 0..=self.custom_row() {
            let start = rows.row_of(first + index as u16);
            let height = rows.height_of(first + index as u16);
            let Some(offset) = start.checked_sub(self.scroll) else {
                continue;
            };
            let height = height.min(visible.saturating_sub(offset));
            if height == 0 {
                continue;
            }
            self.row_hits.push(RowHit {
                area: Rect {
                    x: form.x + 1,
                    y: top + offset,
                    width: form.width.saturating_sub(2),
                    height,
                },
                index,
            });
        }
    }

    pub fn handle_mouse(&mut self, event: MouseEvent) -> QuestionFormAction {
        let position = Position::new(event.column, event.row);
        let hit = self
            .row_hits
            .iter()
            .find(|hit| hit.area.contains(position))
            .map(|hit| hit.index);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.mouse_down = hit;
                if let Some(index) = hit {
                    self.cursor = index;
                }
            }
            MouseEventKind::Moved => {
                if let Some(index) = hit {
                    self.cursor = index;
                }
            }
            // A press and release on the same row is the click; anything else
            // is a drag that only moved the cursor.
            MouseEventKind::Up(MouseButton::Left) => {
                let clicked = self
                    .mouse_down
                    .take()
                    .is_some_and(|pressed| hit == Some(pressed));
                if clicked {
                    return self.activate();
                }
            }
            _ => {}
        }
        QuestionFormAction::Consumed
    }

    /// What `Enter` does on the focused row, shared with the click path so the
    /// two can never drift.
    fn activate(&mut self) -> QuestionFormAction {
        if self.cursor == self.custom_row() {
            self.start_editing();
            return QuestionFormAction::Consumed;
        }
        let label = self.question().options[self.cursor].label.clone();
        if self.is_multi() {
            self.toggle(label);
            return QuestionFormAction::Consumed;
        }
        self.answers[self.tab] = vec![label];
        self.advance()
    }

    fn question(&self) -> &AskedQuestion {
        &self.questions[self.tab]
    }

    /// The custom-answer row always sits one past the offered options, so a
    /// question with no options still has something to pick.
    fn custom_row(&self) -> usize {
        self.question().options.len()
    }

    fn is_multi(&self) -> bool {
        self.question().multiple
    }

    /// A single question with one answer submits the moment it is picked;
    /// anything else needs a review step so the user can go back.
    fn has_review(&self) -> bool {
        self.questions.len() > 1 || self.questions.first().is_some_and(|q| q.multiple)
    }

    fn picked(&self) -> &[String] {
        &self.answers[self.tab]
    }

    fn is_picked(&self, label: &str) -> bool {
        self.picked().iter().any(|p| p == label)
    }

    /// The one answer that is not an offered label, if the user typed one.
    fn custom_answer(&self) -> Option<&String> {
        let options = &self.question().options;
        self.picked()
            .iter()
            .find(|pick| !options.iter().any(|o| &&o.label == pick))
    }

    fn key_selecting(&mut self, key: KeyEvent) -> QuestionFormAction {
        let last = self.custom_row();
        match key.code {
            KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down => self.cursor = (self.cursor + 1).min(last),
            KeyCode::Enter if self.cursor == last => self.start_editing(),
            KeyCode::Enter => {
                let label = self.question().options[self.cursor].label.clone();
                if self.is_multi() {
                    self.toggle(label);
                } else {
                    self.answers[self.tab] = vec![label];
                    return self.advance();
                }
            }
            KeyCode::Tab | KeyCode::Right if self.has_review() => return self.next_tab(),
            KeyCode::BackTab | KeyCode::Left if self.has_review() && self.tab > 0 => {
                self.tab -= 1;
                self.cursor = 0;
            }
            KeyCode::Esc => return QuestionFormAction::Dismiss,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return QuestionFormAction::Dismiss;
            }
            _ => {}
        }
        QuestionFormAction::Consumed
    }

    /// Editing starts from whatever the user typed last, so correcting a typo
    /// does not mean retyping the answer. The cursor lands at the end of it:
    /// a prefilled box the user types in front of is a worse trap than an
    /// empty one.
    fn start_editing(&mut self) {
        let existing = self.custom_answer().cloned().unwrap_or_default();
        self.custom = TextBuffer::new(existing);
        let end = self.custom.value().chars().count();
        self.custom.set_cursor_offset(end);
        self.mode = Mode::EditingCustom;
    }

    fn key_editing(&mut self, key: KeyEvent) -> QuestionFormAction {
        let newline = matches!(key.code, KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL))
            || (key.code == KeyCode::Enter && key.modifiers.intersects(NEWLINE_MODIFIERS));
        if newline {
            self.custom.add_line();
            return QuestionFormAction::Consumed;
        }
        match key.code {
            // A trailing backslash is the plain-terminal way to ask for a
            // newline where the modifier combination never arrives.
            KeyCode::Enter if ends_with_backslash(&self.custom) => {
                self.custom.remove_char();
                self.custom.add_line();
            }
            KeyCode::Enter => return self.commit_custom(),
            KeyCode::Esc => self.mode = Mode::Selecting,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return QuestionFormAction::Dismiss;
            }
            _ => edit(&mut self.custom, key),
        }
        QuestionFormAction::Consumed
    }

    fn commit_custom(&mut self) -> QuestionFormAction {
        let text = self.custom.value().trim().to_owned();
        self.mode = Mode::Selecting;
        let existing = self.custom_answer().cloned();
        match (text.is_empty(), existing) {
            // Clearing the box withdraws the typed answer rather than storing
            // an empty one.
            (true, Some(old)) => self.answers[self.tab].retain(|pick| pick != &old),
            (true, None) => {}
            (false, Some(old)) => {
                let slot = self.answers[self.tab]
                    .iter_mut()
                    .find(|pick| **pick == old)
                    .expect("the existing answer is in the list");
                *slot = text;
            }
            (false, None) if self.is_multi() => self.answers[self.tab].push(text),
            (false, None) => {
                self.answers[self.tab] = vec![text];
                return self.advance();
            }
        }
        QuestionFormAction::Consumed
    }

    fn key_confirming(&mut self, key: KeyEvent) -> QuestionFormAction {
        match key.code {
            KeyCode::Enter => QuestionFormAction::Submit(std::mem::take(&mut self.answers)),
            KeyCode::BackTab | KeyCode::Left => {
                self.tab = self.questions.len() - 1;
                self.cursor = 0;
                self.mode = Mode::Selecting;
                QuestionFormAction::Consumed
            }
            KeyCode::Esc => QuestionFormAction::Dismiss,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                QuestionFormAction::Dismiss
            }
            _ => QuestionFormAction::Consumed,
        }
    }

    fn toggle(&mut self, label: String) {
        let answers = &mut self.answers[self.tab];
        match answers.iter().position(|pick| *pick == label) {
            Some(index) => {
                answers.remove(index);
            }
            None => answers.push(label),
        }
    }

    fn advance(&mut self) -> QuestionFormAction {
        if self.has_review() {
            return self.next_tab();
        }
        QuestionFormAction::Submit(std::mem::take(&mut self.answers))
    }

    fn next_tab(&mut self) -> QuestionFormAction {
        if self.tab + 1 < self.questions.len() {
            self.tab += 1;
            self.cursor = 0;
            self.mode = Mode::Selecting;
        } else {
            self.mode = Mode::Confirming;
        }
        QuestionFormAction::Consumed
    }

    fn hint(&self) -> Line<'static> {
        match self.mode {
            Mode::EditingCustom => hint_line(&[
                ("Enter", "submit"),
                ("Shift+Enter", "newline"),
                ("Esc", "cancel"),
            ]),
            Mode::Confirming => hint_line(&[
                ("Enter", "submit"),
                ("Shift+Tab", "back"),
                ("Esc", "dismiss"),
            ]),
            Mode::Selecting if self.is_multi() => {
                hint_line(&[("Enter", "toggle"), ("Tab", "next"), ("Esc", "dismiss")])
            }
            Mode::Selecting => {
                hint_line(&[("Enter", "submit"), ("Tab", "next"), ("Esc", "dismiss")])
            }
        }
    }

    fn lines(&self, width: u16) -> Vec<Line<'static>> {
        match self.mode {
            Mode::Confirming => self.review_lines(),
            _ => self.selecting_lines(width),
        }
    }

    /// The first option's index in the list `selecting_lines` builds. Counted
    /// the same way that function assembles it.
    fn first_option_line(&self) -> u16 {
        let header = if self.has_review() { 2 } else { 0 };
        // Question text, the single/multiple hint, and a blank line.
        let preamble = self.question().question.lines().count() as u16 + 2;
        header + preamble
    }

    /// Which line the viewport has to keep on screen.
    fn focus_line(&self) -> u16 {
        if self.mode == Mode::Confirming {
            return 0;
        }
        self.first_option_line() + self.cursor as u16
    }

    fn tab_bar(&self) -> Line<'static> {
        let t = theme::current();
        let mut spans = Vec::new();
        for (index, question) in self.questions.iter().enumerate() {
            let label = tab_label(index, question);
            let answered = !self.answers[index].is_empty();
            spans.push(
                match (index == self.tab && self.mode != Mode::Confirming, answered) {
                    (true, _) => Span::styled(format!(" {label} "), t.active),
                    (false, true) => {
                        Span::styled(format!(" {label}{ANSWERED_MARK}"), t.todo_completed)
                    }
                    (false, false) => Span::styled(format!(" {label} "), t.tool_dim),
                },
            );
            spans.push(Span::styled(TAB_SEPARATOR, t.tool_dim));
        }
        spans.push(match self.mode {
            Mode::Confirming => Span::styled(REVIEW_TAB, t.active),
            _ => Span::styled(REVIEW_TAB, t.tool_dim),
        });
        Line::from(spans)
    }

    fn selecting_lines(&self, width: u16) -> Vec<Line<'static>> {
        let t = theme::current();
        let question = self.question();
        let mut lines = Vec::new();
        if self.has_review() {
            lines.push(self.tab_bar());
            lines.push(Line::default());
        }
        for line in question.question.lines() {
            lines.push(Line::from(Span::raw(format!(" {line}"))));
        }
        lines.push(Line::styled(
            if question.multiple {
                MULTI_HINT
            } else {
                SINGLE_HINT
            },
            t.tool_dim,
        ));
        lines.push(Line::default());
        for (index, option) in question.options.iter().enumerate() {
            lines.push(self.option_line(
                index == self.cursor,
                self.is_picked(&option.label),
                &option.label,
                Some(&option.description),
            ));
        }
        let custom = self.custom_answer().cloned();
        if self.mode == Mode::EditingCustom {
            lines.extend(self.custom_editor_lines(width));
        } else {
            lines.push(self.option_line(
                self.cursor == self.custom_row(),
                custom.is_some(),
                CUSTOM_OPTION,
                custom.as_deref(),
            ));
        }
        lines
    }

    fn option_line(
        &self,
        focused: bool,
        picked: bool,
        label: &str,
        description: Option<&str>,
    ) -> Line<'static> {
        let t = theme::current();
        let pointer = if focused { "▸ " } else { "  " };
        let mark = match (picked, self.is_multi()) {
            (true, true) => "✓ ",
            (true, false) => "● ",
            (false, true) => "  ",
            (false, false) => "○ ",
        };
        let mut spans = vec![
            Span::styled(pointer, t.tool_dim),
            Span::styled(
                mark,
                if picked {
                    t.todo_completed
                } else {
                    Style::default()
                },
            ),
            Span::styled(
                label.to_owned(),
                if focused { t.active } else { Style::default() },
            ),
        ];
        if let Some(description) = description.filter(|d| !d.is_empty()) {
            spans.push(Span::styled(DESC_SEPARATOR, t.tool_dim));
            spans.push(Span::styled(description.replace('\n', " "), t.tool_dim));
        }
        Line::from(spans)
    }

    fn custom_editor_lines(&self, width: u16) -> Vec<Line<'static>> {
        let t = theme::current();
        let indent = " ".repeat(CUSTOM_PROMPT.chars().count());
        let text = self.custom.value();
        let mut lines: Vec<Line<'static>> = text
            .split('\n')
            .enumerate()
            .map(|(index, line)| {
                Line::from(vec![
                    Span::styled(
                        if index == 0 {
                            CUSTOM_PROMPT.to_owned()
                        } else {
                            indent.clone()
                        },
                        t.active,
                    ),
                    Span::raw(line.to_owned()),
                ])
            })
            .collect();
        // An empty box still needs its prompt row, or the form silently loses
        // a line as the user clears what they typed.
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(CUSTOM_PROMPT, t.active)));
        }
        let _ = width;
        lines
    }

    fn review_lines(&self) -> Vec<Line<'static>> {
        let t = theme::current();
        let mut lines = vec![
            self.tab_bar(),
            Line::default(),
            Line::styled(REVIEW_HEADING, t.panel_title),
            Line::default(),
        ];
        for (index, question) in self.questions.iter().enumerate() {
            lines.push(Line::from(Span::raw(format!(
                " {}. {}",
                index + 1,
                question.question.replace('\n', " ")
            ))));
            let picked = &self.answers[index];
            let text = if picked.is_empty() {
                NO_ANSWER.to_owned()
            } else {
                picked.join(", ")
            };
            lines.push(Line::from(vec![
                Span::styled(ANSWER_ARROW, t.tool_dim),
                Span::styled(text, t.todo_completed),
            ]));
        }
        lines
    }
}

/// Where every line ends up once the paragraph has wrapped it. A long option
/// occupies more than one row, so the line the form counts in and the row the
/// terminal draws in are not the same number; scrolling and click targets both
/// need the second one.
struct VisualRows {
    starts: Vec<u16>,
    total: u16,
}

impl VisualRows {
    fn row_of(&self, line: u16) -> u16 {
        self.starts
            .get(line as usize)
            .copied()
            .unwrap_or(self.total)
    }

    fn height_of(&self, line: u16) -> u16 {
        self.row_of(line + 1).saturating_sub(self.row_of(line))
    }
}

/// Measured with the same widget that draws them, so the two can never
/// disagree about where a wrap falls.
fn visual_rows(lines: &[Line<'static>], width: u16) -> VisualRows {
    let width = width.max(1);
    let mut starts = Vec::with_capacity(lines.len());
    let mut total = 0;
    for line in lines {
        starts.push(total);
        total += Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(width) as u16;
    }
    VisualRows { starts, total }
}

const NEWLINE_MODIFIERS: KeyModifiers = KeyModifiers::ALT
    .union(KeyModifiers::SHIFT)
    .union(KeyModifiers::CONTROL);

/// A trailing backslash is the plain-terminal way to ask for a newline where
/// the modifier combination never reaches the process.
fn ends_with_backslash(buffer: &TextBuffer) -> bool {
    let value = buffer.value();
    buffer.cursor_offset() > 0 && value[..buffer.cursor_offset()].ends_with('\\')
}

/// The custom-answer box only needs what a one-line answer calls for; the
/// modal editors own the rest of `TextBuffer`.
fn edit(buffer: &mut TextBuffer, key: KeyEvent) {
    match key.code {
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            buffer.push_char(c);
        }
        KeyCode::Backspace => buffer.remove_char(),
        KeyCode::Delete => buffer.delete_char(),
        KeyCode::Left => buffer.move_left(),
        KeyCode::Right => buffer.move_right(),
        KeyCode::Up => buffer.move_up(),
        KeyCode::Down => buffer.move_down(),
        KeyCode::Home => buffer.move_home(),
        KeyCode::End => buffer.move_end(),
        _ => {}
    }
}

fn tab_label(index: usize, question: &AskedQuestion) -> String {
    if question.header.is_empty() {
        format!("Q{}", index + 1)
    } else {
        question.header.clone()
    }
}

impl Overlay for QuestionForm {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }

    fn is_modal(&self) -> bool {
        true
    }

    fn cadence(&self) -> Cadence {
        Cadence::IDLE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key as key_event;
    use caudra_agent::types::QuestionOption;

    const PICK: &str = "Pick one";
    const HEADER: &str = "Choice";
    const YES: &str = "Yes";
    const NO: &str = "No";
    const TYPED: &str = "something else";

    fn question(header: &str, multiple: bool) -> AskedQuestion {
        AskedQuestion {
            question: PICK.into(),
            header: header.into(),
            options: vec![
                QuestionOption {
                    label: YES.into(),
                    description: "affirmative".into(),
                },
                QuestionOption {
                    label: NO.into(),
                    description: "negative".into(),
                },
            ],
            multiple,
        }
    }

    fn opened(questions: Vec<AskedQuestion>) -> QuestionForm {
        let mut form = QuestionForm::new();
        form.open(questions);
        form
    }

    fn press(form: &mut QuestionForm, code: KeyCode) -> QuestionFormAction {
        form.handle_key(key_event(code))
    }

    fn type_text(form: &mut QuestionForm, text: &str) {
        for ch in text.chars() {
            form.handle_key(key_event(KeyCode::Char(ch)));
        }
    }

    const SCREEN: Rect = Rect {
        x: 0,
        y: 0,
        width: TERMINAL_WIDTH,
        height: TERMINAL_HEIGHT,
    };
    const TERMINAL_WIDTH: u16 = 80;
    const TERMINAL_HEIGHT: u16 = 24;

    fn render(form: &mut QuestionForm) -> Rect {
        let backend = ratatui::backend::TestBackend::new(TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut drawn = Rect::default();
        terminal
            .draw(|frame| drawn = form.view(frame, SCREEN))
            .unwrap();
        drawn
    }

    /// The screen with the form drawn over a full-width backdrop, so a test
    /// can tell an opaque surface from one the transcript shows through.
    fn painted(form: &mut QuestionForm) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let backdrop = BEHIND.to_string().repeat(TERMINAL_WIDTH as usize);
                let lines: Vec<Line<'static>> = (0..TERMINAL_HEIGHT)
                    .map(|_| Line::from(Span::raw(backdrop.clone())))
                    .collect();
                frame.render_widget(Paragraph::new(lines), SCREEN);
                form.view(frame, SCREEN);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..TERMINAL_HEIGHT)
            .map(|y| {
                (0..TERMINAL_WIDTH)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect()
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click(form: &mut QuestionForm, column: u16, row: u16) -> QuestionFormAction {
        form.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), column, row));
        form.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), column, row))
    }

    /// The middle of the row that option `index` was drawn on.
    fn option_row(form: &QuestionForm, index: usize) -> (u16, u16) {
        let hit = form.row_hits[index].area;
        (hit.x + 1, hit.y)
    }

    #[test]
    fn the_form_reports_where_it_drew() {
        let mut form = opened(vec![question(HEADER, false)]);
        let drawn = render(&mut form);
        assert!(drawn.height > 0);
        assert!(
            form.contains(Position::new(drawn.x, drawn.y)),
            "a wheel event over the form has to reach it"
        );
        assert!(
            !form.contains(Position::new(drawn.x, drawn.y.saturating_sub(1))),
            "and one above it must not"
        );
    }

    /// One character, so a single surviving cell is a failure. A word would
    /// let a border cover its first letter and hide the leak.
    const BEHIND: char = '\u{2591}';

    #[test]
    fn the_form_paints_over_what_is_behind_it() {
        let mut form = opened(vec![question(HEADER, false)]);
        let drawn = render(&mut form);
        let rows = painted(&mut form);
        for y in drawn.y..drawn.bottom() {
            assert!(
                !rows[y as usize].contains(BEHIND),
                "row {y} shows the transcript through the form: {:?}",
                rows[y as usize]
            );
        }
        assert!(
            rows[drawn.y.saturating_sub(1) as usize].contains(BEHIND),
            "the form must not erase more than it covers"
        );
    }

    #[test]
    fn the_hint_gets_its_own_row_and_leaves_the_border_whole() {
        let mut form = opened(vec![question(HEADER, false)]);
        let drawn = render(&mut form);
        let rows = painted(&mut form);
        let bottom = &rows[(drawn.bottom() - 1) as usize];
        assert!(
            bottom.starts_with(BORDER_BOTTOM_LEFT) && bottom.ends_with(BORDER_BOTTOM_RIGHT),
            "the hint must not be drawn over the bottom border: {bottom:?}"
        );
        assert!(
            rows[(drawn.bottom() - 2) as usize].contains(SUBMIT_HINT),
            "the hint belongs on the content row above it"
        );
    }

    const BORDER_BOTTOM_LEFT: &str = "\u{2570}";
    const BORDER_BOTTOM_RIGHT: &str = "\u{256f}";
    const SUBMIT_HINT: &str = "submit";

    #[test]
    fn clicking_an_option_picks_it() {
        let mut form = opened(vec![question(HEADER, false)]);
        render(&mut form);
        let (column, row) = option_row(&form, 1);
        let action = click(&mut form, column, row);
        assert!(
            matches!(&action, QuestionFormAction::Submit(picks) if picks == &[vec![NO.to_owned()]]),
            "a click does what Enter on the same row does"
        );
    }

    #[test]
    fn clicking_the_custom_row_opens_the_answer_box() {
        let mut form = opened(vec![question(HEADER, false)]);
        render(&mut form);
        let (column, row) = option_row(&form, form.custom_row());
        click(&mut form, column, row);
        assert_eq!(form.mode, Mode::EditingCustom);
    }

    #[test]
    fn clicking_a_multi_select_option_toggles_it() {
        let mut form = opened(vec![question(HEADER, true)]);
        render(&mut form);
        let (column, row) = option_row(&form, 0);
        click(&mut form, column, row);
        assert_eq!(form.picked(), [YES.to_owned()]);
        click(&mut form, column, row);
        assert!(form.picked().is_empty());
    }

    #[test]
    fn releasing_on_another_row_is_a_drag_not_a_click() {
        let mut form = opened(vec![question(HEADER, false)]);
        render(&mut form);
        let (column, first) = option_row(&form, 0);
        let (_, second) = option_row(&form, 1);
        form.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            column,
            first,
        ));
        let action =
            form.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), column, second));
        assert!(matches!(action, QuestionFormAction::Consumed));
    }

    #[test]
    fn clicking_the_chrome_picks_nothing() {
        let mut form = opened(vec![question(HEADER, false)]);
        let drawn = render(&mut form);
        let action = click(&mut form, drawn.x, drawn.y);
        assert!(matches!(action, QuestionFormAction::Consumed));
    }

    #[test]
    fn moving_over_a_row_moves_the_cursor() {
        let mut form = opened(vec![question(HEADER, false)]);
        render(&mut form);
        let (column, row) = option_row(&form, 1);
        form.handle_mouse(mouse(MouseEventKind::Moved, column, row));
        assert_eq!(form.cursor, 1);
    }

    #[test]
    fn a_wrapped_option_is_clickable_over_its_whole_height() {
        let long = "l".repeat(TERMINAL_WIDTH as usize * 2);
        let mut form = opened(vec![AskedQuestion {
            question: PICK.into(),
            header: HEADER.into(),
            options: vec![QuestionOption {
                label: long,
                description: String::new(),
            }],
            multiple: false,
        }]);
        render(&mut form);
        assert!(
            form.row_hits[0].area.height > 1,
            "a label past the edge wraps onto more than one row"
        );
        let hit = form.row_hits[0].area;
        let action = click(&mut form, hit.x, hit.bottom() - 1);
        assert!(
            matches!(action, QuestionFormAction::Submit(_)),
            "the second row of a wrapped option belongs to that option"
        );
    }

    #[test]
    fn an_option_scrolled_out_of_sight_is_not_clickable() {
        let options: Vec<QuestionOption> = (0..60)
            .map(|index| QuestionOption {
                label: format!("option {index}"),
                description: String::new(),
            })
            .collect();
        let count = options.len();
        let mut form = opened(vec![AskedQuestion {
            question: PICK.into(),
            header: HEADER.into(),
            options,
            multiple: false,
        }]);
        for _ in 0..count {
            press(&mut form, KeyCode::Down);
        }
        render(&mut form);
        assert!(
            form.row_hits.len() < count,
            "the list is taller than the form"
        );
        assert!(
            form.row_hits.iter().all(|hit| hit.index > 0),
            "the first option has scrolled off and must not be hit"
        );
    }

    #[test]
    fn a_lone_single_answer_question_submits_the_moment_it_is_picked() {
        let mut form = opened(vec![question(HEADER, false)]);
        let action = press(&mut form, KeyCode::Enter);
        assert!(
            matches!(&action, QuestionFormAction::Submit(picks) if picks == &[vec![YES.to_owned()]]),
            "no review step for a single question with a single answer"
        );
    }

    #[test]
    fn a_multi_select_question_toggles_instead_of_submitting() {
        let mut form = opened(vec![question(HEADER, true)]);
        press(&mut form, KeyCode::Enter);
        press(&mut form, KeyCode::Down);
        press(&mut form, KeyCode::Enter);
        assert_eq!(form.picked(), [YES.to_owned(), NO.to_owned()]);
    }

    #[test]
    fn picking_the_same_option_twice_unpicks_it() {
        let mut form = opened(vec![question(HEADER, true)]);
        press(&mut form, KeyCode::Enter);
        press(&mut form, KeyCode::Enter);
        assert!(form.picked().is_empty());
    }

    #[test]
    fn several_questions_walk_to_a_review_before_submitting() {
        let mut form = opened(vec![question(HEADER, false), question("Second", false)]);
        press(&mut form, KeyCode::Enter);
        assert_eq!(form.tab, 1, "picking the first answer moves to the second");
        press(&mut form, KeyCode::Down);
        press(&mut form, KeyCode::Enter);
        assert_eq!(form.mode, Mode::Confirming);
        let action = press(&mut form, KeyCode::Enter);
        assert!(matches!(&action, QuestionFormAction::Submit(picks)
                if picks == &[vec![YES.to_owned()], vec![NO.to_owned()]]),);
    }

    #[test]
    fn review_hands_back_to_the_last_question() {
        let mut form = opened(vec![question(HEADER, false), question("Second", false)]);
        press(&mut form, KeyCode::Enter);
        press(&mut form, KeyCode::Enter);
        assert_eq!(form.mode, Mode::Confirming);
        press(&mut form, KeyCode::BackTab);
        assert_eq!(form.mode, Mode::Selecting);
        assert_eq!(form.tab, 1);
    }

    #[test]
    fn a_skipped_question_submits_an_empty_answer() {
        let mut form = opened(vec![question(HEADER, false), question("Second", false)]);
        press(&mut form, KeyCode::Tab);
        press(&mut form, KeyCode::Tab);
        let action = press(&mut form, KeyCode::Enter);
        assert!(
            matches!(&action, QuestionFormAction::Submit(picks)
                if picks == &[Vec::<String>::new(), Vec::<String>::new()]),
            "every question keeps its slot even unanswered"
        );
    }

    #[test]
    fn a_typed_answer_is_kept_verbatim() {
        let mut form = opened(vec![question(HEADER, false)]);
        press(&mut form, KeyCode::Down);
        press(&mut form, KeyCode::Down);
        press(&mut form, KeyCode::Enter);
        assert_eq!(form.mode, Mode::EditingCustom);
        type_text(&mut form, TYPED);
        let action = press(&mut form, KeyCode::Enter);
        assert!(
            matches!(&action, QuestionFormAction::Submit(picks) if picks == &[vec![TYPED.to_owned()]]),
        );
    }

    #[test]
    fn reopening_the_editor_starts_from_what_was_typed_before() {
        let mut form = opened(vec![question(HEADER, true)]);
        press(&mut form, KeyCode::Down);
        press(&mut form, KeyCode::Down);
        press(&mut form, KeyCode::Enter);
        type_text(&mut form, TYPED);
        press(&mut form, KeyCode::Enter);

        press(&mut form, KeyCode::Enter);
        assert_eq!(form.custom.value(), TYPED, "the box is prefilled to edit");
    }

    #[test]
    fn clearing_the_box_withdraws_the_typed_answer() {
        let mut form = opened(vec![question(HEADER, true)]);
        press(&mut form, KeyCode::Down);
        press(&mut form, KeyCode::Down);
        press(&mut form, KeyCode::Enter);
        type_text(&mut form, TYPED);
        press(&mut form, KeyCode::Enter);
        assert_eq!(form.picked(), [TYPED.to_owned()]);

        press(&mut form, KeyCode::Enter);
        for _ in 0..TYPED.len() {
            press(&mut form, KeyCode::Backspace);
        }
        press(&mut form, KeyCode::Enter);
        assert!(form.picked().is_empty());
    }

    #[test]
    fn a_typed_answer_lives_alongside_the_picked_options() {
        let mut form = opened(vec![question(HEADER, true)]);
        press(&mut form, KeyCode::Enter);
        press(&mut form, KeyCode::Down);
        press(&mut form, KeyCode::Down);
        press(&mut form, KeyCode::Enter);
        type_text(&mut form, TYPED);
        press(&mut form, KeyCode::Enter);
        assert_eq!(form.picked(), [YES.to_owned(), TYPED.to_owned()]);
    }

    #[test]
    fn escape_out_of_the_editor_returns_to_the_list_rather_than_dismissing() {
        let mut form = opened(vec![question(HEADER, false)]);
        press(&mut form, KeyCode::Down);
        press(&mut form, KeyCode::Down);
        press(&mut form, KeyCode::Enter);
        let action = press(&mut form, KeyCode::Esc);
        assert!(matches!(action, QuestionFormAction::Consumed));
        assert_eq!(form.mode, Mode::Selecting);
    }

    #[test]
    fn escape_from_the_list_dismisses_the_whole_form() {
        let mut form = opened(vec![question(HEADER, false)]);
        assert!(matches!(
            press(&mut form, KeyCode::Esc),
            QuestionFormAction::Dismiss
        ));
    }

    #[test]
    fn the_cursor_stops_at_the_typed_answer_row() {
        let mut form = opened(vec![question(HEADER, false)]);
        for _ in 0..10 {
            press(&mut form, KeyCode::Down);
        }
        assert_eq!(form.cursor, form.custom_row(), "two options plus custom");
        for _ in 0..10 {
            press(&mut form, KeyCode::Up);
        }
        assert_eq!(form.cursor, 0);
    }

    #[test]
    fn a_question_with_no_options_still_offers_a_typed_answer() {
        let mut form = opened(vec![AskedQuestion {
            question: PICK.into(),
            header: HEADER.into(),
            options: Vec::new(),
            multiple: false,
        }]);
        assert_eq!(form.custom_row(), 0);
        press(&mut form, KeyCode::Enter);
        assert_eq!(form.mode, Mode::EditingCustom);
    }

    #[test]
    fn a_headerless_question_is_numbered_in_the_tab_bar() {
        let form = opened(vec![question("", false), question("", false)]);
        assert_eq!(tab_label(0, &form.questions[0]), "Q1");
        assert_eq!(tab_label(1, &form.questions[1]), "Q2");
    }
}
