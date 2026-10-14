//! The form behind the `question` tool and MCP elicitations: one tab per
//! question, an option list, and a free-text answer the user can type instead.
//! A server's text or number field opens the answer box directly and checks
//! what is typed before moving on, and a server's request to open an address
//! asks only for consent.
//!
//! The form holds only what the user has picked. Everything on screen is
//! derived from that plus the questions, so a resize or a theme switch can
//! never disagree with what will be submitted.

use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;
use url::Url;

use caudra_agent::mcp::elicitation::{REQUIRED, describe_answer, validate_answer};
use caudra_agent::permissions::AutopilotNote;
use caudra_agent::types::{AskedQuestion, QuestionDelivery, QuestionEvent, QuestionSource};
use caudra_grab::grab_scope;
use caudra_workbench::buffer::Cursor;
use caudra_workbench::render::Row;
use caudra_workbench::text_field::{FieldKind, FieldStyles, TextField, TextKey, is_newline_key};

use super::autopilot_panel::card_note;
use super::form::render_form;
use super::keybindings::{Bind, key};
use super::tab_bar::{Tab, tab_spans};
use super::{Hint, Overlay, VisualRows, field_styles, hanging_lines, visual_rows};
use crate::repaint::Cadence;
use crate::theme;

const TITLE: &str = " Question ";
const ASKS: &str = "asks";
const CUSTOM_OPTION: &str = "Type your own answer";
const MULTIPLE_ANSWERS: &str = "multiple answers";
const SINGLE_ANSWER: &str = "single answer";
const OPTIONAL_NOTE: &str = ", optional";
const KIND_INDENT: &str = "  ";
const ERROR_MARK: &str = " ✗ ";
const WARNING_MARK: &str = " ⚠ ";
const LOOKALIKE_WARNING: &str = "This address uses lookalike characters";
const LOOKALIKE_HOST: &str = ", it really goes to ";
const PUNYCODE_PREFIX: &str = "xn--";
const SCHEME_SEPARATOR: &str = "://";
const OPEN_IN_BROWSER: &str = "Open in browser";
const ACCEPT: &str = "Accept";
const DECLINE: &str = "Decline";
/// The consent rows: accepting, then declining.
const ACCEPT_ROW: usize = 0;
const DECLINE_ROW: usize = 1;
const NO_ANSWER: &str = "(no answer)";
const REVIEW_HEADING: &str = " Review your answers:";
/// A description sits under its label, past the pointer and the mark.
const DESC_INDENT: &str = "    ";
const ANSWER_ARROW: &str = "    → ";
const CUSTOM_PROMPT: &str = "  ❯ ";
/// Two borders, the blank row above the hint, and the hint row.
const CHROME_ROWS: u16 = 4;
const MAX_HEIGHT_PERCENT: u16 = 75;
/// The form wraps the answer box itself, so a painted line is never clipped.
const UNCLIPPED: usize = usize::MAX;
const LINE_BREAK: &str = "\n";
const ASK_HINT: Hint = Hint::bind(key::QUESTION_ASK_BTW, "Ask /btw");
const COMPACT_ASK_HINT: Hint = Hint::bind(key::QUESTION_ASK_BTW, "Ask");
const ASK_HINT_INDEX: usize = 0;
const DECLINE_HINT: Hint = Hint::bind(key::QUESTION_DECLINE, "decline");
const DEFER_HINT: Hint = Hint::bind(key::QUESTION_LATER, "later");
const NEXT_HINT: Hint = Hint::bind(key::QUESTION_LATER, "next");
const PUT_ASIDE_HINT: Hint = Hint::bind(key::ESC, "later");
const DISMISS_LABEL: &str = "dismiss";
const DEFERRED_TITLE: &str = "Deferred question";
/// Every bar ends on dismiss and cancel, and a server's form adds decline
/// right before them.
const CLOSING_HINTS: usize = 2;

const SHIFT_ENTER: Bind = Bind {
    code: KeyCode::Enter,
    modifiers: KeyModifiers::SHIFT,
    label: "Shift+Enter",
};

const EDITING_HINTS: [Hint; 5] = [
    ASK_HINT,
    Hint::bind(key::ENTER, "submit"),
    Hint::bind(SHIFT_ENTER, "newline"),
    Hint::bind(key::ESC, "back"),
    Hint::bind(key::QUIT, "cancel"),
];
/// A server's text or number field has no list to go back to, so `Esc`
/// dismisses it like every other question.
const TYPED_HINTS: [Hint; 5] = [
    ASK_HINT,
    Hint::bind(key::ENTER, "next"),
    Hint::bind(key::SHIFT_TAB, "back"),
    Hint::bind(key::ESC, "dismiss"),
    Hint::bind(key::QUIT, "cancel"),
];
const CONFIRMING_HINTS: [Hint; 5] = [
    ASK_HINT,
    Hint::bind(key::ENTER, "submit"),
    Hint::bind(key::SHIFT_TAB, "back"),
    Hint::bind(key::ESC, "dismiss"),
    Hint::bind(key::QUIT, "cancel"),
];
const MULTI_SELECT_HINTS: [Hint; 5] = [
    ASK_HINT,
    Hint::bind(key::ENTER, "toggle"),
    Hint::bind(key::TAB, "next"),
    Hint::bind(key::ESC, "dismiss"),
    Hint::bind(key::QUIT, "cancel"),
];
const SINGLE_SELECT_HINTS: [Hint; 5] = [
    ASK_HINT,
    Hint::bind(key::ENTER, "submit"),
    Hint::bind(key::TAB, "next"),
    Hint::bind(key::ESC, "dismiss"),
    Hint::bind(key::QUIT, "cancel"),
];
const CONSENT_HINTS: [Hint; 3] = [
    Hint::bind(key::ENTER, "select"),
    Hint::bind(key::ESC, "dismiss"),
    Hint::bind(key::QUIT, "cancel"),
];

pub enum QuestionFormAction {
    Consumed,
    AskBtw,
    /// One label list per question, in question order. An empty list is a
    /// question the user skipped.
    Submit(Vec<Vec<String>>),
    /// Consent to a server's request that asks for no answers: opening the
    /// address it gave, or a form with no fields.
    Accept,
    /// Turn a server's request down outright.
    Decline,
    /// Answer nothing and let the asking agent carry on.
    Dismiss,
    /// Stop the agent that asked, so only a new message moves it again.
    Cancel,
    /// `F4`: answer a waiting question later, or show the next deferred one.
    Later,
    /// `Esc` on a deferred question: hide the form and keep its draft.
    PutAside,
    /// Text the answer box copied or cut, for the clipboard.
    Copy(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Selecting,
    EditingCustom,
    Confirming,
}

pub struct QuestionForm {
    questions: Vec<AskedQuestion>,
    /// `None` for the agent's own `question` tool.
    source: Option<QuestionSource>,
    /// Why the answer on screen cannot be sent yet, until it changes.
    error: Option<String>,
    mode: Mode,
    tab: usize,
    cursor: usize,
    answers: Vec<Vec<String>>,
    custom: TextField,
    scroll: u16,
    /// Whether the viewport is tracking the cursor. A keystroke turns it back
    /// on, the wheel turns it off: a reader looking around a long option list
    /// must not be dragged back to the cursor on the next frame.
    follow_cursor: bool,
    /// Where the form last drew, so a wheel event can tell whether it landed
    /// on the form or on the transcript behind it.
    area: Rect,
    row_hits: Vec<RowHit>,
    /// What a left press landed on, so a release somewhere else is a drag
    /// rather than a click on whatever it ended up over.
    mouse_down: Option<FormTarget>,
    /// What the pointer is resting on, so a control can say it is about to
    /// act. Option rows are absent: hovering one moves the cursor, which
    /// already marks it, and reversing it too would say the same thing twice.
    hover: Option<FormTarget>,
    /// What autopilot is doing about the form, drawn muted above it.
    autopilot_note: Option<AutopilotNote>,
    /// The tool call that asked, which keys its draft and autopilot's claim.
    call_id: Option<String>,
    delivery: QuestionDelivery,
    /// Where a deferred form stands among the open ones, as `(n, of)`.
    place: Option<(usize, usize)>,
    /// The user acted on the form, so a new ask must not push it aside.
    touched: bool,
}

/// Everything on the form a click can name. The form is rebuilt from scratch
/// each frame, so a target is only ever read in the frame that recorded it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FormTarget {
    /// An offered option, or the custom-answer row one past the last of them.
    Option(usize),
    /// A question's tab in the bar.
    Tab(usize),
    /// The review tab at the end of the bar.
    Review,
    /// A question's summary in the review, which goes back to answer it again.
    ReviewRow(usize),
    /// A line of the custom-answer box, which takes the cursor to the cell.
    Editor(usize),
    /// A hint in the bottom bar, by its position in `hint_pairs`.
    Hint(usize),
}

#[derive(Clone, Copy)]
struct RowHit {
    area: Rect,
    target: FormTarget,
}

/// The body the form draws, and the logical-line span each clickable target
/// occupies in it. Built in one pass, so a hit rect can never sit on a line
/// other than the one that drew the target.
#[derive(Default)]
struct Body {
    lines: Vec<Line<'static>>,
    spans: Vec<TargetSpan>,
}

#[derive(Clone, Copy)]
struct TargetSpan {
    target: FormTarget,
    first: u16,
    count: u16,
}

impl Body {
    fn push(&mut self, target: FormTarget, lines: Vec<Line<'static>>) {
        self.spans.push(TargetSpan {
            target,
            first: self.lines.len() as u16,
            count: lines.len() as u16,
        });
        self.lines.extend(lines);
    }

    /// Where a target starts, or the top for one this mode never drew.
    fn first(&self, target: FormTarget) -> u16 {
        self.spans
            .iter()
            .find(|span| span.target == target)
            .map_or(0, |span| span.first)
    }
}

impl QuestionForm {
    pub fn new() -> Self {
        Self {
            questions: Vec::new(),
            source: None,
            error: None,
            mode: Mode::Selecting,
            tab: 0,
            cursor: 0,
            answers: Vec::new(),
            custom: TextField::new(FieldKind::Block),
            scroll: 0,
            follow_cursor: true,
            hover: None,
            area: Rect::default(),
            row_hits: Vec::new(),
            mouse_down: None,
            autopilot_note: None,
            call_id: None,
            delivery: QuestionDelivery::Blocking,
            place: None,
            touched: false,
        }
    }

    /// Opens on what an agent asked, carrying how the answer is waited for.
    pub(crate) fn ask(&mut self, event: QuestionEvent) {
        self.open(event.questions, event.source);
        self.call_id = event.call_id;
        self.delivery = event.delivery;
    }

    /// The same question asked again under a new delivery, as when the user
    /// deferred it: the draft stays.
    pub(crate) fn redeliver(&mut self, delivery: QuestionDelivery) {
        self.delivery = delivery;
        self.place = None;
    }

    pub(crate) fn call_id(&self) -> Option<&str> {
        self.call_id.as_deref()
    }

    /// The job a deferred form answers, `None` for one an agent waits on.
    pub(crate) fn deferred_task(&self) -> Option<&str> {
        match &self.delivery {
            QuestionDelivery::Deferred { task_id, .. } => Some(task_id),
            QuestionDelivery::Blocking | QuestionDelivery::Deferrable => None,
        }
    }

    pub(crate) fn set_place(&mut self, place: Option<(usize, usize)>) {
        self.place = place;
    }

    pub(crate) fn touch(&mut self) {
        self.touched = true;
    }

    pub(crate) fn touched(&self) -> bool {
        self.touched
    }

    /// Opens on the first question, with every default already picked. A
    /// server's request with no questions asks for consent alone.
    pub fn open(&mut self, questions: Vec<AskedQuestion>, source: Option<QuestionSource>) {
        self.autopilot_note = None;
        self.call_id = None;
        self.delivery = QuestionDelivery::Blocking;
        self.place = None;
        self.touched = false;
        self.answers = questions
            .iter()
            .map(|question| question.default.clone())
            .collect();
        self.questions = questions;
        self.source = source;
        self.mode = Mode::Selecting;
        self.custom.clear();
        self.scroll = 0;
        self.follow_cursor = true;
        self.row_hits.clear();
        self.mouse_down = None;
        self.hover = None;
        self.enter(0);
    }

    pub fn is_open(&self) -> bool {
        !self.questions.is_empty() || self.source.is_some()
    }

    pub(crate) fn text_input_active(&self) -> bool {
        self.is_open() && self.mode == Mode::EditingCustom
    }

    pub(crate) fn clarification_context(&self) -> String {
        self.questions
            .iter()
            .enumerate()
            .filter(|(index, _)| self.mode == Mode::Confirming || *index == self.tab)
            .map(|(index, question)| {
                let options = question
                    .options
                    .iter()
                    .map(|option| {
                        if option.description.is_empty() {
                            format!("- {}", option.label)
                        } else {
                            format!("- {}\n  {}", option.label, option.description)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(LINE_BREAK);
                format!(
                    "About: {}/{} · {}\n{}\n{}\n{}",
                    index + 1,
                    self.questions.len(),
                    tab_label(index, question),
                    question.question,
                    kind_hint(question).trim(),
                    options,
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub(crate) fn set_autopilot_note(&mut self, note: Option<AutopilotNote>) {
        self.autopilot_note = note;
    }

    pub(crate) fn autopilot_note(&self) -> Option<&AutopilotNote> {
        self.autopilot_note.as_ref()
    }

    pub fn close(&mut self) {
        self.autopilot_note = None;
        self.call_id = None;
        self.delivery = QuestionDelivery::Blocking;
        self.place = None;
        self.touched = false;
        self.questions.clear();
        self.source = None;
        self.error = None;
        self.answers.clear();
        self.row_hits.clear();
        self.mouse_down = None;
        self.hover = None;
        self.area = Rect::default();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> QuestionFormAction {
        if !self.is_open() {
            return QuestionFormAction::Consumed;
        }
        if key::QUESTION_ASK_BTW.matches(key) && !self.is_consent() {
            self.hover = None;
            self.mouse_down = None;
            return QuestionFormAction::AskBtw;
        }
        if key::QUESTION_DECLINE.matches(key) && self.source.is_some() {
            return QuestionFormAction::Decline;
        }
        if key::QUESTION_LATER.matches(key) && self.delivery != QuestionDelivery::Blocking {
            return QuestionFormAction::Later;
        }
        self.follow_cursor = true;
        if self.is_consent() {
            return self.key_consent(key);
        }
        match self.mode {
            Mode::Selecting => self.key_selecting(key),
            Mode::EditingCustom => self.key_editing(key),
            Mode::Confirming => self.key_confirming(key),
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.handle_text(text, true)
    }

    pub fn handle_text(&mut self, text: &str, paste: bool) -> bool {
        if self.mode != Mode::EditingCustom {
            return false;
        }
        if !paste {
            self.follow_cursor = true;
        }
        self.error = None;
        self.custom.handle_text(text, paste);
        true
    }

    pub fn contains(&self, pos: ratatui::layout::Position) -> bool {
        self.area.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll = super::apply_scroll_delta(self.scroll, delta);
        self.follow_cursor = false;
    }

    /// The rows the form wants, so the layout can reserve them before the
    /// form draws into them. The cap leaves the transcript on screen; the
    /// layout clamps this again against the room it actually has.
    pub fn height(&self, width: u16, available: u16) -> u16 {
        let body_width = width.saturating_sub(2);
        (visual_rows(&self.body(body_width, true).lines, body_width).total + CHROME_ROWS)
            .min(available * MAX_HEIGHT_PERCENT / 100)
            .max(CHROME_ROWS + 1)
    }

    #[cfg(test)]
    pub fn view(&mut self, frame: &mut Frame, area: Rect) {
        self.view_with_focus(frame, area, true);
    }

    pub(crate) fn view_with_focus(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        grab_scope!("question_form", area);
        let width = area.width.saturating_sub(2);
        let body = self.body(width, focused);
        let rows = visual_rows(&body.lines, width);
        self.area = area;
        // The cursor may sit below the fold on a long option list, so the
        // viewport follows it rather than staying where the user left it.
        let visible = area.height.saturating_sub(CHROME_ROWS);
        if self.follow_cursor {
            let focus = rows.row_of(body.first(self.focus_target()));
            self.scroll = self
                .scroll
                .min(focus)
                .max(focus.saturating_sub(visible.saturating_sub(1)));
        }
        self.scroll = self.scroll.min(rows.total.saturating_sub(visible));
        let t = theme::current();
        let title = match (&self.source, self.place) {
            (Some(source), _) => format!(" {} {ASKS} ", source.asker()),
            (None, Some((place, of))) => format!(" {DEFERRED_TITLE} {place} of {of} "),
            (None, None) => TITLE.to_owned(),
        };
        let hint = render_form(
            &t,
            &title,
            frame,
            area,
            body.lines,
            (self.scroll, 0),
            Some(self.hint(width)),
        );
        self.record_row_hits(&body.spans, &rows, area, visible, hint);
    }

    /// Where everything clickable landed on screen, so a click can name what
    /// it hit. Only what is inside the viewport is recorded: a row scrolled
    /// out of sight must not be clickable through whatever is drawn over it.
    fn record_row_hits(
        &mut self,
        spans: &[TargetSpan],
        rows: &VisualRows,
        form: Rect,
        visible: u16,
        hint: Rect,
    ) {
        self.row_hits.clear();
        self.push_tab_hits(rows, form, visible);
        for span in spans {
            for line in span.first..span.first + span.count {
                self.push_line_hit(rows, form, visible, line, span.target);
            }
        }
        let hints = super::hint_hits(&self.visible_hints(hint.width), hint);
        for (index, area) in hints.into_iter().enumerate() {
            self.row_hits.push(RowHit {
                area,
                target: FormTarget::Hint(index),
            });
        }
    }

    /// The block of rows one form line occupies, clipped to the viewport.
    fn push_line_hit(
        &mut self,
        rows: &VisualRows,
        form: Rect,
        visible: u16,
        line: u16,
        target: FormTarget,
    ) {
        let Some(offset) = rows.row_of(line).checked_sub(self.scroll) else {
            return;
        };
        let height = rows.height_of(line).min(visible.saturating_sub(offset));
        if height == 0 {
            return;
        }
        self.row_hits.push(RowHit {
            area: Rect {
                x: form.x + 1,
                y: form.y + 1 + offset,
                width: form.width.saturating_sub(2),
                height,
            },
            target,
        });
    }

    /// The tabs are spans on one line, so their hits are column ranges walked
    /// in step with the spans themselves. A bar wide enough to wrap is left
    /// alone: past the fold a column no longer maps to the span under it.
    fn push_tab_hits(&mut self, rows: &VisualRows, form: Rect, visible: u16) {
        let line = self.noted_body().lines.len() as u16;
        if !self.has_review() || rows.height_of(line) != 1 {
            return;
        }
        let Some(offset) = rows.row_of(line).checked_sub(self.scroll) else {
            return;
        };
        if offset >= visible {
            return;
        }
        let right = form.right().saturating_sub(1);
        let mut x = form.x + 1;
        for (span, target) in self.tab_spans() {
            let width = UnicodeWidthStr::width(span.content.as_ref()) as u16;
            if let Some(target) = target
                && x < right
            {
                self.row_hits.push(RowHit {
                    area: Rect {
                        x,
                        y: form.y + 1 + offset,
                        width: width.min(right - x),
                        height: 1,
                    },
                    target,
                });
            }
            x = x.saturating_add(width);
        }
    }

    /// `None` where the event missed everything the form drew. The form is
    /// not modal, so the transcript behind it acts on what it does not want.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> Option<QuestionFormAction> {
        let position = Position::new(event.column, event.row);
        let hit = self
            .row_hits
            .iter()
            .find(|hit| hit.area.contains(position))
            .copied();
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.mouse_down = hit.map(|hit| hit.target);
                let hit = hit?;
                if hit.target != FormTarget::Hint(ASK_HINT_INDEX) {
                    self.aim(hit, event);
                }
                Some(QuestionFormAction::Consumed)
            }
            // Hovering picks the row out, the same as walking onto it would.
            // Nothing else moves under the pointer alone.
            MouseEventKind::Moved => {
                self.hover = hit.map(|hit| hit.target);
                if let Some(FormTarget::Option(index)) = self.hover {
                    self.cursor = index;
                    self.follow_cursor = true;
                }
                hit.map(|_| QuestionFormAction::Consumed)
            }
            // A press and release on the same target is the click; anything
            // else is a drag that only moved the cursor.
            MouseEventKind::Up(MouseButton::Left) => {
                let pressed = self.mouse_down.take()?;
                if hit.map(|hit| hit.target) != Some(pressed) {
                    return Some(QuestionFormAction::Consumed);
                }
                // Hints are held by position and a mode change reshuffles
                // them, so the mark is dropped rather than left pointing at
                // whatever moved into its place.
                self.hover = None;
                Some(self.activate(pressed))
            }
            _ => None,
        }
    }

    /// What a press does before the release decides whether it was a click:
    /// move whatever cursor the target owns, so the form reads as pressed.
    fn aim(&mut self, hit: RowHit, event: MouseEvent) {
        self.follow_cursor = true;
        match hit.target {
            FormTarget::Option(index) => self.cursor = index,
            FormTarget::Editor(line) => {
                // The prefix is part of the line, so it occupies the first
                // cells of the wrapped content rather than a gutter beside it.
                let width = usize::from(hit.area.width.max(1));
                let wrap_row = usize::from(event.row.saturating_sub(hit.area.y));
                let column = usize::from(event.column.saturating_sub(hit.area.x));
                let cell = wrap_row * width + column;
                let col = cell.saturating_sub(CUSTOM_PROMPT.chars().count());
                self.custom.set_cursor(Cursor::new(line, col), false);
            }
            _ => {}
        }
    }

    fn activate(&mut self, target: FormTarget) -> QuestionFormAction {
        match target {
            FormTarget::Option(index) => {
                self.cursor = index;
                self.activate_cursor()
            }
            FormTarget::Tab(index) | FormTarget::ReviewRow(index) => {
                self.go_to(index);
                QuestionFormAction::Consumed
            }
            FormTarget::Review => {
                self.review();
                QuestionFormAction::Consumed
            }
            // The press already placed the cursor; a release must not move it
            // again or a click that drifted a cell would land somewhere else.
            FormTarget::Editor(_) => QuestionFormAction::Consumed,
            FormTarget::Hint(index) => self.activate_hint(index),
        }
    }

    /// A hint stands in for the key it advertises, so the two can never come
    /// to mean different things.
    fn activate_hint(&mut self, index: usize) -> QuestionFormAction {
        let Some(key) = self.hint_pairs().get(index).and_then(Hint::press) else {
            return QuestionFormAction::Consumed;
        };
        self.handle_key(key)
    }

    /// What `Enter` does on the focused row, shared with the click path so the
    /// two can never drift.
    fn activate_cursor(&mut self) -> QuestionFormAction {
        if self.is_consent() {
            return match self.cursor {
                ACCEPT_ROW => QuestionFormAction::Accept,
                _ => QuestionFormAction::Decline,
            };
        }
        self.error = None;
        let Some(label) = self
            .question()
            .options
            .get(self.cursor)
            .map(|option| option.label.clone())
        else {
            if self.offers_custom() {
                self.start_editing();
            }
            return QuestionFormAction::Consumed;
        };
        if self.is_multi() {
            self.toggle(label);
            return QuestionFormAction::Consumed;
        }
        // An optional answer picked again is taken back, which is how a
        // default the user does not want is left unanswered.
        if self.question().optional && self.picked() == [label.as_str()] {
            self.answers[self.tab].clear();
            return QuestionFormAction::Consumed;
        }
        self.answers[self.tab] = vec![label];
        self.advance()
    }

    fn question(&self) -> &AskedQuestion {
        &self.questions[self.tab]
    }

    /// A server's request with no questions, which only asks for consent:
    /// to open the address it gave, or to accept a form with no fields.
    fn is_consent(&self) -> bool {
        self.questions.is_empty() && self.source.is_some()
    }

    fn url(&self) -> Option<&str> {
        match &self.source {
            Some(QuestionSource::Mcp { url, .. }) => url.as_deref(),
            None => None,
        }
    }

    /// Only the agent's own questions take words of the user's choosing: a
    /// server checks every answer against the choices it listed.
    fn offers_custom(&self) -> bool {
        self.source.is_none()
    }

    /// The custom-answer row always sits one past the offered options, so a
    /// question with no options still has something to pick.
    fn custom_row(&self) -> usize {
        self.question().options.len()
    }

    fn last_row(&self) -> usize {
        match self.offers_custom() {
            true => self.custom_row(),
            false => self.custom_row().saturating_sub(1),
        }
    }

    fn is_multi(&self) -> bool {
        self.questions
            .get(self.tab)
            .is_some_and(|question| question.multiple)
    }

    fn is_typed(&self) -> bool {
        self.questions
            .get(self.tab)
            .is_some_and(|question| question.answer.is_typed())
    }

    /// A single question with one answer submits the moment it is picked;
    /// anything else needs a review step so the user can go back. A server's
    /// form is always reviewed before it is sent.
    fn has_review(&self) -> bool {
        (self.source.is_some() && !self.questions.is_empty())
            || self.questions.len() > 1
            || self.questions.first().is_some_and(|q| q.multiple)
    }

    /// Opens question `tab`. A typed one goes straight to its answer box,
    /// filled with the answer so far.
    fn enter(&mut self, tab: usize) {
        self.tab = tab;
        self.cursor = 0;
        self.error = None;
        if !self.is_typed() {
            self.mode = Mode::Selecting;
            return;
        }
        let answer = self.answers[tab].first().cloned().unwrap_or_default();
        self.custom.set_text(&answer);
        self.mode = Mode::EditingCustom;
    }

    /// Moves to question `tab`, keeping what was typed on the one left.
    fn go_to(&mut self, tab: usize) {
        self.keep_draft();
        self.enter(tab);
    }

    fn review(&mut self) {
        self.keep_draft();
        self.error = None;
        self.mode = Mode::Confirming;
    }

    /// A typed answer is kept as it stands when the user moves off it, checked
    /// or not: the review checks every answer again before anything is sent.
    fn keep_draft(&mut self) {
        if self.mode != Mode::EditingCustom || !self.is_typed() {
            return;
        }
        let text = self.custom.text().trim().to_owned();
        self.answers[self.tab] = match text.is_empty() {
            true => Vec::new(),
            false => vec![text],
        };
    }

    /// Why question `index` cannot be sent as answered. Only a server's form
    /// needs an answer to every question it did not mark optional.
    fn problem(&self, index: usize) -> Option<String> {
        let question = &self.questions[index];
        match self.answers[index].first() {
            None if self.source.is_some() && !question.optional => Some(REQUIRED.to_owned()),
            None => None,
            Some(answer) => validate_answer(&question.answer, answer).err(),
        }
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

    fn key_consent(&mut self, key: KeyEvent) -> QuestionFormAction {
        match key.code {
            KeyCode::Up => self.cursor = ACCEPT_ROW,
            KeyCode::Down => self.cursor = DECLINE_ROW,
            KeyCode::Enter => return self.activate_cursor(),
            KeyCode::Esc => return self.dismiss(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return QuestionFormAction::Cancel;
            }
            _ => {}
        }
        QuestionFormAction::Consumed
    }

    /// `Esc` lets the asking agent carry on without an answer, except on a
    /// deferred form: nothing waits for that one, so it is only put aside.
    fn dismiss(&self) -> QuestionFormAction {
        match self.delivery {
            QuestionDelivery::Deferred { .. } => QuestionFormAction::PutAside,
            QuestionDelivery::Blocking | QuestionDelivery::Deferrable => {
                QuestionFormAction::Dismiss
            }
        }
    }

    fn key_selecting(&mut self, key: KeyEvent) -> QuestionFormAction {
        match key.code {
            KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down => self.cursor = (self.cursor + 1).min(self.last_row()),
            KeyCode::Enter => return self.activate_cursor(),
            KeyCode::Tab | KeyCode::Right if self.has_review() => return self.next_tab(),
            KeyCode::BackTab | KeyCode::Left if self.has_review() && self.tab > 0 => {
                self.go_to(self.tab - 1);
            }
            KeyCode::Esc => return self.dismiss(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return QuestionFormAction::Cancel;
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
        self.custom.set_text(&existing);
        self.mode = Mode::EditingCustom;
    }

    /// The box sees every key before the form, so `Ctrl+C` copies a selection
    /// and only cancels when there is nothing to copy.
    fn key_editing(&mut self, key: KeyEvent) -> QuestionFormAction {
        let submit = key.code == KeyCode::Enter && !is_newline_key(key);
        let typed = self.is_typed();
        match key.code {
            KeyCode::Enter if submit && typed => return self.commit_typed(),
            KeyCode::Esc if typed => return self.dismiss(),
            KeyCode::Tab if typed => return self.next_tab(),
            KeyCode::BackTab if typed => {
                if self.tab > 0 {
                    self.go_to(self.tab - 1);
                }
            }
            // A trailing backslash is the plain-terminal way to ask for a
            // newline where the modifier combination never arrives. The break
            // is typed over the backslash, so one undo brings it back.
            KeyCode::Enter if submit && ends_with_backslash(&self.custom) => {
                let caret = self.custom.cursor();
                self.custom
                    .set_cursor(Cursor::new(caret.line, caret.col - 1), false);
                self.custom.set_cursor(caret, true);
                self.custom.insert_text(LINE_BREAK);
            }
            KeyCode::Enter if submit => return self.commit_custom(),
            KeyCode::Esc => self.mode = Mode::Selecting,
            _ => {
                let outcome = self.custom.handle_key(key);
                if outcome.changed() {
                    self.error = None;
                }
                match outcome {
                    TextKey::Copy(text) | TextKey::Cut(text) => {
                        return QuestionFormAction::Copy(text);
                    }
                    TextKey::Ignored if key::QUIT.matches(key) => {
                        return QuestionFormAction::Cancel;
                    }
                    _ => {}
                }
            }
        }
        QuestionFormAction::Consumed
    }

    /// A typed answer moves on only once it is one the server takes; until
    /// then the reason sits under the box and the box keeps the text.
    fn commit_typed(&mut self) -> QuestionFormAction {
        self.keep_draft();
        self.error = self.problem(self.tab);
        if self.error.is_some() {
            return QuestionFormAction::Consumed;
        }
        self.advance()
    }

    fn commit_custom(&mut self) -> QuestionFormAction {
        let text = self.custom.text().trim().to_owned();
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
            KeyCode::Enter => self.submit(),
            KeyCode::BackTab | KeyCode::Left => {
                self.go_to(self.questions.len() - 1);
                QuestionFormAction::Consumed
            }
            KeyCode::Esc => self.dismiss(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                QuestionFormAction::Cancel
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
        self.submit()
    }

    fn next_tab(&mut self) -> QuestionFormAction {
        if self.tab + 1 < self.questions.len() {
            self.go_to(self.tab + 1);
        } else {
            self.review();
        }
        QuestionFormAction::Consumed
    }

    /// Sends the answers, unless one of them cannot go: then the form opens
    /// on the first such question with the reason under it.
    fn submit(&mut self) -> QuestionFormAction {
        let blocked =
            (0..self.questions.len()).find_map(|index| Some((index, self.problem(index)?)));
        if let Some((index, problem)) = blocked {
            self.enter(index);
            self.error = Some(problem);
            return QuestionFormAction::Consumed;
        }
        QuestionFormAction::Submit(std::mem::take(&mut self.answers))
    }

    /// The hint bar's contents, which a click on one of them turns back into
    /// the key it names.
    fn hint_pairs(&self) -> Vec<Hint> {
        let pairs: &[Hint] = match self.mode {
            _ if self.is_consent() => &CONSENT_HINTS,
            Mode::EditingCustom if self.is_typed() => &TYPED_HINTS,
            Mode::EditingCustom => &EDITING_HINTS,
            Mode::Confirming => &CONFIRMING_HINTS,
            Mode::Selecting if self.is_multi() => &MULTI_SELECT_HINTS,
            Mode::Selecting => &SINGLE_SELECT_HINTS,
        };
        let mut hints = pairs.to_vec();
        if self.source.is_some() {
            hints.insert(hints.len() - CLOSING_HINTS, DECLINE_HINT);
        }
        match self.delivery {
            QuestionDelivery::Blocking => {}
            QuestionDelivery::Deferrable => hints.insert(hints.len() - CLOSING_HINTS, DEFER_HINT),
            QuestionDelivery::Deferred { .. } => {
                for hint in &mut hints {
                    if hint.description == DISMISS_LABEL {
                        *hint = PUT_ASIDE_HINT;
                    }
                }
                if self.place.is_some_and(|(_, of)| of > 1) {
                    hints.insert(hints.len() - CLOSING_HINTS, NEXT_HINT);
                }
            }
        }
        hints
    }

    fn visible_hints(&self, width: u16) -> Vec<Hint> {
        let mut hints = self.hint_pairs();
        let area = Rect::new(0, 0, width, 1);
        if !self.is_consent() && super::hint_hits(&hints[..1], area).is_empty() {
            hints[ASK_HINT_INDEX] = COMPACT_ASK_HINT;
        }
        hints.truncate(super::hint_hits(&hints, area).len());
        hints
    }

    fn hint(&self, width: u16) -> Line<'static> {
        let hovered = match self.hover {
            Some(FormTarget::Hint(index)) => Some(index),
            _ => None,
        };
        super::hint_line_hovered(&self.visible_hints(width), hovered)
    }

    fn body(&self, width: u16, focused: bool) -> Body {
        match self.mode {
            _ if self.is_consent() => self.consent_body(),
            Mode::Confirming => self.review_body(width),
            _ => self.selecting_body(width, focused),
        }
    }

    /// What the viewport has to keep on screen. The review draws neither, so
    /// it falls back to the top of the body.
    fn focus_target(&self) -> FormTarget {
        match self.mode {
            Mode::EditingCustom => FormTarget::Editor(0),
            _ => FormTarget::Option(self.cursor),
        }
    }

    /// The tab bar's spans paired with what each one selects. Drawing and hit
    /// testing both read this, so a tab can never sit somewhere other than
    /// where a click looks for it. Separators select nothing.
    fn tab_spans(&self) -> Vec<(Span<'static>, Option<FormTarget>)> {
        let tabs: Vec<_> = self
            .questions
            .iter()
            .enumerate()
            .map(|(index, question)| Tab {
                label: tab_label(index, question),
                done: !self.answers[index].is_empty(),
                target: FormTarget::Tab(index),
            })
            .collect();
        tab_spans(
            &tabs,
            (self.mode != Mode::Confirming).then_some(self.tab),
            FormTarget::Review,
            self.hover.as_ref(),
            u16::MAX,
        )
    }

    fn tab_bar(&self) -> Line<'static> {
        Line::from(
            self.tab_spans()
                .into_iter()
                .map(|(span, _)| span)
                .collect::<Vec<_>>(),
        )
    }

    /// A body that opens with autopilot's note and the server's message, when
    /// the form carries them. Everything below starts on the line after.
    fn noted_body(&self) -> Body {
        let mut body = Body::default();
        if let Some(note) = &self.autopilot_note {
            body.lines.extend([
                Line::styled(format!(" {}", card_note(note)), theme::current().tool_dim),
                Line::default(),
            ]);
        }
        if let Some(QuestionSource::Mcp { message, .. }) = &self.source
            && !message.is_empty()
        {
            body.lines
                .extend(message.lines().map(|line| Line::raw(format!(" {line}"))));
            body.lines.push(Line::default());
        }
        body
    }

    fn selecting_body(&self, width: u16, focused: bool) -> Body {
        let t = theme::current();
        let question = self.question();
        let mut body = self.noted_body();
        if self.has_review() {
            body.lines.push(self.tab_bar());
            body.lines.push(Line::default());
        }
        for line in question.question.lines() {
            body.lines.push(Line::from(Span::raw(format!(" {line}"))));
        }
        body.lines
            .push(Line::styled(kind_hint(question), t.tool_dim));
        body.lines.push(Line::default());
        for index in 0..question.options.len() {
            body.push(FormTarget::Option(index), self.option_lines(index, width));
        }
        if self.mode == Mode::EditingCustom {
            for (index, line) in self.custom_editor_lines(focused).into_iter().enumerate() {
                body.push(FormTarget::Editor(index), vec![line]);
            }
        } else if self.offers_custom() {
            let custom = self.custom_row();
            body.push(FormTarget::Option(custom), self.option_lines(custom, width));
        }
        if let Some(error) = &self.error {
            body.lines.push(marked_line(ERROR_MARK, error, t.error));
        }
        body
    }

    /// A request that asks only for consent: the server's message, the
    /// address it wants opened if any, and the two answers.
    fn consent_body(&self) -> Body {
        let t = theme::current();
        let mut body = self.noted_body();
        let accept = match self.url() {
            Some(url) => {
                body.lines.push(url_line(url, t.accent));
                if let Some(host) = lookalike_host(url) {
                    let warning = format!("{LOOKALIKE_WARNING}{LOOKALIKE_HOST}{host}");
                    body.lines
                        .push(marked_line(WARNING_MARK, &warning, t.tool_warning));
                }
                body.lines.push(Line::default());
                OPEN_IN_BROWSER
            }
            None => ACCEPT,
        };
        for (row, label) in [(ACCEPT_ROW, accept), (DECLINE_ROW, DECLINE)] {
            let line = self.option_line(row == self.cursor, false, label);
            body.push(FormTarget::Option(row), vec![line]);
        }
        body
    }

    /// The label an option offers. The row past the last of them is the one
    /// the user types their own answer on.
    fn option_label(&self, index: usize) -> &str {
        self.question()
            .options
            .get(index)
            .map_or(CUSTOM_OPTION, |option| &option.label)
    }

    /// What is written under a label: the offered description, or the answer
    /// the user typed on the custom row.
    fn option_description(&self, index: usize) -> Option<&str> {
        let described = match self.question().options.get(index) {
            Some(option) => option.description.as_str(),
            None => self.custom_answer().map_or("", String::as_str),
        };
        (!described.is_empty()).then_some(described)
    }

    /// An option's label, and its description on the lines below so a long
    /// one never crowds the labels the user is choosing between. A wrapped
    /// description hangs under the first row of itself, not under the pointer.
    fn option_lines(&self, index: usize, width: u16) -> Vec<Line<'static>> {
        let t = theme::current();
        let picked = match self.question().options.get(index) {
            Some(option) => self.is_picked(&option.label),
            None => self.custom_answer().is_some(),
        };
        let mut lines =
            vec![self.option_line(index == self.cursor, picked, self.option_label(index))];
        if let Some(description) = self.option_description(index) {
            lines.extend(hanging_lines(
                Span::styled(DESC_INDENT, t.tool_dim),
                Span::styled(description.replace('\n', " "), t.tool_dim),
                width,
            ));
        }
        lines
    }

    fn option_line(&self, focused: bool, picked: bool, label: &str) -> Line<'static> {
        let t = theme::current();
        let pointer = if focused { "▸ " } else { "  " };
        let mark = match (picked, self.is_multi()) {
            (true, true) => "✓ ",
            (true, false) => "● ",
            (false, true) => "  ",
            (false, false) => "○ ",
        };
        let spans = vec![
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
        Line::from(spans)
    }

    /// The answer box, caret and all. A `TextField` always keeps a line, so
    /// an emptied box still draws its prompt row rather than losing a line as
    /// the user clears what they typed.
    fn custom_editor_lines(&self, focused: bool) -> Vec<Line<'static>> {
        let t = theme::current();
        let styles = field_styles(Style::default());
        let indent = " ".repeat(CUSTOM_PROMPT.chars().count());
        (0..self.custom.lines().len())
            .map(|index| {
                let mut spans = vec![Span::styled(
                    if index == 0 {
                        CUSTOM_PROMPT.to_owned()
                    } else {
                        indent.clone()
                    },
                    t.active,
                )];
                spans.extend(caret_spans(&self.custom, index, &styles, focused));
                Line::from(spans)
            })
            .collect()
    }

    fn review_body(&self, width: u16) -> Body {
        let t = theme::current();
        let mut body = self.noted_body();
        body.lines.extend([
            self.tab_bar(),
            Line::default(),
            Line::styled(REVIEW_HEADING, t.panel_title),
            Line::default(),
        ]);
        for (index, question) in self.questions.iter().enumerate() {
            let on = self.hover == Some(FormTarget::ReviewRow(index));
            let numbered = super::hover_style(Style::default(), on);
            let mut lines = hanging_lines(
                Span::styled(format!(" {}. ", index + 1), numbered),
                Span::styled(question.question.replace('\n', " "), numbered),
                width,
            );
            let picked = &self.answers[index];
            let text = if picked.is_empty() {
                NO_ANSWER.to_owned()
            } else {
                picked.join(", ")
            };
            lines.extend(hanging_lines(
                Span::styled(ANSWER_ARROW, super::hover_style(t.tool_dim, on)),
                Span::styled(text, super::hover_style(t.todo_completed, on)),
                width,
            ));
            if let Some(problem) = self.problem(index) {
                lines.push(marked_line(ERROR_MARK, &problem, t.error));
            }
            body.push(FormTarget::ReviewRow(index), lines);
        }
        body
    }
}

/// A trailing backslash is the plain-terminal way to ask for a newline where
/// the modifier combination never reaches the process.
fn ends_with_backslash(field: &TextField) -> bool {
    let caret = field.cursor();
    caret
        .col
        .checked_sub(1)
        .and_then(|index| field.lines()[caret.line].chars().nth(index))
        == Some('\\')
}

/// Line `index` of the box with its selection and caret painted on. Nothing
/// places a terminal cursor over the form, so the caret is painted like the
/// composer's.
fn caret_spans(
    field: &TextField,
    index: usize,
    styles: &FieldStyles,
    focused: bool,
) -> Vec<Span<'static>> {
    let overlays = field.overlays(index, styles, focused);
    Row {
        text: &field.lines()[index],
        segments: None,
        base: styles.text,
        fill: None,
        overlays: &overlays,
    }
    .paint(0, UNCLIPPED)
    .spans
}

fn tab_label(index: usize, question: &AskedQuestion) -> String {
    if question.header.is_empty() {
        format!("Q{}", index + 1)
    } else {
        question.header.clone()
    }
}

/// What a question takes, under it: how many choices, or what to type and
/// within which limits, and whether it may be left empty.
fn kind_hint(question: &AskedQuestion) -> String {
    let kind = describe_answer(&question.answer).unwrap_or_else(|| {
        match question.multiple {
            true => MULTIPLE_ANSWERS,
            false => SINGLE_ANSWER,
        }
        .to_owned()
    });
    let optional = if question.optional { OPTIONAL_NOTE } else { "" };
    format!("{KIND_INDENT}({kind}{optional})")
}

fn marked_line(mark: &'static str, text: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(mark, style),
        Span::styled(text.to_owned(), style),
    ])
}

/// The whole address, with the host picked out so the reader sees where it
/// really goes rather than whatever the path claims.
fn url_line(url: &str, host_style: Style) -> Line<'static> {
    let Some(host) = host_range(url) else {
        return Line::raw(format!(" {url}"));
    };
    Line::from(vec![
        Span::raw(format!(" {}", &url[..host.start])),
        Span::styled(url[host.clone()].to_owned(), host_style),
        Span::raw(url[host.end..].to_owned()),
    ])
}

/// Where the host sits in `url` as written: past the scheme and any user
/// name, before the port and the path.
fn host_range(url: &str) -> Option<Range<usize>> {
    let start = url.find(SCHEME_SEPARATOR)? + SCHEME_SEPARATOR.len();
    let end = url[start..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |offset| start + offset);
    let host_start = url[start..end]
        .rfind('@')
        .map_or(start, |at| start + at + 1);
    let authority = &url[host_start..end];
    let host_end = match authority.rfind(':') {
        Some(colon) if !authority[colon..].contains(']') => host_start + colon,
        _ => end,
    };
    (host_start < host_end).then_some(host_start..host_end)
}

/// The host `url` really reaches, when what is written could pass for some
/// other one: letters outside ASCII, or a label already in punycode.
fn lookalike_host(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let written = host_range(url).map_or(host, |range| &url[range]);
    let lookalike = !written.is_ascii()
        || host
            .split('.')
            .any(|label| label.starts_with(PUNYCODE_PREFIX));
    lookalike.then(|| host.to_owned())
}

impl Overlay for QuestionForm {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }

    /// The form is docked under the transcript rather than drawn over it, so
    /// scrolling, selecting and hovering the chat behind it stay live while
    /// it waits for an answer.
    fn is_modal(&self) -> bool {
        false
    }

    fn cadence(&self) -> Cadence {
        Cadence::IDLE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key as key_event;
    use caudra_agent::mcp::elicitation::{
        ABOVE_MAXIMUM, ElicitForm, NO_LABEL, NOT_A_WHOLE_NUMBER, YES_LABEL,
    };
    use caudra_agent::types::{AnswerKind, QuestionOption};

    const PICK: &str = "Pick one";
    const HEADER: &str = "Choice";
    const YES: &str = "Yes";
    const YES_DESC: &str = "affirmative";
    const NO: &str = "No";
    const NO_DESC: &str = "negative";
    const TYPED: &str = "something else";

    fn question(header: &str, multiple: bool) -> AskedQuestion {
        AskedQuestion {
            question: PICK.into(),
            header: header.into(),
            options: vec![
                QuestionOption {
                    label: YES.into(),
                    description: YES_DESC.into(),
                },
                QuestionOption {
                    label: NO.into(),
                    description: NO_DESC.into(),
                },
            ],
            multiple,
            ..AskedQuestion::default()
        }
    }

    fn opened(questions: Vec<AskedQuestion>) -> QuestionForm {
        let mut form = QuestionForm::new();
        form.open(questions, None);
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

    fn chord(
        form: &mut QuestionForm,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> QuestionFormAction {
        form.handle_key(KeyEvent::new(code, modifiers))
    }

    /// A form sitting in the custom-answer box with `TYPED` already in it.
    fn typing(multiple: bool) -> QuestionForm {
        let mut form = opened(vec![question(HEADER, multiple)]);
        for _ in 0..=form.custom_row() {
            press(&mut form, KeyCode::Down);
        }
        press(&mut form, KeyCode::Enter);
        type_text(&mut form, TYPED);
        form
    }

    const TERMINAL_WIDTH: u16 = 80;
    const TERMINAL_HEIGHT: u16 = 24;

    /// Where the layout would dock the form: the rows it asks for, at the
    /// bottom of the screen.
    fn docked(form: &QuestionForm) -> Rect {
        let height = form.height(TERMINAL_WIDTH, TERMINAL_HEIGHT);
        Rect {
            x: 0,
            y: TERMINAL_HEIGHT - height,
            width: TERMINAL_WIDTH,
            height,
        }
    }

    fn render(form: &mut QuestionForm) -> Rect {
        let backend = ratatui::backend::TestBackend::new(TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let area = docked(form);
        terminal.draw(|frame| form.view(frame, area)).unwrap();
        area
    }

    /// The screen the form drew on, row by row.
    fn painted(form: &mut QuestionForm) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let area = docked(form);
        terminal.draw(|frame| form.view(frame, area)).unwrap();
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

    /// A press and release on one cell. An event the form passes through is
    /// one that did nothing to it, which is what `Consumed` stands for here.
    fn click(form: &mut QuestionForm, column: u16, row: u16) -> QuestionFormAction {
        form.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), column, row));
        form.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), column, row))
            .unwrap_or(QuestionFormAction::Consumed)
    }

    fn hit_area(form: &QuestionForm, target: FormTarget) -> Rect {
        form.row_hits
            .iter()
            .find(|hit| hit.target == target)
            .unwrap_or_else(|| panic!("{target:?} was not drawn"))
            .area
    }

    /// A cell inside the row `target` was drawn on.
    fn target_cell(form: &QuestionForm, target: FormTarget) -> (u16, u16) {
        let hit = hit_area(form, target);
        (hit.x + 1, hit.y)
    }

    fn option_row(form: &QuestionForm, index: usize) -> (u16, u16) {
        target_cell(form, FormTarget::Option(index))
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

    /// The form is docked, so the rows it asks for are the rows it draws in:
    /// a mismatch would leave a gap the transcript no longer owns.
    #[test]
    fn the_form_fills_the_rows_it_asked_for() {
        let mut form = opened(vec![question(HEADER, false)]);
        let drawn = render(&mut form);
        assert_eq!(drawn.height, form.height(TERMINAL_WIDTH, TERMINAL_HEIGHT));
        let rows = painted(&mut form);
        assert!(
            rows[drawn.y as usize].starts_with(BORDER_TOP_LEFT),
            "the form starts on the first row it reserved: {:?}",
            rows[drawn.y as usize]
        );
    }

    /// A list longer than the screen must not push the transcript off it.
    #[test]
    fn the_height_is_capped_and_floored() {
        let capped = opened(vec![long_question()]).height(TERMINAL_WIDTH, TERMINAL_HEIGHT);
        assert_eq!(capped, TERMINAL_HEIGHT * MAX_HEIGHT_PERCENT / 100);
        let floored = opened(vec![question(HEADER, false)]).height(TERMINAL_WIDTH, CHROME_ROWS);
        assert_eq!(floored, CHROME_ROWS + 1, "the chrome always has a body row");
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

    const BORDER_TOP_LEFT: &str = "\u{256d}";
    const BORDER_BOTTOM_LEFT: &str = "\u{2570}";
    const BORDER_BOTTOM_RIGHT: &str = "\u{256f}";
    const SUBMIT_HINT: &str = "submit";

    /// The row between the last answer and the legend, so the two do not read
    /// as one block.
    #[test]
    fn the_hint_has_a_blank_row_above_it() {
        let mut form = opened(vec![long_question()]);
        for _ in 0..=form.custom_row() {
            press(&mut form, KeyCode::Down);
        }
        let drawn = render(&mut form);
        let rows = painted(&mut form);
        let spacer = &rows[(drawn.bottom() - 3) as usize];
        let inside: String = spacer
            .chars()
            .skip(drawn.x as usize + 1)
            .take(drawn.width as usize - 2)
            .collect();
        assert!(
            inside.trim().is_empty(),
            "the legend needs a blank row above it: {inside:?}"
        );
    }

    /// The body is confined to its own rows, so a list longer than the form
    /// scrolls under the legend instead of printing next to it.
    #[test]
    fn content_never_reaches_the_hint_row() {
        let mut form = opened(vec![long_question()]);
        let drawn = render(&mut form);
        let rows = painted(&mut form);
        for y in [drawn.bottom() - 3, drawn.bottom() - 2] {
            assert!(
                !rows[y as usize].contains(OPTION_LABEL),
                "row {y} carries body text into the footer: {:?}",
                rows[y as usize]
            );
        }
    }

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

    /// A description belongs under its label, not trailing off the end of it,
    /// so the labels stay in one column the eye can run down.
    #[test]
    fn a_description_is_drawn_under_its_label() {
        let mut form = opened(vec![question(HEADER, false)]);
        render(&mut form);
        let (_, row) = option_row(&form, 0);
        let rows = painted(&mut form);
        assert!(
            rows[row as usize].contains(YES) && !rows[row as usize].contains(YES_DESC),
            "the label row holds the label alone: {:?}",
            rows[row as usize]
        );
        assert!(
            rows[row as usize + 1].contains(YES_DESC),
            "the description belongs on the row below: {:?}",
            rows[row as usize + 1]
        );
    }

    #[test]
    fn clicking_a_description_picks_its_option() {
        let mut form = opened(vec![question(HEADER, false)]);
        render(&mut form);
        let description = form
            .row_hits
            .iter()
            .filter(|hit| hit.target == FormTarget::Option(1))
            .nth(1)
            .expect("a described option owns its description row")
            .area;
        let action = click(&mut form, description.x + 1, description.y);
        assert!(
            matches!(&action, QuestionFormAction::Submit(picks) if picks == &[vec![NO.to_owned()]]),
            "a description belongs to the option above it"
        );
    }

    /// A description that wraps has to read as one block under its label.
    /// `Wrap` alone restarts every continuation row in column zero.
    #[test]
    fn a_wrapped_description_hangs_under_the_first_row_of_itself() {
        let mut form = opened(vec![wordy_question()]);
        let drawn = render(&mut form);
        let hits: Vec<Rect> = form
            .row_hits
            .iter()
            .filter(|hit| hit.target == FormTarget::Option(0))
            .map(|hit| hit.area)
            .collect();
        assert!(hits.len() > 2, "the description wrapped onto more rows");
        let rows = painted(&mut form);
        // The first cell inside the border, where a row that lost its indent
        // would start instead.
        let inside = drawn.x as usize + 1;
        for hit in &hits[1..] {
            let row: Vec<char> = rows[hit.y as usize].chars().collect();
            let (indent, text) = row[inside..].split_at(DESC_INDENT.len());
            assert!(
                indent.iter().all(|cell| *cell == ' ') && text[0] != ' ',
                "row {} starts somewhere other than the indent: {:?}",
                hit.y,
                rows[hit.y as usize]
            );
        }
    }

    /// Every row of a description belongs to the option it describes, so a
    /// click on the last of them picks the same thing as a click on the label.
    #[test]
    fn clicking_the_last_row_of_a_wrapped_description_picks_its_option() {
        let mut form = opened(vec![wordy_question()]);
        render(&mut form);
        let last = form
            .row_hits
            .iter()
            .rfind(|hit| hit.target == FormTarget::Option(0))
            .expect("the option was drawn")
            .area;
        let action = click(&mut form, last.x + 1, last.y);
        assert!(
            matches!(&action, QuestionFormAction::Submit(picks) if picks == &[vec![YES.to_owned()]]),
        );
    }

    /// One option whose description is far wider than the form.
    fn wordy_question() -> AskedQuestion {
        AskedQuestion {
            question: PICK.into(),
            header: HEADER.into(),
            options: vec![QuestionOption {
                label: YES.into(),
                description: [YES_DESC; 20].join(" "),
            }],
            multiple: false,
            ..AskedQuestion::default()
        }
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
        assert!(matches!(action, Some(QuestionFormAction::Consumed)));
    }

    /// The border belongs to no control, and the form is not modal, so the
    /// event has to reach whatever is behind it rather than being swallowed.
    #[test]
    fn clicking_the_chrome_passes_the_event_through() {
        let mut form = opened(vec![question(HEADER, false)]);
        let drawn = render(&mut form);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Moved,
        ] {
            assert!(
                form.handle_mouse(mouse(kind, drawn.x, drawn.y)).is_none(),
                "{kind:?} on the border is not the form's to take"
            );
        }
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
            ..AskedQuestion::default()
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
            ..AskedQuestion::default()
        }]);
        for _ in 0..count {
            press(&mut form, KeyCode::Down);
        }
        render(&mut form);
        let options: Vec<usize> = form
            .row_hits
            .iter()
            .filter_map(|hit| match hit.target {
                FormTarget::Option(index) => Some(index),
                _ => None,
            })
            .collect();
        assert!(options.len() < count, "the list is taller than the form");
        assert!(
            options.iter().all(|index| *index > 0),
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

    #[test_case(TYPED ; "ascii")]
    #[test_case("Wait…" ; "ellipsis")]
    #[test_case("café" ; "accented")]
    #[test_case("\u{10400}" ; "supplementary_plane")]
    fn a_typed_answer_is_kept_verbatim(answer: &str) {
        let mut form = opened(vec![question(HEADER, false)]);
        press(&mut form, KeyCode::Down);
        press(&mut form, KeyCode::Down);
        press(&mut form, KeyCode::Enter);
        assert_eq!(form.mode, Mode::EditingCustom);
        type_text(&mut form, answer);
        let action = press(&mut form, KeyCode::Enter);
        assert!(
            matches!(&action, QuestionFormAction::Submit(picks) if picks == &[vec![answer.to_owned()]]),
        );
    }

    #[test_case("", 0, false ; "empty")]
    #[test_case("\\", 0, false ; "start_of_buffer")]
    #[test_case("\\", 1, true ; "ascii_backslash")]
    #[test_case("…", 1, false ; "unicode_without_backslash")]
    #[test_case("…\\", 2, true ; "unicode_backslash")]
    #[test_case("éa\\", 3, true ; "byte_boundary_still_wrong_character")]
    #[test_case("…\\tail", 2, true ; "middle_of_line")]
    #[test_case("…a\\", 2, false ; "backslash_after_caret")]
    #[test_case("…\n\\", 3, true ; "later_line")]
    #[test_case("…\\\ntail", 3, false ; "start_of_later_line")]
    fn backslash_detection_uses_character_positions(text: &str, cursor: usize, expected: bool) {
        let mut field = TextField::with_text(FieldKind::Block, text);
        field.set_cursor_offset(cursor);
        assert_eq!(ends_with_backslash(&field), expected);
    }

    #[test_case("plain\\", 6, "plain\n" ; "ascii")]
    #[test_case("…\\", 2, "…\n" ; "ellipsis")]
    #[test_case("éa\\", 3, "éa\n" ; "accented")]
    #[test_case("\u{10400}\\", 2, "\u{10400}\n" ; "supplementary_plane")]
    #[test_case("…\\tail", 2, "…\ntail" ; "middle_of_line")]
    #[test_case("…\né\\tail", 4, "…\né\ntail" ; "later_line")]
    fn enter_replaces_backslash_before_caret_with_newline(
        text: &str,
        cursor: usize,
        expected: &str,
    ) {
        let mut form = typing(false);
        form.custom.set_text(text);
        form.custom.set_cursor_offset(cursor);

        let action = press(&mut form, KeyCode::Enter);

        assert!(matches!(action, QuestionFormAction::Consumed));
        assert_eq!(form.mode, Mode::EditingCustom);
        assert_eq!(form.custom.text(), expected);
        assert_eq!(form.custom.cursor_offset(), cursor);
        chord(&mut form, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(
            form.custom.text(),
            text,
            "one undo brings the backslash back"
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
        assert_eq!(form.custom.text(), TYPED, "the box is prefilled to edit");
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

    /// The answer box is a `TextField`, so it answers to the same editing
    /// keys as the composer rather than a subset of them.
    #[test]
    fn ctrl_w_deletes_the_word_before_the_cursor() {
        let mut form = typing(false);
        chord(&mut form, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(form.custom.text(), TYPED_HEAD);
    }

    /// `Ctrl+C` is the box's copy while something is selected, and the form's
    /// cancel only once there is nothing to copy.
    #[test_case(true ; "selection_copies")]
    #[test_case(false ; "no_selection_cancels")]
    fn ctrl_c_in_the_box_copies_a_selection_before_cancelling(selected: bool) {
        let mut form = typing(false);
        if selected {
            chord(&mut form, KeyCode::Char('a'), KeyModifiers::CONTROL);
        }
        let action = chord(&mut form, KeyCode::Char('c'), KeyModifiers::CONTROL);
        match selected {
            true => assert!(matches!(&action, QuestionFormAction::Copy(text) if text == TYPED)),
            false => assert!(matches!(action, QuestionFormAction::Cancel)),
        }
        assert_eq!(form.custom.text(), TYPED);
    }

    #[test]
    fn ctrl_left_moves_the_cursor_by_word() {
        let mut form = typing(false);
        chord(&mut form, KeyCode::Left, KeyModifiers::CONTROL);
        assert_eq!(form.custom.cursor_offset(), TYPED_HEAD.chars().count());
    }

    /// `TYPED` up to the word `Ctrl+W` eats and `Ctrl+Left` jumps over.
    const TYPED_HEAD: &str = "something ";

    /// Nothing puts a terminal cursor over the form, so the box has to paint
    /// its own caret or the user is typing blind.
    #[test]
    fn the_answer_box_paints_a_caret_where_the_next_character_lands() {
        let mut form = typing(false);
        render(&mut form);
        let editor = hit_area(&form, FormTarget::Editor(0));
        let caret = caret_cells(&mut form);
        assert_eq!(caret.len(), 1, "the caret is the only marked cell");
        assert!(
            reversed_cells(&mut form).is_empty(),
            "the caret is not a selection"
        );
        let typed_width = (CUSTOM_PROMPT.chars().count() + TYPED.chars().count()) as u16;
        assert_eq!(caret[0], (editor.x + typed_width, editor.y));

        chord(&mut form, KeyCode::Left, KeyModifiers::CONTROL);
        render(&mut form);
        let moved = caret_cells(&mut form);
        let head_width = (CUSTOM_PROMPT.chars().count() + TYPED_HEAD.chars().count()) as u16;
        assert_eq!(
            moved,
            [(editor.x + head_width, editor.y)],
            "the caret follows the cursor the keys move"
        );
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

    /// Dismissing answers nothing and lets the run carry on; cancelling stops
    /// the agent that asked. The two must never collapse back into one key.
    #[test_case(Mode::Selecting ; "selecting")]
    #[test_case(Mode::EditingCustom ; "editing")]
    #[test_case(Mode::Confirming ; "confirming")]
    fn ctrl_c_cancels_instead_of_dismissing(mode: Mode) {
        let mut form = opened(vec![question(HEADER, false)]);
        form.mode = mode;
        assert!(matches!(
            chord(&mut form, KeyCode::Char('c'), KeyModifiers::CONTROL),
            QuestionFormAction::Cancel
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
            ..AskedQuestion::default()
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

    const SECOND: &str = "Second";
    const EXPECT_TAB_SWITCH: &str = "clicking a tab opens that question";
    const EXPECT_NO_DRIFT: &str = "the hit rects must sit on the spans they name";

    fn two_questions() -> Vec<AskedQuestion> {
        vec![question(HEADER, false), question(SECOND, false)]
    }

    fn click_target(form: &mut QuestionForm, target: FormTarget) -> QuestionFormAction {
        let (column, row) = target_cell(form, target);
        click(form, column, row)
    }

    #[test]
    fn clicking_a_tab_switches_question() {
        let mut form = opened(two_questions());
        render(&mut form);
        click_target(&mut form, FormTarget::Tab(1));
        assert_eq!(form.tab, 1, "{EXPECT_TAB_SWITCH}");
        click_target(&mut form, FormTarget::Tab(0));
        assert_eq!(form.tab, 0, "{EXPECT_TAB_SWITCH}");
    }

    /// The bar is one line of variable-width spans, so a hit rect that is off
    /// by a cell silently selects the neighbouring tab.
    #[test]
    fn every_tab_hit_covers_the_span_it_names() {
        let mut form = opened(two_questions());
        render(&mut form);
        for index in 0..form.questions.len() {
            let area = hit_area(&form, FormTarget::Tab(index));
            let label = tab_label(index, &form.questions[index]);
            for column in area.x..area.right() {
                click(&mut form, column, area.y);
                assert_eq!(form.tab, index, "{EXPECT_NO_DRIFT}: {label} at {column}");
            }
        }
    }

    #[test]
    fn clicking_the_review_tab_opens_the_review() {
        let mut form = opened(two_questions());
        render(&mut form);
        click_target(&mut form, FormTarget::Review);
        assert_eq!(form.mode, Mode::Confirming);
    }

    /// The review is where a reader notices they picked wrong, so its rows
    /// are the shortest way back to the question that made them.
    #[test]
    fn clicking_a_review_row_goes_back_to_that_question() {
        let mut form = opened(two_questions());
        render(&mut form);
        press(&mut form, KeyCode::Enter);
        press(&mut form, KeyCode::Enter);
        assert_eq!(form.mode, Mode::Confirming);
        render(&mut form);
        click_target(&mut form, FormTarget::ReviewRow(0));
        assert_eq!(form.mode, Mode::Selecting);
        assert_eq!(form.tab, 0);
    }

    #[test]
    fn the_review_submits_from_its_hint() {
        let mut form = opened(two_questions());
        render(&mut form);
        press(&mut form, KeyCode::Enter);
        press(&mut form, KeyCode::Enter);
        render(&mut form);
        let index = hint_index(&form, key::ENTER);
        let action = click_target(&mut form, FormTarget::Hint(index));
        assert!(
            matches!(&action, QuestionFormAction::Submit(picks) if picks.len() == 2),
            "the submit hint has to do what Enter does"
        );
    }

    fn hint_index(form: &QuestionForm, bind: Bind) -> usize {
        form.hint_pairs()
            .iter()
            .position(|hint| hint.label() == bind.label)
            .expect("the bar offers the hint")
    }

    #[test]
    fn the_dismiss_hint_dismisses() {
        let mut form = opened(vec![question(HEADER, false)]);
        render(&mut form);
        let index = hint_index(&form, key::ESC);
        let action = click_target(&mut form, FormTarget::Hint(index));
        assert!(matches!(action, QuestionFormAction::Dismiss));
    }

    #[test]
    fn the_cancel_hint_cancels() {
        let mut form = opened(vec![question(HEADER, false)]);
        render(&mut form);
        let index = hint_index(&form, key::QUIT);
        let action = click_target(&mut form, FormTarget::Hint(index));
        assert!(matches!(action, QuestionFormAction::Cancel));
    }

    /// A hint that resolves to no key is a control the form draws and then
    /// ignores, which is worse than not drawing it.
    #[test]
    fn every_hint_stands_for_a_key_and_gets_a_hit() {
        let mut form = opened(two_questions());
        for mode in [Mode::Selecting, Mode::EditingCustom, Mode::Confirming] {
            form.mode = mode;
            render(&mut form);
            for (index, hint) in form.hint_pairs().iter().enumerate() {
                let label = hint.label();
                assert!(hint.press().is_some(), "{label:?} in {mode:?} names no key");
                let drawn = form
                    .row_hits
                    .iter()
                    .any(|hit| hit.target == FormTarget::Hint(index));
                assert!(drawn, "hint {label:?} in {mode:?} got no hit rect");
            }
        }
    }

    #[test]
    fn clicking_in_the_answer_box_moves_the_cursor() {
        let mut form = opened(vec![question(HEADER, false)]);
        render(&mut form);
        press(&mut form, KeyCode::Down);
        press(&mut form, KeyCode::Down);
        assert_eq!(form.mode, Mode::Selecting);
        press(&mut form, KeyCode::Enter);
        type_text(&mut form, TYPED);
        render(&mut form);
        assert_eq!(form.custom.cursor().col, TYPED.chars().count());
        let area = hit_area(&form, FormTarget::Editor(0));
        let column = area.x + CUSTOM_PROMPT.chars().count() as u16 + 3;
        click(&mut form, column, area.y);
        assert_eq!(
            form.custom.cursor().col,
            3,
            "the cursor lands on the clicked cell"
        );
    }

    /// Past the end of the text there is no cell to land on, so the cursor
    /// stops at the end rather than running off it.
    #[test]
    fn clicking_past_the_answer_clamps_to_its_end() {
        let mut form = opened(vec![question(HEADER, false)]);
        render(&mut form);
        press(&mut form, KeyCode::Down);
        press(&mut form, KeyCode::Down);
        press(&mut form, KeyCode::Enter);
        type_text(&mut form, TYPED);
        render(&mut form);
        let area = hit_area(&form, FormTarget::Editor(0));
        click(&mut form, area.right() - 1, area.y);
        assert_eq!(form.custom.cursor().col, TYPED.chars().count());
    }

    #[test]
    fn the_wheel_scrolls_up_on_a_positive_delta() {
        let mut form = opened(vec![long_question()]);
        render(&mut form);
        for _ in 0..form.custom_row() {
            press(&mut form, KeyCode::Down);
        }
        render(&mut form);
        let scrolled = form.scroll;
        assert!(scrolled > 0, "a long list has somewhere to scroll from");
        form.scroll(1);
        render(&mut form);
        assert!(
            form.scroll < scrolled,
            "positive delta is ScrollUp everywhere else in the app"
        );
    }

    /// The viewport used to be dragged back to the cursor on the very next
    /// frame, which made the wheel look broken.
    #[test]
    fn the_wheel_survives_the_next_frame() {
        let mut form = opened(vec![long_question()]);
        render(&mut form);
        for _ in 0..form.custom_row() {
            press(&mut form, KeyCode::Down);
        }
        render(&mut form);
        form.scroll(i32::from(u16::MAX));
        render(&mut form);
        assert_eq!(form.scroll, 0, "the wheel reached the top and stayed");
        render(&mut form);
        assert_eq!(form.scroll, 0, "and a repaint alone must not undo it");
    }

    /// Moving the cursor is the reader asking to see it again.
    #[test]
    fn a_keystroke_puts_the_viewport_back_on_the_cursor() {
        let mut form = opened(vec![long_question()]);
        render(&mut form);
        for _ in 0..form.custom_row() {
            press(&mut form, KeyCode::Down);
        }
        render(&mut form);
        let followed = form.scroll;
        form.scroll(i32::from(u16::MAX));
        render(&mut form);
        assert_eq!(form.scroll, 0);
        press(&mut form, KeyCode::Up);
        render(&mut form);
        assert!(form.scroll >= followed.saturating_sub(1));
    }

    const OPTION_LABEL: &str = "option";

    fn long_question() -> AskedQuestion {
        AskedQuestion {
            question: PICK.into(),
            header: HEADER.into(),
            options: (0..60)
                .map(|index| QuestionOption {
                    label: format!("{OPTION_LABEL} {index}"),
                    description: String::new(),
                })
                .collect(),
            multiple: false,
            ..AskedQuestion::default()
        }
    }
    const EXPECT_MARKED: &str = "the control under the pointer has to be marked";
    const EXPECT_UNMARKED: &str = "nothing else may be marked";

    use ratatui::buffer::Cell;
    use ratatui::style::Modifier;
    use test_case::test_case;

    fn reversed_cells(form: &mut QuestionForm) -> Vec<(u16, u16)> {
        painted_cells(form, |cell| cell.modifier.contains(Modifier::REVERSED))
    }

    fn caret_cells(form: &mut QuestionForm) -> Vec<(u16, u16)> {
        let caret = theme::current().cursor;
        painted_cells(form, |cell| {
            Some(cell.fg) == caret.fg && Some(cell.bg) == caret.bg
        })
    }

    fn painted_cells(form: &mut QuestionForm, marked: impl Fn(&Cell) -> bool) -> Vec<(u16, u16)> {
        let backend = ratatui::backend::TestBackend::new(TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let area = docked(form);
        terminal.draw(|frame| form.view(frame, area)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..TERMINAL_HEIGHT)
            .flat_map(|y| (0..TERMINAL_WIDTH).map(move |x| (x, y)))
            .filter(|(x, y)| marked(&buffer[(*x, *y)]))
            .collect()
    }

    fn hover(form: &mut QuestionForm, target: FormTarget) {
        let (column, row) = target_cell(form, target);
        form.handle_mouse(mouse(MouseEventKind::Moved, column, row));
    }

    #[test_case(FormTarget::Tab(1)  ; "another_tab")]
    #[test_case(FormTarget::Review  ; "the_review_tab")]
    #[test_case(FormTarget::Hint(0) ; "a_hint")]
    fn hovering_a_control_marks_it(target: FormTarget) {
        let mut form = opened(two_questions());
        render(&mut form);
        assert!(reversed_cells(&mut form).is_empty(), "{EXPECT_UNMARKED}");

        hover(&mut form, target);
        let marked = reversed_cells(&mut form);
        assert!(!marked.is_empty(), "{EXPECT_MARKED}");
        let area = hit_area(&form, target);
        assert!(
            marked
                .iter()
                .all(|(x, y)| area.contains(Position::new(*x, *y))),
            "{EXPECT_UNMARKED}: {marked:?} outside {area:?}"
        );
    }

    /// A short question and its answer, neither of them wrapped.
    const REVIEW_ROWS_PER_QUESTION: usize = 2;

    /// A review row is the question and the answer under it, so the mark
    /// covers both of its lines and nothing outside them.
    #[test]
    fn hovering_a_review_row_marks_it() {
        let mut form = opened(two_questions());
        form.mode = Mode::Confirming;
        render(&mut form);
        hover(&mut form, FormTarget::ReviewRow(1));
        let marked = reversed_cells(&mut form);
        assert!(!marked.is_empty(), "{EXPECT_MARKED}");
        let areas: Vec<Rect> = form
            .row_hits
            .iter()
            .filter(|hit| hit.target == FormTarget::ReviewRow(1))
            .map(|hit| hit.area)
            .collect();
        assert_eq!(areas.len(), REVIEW_ROWS_PER_QUESTION);
        assert!(
            marked.iter().all(|(x, y)| areas
                .iter()
                .any(|area| area.contains(Position::new(*x, *y)))),
            "{EXPECT_UNMARKED}: {marked:?} outside {areas:?}"
        );
    }

    /// A review row that wraps is still one target, so the mark and the click
    /// have to reach every row of it.
    #[test]
    fn a_wrapped_review_row_is_marked_and_clickable_throughout() {
        let mut form = opened(vec![
            AskedQuestion {
                question: [PICK; 20].join(" "),
                header: HEADER.into(),
                options: Vec::new(),
                multiple: false,
                ..AskedQuestion::default()
            },
            question(SECOND, false),
        ]);
        form.mode = Mode::Confirming;
        render(&mut form);
        let areas: Vec<Rect> = form
            .row_hits
            .iter()
            .filter(|hit| hit.target == FormTarget::ReviewRow(0))
            .map(|hit| hit.area)
            .collect();
        assert!(
            areas.len() > REVIEW_ROWS_PER_QUESTION,
            "the question wrapped onto more rows"
        );

        hover(&mut form, FormTarget::ReviewRow(0));
        let marked = reversed_cells(&mut form);
        assert!(!marked.is_empty(), "{EXPECT_MARKED}");
        assert!(
            marked.iter().all(|(x, y)| areas
                .iter()
                .any(|area| area.contains(Position::new(*x, *y)))),
            "{EXPECT_UNMARKED}: {marked:?} outside {areas:?}"
        );

        let last = areas.last().expect("the row was drawn");
        click(&mut form, last.x + 1, last.y);
        assert_eq!(form.mode, Mode::Selecting);
        assert_eq!(form.tab, 0, "the last row goes back to its own question");
    }

    /// An option row is already marked by the cursor the hover moves, so
    /// reversing it too would say the same thing twice.
    #[test]
    fn hovering_an_option_marks_nothing_extra() {
        let mut form = opened(two_questions());
        render(&mut form);
        let (column, row) = option_row(&form, 1);
        form.handle_mouse(mouse(MouseEventKind::Moved, column, row));
        assert_eq!(form.cursor, 1);
        assert!(reversed_cells(&mut form).is_empty(), "{EXPECT_UNMARKED}");
    }

    #[test]
    fn moving_off_every_control_unmarks_them() {
        let mut form = opened(two_questions());
        render(&mut form);
        hover(&mut form, FormTarget::Review);
        assert!(!reversed_cells(&mut form).is_empty(), "{EXPECT_MARKED}");

        let outside = hit_area(&form, FormTarget::Review);
        form.handle_mouse(mouse(MouseEventKind::Moved, outside.x, TERMINAL_HEIGHT - 1));
        assert!(reversed_cells(&mut form).is_empty(), "{EXPECT_UNMARKED}");
    }

    /// The gap between two hints is not part of either, so the pointer marks
    /// a control only once it is over its glyphs.
    #[test]
    fn the_gap_before_a_hint_marks_nothing() {
        let mut form = opened(two_questions());
        render(&mut form);
        let hit = hit_area(&form, FormTarget::Hint(1));

        form.handle_mouse(mouse(MouseEventKind::Moved, hit.x - 1, hit.y));
        assert_eq!(form.hover, None);
        assert!(reversed_cells(&mut form).is_empty(), "{EXPECT_UNMARKED}");
    }

    /// The hover marks what a click would press, so a hint that reads as a
    /// button has to act like one.
    #[test]
    fn the_marked_hint_is_the_one_a_click_presses() {
        let mut form = opened(two_questions());
        render(&mut form);
        let index = hint_index(&form, key::TAB);
        let (column, row) = target_cell(&form, FormTarget::Hint(index));
        form.handle_mouse(mouse(MouseEventKind::Moved, column, row));
        assert_eq!(form.hover, Some(FormTarget::Hint(index)));
        click(&mut form, column, row);
        assert_eq!(form.tab, 1, "the next-question hint moved the tab");
    }

    /// Hints are held by position, so a click that reshuffles them must not
    /// leave the mark on whatever moved into the old place.
    #[test]
    fn a_click_that_changes_the_hints_drops_the_mark() {
        let mut form = opened(two_questions());
        render(&mut form);
        let (column, row) = target_cell(&form, FormTarget::Review);
        form.handle_mouse(mouse(MouseEventKind::Moved, column, row));
        assert!(form.hover.is_some(), "{EXPECT_MARKED}");

        click(&mut form, column, row);
        assert_eq!(form.mode, Mode::Confirming, "the review tab opened review");
        assert_eq!(form.hover, None);
        assert!(reversed_cells(&mut form).is_empty(), "{EXPECT_UNMARKED}");
    }

    const UNCOMMITTED: &str = "uncommitted custom draft";
    const BTW_TEXT: &str = "/btw";
    const FULL_QUESTION: &str = "Which choice?\nExplain the tradeoffs.";
    const FULL_DESCRIPTION: &str = "First detail\nSecond detail";
    const MANUAL_SCROLL: i32 = -2;

    #[test_case(Mode::Selecting, false, false ; "single_key")]
    #[test_case(Mode::Selecting, false, true ; "single_footer")]
    #[test_case(Mode::Selecting, true, false ; "multi_key")]
    #[test_case(Mode::Selecting, true, true ; "multi_footer")]
    #[test_case(Mode::EditingCustom, false, false ; "editing_key")]
    #[test_case(Mode::EditingCustom, false, true ; "editing_footer")]
    #[test_case(Mode::Confirming, true, false ; "review_key")]
    #[test_case(Mode::Confirming, true, true ; "review_footer")]
    fn asking_preserves_answers_editor_and_manual_scroll(mode: Mode, multiple: bool, footer: bool) {
        let mut question = long_question();
        question.question = PICK.repeat(usize::from(TERMINAL_WIDTH));
        question.multiple = multiple;
        let mut form = opened(vec![question.clone(), question]);
        form.tab = 1;
        form.cursor = form.custom_row();
        form.mode = mode;
        form.answers = vec![vec![TYPED.into()], vec![OPTION_LABEL.into()]];
        form.custom.set_text(TYPED);
        form.custom.insert_text(UNCOMMITTED);
        form.custom.set_cursor(Cursor::new(0, 1), false);
        form.custom.set_cursor(Cursor::new(0, 3), true);
        render(&mut form);
        form.scroll(i32::from(u16::MAX));
        form.scroll(MANUAL_SCROLL);
        render(&mut form);
        assert!(form.scroll > 0);
        assert!(!form.follow_cursor);

        let state = |form: &QuestionForm| {
            (
                form.mode,
                form.tab,
                form.cursor,
                form.answers.clone(),
                form.custom.text(),
                form.custom.cursor(),
                form.custom.selection(),
                form.scroll,
                form.follow_cursor,
            )
        };
        let before = state(&form);
        let index = hint_index(&form, key::QUESTION_ASK_BTW);
        let action = if footer {
            let (column, row) = target_cell(&form, FormTarget::Hint(index));
            form.handle_mouse(mouse(MouseEventKind::Moved, column, row));
            assert_eq!(state(&form), before);
            form.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), column, row));
            assert_eq!(state(&form), before);
            render(&mut form);
            assert_eq!(state(&form), before);
            form.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), column, row))
                .unwrap()
        } else {
            form.hover = Some(FormTarget::Hint(index));
            form.mouse_down = form.hover;
            form.handle_key(key::QUESTION_ASK_BTW.to_key_event())
        };
        assert!(matches!(action, QuestionFormAction::AskBtw));
        assert_eq!(state(&form), before);
        assert_eq!(form.hover, None);
        assert_eq!(form.mouse_down, None);
        render(&mut form);
        assert_eq!(state(&form), before);
        form.custom.handle_key(key::UNDO.to_key_event());
        assert_eq!(form.custom.text(), TYPED);
    }

    #[test_case(Mode::Selecting ; "selecting")]
    #[test_case(Mode::EditingCustom ; "editing")]
    #[test_case(Mode::Confirming ; "review")]
    fn clarification_context_contains_only_question_definitions(mode: Mode) {
        let mut questions = two_questions();
        questions[1].question = FULL_QUESTION.into();
        questions[1].options[0].description = FULL_DESCRIPTION.into();
        questions[1].multiple = true;
        let mut form = opened(questions);
        form.mode = mode;
        form.tab = 1;
        let before = form.clarification_context();
        form.answers = vec![vec![TYPED.into()], vec![YES.into(), TYPED.into()]];
        form.custom.set_text(UNCOMMITTED);
        let context = form.clarification_context();
        assert_eq!(context, before);
        assert!(!context.contains(TYPED));
        assert!(!context.contains(UNCOMMITTED));
        let active = format!(
            "About: 2/2 · {SECOND}\n{FULL_QUESTION}\n({MULTIPLE_ANSWERS})\n- {YES}\n  {FULL_DESCRIPTION}\n- {NO}\n  {NO_DESC}",
        );
        let expected = if mode == Mode::Confirming {
            format!(
                "About: 1/2 · {HEADER}\n{PICK}\n({SINGLE_ANSWER})\n- {YES}\n  {YES_DESC}\n- {NO}\n  {NO_DESC}\n\n{active}",
            )
        } else {
            active
        };
        assert_eq!(context, expected);
    }

    #[test_case(false ; "typed")]
    #[test_case(true ; "pasted")]
    fn literal_btw_is_custom_answer_text(pasted: bool) {
        let mut form = opened(vec![question(HEADER, false)]);
        form.cursor = form.custom_row();
        press(&mut form, KeyCode::Enter);
        if pasted {
            assert!(form.handle_paste(BTW_TEXT));
        } else {
            type_text(&mut form, BTW_TEXT);
        }
        assert_eq!(form.custom.text(), BTW_TEXT);
        assert!(matches!(
            press(&mut form, KeyCode::Enter),
            QuestionFormAction::Submit(answers) if answers == vec![vec![BTW_TEXT.to_owned()]]
        ));
    }

    #[test_case(10, COMPACT_ASK_HINT ; "compact")]
    #[test_case(15, ASK_HINT ; "ask_only")]
    #[test_case(30, ASK_HINT ; "narrow")]
    #[test_case(TERMINAL_WIDTH, ASK_HINT ; "normal")]
    fn ask_hint_stays_visible_and_clickable(width: u16, expected: Hint) {
        for mode in [Mode::Selecting, Mode::EditingCustom, Mode::Confirming] {
            let mut form = opened(two_questions());
            form.mode = mode;
            let backend = ratatui::backend::TestBackend::new(width, TERMINAL_HEIGHT);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            let area = Rect::new(0, 0, width, TERMINAL_HEIGHT);
            terminal.draw(|frame| form.view(frame, area)).unwrap();
            let index = hint_index(&form, key::QUESTION_ASK_BTW);
            let hit = hit_area(&form, FormTarget::Hint(index));
            let text: String = (hit.x..hit.right())
                .map(|x| terminal.backend().buffer()[(x, hit.y)].symbol())
                .collect();
            assert_eq!(
                text,
                format!("{} {}", expected.label(), expected.description())
            );
            assert!(form.hint(width - 2).width() <= usize::from(width - 2));
            assert!(matches!(
                click_target(&mut form, FormTarget::Hint(index)),
                QuestionFormAction::AskBtw
            ));
        }
    }

    #[test]
    fn inactive_form_hides_only_the_custom_caret() {
        let mut form = typing(false);
        let before = (
            form.custom.text(),
            form.custom.cursor(),
            form.custom.selection(),
        );
        let backend = ratatui::backend::TestBackend::new(TERMINAL_WIDTH, TERMINAL_HEIGHT);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let area = docked(&form);
        let caret = theme::current().cursor;
        for focused in [true, false, true] {
            terminal
                .draw(|frame| form.view_with_focus(frame, area, focused))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let visible = buffer
                .content
                .iter()
                .any(|cell| Some(cell.fg) == caret.fg && Some(cell.bg) == caret.bg);
            assert_eq!(visible, focused);
            assert_eq!(
                (
                    form.custom.text(),
                    form.custom.cursor(),
                    form.custom.selection()
                ),
                before
            );
        }
    }

    #[test]
    fn closed_form_does_not_offer_clarification() {
        let mut form = QuestionForm::new();
        assert!(form.clarification_context().is_empty());
        assert!(matches!(
            form.handle_key(key::QUESTION_ASK_BTW.to_key_event()),
            QuestionFormAction::Consumed
        ));
    }

    const ESCALATION: &str = "the user must choose";
    const EXPECT_NOTE: &str = "the form must say what autopilot is doing about it";

    /// The note sits above the tab bar, so the tabs must still be found
    /// where they are drawn.
    #[test_case(AutopilotNote::Deciding ; "deciding")]
    #[test_case(AutopilotNote::Escalated { reason: ESCALATION.into() } ; "left_to_you")]
    fn the_form_says_what_autopilot_is_doing(note: AutopilotNote) {
        let mut form = opened(two_questions());
        form.set_autopilot_note(Some(note.clone()));

        let screen = painted(&mut form).join(LINE_BREAK);

        assert!(
            screen.contains(&card_note(&note)),
            "{EXPECT_NOTE}\n{screen}"
        );
        click_target(&mut form, FormTarget::Tab(1));
        assert_eq!(form.tab, 1, "{EXPECT_TAB_SWITCH}");
    }

    #[test]
    fn a_new_form_drops_the_note() {
        let mut form = opened(two_questions());
        form.set_autopilot_note(Some(AutopilotNote::Deciding));

        form.open(two_questions(), None);

        assert_eq!(form.autopilot_note(), None);
    }

    const SERVER: &str = "tracker";
    const MESSAGE: &str = "Fill in the ticket";
    const COUNT: &str = "Count";
    const NOTE: &str = "Note";
    const MAXIMUM: i64 = 10;
    const IN_RANGE: &str = "7";
    const OUT_OF_RANGE: &str = "42";
    const NOT_WHOLE: &str = "seven";
    const DRAFT: &str = "3";
    const SAFE_URL: &str = "https://example.com/login";
    const SAFE_HOST: &str = "example.com";
    const PUNYCODE_URL: &str = "https://xn--pple-43d.com/login";
    const PUNYCODE_HOST: &str = "xn--pple-43d.com";
    const CYRILLIC_URL: &str = "https://\u{430}pple.com/login";
    const FULLWIDTH_URL: &str = "https://\u{ff45}xample.com/login";
    const CONFIRM_FIELD: &str = "confirm";

    fn mcp(url: Option<&str>) -> QuestionSource {
        QuestionSource::Mcp {
            server: SERVER.into(),
            kind: None,
            message: MESSAGE.into(),
            url: url.map(str::to_owned),
        }
    }

    fn asked_by_server(questions: Vec<AskedQuestion>) -> QuestionForm {
        let mut form = QuestionForm::new();
        form.open(questions, Some(mcp(None)));
        form
    }

    fn count_question() -> AskedQuestion {
        AskedQuestion {
            question: PICK.into(),
            header: COUNT.into(),
            answer: AnswerKind::Number {
                integer: true,
                minimum: Some(1.into()),
                maximum: Some(MAXIMUM.into()),
            },
            ..AskedQuestion::default()
        }
    }

    fn note_question() -> AskedQuestion {
        AskedQuestion {
            question: PICK.into(),
            header: NOTE.into(),
            answer: AnswerKind::Text {
                format: None,
                min_length: None,
                max_length: None,
            },
            optional: true,
            ..AskedQuestion::default()
        }
    }

    fn offers(form: &QuestionForm, bind: Bind) -> Option<usize> {
        form.hint_pairs()
            .iter()
            .position(|hint| hint.label() == bind.label)
    }

    const DEFERRED_TASK: &str = "question-task";
    const CALL: &str = "toolu_question";
    const PLACED_TITLE: &str = "Deferred question 2 of 3";

    fn delivered(delivery: QuestionDelivery) -> QuestionForm {
        let mut form = QuestionForm::new();
        form.ask(QuestionEvent {
            questions: vec![question(HEADER, false)],
            call_id: Some(CALL.into()),
            delivery,
            ..QuestionEvent::default()
        });
        form
    }

    fn deferred() -> QuestionDelivery {
        QuestionDelivery::Deferred {
            task_id: DEFERRED_TASK.into(),
            docked: true,
        }
    }

    #[test_case(QuestionDelivery::Blocking, false ; "an_ask_that_cannot_be_put_off")]
    #[test_case(QuestionDelivery::Deferrable, true ; "an_ask_the_user_may_put_off")]
    fn only_a_deferrable_ask_offers_later(delivery: QuestionDelivery, offered: bool) {
        let mut form = delivered(delivery);

        assert_eq!(offers(&form, key::QUESTION_LATER).is_some(), offered);
        let action = form.handle_key(key::QUESTION_LATER.to_key_event());
        assert_eq!(matches!(action, QuestionFormAction::Later), offered);
        assert_eq!(form.call_id(), Some(CALL));
    }

    #[test]
    fn escape_puts_a_deferred_form_aside_instead_of_dismissing_it() {
        let mut form = delivered(deferred());

        assert_eq!(form.deferred_task(), Some(DEFERRED_TASK));
        assert!(matches!(
            press(&mut form, KeyCode::Esc),
            QuestionFormAction::PutAside
        ));
        let hints = form.hint_pairs();
        assert!(hints.iter().all(|hint| hint.description != DISMISS_LABEL));
        assert!(
            hints.iter().any(|hint| hint.label() == key::ESC.label
                && hint.description == PUT_ASIDE_HINT.description)
        );
        assert_eq!(
            offers(&form, key::QUESTION_LATER),
            None,
            "one question has no next"
        );
    }

    #[test]
    fn a_placed_deferred_form_names_its_place_and_offers_the_next() {
        let mut form = delivered(deferred());
        form.set_place(Some((2, 3)));

        assert!(offers(&form, key::QUESTION_LATER).is_some());
        assert!(matches!(
            form.handle_key(key::QUESTION_LATER.to_key_event()),
            QuestionFormAction::Later
        ));
        assert!(
            painted(&mut form)
                .iter()
                .any(|row| row.contains(PLACED_TITLE))
        );
    }

    #[test]
    fn a_redelivered_form_keeps_its_draft() {
        let mut form = delivered(QuestionDelivery::Deferrable);
        press(&mut form, KeyCode::Down);
        form.touch();

        form.redeliver(deferred());

        assert_eq!(form.cursor, 1);
        assert!(form.touched());
        assert_eq!(form.deferred_task(), Some(DEFERRED_TASK));
    }

    #[test]
    fn only_a_server_form_can_be_declined() {
        let mut own = opened(vec![question(HEADER, false)]);
        assert_eq!(offers(&own, key::QUESTION_DECLINE), None);
        assert!(matches!(
            own.handle_key(key::QUESTION_DECLINE.to_key_event()),
            QuestionFormAction::Consumed
        ));

        let mut asked = asked_by_server(vec![question(HEADER, false)]);
        let index = offers(&asked, key::QUESTION_DECLINE).expect("a server's form offers decline");
        assert_eq!(index, asked.hint_pairs().len() - CLOSING_HINTS - 1);
        assert!(matches!(
            asked.handle_key(key::QUESTION_DECLINE.to_key_event()),
            QuestionFormAction::Decline
        ));
        render(&mut asked);
        assert!(matches!(
            click_target(&mut asked, FormTarget::Hint(index)),
            QuestionFormAction::Decline
        ));
    }

    #[test]
    fn a_server_form_takes_only_the_offered_choices() {
        let mut form = asked_by_server(vec![question(HEADER, false)]);
        for _ in 0..=form.custom_row() {
            press(&mut form, KeyCode::Down);
        }
        assert_eq!(form.cursor, form.custom_row() - 1);
        let screen = painted(&mut form).join(LINE_BREAK);
        assert!(!screen.contains(CUSTOM_OPTION), "{screen}");
        assert!(screen.contains(MESSAGE), "{screen}");
        assert!(screen.contains(&format!(" {SERVER} {ASKS} ")), "{screen}");
    }

    #[test_case(NOT_WHOLE, NOT_A_WHOLE_NUMBER ; "not_a_number")]
    #[test_case(OUT_OF_RANGE, ABOVE_MAXIMUM ; "out_of_range")]
    #[test_case("", REQUIRED ; "empty")]
    fn a_typed_answer_that_does_not_fit_stays_put(text: &str, problem: &str) {
        let mut form = asked_by_server(vec![count_question()]);
        assert_eq!(form.mode, Mode::EditingCustom);
        type_text(&mut form, text);

        assert!(matches!(
            press(&mut form, KeyCode::Enter),
            QuestionFormAction::Consumed
        ));

        assert_eq!(form.mode, Mode::EditingCustom);
        assert_eq!(form.custom.text(), text);
        let error = form.error.clone().expect("the problem is shown");
        assert!(error.starts_with(problem), "{error}");
        assert!(painted(&mut form).join(LINE_BREAK).contains(&error));
        press(&mut form, KeyCode::Backspace);
        type_text(&mut form, IN_RANGE);
        assert_eq!(form.error, None, "editing the answer clears the problem");
    }

    #[test]
    fn a_fitting_typed_answer_goes_through_the_review() {
        let mut form = asked_by_server(vec![count_question()]);
        type_text(&mut form, IN_RANGE);
        press(&mut form, KeyCode::Enter);
        assert_eq!(form.mode, Mode::Confirming);
        assert!(matches!(
            press(&mut form, KeyCode::Enter),
            QuestionFormAction::Submit(answers) if answers == vec![vec![IN_RANGE.to_owned()]]
        ));
    }

    #[test]
    fn escape_in_a_typed_answer_dismisses() {
        let mut form = asked_by_server(vec![count_question()]);
        assert!(matches!(
            press(&mut form, KeyCode::Esc),
            QuestionFormAction::Dismiss
        ));
    }

    #[test]
    fn an_optional_typed_answer_may_be_left_empty() {
        let mut form = asked_by_server(vec![note_question()]);
        assert!(painted(&mut form).join(LINE_BREAK).contains(OPTIONAL_NOTE));
        press(&mut form, KeyCode::Enter);
        assert_eq!(form.mode, Mode::Confirming);
        assert!(matches!(
            press(&mut form, KeyCode::Enter),
            QuestionFormAction::Submit(answers) if answers == vec![Vec::<String>::new()]
        ));
    }

    #[test]
    fn moving_between_typed_answers_keeps_each_draft() {
        let mut form = asked_by_server(vec![count_question(), note_question()]);
        type_text(&mut form, DRAFT);
        press(&mut form, KeyCode::Tab);
        assert_eq!((form.tab, form.mode), (1, Mode::EditingCustom));
        assert!(form.custom.text().is_empty());
        type_text(&mut form, TYPED);
        press(&mut form, KeyCode::BackTab);
        assert_eq!((form.tab, form.custom.text()), (0, DRAFT.to_owned()));
        press(&mut form, KeyCode::Tab);
        assert_eq!(form.custom.text(), TYPED);
    }

    #[test]
    fn the_review_will_not_send_a_missing_required_answer() {
        let mut form = asked_by_server(vec![question(HEADER, false), count_question()]);
        press(&mut form, KeyCode::Tab);
        type_text(&mut form, IN_RANGE);
        press(&mut form, KeyCode::Tab);
        assert_eq!(form.mode, Mode::Confirming);
        assert!(painted(&mut form).join(LINE_BREAK).contains(REQUIRED));

        assert!(matches!(
            press(&mut form, KeyCode::Enter),
            QuestionFormAction::Consumed
        ));

        assert_eq!((form.tab, form.mode), (0, Mode::Selecting));
        assert_eq!(form.error.as_deref(), Some(REQUIRED));
        assert_eq!(form.answers[1], vec![IN_RANGE.to_owned()]);
    }

    #[test]
    fn a_default_starts_picked_and_an_optional_one_can_be_taken_back() {
        let mut asked = question(HEADER, false);
        asked.default = vec![NO.into()];
        asked.optional = true;
        let mut form = asked_by_server(vec![asked]);
        assert_eq!(form.answers, vec![vec![NO.to_owned()]]);
        form.cursor = 1;

        press(&mut form, KeyCode::Enter);

        assert_eq!(form.answers, vec![Vec::<String>::new()]);
        assert_eq!(form.mode, Mode::Selecting);
    }

    #[test]
    fn a_yes_or_no_field_is_picked_and_sent_as_a_boolean() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { CONFIRM_FIELD: { "type": "boolean" } },
            "required": [CONFIRM_FIELD],
        });
        let elicit = ElicitForm::parse(&schema).unwrap();
        let mut form = asked_by_server(elicit.questions());
        let screen = painted(&mut form).join(LINE_BREAK);
        let kind = describe_answer(&AnswerKind::Boolean).unwrap();
        for expected in [YES_LABEL, NO_LABEL, kind.as_str()] {
            assert!(screen.contains(expected), "{expected:?} in\n{screen}");
        }
        press(&mut form, KeyCode::Enter);
        let QuestionFormAction::Submit(answers) = press(&mut form, KeyCode::Enter) else {
            panic!("the review submits");
        };
        let content = elicit.content(&answers).unwrap();
        assert_eq!(content[CONFIRM_FIELD], serde_json::Value::Bool(true));
    }

    #[test_case(None, ACCEPT ; "form_without_fields")]
    #[test_case(Some(SAFE_URL), OPEN_IN_BROWSER ; "address")]
    fn consent_is_accepted_declined_or_dismissed(url: Option<&str>, accept: &str) {
        let mut form = QuestionForm::new();
        form.open(Vec::new(), Some(mcp(url)));
        assert!(form.is_open());
        let screen = painted(&mut form).join(LINE_BREAK);
        for expected in [MESSAGE, accept, DECLINE] {
            assert!(screen.contains(expected), "{expected:?} in\n{screen}");
        }
        assert!(offers(&form, key::QUESTION_DECLINE).is_some());
        assert!(matches!(
            form.handle_key(key::QUESTION_ASK_BTW.to_key_event()),
            QuestionFormAction::Consumed
        ));
        assert!(matches!(
            click_target(&mut form, FormTarget::Option(ACCEPT_ROW)),
            QuestionFormAction::Accept
        ));
        assert!(matches!(
            press(&mut form, KeyCode::Enter),
            QuestionFormAction::Accept
        ));
        press(&mut form, KeyCode::Down);
        assert!(matches!(
            press(&mut form, KeyCode::Enter),
            QuestionFormAction::Decline
        ));
        assert!(matches!(
            press(&mut form, KeyCode::Esc),
            QuestionFormAction::Dismiss
        ));
    }

    #[test]
    fn an_address_is_shown_whole_with_its_host_picked_out() {
        let mut form = QuestionForm::new();
        form.open(Vec::new(), Some(mcp(Some(SAFE_URL))));
        let screen = painted(&mut form).join(LINE_BREAK);
        assert!(screen.contains(SAFE_URL), "{screen}");
        assert!(!screen.contains(LOOKALIKE_WARNING), "{screen}");
        let accent = theme::current().accent;
        let line = url_line(SAFE_URL, accent);
        let host = line
            .spans
            .iter()
            .find(|span| span.style == accent)
            .expect("the host is styled apart");
        assert_eq!(host.content, SAFE_HOST);
    }

    #[test]
    fn a_punycode_address_warns_where_it_really_goes() {
        let mut form = QuestionForm::new();
        form.open(Vec::new(), Some(mcp(Some(PUNYCODE_URL))));
        let screen = painted(&mut form).join(LINE_BREAK);
        assert!(screen.contains(LOOKALIKE_WARNING), "{screen}");
        assert!(screen.contains(PUNYCODE_HOST), "{screen}");
    }

    #[test_case(SAFE_URL, None ; "plain")]
    #[test_case(PUNYCODE_URL, Some(PUNYCODE_HOST) ; "punycode")]
    #[test_case(CYRILLIC_URL, Some(PUNYCODE_HOST) ; "cyrillic")]
    #[test_case(FULLWIDTH_URL, Some(SAFE_HOST) ; "fullwidth")]
    fn a_lookalike_host_is_flagged_with_where_it_goes(url: &str, host: Option<&str>) {
        assert_eq!(lookalike_host(url).as_deref(), host);
    }

    #[test_case(SAFE_URL, Some(SAFE_HOST) ; "path")]
    #[test_case("https://user:pw@example.com:8443/x?y#z", Some(SAFE_HOST) ; "userinfo_and_port")]
    #[test_case("https://example.com@evil.test/", Some("evil.test") ; "deceptive_userinfo")]
    #[test_case("http://[::1]:8080/", Some("[::1]") ; "ipv6")]
    #[test_case("mailto:someone", None ; "no_authority")]
    fn the_host_is_found_where_it_is_written(url: &str, host: Option<&str>) {
        assert_eq!(host_range(url).map(|range| &url[range]), host);
    }
}
