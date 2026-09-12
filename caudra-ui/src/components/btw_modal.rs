use crate::components::ModalScroll;
use crate::components::Overlay;
use crate::components::modal::Modal;
use crate::components::scrollbar::{Scrollbar, ScrollbarMouse};
use crate::components::streaming_content::StreamingContent;
use crate::theme;

use caudra_agent::CancelTrigger;
use caudra_providers::{Billing, TokenUsage};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::repaint::{Cadence, Dirty};

const TITLE: &str = " /btw ";
const H_PAD: u16 = 2;
const WIDTH_PERCENT: u16 = 65;
const MAX_HEIGHT_PERCENT: u16 = 80;

/// What a finished btw call cost. It reaches the session ledger through
/// [`BtwModal::take_usage`] rather than the agent event channel, because btw
/// never produces a turn or a chat bubble to hang it on.
pub struct BtwUsage {
    pub usage: TokenUsage,
    pub cost: Option<f64>,
    pub billing: Billing,
    pub model: String,
    pub provider: String,
}

pub enum BtwEvent {
    TextDelta(String),
    Done(BtwUsage),
    Error(String),
}

pub struct BtwModal {
    open: bool,
    question: String,
    answer: StreamingContent,
    scroll: ModalScroll,
    scrollbar: Scrollbar,
    rx: Option<flume::Receiver<BtwEvent>>,
    /// Dropping this cancels the in-flight request, so every teardown path that
    /// already funnels through [`BtwModal::close`] cancels for free.
    cancel: Option<CancelTrigger>,
    pending_usage: Option<BtwUsage>,
    popup: Rect,
}

impl BtwModal {
    pub fn new(ms_per_char: u64) -> Self {
        let theme = theme::current();
        Self {
            open: false,
            question: String::new(),
            answer: StreamingContent::new_noninteractive(
                "",
                theme.assistant,
                theme.assistant,
                ms_per_char,
            ),
            scroll: ModalScroll::new(),
            scrollbar: Scrollbar::default(),
            rx: None,
            cancel: None,
            pending_usage: None,
            popup: Rect::default(),
        }
    }

    pub fn open(&mut self, question: &str, rx: flume::Receiver<BtwEvent>, cancel: CancelTrigger) {
        self.close();
        self.open = true;
        self.question = question.to_string();
        self.rx = Some(rx);
        self.cancel = Some(cancel);
    }

    /// Leaves `pending_usage` alone: a call that finished just before the user
    /// dismissed the modal still has to be billed.
    pub fn close(&mut self) {
        self.open = false;
        self.question.clear();
        self.answer.clear();
        self.scroll.reset();
        self.rx = None;
        self.cancel = None;
    }

    pub fn take_usage(&mut self) -> Option<BtwUsage> {
        self.pending_usage.take()
    }

    pub fn is_streaming(&self) -> bool {
        self.rx.is_some()
    }

    /// Only the typewriter moves on its own, and only while it is on screen.
    /// A pending stream is drained by [`Self::poll`], which reports its own
    /// [`Dirty`].
    pub fn cadence(&self) -> Cadence {
        Cadence::when(self.open && self.answer.is_animating(), Cadence::SMOOTH)
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.open && self.popup.contains(pos)
    }

    pub fn poll(&mut self) -> Dirty {
        let Some(ref rx) = self.rx else {
            return Dirty::NO;
        };
        let mut dirty = Dirty::NO;
        while let Ok(event) = rx.try_recv() {
            dirty = Dirty::YES;
            match event {
                BtwEvent::TextDelta(text) => self.answer.push(&text),
                BtwEvent::Done(usage) => {
                    self.pending_usage = Some(usage);
                    self.finish_stream();
                    break;
                }
                BtwEvent::Error(msg) => {
                    self.answer.clear();
                    self.answer.push(&msg);
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

    /// The modal reads nothing else from the pointer, so the bar is all there
    /// is to offer and a bool is all there is to say.
    pub fn handle_mouse(&mut self, event: &MouseEvent) -> bool {
        match self.scrollbar.handle(event) {
            ScrollbarMouse::Ignored => false,
            ScrollbarMouse::Consumed => true,
            ScrollbarMouse::ScrollTo(top) => {
                self.scroll.scroll_to(top as u16);
                true
            }
        }
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') => {
                self.close();
            }
            _ => {
                self.scroll.handle_key(key_event);
            }
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }

        let theme = theme::current();
        let padded_width =
            Modal::inner_width(area.width, WIDTH_PERCENT).saturating_sub(H_PAD * 2);

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(Span::styled(
            format!("Q: {}", self.question),
            theme.tool_dim,
        )));
        lines.push(Line::default());

        let md_lines = self.answer.render_lines(padded_width);
        lines.extend_from_slice(md_lines);

        let total = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(padded_width) as u16;
        let modal = Modal {
            title: TITLE,
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

        let paragraph = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        frame.render_widget(paragraph, padded);

        self.scrollbar.draw(frame, inner, total, scroll);

        self.popup = popup;
        popup
    }

    #[cfg(test)]
    pub fn answer_eq(&self, expected: &str) -> bool {
        self.answer == expected
    }
}

impl Overlay for BtwModal {
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
    use crossterm::event::KeyCode;
    use test_case::test_case;

    const MODEL: &str = "test-model";
    const PROVIDER: &str = "anthropic";

    fn open_modal(m: &mut BtwModal, question: &str) -> (flume::Sender<BtwEvent>, CancelToken) {
        let (tx, rx) = flume::bounded(64);
        let (trigger, cancel) = CancelToken::new();
        m.open(question, rx, trigger);
        (tx, cancel)
    }

    fn done() -> BtwEvent {
        BtwEvent::Done(BtwUsage {
            usage: TokenUsage {
                input: 10,
                output: 20,
                ..Default::default()
            },
            cost: Some(0.5),
            billing: Billing::Api,
            provider: PROVIDER.into(),
            model: MODEL.into(),
        })
    }

    #[test]
    fn open_sets_question_and_state() {
        let mut m = BtwModal::new(0);
        let (_tx, _cancel) = open_modal(&mut m, "why?");
        assert!(m.is_open());
        assert_eq!(m.question, "why?");
        assert!(m.answer.is_empty());
        assert!(m.is_streaming());
    }

    #[test]
    fn close_resets_all_fields() {
        let mut m = BtwModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q");
        tx.send(BtwEvent::TextDelta("some answer".into())).unwrap();
        let _ = m.poll();
        m.scroll.update_dimensions(100, 10);
        m.scroll.scroll(-5);
        m.close();
        assert!(!m.is_open());
        assert!(m.question.is_empty());
        assert!(m.answer.is_empty());
        assert_eq!(m.scroll.offset(), 0);
        assert!(!m.is_streaming());
    }

    #[test]
    fn poll_accumulates_text() {
        let mut m = BtwModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q");
        tx.send(BtwEvent::TextDelta("hello ".into())).unwrap();
        tx.send(BtwEvent::TextDelta("world".into())).unwrap();
        let _ = m.poll();
        assert!(m.answer_eq("hello world"));
    }

    #[test]
    fn poll_done_sets_done_and_drops_rx() {
        let mut m = BtwModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q");
        tx.send(done()).unwrap();
        let _ = m.poll();
        assert!(!m.is_streaming());
    }

    #[test]
    fn poll_done_surfaces_usage_exactly_once() {
        let mut m = BtwModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q");
        tx.send(done()).unwrap();
        let _ = m.poll();
        let billed = m.take_usage().expect("usage reaches the ledger");
        assert_eq!(billed.usage.input, 10);
        assert_eq!(billed.cost, Some(0.5));
        assert_eq!(billed.model, MODEL);
        assert!(m.take_usage().is_none(), "usage is never billed twice");
    }

    #[test]
    fn close_keeps_usage_that_already_arrived() {
        let mut m = BtwModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q");
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
        let mut m = BtwModal::new(0);
        let (_tx, cancel) = open_modal(&mut m, "q");
        assert!(!cancel.is_cancelled());
        m.close();
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn poll_error_replaces_answer_and_marks_done() {
        let mut m = BtwModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q");
        tx.send(BtwEvent::TextDelta("partial".into())).unwrap();
        tx.send(BtwEvent::Error("oops".into())).unwrap();
        let _ = m.poll();
        assert!(m.answer_eq("oops"));
        assert!(!m.is_streaming());
    }

    #[test_case(KeyCode::Esc   ; "esc_closes")]
    #[test_case(KeyCode::Enter ; "enter_closes")]
    #[test_case(KeyCode::Char(' ') ; "space_closes")]
    fn dismiss_keys_close(code: KeyCode) {
        let mut m = BtwModal::new(0);
        let (_tx, cancel) = open_modal(&mut m, "q");
        m.handle_key(key_ev(code));
        assert!(!m.is_open());
        assert!(!m.is_streaming());
        assert!(cancel.is_cancelled(), "dismissing stops the request");
    }

    #[test]
    fn other_keys_consumed_but_stay_open() {
        let mut m = BtwModal::new(0);
        let (_tx, _cancel) = open_modal(&mut m, "q");
        m.handle_key(key_ev(KeyCode::Char('a')));
        assert!(m.is_open());
    }

    #[test]
    fn scroll_up_down() {
        let mut m = BtwModal::new(0);
        let (_tx, _cancel) = open_modal(&mut m, "q");
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
        let mut m = BtwModal::new(0);
        let (tx1, first_cancel) = open_modal(&mut m, "first");
        tx1.send(BtwEvent::TextDelta("leftover".into())).unwrap();
        let _ = m.poll();
        m.scroll.update_dimensions(100, 10);
        m.scroll.scroll(-10);
        let (_tx2, _cancel2) = open_modal(&mut m, "second");
        assert!(m.is_open());
        assert_eq!(m.question, "second");
        assert!(m.answer.is_empty());
        assert_eq!(m.scroll.offset(), 0);
        assert!(
            first_cancel.is_cancelled(),
            "a second question stops the first request"
        );
    }

    #[test]
    fn close_drops_rx_signaling_sender() {
        let mut m = BtwModal::new(0);
        let (tx, _cancel) = open_modal(&mut m, "q");
        m.close();
        assert!(tx.send(BtwEvent::TextDelta("x".into())).is_err());
    }

    #[test]
    fn poll_noop_when_no_rx() {
        let mut m = BtwModal::new(0);
        let _ = m.poll();
        assert!(!m.is_open());
    }
}
