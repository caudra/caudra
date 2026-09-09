//! A sliding window over the rotating log files.
//!
//! The window never holds more than `capacity` entries, so a 200 MB log costs a
//! few hundred kilobytes. Scrolling reads fixed chunks backwards from the front
//! cursor, crossing into `caudra.1.log` and older when the current file runs
//! out. Following reads only the bytes appended since the back cursor.

use std::collections::VecDeque;
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::{file_path, record::Entry, record::Filter};

const CHUNK_BYTES: usize = 64 * 1024;
/// A filter that matches almost nothing must not turn one keypress into a scan
/// of the whole file, so a request gives up and says so instead.
const MAX_SCAN_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SCAN_LINES: usize = 50_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanOutcome {
    /// Every requested line was found.
    Filled,
    /// The oldest retained file ended first.
    Exhausted,
    /// The scan budget ran out with the filter still hiding everything.
    ScanLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cursor {
    file: u32,
    offset: u64,
}

/// One retained line with the place it came from, so a copy action can hand
/// back exactly what is on disk.
#[derive(Debug, Clone)]
pub struct Line {
    pub raw: String,
    pub entry: Entry,
}

/// How far a backward scan got. `added` shifts every index a caller holds into
/// the window, so it has to come back with the outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Advance {
    pub added: usize,
    pub outcome: ScanOutcome,
}

pub struct LogTail {
    dir: PathBuf,
    max_files: u32,
    capacity: usize,
    window: VecDeque<Line>,
    front: Cursor,
    back: Cursor,
    /// Bytes read past the back cursor that do not yet end in a newline.
    carry: Vec<u8>,
    /// Held open so a rename cannot make us lose the bytes written between the
    /// last poll and the rotation.
    reader: Option<File>,
    reader_id: Option<FileId>,
}

impl LogTail {
    pub fn open(dir: &Path, max_files: u32, capacity: usize) -> io::Result<Self> {
        let mut tail = Self {
            dir: dir.to_path_buf(),
            max_files: max_files.max(1),
            capacity: capacity.max(1),
            window: VecDeque::new(),
            front: Cursor { file: 0, offset: 0 },
            back: Cursor { file: 0, offset: 0 },
            carry: Vec::new(),
            reader: None,
            reader_id: None,
        };
        tail.reset_to_end()?;
        Ok(tail)
    }

    pub fn path(&self) -> PathBuf {
        file_path(&self.dir, 0)
    }

    pub fn size(&self) -> u64 {
        std::fs::metadata(self.path()).map(|m| m.len()).unwrap_or(0)
    }

    pub fn window(&self) -> &VecDeque<Line> {
        &self.window
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// True once the back cursor sits at the end of the live file, so following
    /// has nothing left to catch up on.
    pub fn at_end(&self) -> bool {
        self.back.file == 0 && self.back.offset >= self.size()
    }

    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        while self.window.len() > self.capacity {
            self.drop_front();
        }
    }

    /// Anchors both cursors at the end of the live file and clears the window.
    /// Callers follow with [`Self::fill_back`] to pull the newest lines in.
    pub fn reset_to_end(&mut self) -> io::Result<()> {
        let end = self.size();
        self.window.clear();
        self.front = Cursor {
            file: 0,
            offset: end,
        };
        self.back = Cursor {
            file: 0,
            offset: end,
        };
        self.invalidate_reader();
        self.reader_id = std::fs::metadata(self.path()).ok().map(|m| FileId::of(&m));
        Ok(())
    }

    /// Forces the next read to reopen and seek. Required whenever the back
    /// cursor moves other than by consuming bytes. Keeps `reader_id` so a
    /// rotation that happens while no handle is held stays detectable.
    fn invalidate_reader(&mut self) {
        self.carry.clear();
        self.reader = None;
    }

    /// Jumps to the end and fills the window with the newest matching lines.
    pub fn tail(&mut self, filter: &Filter) -> io::Result<ScanOutcome> {
        self.reset_to_end()?;
        Ok(self.fill_back(self.capacity, filter)?.outcome)
    }

    /// Prepends up to `want` older matching lines. The newest edge is evicted
    /// only when the window would otherwise exceed its capacity.
    pub fn scroll_back(&mut self, want: usize, filter: &Filter) -> io::Result<Advance> {
        self.fill_back(want, filter)
    }

    /// Appends up to `want` newer matching lines, evicting from the oldest edge
    /// once the window is full.
    pub fn scroll_forward(&mut self, want: usize, filter: &Filter) -> io::Result<usize> {
        self.fill_forward(want, filter)
    }

    /// Picks up everything appended since the last call. Returns how many
    /// matching lines arrived.
    pub fn poll(&mut self, filter: &Filter) -> io::Result<usize> {
        self.fill_forward(self.capacity, filter)
    }

    fn drop_front(&mut self) {
        if let Some(line) = self.window.pop_front() {
            self.front = advance(self.front, &line);
        }
    }

    fn drop_back(&mut self) {
        if let Some(line) = self.window.pop_back() {
            self.back = retreat(self.back, &line);
            self.invalidate_reader();
        }
    }

    /// The window is capped, so a length delta would report zero once it is
    /// full. `added` counts what was prepended, which is what shifts a caller's
    /// indices.
    fn fill_back(&mut self, want: usize, filter: &Filter) -> io::Result<Advance> {
        let mut found = Vec::new();
        let mut scanned_bytes = 0u64;
        let mut scanned_lines = 0usize;
        let mut carry: Vec<u8> = Vec::new();
        let mut cursor = self.front;
        let outcome = loop {
            if found.len() >= want {
                break ScanOutcome::Filled;
            }
            if cursor.offset == 0 {
                if !carry.is_empty() {
                    push_back_match(&mut found, &carry, filter);
                    carry.clear();
                }
                match self.older_file_end(cursor.file)? {
                    Some(next) => {
                        cursor = next;
                        continue;
                    }
                    None => break ScanOutcome::Exhausted,
                }
            }
            if scanned_bytes >= MAX_SCAN_BYTES || scanned_lines >= MAX_SCAN_LINES {
                break ScanOutcome::ScanLimit;
            }

            let read_len = CHUNK_BYTES.min(cursor.offset as usize);
            let start = cursor.offset - read_len as u64;
            let chunk = read_at(&file_path(&self.dir, cursor.file), start, read_len)?;
            scanned_bytes += read_len as u64;

            let mut end_idx = chunk.len();
            let mut consumed_to = start;
            while let Some(nl) = memchr::memrchr(b'\n', &chunk[..end_idx]) {
                let mut line = chunk[nl + 1..end_idx].to_vec();
                line.extend_from_slice(&carry);
                carry.clear();
                if !line.is_empty() {
                    scanned_lines += 1;
                    push_back_match(&mut found, &line, filter);
                }
                end_idx = nl;
                consumed_to = start + nl as u64 + 1;
                if found.len() >= want {
                    break;
                }
            }
            if found.len() >= want {
                cursor.offset = consumed_to;
                break ScanOutcome::Filled;
            }
            carry.splice(0..0, chunk[..end_idx].iter().copied());
            cursor.offset = start;
        };

        let added = found.len();
        for line in found {
            self.window.push_front(line);
        }
        self.front = cursor;
        while self.window.len() > self.capacity {
            self.drop_back();
        }
        Ok(Advance { added, outcome })
    }

    fn fill_forward(&mut self, want: usize, filter: &Filter) -> io::Result<usize> {
        let mut added = 0usize;
        while added < want {
            let Some(chunk) = self.next_chunk()? else {
                break;
            };
            let mut cursor = 0usize;
            while let Some(nl) = memchr::memchr(b'\n', &chunk[cursor..]) {
                let end = cursor + nl;
                let mut line = std::mem::take(&mut self.carry);
                line.extend_from_slice(&chunk[cursor..end]);
                cursor = end + 1;
                self.back.offset += line.len() as u64 + 1;
                if let Some(line) = matching(&line, filter) {
                    self.window.push_back(line);
                    added += 1;
                }
            }
            // A partial trailing line waits in `carry` until its newline lands.
            self.carry.extend_from_slice(&chunk[cursor..]);
        }
        while self.window.len() > self.capacity {
            self.drop_front();
        }
        Ok(added)
    }

    /// Reads the next block from the held handle, stepping into the new
    /// `caudra.log` once the old one is drained.
    fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            let Some(reader) = self.reader_at_back()? else {
                return Ok(None);
            };
            let mut chunk = vec![0u8; CHUNK_BYTES];
            let read = reader.read(&mut chunk)?;
            if read > 0 {
                chunk.truncate(read);
                return Ok(Some(chunk));
            }
            if !self.advance_past_rotation()? {
                return Ok(None);
            }
        }
    }

    /// Opens (or reuses) a handle positioned at the back cursor. Holding the
    /// handle across a rename is what lets a rotation cost no lines; when no
    /// handle was held, the cursor follows the bytes into the renamed file.
    fn reader_at_back(&mut self) -> io::Result<Option<&mut File>> {
        if self.reader.is_some() {
            return Ok(self.reader.as_mut());
        }
        if self.back.file == 0 && self.rotated_while_closed() {
            self.note_rotation();
            self.back.file = 1;
            self.carry.clear();
        }
        let path = file_path(&self.dir, self.back.file);
        let Ok(mut file) = File::open(&path) else {
            return Ok(None);
        };
        let meta = file.metadata()?;
        if self.back.offset > meta.len() {
            self.back.offset = 0;
            self.carry.clear();
        }
        if self.back.file == 0 {
            self.reader_id = Some(FileId::of(&meta));
        }
        file.seek(SeekFrom::Start(self.back.offset))?;
        self.reader = Some(file);
        Ok(self.reader.as_mut())
    }

    /// The live `caudra.log` is a different file than the one the back cursor's
    /// offsets were measured against.
    fn rotated_while_closed(&self) -> bool {
        let Some(known) = self.reader_id else {
            return false;
        };
        match std::fs::metadata(self.path()) {
            Ok(meta) => FileId::of(&meta) != known,
            Err(_) => false,
        }
    }

    /// Called when the held handle is at EOF. Returns whether reading should
    /// continue in another file.
    fn advance_past_rotation(&mut self) -> io::Result<bool> {
        if self.back.file > 0 {
            self.back = Cursor {
                file: self.back.file - 1,
                offset: 0,
            };
            self.invalidate_reader();
            // The rotation this cursor was chasing is now behind us; the next
            // open re-learns the live file's identity.
            self.reader_id = None;
            return Ok(true);
        }
        if !self.rotated_while_closed() {
            return Ok(false);
        }
        self.note_rotation();
        self.back = Cursor { file: 0, offset: 0 };
        self.invalidate_reader();
        self.reader_id = None;
        Ok(self.path().exists())
    }

    /// Every retained byte just moved one file older, so the front cursor has
    /// to name the file its offset now lives in.
    fn note_rotation(&mut self) {
        self.front.file = self.front.file.saturating_add(1).min(self.max_files - 1);
    }

    fn older_file_end(&self, current: u32) -> io::Result<Option<Cursor>> {
        let next = current + 1;
        if next >= self.max_files {
            return Ok(None);
        }
        match std::fs::metadata(file_path(&self.dir, next)) {
            Ok(meta) => Ok(Some(Cursor {
                file: next,
                offset: meta.len(),
            })),
            Err(_) => Ok(None),
        }
    }
}

/// Identifies the file behind a path across renames. On unix that is the
/// inode. Elsewhere only a shrink can be detected, so the creation time stands
/// in and a rotation that preserves it degrades to the truncation check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileId(u64);

impl FileId {
    #[cfg(unix)]
    fn of(meta: &Metadata) -> Self {
        Self(meta.ino())
    }

    #[cfg(not(unix))]
    fn of(meta: &Metadata) -> Self {
        use std::time::UNIX_EPOCH;
        let nanos = meta
            .created()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or_default();
        Self(nanos)
    }
}

fn read_at(path: &Path, start: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

fn matching(raw: &[u8], filter: &Filter) -> Option<Line> {
    let raw = String::from_utf8_lossy(raw).into_owned();
    let entry = Entry::parse(&raw);
    filter.matches(&entry).then_some(Line { raw, entry })
}

fn push_back_match(found: &mut Vec<Line>, raw: &[u8], filter: &Filter) {
    if let Some(line) = matching(raw, filter) {
        found.push(line);
    }
}

/// A cursor moves by the line's bytes plus the newline the writer always emits.
fn advance(cursor: Cursor, line: &Line) -> Cursor {
    Cursor {
        offset: cursor.offset + line.raw.len() as u64 + 1,
        ..cursor
    }
}

fn retreat(cursor: Cursor, line: &Line) -> Cursor {
    Cursor {
        offset: cursor.offset.saturating_sub(line.raw.len() as u64 + 1),
        ..cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::record::Level;
    use std::io::Write;

    const CAPACITY: usize = 8;
    const MAX_FILES: u32 = 3;
    const NO_MATCH: &str = "nothing in the log says this";

    fn event(level: &str, message: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-09-09T14:22:07.418123Z","level":"{level}","fields":{{"message":"{message}"}},"target":"caudra::test"}}"#
        )
    }

    fn write_lines(path: &Path, lines: &[String]) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    fn messages(tail: &LogTail) -> Vec<String> {
        tail.window()
            .iter()
            .map(|line| match &line.entry {
                Entry::Record(record) => record.message.clone(),
                Entry::Raw(raw) => raw.clone(),
            })
            .collect()
    }

    fn seeded(dir: &Path, count: usize) -> LogTail {
        let lines: Vec<String> = (0..count).map(|i| event("INFO", &i.to_string())).collect();
        write_lines(&file_path(dir, 0), &lines);
        LogTail::open(dir, MAX_FILES, CAPACITY).unwrap()
    }

    #[test]
    fn tail_returns_the_newest_lines_only() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tail = seeded(tmp.path(), 20);
        tail.tail(&Filter::default()).unwrap();

        assert_eq!(tail.window().len(), CAPACITY);
        assert_eq!(
            messages(&tail),
            (12..20).map(|i| i.to_string()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn scroll_back_walks_further_into_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tail = seeded(tmp.path(), 20);
        tail.tail(&Filter::default()).unwrap();
        let advance = tail.scroll_back(4, &Filter::default()).unwrap();

        assert_eq!(advance.added, 4);
        assert_eq!(tail.window().len(), CAPACITY);
        assert_eq!(
            messages(&tail),
            (8..16).map(|i| i.to_string()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn scroll_back_reports_the_start_of_the_oldest_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tail = seeded(tmp.path(), 4);
        tail.tail(&Filter::default()).unwrap();

        assert_eq!(
            tail.scroll_back(CAPACITY, &Filter::default())
                .unwrap()
                .outcome,
            ScanOutcome::Exhausted
        );
    }

    #[test]
    fn a_line_longer_than_the_chunk_survives_the_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let long = "x".repeat(CHUNK_BYTES * 2);
        write_lines(
            &file_path(tmp.path(), 0),
            &[
                event("INFO", "before"),
                event("INFO", &long),
                event("INFO", "after"),
            ],
        );
        let mut tail = LogTail::open(tmp.path(), MAX_FILES, CAPACITY).unwrap();
        tail.tail(&Filter::default()).unwrap();

        let found = messages(&tail);
        assert_eq!(found.len(), 3);
        assert_eq!(found[1].len(), long.len());
    }

    #[test]
    fn scroll_back_crosses_into_rotated_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_lines(
            &file_path(tmp.path(), 1),
            &[event("INFO", "older-a"), event("INFO", "older-b")],
        );
        write_lines(&file_path(tmp.path(), 0), &[event("INFO", "newest")]);

        let mut tail = LogTail::open(tmp.path(), MAX_FILES, CAPACITY).unwrap();
        tail.tail(&Filter::default()).unwrap();

        assert_eq!(messages(&tail), ["older-a", "older-b", "newest"]);
    }

    #[test]
    fn poll_picks_up_appended_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tail = seeded(tmp.path(), 2);
        tail.tail(&Filter::default()).unwrap();

        write_lines(&file_path(tmp.path(), 0), &[event("WARN", "fresh")]);
        assert_eq!(tail.poll(&Filter::default()).unwrap(), 1);
        assert_eq!(messages(&tail).last().unwrap(), "fresh");
    }

    #[test]
    fn poll_leaves_a_partial_trailing_line_for_the_next_call() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tail = seeded(tmp.path(), 1);
        tail.tail(&Filter::default()).unwrap();

        let whole = event("INFO", "split");
        let (head, rest) = whole.split_at(whole.len() / 2);
        let path = file_path(tmp.path(), 0);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(file, "{head}").unwrap();
        file.flush().unwrap();
        assert_eq!(tail.poll(&Filter::default()).unwrap(), 0);

        writeln!(file, "{rest}").unwrap();
        file.flush().unwrap();
        assert_eq!(tail.poll(&Filter::default()).unwrap(), 1);
        assert_eq!(messages(&tail).last().unwrap(), "split");
    }

    #[test]
    fn poll_follows_the_writer_across_a_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tail = seeded(tmp.path(), 1);
        tail.tail(&Filter::default()).unwrap();

        let path = file_path(tmp.path(), 0);
        write_lines(&path, &[event("INFO", "last-before-rotate")]);
        std::fs::rename(&path, file_path(tmp.path(), 1)).unwrap();
        write_lines(&path, &[event("INFO", "first-after-rotate")]);

        let mut seen = Vec::new();
        for _ in 0..3 {
            tail.poll(&Filter::default()).unwrap();
            seen = messages(&tail);
        }
        assert!(seen.contains(&"last-before-rotate".to_string()), "{seen:?}");
        assert!(seen.contains(&"first-after-rotate".to_string()), "{seen:?}");
    }

    #[test]
    fn a_filter_that_matches_nothing_reports_the_scan_limit_or_exhaustion() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tail = seeded(tmp.path(), 50);
        let filter = Filter::new(Level::Trace, NO_MATCH);
        let outcome = tail.tail(&filter).unwrap();

        assert_eq!(outcome, ScanOutcome::Exhausted);
        assert!(tail.window().is_empty());
    }

    #[test]
    fn a_filter_keeps_filling_the_window_past_non_matching_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lines: Vec<String> = (0..100).map(|i| event("INFO", &i.to_string())).collect();
        lines.extend((0..CAPACITY).map(|i| event("ERROR", &format!("boom-{i}"))));
        lines.extend((0..100).map(|i| event("INFO", &format!("noise-{i}"))));
        write_lines(&file_path(tmp.path(), 0), &lines);

        let mut tail = LogTail::open(tmp.path(), MAX_FILES, CAPACITY).unwrap();
        let filter = Filter::new(Level::Error, "");
        tail.tail(&filter).unwrap();

        assert_eq!(tail.window().len(), CAPACITY);
        assert!(messages(&tail).iter().all(|m| m.starts_with("boom-")));
    }

    #[test]
    fn a_missing_log_file_opens_as_an_empty_window() {
        let tmp = tempfile::tempdir().unwrap();
        let mut tail = LogTail::open(tmp.path(), MAX_FILES, CAPACITY).unwrap();

        assert_eq!(
            tail.tail(&Filter::default()).unwrap(),
            ScanOutcome::Exhausted
        );
        assert!(tail.window().is_empty());
    }

    #[test]
    fn a_file_without_a_trailing_newline_still_yields_its_last_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = file_path(tmp.path(), 0);
        let mut file = std::fs::File::create(&path).unwrap();
        write!(file, "{}", event("INFO", "unterminated")).unwrap();
        file.flush().unwrap();

        let mut tail = LogTail::open(tmp.path(), MAX_FILES, CAPACITY).unwrap();
        tail.tail(&Filter::default()).unwrap();

        assert_eq!(messages(&tail), ["unterminated"]);
    }
}
