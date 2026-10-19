#[cfg(unix)]
use std::io::{self, stdout};
#[cfg(unix)]
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};

use crate::event::KeyboardEnhancementFlags;
#[cfg(unix)]
use crate::event::TerminalGraphics;
use crate::event::{Event, filter::Filter, read::InternalEventReader, timeout::PollTimeout};

/// Static instance of `InternalEventReader`.
/// This needs to be static because there can be one event reader.
static EVENT_READER: Mutex<Option<InternalEventReader>> = parking_lot::const_mutex(None);
#[cfg(unix)]
static GRAPHICS_QUERY_ID: AtomicU32 = AtomicU32::new(0xca00_0001);

pub(crate) fn lock_event_reader() -> MappedMutexGuard<'static, InternalEventReader> {
    MutexGuard::map(EVENT_READER.lock(), |reader| {
        reader.get_or_insert_with(InternalEventReader::default)
    })
}

#[cfg(unix)]
pub(crate) fn query_program_status(timeout: Duration) -> io::Result<bool> {
    let timeout = PollTimeout::new(Some(timeout));
    let Some(mut reader) = try_lock_event_reader_for(timeout.leftover().unwrap_or_default()) else {
        return Ok(false);
    };
    reader.query_program_status(timeout.leftover().unwrap_or_default(), &mut stdout().lock())
}

#[cfg(unix)]
pub(crate) fn query_kitty_graphics(timeout: Duration) -> io::Result<bool> {
    query_terminal_graphics(timeout).map(|reply| reply.kitty)
}

#[cfg(unix)]
pub(crate) fn query_terminal_graphics(timeout: Duration) -> io::Result<TerminalGraphics> {
    let timeout = PollTimeout::new(Some(timeout));
    let Some(mut reader) = try_lock_event_reader_for(timeout.leftover().unwrap_or_default()) else {
        return Ok(TerminalGraphics::default());
    };
    let id = GRAPHICS_QUERY_ID.fetch_add(1, Ordering::Relaxed).max(1);
    reader.query_terminal_graphics(
        timeout.leftover().unwrap_or_default(),
        &mut stdout().lock(),
        id,
    )
}

fn try_lock_event_reader_for(
    duration: Duration,
) -> Option<MappedMutexGuard<'static, InternalEventReader>> {
    Some(MutexGuard::map(
        EVENT_READER.try_lock_for(duration)?,
        |reader| reader.get_or_insert_with(InternalEventReader::default),
    ))
}

/// Polls to check if there are any `InternalEvent`s that can be read within the given duration.
pub(crate) fn poll<F>(timeout: Option<Duration>, filter: &F) -> std::io::Result<bool>
where
    F: Filter,
{
    let (mut reader, timeout) = if let Some(timeout) = timeout {
        let poll_timeout = PollTimeout::new(Some(timeout));
        let reader = match try_lock_event_reader_for(timeout) {
            Some(reader) => reader,
            None => return Ok(false),
        };
        (reader, poll_timeout.leftover())
    } else {
        (lock_event_reader(), None)
    };
    reader.poll(timeout, filter)
}

/// Reads a single `InternalEvent`.
pub(crate) fn read<F>(filter: &F) -> std::io::Result<InternalEvent>
where
    F: Filter,
{
    let mut reader = lock_event_reader();
    reader.read(filter)
}

/// Reads a single `InternalEvent`. Non-blocking.
pub(crate) fn try_read<F>(filter: &F) -> Option<InternalEvent>
where
    F: Filter,
{
    let mut reader = lock_event_reader();
    reader.try_read(filter)
}

/// An internal event.
///
/// Encapsulates publicly available `Event` with additional internal
/// events that shouldn't be publicly available to the crate users.
#[derive(Debug, PartialOrd, PartialEq, Hash, Clone, Eq)]
pub(crate) enum InternalEvent {
    /// An event.
    Event(Event),
    /// A cursor position (`col`, `row`).
    CursorPosition(u16, u16),
    /// The progressive keyboard enhancement flags enabled by the terminal.
    KeyboardEnhancementFlags(KeyboardEnhancementFlags),
    /// Attributes and architectural class of the terminal.
    PrimaryDeviceAttributes,
    ProgramStatusSupported,
    /// The terminal's answer to a Kitty graphics query: whether it said `OK`.
    KittyGraphicsReply(u32, bool),
    /// Cell width and height in terminal-reported pixels, without device scaling.
    CellSize(u16, u16),
    GraphicsQueryLimit,
}
