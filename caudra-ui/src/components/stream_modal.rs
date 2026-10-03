use std::time::{Duration, Instant};

use crate::animation::{live_elapsed, spinner_str};
use crate::components::ModalScroll;
use crate::components::Overlay;
use crate::components::modal::{ESC_LABEL, FooterHits, FooterLine, Modal};
use crate::components::prompt_progress::{self, PromptProgress, PromptRate};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::streaming_content::StreamingContent;
use crate::components::{field_styles, input_text_style};
use crate::theme;

use caudra_agent::{CancelTrigger, format_live_duration, format_settled_duration};
use caudra_grab::grab_scope;
use caudra_providers::{Billing, TokenUsage};
use caudra_storage::usage_ledger::LedgerPurpose;
use caudra_workbench::text_field::{FieldKind, FieldStyles, TextField, TextKey};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::repaint::{Cadence, Dirty};

const H_PAD: u16 = 2;
const WIDTH_PERCENT: u16 = 65;
const MAX_HEIGHT_PERCENT: u16 = 80;
pub(crate) const COPY_LABEL: &str = "y";
const CTRL_COPY_LABEL: &str = "⌃Y";
const COPY_HINT: &str = " Copy";
const CLOSE_HINT: &str = " Close";
const FOOTER_GAP: &str = "   ";
const INPUT_PREFIX: &str = "> ";
const SEND_LABEL: &str = "Enter";
const SEND_HINT: &str = " Send a follow-up";
/// While an answer is still streaming the key does not send: the thread is
/// one question deep at a time, so what it takes is held until the answer is
/// in.
const QUEUE_HINT: &str = " Queue a follow-up";
const WAITING_PLACEHOLDER: &str = "waiting for the answer…";
const QUEUED_PREFIX: &str = "queued: ";
/// A stopped answer says so in its own body, where the truncation is, rather
/// than only in the header.
const STOPPED_NOTE: &str = "\n\n_Stopped._";
const TOKENS_IN: &str = " in";
const TOKENS_CACHED: &str = " cached";
const TOKENS_OUT: &str = " out";
const SUFFIX_SEPARATOR: &str = " · ";
/// The spinner, the clock and the prefill bar, kept out of the scroll region
/// so the wait is always visible.
const STATUS_ROW: u16 = 1;
/// The input line, kept out of the scroll region so a long thread never
/// scrolls the prompt away.
const INPUT_ROW: u16 = 1;
const FOOTER_ROW: u16 = 1;

const CLOSE_CONTROL: StreamControl = StreamControl {
    label: ESC_LABEL,
    hint: CLOSE_HINT,
    command: StreamCommand::Close,
};
const CLOSE_FOOTER: [StreamControl; 1] = [CLOSE_CONTROL];
const COPY_FOOTER: [StreamControl; 2] = [
    StreamControl {
        label: COPY_LABEL,
        hint: COPY_HINT,
        command: StreamCommand::Copy,
    },
    CLOSE_CONTROL,
];
/// Copy needs a chord here: a bare `y` is text the input is owed.
const FOLLOW_UP_FOOTER: [StreamControl; 3] = [
    StreamControl {
        label: SEND_LABEL,
        hint: SEND_HINT,
        command: StreamCommand::Send,
    },
    StreamControl {
        label: CTRL_COPY_LABEL,
        hint: COPY_HINT,
        command: StreamCommand::Copy,
    },
    CLOSE_CONTROL,
];

/// What a finished side request cost. It reaches the session ledger through
/// [`StreamModal::take_done`] rather than the agent event channel, because a
/// side request never produces a turn or a chat bubble to hang it on.
pub struct StreamUsage {
    pub usage: TokenUsage,
    pub cost: Option<f64>,
    pub billing: Billing,
    pub model: String,
    pub provider: String,
    pub purpose: LedgerPurpose,
}

pub struct StreamDone {
    pub usage: StreamUsage,
    /// The answer as the model returned it, for a caller that continues the
    /// thread. The body is not a substitute: the reveal may also have shown
    /// reasoning, which the next request must not quote back as the answer.
    pub answer: Option<String>,
}

pub enum StreamEvent {
    /// Replaces the header once the request knows something it did not at
    /// open time, such as which model answered.
    Header(String),
    /// How much of the prompt the server has prefilled. Only providers that
    /// report it send this, so it is the bar's only source and never a
    /// guarantee that a bar appears.
    Progress(PromptProgress),
    TextDelta(String),
    ThinkingDelta(String),
    Done(StreamDone),
    Error(String),
}

/// What sits under the answer, and so what the keys do. Every footer's
/// controls answer a click as they answer their key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamFooter {
    /// Enter, Space, or Esc dismisses.
    Close,
    /// `y` copies the text; Enter, Space, or Esc dismisses.
    Copy,
    /// A single-line input takes the keys: Enter sends what was typed as the
    /// next question in the same thread, Esc closes, an empty Enter closes. A
    /// click on `Send` with nothing typed does nothing: a control that says
    /// send must not dismiss.
    FollowUp,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StreamCommand {
    Copy,
    Send,
    Close,
}

/// One footer control: the key it names, the words that gloss it, and what it
/// does. The footer is built from these and a click resolves through them, so
/// a control's index is never magic.
struct StreamControl {
    label: &'static str,
    hint: &'static str,
    command: StreamCommand,
}

/// What a key or the pointer asked of the host. `Copy` carries the text for
/// the clipboard, `Submit` the follow-up the input held. A key is never
/// `Ignored`: the modal owns the keyboard while it is up.
pub enum StreamAction {
    Ignored,
    Consumed,
    Copy(String),
    Submit(String),
}

/// How an exchange ended, which is what its body is painted as.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExchangeOutcome {
    Live,
    Done,
    Failed,
    Stopped,
}

#[derive(PartialEq, Eq)]
enum SectionKind {
    Text,
    Thinking,
    Error,
}

struct StreamSection {
    kind: SectionKind,
    body: StreamingContent,
}

/// One question and the answer streaming under it.
struct Exchange {
    header: String,
    sections: Vec<StreamSection>,
    started_at: Instant,
    /// How long the answer took, once it is in. `None` while it is still
    /// coming, which is also what says the live clock belongs to this one.
    settled: Option<Duration>,
    usage: Option<TokenUsage>,
    outcome: ExchangeOutcome,
}

impl Exchange {
    fn new(header: String) -> Self {
        Self {
            header,
            sections: Vec::new(),
            started_at: Instant::now(),
            settled: None,
            usage: None,
            outcome: ExchangeOutcome::Live,
        }
    }

    fn push(&mut self, kind: SectionKind, text: &str, ms_per_char: u64) {
        if text.is_empty() {
            return;
        }
        if let Some(section) = self.sections.last_mut().filter(|s| s.kind == kind) {
            section.body.push(text);
            return;
        }
        let theme = theme::current();
        let style = match kind {
            SectionKind::Text => theme.assistant,
            SectionKind::Thinking => theme.thinking,
            SectionKind::Error => theme.error,
        };
        let mut body = StreamingContent::new_noninteractive("", style, style, ms_per_char);
        body.push(text);
        self.sections.push(StreamSection { kind, body });
    }

    fn text(&self) -> String {
        self.sections
            .iter()
            .filter(|s| s.kind != SectionKind::Thinking)
            .map(|s| s.body.buffer())
            .collect()
    }

    fn settle(&mut self, outcome: ExchangeOutcome) {
        self.settled = Some(live_elapsed(self.started_at));
        self.outcome = outcome;
    }

    /// What the exchange cost, appended to its question once the answer is in.
    /// A reader scrolling back wants the price of the answer beside the
    /// question that bought it.
    fn suffix(&self) -> String {
        let Some(settled) = self.settled else {
            return String::new();
        };
        let mut suffix = format!("{SUFFIX_SEPARATOR}{}", format_settled_duration(settled));
        let Some(usage) = self.usage else {
            return suffix;
        };
        let input =
            u64::from(usage.input) + u64::from(usage.cache_read) + u64::from(usage.cache_creation);
        suffix.push_str(&format!(
            "{SUFFIX_SEPARATOR}{}{TOKENS_IN}",
            super::format_compact(input)
        ));
        if usage.cache_read > 0 {
            suffix.push_str(&format!(
                "{SUFFIX_SEPARATOR}{}{TOKENS_CACHED}",
                super::format_compact(u64::from(usage.cache_read))
            ));
        }
        suffix.push_str(&format!(
            "{SUFFIX_SEPARATOR}{}{TOKENS_OUT}",
            super::format_compact(u64::from(usage.output))
        ));
        suffix
    }
}

/// A modal that draws model answers as they stream: `/btw` and `/extract`
/// both open it, with their own title, header, and footer. A thread of
/// exchanges grows downwards; only the last one is ever live.
pub struct StreamModal {
    open: bool,
    title: &'static str,
    footer: StreamFooter,
    exchanges: Vec<Exchange>,
    ms_per_char: u64,
    input: TextField,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    footer_hits: FooterHits,
    rx: Option<flume::Receiver<StreamEvent>>,
    /// Dropping this cancels the in-flight request, so every teardown path that
    /// already funnels through [`StreamModal::close`] cancels for free.
    cancel: Option<CancelTrigger>,
    pending_done: Option<StreamDone>,
    /// How far the live request's prompt has been prefilled, while that is
    /// still short of the whole prompt.
    progress: Option<PromptProgress>,
    rate: PromptRate,
    /// A follow-up typed while the answer was still streaming, held until the
    /// host can extend the thread with it.
    queued: Option<String>,
    popup: Rect,
}

impl StreamModal {
    pub fn new(ms_per_char: u64) -> Self {
        Self {
            open: false,
            title: "",
            footer: StreamFooter::Close,
            exchanges: Vec::new(),
            ms_per_char,
            input: TextField::new(FieldKind::Line),
            scroll: ModalScroll::new(),
            scrollbar: Scrollbar::default(),
            footer_hits: FooterHits::default(),
            rx: None,
            cancel: None,
            pending_done: None,
            progress: None,
            rate: PromptRate::default(),
            queued: None,
            popup: Rect::default(),
        }
    }

    pub fn open(
        &mut self,
        title: &'static str,
        header: String,
        footer: StreamFooter,
        rx: flume::Receiver<StreamEvent>,
        cancel: CancelTrigger,
    ) {
        self.close();
        self.open = true;
        self.title = title;
        self.footer = footer;
        self.begin_exchange(header, rx, cancel);
    }

    /// Appends the next question to the thread and streams its answer under
    /// the ones before it. Replacing the trigger stops whatever was still in
    /// flight, so a caller that wants the previous answer waits for it first.
    pub fn begin_exchange(
        &mut self,
        header: String,
        rx: flume::Receiver<StreamEvent>,
        cancel: CancelTrigger,
    ) {
        self.exchanges.push(Exchange::new(header));
        self.rx = Some(rx);
        self.cancel = Some(cancel);
        self.progress = None;
        self.rate.reset();
        self.scroll.scroll_to(u16::MAX);
    }

    /// Leaves `pending_done` alone: a call that finished just before the user
    /// dismissed the modal still has to be billed.
    pub fn close(&mut self) {
        self.open = false;
        self.exchanges.clear();
        self.input.clear();
        self.scroll.reset();
        self.footer_hits.reset();
        self.queued = None;
        self.finish_stream();
    }

    pub fn take_done(&mut self) -> Option<StreamDone> {
        self.pending_done.take()
    }

    /// The follow-up typed while the last answer was still streaming. Taken
    /// once: the host either extends the thread with it or it is gone.
    pub fn take_queued(&mut self) -> Option<String> {
        self.queued.take()
    }

    pub fn is_streaming(&self) -> bool {
        self.rx.is_some()
    }

    /// The spinner and the clock carry the wait before the first token, which
    /// is exactly the window the typewriter cannot: it has nothing to reveal
    /// yet.
    pub fn cadence(&self) -> Cadence {
        Cadence::when(
            self.open,
            Cadence::any([
                Cadence::when(self.is_streaming(), Cadence::SPINNER),
                Cadence::when(
                    self.exchanges
                        .iter()
                        .flat_map(|e| &e.sections)
                        .any(|s| s.body.is_animating()),
                    Cadence::SMOOTH,
                ),
            ]),
        )
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    /// Everything the live answer streamed so far, whether or not the reveal
    /// has drawn it.
    pub fn text(&self) -> String {
        self.exchanges
            .last()
            .map_or_else(String::new, Exchange::text)
    }

    pub fn poll(&mut self) -> Dirty {
        let Some(ref rx) = self.rx else {
            return Dirty::NO;
        };
        let Some(live) = self.exchanges.last_mut() else {
            return Dirty::NO;
        };
        let mut dirty = Dirty::NO;
        let mut finished = false;
        while let Ok(event) = rx.try_recv() {
            dirty = Dirty::YES;
            match event {
                StreamEvent::Header(header) => live.header = header,
                StreamEvent::Progress(progress) => {
                    self.rate.sample(progress.processed, Instant::now());
                    self.progress = (progress.processed < progress.total).then_some(progress);
                }
                StreamEvent::TextDelta(text) => {
                    // Text means the prefill is over, whatever the last count
                    // said; a bar left part-full outlives what it measured.
                    self.progress = None;
                    live.push(SectionKind::Text, &text, self.ms_per_char);
                }
                StreamEvent::ThinkingDelta(text) => {
                    self.progress = None;
                    live.push(SectionKind::Thinking, &text, self.ms_per_char);
                }
                StreamEvent::Done(done) => {
                    live.usage = Some(done.usage.usage);
                    live.settle(ExchangeOutcome::Done);
                    self.pending_done = Some(done);
                    finished = true;
                    break;
                }
                StreamEvent::Error(msg) => {
                    live.sections.clear();
                    live.push(SectionKind::Error, &msg, self.ms_per_char);
                    live.settle(ExchangeOutcome::Failed);
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            self.finish_stream();
        }
        dirty
    }

    /// Retires the cancel trigger alongside the receiver so a finished stream
    /// never leaves a live trigger behind, and drops the prefill the trigger
    /// was measuring.
    fn finish_stream(&mut self) {
        self.rx = None;
        self.cancel = None;
        self.progress = None;
        self.rate.reset();
    }

    /// Abandons the answer and keeps everything around it: the modal stays up,
    /// the thread keeps its earlier exchanges, and the stopped one says where
    /// it was cut. Dropping the trigger cancels the request.
    fn stop(&mut self) {
        if let Some(live) = self.exchanges.last_mut() {
            live.push(SectionKind::Text, STOPPED_NOTE, self.ms_per_char);
            live.settle(ExchangeOutcome::Stopped);
        }
        self.finish_stream();
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    /// The bar and the footer controls are all the modal reads from the pointer.
    pub fn handle_mouse(&mut self, event: &MouseEvent) -> StreamAction {
        match self.scrollbar.handle(event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return StreamAction::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return StreamAction::Consumed;
            }
        }
        let Some(index) = self.footer_hits.handle_mouse(*event) else {
            return StreamAction::Ignored;
        };
        match self.controls()[index].command {
            StreamCommand::Copy => StreamAction::Copy(self.text()),
            StreamCommand::Send => self
                .take_question()
                .map_or(StreamAction::Consumed, StreamAction::Submit),
            StreamCommand::Close => {
                self.close();
                StreamAction::Consumed
            }
        }
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> StreamAction {
        let chord = key_event.modifiers.contains(KeyModifiers::CONTROL);
        if chord && key_event.code == KeyCode::Char('y') {
            return StreamAction::Copy(self.text());
        }
        if self.footer == StreamFooter::FollowUp {
            return self.handle_follow_up_key(key_event);
        }
        if chord {
            if key_event.code == KeyCode::Char('c') {
                self.interrupt();
            }
            return StreamAction::Consumed;
        }
        match key_event.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') => self.close(),
            KeyCode::Char('y') if self.footer == StreamFooter::Copy => {
                return StreamAction::Copy(self.text());
            }
            _ => {
                self.scroll.handle_key(key_event);
            }
        }
        StreamAction::Consumed
    }

    /// The input owns every key but the ones that move the thread: `y` and
    /// Space are text here, and `Ctrl+C` copies what is selected in it before
    /// it stops anything.
    fn handle_follow_up_key(&mut self, key_event: KeyEvent) -> StreamAction {
        match key_event.code {
            KeyCode::Esc => self.close(),
            KeyCode::Enter => {
                if self.input.text().trim().is_empty() {
                    self.close();
                } else if let Some(question) = self.take_question() {
                    return StreamAction::Submit(question);
                }
            }
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                self.scroll.handle_key(key_event);
            }
            _ => match self.input.handle_key(key_event) {
                TextKey::Copy(text) | TextKey::Cut(text) => return StreamAction::Copy(text),
                TextKey::Ignored
                    if key_event.code == KeyCode::Char('c')
                        && key_event.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    self.interrupt();
                }
                _ => {}
            },
        }
        StreamAction::Consumed
    }

    /// The global gesture stops what is running, so here it stops the answer
    /// and leaves the thread that asked for it. With nothing running it
    /// dismisses.
    fn interrupt(&mut self) {
        match self.is_streaming() {
            true => self.stop(),
            false => self.close(),
        }
    }

    /// Whether keys are typing into the follow-up input.
    pub fn text_input_active(&self) -> bool {
        self.open && self.footer == StreamFooter::FollowUp
    }

    /// The typed follow-up, cleared from the input once taken. A question
    /// asked while the answer is still streaming is queued instead of sent,
    /// so it cannot cancel the answer it is following up.
    fn take_question(&mut self) -> Option<String> {
        let question = self.input.text().trim().to_owned();
        if question.is_empty() {
            return None;
        }
        self.input.clear();
        if self.is_streaming() {
            self.queued = Some(question);
            return None;
        }
        Some(question)
    }

    fn controls(&self) -> &'static [StreamControl] {
        match self.footer {
            StreamFooter::Close => &CLOSE_FOOTER,
            StreamFooter::Copy => &COPY_FOOTER,
            StreamFooter::FollowUp => &FOLLOW_UP_FOOTER,
        }
    }

    /// Each control is one phrase, key and gloss together, so the pointer
    /// marks and presses the whole of what it reads. The send control says
    /// which of the two things it does, because mid-stream it queues.
    fn footer_line(&self) -> FooterLine {
        let theme = theme::current();
        let mut footer = FooterLine::default();
        for (index, control) in self.controls().iter().enumerate() {
            if index > 0 {
                footer.text(FOOTER_GAP, theme.tool_dim);
            }
            let hint = match control.command {
                StreamCommand::Send if self.is_streaming() => QUEUE_HINT,
                _ => control.hint,
            };
            footer.command(control.label, theme.keybind_key);
            footer.describe(hint, theme.tool_dim);
        }
        footer
    }

    /// A paste lands in the input, flattened to the one line it has.
    pub fn handle_paste(&mut self, text: &str) -> bool {
        if !self.text_input_active() {
            return false;
        }
        self.input.paste(text);
        true
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("stream_modal", area);

        let theme = theme::current();
        let padded_width = Modal::inner_width(area.width, WIDTH_PERCENT).saturating_sub(H_PAD * 2);

        let mut lines: Vec<Line> = Vec::new();
        for (index, exchange) in self.exchanges.iter_mut().enumerate() {
            if index > 0 {
                lines.push(Line::default());
            }
            lines.push(Line::from(vec![
                Span::styled(exchange.header.clone(), theme.tool_dim),
                Span::styled(exchange.suffix(), theme.tool_dim),
            ]));
            lines.push(Line::default());
            for (section_index, section) in exchange.sections.iter_mut().enumerate() {
                if section_index > 0 {
                    lines.push(Line::default());
                }
                for line in section.body.render_lines(padded_width) {
                    let mut line = line.clone();
                    if section.kind == SectionKind::Thinking {
                        for span in &mut line.spans {
                            span.style = span.style.patch(theme.thinking);
                        }
                    }
                    lines.push(line);
                }
            }
        }

        let chrome = self.chrome_rows();
        let total = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(padded_width) as u16;
        let modal = Modal {
            title: self.title,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, total.saturating_add(chrome));
        let padded = Rect {
            x: inner.x + H_PAD,
            width: inner.width.saturating_sub(H_PAD * 2),
            ..inner
        };
        // The chrome is claimed from the bottom up, so a modal clamped to the
        // height cap loses transcript rows rather than the rows that say what
        // is happening.
        let viewport = Rect {
            height: padded.height.saturating_sub(chrome.min(padded.height)),
            ..padded
        };
        let mut row = viewport.bottom();
        let mut claim = |height: u16| {
            let taken = Rect {
                y: row,
                height: height.min(padded.bottom().saturating_sub(row)),
                ..padded
            };
            row = taken.bottom();
            taken
        };
        let status = self.is_streaming().then(|| claim(STATUS_ROW));
        let input = (self.footer == StreamFooter::FollowUp).then(|| claim(INPUT_ROW));
        let footer_row = claim(FOOTER_ROW);

        self.scroll.update_dimensions(total, viewport.height);
        let scroll = self.scroll.offset();
        let paragraph = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        frame.render_widget(paragraph, viewport);

        self.scrollbar.draw(
            frame,
            Rect {
                height: viewport.height,
                ..inner
            },
            total,
            scroll,
        );

        if let Some(status) = status {
            self.draw_status(frame, status);
        }
        if let Some(input) = input {
            frame.render_widget(Paragraph::new(self.input_line(input.width)), input);
        }
        let footer = self.footer_line();
        self.footer_hits.set(footer.hits(footer_row, 0, 1));
        frame.render_widget(
            Paragraph::new(footer.line(self.footer_hits.hovered())),
            footer_row,
        );

        self.popup = popup;
        popup
    }

    fn chrome_rows(&self) -> u16 {
        u16::from(self.is_streaming()) * STATUS_ROW
            + u16::from(self.footer == StreamFooter::FollowUp) * INPUT_ROW
            + FOOTER_ROW
    }

    /// The spinner and the clock on the left, the prefill bar on the right.
    /// Only providers that report prefill draw the bar; the rest are carried
    /// by the clock alone.
    fn draw_status(&mut self, frame: &mut Frame, area: Rect) {
        let theme = theme::current();
        let Some(elapsed) = self.exchanges.last().map(|e| live_elapsed(e.started_at)) else {
            return;
        };
        grab_scope!("stream_modal_status", area);
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
        if let Some(progress) = self.progress {
            prompt_progress::render(frame, area, progress, &self.rate);
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

    /// An empty input says why it is empty: what is already queued, or that
    /// the thread is waiting on the answer before it takes the next question.
    fn placeholder(&self) -> Option<String> {
        match &self.queued {
            Some(question) => Some(format!("{QUEUED_PREFIX}{question}")),
            None => self.is_streaming().then(|| WAITING_PLACEHOLDER.to_owned()),
        }
    }

    #[cfg(test)]
    pub fn body_eq(&self, expected: &str) -> bool {
        self.exchanges.last().is_some_and(|e| e.text() == expected)
    }

    #[cfg(test)]
    pub(crate) fn footer_hit(&self, index: usize) -> Rect {
        self.footer_hits.hit(index)
    }

    #[cfg(test)]
    pub(crate) fn headers(&self) -> Vec<&str> {
        self.exchanges.iter().map(|e| e.header.as_str()).collect()
    }

    #[cfg(test)]
    pub(crate) fn input_text(&self) -> String {
        self.input.text()
    }
}

impl Overlay for StreamModal {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::animation::test_clock::FrozenClock;
    use crate::components::key as key_ev;
    use crate::components::prompt_progress::PROMPT_PROGRESS_LABEL;
    use caudra_agent::CancelToken;
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::{Modifier, Style};
    use test_case::test_case;

    const COPY: usize = 0;
    const SEND: usize = 0;
    const CLOSE: usize = 1;
    const FOLLOW_UP_COPY: usize = 1;
    const FOLLOW_UP_CLOSE: usize = 2;
    const HOVER_MISSED: &str = "the footer control must reverse under the pointer as one phrase";
    const STATUS_MISSING: &str = "a waiting request has to show that it is waiting";
    const STATUS_LINGERED: &str = "a settled request has nothing left to report";
    const WIDTH: u16 = 80;
    const HEIGHT: u16 = 24;
    const MODEL: &str = "test-model";
    const PROVIDER: &str = "anthropic";
    const TITLE: &str = " /btw ";
    const HEADER: &str = "Q: why?";
    const FOLLOW_UP: &str = "Q: and then?";
    const ANSWER: &str = "because";
    const THINKING: &str = "weighing options";
    const RECONSIDERING: &str = "checking assumptions";
    const ERROR: &str = "the request failed";
    const CONTENT_MISSING: &str = "the streamed section must be visible";
    const STYLE_MISMATCH: &str = "the section must retain its own style";
    const REVEAL_MS_PER_CHAR: u64 = 20;

    fn open_modal(
        m: &mut StreamModal,
        header: &str,
        footer: StreamFooter,
    ) -> (flume::Sender<StreamEvent>, CancelToken) {
        let (tx, rx) = flume::bounded(64);
        let (trigger, cancel) = CancelToken::new();
        m.open(TITLE, header.to_owned(), footer, rx, trigger);
        (tx, cancel)
    }

    fn follow_up(m: &mut StreamModal, header: &str) -> (flume::Sender<StreamEvent>, CancelToken) {
        let (tx, rx) = flume::bounded(64);
        let (trigger, cancel) = CancelToken::new();
        m.begin_exchange(header.to_owned(), rx, trigger);
        (tx, cancel)
    }

    fn done() -> StreamEvent {
        StreamEvent::Done(StreamDone {
            usage: StreamUsage {
                usage: TokenUsage {
                    input: 10,
                    output: 20,
                    ..Default::default()
                },
                cost: Some(0.5),
                billing: Billing::Api,
                provider: PROVIDER.into(),
                model: MODEL.into(),
                purpose: LedgerPurpose::Btw,
            },
            answer: Some(ANSWER.into()),
        })
    }

    fn type_text(m: &mut StreamModal, text: &str) {
        for c in text.chars() {
            m.handle_key(key_ev(KeyCode::Char(c)));
        }
    }

    fn draw(m: &mut StreamModal, terminal: &mut Terminal<TestBackend>) {
        terminal
            .draw(|frame| {
                m.view(frame, frame.area());
            })
            .unwrap();
    }

    fn delta(thinking: bool, text: &str) -> StreamEvent {
        if thinking {
            StreamEvent::ThinkingDelta(text.into())
        } else {
            StreamEvent::TextDelta(text.into())
        }
    }

    fn assert_text_style(terminal: &Terminal<TestBackend>, text: &str, style: Style) -> Position {
        let buffer = terminal.backend().buffer();
        let position = buffer
            .area
            .positions()
            .find(|position| {
                text.chars().enumerate().all(|(offset, ch)| {
                    let x = position.x + offset as u16;
                    x < buffer.area.right() && buffer[(x, position.y)].symbol() == ch.to_string()
                })
            })
            .expect(CONTENT_MISSING);
        for offset in 0..text.chars().count() as u16 {
            let cell = &buffer[(position.x + offset, position.y)];
            assert_eq!(Some(cell.fg), style.fg, "{STYLE_MISMATCH}");
            assert_eq!(cell.modifier, style.add_modifier, "{STYLE_MISMATCH}");
        }
        position
    }

    fn mouse(kind: MouseEventKind, at: Rect) -> MouseEvent {
        MouseEvent {
            kind,
            column: at.x,
            row: at.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click(m: &mut StreamModal, at: Rect) -> StreamAction {
        m.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), at));
        m.handle_mouse(&mouse(MouseEventKind::Up(MouseButton::Left), at))
    }

    fn reversed_cells(terminal: &Terminal<TestBackend>) -> Vec<Position> {
        let buffer = terminal.backend().buffer();
        buffer
            .area
            .positions()
            .filter(|position| {
                buffer[(position.x, position.y)]
                    .modifier
                    .contains(Modifier::REVERSED)
            })
            .collect()
    }

    #[test]
    fn open_sets_header_and_state() {
        let mut m = StreamModal::new(0);
        let (_tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::Close);
        assert!(m.is_open());
        assert_eq!(m.headers(), [HEADER]);
        assert!(m.body_eq(""));
        assert!(m.is_streaming());
    }

    #[test]
    fn close_resets_all_fields() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", StreamFooter::FollowUp);
        tx.send(StreamEvent::TextDelta("some answer".into()))
            .unwrap();
        let _ = m.poll();
        type_text(&mut m, "draft");
        m.scroll.update_dimensions(100, 10);
        m.scroll.scroll(-5);
        m.close();
        assert!(!m.is_open());
        assert!(m.headers().is_empty());
        assert_eq!(m.text(), "");
        assert_eq!(m.input_text(), "");
        assert_eq!(m.scroll.offset(), 0);
        assert!(!m.is_streaming());
    }

    #[test]
    fn poll_accumulates_text() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", StreamFooter::Close);
        tx.send(StreamEvent::TextDelta("hello ".into())).unwrap();
        tx.send(StreamEvent::TextDelta("world".into())).unwrap();
        let _ = m.poll();
        assert!(m.body_eq("hello world"));
        assert_eq!(m.text(), "hello world");
    }

    #[test_case(false, false ; "answer_live")]
    #[test_case(false, true ; "answer_settled")]
    #[test_case(true, false ; "thinking_live")]
    #[test_case(true, true ; "thinking_settled")]
    fn generated_sections_use_their_role_style(thinking: bool, settled: bool) {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::Close);
        tx.send(delta(thinking, ANSWER)).unwrap();
        if settled {
            tx.send(done()).unwrap();
        }
        let _ = m.poll();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        let theme = theme::current();
        assert_text_style(
            &terminal,
            ANSWER,
            if thinking {
                theme.thinking
            } else {
                theme.assistant
            },
        );
    }

    #[test_case(false ; "live")]
    #[test_case(true ; "settled")]
    fn interleaved_sections_keep_their_order_and_style(settled: bool) {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::Close);
        for event in [
            StreamEvent::ThinkingDelta(THINKING.into()),
            StreamEvent::TextDelta(ANSWER.into()),
            StreamEvent::ThinkingDelta(RECONSIDERING.into()),
        ] {
            tx.send(event).unwrap();
        }
        if settled {
            tx.send(done()).unwrap();
        }
        let _ = m.poll();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        let theme = theme::current();
        let thinking = assert_text_style(&terminal, THINKING, theme.thinking);
        let answer = assert_text_style(&terminal, ANSWER, theme.assistant);
        let reconsidering = assert_text_style(&terminal, RECONSIDERING, theme.thinking);
        assert!(answer.y > thinking.y + 1);
        assert!(reconsidering.y > answer.y + 1);
        assert_eq!(m.text(), ANSWER);
    }

    #[test_case(false ; "answer")]
    #[test_case(true ; "thinking")]
    fn same_kind_chunks_share_markdown_without_empty_sections(thinking: bool) {
        const OPEN: &str = "**because";
        const CLOSE: &str = "**";
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::Close);
        tx.send(delta(!thinking, "")).unwrap();
        tx.send(delta(thinking, OPEN)).unwrap();
        tx.send(delta(!thinking, "")).unwrap();
        tx.send(delta(thinking, CLOSE)).unwrap();
        let _ = m.poll();
        assert_eq!(m.exchanges[0].sections.len(), 1);
        assert_eq!(
            m.exchanges[0].sections[0].body.buffer(),
            format!("{OPEN}{CLOSE}")
        );
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        let theme = theme::current();
        let style = if thinking {
            theme.bold.patch(theme.thinking)
        } else {
            theme.assistant.patch(theme.bold)
        };
        assert_text_style(&terminal, ANSWER, style);
    }

    #[test_case("# because", true ; "heading")]
    #[test_case("**because**", true ; "bold")]
    #[test_case("_because_", false ; "italic")]
    #[test_case("```rust\nbecause\n```", false ; "highlighted_code")]
    fn thinking_markdown_keeps_the_thinking_style(source: &str, bold: bool) {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::Close);
        tx.send(StreamEvent::ThinkingDelta(source.into())).unwrap();
        tx.send(StreamEvent::TextDelta(RECONSIDERING.into()))
            .unwrap();
        let _ = m.poll();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        let theme = theme::current();
        let style = if bold {
            theme.thinking.add_modifier(Modifier::BOLD)
        } else {
            theme.thinking
        };
        for settled in [false, true] {
            if settled {
                tx.send(done()).unwrap();
                let _ = m.poll();
            }
            draw(&mut m, &mut terminal);
            assert_text_style(&terminal, ANSWER, style);
            assert_text_style(&terminal, RECONSIDERING, theme.assistant);
        }
    }

    #[test_case("```\nweighing options" ; "open_code_fence")]
    #[test_case("**weighing options" ; "open_emphasis")]
    fn reasoning_markdown_cannot_consume_the_answer(thinking: &str) {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::Close);
        tx.send(StreamEvent::ThinkingDelta(thinking.into()))
            .unwrap();
        tx.send(StreamEvent::TextDelta(ANSWER.into())).unwrap();
        let _ = m.poll();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        assert_text_style(&terminal, ANSWER, theme::current().assistant);
    }

    #[test_case(false, false ; "thinking_only_live")]
    #[test_case(true, false ; "thinking_only_settled")]
    #[test_case(false, true ; "answer_live")]
    #[test_case(true, true ; "answer_settled")]
    fn copying_excludes_reasoning_and_preserves_unrevealed_answer_text(
        settled: bool,
        with_answer: bool,
    ) {
        const ANSWER_PARTS: [&str; 2] = ["**be", "cause**"];
        let mut m = StreamModal::new(REVEAL_MS_PER_CHAR);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        tx.send(StreamEvent::ThinkingDelta(THINKING.into()))
            .unwrap();
        if with_answer {
            tx.send(StreamEvent::TextDelta(ANSWER_PARTS[0].into()))
                .unwrap();
            tx.send(StreamEvent::ThinkingDelta(RECONSIDERING.into()))
                .unwrap();
            tx.send(StreamEvent::TextDelta(ANSWER_PARTS[1].into()))
                .unwrap();
        }
        if settled {
            tx.send(done()).unwrap();
        }
        let _ = m.poll();
        assert!(
            m.exchanges[0]
                .sections
                .iter()
                .all(|s| s.body.visible().is_empty())
        );
        let expected = if with_answer {
            format!("**{ANSWER}**")
        } else {
            String::new()
        };
        assert!(matches!(
            m.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)),
            StreamAction::Copy(text) if text == expected
        ));
    }

    #[test_case(StreamFooter::FollowUp, FOLLOW_UP_COPY ; "btw")]
    #[test_case(StreamFooter::Copy, COPY ; "copy_footer")]
    fn all_copy_controls_exclude_reasoning(footer: StreamFooter, copy_index: usize) {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, footer);
        tx.send(StreamEvent::ThinkingDelta(THINKING.into()))
            .unwrap();
        tx.send(StreamEvent::TextDelta(ANSWER.into())).unwrap();
        tx.send(done()).unwrap();
        let _ = m.poll();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        let copy_hit = m.footer_hit(copy_index);
        assert!(matches!(click(&mut m, copy_hit), StreamAction::Copy(text) if text == ANSWER));
        assert!(matches!(
            m.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)),
            StreamAction::Copy(text) if text == ANSWER
        ));
        if footer == StreamFooter::Copy {
            assert!(matches!(
                m.handle_key(key_ev(KeyCode::Char('y'))),
                StreamAction::Copy(text) if text == ANSWER
            ));
        }
        assert_text_style(&terminal, THINKING, theme::current().thinking);
    }

    #[test_case(false ; "reasoning_only")]
    #[test_case(true ; "earlier_reasoning_section")]
    fn reasoning_reveal_keeps_repainting_after_done(with_answer: bool) {
        let _clock = FrozenClock::at(Duration::ZERO);
        let mut m = StreamModal::new(REVEAL_MS_PER_CHAR);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::Close);
        tx.send(StreamEvent::ThinkingDelta(THINKING.into()))
            .unwrap();
        let _ = m.poll();
        assert_eq!(m.cadence(), Cadence::SMOOTH);
        if with_answer {
            m.ms_per_char = 0;
            tx.send(StreamEvent::TextDelta(ANSWER.into())).unwrap();
        }
        tx.send(done()).unwrap();
        let _ = m.poll();
        assert_eq!(m.cadence(), Cadence::SMOOTH);
        m.exchanges[0].sections[0].body.set_buffer(THINKING);
        assert_eq!(m.cadence(), Cadence::IDLE);
    }

    #[test_case(false ; "error")]
    #[test_case(true ; "stop")]
    fn follow_up_failure_preserves_earlier_sections_and_styles(stopped: bool) {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        tx.send(StreamEvent::ThinkingDelta(THINKING.into()))
            .unwrap();
        tx.send(StreamEvent::TextDelta(ANSWER.into())).unwrap();
        tx.send(done()).unwrap();
        let _ = m.poll();
        let (tx, cancel) = follow_up(&mut m, FOLLOW_UP);
        assert!(m.exchanges[1].sections.is_empty());
        tx.send(StreamEvent::TextDelta(RECONSIDERING.into()))
            .unwrap();
        tx.send(StreamEvent::ThinkingDelta(RECONSIDERING.into()))
            .unwrap();
        let _ = m.poll();
        if stopped {
            m.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        } else {
            tx.send(StreamEvent::Error(ERROR.into())).unwrap();
            let _ = m.poll();
            assert_eq!(m.text(), ERROR);
            assert_eq!(m.exchanges[1].sections.len(), 1);
        }
        assert!(cancel.is_cancelled());
        assert!(!m.is_streaming());
        assert_eq!(m.exchanges[0].text(), ANSWER);
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT * 2)).unwrap();
        draw(&mut m, &mut terminal);
        let theme = theme::current();
        assert_text_style(&terminal, THINKING, theme.thinking);
        assert_text_style(&terminal, ANSWER, theme.assistant);
        if stopped {
            assert_eq!(m.text(), format!("{RECONSIDERING}{STOPPED_NOTE}"));
            assert!(matches!(
                m.exchanges[1].sections.last().unwrap().kind,
                SectionKind::Text
            ));
            assert_text_style(
                &terminal,
                STOPPED_NOTE.trim().trim_matches('_'),
                theme.assistant.patch(theme.italic),
            );
        } else {
            assert!(!terminal.backend().to_string().contains(RECONSIDERING));
            assert_text_style(&terminal, ERROR, theme.error);
        }
    }

    #[test]
    fn a_header_event_replaces_the_header_mid_stream() {
        const LATER: &str = "Extracting with fast-model…";
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::Copy);
        tx.send(StreamEvent::Header(LATER.into())).unwrap();
        assert_eq!(m.poll(), Dirty::YES);
        assert_eq!(m.headers(), [LATER]);
        assert!(m.is_streaming());
    }

    #[test]
    fn poll_done_sets_done_and_drops_rx() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", StreamFooter::Close);
        tx.send(done()).unwrap();
        let _ = m.poll();
        assert!(!m.is_streaming());
    }

    #[test]
    fn poll_done_surfaces_the_outcome_exactly_once() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", StreamFooter::Close);
        tx.send(done()).unwrap();
        let _ = m.poll();
        let done = m.take_done().expect("the outcome reaches the ledger");
        assert_eq!(done.usage.usage.input, 10);
        assert_eq!(done.usage.cost, Some(0.5));
        assert_eq!(done.usage.model, MODEL);
        assert_eq!(done.usage.purpose, LedgerPurpose::Btw);
        assert_eq!(done.answer.as_deref(), Some(ANSWER));
        assert!(m.take_done().is_none(), "usage is never billed twice");
    }

    #[test]
    fn close_keeps_an_outcome_that_already_arrived() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", StreamFooter::Close);
        tx.send(done()).unwrap();
        let _ = m.poll();
        m.close();
        assert!(
            m.take_done().is_some(),
            "dismissing after the answer landed must not discard the bill"
        );
    }

    #[test]
    fn close_cancels_the_in_flight_request() {
        let mut m = StreamModal::new(0);
        let (_tx, cancel) = open_modal(&mut m, "q", StreamFooter::Close);
        assert!(!cancel.is_cancelled());
        m.close();
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn poll_error_replaces_body_and_marks_done() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", StreamFooter::Close);
        tx.send(StreamEvent::TextDelta("partial".into())).unwrap();
        tx.send(StreamEvent::Error("oops".into())).unwrap();
        let _ = m.poll();
        assert!(m.body_eq("oops"));
        assert!(!m.is_streaming());
    }

    #[test_case(KeyCode::Esc   ; "esc_closes")]
    #[test_case(KeyCode::Enter ; "enter_closes")]
    #[test_case(KeyCode::Char(' ') ; "space_closes")]
    fn dismiss_keys_close(code: KeyCode) {
        let mut m = StreamModal::new(0);
        let (_tx, cancel) = open_modal(&mut m, "q", StreamFooter::Close);
        assert!(matches!(m.handle_key(key_ev(code)), StreamAction::Consumed));
        assert!(!m.is_open());
        assert!(!m.is_streaming());
        assert!(cancel.is_cancelled(), "dismissing stops the request");
    }

    #[test]
    fn other_keys_consumed_but_stay_open() {
        let mut m = StreamModal::new(0);
        let (_tx, _cancel) = open_modal(&mut m, "q", StreamFooter::Close);
        assert!(matches!(
            m.handle_key(key_ev(KeyCode::Char('a'))),
            StreamAction::Consumed
        ));
        assert!(m.is_open());
    }

    /// The copy key hands over what has streamed so far, even mid-stream: a
    /// partial list is still worth pasting.
    #[test_case(StreamFooter::Copy, Some("partial") ; "copyable_copies")]
    #[test_case(StreamFooter::Close, None ; "plain_ignores_the_key")]
    fn y_copies_only_when_copyable(footer: StreamFooter, expected: Option<&str>) {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", footer);
        tx.send(StreamEvent::TextDelta("partial".into())).unwrap();
        let _ = m.poll();

        let copied = match m.handle_key(key_ev(KeyCode::Char('y'))) {
            StreamAction::Copy(text) => Some(text),
            _ => None,
        };

        assert_eq!(copied.as_deref(), expected);
        assert!(m.is_open(), "copying leaves the modal up");
    }

    #[test]
    fn the_copy_footer_hovers_and_answers_clicks_as_whole_phrases() {
        let mut m = StreamModal::new(0);
        let (tx, cancel) = open_modal(&mut m, "q", StreamFooter::Copy);
        tx.send(StreamEvent::TextDelta("list".into())).unwrap();
        let _ = m.poll();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        draw(&mut m, &mut terminal);
        let copy = m.footer_hit(COPY);
        assert_eq!(
            usize::from(copy.width),
            COPY_LABEL.len() + COPY_HINT.len(),
            "the hit spans the key and its gloss"
        );

        assert!(matches!(
            m.handle_mouse(&mouse(MouseEventKind::Moved, copy)),
            StreamAction::Ignored
        ));
        draw(&mut m, &mut terminal);
        let reversed = reversed_cells(&terminal);
        assert!(
            reversed.len() == usize::from(copy.width)
                && reversed.iter().all(|position| copy.contains(*position)),
            "{HOVER_MISSED}: hit={copy:?} reversed={reversed:?}"
        );

        assert!(matches!(click(&mut m, copy), StreamAction::Copy(text) if text == "list"));
        assert!(m.is_open(), "copying leaves the modal up");

        let close = m.footer_hit(CLOSE);
        assert!(matches!(click(&mut m, close), StreamAction::Consumed));
        assert!(!m.is_open());
        assert!(cancel.is_cancelled(), "closing stops the request");
    }

    #[test]
    fn the_follow_up_footer_sends_and_closes_by_click() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        let send = m.footer_hit(SEND);
        assert!(send.width > 0, "the send control is drawn");

        assert!(matches!(click(&mut m, send), StreamAction::Consumed));
        assert!(m.is_open(), "a control that says send never dismisses");

        type_text(&mut m, "and y?");
        assert!(
            matches!(click(&mut m, send), StreamAction::Consumed),
            "a follow-up waits for the answer it follows up"
        );
        assert_eq!(
            m.input_text(),
            "",
            "what was typed is held, not left behind"
        );

        tx.send(done()).unwrap();
        let _ = m.poll();
        assert_eq!(
            m.take_queued().as_deref(),
            Some("and y?"),
            "the held question is the host's to send"
        );

        draw(&mut m, &mut terminal);
        type_text(&mut m, "and z?");
        let send = m.footer_hit(SEND);
        assert!(matches!(
            click(&mut m, send),
            StreamAction::Submit(question) if question == "and z?"
        ));
        assert_eq!(m.input_text(), "", "a sent question leaves the input");
        assert!(m.is_open());

        let close = m.footer_hit(FOLLOW_UP_CLOSE);
        assert!(matches!(click(&mut m, close), StreamAction::Consumed));
        assert!(!m.is_open());
    }

    /// A bare `y` is text the input is owed, so copying needs a chord here.
    #[test]
    fn the_follow_up_footer_copies_by_chord_and_by_click() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        tx.send(StreamEvent::TextDelta(ANSWER.into())).unwrap();
        let _ = m.poll();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);

        let copy = m.footer_hit(FOLLOW_UP_COPY);
        assert!(matches!(
            click(&mut m, copy),
            StreamAction::Copy(text) if text == ANSWER
        ));
        assert!(matches!(
            m.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)),
            StreamAction::Copy(text) if text == ANSWER
        ));
        assert_eq!(m.input_text(), "", "the chord copies rather than typing");
        assert!(m.is_open(), "copying leaves the modal up");
    }

    #[test]
    fn scroll_up_down() {
        let mut m = StreamModal::new(0);
        let (_tx, _cancel) = open_modal(&mut m, "q", StreamFooter::Close);
        m.scroll.update_dimensions(100, 10);
        m.scroll.scroll(-5);
        assert_eq!(m.scroll.offset(), 90);
        m.handle_key(key_ev(KeyCode::Up));
        assert_eq!(m.scroll.offset(), 89);
        m.handle_key(key_ev(KeyCode::Down));
        assert_eq!(m.scroll.offset(), 90);
        m.scroll.scroll(200);
        assert_eq!(m.scroll.offset(), 0);
    }

    #[test]
    fn double_open_resets_first() {
        let mut m = StreamModal::new(0);
        let (tx1, first_cancel) = open_modal(&mut m, "first", StreamFooter::Close);
        tx1.send(StreamEvent::TextDelta("leftover".into())).unwrap();
        let _ = m.poll();
        m.scroll.update_dimensions(100, 10);
        m.scroll.scroll(-10);
        let (_tx2, _cancel2) = open_modal(&mut m, "second", StreamFooter::Close);
        assert!(m.is_open());
        assert_eq!(m.headers(), ["second"]);
        assert!(m.body_eq(""));
        assert_eq!(m.scroll.offset(), 0);
        assert!(
            first_cancel.is_cancelled(),
            "a second question stops the first request"
        );
    }

    #[test]
    fn close_drops_rx_signaling_sender() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", StreamFooter::Close);
        m.close();
        assert!(tx.send(StreamEvent::TextDelta("x".into())).is_err());
    }

    #[test]
    fn poll_noop_when_no_rx() {
        let mut m = StreamModal::new(0);
        let _ = m.poll();
        assert!(!m.is_open());
    }

    #[test]
    fn a_follow_up_appends_an_exchange_and_streams_into_it_alone() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        tx.send(StreamEvent::TextDelta(ANSWER.into())).unwrap();
        tx.send(done()).unwrap();
        let _ = m.poll();

        let (tx2, _cancel2) = follow_up(&mut m, FOLLOW_UP);
        assert!(m.is_streaming());
        assert_eq!(m.headers(), [HEADER, FOLLOW_UP]);
        assert_eq!(m.text(), "", "the new exchange starts empty");
        tx2.send(StreamEvent::Error("oops".into())).unwrap();
        let _ = m.poll();

        assert!(m.body_eq("oops"));
        assert!(
            m.exchanges[0].text() == ANSWER,
            "an error in the follow-up leaves the first answer alone"
        );
    }

    #[test]
    fn enter_submits_the_input_once_the_answer_has_landed() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        tx.send(done()).unwrap();
        let _ = m.poll();

        type_text(&mut m, "and y? ");
        assert!(matches!(
            m.handle_key(key_ev(KeyCode::Enter)),
            StreamAction::Submit(question) if question == "and y?"
        ));
        assert_eq!(m.input_text(), "", "a sent question leaves the input");
        assert!(m.is_open());
    }

    /// The thread is one question deep at a time, so a question asked into a
    /// streaming answer is held rather than dropped or sent over the top of it.
    #[test]
    fn enter_mid_stream_queues_the_question_for_the_host() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        type_text(&mut m, "and y? ");
        assert!(
            matches!(m.handle_key(key_ev(KeyCode::Enter)), StreamAction::Consumed),
            "a follow-up waits for the answer it follows up"
        );
        assert_eq!(
            m.input_text(),
            "",
            "what was typed is held, not left behind"
        );
        assert!(m.is_open(), "queueing never dismisses");

        tx.send(done()).unwrap();
        let _ = m.poll();
        assert_eq!(m.take_queued().as_deref(), Some("and y?"));
        assert!(m.take_queued().is_none(), "a queued question is sent once");
    }

    #[test]
    fn a_queued_question_and_a_pending_answer_each_say_so_in_the_input() {
        let mut m = StreamModal::new(0);
        let (_tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        assert!(
            terminal.backend().to_string().contains(WAITING_PLACEHOLDER),
            "an inert-looking input has to say what it is waiting for"
        );

        type_text(&mut m, "and y?");
        m.handle_key(key_ev(KeyCode::Enter));
        draw(&mut m, &mut terminal);
        let screen = terminal.backend().to_string();
        assert!(
            screen.contains(&format!("{QUEUED_PREFIX}and y?")),
            "a queued question stays visible after the input clears: {screen}"
        );
    }

    #[test]
    fn space_and_y_type_into_the_follow_up_input() {
        let mut m = StreamModal::new(0);
        let (_tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        type_text(&mut m, "y ");
        assert!(m.is_open());
        assert_eq!(m.input_text(), "y ");
    }

    #[test_case(KeyCode::Esc ; "esc")]
    #[test_case(KeyCode::Enter ; "empty_enter")]
    fn follow_up_dismissal(code: KeyCode) {
        let mut m = StreamModal::new(0);
        let (_tx, cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        assert!(matches!(m.handle_key(key_ev(code)), StreamAction::Consumed));
        assert!(!m.is_open());
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn a_paste_lands_in_the_input_as_one_line() {
        let mut m = StreamModal::new(0);
        let (_tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        assert!(m.handle_paste("one\ntwo"));
        assert_eq!(m.input_text(), "one two");

        let mut plain = StreamModal::new(0);
        let (_tx, _cancel) = open_modal(&mut plain, HEADER, StreamFooter::Copy);
        assert!(!plain.handle_paste("x"), "no input, nothing to paste into");
    }

    /// The window before the first token is exactly the one the typewriter
    /// cannot carry, and it is where a long prefill is spent.
    #[test]
    fn a_waiting_request_still_asks_to_be_repainted() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::Close);
        assert_eq!(m.cadence(), Cadence::SPINNER, "{STATUS_MISSING}");

        tx.send(done()).unwrap();
        let _ = m.poll();
        assert_eq!(m.cadence(), Cadence::IDLE, "{STATUS_LINGERED}");
    }

    #[test]
    fn the_status_row_carries_the_wait_and_leaves_when_the_answer_lands() {
        let _clock = FrozenClock::at(Duration::ZERO);
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::Close);
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        let waiting = terminal.backend().to_string();
        assert!(waiting.contains("0.0s"), "{STATUS_MISSING}: {waiting}");

        tx.send(done()).unwrap();
        let _ = m.poll();
        draw(&mut m, &mut terminal);
        let settled = terminal.backend().to_string();
        assert!(
            !settled.contains(PROMPT_PROGRESS_LABEL.trim()),
            "{STATUS_LINGERED}: {settled}"
        );
    }

    /// Only some providers report prefill, so the bar is drawn from what
    /// arrives and retired the moment the answer starts instead.
    #[test_case(false ; "answer")]
    #[test_case(true ; "thinking")]
    fn prefill_progress_fills_the_bar_until_the_first_token(thinking: bool) {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::Close);
        tx.send(StreamEvent::Progress(PromptProgress {
            processed: 1_000,
            total: 4_000,
            cache: 800,
        }))
        .unwrap();
        let _ = m.poll();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        let prefilling = terminal.backend().to_string();
        assert!(
            prefilling.contains(PROMPT_PROGRESS_LABEL.trim()),
            "{STATUS_MISSING}: {prefilling}"
        );

        tx.send(delta(thinking, ANSWER)).unwrap();
        let _ = m.poll();
        draw(&mut m, &mut terminal);
        let answering = terminal.backend().to_string();
        assert!(
            !answering.contains(PROMPT_PROGRESS_LABEL.trim()),
            "text means the prefill is over: {answering}"
        );
    }

    /// A prompt the server has entirely prefilled has nothing left to report,
    /// and a full bar that never empties reads as a stall.
    #[test]
    fn a_finished_prefill_retires_its_own_bar() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::Close);
        tx.send(StreamEvent::Progress(PromptProgress {
            processed: 4_000,
            total: 4_000,
            cache: 4_000,
        }))
        .unwrap();
        let _ = m.poll();
        assert!(m.progress.is_none());
    }

    #[test]
    fn ctrl_c_stops_the_answer_and_keeps_the_thread() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        tx.send(StreamEvent::TextDelta(ANSWER.into())).unwrap();
        tx.send(done()).unwrap();
        let _ = m.poll();
        let (_tx2, second) = follow_up(&mut m, FOLLOW_UP);

        m.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(second.is_cancelled(), "stopping cancels the live request");
        assert!(
            m.is_open(),
            "stopping an answer is not dismissing the modal"
        );
        assert!(!m.is_streaming());
        assert_eq!(m.headers(), [HEADER, FOLLOW_UP], "the thread survives");
        assert!(
            m.exchanges[0].text() == ANSWER,
            "the answered exchange keeps its answer"
        );
        assert!(m.text().contains(STOPPED_NOTE.trim()));
    }

    /// Without a stream to stop the chord is the global one, which dismisses.
    #[test]
    fn ctrl_c_closes_once_nothing_is_streaming() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        tx.send(done()).unwrap();
        let _ = m.poll();

        m.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!m.is_open());
    }

    #[test_case(KeyCode::Char('w'), "one !"; "ctrl_w_deletes_the_word_before_the_caret")]
    #[test_case(KeyCode::Backspace, "one !"; "ctrl_backspace_deletes_the_word_before_the_caret")]
    #[test_case(KeyCode::Left, "one !two"; "ctrl_left_moves_back_a_word")]
    fn the_follow_up_input_edits_like_the_composer(code: KeyCode, expected: &str) {
        let mut m = StreamModal::new(0);
        let (_tx, cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        type_text(&mut m, "one two");
        assert!(matches!(
            m.handle_key(KeyEvent::new(code, KeyModifiers::CONTROL)),
            StreamAction::Consumed
        ));
        type_text(&mut m, "!");
        assert_eq!(m.input_text(), expected);
        assert!(m.is_streaming() && !cancel.is_cancelled());
    }

    /// `Ctrl+C` copies a selection before it stops anything, and `Ctrl+Y`
    /// stays the modal's: it copies the answer, never the selection.
    #[test]
    fn ctrl_c_copies_a_selected_follow_up_and_stops_nothing() {
        const SELECTED: &str = "two";
        let mut m = StreamModal::new(0);
        let (tx, cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        tx.send(StreamEvent::TextDelta(ANSWER.into())).unwrap();
        let _ = m.poll();
        type_text(&mut m, "one two");
        for _ in SELECTED.chars() {
            m.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));
        }

        assert!(matches!(
            m.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            StreamAction::Copy(text) if text == SELECTED
        ));
        assert!(m.is_streaming() && !cancel.is_cancelled());
        assert!(matches!(
            m.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)),
            StreamAction::Copy(text) if text == ANSWER
        ));
    }

    /// Scrolling back through a thread, the price of an answer belongs beside
    /// the question that bought it.
    #[test]
    fn a_settled_question_carries_what_its_answer_cost() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::Close);
        tx.send(StreamEvent::Done(StreamDone {
            usage: StreamUsage {
                usage: TokenUsage {
                    input: 200,
                    output: 340,
                    cache_creation: 0,
                    cache_read: 1_900,
                },
                cost: None,
                billing: Billing::Api,
                provider: PROVIDER.into(),
                model: MODEL.into(),
                purpose: LedgerPurpose::Btw,
            },
            answer: Some(ANSWER.into()),
        }))
        .unwrap();
        let _ = m.poll();

        let suffix = m.exchanges[0].suffix();
        assert!(suffix.contains(&format!("2k{TOKENS_IN}")), "{suffix}");
        assert!(suffix.contains(&format!("1k{TOKENS_CACHED}")), "{suffix}");
        assert!(suffix.contains(&format!("340{TOKENS_OUT}")), "{suffix}");
    }

    /// A question with no answer under it is unreadable, so a failure is
    /// styled as one rather than replacing the exchange.
    #[test]
    fn an_error_is_painted_as_one_under_the_question_that_caused_it() {
        const OOPS: &str = "the request failed";
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::Close);
        tx.send(StreamEvent::Error(OOPS.into())).unwrap();
        let _ = m.poll();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);

        let screen = terminal.backend().to_string();
        assert!(screen.contains(HEADER), "the question stays: {screen}");
        assert!(screen.contains(OOPS), "{screen}");
        let buffer = terminal.backend().buffer();
        let error = theme::current().error.fg.expect("errors carry a colour");
        assert!(
            buffer
                .area
                .positions()
                .any(|position| buffer[(position.x, position.y)].fg == error),
            "an error reads as an error, not as an answer"
        );
    }

    /// The key does two different things, and the footer says which.
    #[test]
    fn the_follow_up_input_is_drawn_under_the_thread() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        type_text(&mut m, "next");
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        let streaming = terminal.backend().to_string();
        assert!(streaming.contains(HEADER));
        assert!(streaming.contains("> next"));
        assert!(streaming.contains(QUEUE_HINT.trim()), "{streaming}");

        tx.send(done()).unwrap();
        let _ = m.poll();
        draw(&mut m, &mut terminal);
        let settled = terminal.backend().to_string();
        assert!(settled.contains(SEND_HINT.trim()), "{settled}");
    }
}
