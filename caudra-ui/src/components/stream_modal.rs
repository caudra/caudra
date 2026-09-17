use crate::components::ModalScroll;
use crate::components::Overlay;
use crate::components::modal::{FooterHits, FooterLine, Modal};
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
const CLOSE_HINT: &str = "   Esc Close";
const INPUT_PREFIX: &str = "> ";
const SEND_LABEL: &str = "Enter";
const SEND_HINT: &str = " Send a follow-up";
/// The input line and the hint under it, kept out of the scroll region so a
/// long thread never scrolls the prompt away.
const INPUT_ROWS: u16 = 2;

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

/// What sits under the answer, and so what the keys do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamFooter {
    /// Enter, Space, or Esc dismisses.
    Close,
    /// `y` or a click on the footer copies the text; Enter, Space, or Esc dismisses.
    Copy,
    /// A single-line input takes the keys: Enter sends what was typed as the
    /// next question in the same thread, Esc closes, an empty Enter closes.
    FollowUp,
}

/// What the pointer did to the modal. `Copy` carries the text the footer
/// control asked the host to hand to the clipboard.
pub enum StreamMouse {
    Ignored,
    Consumed,
    Copy(String),
}

/// What a key asked of the host. `Copy` carries the text for the clipboard,
/// `Submit` the follow-up the input held.
pub enum StreamKey {
    Handled,
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
    copy_hits: FooterHits,
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
            copy_hits: FooterHits::default(),
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
        self.copy_hits.reset();
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

    /// The bar and the copy footer are all the modal reads from the pointer.
    pub fn handle_mouse(&mut self, event: &MouseEvent) -> StreamMouse {
        match self.scrollbar.handle(event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return StreamMouse::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return StreamMouse::Consumed;
            }
        }
        if self.footer != StreamFooter::Copy {
            return StreamMouse::Ignored;
        }
        match self.copy_hits.handle_mouse(*event) {
            Some(_) => StreamMouse::Copy(self.text().to_owned()),
            None => StreamMouse::Ignored,
        }
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> StreamKey {
        if self.footer == StreamFooter::FollowUp {
            return self.handle_follow_up_key(key_event);
        }
        match key_event.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') => self.close(),
            KeyCode::Char('y') if self.footer == StreamFooter::Copy => {
                return StreamKey::Copy(self.text().to_owned());
            }
            _ => {
                self.scroll.handle_key(key_event);
            }
        }
        StreamKey::Handled
    }

    /// The input owns every key but the four that move the thread: `y` and
    /// Space are text here. Enter is ignored while an answer is still
    /// streaming, so a follow-up cannot cancel the answer it is following up.
    fn handle_follow_up_key(&mut self, key_event: KeyEvent) -> StreamKey {
        match key_event.code {
            KeyCode::Esc => self.close(),
            KeyCode::Enter => {
                let question = self.input.value().trim().to_owned();
                if question.is_empty() {
                    self.close();
                } else if !self.is_streaming() {
                    self.input.clear();
                    return StreamKey::Submit(question);
                }
            }
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                self.scroll.handle_key(key_event);
            }
            _ => {
                self.input.handle_key(key_event);
            }
        }
        StreamKey::Handled
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
        let copy_footer = (self.footer == StreamFooter::Copy).then(copy_footer);
        if let Some(footer) = &copy_footer {
            lines.push(Line::default());
            lines.push(footer.line(self.copy_hits.hovered()));
        }
        let input_rows = if self.footer == StreamFooter::FollowUp {
            INPUT_ROWS
        } else {
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
        if let Some(footer) = &copy_footer {
            self.copy_hits.set(footer.hits(viewport, scroll, total));
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
            let input_area = Rect {
                y: viewport.y + viewport.height,
                height: padded.height - viewport.height,
                ..padded
            };
            frame.render_widget(Paragraph::new(self.input_lines()), input_area);
        }

        self.popup = popup;
        popup
    }

    fn input_lines(&self) -> Vec<Line<'static>> {
        let theme = theme::current();
        let text = self.input.value();
        let cursor_byte = TextBuffer::char_to_byte(&text, self.input.x());
        let (before, rest) = text.split_at(cursor_byte);
        let mut chars = rest.chars();
        let cursor_char = chars.next().unwrap_or(' ');
        let after = chars.as_str();
        let style = super::input_text_style();
        let prompt = Line::from(vec![
            Span::styled(INPUT_PREFIX, theme.tool_dim),
            Span::styled(before.to_owned(), style),
            Span::styled(cursor_char.to_string(), theme.cursor),
            Span::styled(after.to_owned(), style),
        ]);
        let mut hint = FooterLine::default();
        hint.text(SEND_LABEL, theme.keybind_key);
        hint.text(SEND_HINT, theme.tool_dim);
        hint.text(CLOSE_HINT, theme.tool_dim);
        vec![prompt, hint.line(None)]
    }

    #[cfg(test)]
    pub fn body_eq(&self, expected: &str) -> bool {
        self.exchanges.last().is_some_and(|e| e.body == expected)
    }

    #[cfg(test)]
    pub(crate) fn footer_hit(&self) -> Rect {
        self.copy_hits.hit(0)
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

fn copy_footer() -> FooterLine {
    let theme = theme::current();
    let mut footer = FooterLine::default();
    footer.command(COPY_LABEL, theme.keybind_key);
    footer.text(COPY_HINT, theme.tool_dim);
    footer.text(CLOSE_HINT, theme.tool_dim);
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
    use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use test_case::test_case;

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
        assert!(matches!(m.handle_key(key_ev(code)), StreamKey::Handled));
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
            StreamKey::Handled
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
            StreamKey::Copy(text) => Some(text),
            _ => None,
        };

        assert_eq!(copied.as_deref(), expected);
        assert!(m.is_open(), "copying leaves the modal up");
    }

    #[test]
    fn the_footer_copy_answers_a_click() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", StreamFooter::Copy);
        tx.send(StreamEvent::TextDelta("list".into())).unwrap();
        let _ = m.poll();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                m.view(frame, frame.area());
            })
            .unwrap();
        let hit = m.footer_hit();
        assert!(hit.width > 0, "the copy control is drawn");
        let at = |kind| MouseEvent {
            kind,
            column: hit.x,
            row: hit.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };

        assert!(matches!(
            m.handle_mouse(&at(MouseEventKind::Down(MouseButton::Left))),
            StreamMouse::Ignored
        ));
        assert!(matches!(
            m.handle_mouse(&at(MouseEventKind::Up(MouseButton::Left))),
            StreamMouse::Copy(text) if text == "list"
        ));
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
            matches!(m.handle_key(key_ev(KeyCode::Enter)), StreamKey::Handled),
            "a follow-up waits for the answer it follows up"
        );
        assert_eq!(m.input_text(), "and y? ", "the draft survives the wait");

        tx.send(done()).unwrap();
        let _ = m.poll();
        assert!(matches!(
            m.handle_key(key_ev(KeyCode::Enter)),
            StreamKey::Submit(question) if question == "and y?"
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
        assert!(matches!(m.handle_key(key_ev(code)), StreamKey::Handled));
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
