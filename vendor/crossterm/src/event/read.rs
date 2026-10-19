#[cfg(unix)]
use crate::event::TerminalGraphics;
#[cfg(unix)]
use crate::event::filter::{KittyGraphicsFilter, ProgramStatusFilter};
#[cfg(unix)]
use std::io::Write;
use std::{collections::vec_deque::VecDeque, io, time::Duration};

#[cfg(unix)]
use crate::event::source::unix::UnixInternalEventSource;
#[cfg(windows)]
use crate::event::source::windows::WindowsEventSource;
#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{
    filter::Filter, internal::InternalEvent, source::EventSource, timeout::PollTimeout,
};

#[cfg(unix)]
const GRAPHICS_QUERY_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

/// Can be used to read `InternalEvent`s.
pub(crate) struct InternalEventReader {
    events: VecDeque<InternalEvent>,
    source: Option<Box<dyn EventSource>>,
    skipped_events: Vec<InternalEvent>,
}

impl Default for InternalEventReader {
    fn default() -> Self {
        #[cfg(windows)]
        let source = WindowsEventSource::new();
        #[cfg(unix)]
        let source = UnixInternalEventSource::new();

        let source = source.ok().map(|x| Box::new(x) as Box<dyn EventSource>);

        InternalEventReader {
            source,
            events: VecDeque::with_capacity(32),
            skipped_events: Vec::with_capacity(32),
        }
    }
}

impl InternalEventReader {
    #[cfg(unix)]
    pub(crate) fn query_program_status(
        &mut self,
        timeout: Duration,
        output: &mut impl Write,
    ) -> io::Result<bool> {
        const QUERY: &[u8] = b"\x1b]7501;?\x1b\\\x1b[c";
        if let Some(source) = self.source.as_mut() {
            source.set_program_status_query(true);
        }
        let result = (|| {
            let timeout = PollTimeout::new(Some(timeout));
            output.write_all(QUERY)?;
            output.flush()?;
            let mut supported = false;
            while self.poll(timeout.leftover(), &ProgramStatusFilter)? {
                match self.try_read(&ProgramStatusFilter) {
                    Some(InternalEvent::ProgramStatusSupported) => supported = true,
                    Some(InternalEvent::PrimaryDeviceAttributes) => return Ok(supported),
                    _ => return Ok(false),
                }
                if timeout.elapsed() {
                    break;
                }
            }
            Ok(supported)
        })();
        if let Some(source) = self.source.as_mut() {
            source.set_program_status_query(false);
        }
        result
    }

    #[cfg(unix)]
    pub(crate) fn query_terminal_graphics(
        &mut self,
        timeout: Duration,
        output: &mut impl Write,
        id: u32,
    ) -> io::Result<TerminalGraphics> {
        if timeout.is_zero() {
            return Ok(TerminalGraphics::default());
        }
        self.events.retain(|event| !KittyGraphicsFilter.eval(event));
        if let Some(source) = self.source.as_mut() {
            source.set_graphics_query(true);
        }
        let result = (|| {
            let timeout = PollTimeout::new(Some(timeout));
            write!(
                output,
                "\x1b_Gi={id},a=q,t=d,f=100;{GRAPHICS_QUERY_PNG}\x1b\\\x1b[16t\x1b[c"
            )?;
            output.flush()?;
            let mut reply = TerminalGraphics::default();
            while self.poll(timeout.leftover(), &KittyGraphicsFilter)? {
                match self.try_read(&KittyGraphicsFilter) {
                    Some(InternalEvent::KittyGraphicsReply(reply_id, ok)) if reply_id == id => {
                        reply.kitty = ok
                    }
                    Some(InternalEvent::CellSize(width, height)) => {
                        reply.cell_pixels = Some((width, height))
                    }
                    Some(
                        InternalEvent::PrimaryDeviceAttributes | InternalEvent::GraphicsQueryLimit,
                    ) => break,
                    _ => {}
                }
                if timeout.elapsed() {
                    break;
                }
            }
            Ok(reply)
        })();
        if let Some(source) = self.source.as_mut() {
            source.set_graphics_query(false);
        }
        result
    }

    /// Returns a `Waker` allowing to wake/force the `poll` method to return `Ok(false)`.
    #[cfg(feature = "event-stream")]
    pub(crate) fn waker(&self) -> Waker {
        self.source.as_ref().expect("reader source not set").waker()
    }

    pub(crate) fn poll<F>(&mut self, timeout: Option<Duration>, filter: &F) -> io::Result<bool>
    where
        F: Filter,
    {
        for event in &self.events {
            if filter.eval(event) {
                return Ok(true);
            }
        }

        let event_source = match self.source.as_mut() {
            Some(source) => source,
            None => return Err(std::io::Error::other("Failed to initialize input reader")),
        };

        let poll_timeout = PollTimeout::new(timeout);

        loop {
            if self.events.len().saturating_add(self.skipped_events.len()) >= filter.pending_limit()
            {
                self.events.extend(self.skipped_events.drain(..));
                return Ok(false);
            }
            let maybe_event = match event_source.try_read(poll_timeout.leftover()) {
                Ok(None) => {
                    self.events.extend(self.skipped_events.drain(..));
                    return Ok(false);
                }
                Ok(Some(event)) => {
                    if filter.eval(&event) {
                        Some(event)
                    } else {
                        self.skipped_events.push(event);
                        None
                    }
                }
                Err(e) => {
                    self.events.extend(self.skipped_events.drain(..));
                    if e.kind() == io::ErrorKind::Interrupted {
                        return Ok(false);
                    }

                    return Err(e);
                }
            };

            if poll_timeout.elapsed() || maybe_event.is_some() {
                self.events.extend(self.skipped_events.drain(..));

                if let Some(event) = maybe_event {
                    self.events.push_front(event);
                    return Ok(true);
                }

                return Ok(false);
            }
        }
    }

    /// Blocks the thread until a valid `InternalEvent` can be read.
    ///
    /// Internally, we use `try_read`, which buffers the events that do not fulfill the filter
    /// conditions to prevent stalling the thread in an infinite loop.
    pub(crate) fn read<F>(&mut self, filter: &F) -> io::Result<InternalEvent>
    where
        F: Filter,
    {
        // blocks the thread until a valid event is found
        loop {
            if let Some(event) = self.try_read(filter) {
                return Ok(event);
            }

            let _ = self.poll(None, filter)?;
        }
    }

    /// Attempts to read the first valid `InternalEvent`.
    pub(crate) fn try_read<F>(&mut self, filter: &F) -> Option<InternalEvent>
    where
        F: Filter,
    {
        let index = self.events.iter().position(|event| filter.eval(event))?;
        self.events.remove(index)
    }
}

#[cfg(all(test, unix))]
mod program_status_tests {
    use super::InternalEventReader;
    #[cfg(feature = "event-stream")]
    use crate::event::sys::Waker;
    use crate::event::{
        Event, KeyCode, filter::EventFilter, internal::InternalEvent, source::EventSource,
        sys::parse::Parser,
    };
    use std::{collections::VecDeque, io, time::Duration};
    use test_case::test_case;

    const TIMEOUT: Duration = Duration::from_secs(1);
    const SUPPORTED: &[u8] = b"\x1b]7501;?\x1b\\";
    const DA1: &[u8] = b"\x1b[?1;2c";
    const QUERY: &[u8] = b"\x1b]7501;?\x1b\\\x1b[c";

    struct InputSource {
        parser: Parser,
        chunks: VecDeque<Vec<u8>>,
        error: Option<io::ErrorKind>,
    }

    impl EventSource for InputSource {
        fn set_program_status_query(&mut self, enabled: bool) {
            self.parser.set_program_status_query(enabled);
        }

        fn set_graphics_query(&mut self, enabled: bool) {
            self.parser.set_graphics_query(enabled);
        }

        fn try_read(&mut self, _timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
            loop {
                if let Some(event) = self.parser.next() {
                    return Ok(Some(event));
                }
                if let Some(chunk) = self.chunks.pop_front() {
                    self.parser.advance(&chunk, false);
                } else {
                    return match self.error.take() {
                        Some(error) => Err(error.into()),
                        None => Ok(None),
                    };
                }
            }
        }

        #[cfg(feature = "event-stream")]
        fn waker(&self) -> Waker {
            unimplemented!()
        }
    }

    fn reader(
        input: &[u8],
        chunk_size: usize,
        error: Option<io::ErrorKind>,
    ) -> InternalEventReader {
        InternalEventReader {
            events: VecDeque::new(),
            skipped_events: Vec::new(),
            source: Some(Box::new(InputSource {
                parser: Parser::default(),
                chunks: input.chunks(chunk_size).map(<[u8]>::to_vec).collect(),
                error,
            })),
        }
    }

    fn remaining_input(reader: &mut InternalEventReader) -> Vec<InternalEvent> {
        let mut events = Vec::new();
        while reader.poll(Some(Duration::ZERO), &EventFilter).unwrap() {
            events.push(reader.try_read(&EventFilter).unwrap());
        }
        events
    }

    #[cfg(feature = "bracketed-paste")]
    #[test_case(1; "byte_at_a_time")]
    #[test_case(4; "fragmented")]
    #[test_case(usize::MAX; "coalesced")]
    fn program_status_query_preserves_interleaved_input_and_consumes_fence(chunk_size: usize) {
        let input = [
            "a界\x1b[I".as_bytes(),
            SUPPORTED,
            b"b\x1b[200~literal",
            SUPPORTED,
            b"\x1b[201~\x1b[O",
            DA1,
            b"z",
        ]
        .concat();
        let mut reader = reader(&input, chunk_size, None);
        let mut output = Vec::new();
        assert!(reader.query_program_status(TIMEOUT, &mut output).unwrap());
        assert_eq!(output, QUERY);
        assert_eq!(
            remaining_input(&mut reader),
            vec![
                InternalEvent::Event(Event::Key(KeyCode::Char('a').into())),
                InternalEvent::Event(Event::Key(KeyCode::Char('界').into())),
                InternalEvent::Event(Event::FocusGained),
                InternalEvent::Event(Event::Key(KeyCode::Char('b').into())),
                InternalEvent::Event(Event::Paste("literal\x1b]7501;?\x1b\\".into())),
                InternalEvent::Event(Event::FocusLost),
                InternalEvent::Event(Event::Key(KeyCode::Char('z').into())),
            ]
        );
        assert!(reader.events.is_empty());
        assert!(reader.skipped_events.is_empty());
    }

    #[test_case(DA1, false; "da1_first")]
    #[test_case(b"", false; "timeout")]
    #[test_case(SUPPORTED, true; "missing_fence")]
    fn program_status_query_requires_reply_before_fence(reply: &[u8], expected: bool) {
        let mut reader = reader(&[b"a".as_slice(), reply].concat(), 1, None);
        let mut output = Vec::new();
        assert_eq!(
            reader.query_program_status(TIMEOUT, &mut output).unwrap(),
            expected
        );
        assert_eq!(output, QUERY);
        assert_eq!(
            remaining_input(&mut reader),
            vec![InternalEvent::Event(Event::Key(KeyCode::Char('a').into()))]
        );
        assert!(reader.events.is_empty());
    }

    const GRAPHICS_ID: u32 = 31;
    const GRAPHICS_QUERY: &[u8] = b"\x1b_Gi=31,a=q,t=d,f=100;iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==\x1b\\\x1b[16t\x1b[c";

    #[test_case(b"\x1b_Gi=31;OK\x1b\\\x1b[?62;4c", true; "ok_then_fence")]
    #[test_case(b"\x1b_Gi=31;ENOTSUPPORTED:no\x1b\\\x1b[?62c", false; "error_then_fence")]
    #[test_case(b"\x1b[?62c", false; "fence_only")]
    #[test_case(b"", false; "timeout")]
    #[test_case(b"\x1b_Gi=32;OK\x1b\\\x1b[?62c", false; "unrelated_id")]
    #[test_case(b"\x1b_Gi=32;OK\x1b\\\x1b_Gi=31;OK\x1b\\\x1b[?62c", true; "unrelated_then_matching")]
    fn kitty_graphics_query_needs_ok_before_fence(reply: &[u8], expected: bool) {
        for chunk_size in [1, 3, usize::MAX] {
            let mut reader = reader(&[b"a".as_slice(), reply].concat(), chunk_size, None);
            let mut output = Vec::new();
            assert_eq!(
                reader
                    .query_terminal_graphics(TIMEOUT, &mut output, GRAPHICS_ID)
                    .unwrap()
                    .kitty,
                expected
            );
            assert_eq!(output, GRAPHICS_QUERY);
            assert_eq!(
                remaining_input(&mut reader),
                vec![InternalEvent::Event(Event::Key(KeyCode::Char('a').into()))]
            );
        }
    }

    #[test]
    fn graphics_and_size_share_one_fence_and_preserve_input() {
        let input = b"a\x1b_Gi=31;OK\x1b\\\x1b[6;20;10t\x1b[?62cz";
        for chunk_size in [1, 3, usize::MAX] {
            let mut reader = reader(input, chunk_size, None);
            let reply = reader
                .query_terminal_graphics(TIMEOUT, &mut Vec::new(), GRAPHICS_ID)
                .unwrap();
            assert!(reply.kitty);
            assert_eq!(reply.cell_pixels, Some((10, 20)));
            assert_eq!(
                remaining_input(&mut reader),
                vec![
                    InternalEvent::Event(Event::Key(KeyCode::Char('a').into())),
                    InternalEvent::Event(Event::Key(KeyCode::Char('z').into())),
                ]
            );
        }
    }

    #[test]
    fn graphics_input_budget_stops_without_losing_keys() {
        const KEY_COUNT: usize = 2048;
        let mut reader = reader(&vec![b'a'; KEY_COUNT], 1, None);
        assert!(
            !reader
                .query_terminal_graphics(TIMEOUT, &mut Vec::new(), GRAPHICS_ID)
                .unwrap()
                .kitty
        );
        assert!(reader.events.len() < KEY_COUNT);
        assert_eq!(remaining_input(&mut reader).len(), KEY_COUNT);
    }

    #[test]
    fn expired_graphics_budget_sends_nothing() {
        let mut reader = reader(b"a", 1, None);
        let mut output = Vec::new();
        assert!(
            !reader
                .query_terminal_graphics(Duration::ZERO, &mut output, GRAPHICS_ID)
                .unwrap()
                .kitty
        );
        assert!(output.is_empty());
        assert_eq!(remaining_input(&mut reader).len(), 1);
    }

    #[test]
    fn graphics_reply_after_fence_cannot_enable_support() {
        let mut reader = reader(b"\x1b[?62c\x1b_Gi=31;OK\x1b\\z", usize::MAX, None);
        assert!(
            !reader
                .query_terminal_graphics(TIMEOUT, &mut Vec::new(), GRAPHICS_ID)
                .unwrap()
                .kitty
        );
        assert_eq!(
            remaining_input(&mut reader),
            vec![InternalEvent::Event(Event::Key(KeyCode::Char('z').into()))]
        );
    }

    #[test_case(io::ErrorKind::BrokenPipe; "io_error")]
    #[test_case(io::ErrorKind::Interrupted; "interrupted")]
    fn graphics_errors_retain_unrelated_input(error: io::ErrorKind) {
        let mut reader = reader("a界\x1b".as_bytes(), 1, Some(error));
        let result = reader.query_terminal_graphics(TIMEOUT, &mut Vec::new(), GRAPHICS_ID);
        if error == io::ErrorKind::Interrupted {
            assert!(!result.unwrap().kitty);
        } else {
            assert_eq!(result.unwrap_err().kind(), error);
        }
        assert_eq!(
            remaining_input(&mut reader),
            vec![
                InternalEvent::Event(Event::Key(KeyCode::Char('a').into())),
                InternalEvent::Event(Event::Key(KeyCode::Char('界').into())),
                InternalEvent::Event(Event::Key(KeyCode::Esc.into())),
            ]
        );
    }

    #[test]
    fn queued_replies_from_earlier_queries_do_not_enable_support() {
        let mut reader = reader(b"\x1b[?62c", 1, None);
        reader
            .events
            .push_back(InternalEvent::KittyGraphicsReply(GRAPHICS_ID, true));
        reader.events.push_back(InternalEvent::CellSize(8, 16));
        let reply = reader
            .query_terminal_graphics(TIMEOUT, &mut Vec::new(), GRAPHICS_ID)
            .unwrap();
        assert!(!reply.kitty);
        assert_eq!(reply.cell_pixels, None);
    }

    #[test_case(io::ErrorKind::BrokenPipe; "io_error")]
    #[test_case(io::ErrorKind::Interrupted; "interrupted")]
    fn program_status_query_errors_retain_skipped_keys(error: io::ErrorKind) {
        let mut reader = reader("a界\x1b".as_bytes(), 1, Some(error));
        let result = reader.query_program_status(TIMEOUT, &mut Vec::new());
        if error == io::ErrorKind::Interrupted {
            assert!(!result.unwrap());
        } else {
            assert_eq!(result.unwrap_err().kind(), error);
        }
        assert_eq!(
            remaining_input(&mut reader),
            vec![
                InternalEvent::Event(Event::Key(KeyCode::Char('a').into())),
                InternalEvent::Event(Event::Key(KeyCode::Char('界').into())),
                InternalEvent::Event(Event::Key(KeyCode::Esc.into())),
            ]
        );
    }

    #[test_case(true; "supported")]
    #[test_case(false; "unsupported")]
    fn program_status_filter_preserves_already_queued_key_order(supported: bool) {
        let mut reader = reader(b"", 1, None);
        let first = InternalEvent::Event(Event::Key(KeyCode::Char('a').into()));
        let second = InternalEvent::Event(Event::Key(KeyCode::Char('b').into()));
        reader.events.push_back(first.clone());
        if supported {
            reader
                .events
                .push_back(InternalEvent::ProgramStatusSupported);
        }
        reader.events.push_back(second.clone());
        reader
            .events
            .push_back(InternalEvent::PrimaryDeviceAttributes);
        assert_eq!(
            reader
                .query_program_status(TIMEOUT, &mut Vec::new())
                .unwrap(),
            supported
        );
        assert_eq!(remaining_input(&mut reader), vec![first, second]);
        assert!(reader.events.is_empty());
    }

    #[test_case(b"\x1b"; "escape_key")]
    #[test_case(b"\x1b]7501;?"; "incomplete_reply")]
    fn program_status_timeout_restores_escape_without_flushing_reply_payload(input: &[u8]) {
        let mut reader = reader(input, 1, None);
        assert!(
            !reader
                .query_program_status(TIMEOUT, &mut Vec::new())
                .unwrap()
        );
        let expected = if input == b"\x1b" {
            vec![InternalEvent::Event(Event::Key(KeyCode::Esc.into()))]
        } else {
            Vec::new()
        };
        assert_eq!(remaining_input(&mut reader), expected);
    }

    #[test_case(usize::MAX; "coalesced")]
    fn program_status_reply_after_da1_is_not_support_or_keyboard_input(chunk_size: usize) {
        let mut reader = reader(&[DA1, SUPPORTED, b"z"].concat(), chunk_size, None);
        assert!(
            !reader
                .query_program_status(TIMEOUT, &mut Vec::new())
                .unwrap()
        );
        while reader.poll(Some(TIMEOUT), &EventFilter).unwrap() {
            assert_eq!(
                reader.try_read(&EventFilter),
                Some(InternalEvent::Event(Event::Key(KeyCode::Char('z').into())))
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::{collections::VecDeque, time::Duration};

    use super::super::filter::CursorPositionFilter;
    use super::{super::Event, EventSource, Filter, InternalEvent, InternalEventReader};

    #[derive(Debug, Clone)]
    pub(crate) struct InternalEventFilter;

    impl Filter for InternalEventFilter {
        fn eval(&self, _: &InternalEvent) -> bool {
            true
        }
    }

    #[test]
    fn test_poll_fails_without_event_source() {
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &InternalEventFilter).is_err());
        assert!(
            reader
                .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
                .is_err()
        );
        assert!(
            reader
                .poll(Some(Duration::from_secs(10)), &InternalEventFilter)
                .is_err()
        );
    }

    #[test]
    fn test_poll_returns_true_for_matching_event_in_queue_at_front() {
        let mut reader = InternalEventReader {
            events: vec![InternalEvent::Event(Event::Resize(10, 10))].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &InternalEventFilter).unwrap());
    }

    #[test]
    fn test_poll_returns_true_for_matching_event_in_queue_at_back() {
        let mut reader = InternalEventReader {
            events: vec![
                InternalEvent::Event(Event::Resize(10, 10)),
                InternalEvent::CursorPosition(10, 20),
            ]
            .into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &CursorPositionFilter).unwrap());
    }

    #[test]
    fn test_read_returns_matching_event_in_queue_at_front() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let mut reader = InternalEventReader {
            events: vec![EVENT].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[test]
    fn test_read_returns_matching_event_in_queue_at_back() {
        const CURSOR_EVENT: InternalEvent = InternalEvent::CursorPosition(10, 20);

        let mut reader = InternalEventReader {
            events: vec![InternalEvent::Event(Event::Resize(10, 10)), CURSOR_EVENT].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&CursorPositionFilter).unwrap(), CURSOR_EVENT);
    }

    #[test]
    fn test_read_does_not_consume_skipped_event() {
        const SKIPPED_EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));
        const CURSOR_EVENT: InternalEvent = InternalEvent::CursorPosition(10, 20);

        let mut reader = InternalEventReader {
            events: vec![SKIPPED_EVENT, CURSOR_EVENT].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&CursorPositionFilter).unwrap(), CURSOR_EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), SKIPPED_EVENT);
    }

    #[test]
    fn test_try_read_does_not_consume_skipped_event() {
        const SKIPPED_EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));
        const CURSOR_EVENT: InternalEvent = InternalEvent::CursorPosition(10, 20);

        let mut reader = InternalEventReader {
            events: vec![SKIPPED_EVENT, CURSOR_EVENT].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(
            reader.try_read(&CursorPositionFilter).unwrap(),
            CURSOR_EVENT
        );
        assert_eq!(
            reader.try_read(&InternalEventFilter).unwrap(),
            SKIPPED_EVENT
        );
    }

    #[test]
    fn test_poll_timeouts_if_source_has_no_events() {
        let source = FakeSource::default();

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert!(
            !reader
                .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
                .unwrap()
        );
    }

    #[test]
    fn test_poll_returns_true_if_source_has_at_least_one_event() {
        let source = FakeSource::with_events(&[InternalEvent::Event(Event::Resize(10, 10))]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &InternalEventFilter).unwrap());
        assert!(
            reader
                .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
                .unwrap()
        );
    }

    #[test]
    fn test_reads_returns_event_if_source_has_at_least_one_event() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::with_events(&[EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[test]
    fn test_read_returns_events_if_source_has_events() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::with_events(&[EVENT, EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[test]
    fn test_poll_returns_false_after_all_source_events_are_consumed() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::with_events(&[EVENT, EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert!(
            !reader
                .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
                .unwrap()
        );
    }

    #[test]
    fn test_poll_propagates_error() {
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(FakeSource::new(&[]))),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(
            reader
                .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
                .err()
                .map(|e| format!("{:?}", e.kind())),
            Some(format!("{:?}", io::ErrorKind::Other))
        );
    }

    #[test]
    fn test_read_propagates_error() {
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(FakeSource::new(&[]))),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(
            reader
                .read(&InternalEventFilter)
                .err()
                .map(|e| format!("{:?}", e.kind())),
            Some(format!("{:?}", io::ErrorKind::Other))
        );
    }

    #[test]
    fn test_poll_continues_after_error() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::new(&[EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert!(reader.read(&InternalEventFilter).is_err());
        assert!(
            reader
                .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
                .unwrap()
        );
    }

    #[test]
    fn test_read_continues_after_error() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::new(&[EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert!(reader.read(&InternalEventFilter).is_err());
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[derive(Default)]
    struct FakeSource {
        events: VecDeque<InternalEvent>,
        error: Option<io::Error>,
    }

    impl FakeSource {
        fn new(events: &[InternalEvent]) -> FakeSource {
            FakeSource {
                events: events.to_vec().into(),
                error: Some(io::Error::other("")),
            }
        }

        fn with_events(events: &[InternalEvent]) -> FakeSource {
            FakeSource {
                events: events.to_vec().into(),
                error: None,
            }
        }
    }

    impl EventSource for FakeSource {
        fn try_read(&mut self, _timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
            // Return error if set in case there's just one remaining event
            if self.events.len() == 1 {
                if let Some(error) = self.error.take() {
                    return Err(error);
                }
            }

            // Return all events from the queue
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }

            // Return error if there're no more events
            if let Some(error) = self.error.take() {
                return Err(error);
            }

            // Timeout
            Ok(None)
        }

        #[cfg(feature = "event-stream")]
        fn waker(&self) -> super::super::sys::Waker {
            unimplemented!();
        }
    }
}
