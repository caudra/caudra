//! A conversation drawn beside the session's own: a real [`Chat`] in a modal,
//! with an optional composer and a footer that says what the keys do. The
//! window never runs anything. Its owner feeds it events, starts and settles
//! runs, and acts on the [`SideAction`] a key or a click hands back.

use std::mem;
use std::path::Path;
use std::time::{Duration, Instant};

use caudra_agent::{AgentEvent, format_live_duration, format_settled_duration};
use caudra_grab::grab_scope;
use caudra_providers::{Billing, TokenUsage, format_tokens};
use caudra_workbench::text_field::{FieldKind, FieldStyles, TextField, TextKey};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::animation::{live_elapsed, spinner_str};
use crate::chat::{Chat, ChatEventResult};
use crate::components::command::Completion;
use crate::components::input::control_tip;
use crate::components::keybindings::key;
use crate::components::modal::{ESC_LABEL, FooterHits, FooterLine, Modal};
use crate::components::status_bar::short_spend;
use crate::components::tooltip::{Tip, TipKey};
use crate::components::{
    DisplayMessage, DisplayRole, Overlay, counted, field_styles, input_text_style,
};
use crate::repaint::{Cadence, Dirty};
use crate::selection::{Selection, SelectionZone, apply_highlight};
use crate::theme;

const PERCENT: u16 = 90;
/// A chat counts a positive scroll delta upwards, towards older rows.
const LINE_UP: i32 = 1;
/// The spinner and the clock, kept out of the scroll region so the wait is
/// always visible.
const STATUS_ROW: u16 = 1;
/// The input line, kept out of the scroll region so a long thread never
/// scrolls the prompt away.
const INPUT_ROW: u16 = 1;
const FOOTER_ROW: u16 = 1;
const INPUT_PREFIX: &str = "> ";
const COPY_LABEL: &str = "y";
const CTRL_COPY_LABEL: &str = "⌃Y";
const SEND_LABEL: &str = "Enter";
const CLEAR_LABEL: &str = key::CLEAR_BTW.label;
const SCROLL_LABEL: &str = "\u{2191}\u{2193}";
const ENDS_LABEL: &str = "Home/End";
const COPY_HINT: &str = " Copy";
const CLOSE_HINT: &str = " Close";
const OBSERVE_CLOSE_HINT: &str = " back";
const SCROLL_HINT: &str = " scroll";
const ENDS_HINT: &str = " top/bottom";
const SEND_HINT: &str = " Send a follow-up";
/// While an answer is still streaming the key does not send: the thread is
/// one question deep at a time, so what it takes is held until the answer is
/// in.
const QUEUE_HINT: &str = " Queue a follow-up";
const BACK_HINT: &str = " Back to questions";
const COMPACT_BACK_HINT: &str = " Back";
const INITIAL_SEND_HINT: &str = " Send";
const COMPACT_SEND_HINT: &str = " Follow-up";
const COMPACT_QUEUE_HINT: &str = " Queue";
const CLEAR_HINT: &str = " Clear";
const FOOTER_GAP: &str = "   ";
const COMPACT_FOOTER_GAP: &str = " ";
const WAITING_PLACEHOLDER: &str = "waiting for the answer…";
const QUEUED_PREFIX: &str = "queued: ";
const CLARIFICATION_PLACEHOLDER: &str = "Ask about these choices…";
const ASK_PLACEHOLDER: &str = "Ask about this conversation…";
const DRAFT_SEPARATOR: &str = " | ";
const CONTEXT_SUFFIX: &str = " context";
pub(crate) const REQUEST_NOUN: &str = "request";
const TOKENS_OUT: &str = " out";
const SUMMARY_SEPARATOR: &str = " · ";
/// Drawn under the last answer a thread kept from before the conversation it
/// reads moved on, so the reader knows the next answer sees more than it did.
pub(crate) const MOVED_ON_NOTE: &str =
    "The conversation moved on since this answer. The next question reads it as it stands now.";

const TIP_SEND: &str = "Ask the next question in this thread. While an answer is still \
streaming, the question waits until it is in";
const TIP_ASK: &str =
    "Ask about the conversation so far. Only read-only tools run, and nothing enters history";
const TIP_CLARIFY: &str = "Ask about these choices. Nothing is answered or submitted";
const TIP_COPY: &str = "Copy the selection, or the whole answer when nothing is selected";
const TIP_CLOSE: &str = "Close this window";
const TIP_KEEP: &str = "Close this side thread. It is kept until you clear it or the \
session changes, and F2 or /btw reopens it";
const TIP_BACK: &str = "Return to the question form, unchanged";
const TIP_CLEAR: &str = "Forget this side thread and start an empty one. Nothing in it is kept";

const CLOSE_CONTROL: Control = Control::command(ESC_LABEL, CLOSE_HINT, TIP_CLOSE, Command::Close);
const CTRL_COPY_CONTROL: Control =
    Control::command(CTRL_COPY_LABEL, COPY_HINT, TIP_COPY, Command::Copy);
const OBSERVE_FOOTER: [Control; 3] = [
    Control::inert(SCROLL_LABEL, SCROLL_HINT),
    Control::inert(ENDS_LABEL, ENDS_HINT),
    Control::command(ESC_LABEL, OBSERVE_CLOSE_HINT, TIP_CLOSE, Command::Close),
];
const COPY_FOOTER: [Control; 2] = [
    Control::command(COPY_LABEL, COPY_HINT, TIP_COPY, Command::Copy),
    CLOSE_CONTROL,
];
/// Copy needs a chord here: a bare `y` is text the input is owed. Clear comes
/// last, so a narrow footer loses it before the controls a reader needs.
const FOLLOW_UP_FOOTER: [Control; 4] = [
    Control::command(SEND_LABEL, SEND_HINT, TIP_SEND, Command::Send),
    CTRL_COPY_CONTROL,
    Control::command(ESC_LABEL, CLOSE_HINT, TIP_KEEP, Command::Close),
    Control::command(CLEAR_LABEL, CLEAR_HINT, TIP_CLEAR, Command::Clear),
];
const CLARIFY_FOOTER: [Control; 3] = [
    Control::command(ESC_LABEL, BACK_HINT, TIP_BACK, Command::Close),
    Control::command(SEND_LABEL, INITIAL_SEND_HINT, TIP_CLARIFY, Command::Send),
    CTRL_COPY_CONTROL,
];
/// Before the first question there is nothing to copy or follow up.
const ASK_FOOTER: [Control; 2] = [
    Control::command(SEND_LABEL, INITIAL_SEND_HINT, TIP_ASK, Command::Send),
    CLOSE_CONTROL,
];

/// What sits under the transcript, and so what the keys do. Every footer's
/// controls answer a click as they answer their key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SideFooter {
    /// Read only: the keys scroll, Esc closes.
    Observe,
    /// `y` copies the answer; Enter, Space, or Esc closes.
    Copy,
    /// A composer takes the keys: Enter sends a follow-up, or queues it while
    /// a run is live, Esc closes and keeps the thread, the clear chord
    /// forgets it.
    FollowUp,
    /// A composer about a pending question form: Esc goes back to it.
    Clarify,
    /// A composer before the first question.
    Ask,
}

impl SideFooter {
    fn has_composer(self) -> bool {
        matches!(self, Self::FollowUp | Self::Clarify | Self::Ask)
    }

    fn controls(self) -> &'static [Control] {
        match self {
            Self::Observe => &OBSERVE_FOOTER,
            Self::Copy => &COPY_FOOTER,
            Self::FollowUp => &FOLLOW_UP_FOOTER,
            Self::Clarify => &CLARIFY_FOOTER,
            Self::Ask => &ASK_FOOTER,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Copy,
    Send,
    Close,
    Clear,
}

/// One footer control: the key it names, the words that gloss it, what its
/// tooltip says, and what it does. An inert one only explains keys.
struct Control {
    label: &'static str,
    hint: &'static str,
    tip: &'static str,
    command: Option<Command>,
}

impl Control {
    const fn command(
        label: &'static str,
        hint: &'static str,
        tip: &'static str,
        command: Command,
    ) -> Self {
        Self {
            label,
            hint,
            tip,
            command: Some(command),
        }
    }

    const fn inert(label: &'static str, hint: &'static str) -> Self {
        Self {
            label,
            hint,
            tip: "",
            command: None,
        }
    }
}

/// What a key or the pointer asked of the window's owner.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SideAction {
    /// The pointer was outside the window.
    Ignored,
    Consumed,
    /// The window closed itself. What closing means is the owner's call.
    Close,
    /// For the clipboard: the whole answer, or what the composer cut.
    Copy(String),
    /// What a sweep over the transcript covers.
    CopySelection(String),
    /// The question the composer held.
    Submit(String),
    /// Forget the thread the window shows.
    Clear,
    /// Stop the live run and keep the window.
    Interrupt,
}

/// What a run spent, request by request. Every request resends the whole
/// conversation, so their prompts are not added up: the context is the last
/// request's, which is what the status bar's gauge measures.
#[derive(Default)]
struct RunSpend {
    requests: usize,
    context: u32,
    output: u32,
    billed: Option<f64>,
    covered: Option<f64>,
}

impl RunSpend {
    fn record(&mut self, usage: TokenUsage, cost: Option<f64>, billing: Billing) {
        self.requests += 1;
        self.context = usage.context_tokens();
        self.output = self.output.saturating_add(usage.output);
        let total = match billing {
            Billing::Api => &mut self.billed,
            Billing::Subscription => &mut self.covered,
        };
        if let Some(cost) = cost {
            *total = Some(total.unwrap_or_default() + cost);
        }
    }

    /// How long the run took, then its context, requests, output and cost
    /// when any request reported them.
    fn summary(&self, settled: Duration) -> String {
        let mut parts = vec![format_settled_duration(settled)];
        if self.requests > 0 {
            parts.push(format!("{}{CONTEXT_SUFFIX}", format_tokens(self.context)));
            parts.push(counted(self.requests, REQUEST_NOUN));
            parts.push(format!("{}{TOKENS_OUT}", format_tokens(self.output)));
            parts.extend(short_spend(self.billed, self.covered));
        }
        parts.join(SUMMARY_SEPARATOR)
    }
}

pub(crate) struct SideTranscript {
    open: bool,
    title: &'static str,
    footer: SideFooter,
    /// Pinned above the transcript: the first row in the accent, the rest
    /// dimmed.
    header: Vec<String>,
    chat: Chat,
    input: TextField,
    /// Where the composer was drawn, which is where the slash palette opens.
    composer: Rect,
    spend: RunSpend,
    /// A question taken while a run was live, held until the owner can send it.
    queued: Option<String>,
    /// When the live run started, which is also what says one is live.
    started: Option<Instant>,
    footer_hits: FooterHits,
    popup: Rect,
    /// Where the chat was drawn, and whether a press landed in it, so only a
    /// press and release both on the chat click it.
    body: Rect,
    pressed: bool,
    /// The last sweep over the chat. It stays standing after the release
    /// until the next press, so a copy chord can still take it.
    sweep: Option<Selection>,
    sweeping: bool,
}

impl SideTranscript {
    /// A closed window over `chat`.
    pub fn new(title: &'static str, footer: SideFooter, chat: Chat) -> Self {
        Self {
            open: false,
            title,
            footer,
            header: Vec::new(),
            chat,
            input: TextField::new(FieldKind::Line),
            composer: Rect::default(),
            spend: RunSpend::default(),
            queued: None,
            started: None,
            footer_hits: FooterHits::default(),
            popup: Rect::default(),
            body: Rect::default(),
            pressed: false,
            sweep: None,
            sweeping: false,
        }
    }

    pub fn open(&mut self) {
        self.open = true;
    }

    pub fn set_footer(&mut self, footer: SideFooter) {
        self.footer = footer;
        self.footer_hits.reset();
    }

    #[cfg(test)]
    pub fn chat(&self) -> &Chat {
        &self.chat
    }

    pub fn chat_mut(&mut self) -> &mut Chat {
        &mut self.chat
    }

    /// Feeds the chat an event of the run it shows. The events the app routes
    /// itself before a chat sees them are answered here or dropped.
    pub fn handle_event(&mut self, event: AgentEvent) -> ChatEventResult {
        match event {
            AgentEvent::StreamReset => self.chat.stream_reset(),
            event if Chat::routed_before(&event) => {}
            event => return self.chat.handle_event(event, None),
        }
        ChatEventResult::Continue
    }

    /// Pins `header` above the transcript. Dirty only when it changed.
    pub fn set_header(&mut self, header: Vec<String>) -> Dirty {
        if self.header == header {
            return Dirty::NO;
        }
        self.header = header;
        Dirty::YES
    }

    #[cfg(test)]
    pub fn header(&self) -> &[String] {
        &self.header
    }

    /// Starts the status row's clock for a run the owner just started.
    pub fn begin_run(&mut self) {
        self.started = Some(Instant::now());
        self.spend = RunSpend::default();
    }

    /// Counts one request of the live run towards the line under its answer.
    pub fn record_request(&mut self, usage: TokenUsage, cost: Option<f64>, billing: Billing) {
        self.spend.record(usage, cost, billing);
    }

    /// Stops the clock and says under the answer what the run took and cost.
    pub fn finish_run(&mut self) {
        let Some(started) = self.started.take() else {
            return;
        };
        self.chat.flush();
        let summary = self.spend.summary(live_elapsed(started));
        self.push_notice(summary);
    }

    pub fn is_running(&self) -> bool {
        self.started.is_some()
    }

    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.chat
            .push(DisplayMessage::new(DisplayRole::Notice, text.into()));
    }

    /// The question queued behind a live run. Taken once: the owner either
    /// sends it or it is gone.
    pub fn take_queued(&mut self) -> Option<String> {
        self.queued.take()
    }

    pub fn set_draft(&mut self, draft: &str) {
        self.input.set_text(draft);
    }

    #[cfg(test)]
    pub fn take_draft(&mut self) -> String {
        let draft = self.input.text();
        self.input.clear();
        draft
    }

    /// What the composer holds and where its caret is, in chars, while keys
    /// type into it.
    pub fn composer_line(&self) -> Option<(String, usize)> {
        self.text_input_active()
            .then(|| (self.input.text(), self.input.cursor_offset()))
    }

    /// Where the composer was last drawn, while it was.
    pub fn composer_area(&self) -> Option<Rect> {
        (self.text_input_active() && self.composer.width > 0).then_some(self.composer)
    }

    /// Makes the slash palette's edit to the composer.
    pub fn complete(&mut self, completion: &Completion) {
        let text = completion.applied_to(&self.input.text());
        self.input.set_text(&text);
        self.input
            .set_cursor_offset(completion.range.start + completion.caret);
    }

    /// The last answer, as its source.
    pub fn text(&self) -> Option<String> {
        self.chat.last_reply_source()
    }

    /// Whether keys are typing into the composer.
    pub fn text_input_active(&self) -> bool {
        self.open && self.footer.has_composer()
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    pub fn tick(&mut self) -> Dirty {
        match self.open {
            true => self.chat.tick(),
            false => Dirty::NO,
        }
    }

    /// A paste or typed text lands in the composer, flattened to its one line.
    pub fn handle_text(&mut self, text: &str, paste: bool) -> bool {
        if !self.text_input_active() {
            return false;
        }
        self.input.handle_text(text, paste);
        true
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> SideAction {
        let chord = key_event.modifiers.contains(KeyModifiers::CONTROL);
        if chord && key_event.code == KeyCode::Char('y') && self.footer != SideFooter::Observe {
            return self.copy();
        }
        if self.footer.has_composer() {
            return self.handle_composer_key(key_event);
        }
        if chord {
            return match key_event.code {
                KeyCode::Char('c') => self.copy_or_interrupt(),
                _ => SideAction::Consumed,
            };
        }
        match (self.footer, key_event.code) {
            (SideFooter::Observe, KeyCode::Esc | KeyCode::Char('q'))
            | (SideFooter::Copy, KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ')) => {
                self.dismiss()
            }
            (SideFooter::Copy, KeyCode::Char('y')) => self.copy(),
            (_, code) => {
                self.scroll_key(code);
                SideAction::Consumed
            }
        }
    }

    /// The composer owns every key but the ones that move the transcript:
    /// `y` and Space are text here, and `Ctrl+C` copies what is selected in
    /// it before it stops anything.
    fn handle_composer_key(&mut self, key_event: KeyEvent) -> SideAction {
        if key::CLEAR_BTW.matches(key_event) && self.footer == SideFooter::FollowUp {
            return SideAction::Clear;
        }
        match key_event.code {
            KeyCode::Esc => self.dismiss(),
            KeyCode::Enter if self.input.text().trim().is_empty() => self.dismiss(),
            KeyCode::Enter => self.send(),
            code @ (KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown) => {
                self.scroll_key(code);
                SideAction::Consumed
            }
            _ => match self.input.handle_key(key_event) {
                TextKey::Copy(text) | TextKey::Cut(text) => SideAction::Copy(text),
                TextKey::Ignored
                    if key_event.code == KeyCode::Char('c')
                        && key_event.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    self.copy_or_interrupt()
                }
                _ => SideAction::Consumed,
            },
        }
    }

    fn scroll_key(&mut self, code: KeyCode) {
        let chat = &mut self.chat;
        match code {
            KeyCode::Up | KeyCode::Char('k') => chat.scroll(LINE_UP),
            KeyCode::Down | KeyCode::Char('j') => chat.scroll(-LINE_UP),
            KeyCode::PageUp => chat.scroll(chat.half_page()),
            KeyCode::PageDown => chat.scroll(-chat.half_page()),
            KeyCode::Home | KeyCode::Char('g') => chat.scroll_to_top(),
            KeyCode::End | KeyCode::Char('G') => chat.enable_auto_scroll(),
            _ => {}
        }
    }

    /// The typed question, cleared from the composer once taken. One taken
    /// while a run is live is queued instead of sent, so it cannot cancel the
    /// answer it follows up. A send with nothing typed does nothing: a control
    /// that says send must not dismiss.
    fn send(&mut self) -> SideAction {
        let question = self.input.text().trim().to_owned();
        if question.is_empty() {
            return SideAction::Consumed;
        }
        self.input.clear();
        if self.is_running() {
            self.queued = Some(question);
            return SideAction::Consumed;
        }
        SideAction::Submit(question)
    }

    /// The sweep if one stands, the whole answer otherwise.
    fn copy(&self) -> SideAction {
        if let Some(text) = self.swept() {
            return SideAction::CopySelection(text);
        }
        self.text().map_or(SideAction::Consumed, SideAction::Copy)
    }

    /// A standing sweep is what the gesture copies. Without one it stops a
    /// live run, and with nothing live it closes. A window that only
    /// observes has nothing of its own to stop or close this way.
    fn copy_or_interrupt(&mut self) -> SideAction {
        if let Some(text) = self.swept() {
            return SideAction::CopySelection(text);
        }
        match (self.footer, self.is_running()) {
            (SideFooter::Observe, _) => SideAction::Consumed,
            (_, true) => SideAction::Interrupt,
            (_, false) => self.dismiss(),
        }
    }

    fn dismiss(&mut self) -> SideAction {
        self.close();
        SideAction::Close
    }

    fn swept(&self) -> Option<String> {
        let sweep = self.sweep.filter(|sweep| !sweep.is_empty())?;
        let text = self.chat.extract_selection_text(&sweep, self.body);
        (!text.is_empty()).then_some(text)
    }

    /// Puts the question queued behind a live run back in the composer,
    /// ahead of whatever was typed after it, so a run that stops or a window
    /// that closes loses neither and sends nothing on its own.
    pub fn unqueue(&mut self) {
        let Some(queued) = self.queued.take() else {
            return;
        };
        let draft = self.input.text();
        let unsent = if draft.trim().is_empty() || draft.trim() == queued {
            queued
        } else {
            format!("{queued}{DRAFT_SEPARATOR}{draft}")
        };
        self.input.set_text(&unsent);
    }

    /// An armed scroll card under the pointer takes the wheel first, as in
    /// the main transcript, and hands back what it cannot use.
    pub fn scroll_at(&mut self, at: Position, delta: i32) {
        let delta = self.chat.scroll_card_at(at.x, at.y, delta);
        if delta != 0 {
            self.chat.scroll(delta);
        }
    }

    /// The chat answers the pointer as the main transcript does: hovering
    /// marks what a click acts on, a press arms a scroll card, and a click
    /// folds or opens what it lands on. A drag over it sweeps text, copied
    /// when it is let go. A footer control acts as its key does.
    pub fn handle_mouse(&mut self, event: MouseEvent, cwd: &Path) -> SideAction {
        let at = Position::new(event.column, event.row);
        if self.sweeping {
            match event.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    let top = self.chat.scroll_top();
                    if let Some(sweep) = &mut self.sweep {
                        sweep.update(event.row, event.column, top);
                    }
                    return SideAction::Consumed;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.sweeping = false;
                    if let Some(text) = self.swept() {
                        self.pressed = false;
                        self.chat.disarm_card();
                        return SideAction::CopySelection(text);
                    }
                    self.sweep = None;
                }
                _ => {}
            }
        }
        if let Some(index) = self.footer_hits.handle_mouse(event) {
            return self.press(index);
        }
        if self.chat.handle_card_scrollbar(&event) || self.chat.handle_scrollbar(&event) {
            return SideAction::Consumed;
        }
        let body = self.body;
        let on_body = body.contains(at);
        let pressed = mem::replace(
            &mut self.pressed,
            on_body && event.kind == MouseEventKind::Down(MouseButton::Left),
        );
        let chat = &mut self.chat;
        match event.kind {
            MouseEventKind::Moved if on_body => {
                chat.update_hover(event.row, event.column, body, false, cwd);
            }
            MouseEventKind::Moved => {
                chat.clear_hover();
                chat.disarm_card();
            }
            MouseEventKind::Down(MouseButton::Left) if on_body => {
                chat.arm_card_at(event.column, event.row);
                self.sweep = Some(Selection::start(
                    event.row,
                    event.column,
                    body,
                    SelectionZone::Messages,
                    chat.scroll_top(),
                ));
                self.sweeping = true;
                self.input.clear_selection();
            }
            MouseEventKind::Down(_) => self.sweep = None,
            MouseEventKind::Up(MouseButton::Left)
                if pressed && on_body && !chat.armed_card_at(event.column, event.row) =>
            {
                if !chat.toggle_decisions_at(event.column, event.row, body) {
                    chat.handle_click(event.row, body);
                }
            }
            MouseEventKind::ScrollUp => chat.scroll(LINE_UP),
            MouseEventKind::ScrollDown => chat.scroll(-LINE_UP),
            _ => {}
        }
        match self.popup.contains(at) {
            true => SideAction::Consumed,
            false => SideAction::Ignored,
        }
    }

    /// The pressable control at `index` among the ones the footer drew.
    fn pressable(&self, index: usize) -> Option<(&'static Control, Command)> {
        self.footer
            .controls()
            .iter()
            .filter_map(|control| control.command.map(|command| (control, command)))
            .nth(index)
    }

    fn press(&mut self, index: usize) -> SideAction {
        match self.pressable(index).map(|(_, command)| command) {
            Some(Command::Copy) => self.copy(),
            Some(Command::Send) => self.send(),
            Some(Command::Close) => self.dismiss(),
            Some(Command::Clear) => SideAction::Clear,
            None => SideAction::Consumed,
        }
    }

    /// Each control is one phrase, key and gloss together, so the pointer
    /// marks and presses the whole of what it reads. A footer too wide for
    /// its row says less about each control and then drops the last ones, so
    /// the controls that stay are drawn whole.
    fn footer_line(&self, width: u16) -> FooterLine {
        let theme = theme::current();
        let controls = self.footer.controls();
        let roomy: usize = controls
            .iter()
            .enumerate()
            .map(|(index, control)| {
                control.label.width()
                    + self.control_hint(control, false, width).width()
                    + usize::from(index > 0) * FOOTER_GAP.width()
            })
            .sum();
        let compact = self.footer == SideFooter::Clarify || roomy > usize::from(width);
        let gap = match compact {
            true => COMPACT_FOOTER_GAP,
            false => FOOTER_GAP,
        };
        let mut footer = FooterLine::default();
        let mut used = 0;
        for (index, control) in controls.iter().enumerate() {
            let hint = self.control_hint(control, compact, width);
            used += control.label.width() + hint.width() + usize::from(index > 0) * gap.width();
            if compact && used > usize::from(width) {
                break;
            }
            if index > 0 {
                footer.text(gap, theme.tool_dim);
            }
            match control.command {
                Some(_) => {
                    footer.command(control.label, theme.keybind_key);
                    footer.describe(hint, theme.tool_dim);
                }
                None => {
                    footer.text(control.label, theme.keybind_key);
                    footer.text(hint, theme.tool_dim);
                }
            }
        }
        footer
    }

    fn control_hint(&self, control: &Control, compact: bool, width: u16) -> &'static str {
        match control.command {
            Some(Command::Close)
                if self.footer == SideFooter::Clarify
                    && control.label.width() + BACK_HINT.width() > usize::from(width) =>
            {
                COMPACT_BACK_HINT
            }
            Some(Command::Send) if compact && self.is_running() => COMPACT_QUEUE_HINT,
            Some(Command::Send) if compact && self.footer == SideFooter::FollowUp => {
                COMPACT_SEND_HINT
            }
            Some(Command::Send) if self.is_running() => QUEUE_HINT,
            _ => control.hint,
        }
    }

    /// An empty composer says why it is empty: what is already queued, that
    /// the run has to finish first, or what it is for.
    fn placeholder(&self) -> Option<String> {
        if let Some(question) = &self.queued {
            return Some(format!("{QUEUED_PREFIX}{question}"));
        }
        if self.is_running() {
            return Some(WAITING_PLACEHOLDER.to_owned());
        }
        match self.footer {
            SideFooter::Clarify => Some(CLARIFICATION_PLACEHOLDER.to_owned()),
            SideFooter::Ask => Some(ASK_PLACEHOLDER.to_owned()),
            SideFooter::Observe | SideFooter::Copy | SideFooter::FollowUp => None,
        }
    }

    fn input_line(&self, width: u16) -> Line<'static> {
        let theme = theme::current();
        let styles = FieldStyles {
            placeholder: theme.tool_dim,
            ..field_styles(input_text_style())
        };
        let width = usize::from(width).saturating_sub(INPUT_PREFIX.width());
        let placeholder = self.placeholder().unwrap_or_default();
        let mut line = self.input.paint(width, &styles, true, &placeholder);
        line.spans
            .insert(0, Span::styled(INPUT_PREFIX, theme.tool_dim));
        line
    }

    fn chrome_rows(&self) -> u16 {
        u16::from(self.is_running()) * STATUS_ROW
            + u16::from(self.footer.has_composer()) * INPUT_ROW
            + FOOTER_ROW
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let theme = theme::current();
        let lines: Vec<Line> = self
            .header
            .iter()
            .enumerate()
            .map(|(row, text)| {
                let style = match row {
                    0 => theme.accent,
                    _ => theme.tool_dim,
                };
                Line::from(Span::styled(text.clone(), style))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn draw_status(frame: &mut Frame, area: Rect, started: Instant) {
        let theme = theme::current();
        let elapsed = live_elapsed(started);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(spinner_str(elapsed.as_millis()), theme.spinner),
                Span::styled(
                    format!(" {}", format_live_duration(elapsed)),
                    theme.tool_dim,
                ),
            ])),
            area,
        );
    }

    /// The chrome is claimed from the bottom up, so a short window loses
    /// transcript rows rather than the rows that say what is happening.
    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("side_transcript", area);
        let modal = Modal {
            title: self.title,
            width_percent: PERCENT,
            max_height_percent: PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, area.height);
        let header_rows = u16::try_from(self.header.len())
            .unwrap_or(u16::MAX)
            .min(inner.height);
        let body = Rect {
            y: inner.y + header_rows,
            height: inner
                .height
                .saturating_sub(header_rows)
                .saturating_sub(self.chrome_rows()),
            ..inner
        };
        let mut row = body.bottom();
        let mut claim = |height: u16| {
            let taken = Rect {
                y: row,
                height: height.min(inner.bottom().saturating_sub(row)),
                ..inner
            };
            row = taken.bottom();
            taken
        };
        let status = self.started.map(|started| (claim(STATUS_ROW), started));
        let input = self.footer.has_composer().then(|| claim(INPUT_ROW));
        let footer_row = claim(FOOTER_ROW);

        self.draw_header(
            frame,
            Rect {
                height: header_rows,
                ..inner
            },
        );
        if self.chat.message_count() > 0 {
            self.chat.view(frame, body, self.sweeping, false);
        }
        if let Some(sweep) = self.sweep
            && let Some(screen) = sweep.to_screen(self.chat.scroll_top())
        {
            apply_highlight(frame.buffer_mut(), sweep.highlight_area(), &screen);
        }
        if let Some((status, started)) = status {
            Self::draw_status(frame, status, started);
        }
        if let Some(input) = input {
            frame.render_widget(Paragraph::new(self.input_line(input.width)), input);
        }
        self.composer = input.unwrap_or_default();
        let footer = self.footer_line(footer_row.width);
        self.footer_hits.set(footer.hits(footer_row, 0, 1));
        frame.render_widget(
            Paragraph::new(footer.line(self.footer_hits.hovered())),
            footer_row,
        );
        self.body = body;
        self.popup = popup;
        popup
    }

    #[cfg(test)]
    pub(crate) fn input_text(&self) -> String {
        self.input.text()
    }

    #[cfg(test)]
    pub(crate) fn footer_hit(&self, index: usize) -> Rect {
        self.footer_hits.hit(index)
    }
}

impl Overlay for SideTranscript {
    fn is_open(&self) -> bool {
        self.open
    }

    /// Hides the window and keeps what it shows: the chat, and what was left
    /// unsent as the composer's draft.
    fn close(&mut self) {
        self.unqueue();
        self.open = false;
        self.chat.clear_hover();
        self.chat.disarm_card();
        self.footer_hits.reset();
        self.popup = Rect::default();
        self.body = Rect::default();
        self.composer = Rect::default();
        self.pressed = false;
        self.sweep = None;
        self.sweeping = false;
    }

    fn cadence(&self) -> Cadence {
        Cadence::when(
            self.open,
            Cadence::any([
                self.chat.cadence(),
                Cadence::when(self.is_running(), Cadence::SPINNER),
            ]),
        )
    }

    fn tooltip(&self) -> Option<Tip> {
        self.footer_hits
            .tip(TipKey::FooterHint, |index| {
                let (control, _) = self.pressable(index)?;
                Some(control_tip(control.tip, control.label))
            })
            .or_else(|| self.chat.hover_tip())
    }
}

#[cfg(test)]
mod tests {
    use caudra_config::UiConfig;
    use caudra_lua::EventHandle;
    use caudra_providers::NOT_BILLED_MARK;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use test_case::test_case;

    use super::*;
    use crate::components::{buffer_text, key};

    const TITLE: &str = " Side thread ";
    const CHAT_NAME: &str = "side";
    const ANSWER: &str = "The cache key is the session id.";
    const QUESTION: &str = "why";
    const LATER: &str = "and then";
    const SWEPT_CHARS: usize = 9;
    const WIDE: u16 = 100;
    const NARROW: u16 = 40;
    const TINY: u16 = 20;
    const HEIGHT: u16 = 24;
    const FILLER_ROWS: usize = 60;
    const INPUT_TOKENS: u32 = 1_200;
    const PROMPT_TOKENS: u32 = 143_500;
    const OUTPUT_TOKENS: u32 = 340;
    const REQUEST_COST: f64 = 0.02;
    const HEADER_TOP: &str = "Side conversation";
    const HEADER_FORK: &str = "Forked from main";
    const NOT_DRAWN: &str = "the window must draw what it was given";
    const STILL_FOLLOWING: &str = "a scroll key must move the transcript off the bottom";
    const ANSWER_ROW_MISSING: &str = "the answer must be drawn to sweep it";

    fn window(footer: SideFooter) -> SideTranscript {
        let chat = Chat::new(
            CHAT_NAME.to_owned(),
            Path::new("."),
            UiConfig::default(),
            EventHandle::disconnected_for_test(),
        );
        let mut side = SideTranscript::new(TITLE, footer, chat);
        side.open();
        side
    }

    fn answered(footer: SideFooter) -> SideTranscript {
        let mut side = window(footer);
        side.chat_mut().push(DisplayMessage::new(
            DisplayRole::Assistant,
            ANSWER.to_owned(),
        ));
        side
    }

    fn draw(side: &mut SideTranscript, width: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, HEIGHT)).expect("terminal");
        terminal
            .draw(|frame| {
                side.view(frame, frame.area());
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    fn type_text(side: &mut SideTranscript, text: &str) {
        for c in text.chars() {
            side.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn mouse(side: &mut SideTranscript, kind: MouseEventKind, at: Position) -> SideAction {
        let event = MouseEvent {
            kind,
            column: at.x,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        };
        side.handle_mouse(event, Path::new("."))
    }

    fn find(buffer: &Buffer, text: &str) -> Option<Position> {
        let wanted: Vec<String> = text.chars().map(String::from).collect();
        let area = buffer.area;
        (area.top()..area.bottom()).find_map(|y| {
            let cells: Vec<&str> = (area.left()..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            cells
                .windows(wanted.len())
                .position(|window| window == wanted.as_slice())
                .map(|x| Position::new(x as u16, y))
        })
    }

    #[test_case(SideFooter::Observe, "Esc back" ; "observe")]
    #[test_case(SideFooter::Copy, "y Copy" ; "copy")]
    #[test_case(SideFooter::FollowUp, "Enter Send a follow-up" ; "follow_up")]
    #[test_case(SideFooter::Clarify, "Esc Back to questions" ; "clarify")]
    #[test_case(SideFooter::Ask, "Enter Send" ; "ask")]
    fn renders_title_chat_and_footer(footer: SideFooter, control: &str) {
        let mut side = answered(footer);
        let screen = buffer_text(&draw(&mut side, WIDE));
        for drawn in [TITLE.trim(), ANSWER, control] {
            assert!(screen.contains(drawn), "{NOT_DRAWN}: {drawn}");
        }
    }

    #[test_case(SideFooter::FollowUp, NARROW, "Enter Follow-up", "Clear" ; "follow_up_drops_clear")]
    #[test_case(SideFooter::Observe, NARROW, "Esc back", "   Esc" ; "observe_closes_its_gaps")]
    #[test_case(SideFooter::Clarify, NARROW, "Esc Back to questions Enter Send", "Copy" ; "clarify_drops_copy")]
    #[test_case(SideFooter::Clarify, TINY, "Esc Back", "questions" ; "clarify_says_back")]
    fn narrow_footers_say_less(footer: SideFooter, width: u16, shown: &str, dropped: &str) {
        let mut side = answered(footer);
        let screen = buffer_text(&draw(&mut side, width));
        assert!(screen.contains(shown), "{NOT_DRAWN}: {shown}");
        assert!(!screen.contains(dropped), "{dropped} must give way");
    }

    #[test_case(false, " Send a follow-up" ; "idle_sends")]
    #[test_case(true, " Queue a follow-up" ; "running_queues")]
    fn the_send_control_says_what_it_does(running: bool, hint: &str) {
        let mut side = answered(SideFooter::FollowUp);
        if running {
            side.begin_run();
        }
        let screen = buffer_text(&draw(&mut side, WIDE));
        assert!(screen.contains(hint), "{NOT_DRAWN}: {hint}");
    }

    #[test_case(SideFooter::Observe, KeyCode::Char('k') ; "observe_k")]
    #[test_case(SideFooter::Observe, KeyCode::Home ; "observe_home")]
    #[test_case(SideFooter::Copy, KeyCode::PageUp ; "copy_page_up")]
    #[test_case(SideFooter::FollowUp, KeyCode::Up ; "follow_up_up")]
    fn scroll_keys_leave_the_bottom(footer: SideFooter, code: KeyCode) {
        let mut side = window(footer);
        for row in 0..FILLER_ROWS {
            side.chat_mut()
                .push(DisplayMessage::new(DisplayRole::Assistant, row.to_string()));
        }
        draw(&mut side, WIDE);
        assert_eq!(side.handle_key(key(code)), SideAction::Consumed);
        assert!(!side.chat().auto_scroll(), "{STILL_FOLLOWING}");
        side.handle_key(key(KeyCode::End));
        assert_eq!(
            side.chat().auto_scroll(),
            !footer.has_composer(),
            "End follows again only where it is not the composer's"
        );
    }

    #[test_case(SideFooter::Observe, KeyCode::Esc ; "observe_esc")]
    #[test_case(SideFooter::Observe, KeyCode::Char('q') ; "observe_q")]
    #[test_case(SideFooter::Copy, KeyCode::Enter ; "copy_enter")]
    #[test_case(SideFooter::Copy, KeyCode::Char(' ') ; "copy_space")]
    #[test_case(SideFooter::FollowUp, KeyCode::Esc ; "follow_up_esc")]
    #[test_case(SideFooter::FollowUp, KeyCode::Enter ; "follow_up_empty_enter")]
    #[test_case(SideFooter::Clarify, KeyCode::Esc ; "clarify_esc")]
    #[test_case(SideFooter::Ask, KeyCode::Esc ; "ask_esc")]
    fn close_keys_close(footer: SideFooter, code: KeyCode) {
        let mut side = answered(footer);
        assert_eq!(side.handle_key(key(code)), SideAction::Close);
        assert!(!side.is_open());
        assert_eq!(side.chat().last_reply_source().as_deref(), Some(ANSWER));
    }

    #[test]
    fn observe_ignores_typing() {
        let mut side = answered(SideFooter::Observe);
        for key_event in [key(KeyCode::Char('x')), key(KeyCode::Enter), ctrl('c')] {
            assert_eq!(side.handle_key(key_event), SideAction::Consumed);
        }
        assert!(!side.handle_text(QUESTION, true));
        assert!(side.is_open());
        assert!(side.input_text().is_empty());
    }

    #[test_case(SideFooter::FollowUp ; "follow_up")]
    #[test_case(SideFooter::Clarify ; "clarify")]
    #[test_case(SideFooter::Ask ; "ask")]
    fn the_composer_submits_what_was_typed(footer: SideFooter) {
        let mut side = window(footer);
        type_text(&mut side, QUESTION);
        assert_eq!(
            side.handle_key(key(KeyCode::Enter)),
            SideAction::Submit(QUESTION.to_owned())
        );
        assert!(side.input_text().is_empty());
        assert!(side.is_open());
    }

    #[test]
    fn a_set_draft_is_what_enter_sends() {
        let mut side = window(SideFooter::Ask);
        side.set_draft(QUESTION);
        assert_eq!(
            side.handle_key(key(KeyCode::Enter)),
            SideAction::Submit(QUESTION.to_owned())
        );
    }

    #[test]
    fn the_first_answer_turns_ask_into_a_follow_up() {
        let mut side = answered(SideFooter::Ask);
        side.set_footer(SideFooter::FollowUp);
        let screen = buffer_text(&draw(&mut side, WIDE));
        assert!(
            screen.contains(SEND_HINT.trim()),
            "{NOT_DRAWN}: {SEND_HINT}"
        );
        assert_eq!(
            side.handle_key(key::CLEAR_BTW.to_key_event()),
            SideAction::Clear
        );
    }

    #[test]
    fn a_question_during_a_run_is_queued() {
        let mut side = window(SideFooter::FollowUp);
        side.begin_run();
        type_text(&mut side, QUESTION);
        assert_eq!(side.handle_key(key(KeyCode::Enter)), SideAction::Consumed);
        let screen = buffer_text(&draw(&mut side, WIDE));
        assert!(screen.contains(&format!("{QUEUED_PREFIX}{QUESTION}")));
        assert_eq!(side.take_queued().as_deref(), Some(QUESTION));
    }

    #[test]
    fn closing_keeps_what_was_left_unsent() {
        let mut side = window(SideFooter::FollowUp);
        side.begin_run();
        type_text(&mut side, QUESTION);
        side.handle_key(key(KeyCode::Enter));
        type_text(&mut side, LATER);
        assert_eq!(side.handle_key(key(KeyCode::Esc)), SideAction::Close);
        assert_eq!(
            side.take_draft(),
            format!("{QUESTION}{DRAFT_SEPARATOR}{LATER}")
        );
        assert!(side.take_queued().is_none());
    }

    #[test_case(SideFooter::FollowUp, true, SideAction::Interrupt ; "follow_up_running_interrupts")]
    #[test_case(SideFooter::FollowUp, false, SideAction::Close ; "follow_up_idle_closes")]
    #[test_case(SideFooter::Copy, true, SideAction::Interrupt ; "copy_running_interrupts")]
    #[test_case(SideFooter::Observe, true, SideAction::Consumed ; "observe_never_stops_anything")]
    fn ctrl_c_copies_or_interrupts(footer: SideFooter, running: bool, expected: SideAction) {
        let mut side = window(footer);
        if running {
            side.begin_run();
        }
        assert_eq!(side.handle_key(ctrl('c')), expected);
    }

    #[test_case(SideFooter::FollowUp, true ; "follow_up_clears")]
    #[test_case(SideFooter::Ask, false ; "ask_has_nothing_to_clear")]
    fn the_clear_chord_belongs_to_a_follow_up(footer: SideFooter, clears: bool) {
        let mut side = window(footer);
        let action = side.handle_key(key::CLEAR_BTW.to_key_event());
        assert_eq!(action == SideAction::Clear, clears);
    }

    #[test_case(SideFooter::Copy, key(KeyCode::Char('y')) ; "copy_y")]
    #[test_case(SideFooter::FollowUp, ctrl('y') ; "follow_up_ctrl_y")]
    fn copy_takes_the_whole_answer(footer: SideFooter, key_event: KeyEvent) {
        let mut side = answered(footer);
        assert_eq!(
            side.handle_key(key_event),
            SideAction::Copy(ANSWER.to_owned())
        );
    }

    fn request(prompt: u32) -> TokenUsage {
        TokenUsage {
            input: INPUT_TOKENS,
            cache_read: prompt - INPUT_TOKENS,
            output: OUTPUT_TOKENS,
            ..Default::default()
        }
    }

    /// Each request resends the conversation, so the line names the last
    /// request's context once and counts the requests rather than adding
    /// their prompts up.
    #[test_case(1, Billing::Api, "1 request", "$0.02" ; "one_billed_request")]
    #[test_case(3, Billing::Api, "3 requests", "$0.06" ; "three_billed_requests")]
    #[test_case(2, Billing::Subscription, "2 requests", "$0.04" ; "covered_requests")]
    fn finishing_a_run_says_what_it_cost(
        requests: u32,
        billing: Billing,
        shown_requests: &str,
        cost: &str,
    ) {
        let mut side = window(SideFooter::FollowUp);
        side.begin_run();
        for growth in 0..requests {
            side.record_request(request(PROMPT_TOKENS + growth), Some(REQUEST_COST), billing);
        }
        side.finish_run();
        assert!(!side.is_running());
        let note = side.chat().last_message_text();
        let last = request(PROMPT_TOKENS + requests - 1).context_tokens();
        let mark = match billing {
            Billing::Api => "",
            Billing::Subscription => NOT_BILLED_MARK,
        };
        for part in [
            format!("{}{CONTEXT_SUFFIX}", format_tokens(last)),
            shown_requests.to_owned(),
            format!("{}{TOKENS_OUT}", format_tokens(OUTPUT_TOKENS * requests)),
            format!("{mark}{cost}"),
        ] {
            assert!(note.contains(&part), "{note} must name {part}");
        }
        let summed = format_tokens(PROMPT_TOKENS * requests);
        assert!(
            requests == 1 || !note.contains(&summed),
            "{note} must not add the prompts up"
        );
    }

    #[test]
    fn a_run_that_never_reached_the_provider_says_only_how_long_it_took() {
        let mut side = window(SideFooter::FollowUp);
        side.begin_run();
        side.finish_run();
        let note = side.chat().last_message_text();
        assert!(
            !note.contains(SUMMARY_SEPARATOR),
            "{note} must be the duration alone"
        );
    }

    #[test]
    fn the_header_stays_pinned_above_a_scrolled_transcript() {
        let mut side = answered(SideFooter::FollowUp);
        for row in 0..FILLER_ROWS {
            side.push_notice(format!("filler {row}"));
        }
        assert_eq!(
            side.set_header(vec![HEADER_TOP.to_owned(), HEADER_FORK.to_owned()]),
            Dirty::YES
        );
        assert_eq!(
            side.set_header(vec![HEADER_TOP.to_owned(), HEADER_FORK.to_owned()]),
            Dirty::NO
        );
        for code in [KeyCode::End, KeyCode::PageUp] {
            side.handle_key(key(code));
            let rows = rows_under_title(&mut side);
            assert!(rows[0].contains(HEADER_TOP), "{rows:?}");
            assert!(rows[1].contains(HEADER_FORK), "{rows:?}");
        }
    }

    #[test]
    fn an_empty_window_shows_no_start_screen() {
        let mut side = window(SideFooter::Ask);
        let screen = buffer_text(&draw(&mut side, WIDE));
        assert!(!screen.contains(crate::splash::TAGLINE), "{screen}");
    }

    #[test]
    fn no_header_leaves_the_transcript_at_the_top() {
        let mut side = answered(SideFooter::Observe);
        let rows = rows_under_title(&mut side);
        assert!(rows[0].contains(ANSWER), "{rows:?}");
    }

    fn rows_under_title(side: &mut SideTranscript) -> Vec<String> {
        let cells: Vec<char> = buffer_text(&draw(side, WIDE)).chars().collect();
        cells
            .chunks(usize::from(WIDE))
            .map(|row| row.iter().collect::<String>())
            .skip_while(|row| !row.contains(TITLE.trim()))
            .skip(1)
            .collect()
    }

    #[test]
    fn the_footer_answers_a_click() {
        let mut side = answered(SideFooter::Observe);
        draw(&mut side, WIDE);
        let close = side.footer_hit(0).as_position();
        mouse(&mut side, MouseEventKind::Down(MouseButton::Left), close);
        assert_eq!(
            mouse(&mut side, MouseEventKind::Up(MouseButton::Left), close),
            SideAction::Close
        );
        assert!(!side.is_open());
    }

    #[test]
    fn a_sweep_over_the_chat_is_copied() {
        let mut side = answered(SideFooter::Observe);
        let buffer = draw(&mut side, WIDE);
        let start = find(&buffer, ANSWER).expect(ANSWER_ROW_MISSING);
        let end = Position::new(start.x + SWEPT_CHARS as u16, start.y);
        mouse(&mut side, MouseEventKind::Down(MouseButton::Left), start);
        mouse(&mut side, MouseEventKind::Drag(MouseButton::Left), end);
        let SideAction::CopySelection(text) =
            mouse(&mut side, MouseEventKind::Up(MouseButton::Left), end)
        else {
            panic!("a sweep must hand back what it covers");
        };
        assert!(
            !text.is_empty() && ANSWER.starts_with(text.trim()),
            "{text}"
        );
        assert_eq!(
            side.handle_key(ctrl('c')),
            SideAction::CopySelection(text),
            "the sweep stands for the copy chord"
        );
    }
}
