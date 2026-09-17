use crate::components::ModalScroll;
use crate::components::Overlay;
use crate::components::modal::{FooterHits, FooterLine, Modal};
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::streaming_content::StreamingContent;
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

/// What a finished side request cost. It reaches the session ledger through
/// [`StreamModal::take_usage`] rather than the agent event channel, because a
/// side request never produces a turn or a chat bubble to hang it on.
pub struct StreamUsage {
    pub usage: TokenUsage,
    pub cost: Option<f64>,
    pub billing: Billing,
    pub model: String,
    pub provider: String,
    pub purpose: LedgerPurpose,
}

pub enum StreamEvent {
    /// Replaces the header once the request knows something it did not at
    /// open time, such as which model answered.
    Header(String),
    TextDelta(String),
    Done(StreamUsage),
    Error(String),
}

/// What the pointer did to the modal. `Copy` carries the text the footer
/// control asked the host to hand to the clipboard.
pub enum StreamMouse {
    Ignored,
    Consumed,
    Copy(String),
}

/// A modal that draws one model answer as it streams: `/btw` and `/extract`
/// both open it, with their own title and header. `copyable` adds a footer
/// that hands the text to the clipboard, for answers meant to leave the modal.
pub struct StreamModal {
    open: bool,
    title: &'static str,
    header: String,
    body: StreamingContent,
    copyable: bool,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    footer: FooterHits,
    rx: Option<flume::Receiver<StreamEvent>>,
    /// Dropping this cancels the in-flight request, so every teardown path that
    /// already funnels through [`StreamModal::close`] cancels for free.
    cancel: Option<CancelTrigger>,
    pending_usage: Option<StreamUsage>,
    popup: Rect,
}

impl StreamModal {
    pub fn new(ms_per_char: u64) -> Self {
        let theme = theme::current();
        Self {
            open: false,
            title: "",
            header: String::new(),
            body: StreamingContent::new_noninteractive(
                "",
                theme.assistant,
                theme.assistant,
                ms_per_char,
            ),
            copyable: false,
            scroll: ModalScroll::new(),
            scrollbar: Scrollbar::default(),
            footer: FooterHits::default(),
            rx: None,
            cancel: None,
            pending_usage: None,
            popup: Rect::default(),
        }
    }

    pub fn open(
        &mut self,
        title: &'static str,
        header: String,
        copyable: bool,
        rx: flume::Receiver<StreamEvent>,
        cancel: CancelTrigger,
    ) {
        self.close();
        self.open = true;
        self.title = title;
        self.header = header;
        self.copyable = copyable;
        self.rx = Some(rx);
        self.cancel = Some(cancel);
    }

    /// Leaves `pending_usage` alone: a call that finished just before the user
    /// dismissed the modal still has to be billed.
    pub fn close(&mut self) {
        self.open = false;
        self.header.clear();
        self.body.clear();
        self.scroll.reset();
        self.footer.reset();
        self.rx = None;
        self.cancel = None;
    }

    pub fn take_usage(&mut self) -> Option<StreamUsage> {
        self.pending_usage.take()
    }

    pub fn is_streaming(&self) -> bool {
        self.rx.is_some()
    }

    /// Only the typewriter moves on its own, and only while it is on screen.
    /// A pending stream is drained by [`Self::poll`], which reports its own
    /// [`Dirty`].
    pub fn cadence(&self) -> Cadence {
        Cadence::when(self.open && self.body.is_animating(), Cadence::SMOOTH)
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    /// Everything streamed so far, whether or not the reveal has drawn it.
    pub fn text(&self) -> &str {
        self.body.buffer()
    }

    pub fn poll(&mut self) -> Dirty {
        let Some(ref rx) = self.rx else {
            return Dirty::NO;
        };
        let mut dirty = Dirty::NO;
        while let Ok(event) = rx.try_recv() {
            dirty = Dirty::YES;
            match event {
                StreamEvent::Header(header) => self.header = header,
                StreamEvent::TextDelta(text) => self.body.push(&text),
                StreamEvent::Done(usage) => {
                    self.pending_usage = Some(usage);
                    self.finish_stream();
                    break;
                }
                StreamEvent::Error(msg) => {
                    self.body.clear();
                    self.body.push(&msg);
                    self.finish_stream();
                    break;
                }
            }
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

    /// The bar and the footer are all the modal reads from the pointer.
    pub fn handle_mouse(&mut self, event: &MouseEvent) -> StreamMouse {
        match self.scrollbar.handle(event) {
            ScrollbarMouse::Ignored => {}
            ScrollbarMouse::Consumed => return StreamMouse::Consumed,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                return StreamMouse::Consumed;
            }
        }
        if !self.copyable {
            return StreamMouse::Ignored;
        }
        match self.footer.handle_mouse(*event) {
            Some(_) => StreamMouse::Copy(self.text().to_owned()),
            None => StreamMouse::Ignored,
        }
    }

    /// Returns the text to copy when the key asked for it, and only on a
    /// copyable modal: elsewhere `y` is just another key that keeps it open.
    pub fn handle_key(&mut self, key_event: KeyEvent) -> Option<String> {
        match key_event.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') => {
                self.close();
            }
            KeyCode::Char('y') if self.copyable => {
                return Some(self.text().to_owned());
            }
            _ => {
                self.scroll.handle_key(key_event);
            }
        }
        None
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }

        let theme = theme::current();
        let padded_width = Modal::inner_width(area.width, WIDTH_PERCENT).saturating_sub(H_PAD * 2);

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(Span::styled(
            self.header.clone(),
            theme.tool_dim,
        )));
        lines.push(Line::default());

        let md_lines = self.body.render_lines(padded_width);
        lines.extend_from_slice(md_lines);
        let footer = self.copyable.then(footer);
        if let Some(footer) = &footer {
            lines.push(Line::default());
            lines.push(footer.line(self.footer.hovered()));
        }

        let total = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(padded_width) as u16;
        let modal = Modal {
            title: self.title,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, total);
        let padded = Rect {
            x: inner.x + H_PAD,
            width: inner.width.saturating_sub(H_PAD * 2),
            ..inner
        };
        let viewport_h = padded.height;
        self.scroll.update_dimensions(total, viewport_h);
        let scroll = self.scroll.offset();
        if let Some(footer) = &footer {
            self.footer.set(footer.hits(padded, scroll, total));
        }

        let paragraph = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        frame.render_widget(paragraph, padded);

        self.scrollbar.draw(frame, inner, total, scroll);

        self.popup = popup;
        popup
    }

    #[cfg(test)]
    pub fn body_eq(&self, expected: &str) -> bool {
        self.body == expected
    }

    #[cfg(test)]
    pub(crate) fn footer_hit(&self) -> Rect {
        self.footer.hit(0)
    }
}

fn footer() -> FooterLine {
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

    fn open_modal(
        m: &mut StreamModal,
        header: &str,
        copyable: bool,
    ) -> (flume::Sender<StreamEvent>, CancelToken) {
        let (tx, rx) = flume::bounded(64);
        let (trigger, cancel) = CancelToken::new();
        m.open(TITLE, header.to_owned(), copyable, rx, trigger);
        (tx, cancel)
    }

    fn done() -> StreamEvent {
        StreamEvent::Done(StreamUsage {
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
        })
    }

    #[test]
    fn open_sets_header_and_state() {
        let mut m = StreamModal::new(0);
        let (_tx, _cancel) = open_modal(&mut m, HEADER, false);
        assert!(m.is_open());
        assert_eq!(m.header, HEADER);
        assert!(m.body.is_empty());
        assert!(m.is_streaming());
    }

    #[test]
    fn close_resets_all_fields() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", false);
        tx.send(StreamEvent::TextDelta("some answer".into()))
            .unwrap();
        let _ = m.poll();
        m.scroll.update_dimensions(100, 10);
        m.scroll.scroll(-5);
        m.close();
        assert!(!m.is_open());
        assert!(m.header.is_empty());
        assert!(m.body.is_empty());
        assert_eq!(m.scroll.offset(), 0);
        assert!(!m.is_streaming());
    }

    #[test]
    fn poll_accumulates_text() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", false);
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
        let (tx, _cancel) = open_modal(&mut m, HEADER, true);
        tx.send(StreamEvent::Header(LATER.into())).unwrap();
        assert_eq!(m.poll(), Dirty::YES);
        assert_eq!(m.header, LATER);
        assert!(m.is_streaming());
    }

    #[test]
    fn poll_done_sets_done_and_drops_rx() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", false);
        tx.send(done()).unwrap();
        let _ = m.poll();
        assert!(!m.is_streaming());
    }

    #[test]
    fn poll_done_surfaces_usage_exactly_once() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", false);
        tx.send(done()).unwrap();
        let _ = m.poll();
        let billed = m.take_usage().expect("usage reaches the ledger");
        assert_eq!(billed.usage.input, 10);
        assert_eq!(billed.cost, Some(0.5));
        assert_eq!(billed.model, MODEL);
        assert_eq!(billed.purpose, LedgerPurpose::Btw);
        assert!(m.take_usage().is_none(), "usage is never billed twice");
    }

    #[test]
    fn close_keeps_usage_that_already_arrived() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", false);
        tx.send(done()).unwrap();
        let _ = m.poll();
        m.close();
        assert!(
            m.take_usage().is_some(),
            "dismissing after the answer landed must not discard the bill"
        );
    }

    #[test]
    fn close_cancels_the_in_flight_request() {
        let mut m = StreamModal::new(0);
        let (_tx, cancel) = open_modal(&mut m, "q", false);
        assert!(!cancel.is_cancelled());
        m.close();
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn poll_error_replaces_body_and_marks_done() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", false);
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
        let (_tx, cancel) = open_modal(&mut m, "q", false);
        assert!(m.handle_key(key_ev(code)).is_none());
        assert!(!m.is_open());
        assert!(!m.is_streaming());
        assert!(cancel.is_cancelled(), "dismissing stops the request");
    }

    #[test]
    fn other_keys_consumed_but_stay_open() {
        let mut m = StreamModal::new(0);
        let (_tx, _cancel) = open_modal(&mut m, "q", false);
        assert!(m.handle_key(key_ev(KeyCode::Char('a'))).is_none());
        assert!(m.is_open());
    }

    /// The copy key hands over what has streamed so far, even mid-stream: a
    /// partial list is still worth pasting.
    #[test_case(true, Some("partial") ; "copyable_copies")]
    #[test_case(false, None ; "plain_ignores_the_key")]
    fn y_copies_only_when_copyable(copyable: bool, expected: Option<&str>) {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", copyable);
        tx.send(StreamEvent::TextDelta("partial".into())).unwrap();
        let _ = m.poll();

        let copied = m.handle_key(key_ev(KeyCode::Char('y')));

        assert_eq!(copied.as_deref(), expected);
        assert!(m.is_open(), "copying leaves the modal up");
    }

    #[test]
    fn the_footer_copy_answers_a_click() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", true);
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
        let (_tx, _cancel) = open_modal(&mut m, "q", false);
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
        let (tx1, first_cancel) = open_modal(&mut m, "first", false);
        tx1.send(StreamEvent::TextDelta("leftover".into())).unwrap();
        let _ = m.poll();
        m.scroll.update_dimensions(100, 10);
        m.scroll.scroll(-10);
        let (_tx2, _cancel2) = open_modal(&mut m, "second", false);
        assert!(m.is_open());
        assert_eq!(m.header, "second");
        assert!(m.body.is_empty());
        assert_eq!(m.scroll.offset(), 0);
        assert!(
            first_cancel.is_cancelled(),
            "a second question stops the first request"
        );
    }

    #[test]
    fn close_drops_rx_signaling_sender() {
        let mut m = StreamModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q", false);
        m.close();
        assert!(tx.send(StreamEvent::TextDelta("x".into())).is_err());
    }

    #[test]
    fn poll_noop_when_no_rx() {
        let mut m = StreamModal::new(0);
        let _ = m.poll();
        assert!(!m.is_open());
    }
}
