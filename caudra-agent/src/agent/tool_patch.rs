//! The files a `file_apply_patch` call has described so far, read out of its
//! patch text as it arrives.
//!
//! Each file's section of the envelope is drawn the way a write's body is: as
//! far as it has arrived, the file still being written included, while the
//! files before it are already whole. A line is drawn from its first
//! character, once that says it is a change line; one that could still turn
//! out to be a marker or a hunk header waits for its newline.
//!
//! Advisory only, like [`crate::patch`]: Workcell remains the sole parser and
//! the only thing that touches a file. Nothing here reads one, so a hunk has
//! no line numbers, and the call's own output replaces the whole list once it
//! has run. Nothing is applied before then.

use std::mem;

use super::tool_body::LIVE_BODY_MAX_BYTES;
use crate::patch::{ADD_FILE, DELETE_FILE, END_PATCH, MARKER, MOVE_TO, UPDATE_FILE};
use crate::types::PatchedFile;

/// Opens a chunk, and the hunk a section that never marked one is drawn under.
const HUNK: &str = "@@";
const CONTEXT: char = ' ';
const REMOVED: char = '-';
const ADDED: char = '+';

#[derive(Clone, Copy, PartialEq)]
enum Verb {
    Add,
    Update,
    Delete,
}

const VERBS: [(&str, Verb); 3] = [
    (ADD_FILE, Verb::Add),
    (UPDATE_FILE, Verb::Update),
    (DELETE_FILE, Verb::Delete),
];

/// One file's section of the envelope, as far as it has arrived.
struct Section {
    verb: Verb,
    path: String,
    patch: String,
    additions: usize,
    deletions: usize,
    /// A hunk header is already in [`Self::patch`].
    hunked: bool,
    /// A body line arrived, so there is a change to draw.
    changed: bool,
    truncated: bool,
}

impl Section {
    fn new(verb: Verb, path: &str) -> Self {
        Self {
            verb,
            path: path.trim().to_owned(),
            patch: String::new(),
            additions: 0,
            deletions: 0,
            hunked: false,
            changed: false,
            truncated: false,
        }
    }

    /// The mark of a change line Workcell would read for this verb, `None`
    /// for anything else: a line it would refuse, a hunk header, or a marker.
    fn change(&self, line: &str) -> Option<char> {
        let first = line.chars().next()?;
        match (self.verb, first) {
            (Verb::Add | Verb::Update, ADDED) | (Verb::Update, REMOVED | CONTEXT) => Some(first),
            _ => None,
        }
    }

    /// Reads one line the way Workcell reads it for this verb, and says
    /// whether it was drawn. A line Workcell would refuse is left out, and the
    /// refusal arrives with the call's own output.
    fn absorb(&mut self, line: &str, spent: &mut usize) -> bool {
        if self.verb == Verb::Update && line.starts_with(HUNK) {
            self.hunked = true;
            keep(&mut self.patch, &mut self.truncated, spent, line);
            return true;
        }
        let Some(mark) = self.change(line) else {
            return false;
        };
        count(mark, &mut self.additions, &mut self.deletions);
        self.changed = true;
        if !self.hunked {
            self.hunked = true;
            keep(&mut self.patch, &mut self.truncated, spent, HUNK);
        }
        keep(&mut self.patch, &mut self.truncated, spent, line);
        true
    }

    /// The section as drawn so far, with `partial`, the line still being
    /// decoded, read as if it ended here. Nothing is kept from it, so it is
    /// counted once its newline arrives and not before. `None` while there
    /// is nothing to draw, which a deletion never has.
    fn preview(&self, partial: &str, spent: usize) -> Option<PatchedFile> {
        let partial = partial.strip_suffix('\r').unwrap_or(partial);
        let mut file = PatchedFile {
            path: self.path.clone(),
            patch: self.patch.clone(),
            additions: self.additions,
            deletions: self.deletions,
            truncated: self.truncated,
        };
        let Some(mark) = self.change(partial) else {
            return self.changed.then_some(file);
        };
        let mut spent = spent;
        count(mark, &mut file.additions, &mut file.deletions);
        if !self.hunked {
            keep(&mut file.patch, &mut file.truncated, &mut spent, HUNK);
        }
        keep(&mut file.patch, &mut file.truncated, &mut spent, partial);
        Some(file)
    }
}

fn count(mark: char, additions: &mut usize, deletions: &mut usize) {
    match mark {
        ADDED => *additions += 1,
        REMOVED => *deletions += 1,
        _ => {}
    }
}

/// Keeps `line` while the budget every section shares allows, and cuts the
/// patch short from the first line it does not. The counts keep going.
fn keep(patch: &mut String, truncated: &mut bool, spent: &mut usize, line: &str) {
    let cost = line.len() + 1;
    if *truncated || spent.saturating_add(cost) > LIVE_BODY_MAX_BYTES {
        *truncated = true;
        *spent = LIVE_BODY_MAX_BYTES;
        return;
    }
    *spent += cost;
    patch.push_str(line);
    patch.push('\n');
}

/// The files of one patch as its decoded characters arrive.
#[derive(Default)]
pub(super) struct PatchStream {
    /// The line being decoded, bounded like the text kept from it.
    line: String,
    open: Option<Section>,
    /// Every section that has closed.
    files: Vec<PatchedFile>,
    /// Bytes of patch text kept across every section.
    spent: usize,
    /// What [`Self::files`] draws changed since it was last asked.
    unpublished: bool,
    /// The envelope or the string holding it closed.
    ended: bool,
}

impl PatchStream {
    pub(super) fn push(&mut self, c: char) {
        if self.ended {
            return;
        }
        if c != '\n' {
            if self.line.len() < LIVE_BODY_MAX_BYTES {
                self.line.push(c);
                self.unpublished |= self.line_shows();
            }
            return;
        }
        let mut line = mem::take(&mut self.line);
        self.end_line(line.strip_suffix('\r').unwrap_or(&line));
        line.clear();
        self.line = line;
    }

    /// The string holding the patch closed, which ends the last section even
    /// when the envelope never did.
    pub(super) fn finish(&mut self) {
        if self.ended {
            return;
        }
        let line = mem::take(&mut self.line);
        self.end_line(line.strip_suffix('\r').unwrap_or(&line));
        self.close();
        self.ended = true;
    }

    /// Every file described so far, the last of them possibly still being
    /// written, when that changed since the last call.
    pub(super) fn files(&mut self) -> Option<Vec<PatchedFile>> {
        if !mem::take(&mut self.unpublished) {
            return None;
        }
        let mut files = self.files.clone();
        files.extend(
            self.open
                .as_ref()
                .and_then(|section| section.preview(&self.line, self.spent)),
        );
        (!files.is_empty()).then_some(files)
    }

    /// The line being decoded grew what the open section draws: it is a
    /// change line, and either its mark just arrived, which counts it, or
    /// there is still room for its text.
    fn line_shows(&self) -> bool {
        let Some(section) = &self.open else {
            return false;
        };
        let first = self.line.chars().nth(1).is_none();
        section.change(&self.line).is_some() && (first || self.spent < LIVE_BODY_MAX_BYTES)
    }

    fn end_line(&mut self, line: &str) {
        if line.trim() == END_PATCH {
            self.close();
            self.ended = true;
            return;
        }
        let Some(rest) = line.strip_prefix(MARKER) else {
            if let Some(section) = self.open.as_mut() {
                self.unpublished |= section.absorb(line, &mut self.spent);
            }
            return;
        };
        // A move names the file by where it ends up, as the call's output does.
        if let Some(destination) = rest.strip_prefix(MOVE_TO) {
            if let Some(section) = self.open.as_mut().filter(|s| s.verb == Verb::Update) {
                section.path = destination.trim().to_owned();
                self.unpublished = true;
            }
            return;
        }
        let opened = VERBS.iter().find_map(|(verb, kind)| {
            rest.strip_prefix(verb)
                .map(|path| Section::new(*kind, path))
        });
        if let Some(section) = opened {
            self.close();
            self.open = Some(section);
        }
    }

    fn close(&mut self) {
        let finished = self.open.take();
        if let Some(file) = finished.and_then(|section| section.preview("", self.spent)) {
            self.files.push(file);
            self.unpublished = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LIVE_BODY_MAX_BYTES, PatchStream};
    use crate::types::PatchedFile;
    use test_case::test_case;

    const ONE_UPDATE: &str = "*** Begin Patch\n*** Update File: a.rs\n@@\n-one\n+two\n";
    const DRAWN_MSG: &str = "a section is drawn as far as it has arrived";
    const DELETE_MSG: &str = "a deletion has no content to draw";
    const WAITS_MSG: &str = "a line that could still be a marker waits for its newline";

    /// Every list published, in order, after feeding `text` one character at a
    /// time, and closing the string when `close` says so.
    fn published(text: &str, close: bool) -> Vec<Vec<PatchedFile>> {
        let mut stream = PatchStream::default();
        let mut lists = Vec::new();
        for c in text.chars() {
            stream.push(c);
            lists.extend(stream.files());
        }
        if close {
            stream.finish();
            lists.extend(stream.files());
        }
        lists
    }

    /// What the card draws once all of `text` has arrived, asked for once, so
    /// a long patch is not copied out per character.
    fn last(text: &str, close: bool) -> Vec<PatchedFile> {
        let mut stream = PatchStream::default();
        text.chars().for_each(|c| stream.push(c));
        if close {
            stream.finish();
        }
        stream.files().expect(DRAWN_MSG)
    }

    fn paths(files: &[PatchedFile]) -> Vec<&str> {
        files.iter().map(|file| file.path.as_str()).collect()
    }

    fn only_file(text: &str, close: bool) -> PatchedFile {
        let mut files = last(text, close);
        assert_eq!(files.len(), 1, "{DRAWN_MSG}");
        files.remove(0)
    }

    #[test]
    fn the_file_being_written_follows_the_finished_ones() {
        let text = format!("{ONE_UPDATE}*** Update File: b.rs\n-x\n+y");
        let files = last(&text, false);
        assert_eq!(paths(&files), ["a.rs", "b.rs"], "{DRAWN_MSG}");
        assert_eq!(files[0].patch, "@@\n-one\n+two\n");
        assert_eq!(files[1].patch, "@@\n-x\n+y\n");
    }

    #[test_case(&format!("{ONE_UPDATE}*** End Patch\n"), false ; "at_the_end_of_the_envelope")]
    #[test_case(&format!("{ONE_UPDATE}*** End Patch"), true ; "at_an_end_marker_the_string_closed_on")]
    #[test_case("*** Update File: a.rs\n-one\n+two", true ; "when_the_string_closes_without_one")]
    #[test_case(ONE_UPDATE, false ; "while_it_is_still_open")]
    fn the_last_section_is_drawn_whole(text: &str, close: bool) {
        let file = only_file(text, close);
        assert_eq!(file.path, "a.rs");
        assert_eq!(file.patch, "@@\n-one\n+two\n", "{DRAWN_MSG}");
        assert_eq!((file.additions, file.deletions), (1, 1));
    }

    /// A line is drawn from the character that says what it is, and grows
    /// with every one after it, the way a write's body does.
    #[test]
    fn the_open_file_grows_with_every_character_of_a_change_line() {
        let lists = published("*** Update File: a.rs\n+ab\n*** Upd", false);
        let mut patches: Vec<&str> = lists.iter().map(|files| files[0].patch.as_str()).collect();
        patches.dedup();
        assert_eq!(patches, ["@@\n+\n", "@@\n+a\n", "@@\n+ab\n"], "{DRAWN_MSG}");
        assert!(
            lists.iter().all(|files| files[0].additions == 1),
            "a line is counted once, from its mark to its newline"
        );
    }

    #[test_case("*** Update File: a.rs\n-one\n+tw", "@@\n-one\n+tw\n", (1, 1) ; "an_addition")]
    #[test_case("*** Update File: a.rs\n-one\n ctx", "@@\n-one\n ctx\n", (0, 1) ; "a_context_line")]
    #[test_case("*** Update File: a.rs\n-one\n*** Upd", "@@\n-one\n", (0, 1) ; "a_marker_waits")]
    #[test_case("*** Update File: a.rs\n-one\n@@ fn", "@@\n-one\n", (0, 1) ; "a_hunk_header_waits")]
    #[test_case("*** Add File: a.rs\n+x\n-y", "@@\n+x\n", (1, 0) ; "an_add_refuses_a_removal")]
    #[test_case("*** Update File: a.rs\n+tw\r", "@@\n+tw\n", (1, 0) ; "a_carriage_return_is_not_drawn")]
    fn a_half_written_line_is_drawn_once_its_kind_is_known(
        text: &str,
        expected: &str,
        counts: (usize, usize),
    ) {
        let file = only_file(text, false);
        assert_eq!(file.patch, expected, "{WAITS_MSG}");
        assert_eq!((file.additions, file.deletions), counts);
    }

    #[test_case("*** Update File: a.rs\n" ; "a_header")]
    #[test_case("*** Update File: a.rs\n@@ fn ma" ; "a_hunk_header")]
    #[test_case("*** Begin Patch\n*** Upd" ; "a_marker")]
    #[test_case("*** Delete File: gone.rs\n" ; "a_deletion")]
    fn a_section_with_nothing_drawn_yet_publishes_nothing(text: &str) {
        assert!(published(text, false).is_empty(), "{WAITS_MSG}");
    }

    #[test]
    fn an_added_file_is_all_additions() {
        let file = only_file(
            "*** Add File: new.rs\n+fn a() {}\n+fn b() {}\n*** End Patch\n",
            true,
        );
        assert_eq!(file.patch, "@@\n+fn a() {}\n+fn b() {}\n");
        assert_eq!((file.additions, file.deletions), (2, 0));
    }

    #[test]
    fn a_deletion_publishes_nothing_and_closes_the_section_before_it() {
        let text = format!("{ONE_UPDATE}*** Delete File: gone.rs\n*** End Patch\n");
        assert_eq!(paths(&last(&text, true)), ["a.rs"], "{DELETE_MSG}");
        assert!(
            published("*** Delete File: gone.rs\n*** End Patch\n", true).is_empty(),
            "{DELETE_MSG}"
        );
    }

    /// The move line arrives between the header and the first chunk, and must
    /// neither close the section nor be drawn in it.
    #[test]
    fn a_move_is_named_by_its_destination() {
        let file = only_file(
            "*** Update File: old.rs\n*** Move to: new.rs\n@@\n-a\n+b\n",
            true,
        );
        assert_eq!(file.path, "new.rs");
        assert_eq!(file.patch, "@@\n-a\n+b\n");
    }

    #[test_case(false ; "while_open")]
    #[test_case(true ; "once_closed")]
    fn an_end_of_file_anchor_neither_closes_nor_draws(close: bool) {
        let text = "*** Update File: a.rs\n@@\n-a\n+b\n*** End of File\n";
        assert_eq!(only_file(text, close).patch, "@@\n-a\n+b\n");
    }

    /// A patch can quote the envelope it lives in, so only a line that starts
    /// with the marker opens a section.
    #[test]
    fn a_quoted_marker_opens_nothing() {
        let file = only_file(
            "*** Update File: a.rs\n@@\n+*** Update File: decoy.rs\n",
            true,
        );
        assert_eq!(file.path, "a.rs");
        assert_eq!(file.additions, 1);
    }

    #[test_case("-one\n+two\n", "@@\n-one\n+two\n" ; "an_unmarked_chunk_gains_a_hunk")]
    #[test_case("@@ fn main\n-one\n+two\n", "@@ fn main\n-one\n+two\n" ; "a_marked_chunk_is_kept_verbatim")]
    #[test_case("@@\n keep\n-one\n@@\n+two\n", "@@\n keep\n-one\n@@\n+two\n" ; "every_chunk_keeps_its_marker")]
    fn an_update_is_drawn_as_its_chunks(chunks: &str, expected: &str) {
        let file = only_file(&format!("*** Update File: a.rs\n{chunks}"), true);
        assert_eq!(file.patch, expected);
    }

    #[test]
    fn a_section_with_no_change_is_not_published() {
        assert!(published("*** Update File: a.rs\n@@\n*** End Patch\n", true).is_empty());
    }

    #[test]
    fn a_crlf_line_is_read_as_its_text() {
        assert_eq!(
            only_file("*** Update File: a.rs\r\n-one\r\n+two\r\n", true).patch,
            "@@\n-one\n+two\n"
        );
    }

    /// The counts are what the card's heading reports, so they cover the
    /// whole section even after the text stops being carried, and every file
    /// after the cut says it was cut too.
    #[test]
    fn a_section_past_the_cap_is_cut_short_but_counted_whole() {
        let line = format!("+{}\n", "x".repeat(63));
        let lines = LIVE_BODY_MAX_BYTES / line.len() + 8;
        let text = format!(
            "*** Add File: big.rs\n{}*** Add File: small.rs\n+tiny\n*** End Patch\n",
            line.repeat(lines)
        );
        let files = last(&text, true);
        let [big, small] = files.as_slice() else {
            panic!("{DRAWN_MSG}: {files:?}");
        };
        assert!(big.truncated);
        assert_eq!(big.additions, lines);
        assert!(big.patch.len() <= LIVE_BODY_MAX_BYTES);
        assert!(small.truncated);
        assert!(small.patch.is_empty());
        assert_eq!(small.additions, 1);
    }

    /// The file still being written shares the cap, and its count runs on
    /// past it, half-written line included.
    #[test]
    fn an_open_section_past_the_cap_is_cut_short_but_counted_whole() {
        let line = format!("+{}\n", "x".repeat(63));
        let lines = LIVE_BODY_MAX_BYTES / line.len() + 8;
        let text = format!("*** Add File: big.rs\n{}+half", line.repeat(lines));
        let big = only_file(&text, false);
        assert!(big.truncated);
        assert_eq!(big.additions, lines + 1);
        assert!(big.patch.len() <= LIVE_BODY_MAX_BYTES);
    }
}
