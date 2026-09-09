pub mod record;
pub mod tail;

/// Targets that group events by subsystem. `/logs` and `caudra logs` filter on
/// them, so a new event belongs under an existing one whenever it can.
///
/// Field vocabulary shared by every event under these targets:
/// `event` (stable snake_case name), `outcome` (ok, error, cancelled, timeout),
/// `duration_ms`, `error` and `error_kind`, plus the domain fields `provider`,
/// `model`, `tool`, `server`, `attempt`, and `status`.
pub mod target {
    pub const AGENT: &str = "caudra::agent";
    pub const MCP: &str = "caudra::mcp";
    pub const PERMISSION: &str = "caudra::permission";
    pub const PROVIDER: &str = "caudra::provider";
    pub const SESSION: &str = "caudra::session";
    pub const TOOL: &str = "caudra::tool";
}

/// Values for the `outcome` field. Anything with a start and a finish reports
/// one of these plus `duration_ms`.
pub mod outcome {
    pub const OK: &str = "ok";
    pub const ERROR: &str = "error";
    pub const CANCELLED: &str = "cancelled";
    pub const TIMEOUT: &str = "timeout";
}

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use flume::{Receiver, Sender, TrySendError};
use tracing::warn;

use crate::paths;

const LOG_FILE_NAME: &str = "caudra.log";
const LOCK_FILE_NAME: &str = "caudra.log.lock";
pub const DEFAULT_MAX_BYTES: u64 = 200 * 1024 * 1024;
pub const DEFAULT_MAX_FILES: u32 = 10;
/// Enough to absorb a burst of `trace!` from a hot loop without making a normal
/// session allocate megabytes of pending records.
pub const DEFAULT_QUEUE_CAPACITY: usize = 8192;
const FLUSH_POLL: Duration = Duration::from_millis(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const DROPPED_NOTICE: &str = r#"{"level":"WARN","target":"caudra::log","fields":{"message":"log records dropped","dropped":"#;

/// Bumped once per record handed to the writer thread. A reader can compare it
/// against a previous value to learn whether the file changed without asking
/// the filesystem.
static WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// There is one sink per process, so a panic hook can reach it without the
/// caller threading a handle down to it.
static ACTIVE: OnceLock<Arc<SinkState>> = OnceLock::new();

/// Waits for everything enqueued so far to reach the file. Returns whether it
/// drained inside the deadline, and `true` when no sink is installed.
pub fn flush_blocking(timeout: Duration) -> bool {
    match ACTIVE.get() {
        Some(state) => state.wait_for_drain(timeout),
        None => true,
    }
}

/// How many records have been accepted for writing this process. Only useful as
/// a change detector; it says nothing about how many reached the disk.
pub fn write_sequence() -> u64 {
    WRITE_SEQUENCE.load(Ordering::Relaxed)
}

/// The directory `RotatingFileWriter` writes into, or `None` when the platform
/// gave us nowhere to put it.
pub fn tail_dir() -> Option<PathBuf> {
    paths::logs_dir().ok()
}

pub fn file_path(dir: &Path, index: u32) -> PathBuf {
    if index == 0 {
        dir.join(LOG_FILE_NAME)
    } else {
        dir.join(format!("caudra.{index}.log"))
    }
}

fn flock_exclusive(file: &File) -> io::Result<()> {
    file.lock()
}

pub struct RotatingFileWriter {
    dir: PathBuf,
    file: File,
    written: u64,
    max_bytes: u64,
    max_files: u32,
}

impl RotatingFileWriter {
    pub fn new(max_bytes: u64, max_files: u32) -> io::Result<Self> {
        Self::with_limits(&paths::logs_dir()?, max_bytes, max_files)
    }

    fn with_limits(dir: &Path, max_bytes: u64, max_files: u32) -> io::Result<Self> {
        let dir = dir.to_path_buf();
        let path = file_path(&dir, 0);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            dir,
            file,
            written,
            max_bytes,
            max_files,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;

        let _lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(LOCK_FILE_NAME))?;
        flock_exclusive(&_lock)?;

        let primary = file_path(&self.dir, 0);
        #[cfg(unix)]
        let needs_rotate = {
            let our_inode = self.file.metadata()?.ino();
            match fs::metadata(&primary) {
                Ok(m) => m.ino() == our_inode,
                Err(_) => true,
            }
        };
        #[cfg(not(unix))]
        let needs_rotate = true;

        if needs_rotate {
            let last = self.max_files - 1;
            match fs::remove_file(file_path(&self.dir, last)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => warn!(error = %e, "log rotate: failed to remove oldest file"),
            }

            for i in (0..last).rev() {
                let src = file_path(&self.dir, i);
                if src.exists() {
                    let dst = file_path(&self.dir, i + 1);
                    if let Err(e) = fs::rename(&src, &dst) {
                        eprintln!("caudra: log rotate rename {src:?} -> {dst:?}: {e}");
                    }
                }
            }
        }

        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&primary)?;
        self.written = self.file.metadata()?.len();

        Ok(())
    }
}

impl Write for RotatingFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written >= self.max_bytes
            && let Err(e) = self.rotate()
        {
            eprintln!("caudra: log rotation failed: {e}");
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

enum Message {
    Record(Vec<u8>),
    Stop,
}

#[derive(Default)]
struct SinkState {
    enqueued: AtomicU64,
    written: AtomicU64,
    dropped: AtomicU64,
    finished: AtomicU64,
}

impl SinkState {
    fn wait_for_drain(&self, timeout: Duration) -> bool {
        let target = self.enqueued.load(Ordering::Acquire);
        let deadline = Instant::now() + timeout;
        while self.written.load(Ordering::Acquire) < target {
            if Instant::now() >= deadline || self.finished.load(Ordering::Acquire) > 0 {
                return false;
            }
            thread::sleep(FLUSH_POLL);
        }
        true
    }
}

/// Hands records to a dedicated writer thread so an emitting thread never
/// blocks on disk. Cloneable so it can serve as a `MakeWriter` closure.
#[derive(Clone)]
pub struct LogSink {
    tx: Sender<Message>,
    state: Arc<SinkState>,
}

impl LogSink {
    /// Records lost to a full queue since the process started. The writer
    /// thread also reports them into the log itself.
    pub fn dropped(&self) -> u64 {
        self.state.dropped.load(Ordering::Relaxed)
    }
}

impl Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.tx.try_send(Message::Record(buf.to_vec())) {
            Ok(()) => {
                self.state.enqueued.fetch_add(1, Ordering::Release);
                WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) => {
                self.state.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
        Ok(buf.len())
    }

    /// The writer thread flushes after every batch, so an emitting thread has
    /// nothing to wait for. Use [`LogSinkGuard::flush_blocking`] to wait.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Owns the writer thread. Dropping it stops the thread after a bounded wait,
/// so a wedged disk delays exit rather than preventing it.
pub struct LogSinkGuard {
    tx: Sender<Message>,
    state: Arc<SinkState>,
    handle: Option<JoinHandle<()>>,
}

impl LogSinkGuard {
    /// Waits until every record enqueued before the call has reached the file.
    /// Returns whether the queue drained inside the deadline.
    pub fn flush_blocking(&self, timeout: Duration) -> bool {
        self.state.wait_for_drain(timeout)
    }
}

impl Drop for LogSinkGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(Message::Stop);
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        while self.state.finished.load(Ordering::Acquire) == 0 {
            if Instant::now() >= deadline {
                return;
            }
            thread::sleep(FLUSH_POLL);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Starts the writer thread. A `std::thread` rather than a smol task because
/// logging has to outlive the executor and must not queue behind agent work.
pub fn spawn(mut writer: RotatingFileWriter, capacity: usize) -> (LogSink, LogSinkGuard) {
    let (tx, rx) = flume::bounded(capacity);
    let state = Arc::new(SinkState::default());
    let thread_state = Arc::clone(&state);
    let handle = thread::Builder::new()
        .name("caudra-log".into())
        .spawn(move || {
            drain(&mut writer, &rx, &thread_state);
            thread_state.finished.store(1, Ordering::Release);
        })
        .ok();

    if handle.is_none() {
        state.finished.store(1, Ordering::Release);
    }
    let _ = ACTIVE.set(Arc::clone(&state));

    (
        LogSink {
            tx: tx.clone(),
            state: Arc::clone(&state),
        },
        LogSinkGuard { tx, state, handle },
    )
}

fn drain(writer: &mut RotatingFileWriter, rx: &Receiver<Message>, state: &SinkState) {
    while let Ok(first) = rx.recv() {
        let Message::Record(record) = first else {
            break;
        };
        let mut batch = 1;
        write_record(writer, &record);
        while let Ok(next) = rx.try_recv() {
            match next {
                Message::Record(record) => {
                    write_record(writer, &record);
                    batch += 1;
                }
                Message::Stop => {
                    finish(writer, state, batch);
                    return;
                }
            }
        }
        finish(writer, state, batch);
    }
    let _ = writer.flush();
}

/// Reports backpressure losses into the log itself, so a gap is visible rather
/// than silent. Runs after the batch so the notice sits next to the survivors.
fn finish(writer: &mut RotatingFileWriter, state: &SinkState, batch: u64) {
    let dropped = state.dropped.swap(0, Ordering::Relaxed);
    if dropped > 0 {
        let notice = format!(
            "{DROPPED_NOTICE}{dropped}}},\"timestamp\":\"{}\"}}\n",
            jiff::Timestamp::now()
        );
        write_record(writer, notice.as_bytes());
    }
    let _ = writer.flush();
    state.written.fetch_add(batch, Ordering::Release);
}

fn write_record(writer: &mut RotatingFileWriter, record: &[u8]) {
    if let Err(e) = writer.write_all(record) {
        eprintln!("caudra: log write failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MAX_BYTES: u64 = 32;
    const TEST_MAX_FILES: u32 = 3;

    fn test_writer(dir: &Path) -> RotatingFileWriter {
        RotatingFileWriter::with_limits(dir, TEST_MAX_BYTES, TEST_MAX_FILES).unwrap()
    }

    #[test]
    fn write_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = test_writer(tmp.path());
        w.write_all(b"hello\n").unwrap();
        w.flush().unwrap();

        let contents = fs::read_to_string(file_path(tmp.path(), 0)).unwrap();
        assert_eq!(contents, "hello\n");
    }

    #[test]
    fn rotates_when_size_exceeded() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = test_writer(tmp.path());

        let filler = "x".repeat(TEST_MAX_BYTES as usize);
        w.write_all(filler.as_bytes()).unwrap();
        w.flush().unwrap();

        w.write_all(b"after").unwrap();
        w.flush().unwrap();

        let current = fs::read_to_string(file_path(tmp.path(), 0)).unwrap();
        assert_eq!(current, "after");

        let rotated = fs::read_to_string(file_path(tmp.path(), 1)).unwrap();
        assert_eq!(rotated, filler);
    }

    #[test]
    fn evicts_oldest_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = test_writer(tmp.path());

        let chunk = "x".repeat(TEST_MAX_BYTES as usize);
        for _ in 0..TEST_MAX_FILES + 2 {
            w.write_all(chunk.as_bytes()).unwrap();
            w.flush().unwrap();
        }

        w.write_all(b"final").unwrap();
        w.flush().unwrap();

        assert!(!file_path(tmp.path(), TEST_MAX_FILES).exists());
    }

    #[test]
    fn resumes_existing_file_size() {
        let tmp = tempfile::tempdir().unwrap();

        {
            let mut w = test_writer(tmp.path());
            w.write_all(b"preexisting-data-that-is-long-enough")
                .unwrap();
            w.flush().unwrap();
        }

        let mut w = test_writer(tmp.path());
        w.write_all(b"new").unwrap();
        w.flush().unwrap();

        assert!(
            file_path(tmp.path(), 1).exists(),
            "should have rotated on first write since pre-existing data exceeded threshold"
        );
    }

    const SINK_CAPACITY: usize = 64;
    const SINK_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
    const SINK_RECORD: &[u8] = b"{\"level\":\"INFO\"}\n";
    const NOT_FLUSHED: &str = "records should reach the file inside the timeout";

    fn big_writer(dir: &Path) -> RotatingFileWriter {
        RotatingFileWriter::with_limits(dir, DEFAULT_MAX_BYTES, TEST_MAX_FILES).unwrap()
    }

    #[test]
    fn sink_writes_every_record_through_the_thread() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut sink, guard) = spawn(big_writer(tmp.path()), SINK_CAPACITY);

        for _ in 0..SINK_CAPACITY {
            sink.write_all(SINK_RECORD).unwrap();
        }
        assert!(guard.flush_blocking(SINK_FLUSH_TIMEOUT), "{NOT_FLUSHED}");

        let contents = fs::read_to_string(file_path(tmp.path(), 0)).unwrap();
        assert_eq!(contents.lines().count(), SINK_CAPACITY);
    }

    #[test]
    fn sink_bumps_the_write_sequence() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut sink, guard) = spawn(big_writer(tmp.path()), SINK_CAPACITY);

        let before = write_sequence();
        sink.write_all(SINK_RECORD).unwrap();
        assert!(guard.flush_blocking(SINK_FLUSH_TIMEOUT), "{NOT_FLUSHED}");

        assert!(write_sequence() > before);
    }

    #[test]
    fn sink_never_blocks_the_caller_and_reports_what_it_dropped() {
        let (tx, rx) = flume::bounded(1);
        let state = Arc::new(SinkState::default());
        let mut sink = LogSink {
            tx,
            state: Arc::clone(&state),
        };

        for _ in 0..SINK_CAPACITY {
            sink.write_all(SINK_RECORD).unwrap();
        }

        assert_eq!(sink.dropped(), SINK_CAPACITY as u64 - 1);
        drop(rx);
    }

    #[test]
    fn two_writers_no_data_loss() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w1 = test_writer(tmp.path());
        let mut w2 = test_writer(tmp.path());

        let filler = "x".repeat(TEST_MAX_BYTES as usize);
        w1.write_all(filler.as_bytes()).unwrap();
        w1.flush().unwrap();

        w1.write_all(b"from-w1").unwrap();
        w1.flush().unwrap();

        w2.write_all(b"from-w2").unwrap();
        w2.flush().unwrap();

        let all_content: String = (0..TEST_MAX_FILES)
            .filter_map(|i| fs::read_to_string(file_path(tmp.path(), i)).ok())
            .collect();
        assert!(all_content.contains("from-w1"));
        assert!(all_content.contains("from-w2"));
    }
}
