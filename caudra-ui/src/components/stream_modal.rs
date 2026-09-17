use crate::components::ModalScroll;
use crate::components::Overlay;
use crate::components::modal::{ESC_LABEL, FooterHits, FooterLine, Modal};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::streaming_content::StreamingContent;
use crate::text_buffer::TextBuffer;
use crate::theme;

use caudra_agent::CancelTrigger;
use caudra_providers::{Billing, TokenUsage};
use caudra_storage::usage_ledger::LedgerPurpose;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::repaint::{Cadence, Dirty};

const H_PAD: u16 = 2;
const WIDTH_PERCENT: u16 = 65;
const MAX_HEIGHT_PERCENT: u16 = 80;
pub(crate) const COPY_LABEL: &str = "y";
const COPY_HINT: &str = " Copy";
const CLOSE_HINT: &str = " Close";
const FOOTER_GAP: &str = "   ";
const INPUT_PREFIX: &str = "> ";
const SEND_LABEL: &str = "Enter";
const SEND_HINT: &str = " Send a follow-up";
/// The input line and the hint under it, kept out of the scroll region so a
/// long thread never scrolls the prompt away.
const INPUT_ROWS: u16 = 2;

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
const FOLLOW_UP_FOOTER: [StreamControl; 2] = [
    StreamControl {
        label: SEND_LABEL,
        hint: SEND_HINT,
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
    TextDelta(String),
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

/// One question and the answer streaming under it.
struct Exchange {
    header: String,
    body: StreamingContent,
}

impl Exchange {
    fn new(header: String, ms_per_char: u64) -> Self {
        let theme = theme::current();
        Self {
            header,
            body: StreamingContent::new_noninteractive(
                "",
                theme.assistant,
                theme.assistant,
                ms_per_char,
            ),
        }
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
    input: TextBuffer,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    footer_hits: FooterHits,
    rx: Option<flume::Receiver<StreamEvent>>,
    /// Dropping this cancels the in-flight request, so every teardown path that
    /// already funnels through [`StreamModal::close`] cancels for free.
    cancel: Option<CancelTrigger>,
    pending_done: Option<StreamDone>,
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
            input: TextBuffer::new(String::new()),
            scroll: ModalScroll::new(),
            scrollbar: Scrollbar::default(),
            footer_hits: FooterHits::default(),
            rx: None,
            cancel: None,
            pending_done: None,
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
        self.exchanges.push(Exchange::new(header, self.ms_per_char));
        self.rx = Some(rx);
        self.cancel = Some(cancel);
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
        self.rx = None;
        self.cancel = None;
    }

    pub fn take_done(&mut self) -> Option<StreamDone> {
        self.pending_done.take()
    }

    pub fn is_streaming(&self) -> bool {
        self.rx.is_some()
    }

    /// Only the typewriters move on their own, and only while on screen. A
    /// pending stream is drained by [`Self::poll`], which reports its own
    /// [`Dirty`].
    pub fn cadence(&self) -> Cadence {
        Cadence::when(
            self.open && self.exchanges.iter().any(|e| e.body.is_animating()),
            Cadence::SMOOTH,
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
    pub fn text(&self) -> &str {
        self.exchanges.last().map_or("", |e| e.body.buffer())
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
                StreamEvent::TextDelta(text) => live.body.push(&text),
                StreamEvent::Done(done) => {
                    self.pending_done = Some(done);
                    finished = true;
                    break;
                }
                StreamEvent::Error(msg) => {
                    live.body.clear();
                    live.body.push(&msg);
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
    /// never leaves a live trigger behind.
    fn finish_stream(&mut self) {
        self.rx = None;
        self.cancel = None;
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
            StreamCommand::Copy => StreamAction::Copy(self.text().to_owned()),
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
        if self.footer == StreamFooter::FollowUp {
            return self.handle_follow_up_key(key_event);
        }
        match key_event.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') => self.close(),
            KeyCode::Char('y') if self.footer == StreamFooter::Copy => {
                return StreamAction::Copy(self.text().to_owned());
            }
            _ => {
                self.scroll.handle_key(key_event);
            }
        }
        StreamAction::Consumed
    }

    /// The input owns every key but the four that move the thread: `y` and
    /// Space are text here.
    fn handle_follow_up_key(&mut self, key_event: KeyEvent) -> StreamAction {
        match key_event.code {
            KeyCode::Esc => self.close(),
            KeyCode::Enter => match self.take_question() {
                Some(question) => return StreamAction::Submit(question),
                None if self.input.value().trim().is_empty() => self.close(),
                None => {}
            },
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                self.scroll.handle_key(key_event);
            }
            _ => {
                self.input.handle_key(key_event);
            }
        }
        StreamAction::Consumed
    }

    /// The typed follow-up, cleared from the input once taken. Nothing is
    /// taken while an answer is still streaming, so a follow-up cannot cancel
    /// the answer it is following up.
    fn take_question(&mut self) -> Option<String> {
        let question = self.input.value().trim().to_owned();
        if question.is_empty() || self.is_streaming() {
            return None;
        }
        self.input.clear();
        Some(question)
    }

    fn controls(&self) -> &'static [StreamControl] {
        match self.footer {
            StreamFooter::Close => &CLOSE_FOOTER,
            StreamFooter::Copy => &COPY_FOOTER,
            StreamFooter::FollowUp => &FOLLOW_UP_FOOTER,
        }
    }

    /// A paste lands in the input, flattened to the one line it has.
    pub fn handle_paste(&mut self, text: &str) -> bool {
        if !self.open || self.footer != StreamFooter::FollowUp {
            return false;
        }
        let flat: Vec<&str> = text.lines().collect();
        self.input.insert_text(&flat.join(" "));
        true
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }

        let theme = theme::current();
        let padded_width = Modal::inner_width(area.width, WIDTH_PERCENT).saturating_sub(H_PAD * 2);

        let mut lines: Vec<Line> = Vec::new();
        for (index, exchange) in self.exchanges.iter_mut().enumerate() {
            if index > 0 {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(
                exchange.header.clone(),
                theme.tool_dim,
            )));
            lines.push(Line::default());
            lines.extend_from_slice(exchange.body.render_lines(padded_width));
        }
        let footer = footer_line(self.controls());
        let input_rows = if self.footer == StreamFooter::FollowUp {
            INPUT_ROWS
        } else {
            lines.push(Line::default());
            lines.push(footer.line(self.footer_hits.hovered()));
            0
        };

        let total = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(padded_width) as u16;
        let modal = Modal {
            title: self.title,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, total.saturating_add(input_rows));
        let padded = Rect {
            x: inner.x + H_PAD,
            width: inner.width.saturating_sub(H_PAD * 2),
            ..inner
        };
        let viewport = Rect {
            height: padded.height.saturating_sub(input_rows),
            ..padded
        };
        self.scroll.update_dimensions(total, viewport.height);
        let scroll = self.scroll.offset();
        if input_rows == 0 {
            self.footer_hits.set(footer.hits(viewport, scroll, total));
        }

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

        if input_rows > 0 {
            let prompt_row = Rect {
                y: viewport.y + viewport.height,
                height: (padded.height - viewport.height).min(1),
                ..padded
            };
            let hint_row = Rect {
                y: prompt_row.bottom(),
                height: padded.bottom().saturating_sub(prompt_row.bottom()),
                ..padded
            };
            frame.render_widget(Paragraph::new(self.input_line()), prompt_row);
            self.footer_hits.set(footer.hits(hint_row, 0, 1));
            frame.render_widget(
                Paragraph::new(footer.line(self.footer_hits.hovered())),
                hint_row,
            );
        }

        self.popup = popup;
        popup
    }

    fn input_line(&self) -> Line<'static> {
        let theme = theme::current();
        let text = self.input.value();
        let cursor_byte = TextBuffer::char_to_byte(&text, self.input.x());
        let (before, rest) = text.split_at(cursor_byte);
        let mut chars = rest.chars();
        let cursor_char = chars.next().unwrap_or(' ');
        let after = chars.as_str();
        let style = super::input_text_style();
        Line::from(vec![
            Span::styled(INPUT_PREFIX, theme.tool_dim),
            Span::styled(before.to_owned(), style),
            Span::styled(cursor_char.to_string(), theme.cursor),
            Span::styled(after.to_owned(), style),
        ])
    }

    #[cfg(test)]
    pub fn body_eq(&self, expected: &str) -> bool {
        self.exchanges.last().is_some_and(|e| e.body == expected)
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
        self.input.value()
    }
}

/// Each control is one phrase, key and gloss together, so the pointer marks
/// and presses the whole of what it reads.
fn footer_line(controls: &[StreamControl]) -> FooterLine {
    let theme = theme::current();
    let mut footer = FooterLine::default();
    for (index, control) in controls.iter().enumerate() {
        if index > 0 {
            footer.text(FOOTER_GAP, theme.tool_dim);
        }
        footer.command(control.label, theme.keybind_key);
        footer.describe(control.hint, theme.tool_dim);
    }
    footer
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
    use crate::components::key as key_ev;
    use caudra_agent::CancelToken;
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use test_case::test_case;

    const COPY: usize = 0;
    const SEND: usize = 0;
    const CLOSE: usize = 1;
    const HOVER_MISSED: &str = "the footer control must reverse under the pointer as one phrase";
    const MODEL: &str = "test-model";
    const PROVIDER: &str = "anthropic";
    const TITLE: &str = " /btw ";
    const HEADER: &str = "Q: why?";
    const FOLLOW_UP: &str = "Q: and then?";
    const ANSWER: &str = "because";

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
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
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
        assert_eq!(m.input_text(), "and y?", "the draft survives the wait");

        tx.send(done()).unwrap();
        let _ = m.poll();
        assert!(matches!(
            click(&mut m, send),
            StreamAction::Submit(question) if question == "and y?"
        ));
        assert_eq!(m.input_text(), "", "a sent question leaves the input");
        assert!(m.is_open());

        let close = m.footer_hit(CLOSE);
        assert!(matches!(click(&mut m, close), StreamAction::Consumed));
        assert!(!m.is_open());
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
            m.exchanges[0].body == ANSWER,
            "an error in the follow-up leaves the first answer alone"
        );
    }

    #[test]
    fn enter_submits_the_input_once_the_answer_has_landed() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        type_text(&mut m, "and y? ");
        assert!(
            matches!(m.handle_key(key_ev(KeyCode::Enter)), StreamAction::Consumed),
            "a follow-up waits for the answer it follows up"
        );
        assert_eq!(m.input_text(), "and y? ", "the draft survives the wait");

        tx.send(done()).unwrap();
        let _ = m.poll();
        assert!(matches!(
            m.handle_key(key_ev(KeyCode::Enter)),
            StreamAction::Submit(question) if question == "and y?"
        ));
        assert_eq!(m.input_text(), "", "a sent question leaves the input");
        assert!(m.is_open());
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

    #[test]
    fn the_follow_up_input_is_drawn_under_the_thread() {
        let mut m = StreamModal::new(0);
        let (_tx, _cancel) = open_modal(&mut m, HEADER, StreamFooter::FollowUp);
        type_text(&mut m, "next");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                m.view(frame, frame.area());
            })
            .unwrap();
        let screen = terminal.backend().to_string();
        assert!(screen.contains(HEADER));
        assert!(screen.contains("> next"));
        assert!(screen.contains(SEND_HINT.trim()));
    }
}
