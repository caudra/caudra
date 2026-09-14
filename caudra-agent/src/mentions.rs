//! `@path` file mentions.
//!
//! A mention names a file, and optionally a line range, inside prose the user
//! typed: `@src/main.rs`, `@src/main.rs:L42`, `@src/main.rs:L42-L88`. The
//! workbench emits the same spelling when it sends a reference to the composer.
//!
//! Parsing alone cannot tell a mention from a decorator, an email address, or a
//! git revision, so every entry point pairs [`scan`] with an existence
//! predicate. A candidate that does not resolve on disk is left as prose.

use std::ops::{Range, RangeInclusive};
use std::path::{Path, PathBuf};

use caudra_workspace::WorkspacePath;
use serde::{Deserialize, Serialize};

const SIGIL: char = '@';
const RANGE_SEPARATOR: char = ':';
const LINE_PREFIXES: [char; 2] = ['L', 'l'];
const RANGE_SPAN: char = '-';
const QUOTE: char = '"';
const TRAILING_PUNCTUATION: [char; 9] = ['.', ',', ';', ':', '!', '?', ')', ']', '}'];
/// Characters a mention may follow, alongside whitespace and the start of the
/// text. Excluding everything else is what keeps `user@host` and `HEAD@{1}`
/// from ever reaching the existence check.
const OPENING_DELIMITERS: [char; 3] = ['(', '[', '{'];
/// How many candidates [`scan_in`] will stat. A prompt full of `@` must not
/// turn one keystroke or one mouse move into a burst of syscalls.
const MAX_CANDIDATES: usize = 32;

/// A resolved reference to a file, with the source text that produced it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mention {
    /// The mention exactly as it appears in the composer, sigil included. Kept
    /// verbatim so a restored draft renders the text the user actually typed.
    pub raw: String,
    pub target: MentionTarget,
    /// Inclusive and 1-based, matching how the workbench and editors count.
    pub lines: Option<RangeInclusive<usize>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "path", rename_all = "snake_case")]
pub enum MentionTarget {
    Local(PathBuf),
    Remote(WorkspacePath),
}

impl Mention {
    /// Builds a mention in canonical spelling, for insertions the UI originates
    /// rather than parses.
    pub fn new(path: impl Into<PathBuf>, lines: Option<RangeInclusive<usize>>) -> Self {
        let path = path.into();
        Self {
            raw: format(&path, lines.as_ref()),
            target: MentionTarget::Local(path),
            lines,
        }
    }

    pub fn remote(path: WorkspacePath, lines: Option<RangeInclusive<usize>>) -> Self {
        let raw = format_remote(&path, lines.as_ref());
        Self {
            raw,
            target: MentionTarget::Remote(path),
            lines,
        }
    }

    pub fn local_path(&self) -> Option<&Path> {
        match &self.target {
            MentionTarget::Local(path) => Some(path),
            MentionTarget::Remote(_) => None,
        }
    }

    pub fn remote_path(&self) -> Option<&WorkspacePath> {
        match &self.target {
            MentionTarget::Remote(path) => Some(path),
            MentionTarget::Local(_) => None,
        }
    }

    pub fn display_path(&self) -> &str {
        match &self.target {
            MentionTarget::Local(path) => path.to_str().unwrap_or("<non-UTF-8 path>"),
            MentionTarget::Remote(path) => path.as_str(),
        }
    }

    /// Whether the mention covers the file rather than a slice of it. Only a
    /// whole-file read may be recorded against the edit-staleness tracker.
    pub fn is_whole_file(&self) -> bool {
        self.lines.is_none()
    }
}

/// Renders `path` and `lines` in the spelling [`scan`] accepts.
pub fn format(path: &Path, lines: Option<&RangeInclusive<usize>>) -> String {
    let display = path.to_string_lossy();
    let mut out = String::with_capacity(display.len() + 2);
    out.push(SIGIL);
    match display.contains(char::is_whitespace) {
        true => {
            out.push(QUOTE);
            out.push_str(&display);
            out.push(QUOTE);
        }
        false => out.push_str(&display),
    }
    if let Some(lines) = lines {
        out.push(RANGE_SEPARATOR);
        out.push(LINE_PREFIXES[0]);
        out.push_str(&lines.start().to_string());
        if lines.start() != lines.end() {
            out.push(RANGE_SPAN);
            out.push(LINE_PREFIXES[0]);
            out.push_str(&lines.end().to_string());
        }
    }
    out
}

fn format_remote(path: &WorkspacePath, lines: Option<&RangeInclusive<usize>>) -> String {
    format_text(path.as_str(), lines)
}

fn format_text(display: &str, lines: Option<&RangeInclusive<usize>>) -> String {
    let mut out = String::with_capacity(display.len() + 2);
    out.push(SIGIL);
    if display.contains(char::is_whitespace) {
        out.push(QUOTE);
        out.push_str(display);
        out.push(QUOTE);
    } else {
        out.push_str(display);
    }
    if let Some(lines) = lines {
        out.push(RANGE_SEPARATOR);
        out.push(LINE_PREFIXES[0]);
        out.push_str(&lines.start().to_string());
        if lines.start() != lines.end() {
            out.push(RANGE_SPAN);
            out.push(LINE_PREFIXES[0]);
            out.push_str(&lines.end().to_string());
        }
    }
    out
}

/// Finds every mention in `text` that names a path under `cwd`, which is what a
/// caller holding a working directory wants instead of its own predicate.
pub fn scan_in(text: &str, cwd: &Path) -> Vec<(Range<usize>, Mention)> {
    let mut candidates = 0;
    scan(text, |path| {
        candidates += 1;
        candidates <= MAX_CANDIDATES && cwd.join(path).exists()
    })
}

/// Finds every mention in `text` whose path satisfies `exists`.
///
/// Ranges are char offsets, matching the composer's span model. Mentions never
/// overlap: a match consumes its own text before scanning resumes.
pub fn scan(text: &str, mut exists: impl FnMut(&Path) -> bool) -> Vec<(Range<usize>, Mention)> {
    let mut found = Vec::new();
    let mut boundary = true;
    let mut cursor = text.char_indices().enumerate();
    while let Some((char_index, (byte_index, character))) = cursor.next() {
        if character == SIGIL
            && boundary
            && let Some((end, mention)) = parse_from(text, byte_index)
            && mention.local_path().is_some_and(&mut exists)
        {
            let width = text[byte_index..end].chars().count();
            found.push((char_index..char_index + width, mention));
            for _ in 1..width {
                cursor.next();
            }
            boundary = false;
            continue;
        }
        boundary = character.is_whitespace() || OPENING_DELIMITERS.contains(&character);
    }
    found
}

/// Parses workspace-relative mentions without probing the client filesystem.
pub fn scan_remote(text: &str) -> Vec<(Range<usize>, Mention)> {
    let mut found = Vec::new();
    let mut boundary = true;
    let mut candidates = 0;
    let mut cursor = text.char_indices().enumerate();
    while let Some((char_index, (byte_index, character))) = cursor.next() {
        if character == SIGIL && boundary && candidates < MAX_CANDIDATES {
            candidates += 1;
            if let Some((end, path, lines)) = parse_parts(text, byte_index)
                && let Ok(path) = WorkspacePath::new(path.to_owned())
            {
                let mention = Mention {
                    raw: text[byte_index..end].to_owned(),
                    target: MentionTarget::Remote(path),
                    lines,
                };
                let width = text[byte_index..end].chars().count();
                found.push((char_index..char_index + width, mention));
                for _ in 1..width {
                    cursor.next();
                }
                boundary = false;
                continue;
            }
        }
        boundary = character.is_whitespace() || OPENING_DELIMITERS.contains(&character);
    }
    found
}

/// Parses one mention starting at `at`, a byte offset that must land on the
/// sigil. Returns the byte offset just past the mention.
fn parse_from(text: &str, at: usize) -> Option<(usize, Mention)> {
    let (end, path, lines) = parse_parts(text, at)?;
    Some((
        end,
        Mention {
            raw: text[at..end].to_owned(),
            target: MentionTarget::Local(PathBuf::from(path)),
            lines,
        },
    ))
}

fn parse_parts(text: &str, at: usize) -> Option<(usize, &str, Option<RangeInclusive<usize>>)> {
    let body = text.get(at..)?.strip_prefix(SIGIL)?;
    let (path, lines, consumed) = match body.strip_prefix(QUOTE) {
        Some(quoted) => {
            let close = quoted.find(QUOTE)?;
            let tail = &quoted[close + QUOTE.len_utf8()..];
            let (lines, range_len) = match parse_lines(tail) {
                Some((lines, len)) => (Some(lines), len),
                None => (None, 0),
            };
            (
                &quoted[..close],
                lines,
                QUOTE.len_utf8() * 2 + close + range_len,
            )
        }
        None => {
            let end = body.find(char::is_whitespace).unwrap_or(body.len());
            let run = body[..end].trim_end_matches(TRAILING_PUNCTUATION);
            let (path, lines) = split_lines(run);
            (path, lines, run.len())
        }
    };
    if path.is_empty() {
        return None;
    }
    Some((at + SIGIL.len_utf8() + consumed, path, lines))
}

/// Splits a trailing `:L12-L20` off an unquoted run. The suffix counts only
/// when it reaches the end of the run, so a colon inside a filename is kept.
fn split_lines(run: &str) -> (&str, Option<RangeInclusive<usize>>) {
    let Some(separator) = run.rfind(RANGE_SEPARATOR) else {
        return (run, None);
    };
    match parse_lines(&run[separator..]) {
        Some((lines, len)) if separator + len == run.len() => (&run[..separator], Some(lines)),
        _ => (run, None),
    }
}

/// Parses a `:L12` or `:L12-L20` suffix, returning the bytes it consumed. The
/// `L` on the second bound is optional so `:L12-20` also reads.
fn parse_lines(tail: &str) -> Option<(RangeInclusive<usize>, usize)> {
    let after_separator = tail.strip_prefix(RANGE_SEPARATOR)?;
    let (start, rest) = take_number(after_separator.strip_prefix(LINE_PREFIXES)?)?;
    let (end, rest) = match rest.strip_prefix(RANGE_SPAN) {
        Some(after_span) => {
            take_number(after_span.strip_prefix(LINE_PREFIXES).unwrap_or(after_span))?
        }
        None => (start, rest),
    };
    if start == 0 || end < start {
        return None;
    }
    Some((start..=end, tail.len() - rest.len()))
}

fn take_number(text: &str) -> Option<(usize, &str)> {
    let end = text
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(text.len());
    text.get(..end)?
        .parse()
        .ok()
        .map(|number| (number, &text[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const WHOLE: &str = "src/main.rs";
    const SPACED: &str = "my notes.md";

    fn anything(_: &Path) -> bool {
        true
    }

    fn only(expected: &'static str) -> impl FnMut(&Path) -> bool {
        move |path| path == Path::new(expected)
    }

    #[test_case("@src/main.rs", WHOLE, None ; "whole_file")]
    #[test_case("@src/main.rs:L42", WHOLE, Some(42..=42) ; "single_line")]
    #[test_case("@src/main.rs:L42-L88", WHOLE, Some(42..=88) ; "line_range")]
    #[test_case("@src/main.rs:l42-l88", WHOLE, Some(42..=88) ; "lowercase_prefix")]
    #[test_case("@src/main.rs:L42-88", WHOLE, Some(42..=88) ; "second_bound_without_prefix")]
    #[test_case("@src/", "src/", None ; "directory")]
    #[test_case("@\"my notes.md\"", SPACED, None ; "quoted_path")]
    #[test_case("@\"my notes.md\":L3-L5", SPACED, Some(3..=5) ; "quoted_path_with_range")]
    #[test_case("see @src/main.rs now", WHOLE, None ; "mid_sentence")]
    #[test_case("@src/main.rs.", WHOLE, None ; "trailing_period")]
    #[test_case("(@src/main.rs)", WHOLE, None ; "trailing_bracket")]
    #[test_case("@src/main.rs:L42,", WHOLE, Some(42..=42) ; "range_then_comma")]
    fn scan_reads_a_mention(text: &str, path: &str, lines: Option<RangeInclusive<usize>>) {
        let found = scan(text, anything);
        assert_eq!(found.len(), 1, "{text}");
        assert_eq!(found[0].1.local_path(), Some(Path::new(path)));
        assert_eq!(found[0].1.lines, lines);
    }

    #[test_case("@dataclass" ; "decorator")]
    #[test_case("@media screen" ; "css_at_rule")]
    #[test_case("user@host.com" ; "email")]
    #[test_case("HEAD@{1}" ; "git_revision")]
    #[test_case("email me at foo@bar.io" ; "email_after_whitespace_word")]
    fn scan_rejects_what_does_not_resolve(text: &str) {
        assert!(scan(text, only(WHOLE)).is_empty(), "{text}");
    }

    #[test_case("@" ; "bare_sigil")]
    #[test_case("@ src/main.rs" ; "detached_sigil")]
    #[test_case("@\"unterminated" ; "unterminated_quote")]
    fn scan_rejects_malformed_input(text: &str) {
        assert!(scan(text, anything).is_empty(), "{text}");
    }

    #[test]
    fn scan_spans_cover_the_mention_text() {
        const TEXT: &str = "look at @src/main.rs:L1-L2 please";
        let found = scan(TEXT, anything);
        let (range, mention) = &found[0];
        let chars: String = TEXT.chars().take(range.end).skip(range.start).collect();
        assert_eq!(chars, mention.raw);
        assert_eq!(mention.raw, "@src/main.rs:L1-L2");
    }

    #[test]
    fn scan_finds_every_mention() {
        let found = scan("@a.rs and @b.rs:L3", anything);
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].1.lines, Some(3..=3));
    }

    #[test]
    fn scan_does_not_match_inside_a_mention() {
        assert_eq!(scan("@a@b.rs", anything).len(), 1);
    }

    #[test]
    fn scan_offsets_are_chars_not_bytes() {
        const TEXT: &str = "café @src/main.rs";
        let found = scan(TEXT, anything);
        assert_eq!(found[0].0.start, 5);
    }

    #[test_case(WHOLE, None, "@src/main.rs" ; "whole_file")]
    #[test_case(WHOLE, Some(42..=42), "@src/main.rs:L42" ; "single_line")]
    #[test_case(WHOLE, Some(42..=88), "@src/main.rs:L42-L88" ; "line_range")]
    #[test_case(SPACED, None, "@\"my notes.md\"" ; "quoted_path")]
    #[test_case(SPACED, Some(3..=5), "@\"my notes.md\":L3-L5" ; "quoted_path_with_range")]
    fn format_round_trips_through_scan(
        path: &str,
        lines: Option<RangeInclusive<usize>>,
        expected: &str,
    ) {
        let rendered = format(Path::new(path), lines.as_ref());
        assert_eq!(rendered, expected);
        let found = scan(&rendered, anything);
        assert_eq!(found[0].1.local_path(), Some(Path::new(path)));
        assert_eq!(found[0].1.lines, lines);
    }

    #[test_case(0, 5 ; "zero_start")]
    #[test_case(9, 4 ; "end_before_start")]
    fn scan_rejects_an_impossible_range(start: usize, end: usize) {
        let text = format!("@src/main.rs:L{start}-L{end}");
        let found = scan(&text, anything);
        assert_eq!(found[0].1.lines, None, "{text}");
    }

    #[test]
    fn new_canonicalises_the_raw_text() {
        let mention = Mention::new(WHOLE, Some(1..=2));
        assert_eq!(mention.raw, "@src/main.rs:L1-L2");
        assert!(!mention.is_whole_file());
        assert!(Mention::new(WHOLE, None).is_whole_file());
    }
}
