//! The files a `file_apply_patch` call has finished describing, read out of
//! its patch text as it arrives.
//!
//! A half-written diff is worse than none, which is why a patch body is
//! counted rather than drawn while it streams. A section of the envelope is
//! different once the next one begins: it is the model's whole change to that
//! file. So each section is published as a diff the moment it closes, and the
//! card shows the files the call has finished writing while it writes the rest.
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

    /// Reads one line the way Workcell reads it for this verb. A line Workcell
    /// would refuse is left out rather than drawn, and the refusal arrives
    /// with the call's own output.
    fn absorb(&mut self, line: &str, spent: &mut usize) {
        if self.verb == Verb::Update && line.starts_with(HUNK) {
            self.hunked = true;
            self.keep(line, spent);
            return;
        }
        match (self.verb, line.chars().next()) {
            (Verb::Add | Verb::Update, Some(ADDED)) => self.additions += 1,
            (Verb::Update, Some(REMOVED)) => self.deletions += 1,
            (Verb::Update, Some(CONTEXT)) => {}
            _ => return,
        }
        self.changed = true;
        if !self.hunked {
            self.hunked = true;
            self.keep(HUNK, spent);
        }
        self.keep(line, spent);
    }

    /// Keeps `line` while the budget every section shares allows, and cuts the
    /// section short from the first line it does not. The counts keep going.
    fn keep(&mut self, line: &str, spent: &mut usize) {
        let cost = line.len() + 1;
        if self.truncated || spent.saturating_add(cost) > LIVE_BODY_MAX_BYTES {
            self.truncated = true;
            *spent = LIVE_BODY_MAX_BYTES;
            return;
        }
        *spent += cost;
        self.patch.push_str(line);
        self.patch.push('\n');
    }

    /// `None` for a section with nothing to draw: a deletion, whose content
    /// the patch never carries, or a section that wrote no change line.
    fn finished(self) -> Option<PatchedFile> {
        self.changed.then_some(PatchedFile {
            path: self.path,
            patch: self.patch,
            additions: self.additions,
            deletions: self.deletions,
            truncated: self.truncated,
        })
    }
}

/// The finished files of one patch as its decoded characters arrive.
#[derive(Default)]
pub(super) struct PatchStream {
    /// The line being decoded, bounded like the text kept from it.
    line: String,
    open: Option<Section>,
    files: Vec<PatchedFile>,
    /// Bytes of patch text kept across every section.
    spent: usize,
    /// A section closed since [`Self::files`] was last asked.
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

    /// Every file finished so far, when one more finished since the last call.
    pub(super) fn files(&mut self) -> Option<Vec<PatchedFile>> {
        mem::take(&mut self.unpublished).then(|| self.files.clone())
    }

    fn end_line(&mut self, line: &str) {
        if line.trim() == END_PATCH {
            self.close();
            self.ended = true;
            return;
        }
        let Some(rest) = line.strip_prefix(MARKER) else {
            if let Some(section) = self.open.as_mut() {
                section.absorb(line, &mut self.spent);
            }
            return;
        };
        // A move names the file by where it ends up, as the call's output does.
        if let Some(destination) = rest.strip_prefix(MOVE_TO) {
            if let Some(section) = self.open.as_mut().filter(|s| s.verb == Verb::Update) {
                section.path = destination.trim().to_owned();
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
        if let Some(file) = self.open.take().and_then(Section::finished) {
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
    const FINISHED_MSG: &str = "a section is published once, when it is whole";
    const UNFINISHED_MSG: &str = "a section still being written is not published";
    const DELETE_MSG: &str = "a deletion has no content to draw";

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

    fn paths(lists: &[Vec<PatchedFile>]) -> Vec<Vec<&str>> {
        lists
            .iter()
            .map(|files| files.iter().map(|file| file.path.as_str()).collect())
            .collect()
    }

    fn only_file(text: &str) -> PatchedFile {
        let mut lists = published(text, true);
        assert_eq!(lists.len(), 1, "{FINISHED_MSG}");
        let mut files = lists.remove(0);
        assert_eq!(files.len(), 1, "{FINISHED_MSG}");
        files.remove(0)
    }

    #[test]
    fn a_section_is_published_when_the_next_begins() {
        let text = format!("{ONE_UPDATE}*** Update File: b.rs\n-x\n+y\n");
        assert_eq!(
            paths(&published(&text, false)),
            [["a.rs"]],
            "{UNFINISHED_MSG}"
        );
    }

    #[test_case(&format!("{ONE_UPDATE}*** End Patch\n"), false ; "at_the_end_of_the_envelope")]
    #[test_case(&format!("{ONE_UPDATE}*** End Patch"), true ; "at_an_end_marker_the_string_closed_on")]
    #[test_case("*** Update File: a.rs\n-one\n+two", true ; "when_the_string_closes_without_one")]
    fn the_last_section_is_published(text: &str, close: bool) {
        assert_eq!(paths(&published(text, close)), [["a.rs"]], "{FINISHED_MSG}");
    }

    #[test]
    fn an_unfinished_last_section_is_not_published() {
        assert!(published(ONE_UPDATE, false).is_empty(), "{UNFINISHED_MSG}");
    }

    #[test]
    fn an_added_file_is_all_additions() {
        let file = only_file("*** Add File: new.rs\n+fn a() {}\n+fn b() {}\n*** End Patch\n");
        assert_eq!(file.patch, "@@\n+fn a() {}\n+fn b() {}\n");
        assert_eq!((file.additions, file.deletions), (2, 0));
    }

    #[test]
    fn a_deletion_publishes_nothing_and_closes_the_section_before_it() {
        let text = format!("{ONE_UPDATE}*** Delete File: gone.rs\n*** End Patch\n");
        assert_eq!(paths(&published(&text, true)), [["a.rs"]], "{DELETE_MSG}");
        assert!(
            published("*** Delete File: gone.rs\n*** End Patch\n", true).is_empty(),
            "{DELETE_MSG}"
        );
    }

    /// The move line arrives between the header and the first chunk, and must
    /// neither close the section nor be drawn in it.
    #[test]
    fn a_move_is_named_by_its_destination() {
        let file = only_file("*** Update File: old.rs\n*** Move to: new.rs\n@@\n-a\n+b\n");
        assert_eq!(file.path, "new.rs");
        assert_eq!(file.patch, "@@\n-a\n+b\n");
    }

    #[test]
    fn an_end_of_file_anchor_neither_closes_nor_draws() {
        let text = "*** Update File: a.rs\n@@\n-a\n+b\n*** End of File\n";
        assert!(published(text, false).is_empty(), "{UNFINISHED_MSG}");
        assert_eq!(only_file(text).patch, "@@\n-a\n+b\n");
    }

    /// A patch can quote the envelope it lives in, so only a line that starts
    /// with the marker opens a section.
    #[test]
    fn a_quoted_marker_opens_nothing() {
        let file = only_file("*** Update File: a.rs\n@@\n+*** Update File: decoy.rs\n");
        assert_eq!(file.path, "a.rs");
        assert_eq!(file.additions, 1);
    }

    #[test_case("-one\n+two\n", "@@\n-one\n+two\n" ; "an_unmarked_chunk_gains_a_hunk")]
    #[test_case("@@ fn main\n-one\n+two\n", "@@ fn main\n-one\n+two\n" ; "a_marked_chunk_is_kept_verbatim")]
    #[test_case("@@\n keep\n-one\n@@\n+two\n", "@@\n keep\n-one\n@@\n+two\n" ; "every_chunk_keeps_its_marker")]
    fn an_update_is_drawn_as_its_chunks(chunks: &str, expected: &str) {
        let file = only_file(&format!("*** Update File: a.rs\n{chunks}"));
        assert_eq!(file.patch, expected);
    }

    #[test]
    fn a_section_with_no_change_is_not_published() {
        assert!(published("*** Update File: a.rs\n@@\n*** End Patch\n", true).is_empty());
    }

    #[test]
    fn a_crlf_line_is_read_as_its_text() {
        assert_eq!(
            only_file("*** Update File: a.rs\r\n-one\r\n+two\r\n").patch,
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
        let files = published(&text, true).pop().unwrap();
        let [big, small] = files.as_slice() else {
            panic!("{FINISHED_MSG}: {files:?}");
        };
        assert!(big.truncated);
        assert_eq!(big.additions, lines);
        assert!(big.patch.len() <= LIVE_BODY_MAX_BYTES);
        assert!(small.truncated);
        assert!(small.patch.is_empty());
        assert_eq!(small.additions, 1);
    }
}
