#[cfg(feature = "use-dev-tty")]
pub(crate) mod tty;

#[cfg(not(feature = "use-dev-tty"))]
pub(crate) mod mio;

#[cfg(feature = "use-dev-tty")]
pub(crate) use self::tty::UnixInternalEventSource;

#[cfg(not(feature = "use-dev-tty"))]
pub(crate) use self::mio::UnixInternalEventSource;

#[cfg(test)]
mod tests {
    use std::io::Write;
    #[cfg(feature = "libc")]
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::UnixInternalEventSource;
    use crate::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, internal::InternalEvent, source::EventSource,
    };
    use crate::terminal::sys::file_descriptor::FileDesc;
    use test_case::test_case;

    const TEST_TIMEOUT: Duration = Duration::from_secs(3);
    const SHORT_POLL: Duration = Duration::from_millis(1);

    #[test_case(false; "blocking_read")]
    #[test_case(true; "short_poll_then_blocking_read")]
    #[serial_test::serial]
    fn bare_alt_underscore_wakes_without_more_input(short_poll: bool) {
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let (input, mut writer) = UnixStream::pair().unwrap();
            input.set_nonblocking(true).unwrap();
            #[cfg(feature = "libc")]
            let input = FileDesc::new(input.into_raw_fd(), true);
            #[cfg(not(feature = "libc"))]
            let input = FileDesc::Owned(input.into());
            let mut source = UnixInternalEventSource::from_file_descriptor(input).unwrap();
            writer.write_all(b"\x1b_").unwrap();
            let first = if short_poll {
                source.try_read(Some(SHORT_POLL)).unwrap()
            } else {
                None
            };
            let event = first.or_else(|| source.try_read(None).unwrap());
            sender.send(event).unwrap();
        });
        assert_eq!(
            receiver.recv_timeout(TEST_TIMEOUT).unwrap(),
            Some(InternalEvent::Event(Event::Key(KeyEvent::new(
                KeyCode::Char('_'),
                KeyModifiers::ALT
            )),))
        );
        worker.join().unwrap();
    }
}
