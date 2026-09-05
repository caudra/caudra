//! The content search worker.
//!
//! A search is a walk, so it runs on its own thread and streams matches back
//! over a channel: the first hit in a large tree belongs on screen long before
//! the last directory has been read. The walk is single-threaded and sorted, so
//! the list a user reads is the same one they would get from `git grep` rather
//! than whatever order the threads happened to finish in.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use flume::{Receiver, Sender};
use globset::{Glob, GlobSet, GlobSetBuilder};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};

use crate::fs::tree::GIT_DIR;

const MAX_HITS: usize = 5_000;
const MAX_PER_FILE: usize = 100;
const MAX_LINE_CHARS: usize = 400;
const MAX_FILE_SIZE: u64 = 2 * 1024 * 1024;
const GLOB_SEPARATOR: char = ',';

/// What the pane is looking for. Everything a run needs, so a worker never
/// reaches back into the pane it was started from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub text: String,
    /// Comma-separated globs. Empty means every file the walk yields.
    pub include: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub path: PathBuf,
    pub line: u64,
    pub text: String,
    /// Char range of the match inside `text`, already clamped to the part of
    /// the line the pane will show.
    pub range: (usize, usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Hit(Hit),
    /// A cap cut the results short. The pane says so rather than pretending the
    /// list is complete.
    Truncated,
    Done,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("{0} is not a valid pattern: {1}")]
    Pattern(String, String),
    #[error("{0} is not a valid glob: {1}")]
    Glob(String, String),
}

/// A search in flight. Dropping it stops the worker, so closing the pane or
/// starting a new query never leaves a thread walking a tree nobody is reading.
pub struct Run {
    events: Receiver<Event>,
    cancel: Arc<AtomicBool>,
    finished: bool,
}

impl Run {
    pub fn start(root: &Path, query: &Query) -> Result<Self, SearchError> {
        let matcher = build_matcher(query)?;
        let include = build_globs(&query.include)?;
        // Unbounded so a worker never blocks on a pane that is not
        // draining; `MAX_HITS` is what actually bounds the memory.
        let (sender, events) = flume::unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker = Worker {
            root: root.to_path_buf(),
            hidden: query.hidden,
            matcher,
            include,
            sender,
            cancel: Arc::clone(&cancel),
        };
        thread::spawn(move || worker.run());
        Ok(Self {
            events,
            cancel,
            finished: false,
        })
    }

    /// Takes whatever the worker has produced without waiting for more.
    pub fn drain(&mut self) -> Vec<Event> {
        let mut events: Vec<Event> = self.events.try_iter().collect();
        let gone = self.events.is_disconnected();
        if gone {
            // Nothing can arrive once the worker's sender is dropped, so this
            // pass cannot miss whatever it queued between the drain and the
            // check above. Without it a run could be declared finished while
            // its last hits were still sitting in the channel.
            events.extend(self.events.try_iter());
        }
        self.finished |= gone || events.contains(&Event::Done);
        events
    }

    pub fn is_running(&self) -> bool {
        !self.finished
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

struct Worker {
    root: PathBuf,
    hidden: bool,
    matcher: RegexMatcher,
    include: Option<GlobSet>,
    sender: Sender<Event>,
    cancel: Arc<AtomicBool>,
}

impl Worker {
    fn run(self) {
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(0))
            .line_number(true)
            .build();
        let mut budget = MAX_HITS;

        let walk = ignore::WalkBuilder::new(&self.root)
            .hidden(!self.hidden)
            .filter_entry(|entry| entry.file_name() != GIT_DIR)
            .sort_by_file_path(Path::cmp)
            .build();

        for entry in walk {
            if self.cancelled() {
                return;
            }
            let Ok(entry) = entry else {
                continue;
            };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let path = entry.into_path();
            if !self.wanted(&path) {
                continue;
            }

            let mut sink = HitSink {
                path: &path,
                matcher: &self.matcher,
                sender: &self.sender,
                cancel: &self.cancel,
                budget: &mut budget,
                per_file: MAX_PER_FILE,
            };
            let _ = searcher.search_path(&self.matcher, &path, &mut sink);
            if budget == 0 {
                let _ = self.sender.send(Event::Truncated);
                break;
            }
        }
        let _ = self.sender.send(Event::Done);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn wanted(&self, path: &Path) -> bool {
        if path.metadata().is_ok_and(|meta| meta.len() > MAX_FILE_SIZE) {
            return false;
        }
        match (&self.include, path.strip_prefix(&self.root)) {
            (None, _) => true,
            (Some(globs), Ok(relative)) => globs.is_match(relative),
            (Some(globs), Err(_)) => globs.is_match(path),
        }
    }
}

struct HitSink<'a> {
    path: &'a Path,
    matcher: &'a RegexMatcher,
    sender: &'a Sender<Event>,
    cancel: &'a AtomicBool,
    budget: &'a mut usize,
    per_file: usize,
}

impl Sink for HitSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, found: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.cancel.load(Ordering::Relaxed) || *self.budget == 0 || self.per_file == 0 {
            return Ok(false);
        }
        let Some(hit) = self.hit(found) else {
            return Ok(true);
        };
        *self.budget -= 1;
        self.per_file -= 1;
        // A closed channel means the pane stopped reading; there is no point
        // walking the rest of the tree for it.
        match self.sender.send(Event::Hit(hit)) {
            Err(_) => Ok(false),
            Ok(()) => Ok(*self.budget > 0 && self.per_file > 0),
        }
    }
}

impl HitSink<'_> {
    fn hit(&self, found: &SinkMatch<'_>) -> Option<Hit> {
        let line = found.line_number()?;
        let bytes = found.bytes();
        let text = String::from_utf8_lossy(bytes);
        let text = text.trim_end_matches(['\n', '\r']);
        let at = self.matcher.find(bytes).ok().flatten();
        let range = at.map_or((0, 0), |at| {
            (
                text.get(..at.start())
                    .map_or(0, |head| head.chars().count()),
                text.get(..at.end()).map_or(0, |head| head.chars().count()),
            )
        });
        let (text, range) = clamp(text, range);
        Some(Hit {
            path: self.path.to_path_buf(),
            line,
            text,
            range,
        })
    }
}

/// Keeps a minified bundle or a generated table from filling the pane with one
/// enormous row, without pushing the match itself off the end.
fn clamp(text: &str, range: (usize, usize)) -> (String, (usize, usize)) {
    if text.chars().count() <= MAX_LINE_CHARS {
        return (text.to_owned(), range);
    }
    let start = range.0.saturating_sub(MAX_LINE_CHARS / 4);
    let kept: String = text.chars().skip(start).take(MAX_LINE_CHARS).collect();
    let last = kept.chars().count();
    (kept, (range.0 - start, (range.1 - start).min(last)))
}

fn build_matcher(query: &Query) -> Result<RegexMatcher, SearchError> {
    RegexMatcherBuilder::new()
        .fixed_strings(!query.regex)
        .case_insensitive(!query.case_sensitive)
        .word(query.whole_word)
        .line_terminator(Some(b'\n'))
        .build(&query.text)
        .map_err(|error| SearchError::Pattern(query.text.clone(), error.to_string()))
}

fn build_globs(include: &str) -> Result<Option<GlobSet>, SearchError> {
    let patterns: Vec<&str> = include
        .split(GLOB_SEPARATOR)
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .collect();
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)
            .map_err(|error| SearchError::Glob(pattern.to_owned(), error.to_string()))?;
        builder.add(glob);
    }
    builder
        .build()
        .map(Some)
        .map_err(|error| SearchError::Glob(include.to_owned(), error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::{Event, Hit, MAX_LINE_CHARS, Query, Run, build_globs, clamp};

    const NO_HITS: &str = "the search found nothing where the fixture put a match";
    const WRONG_LINE: &str = "the hit points at the wrong line";
    const WRONG_RANGE: &str = "the highlighted range does not cover the match";
    const LEAKED: &str = "a file the filters exclude turned up in the results";

    /// A tree with a match in a Rust file, one in a text file, one behind a
    /// dotfile, and one inside `.git`.
    fn fixture() -> TempDir {
        let dir = TempDir::new().expect("a temporary directory");
        fs::write(dir.path().join("a.rs"), "fn main() {}\nlet needle = 1;\n").expect("a file");
        fs::write(dir.path().join("b.txt"), "a needle in text\n").expect("a file");
        fs::write(dir.path().join(".hidden"), "needle\n").expect("a file");
        fs::create_dir(dir.path().join(".git")).expect("a directory");
        fs::write(dir.path().join(".git/config"), "needle\n").expect("a file");
        dir
    }

    fn run(root: &Path, query: Query) -> Vec<Hit> {
        let mut run = Run::start(root, &query).expect("a search");
        let mut hits = Vec::new();
        loop {
            for event in run.drain() {
                match event {
                    Event::Hit(hit) => hits.push(hit),
                    Event::Done => return hits,
                    Event::Truncated => {}
                }
            }
            if !run.is_running() {
                return hits;
            }
        }
    }

    fn query(text: &str) -> Query {
        Query {
            text: text.to_owned(),
            ..Query::default()
        }
    }

    fn names(hits: &[Hit]) -> Vec<String> {
        hits.iter()
            .filter_map(|hit| hit.path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_literal_query_finds_every_visible_file() {
        let dir = fixture();
        let hits = run(dir.path(), query("needle"));
        assert_eq!(names(&hits), vec!["a.rs", "b.txt"], "{NO_HITS}");
    }

    #[test]
    fn the_git_directory_is_never_searched() {
        let dir = fixture();
        let hits = run(
            dir.path(),
            Query {
                hidden: true,
                ..query("needle")
            },
        );
        assert!(!names(&hits).contains(&"config".to_owned()), "{LEAKED}");
        assert!(names(&hits).contains(&".hidden".to_owned()), "{NO_HITS}");
    }

    #[test]
    fn a_hit_carries_its_line_and_the_range_of_the_match() {
        let dir = fixture();
        let hits = run(dir.path(), query("needle"));
        let hit = hits.first().expect("a hit");
        assert_eq!(hit.line, 2, "{WRONG_LINE}");
        assert_eq!(hit.text, "let needle = 1;", "{WRONG_LINE}");
        assert_eq!(
            &hit.text[hit.range.0..hit.range.1],
            "needle",
            "{WRONG_RANGE}"
        );
    }

    #[test]
    fn an_include_glob_narrows_the_walk() {
        let dir = fixture();
        let hits = run(
            dir.path(),
            Query {
                include: "*.rs".to_owned(),
                ..query("needle")
            },
        );
        assert_eq!(names(&hits), vec!["a.rs"], "{LEAKED}");
    }

    #[test]
    fn a_case_sensitive_query_ignores_the_other_casing() {
        let dir = TempDir::new().expect("a temporary directory");
        fs::write(dir.path().join("a.txt"), "Needle\nneedle\n").expect("a file");
        let hits = run(
            dir.path(),
            Query {
                case_sensitive: true,
                ..query("needle")
            },
        );
        assert_eq!(hits.len(), 1, "{NO_HITS}");
        assert_eq!(hits[0].line, 2, "{WRONG_LINE}");
    }

    #[test]
    fn a_whole_word_query_skips_a_longer_word() {
        let dir = TempDir::new().expect("a temporary directory");
        fs::write(dir.path().join("a.txt"), "needles\nneedle\n").expect("a file");
        let hits = run(
            dir.path(),
            Query {
                whole_word: true,
                ..query("needle")
            },
        );
        assert_eq!(hits.len(), 1, "{NO_HITS}");
        assert_eq!(hits[0].line, 2, "{WRONG_LINE}");
    }

    #[test]
    fn a_literal_query_does_not_read_as_a_pattern() {
        let dir = TempDir::new().expect("a temporary directory");
        fs::write(dir.path().join("a.txt"), "a.b\naxb\n").expect("a file");
        let hits = run(dir.path(), query("a.b"));
        assert_eq!(hits.len(), 1, "{NO_HITS}");
        assert_eq!(hits[0].line, 1, "{WRONG_LINE}");
    }

    #[test]
    fn a_regex_query_reads_as_a_pattern() {
        let dir = TempDir::new().expect("a temporary directory");
        fs::write(dir.path().join("a.txt"), "a.b\naxb\n").expect("a file");
        let hits = run(
            dir.path(),
            Query {
                regex: true,
                ..query("a.b")
            },
        );
        assert_eq!(hits.len(), 2, "{NO_HITS}");
    }

    #[test]
    fn an_invalid_pattern_is_reported_rather_than_searched() {
        let dir = TempDir::new().expect("a temporary directory");
        let outcome = Run::start(
            dir.path(),
            &Query {
                regex: true,
                ..query("a(")
            },
        );
        assert!(outcome.is_err(), "{NO_HITS}");
    }

    #[test]
    fn an_overlong_line_keeps_the_match_visible() {
        let padding = "x".repeat(MAX_LINE_CHARS * 2);
        let (text, range) = clamp(
            &format!("{padding}needle"),
            (padding.len(), padding.len() + 6),
        );
        assert!(text.chars().count() <= MAX_LINE_CHARS, "{WRONG_RANGE}");
        assert_eq!(&text[range.0..range.1], "needle", "{WRONG_RANGE}");
    }

    #[test_case("" ; "empty")]
    #[test_case("  ,  " ; "only separators")]
    fn a_blank_include_filters_nothing(include: &str) {
        assert!(
            build_globs(include).expect("a glob set").is_none(),
            "{LEAKED}"
        );
    }
}
