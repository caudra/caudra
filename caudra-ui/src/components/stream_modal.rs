use std::mem;
use std::ops::Range;
use std::time::{Duration, Instant};

use crate::animation::{live_elapsed, spinner_str};
use crate::components::ModalScroll;
use crate::components::Overlay;
use crate::components::input::control_tip;
use crate::components::keybindings::key;
use crate::components::messages::{
    ASSISTANT_HEADING, ERROR_HEADING, THINKING_HEADING, USER_HEADING, append_selection_section,
};
use crate::components::modal::{ESC_LABEL, FooterHits, FooterLine, Modal};
use crate::components::prompt_progress::{self, PromptProgress, PromptRate};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::side_transcript::{
    ASK_PLACEHOLDER, BACK_HINT, CLARIFICATION_PLACEHOLDER, CLEAR_HINT, CLEAR_LABEL, CLOSE_HINT,
    COMPACT_BACK_HINT, COMPACT_FOOTER_GAP, COMPACT_QUEUE_HINT, COMPACT_SEND_HINT, COPY_HINT,
    COPY_LABEL, CTRL_COPY_LABEL, DRAFT_SEPARATOR, FOOTER_GAP, FOOTER_ROW, INITIAL_SEND_HINT,
    INPUT_PREFIX, INPUT_ROW, MOVED_ON_NOTE, QUEUE_HINT, QUEUED_PREFIX, SEND_HINT, SEND_LABEL,
    STATUS_ROW, SUMMARY_SEPARATOR, TIP_ASK, TIP_BACK, TIP_CLARIFY, TIP_CLEAR, TIP_CLOSE, TIP_COPY,
    TIP_KEEP, TIP_SEND, WAITING_PLACEHOLDER, usage_summary,
};
use crate::components::streaming_content::StreamingContent;
use crate::components::sweep::{self, Sweep};
use crate::components::tooltip::{Tip, TipKey};
use crate::components::{field_styles, input_text_style};
use crate::theme;

use caudra_agent::{CancelTrigger, format_live_duration};
use caudra_grab::grab_scope;
use caudra_providers::{Billing, TokenUsage};
use caudra_storage::usage_ledger::LedgerPurpose;
use caudra_workbench::text_field::{FieldKind, FieldStyles, TextField, TextKey};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::provenance::Provenance;
use crate::repaint::{Cadence, Dirty};

const H_PAD: u16 = 2;
/// What a `/btw` exchange's header puts before the question it asks.
pub(crate) const QUESTION_PREFIX: &str = "Q: ";
/// Over a copied clarification thread's context, the question form it is about.
const CONTEXT_HEADING: &str = "Question form";
/// Over a copied header that names no question, such as `/extract`'s.
const REQUEST_HEADING: &str = "Request";
const WIDTH_PERCENT: u16 = 65;
const MAX_HEIGHT_PERCENT: u16 = 80;
/// A stopped answer says so in its own body, where the truncation is, rather
/// than only in the header.
const STOPPED_NOTE: &str = "\n\n_Stopped._";
const CLARIFICATION_TITLE: &str = " /btw · Clarify questions ";
const ASK_TITLE: &str = " /btw · Side question ";

const CLOSE_CONTROL: StreamControl = StreamControl {
    label: ESC_LABEL,
    hint: CLOSE_HINT,
    tip: TIP_CLOSE,
    command: StreamCommand::Close,
};
const CTRL_COPY_CONTROL: StreamControl = StreamControl {
    label: CTRL_COPY_LABEL,
    hint: COPY_HINT,
    tip: TIP_COPY,
    command: StreamCommand::Copy,
};
const CLOSE_FOOTER: [StreamControl; 1] = [CLOSE_CONTROL];
const COPY_FOOTER: [StreamControl; 2] = [
    StreamControl {
        label: COPY_LABEL,
        hint: COPY_HINT,
        tip: TIP_COPY,
        command: StreamCommand::Copy,
    },
    CLOSE_CONTROL,
];
/// Copy needs a chord here: a bare `y` is text the input is owed. Clear comes
/// last, so a narrow footer loses it before the controls a reader needs.
const FOLLOW_UP_FOOTER: [StreamControl; 4] = [
    StreamControl {
        label: SEND_LABEL,
        hint: SEND_HINT,
        tip: TIP_SEND,
        command: StreamCommand::Send,
    },
    CTRL_COPY_CONTROL,
    StreamControl {
        label: ESC_LABEL,
        hint: CLOSE_HINT,
        tip: TIP_KEEP,
        command: StreamCommand::Close,
    },
    StreamControl {
        label: CLEAR_LABEL,
        hint: CLEAR_HINT,
        tip: TIP_CLEAR,
        command: StreamCommand::Clear,
    },
];
const CLARIFICATION_FOOTER: [StreamControl; 3] = [
    StreamControl {
        label: ESC_LABEL,
        hint: BACK_HINT,
        tip: TIP_BACK,
        command: StreamCommand::Close,
    },
    StreamControl {
        label: SEND_LABEL,
        hint: INITIAL_SEND_HINT,
        tip: TIP_CLARIFY,
        command: StreamCommand::Send,
    },
    CTRL_COPY_CONTROL,
];
/// Before the first question there is nothing to copy or follow up.
const ASK_FOOTER: [StreamControl; 2] = [
    StreamControl {
        label: SEND_LABEL,
        hint: INITIAL_SEND_HINT,
        tip: TIP_ASK,
        command: StreamCommand::Send,
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
    Clear,
}

/// One footer control: the key it names, the words that gloss it, what its
/// tooltip says, and what it does. The footer is built from these and a click
/// resolves through them, so a control's index is never magic.
struct StreamControl {
    label: &'static str,
    hint: &'static str,
    tip: &'static str,
    command: StreamCommand,
}

/// What a key or the pointer asked of the host. `Copy` carries the text for
/// the clipboard, `Submit` the follow-up the input held. A key is never
/// `Ignored`: the modal owns the keyboard while it is up.
pub enum StreamAction {
    Ignored,
    Consumed,
    Copy(String),
    /// What a sweep over the thread covers.
    CopySelection(String),
    Submit(String),
    /// Forget the side thread the modal shows.
    Clear,
}

/// A side thread's exchanges as the modal drew them, set aside while the
/// modal is closed so reopening shows them again. Never persisted.
pub(crate) struct ParkedThread {
    title: &'static str,
    exchanges: Vec<Exchange>,
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
/// What one run of the thread's rows shows, which is what a sweep over it is
/// copied under.
#[derive(Clone, Copy)]
enum PartKind {
    /// The question form a clarification thread is about.
    Context,
    /// An exchange's header: the question asked, where it names one.
    Header,
    /// One of an exchange's sections, by index.
    Section(usize),
}

/// The rows of the thread one part painted, so a sweep over them can be
/// traced back to what they were rendered from.
struct ThreadPart {
    rows: Range<usize>,
    exchange: usize,
    kind: PartKind,
}

struct Exchange {
    header: String,
    sections: Vec<StreamSection>,
    started_at: Instant,
    /// How long the answer took, once it is in. `None` while it is still
    /// coming, which is also what says the live clock belongs to this one.
    settled: Option<Duration>,
    usage: Option<TokenUsage>,
    outcome: ExchangeOutcome,
    /// The conversation the thread reads moved on after this answer.
    moved_on: bool,
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
            moved_on: false,
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
        self.settled.map_or_else(String::new, |settled| {
            format!("{SUMMARY_SEPARATOR}{}", usage_summary(settled, self.usage))
        })
    }
}

/// A modal that draws model answers as they stream: `/btw` and `/extract`
/// both open it, with their own title, header, and footer. A thread of
/// exchanges grows downwards; only the last one is ever live.
pub struct StreamModal {
    open: bool,
    title: &'static str,
    footer: StreamFooter,
    context: Option<String>,
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
    /// What a dismissed side question left unsent, until the next
    /// [`StreamModal::open_ask`]. Outlives [`StreamModal::close`].
    parked_draft: String,
    /// What the last [`StreamModal::park`] set aside, until the host takes it.
    parked: Option<ParkedThread>,
    popup: Rect,
    /// The thread's lines as the last frame wrapped them into `viewport`,
    /// which is what a press is read back against.
    body: Vec<Line<'static>>,
    parts: Vec<ThreadPart>,
    viewport: Rect,
    sweep: Option<Sweep>,
    /// Between a press on the thread and its release.
    sweeping: bool,
}

impl StreamModal {
    pub fn new(ms_per_char: u64) -> Self {
        Self {
            open: false,
            title: "",
            footer: StreamFooter::Close,
            context: None,
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
            parked_draft: String::new(),
            parked: None,
            popup: Rect::default(),
            body: Vec::new(),
            parts: Vec::new(),
            viewport: Rect::default(),
            sweep: None,
            sweeping: false,
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

    pub(crate) fn open_clarification(&mut self, context: String) {
        if !self.is_clarification() {
            self.close();
            self.title = CLARIFICATION_TITLE;
            self.footer = StreamFooter::FollowUp;
        }
        self.context = Some(context);
        self.open = true;
    }

    /// An empty side question that asks nothing until the first Enter, opened
    /// on whatever the last one left unsent.
    pub(crate) fn open_ask(&mut self) {
        self.close();
        self.open = true;
        self.title = ASK_TITLE;
        self.footer = StreamFooter::FollowUp;
        let draft = mem::take(&mut self.parked_draft);
        self.input.set_text(&draft);
    }

    pub(crate) fn forget_parked_draft(&mut self) {
        self.parked_draft.clear();
    }

    pub(crate) fn is_clarification(&self) -> bool {
        self.context.is_some()
    }

    /// The follow-up queued behind a streaming answer, with whatever was typed
    /// after it, so neither is lost when the modal goes away.
    fn take_unsent(&mut self) -> Option<String> {
        let queued = self.queued.take()?;
        let draft = self.input.text();
        Some(if draft.trim().is_empty() || draft.trim() == queued {
            queued
        } else {
            format!("{queued}{DRAFT_SEPARATOR}{draft}")
        })
    }

    pub(crate) fn dismiss(&mut self) {
        if !self.is_clarification() {
            if self.footer == StreamFooter::FollowUp && !self.exchanges.is_empty() {
                self.park();
            } else {
                self.park_draft();
                self.close();
            }
            return;
        }
        let _ = self.poll();
        if self.is_streaming() {
            self.stop();
        }
        if let Some(unsent) = self.take_unsent() {
            self.input.set_text(&unsent);
        }
        self.open = false;
        self.footer_hits.reset();
        self.scrollbar = Scrollbar::default();
        self.sweep = None;
        self.sweeping = false;
    }

    fn park_draft(&mut self) {
        let unsent = self.take_unsent().unwrap_or_else(|| self.input.text());
        if !unsent.trim().is_empty() {
            self.parked_draft = unsent;
        }
    }

    /// Closes the modal and sets its thread aside for the host to keep,
    /// stopping an answer still streaming so it shows as stopped when the
    /// thread comes back. What was left unsent is parked as a draft.
    pub(crate) fn park(&mut self) {
        let _ = self.poll();
        if self.is_streaming() {
            self.stop();
        }
        self.park_draft();
        self.parked = Some(ParkedThread {
            title: self.title,
            exchanges: mem::take(&mut self.exchanges),
        });
        self.close();
    }

    /// The thread the last [`Self::park`] set aside.
    pub(crate) fn take_parked(&mut self) -> Option<ParkedThread> {
        self.parked.take()
    }

    /// Opens on a thread [`Self::park`] set aside, with whatever it left unsent.
    pub(crate) fn resume(&mut self, parked: ParkedThread) {
        self.open_ask();
        self.title = parked.title;
        self.exchanges = parked.exchanges;
        self.scroll.scroll_to(u16::MAX);
    }

    /// Says under the last answer that the conversation has moved on since.
    pub(crate) fn mark_moved_on(&mut self) {
        if let Some(last) = self.exchanges.last_mut() {
            last.moved_on = true;
        }
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
        self.context = None;
        self.exchanges.clear();
        self.input.clear();
        self.scroll.reset();
        self.footer_hits.reset();
        self.queued = None;
        self.body.clear();
        self.parts.clear();
        self.sweep = None;
        self.sweeping = false;
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

    /// The bar, a sweep over the thread, and the footer controls. A sweep is
    /// copied when it is let go and stays standing until the next press. Every
    /// other press inside the popup is claimed, so nothing behind it sweeps
    /// the cells it draws.
    pub fn handle_mouse(&mut self, event: &MouseEvent) -> StreamAction {
        match self.scrollbar.handle(event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return StreamAction::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return StreamAction::Consumed;
            }
        }
        let at = Position::new(event.column, event.row);
        if self.sweeping {
            match event.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    let position = self.position_at(at);
                    if let Some(sweep) = &mut self.sweep {
                        sweep.drag_to(position);
                    }
                    return StreamAction::Consumed;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.sweeping = false;
                    return match self.swept() {
                        Some(text) => StreamAction::CopySelection(text),
                        None => {
                            self.sweep = None;
                            StreamAction::Consumed
                        }
                    };
                }
                _ => {}
            }
        }
        if let Some(index) = self.footer_hits.handle_mouse(*event) {
            return match self.controls()[index].command {
                StreamCommand::Copy => self.copy(),
                StreamCommand::Send => self
                    .take_question()
                    .map_or(StreamAction::Consumed, StreamAction::Submit),
                StreamCommand::Close => {
                    self.dismiss();
                    StreamAction::Consumed
                }
                StreamCommand::Clear => StreamAction::Clear,
            };
        }
        if !self.contains(at) {
            return StreamAction::Ignored;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if self.viewport.contains(at) => {
                self.sweep = Some(Sweep::at(self.position_at(at)));
                self.sweeping = true;
                self.input.clear_selection();
                StreamAction::Consumed
            }
            MouseEventKind::Down(_) | MouseEventKind::Drag(_) | MouseEventKind::Up(_) => {
                StreamAction::Consumed
            }
            _ => StreamAction::Ignored,
        }
    }

    fn position_at(&self, at: Position) -> (usize, usize) {
        sweep::wrapped_position(&self.body, self.viewport, self.scroll.offset(), at)
    }

    /// The sweep if one stands, the whole answer otherwise.
    fn copy(&self) -> StreamAction {
        self.swept().map_or_else(
            || StreamAction::Copy(self.text()),
            StreamAction::CopySelection,
        )
    }

    /// What the sweep covers, copied as a sweep over the transcript copies:
    /// the text of one part as it is, and a sweep over several as a Markdown
    /// document with a section for each, under the headings the transcript
    /// uses. An answer's section is its Markdown wherever its rows can trace
    /// it.
    fn swept(&self) -> Option<String> {
        let sweep = self.sweep.filter(|sweep| !sweep.is_empty())?;
        let (start, end) = sweep.ordered();
        let covered: Vec<_> = self
            .parts
            .iter()
            .filter(|part| part.rows.start <= end.0 && part.rows.end > start.0)
            .map(|part| {
                let from = start.max((part.rows.start, 0));
                let to = end.min((part.rows.end - 1, usize::MAX));
                (part, from, to)
            })
            .collect();
        let text = match covered.as_slice() {
            [] => return None,
            [(part, from, to)] => self.part_text(part, *from, *to)?,
            _ => {
                let mut document = String::new();
                for &(part, from, to) in &covered {
                    self.append_part(&mut document, part, from, to);
                }
                document
            }
        };
        (!text.is_empty()).then_some(text)
    }

    /// The rows of `part` from `from` up to `to`, as their source where they
    /// trace one and as drawn otherwise.
    fn part_text(
        &self,
        part: &ThreadPart,
        from: (usize, usize),
        to: (usize, usize),
    ) -> Option<String> {
        let source = match part.kind {
            PartKind::Section(section) => self
                .provenance(part.exchange, section)
                .and_then(|provenance| {
                    provenance.extract_rows(
                        &self.body[part.rows.clone()],
                        (from.0 - part.rows.start, from.1),
                        (to.0 - part.rows.start, to.1),
                    )
                })
                .filter(|source| !source.is_empty()),
            PartKind::Context | PartKind::Header => None,
        };
        source.or_else(|| {
            let mut covered = Sweep::at(from);
            covered.drag_to(to);
            sweep::swept_text(&self.body, covered)
        })
    }

    fn append_part(
        &self,
        document: &mut String,
        part: &ThreadPart,
        from: (usize, usize),
        to: (usize, usize),
    ) {
        let exchange = &self.exchanges[part.exchange];
        let (heading, body, fenced) = match part.kind {
            PartKind::Context => (
                CONTEXT_HEADING,
                self.part_text(part, from, to).unwrap_or_default(),
                true,
            ),
            PartKind::Header => {
                let (heading, lead) = match exchange.header.strip_prefix(QUESTION_PREFIX) {
                    Some(_) => (USER_HEADING, QUESTION_PREFIX.chars().count()),
                    None => (REQUEST_HEADING, 0),
                };
                let header: Vec<char> = exchange.header.chars().collect();
                let range = from.1.max(lead)..to.1.min(header.len());
                let body = header
                    .get(range)
                    .map_or_else(String::new, String::from_iter);
                (heading, body, false)
            }
            PartKind::Section(section) => {
                let heading = match exchange.sections[section].kind {
                    SectionKind::Text => ASSISTANT_HEADING,
                    SectionKind::Thinking => THINKING_HEADING,
                    SectionKind::Error => ERROR_HEADING,
                };
                let fenced = exchange.sections[section].kind == SectionKind::Error;
                (
                    heading,
                    self.part_text(part, from, to).unwrap_or_default(),
                    fenced,
                )
            }
        };
        append_selection_section(
            document,
            heading,
            None,
            &body,
            fenced,
            None,
            self.viewport.width,
        );
    }

    fn provenance(&self, exchange: usize, section: usize) -> Option<&Provenance> {
        self.exchanges
            .get(exchange)?
            .sections
            .get(section)?
            .body
            .provenance()
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> StreamAction {
        let chord = key_event.modifiers.contains(KeyModifiers::CONTROL);
        if chord && key_event.code == KeyCode::Char('y') {
            return self.copy();
        }
        if self.footer == StreamFooter::FollowUp {
            return self.handle_follow_up_key(key_event);
        }
        if chord {
            if key_event.code == KeyCode::Char('c') {
                return self.copy_or_interrupt();
            }
            return StreamAction::Consumed;
        }
        match key_event.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') => self.dismiss(),
            KeyCode::Char('y') if self.footer == StreamFooter::Copy => {
                return self.copy();
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
        if key::CLEAR_BTW.matches(key_event)
            && self
                .controls()
                .iter()
                .any(|control| control.command == StreamCommand::Clear)
        {
            return StreamAction::Clear;
        }
        match key_event.code {
            KeyCode::Esc => self.dismiss(),
            KeyCode::Enter => {
                if self.input.text().trim().is_empty() {
                    self.dismiss();
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
                    return self.copy_or_interrupt();
                }
                _ => {}
            },
        }
        StreamAction::Consumed
    }

    /// A standing sweep is what the gesture copies; with none it interrupts.
    fn copy_or_interrupt(&mut self) -> StreamAction {
        if let Some(text) = self.swept() {
            return StreamAction::CopySelection(text);
        }
        self.interrupt();
        StreamAction::Consumed
    }

    /// The global gesture stops what is running, so here it stops the answer
    /// and leaves the thread that asked for it. With nothing running it
    /// dismisses.
    fn interrupt(&mut self) {
        let _ = self.poll();
        match self.is_streaming() {
            true => self.stop(),
            false => self.dismiss(),
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
        if self.is_clarification() {
            return &CLARIFICATION_FOOTER;
        }
        match self.footer {
            StreamFooter::Close => &CLOSE_FOOTER,
            StreamFooter::Copy => &COPY_FOOTER,
            StreamFooter::FollowUp if self.exchanges.is_empty() => &ASK_FOOTER,
            StreamFooter::FollowUp => &FOLLOW_UP_FOOTER,
        }
    }

    /// Each control is one phrase, key and gloss together, so the pointer
    /// marks and presses the whole of what it reads. The send control says
    /// which of the two things it does, because mid-stream it queues. A
    /// footer too wide for its row says less about each control and then
    /// drops the last ones, so the controls that stay are drawn whole.
    fn footer_line(&self, width: u16) -> FooterLine {
        let theme = theme::current();
        let mut footer = FooterLine::default();
        let roomy: usize = self
            .controls()
            .iter()
            .enumerate()
            .map(|(index, control)| {
                control.label.width()
                    + self.control_hint(control, false, width).width()
                    + usize::from(index > 0) * FOOTER_GAP.width()
            })
            .sum();
        let compact = self.is_clarification() || roomy > usize::from(width);
        let gap = if compact {
            COMPACT_FOOTER_GAP
        } else {
            FOOTER_GAP
        };
        let mut used = 0;
        for (index, control) in self.controls().iter().enumerate() {
            let hint = self.control_hint(control, compact, width);
            used += control.label.width() + hint.width() + usize::from(index > 0) * gap.width();
            if compact && used > usize::from(width) {
                break;
            }
            if index > 0 {
                footer.text(gap, theme.tool_dim);
            }
            footer.command(control.label, theme.keybind_key);
            footer.describe(hint, theme.tool_dim);
        }
        footer
    }

    fn control_hint(&self, control: &StreamControl, compact: bool, width: u16) -> &'static str {
        match control.command {
            StreamCommand::Close
                if self.is_clarification()
                    && control.label.width() + BACK_HINT.width() > usize::from(width) =>
            {
                COMPACT_BACK_HINT
            }
            StreamCommand::Send if compact && self.is_streaming() => COMPACT_QUEUE_HINT,
            StreamCommand::Send if compact && !self.exchanges.is_empty() => COMPACT_SEND_HINT,
            StreamCommand::Send if self.is_streaming() => QUEUE_HINT,
            _ => control.hint,
        }
    }

    /// A paste lands in the input, flattened to the one line it has.
    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.handle_text(text, true)
    }

    pub fn handle_text(&mut self, text: &str, paste: bool) -> bool {
        if !self.text_input_active() {
            return false;
        }
        self.input.handle_text(text, paste);
        true
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        grab_scope!("stream_modal", area);

        let theme = theme::current();
        let padded_width = Modal::inner_width(area.width, WIDTH_PERCENT).saturating_sub(H_PAD * 2);

        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut parts = Vec::new();
        if let Some(context) = &self.context {
            lines.extend(
                context
                    .lines()
                    .map(|line| Line::styled(line.to_owned(), theme.tool_dim)),
            );
            parts.push(ThreadPart {
                rows: 0..lines.len(),
                exchange: 0,
                kind: PartKind::Context,
            });
            lines.push(Line::default());
        }
        for (index, exchange) in self.exchanges.iter_mut().enumerate() {
            if index > 0 {
                lines.push(Line::default());
            }
            parts.push(ThreadPart {
                rows: lines.len()..lines.len() + 1,
                exchange: index,
                kind: PartKind::Header,
            });
            lines.push(Line::from(vec![
                Span::styled(exchange.header.clone(), theme.tool_dim),
                Span::styled(exchange.suffix(), theme.tool_dim),
            ]));
            lines.push(Line::default());
            for (section_index, section) in exchange.sections.iter_mut().enumerate() {
                if section_index > 0 {
                    lines.push(Line::default());
                }
                let first = lines.len();
                for line in section.body.render_lines(padded_width) {
                    let mut line = line.clone();
                    if section.kind == SectionKind::Thinking {
                        for span in &mut line.spans {
                            span.style = span.style.patch(theme.thinking);
                        }
                    }
                    lines.push(line);
                }
                parts.push(ThreadPart {
                    rows: first..lines.len(),
                    exchange: index,
                    kind: PartKind::Section(section_index),
                });
            }
            if exchange.moved_on {
                lines.push(Line::default());
                lines.push(Line::styled(MOVED_ON_NOTE, theme.tool_dim.italic()));
            }
        }

        let mut shown = lines.clone();
        if let Some(sweep) = self.sweep {
            sweep::highlight(&mut shown, sweep);
        }
        let paragraph = Paragraph::new(shown).wrap(Wrap { trim: false });
        let chrome = self.chrome_rows();
        let total = paragraph.line_count(padded_width) as u16;
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
        frame.render_widget(paragraph.scroll((scroll, 0)), viewport);
        self.body = lines;
        self.parts = parts;
        self.viewport = viewport;

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
        let footer = self.footer_line(footer_row.width);
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
            None if !self.is_streaming() && self.is_clarification() => {
                Some(CLARIFICATION_PLACEHOLDER.to_owned())
            }
            None if self.exchanges.is_empty() => Some(ASK_PLACEHOLDER.to_owned()),
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

    fn tooltip(&self) -> Option<Tip> {
        self.footer_hits.tip(TipKey::FooterHint, |index| {
            let control = self.controls().get(index)?;
            Some(control_tip(control.tip, control.label))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::animation::test_clock::FrozenClock;
    use crate::components::key as key_ev;
    use crate::components::prompt_progress::PROMPT_PROGRESS_LABEL;
    use crate::components::side_transcript::{TOKENS_CACHED, TOKENS_IN, TOKENS_OUT};
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
    const CONTEXT: &str = "Choose a storage backend\nSQLite or PostgreSQL";
    const UPDATED_CONTEXT: &str = "Choose a storage backend\nSQLite selected";
    const DRAFT: &str = "Which is simpler?";
    const NEWER_DRAFT: &str = "What about backups?";
    const CLARIFICATION_BACK: usize = 0;
    const CLARIFICATION_SEND: usize = 1;
    const CLARIFICATION_COPY: usize = 2;

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

    #[test_case(false ; "enter")]
    #[test_case(true ; "footer")]
    fn clarification_opens_an_idle_composer_and_submits_its_first_draft(by_click: bool) {
        let mut m = StreamModal::new(0);
        m.open_clarification(CONTEXT.into());
        assert!(m.is_open() && m.is_clarification() && m.text_input_active());
        assert!(!m.is_streaming());
        assert!(m.exchanges.is_empty());
        assert!(m.text().is_empty());
        assert_eq!(m.cadence(), Cadence::IDLE);
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        let screen = terminal.backend().to_string();
        for line in CONTEXT.lines() {
            assert!(screen.contains(line));
        }
        assert!(screen.contains(CLARIFICATION_PLACEHOLDER));
        assert!(screen.contains(&format!("{ESC_LABEL}{BACK_HINT}")));
        assert!(screen.contains(&format!("{SEND_LABEL}{INITIAL_SEND_HINT}")));
        let send = m.footer_hit(CLARIFICATION_SEND);
        assert!(matches!(click(&mut m, send), StreamAction::Consumed));
        assert!(m.is_open());
        assert!(m.handle_paste(DRAFT));
        let action = if by_click {
            click(&mut m, send)
        } else {
            m.handle_key(key_ev(KeyCode::Enter))
        };
        assert!(matches!(action, StreamAction::Submit(question) if question == DRAFT));
        assert!(m.input_text().is_empty());
        assert!(m.is_open());
        assert!(!m.is_streaming());
    }

    #[test]
    fn clarification_resume_preserves_answers_draft_selection_and_scroll() {
        const PARAGRAPHS: usize = 20;
        const SCROLL_OFFSET: u16 = 3;
        let mut m = StreamModal::new(0);
        m.open_clarification(CONTEXT.into());
        let (tx, _cancel) = follow_up(&mut m, HEADER);
        let answer = format!("{ANSWER}\n\n").repeat(PARAGRAPHS);
        tx.send(StreamEvent::TextDelta(answer.clone())).unwrap();
        tx.send(done()).unwrap();
        let _ = m.poll();
        type_text(&mut m, DRAFT);
        m.input.select_all();
        let selection = m.input.selection();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        m.scroll.scroll_to(SCROLL_OFFSET);
        let offset = m.scroll.offset();
        assert!(offset > 0);

        m.dismiss();
        assert!(!m.is_open() && m.is_clarification());
        assert!(!m.text_input_active());
        assert_eq!(m.cadence(), Cadence::IDLE);
        assert!(!m.handle_paste(NEWER_DRAFT));
        m.open_clarification(UPDATED_CONTEXT.into());
        draw(&mut m, &mut terminal);
        assert!(m.is_open());
        assert_eq!(m.context.as_deref(), Some(UPDATED_CONTEXT));
        assert_eq!(m.headers(), [HEADER]);
        assert_eq!(m.text(), answer);
        assert_eq!(m.input_text(), DRAFT);
        assert_eq!(m.input.selection(), selection);
        assert_eq!(m.scroll.offset(), offset);
        assert!(matches!(
            m.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            StreamAction::Copy(text) if text == DRAFT
        ));
        assert!(m.is_open());
    }

    #[test_case(KeyCode::Esc, KeyModifiers::NONE ; "esc")]
    #[test_case(KeyCode::Enter, KeyModifiers::NONE ; "empty_enter")]
    #[test_case(KeyCode::Char('c'), KeyModifiers::CONTROL ; "idle_ctrl_c")]
    fn clarification_back_keys_retain_the_thread(code: KeyCode, modifiers: KeyModifiers) {
        let mut m = StreamModal::new(0);
        m.open_clarification(CONTEXT.into());
        let (tx, _cancel) = follow_up(&mut m, HEADER);
        tx.send(StreamEvent::TextDelta(ANSWER.into())).unwrap();
        tx.send(done()).unwrap();
        let _ = m.poll();
        assert!(matches!(
            m.handle_key(KeyEvent::new(code, modifiers)),
            StreamAction::Consumed
        ));
        assert!(!m.is_open() && m.is_clarification());
        assert_eq!(m.text(), ANSWER);
    }

    #[test_case(false ; "back")]
    #[test_case(true ; "ctrl_c_then_back")]
    fn clarification_stops_only_its_stream_and_retains_partial_sections(interrupt_first: bool) {
        let mut m = StreamModal::new(0);
        m.open_clarification(CONTEXT.into());
        let (tx, cancel) = follow_up(&mut m, HEADER);
        tx.send(StreamEvent::ThinkingDelta(THINKING.into()))
            .unwrap();
        tx.send(StreamEvent::TextDelta(ANSWER.into())).unwrap();
        if interrupt_first {
            let _ = m.poll();
            m.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert!(m.is_open());
            assert!(!m.is_streaming());
        }
        m.dismiss();
        assert!(cancel.is_cancelled());
        assert!(!m.is_open() && !m.is_streaming() && m.is_clarification());
        assert!(tx.send(StreamEvent::TextDelta(NEWER_DRAFT.into())).is_err());
        assert_eq!(m.text(), format!("{ANSWER}{STOPPED_NOTE}"));
        assert!(m.take_done().is_none());
        assert_eq!(m.cadence(), Cadence::IDLE);
        m.open_clarification(CONTEXT.into());
        assert!(matches!(m.exchanges[0].outcome, ExchangeOutcome::Stopped));
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT * 2)).unwrap();
        draw(&mut m, &mut terminal);
        assert_text_style(&terminal, THINKING, theme::current().thinking);
        assert_text_style(&terminal, ANSWER, theme::current().assistant);
    }

    #[test_case("", false ; "queued_only_stopped")]
    #[test_case(NEWER_DRAFT, false ; "newer_draft_stopped")]
    #[test_case(DRAFT, false ; "duplicate_draft_stopped")]
    #[test_case("", true ; "queued_only_done")]
    #[test_case(NEWER_DRAFT, true ; "newer_draft_done")]
    fn clarification_dismiss_returns_queued_input_to_an_editable_draft(
        draft: &str,
        completed: bool,
    ) {
        let mut m = StreamModal::new(0);
        m.open_clarification(CONTEXT.into());
        let (tx, _cancel) = follow_up(&mut m, HEADER);
        type_text(&mut m, DRAFT);
        assert!(matches!(
            m.handle_key(key_ev(KeyCode::Enter)),
            StreamAction::Consumed
        ));
        type_text(&mut m, draft);
        if completed {
            tx.send(done()).unwrap();
        }
        m.dismiss();
        let expected = if draft.is_empty() || draft == DRAFT {
            DRAFT.to_owned()
        } else {
            format!("{DRAFT}{DRAFT_SEPARATOR}{draft}")
        };
        assert_eq!(m.input_text(), expected);
        assert!(m.take_queued().is_none());
        assert_eq!(m.take_done().is_some(), completed);
        m.open_clarification(CONTEXT.into());
        assert!(matches!(
            m.handle_key(key_ev(KeyCode::Enter)),
            StreamAction::Submit(question) if question == expected
        ));
    }

    #[test_case(false, false ; "back_done_waiting_in_receiver")]
    #[test_case(true, false ; "back_done_already_polled")]
    #[test_case(false, true ; "interrupt_done_waiting_in_receiver")]
    #[test_case(true, true ; "interrupt_done_already_polled")]
    fn clarification_dismiss_preserves_completion_usage_and_answer(
        already_polled: bool,
        interrupt: bool,
    ) {
        let mut m = StreamModal::new(REVEAL_MS_PER_CHAR);
        m.open_clarification(CONTEXT.into());
        let (tx, _cancel) = follow_up(&mut m, HEADER);
        tx.send(StreamEvent::TextDelta(ANSWER.into())).unwrap();
        tx.send(done()).unwrap();
        if already_polled {
            let _ = m.poll();
        }
        if interrupt {
            m.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        } else {
            m.dismiss();
        }
        assert_eq!(m.text(), ANSWER);
        assert!(matches!(m.exchanges[0].outcome, ExchangeOutcome::Done));
        assert_eq!(m.cadence(), Cadence::IDLE);
        m.open_clarification(CONTEXT.into());
        let outcome = m.take_done().unwrap();
        assert_eq!(outcome.answer.as_deref(), Some(ANSWER));
        assert_eq!(outcome.usage.model, MODEL);
        assert_eq!(outcome.usage.cost, Some(0.5));
        assert_eq!(outcome.usage.usage.input, 10);
        assert!(m.take_done().is_none());
    }

    #[test]
    fn clarification_copy_excludes_context_and_thinking() {
        let mut m = StreamModal::new(0);
        m.open_clarification(CONTEXT.into());
        let (tx, _cancel) = follow_up(&mut m, HEADER);
        tx.send(StreamEvent::ThinkingDelta(THINKING.into()))
            .unwrap();
        tx.send(StreamEvent::TextDelta(ANSWER.into())).unwrap();
        tx.send(done()).unwrap();
        let _ = m.poll();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT * 2)).unwrap();
        draw(&mut m, &mut terminal);
        assert_text_style(&terminal, THINKING, theme::current().thinking);
        assert!(
            terminal
                .backend()
                .to_string()
                .contains(CONTEXT.lines().next().unwrap())
        );
        assert!(
            terminal
                .backend()
                .to_string()
                .contains(COMPACT_SEND_HINT.trim())
        );
        assert_eq!(m.text(), ANSWER);
        let copy = m.footer_hit(CLARIFICATION_COPY);
        assert!(copy.width > 0);
        assert!(matches!(click(&mut m, copy), StreamAction::Copy(text) if text == ANSWER));
        assert!(matches!(
            m.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)),
            StreamAction::Copy(text) if text == ANSWER
        ));
    }

    #[test_case(22, COMPACT_BACK_HINT ; "compact_back")]
    #[test_case(32, BACK_HINT ; "back_only")]
    #[test_case(WIDTH, BACK_HINT ; "full_footer")]
    fn clarification_footer_keeps_back_clickable_on_narrow_screens(width: u16, hint: &str) {
        let mut m = StreamModal::new(0);
        m.open_clarification(CONTEXT.into());
        type_text(&mut m, DRAFT);
        let mut terminal = Terminal::new(TestBackend::new(width, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        assert!(
            terminal
                .backend()
                .to_string()
                .contains(&format!("{ESC_LABEL}{hint}"))
        );
        let back = m.footer_hit(CLARIFICATION_BACK);
        assert_eq!(usize::from(back.width), ESC_LABEL.width() + hint.width());
        assert!(matches!(click(&mut m, back), StreamAction::Consumed));
        assert!(!m.is_open() && m.is_clarification());
        assert_eq!(m.input_text(), DRAFT);
    }

    #[test_case(StreamFooter::Close ; "plain")]
    #[test_case(StreamFooter::Copy ; "extract")]
    #[test_case(StreamFooter::FollowUp ; "ordinary_btw")]
    fn ordinary_open_and_dismiss_remain_destructive(footer: StreamFooter) {
        let mut m = StreamModal::new(0);
        m.open_clarification(CONTEXT.into());
        type_text(&mut m, DRAFT);
        let (tx, cancel) = open_modal(&mut m, HEADER, footer);
        assert!(!m.is_clarification());
        assert!(m.input_text().is_empty());
        tx.send(StreamEvent::TextDelta(ANSWER.into())).unwrap();
        let _ = m.poll();
        assert_eq!(m.text(), ANSWER);
        m.dismiss();
        assert!(cancel.is_cancelled());
        assert!(!m.is_open() && !m.is_clarification());
        assert!(m.exchanges.is_empty());
        assert!(m.text().is_empty());
    }

    #[test]
    fn ask_opens_an_empty_composer_that_offers_only_send_and_close() {
        let mut m = StreamModal::new(0);
        m.open_ask();
        assert!(m.is_open() && m.text_input_active() && !m.is_clarification());
        assert!(!m.is_streaming() && m.exchanges.is_empty());
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        let screen = terminal.backend().to_string();
        assert!(screen.contains(ASK_TITLE.trim()), "{screen}");
        assert!(screen.contains(ASK_PLACEHOLDER), "{screen}");
        assert!(screen.contains(&format!("{SEND_LABEL}{INITIAL_SEND_HINT}")));
        assert!(screen.contains(&format!("{ESC_LABEL}{CLOSE_HINT}")));
        assert!(!screen.contains(COPY_HINT.trim()), "{screen}");

        assert!(matches!(
            m.handle_key(key_ev(KeyCode::Enter)),
            StreamAction::Consumed
        ));
        assert!(!m.is_open());
    }

    #[test_case(None, DRAFT, DRAFT.to_owned() ; "unsent_question")]
    #[test_case(Some(DRAFT), "", DRAFT.to_owned() ; "queued_follow_up")]
    #[test_case(
        Some(DRAFT),
        NEWER_DRAFT,
        format!("{DRAFT}{DRAFT_SEPARATOR}{NEWER_DRAFT}")
        ; "queued_and_typed"
    )]
    fn a_dismissed_ask_parks_what_it_left_unsent(
        queued: Option<&str>,
        typed: &str,
        expected: String,
    ) {
        let mut m = StreamModal::new(0);
        m.open_ask();
        let _stream = queued.map(|question| {
            let stream = follow_up(&mut m, HEADER);
            type_text(&mut m, question);
            assert!(matches!(
                m.handle_key(key_ev(KeyCode::Enter)),
                StreamAction::Consumed
            ));
            stream
        });
        type_text(&mut m, typed);

        m.dismiss();
        assert!(!m.is_open() && m.exchanges.is_empty());
        m.close();
        m.open_ask();

        assert_eq!(m.input_text(), expected);
        assert!(m.exchanges.is_empty() && !m.is_streaming());
    }

    #[test]
    fn a_forgotten_parked_draft_does_not_come_back() {
        let mut m = StreamModal::new(0);
        m.open_ask();
        type_text(&mut m, DRAFT);
        m.dismiss();

        m.forget_parked_draft();
        m.open_ask();

        assert!(m.input_text().is_empty());
    }

    const FOLLOW_UP_CLEAR: usize = 3;
    const WIDE: u16 = 200;
    const PARKED_MISSING: &str = "a dismissed side thread must be set aside for the host";
    const TIP_WRONG: &str = "every footer control must explain itself under the pointer";

    #[test_case(false ; "settled")]
    #[test_case(true ; "streaming")]
    fn a_parked_thread_resumes_with_what_it_showed(streaming: bool) {
        let mut m = StreamModal::new(0);
        let (tx, cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        tx.send(StreamEvent::TextDelta(ANSWER.into())).unwrap();
        if !streaming {
            tx.send(done()).unwrap();
        }
        type_text(&mut m, DRAFT);

        m.dismiss();
        assert!(!m.is_open() && cancel.is_cancelled());
        let parked = m.take_parked().expect(PARKED_MISSING);
        assert!(m.take_parked().is_none());
        m.resume(parked);

        assert!(m.is_open() && m.text_input_active() && !m.is_streaming());
        assert_eq!(m.title, TITLE);
        assert_eq!(m.headers(), [HEADER]);
        assert_eq!(m.input_text(), DRAFT);
        let outcome = if streaming {
            ExchangeOutcome::Stopped
        } else {
            ExchangeOutcome::Done
        };
        assert!(m.exchanges[0].outcome == outcome);
        assert_eq!(
            m.take_done().is_some(),
            !streaming,
            "a landed answer is billed"
        );
    }

    #[test]
    fn a_thread_that_moved_on_says_so_under_its_last_answer() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        tx.send(done()).unwrap();
        let _ = m.poll();
        m.mark_moved_on();
        let mut terminal = Terminal::new(TestBackend::new(WIDE, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        assert!(terminal.backend().to_string().contains(MOVED_ON_NOTE));
    }

    #[test_case(StreamFooter::FollowUp, true, true ; "answered_thread")]
    #[test_case(StreamFooter::FollowUp, false, false ; "empty_ask")]
    #[test_case(StreamFooter::Copy, true, false ; "extract")]
    fn ctrl_l_clears_only_a_thread_with_answers(
        footer: StreamFooter,
        answered: bool,
        clears: bool,
    ) {
        let mut m = StreamModal::new(0);
        if answered {
            let (tx, _cancel) = open_modal(&mut m, HEADER, footer);
            tx.send(done()).unwrap();
            let _ = m.poll();
        } else {
            m.open_ask();
        }
        let action = m.handle_key(key::CLEAR_BTW.to_key_event());
        assert_eq!(matches!(action, StreamAction::Clear), clears);
    }

    #[test_case(WIDE ; "roomy")]
    #[test_case(WIDTH ; "compact")]
    fn the_follow_up_footer_clears_by_click_and_explains_every_control(width: u16) {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        tx.send(done()).unwrap();
        let _ = m.poll();
        let mut terminal = Terminal::new(TestBackend::new(width, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);

        let tips: Vec<Option<String>> = (0..FOLLOW_UP_FOOTER.len())
            .map(|index| {
                m.handle_mouse(&mouse(MouseEventKind::Moved, m.footer_hit(index)));
                Overlay::tooltip(&m).map(|tip| tip.text)
            })
            .collect();
        let expected: Vec<Option<String>> = FOLLOW_UP_FOOTER
            .iter()
            .map(|control| Some(control_tip(control.tip, control.label)))
            .collect();
        assert_eq!(tips, expected, "{TIP_WRONG}");

        let clear = m.footer_hit(FOLLOW_UP_CLEAR);
        assert!(matches!(click(&mut m, clear), StreamAction::Clear));
    }

    #[test_case(StreamFooter::Close ; "plain")]
    #[test_case(StreamFooter::Copy ; "extract")]
    fn every_other_footer_explains_its_controls(footer: StreamFooter) {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, footer);
        tx.send(done()).unwrap();
        let _ = m.poll();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        for (index, control) in m.controls().iter().enumerate() {
            m.handle_mouse(&mouse(MouseEventKind::Moved, m.footer_hit(index)));
            assert_eq!(
                Overlay::tooltip(&m).map(|tip| tip.text),
                Some(control_tip(control.tip, control.label)),
                "{TIP_WRONG}"
            );
        }
    }

    #[test]
    fn hard_close_clears_retained_clarification_identity_and_content() {
        let mut m = StreamModal::new(0);
        m.open_clarification(CONTEXT.into());
        let (tx, _cancel) = follow_up(&mut m, HEADER);
        tx.send(StreamEvent::TextDelta(ANSWER.into())).unwrap();
        tx.send(done()).unwrap();
        type_text(&mut m, DRAFT);
        m.dismiss();
        m.close();
        assert!(!m.is_clarification());
        assert!(m.text().is_empty());
        assert!(m.input_text().is_empty());
        assert!(m.take_done().is_some());
        m.open_clarification(UPDATED_CONTEXT.into());
        assert!(m.exchanges.is_empty());
        assert!(!m.is_streaming());
    }

    const WRAPPED_ANSWER: &str = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa quebec romeo";
    const FIRST_WORD: &str = "alpha";
    const LAST_WORD: &str = "romeo";
    const MORE: &str = " sierra";
    const SWEEP_WRONG: &str =
        "a sweep must copy the words it was drawn over, with no break at a wrap";
    const SWEEP_UNMARKED: &str = "the swept cells must be drawn selected";
    const CHROME_SWEPT: &str = "a press off the thread must start no sweep";
    const SWEEP_LOST: &str = "a standing sweep must outlive the thread growing under it";

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// The first cell `text` is drawn at.
    fn cell_of(terminal: &Terminal<TestBackend>, text: &str) -> Rect {
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
        Rect::new(position.x, position.y, 1, 1)
    }

    fn after(cell: Rect, text: &str) -> Rect {
        Rect {
            x: cell.x + text.width() as u16,
            ..cell
        }
    }

    /// A modal showing [`WRAPPED_ANSWER`], drawn once.
    fn answered(
        footer: StreamFooter,
    ) -> (
        StreamModal,
        Terminal<TestBackend>,
        flume::Sender<StreamEvent>,
    ) {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, footer);
        tx.send(StreamEvent::TextDelta(WRAPPED_ANSWER.into()))
            .unwrap();
        let _ = m.poll();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        (m, terminal, tx)
    }

    fn sweep(m: &mut StreamModal, from: Rect, to: Rect) -> StreamAction {
        m.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), from));
        m.handle_mouse(&mouse(MouseEventKind::Drag(MouseButton::Left), to));
        m.handle_mouse(&mouse(MouseEventKind::Up(MouseButton::Left), to))
    }

    fn sweep_first_word(m: &mut StreamModal, terminal: &Terminal<TestBackend>) {
        let first = cell_of(terminal, FIRST_WORD);
        assert!(
            matches!(
                sweep(m, first, after(first, FIRST_WORD)),
                StreamAction::CopySelection(text) if text == FIRST_WORD
            ),
            "{SWEEP_WRONG}"
        );
    }

    #[test]
    fn a_sweep_across_a_wrapped_answer_copies_it_without_breaks() {
        let (mut m, mut terminal, _tx) = answered(StreamFooter::Close);
        let first = cell_of(&terminal, FIRST_WORD);
        let last = cell_of(&terminal, LAST_WORD);
        assert!(
            last.y > first.y,
            "the answer must wrap for this test to mean anything"
        );

        let action = sweep(&mut m, first, after(last, LAST_WORD));
        draw(&mut m, &mut terminal);

        assert!(
            matches!(&action, StreamAction::CopySelection(text) if text == WRAPPED_ANSWER),
            "{SWEEP_WRONG}"
        );
        let marked = reversed_cells(&terminal);
        assert!(
            marked.contains(&Position::new(first.x, first.y)),
            "{SWEEP_UNMARKED}"
        );
        assert!(
            marked.contains(&Position::new(last.x, last.y)),
            "{SWEEP_UNMARKED}"
        );
    }

    #[test_case(true  ; "border")]
    #[test_case(false ; "status_row")]
    fn a_press_off_the_thread_is_claimed_without_sweeping(border: bool) {
        let (mut m, _terminal, _tx) = answered(StreamFooter::Close);
        let at = match border {
            true => Rect::new(m.popup.x, m.popup.y, 1, 1),
            false => Rect::new(m.viewport.x, m.viewport.bottom(), 1, 1),
        };

        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            assert!(matches!(
                m.handle_mouse(&mouse(kind, at)),
                StreamAction::Consumed
            ));
        }
        assert!(m.sweep.is_none(), "{CHROME_SWEPT}");
    }

    #[test]
    fn the_footer_copy_takes_a_standing_sweep() {
        let (mut m, terminal, _tx) = answered(StreamFooter::Copy);
        sweep_first_word(&mut m, &terminal);
        let copy = m.footer_hit(COPY);

        assert!(matches!(
            click(&mut m, copy),
            StreamAction::CopySelection(text) if text == FIRST_WORD
        ));
    }

    #[test_case('y' ; "ctrl_y")]
    #[test_case('c' ; "ctrl_c")]
    fn a_copy_chord_takes_a_standing_sweep_and_stops_nothing(c: char) {
        let (mut m, terminal, _tx) = answered(StreamFooter::FollowUp);
        sweep_first_word(&mut m, &terminal);

        assert!(matches!(
            m.handle_key(ctrl(c)),
            StreamAction::CopySelection(text) if text == FIRST_WORD
        ));
        assert!(m.is_streaming() && m.is_open());
    }

    #[test]
    fn ctrl_c_without_a_sweep_still_stops_the_answer() {
        let (mut m, _terminal, _tx) = answered(StreamFooter::FollowUp);

        assert!(matches!(m.handle_key(ctrl('c')), StreamAction::Consumed));
        assert!(!m.is_streaming() && m.is_open());
    }

    #[test]
    fn a_sweep_outlives_the_answer_growing_and_scrolling_but_not_closing() {
        let (mut m, mut terminal, tx) = answered(StreamFooter::Close);
        sweep_first_word(&mut m, &terminal);
        tx.send(StreamEvent::TextDelta(MORE.into())).unwrap();
        let _ = m.poll();
        m.scroll(-1);
        draw(&mut m, &mut terminal);

        assert!(
            matches!(m.handle_key(ctrl('y')), StreamAction::CopySelection(text) if text == FIRST_WORD),
            "{SWEEP_LOST}"
        );
        m.close();
        assert!(m.sweep.is_none() && m.body.is_empty());
    }

    const QUESTION: &str = "why?";
    const THOUGHT: &str = "pondering";
    const ACROSS_TURNS_WRONG: &str =
        "a sweep over several parts must copy as the transcript does, a section for each";

    #[test]
    fn a_sweep_from_the_question_into_the_answer_copies_a_section_for_each() {
        let (mut m, terminal, _tx) = answered(StreamFooter::Close);
        let header = cell_of(&terminal, HEADER);
        let last = cell_of(&terminal, LAST_WORD);

        let action = sweep(&mut m, header, after(last, LAST_WORD));

        let expected = format!(
            "## {USER_HEADING}\n\n{QUESTION}\n\n---\n\n## {ASSISTANT_HEADING}\n\n{WRAPPED_ANSWER}"
        );
        assert!(
            matches!(&action, StreamAction::CopySelection(text) if *text == expected),
            "{ACROSS_TURNS_WRONG}"
        );
    }

    #[test]
    fn thinking_and_errors_copy_under_their_own_headings() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::Close);
        tx.send(StreamEvent::ThinkingDelta(THOUGHT.into())).unwrap();
        tx.send(StreamEvent::TextDelta(ANSWER.into())).unwrap();
        tx.send(done()).unwrap();
        let _ = m.poll();
        let (tx, _cancel) = follow_up(&mut m, FOLLOW_UP);
        tx.send(StreamEvent::Error(ERROR.into())).unwrap();
        let _ = m.poll();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        draw(&mut m, &mut terminal);
        let thought = cell_of(&terminal, THOUGHT);
        let error = cell_of(&terminal, ERROR);

        let action = sweep(&mut m, thought, after(error, ERROR));

        let StreamAction::CopySelection(text) = action else {
            panic!("{ACROSS_TURNS_WRONG}");
        };
        let headings: Vec<&str> = text
            .lines()
            .filter_map(|line| line.strip_prefix("## "))
            .collect();
        assert_eq!(
            headings,
            [
                THINKING_HEADING,
                ASSISTANT_HEADING,
                USER_HEADING,
                ERROR_HEADING
            ],
            "{ACROSS_TURNS_WRONG}"
        );
        assert!(
            text.contains(THOUGHT) && text.contains(ANSWER),
            "{ACROSS_TURNS_WRONG}"
        );
        assert!(
            text.contains(&format!("\n{ERROR}\n```")),
            "an error is fenced: {text:?}"
        );
    }

    #[test]
    fn a_click_on_the_thread_drops_the_sweep() {
        let (mut m, terminal, _tx) = answered(StreamFooter::Close);
        sweep_first_word(&mut m, &terminal);

        assert!(matches!(
            click(&mut m, cell_of(&terminal, LAST_WORD)),
            StreamAction::Consumed
        ));
        assert!(m.sweep.is_none());
    }
}
