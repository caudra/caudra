//! Ask the terminal a question and read the answer straight off the tty.
//!
//! crossterm parses neither DECRQSS nor OSC replies, so probes bypass it and
//! read the file descriptor directly. Call these only before the input reader
//! thread spawns, or while it is parked, or it will eat the reply.
//!
//! Every request is terminated with a DA1 request, which every terminal
//! answers even when it ignores the question we actually care about. Without
//! it, a terminal lacking the feature would cost the full timeout on every
//! probe instead of one round trip.

/// The DA1 reply is `ESC [ ? ... c`.
#[cfg(any(unix, test))]
fn da1_answered(buf: &[u8]) -> bool {
    find(buf, b"\x1b[?").is_some_and(|start| buf[start + 3..].contains(&b'c'))
}

pub(crate) fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::io::{Write, stdout};
    use std::os::fd::{AsRawFd, RawFd};
    use std::time::{Duration, Instant};

    use super::da1_answered;

    /// Primary device attributes, appended to every request as a terminator.
    const DA1_REQUEST: &[u8] = b"\x1b[c";
    const CHUNK_BYTES: usize = 256;

    pub(crate) fn query(request: &[u8], timeout: Duration) -> Option<Vec<u8>> {
        let (_owned, fd) = open_tty()?;
        let mut out = stdout().lock();
        out.write_all(request).ok()?;
        out.write_all(DA1_REQUEST).ok()?;
        out.flush().ok()?;
        drop(out);

        let deadline = Instant::now() + timeout;
        let mut buf = Vec::with_capacity(CHUNK_BYTES);
        while !da1_answered(&buf) {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            if !wait_readable(fd, remaining) {
                break;
            }
            let mut chunk = [0u8; CHUNK_BYTES];
            let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n <= 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n as usize]);
        }
        Some(buf)
    }

    fn open_tty() -> Option<(Option<File>, RawFd)> {
        if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
            return Some((None, libc::STDIN_FILENO));
        }
        let file = File::open("/dev/tty").ok()?;
        let fd = file.as_raw_fd();
        Some((Some(file), fd))
    }

    fn wait_readable(fd: RawFd, timeout: Duration) -> bool {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        unsafe { libc::poll(&mut pfd, 1, ms) > 0 && pfd.revents & libc::POLLIN != 0 }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::time::Duration;

    /// No tty to poke on non-unix; callers fall back to their own defaults.
    pub(crate) fn query(_request: &[u8], _timeout: Duration) -> Option<Vec<u8>> {
        None
    }
}

pub(crate) use imp::query;

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(b"\x1b[?65;1;9c", true; "da1_reply")]
    #[test_case(b"\x1bP1$r0;48:2:1:2:3m\x1b\\", false; "other_reply_only")]
    #[test_case(b"\x1b[?65;1;9", false; "partial_da1")]
    #[test_case(b"", false; "empty")]
    fn da1(buf: &[u8], expected: bool) {
        assert_eq!(da1_answered(buf), expected);
    }

    #[test_case(b"hello", b"ll", Some(2); "match_in_middle")]
    #[test_case(b"hello", b"h", Some(0); "match_at_start")]
    #[test_case(b"hello", b"xyz", None; "no_match")]
    #[test_case(b"hi", b"hello", None; "needle_longer_than_haystack")]
    fn find_needle(hay: &[u8], needle: &[u8], expected: Option<usize>) {
        assert_eq!(find(hay, needle), expected);
    }
}
