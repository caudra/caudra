//! The children of a `batch` call, read out of its arguments as they arrive.
//!
//! A batch spends its whole stream writing a list of other calls, so until it
//! runs there is nothing on screen but the word `Batching` and then, all at
//! once, the roster. This reader splits that list into elements as the
//! fragments go past and names each child the way the transcript would: the
//! tool it calls, then the one argument worth showing, from the same scanner
//! every other streaming row is previewed by.
//!
//! Only structure is decoded here. Each element's raw JSON is buffered until
//! its row can no longer change, and re-read from the start on each fragment,
//! which is affordable only because the buffer is dropped the moment the row
//! settles: at its headline value's closing quote, at the element's own
//! closing brace, or at [`past_scan_cap`], whichever comes first. A child
//! writing a whole file therefore costs its length once, in the walk that
//! finds where it ends.
//!
//! A model that writes `parameters` before `tool` leaves its child unnamed
//! until that object closes, because a name cannot be read past an unfinished
//! value. The roster stops at the first unnamed child rather than closing the
//! gap, so an index always means the child it looks like, and `ToolStart`
//! replaces the whole thing with the roster the batch actually dispatched.

use super::tool_preview::{
    candidates, object_member, past_scan_cap, preview_for, same_key, string_member,
};
use crate::tools::native::batch::MAX_BATCH_SIZE;
use crate::tools::{BATCH_TOOL_NAME, ToolEffect};
use crate::types::{BatchToolEntry, BatchToolStatus};

/// The argument holding the calls to run.
const TOOL_CALLS_KEY: &str = "tool_calls";
/// What one element names the tool it calls.
const TOOL_KEY: &str = "tool";
/// The nested object an element's arguments may be wrapped in. Absent in the
/// flat `{ tool, ...params }` shape, where the element is its own arguments.
const PARAMETERS_KEY: &str = "parameters";
const OBJECT_OPEN: char = '{';

/// Tracks where a string ends without decoding it: only the quote that closes
/// it and the backslash that hides one are structural.
#[derive(Clone, Copy, Default)]
struct StringScan {
    escaped: bool,
}

impl StringScan {
    fn ends(&mut self, c: char) -> bool {
        if self.escaped {
            self.escaped = false;
            return false;
        }
        match c {
            '\\' => {
                self.escaped = true;
                false
            }
            '"' => true,
            _ => false,
        }
    }
}

/// Where the reader is in the argument object.
#[derive(Clone, Copy, Default)]
enum State {
    /// Before the opening brace.
    #[default]
    Open,
    /// Between members: a quote starts a key, a closing brace ends the object.
    Member,
    Key,
    /// A key has been read; waiting for its colon.
    Colon,
    /// Waiting for the first character of a value.
    Value,
    /// Between elements of `tool_calls`.
    Array,
    /// Inside one element, counting the containers still open within it.
    Element(usize),
    /// Inside a string within the element, where a brace is not structure.
    ElementString(usize),
    /// Discarding a string, which is either an unwanted value or one nested
    /// inside one, so its quotes and braces cannot be mistaken for structure.
    SkipString(usize),
    /// Discarding a non-string value, counting the containers still open.
    Skip(usize),
    /// The list closed, or the object did.
    Done,
}

/// One requested call as the reader has it so far.
#[derive(Default)]
struct Child {
    /// The element's raw JSON, dropped once the row can no longer change.
    text: String,
    /// `None` until the name's closing quote arrives: a half-written name
    /// resolves to no tool, and the row it would draw is worse than no row.
    tool: Option<String>,
    summary: String,
    settled: bool,
}

impl Child {
    fn opened() -> Self {
        Self {
            text: String::from(OBJECT_OPEN),
            ..Self::default()
        }
    }

    fn absorb(&mut self, c: char) {
        if !self.settled {
            self.text.push(c);
        }
    }

    fn settle(&mut self) {
        self.settled = true;
        self.text = String::new();
    }

    /// The roster row this child draws, `None` while it has no name to draw
    /// it under.
    fn entry(&self) -> Option<BatchToolEntry> {
        Some(BatchToolEntry {
            tool: self.tool.clone()?,
            effect: ToolEffect::Unknown,
            summary: self.summary.clone(),
            status: BatchToolStatus::Pending,
            input: None,
            raw_input: None,
            output: None,
            annotation: None,
        })
    }
}

/// The roster of one `batch` call as its argument fragments arrive.
#[derive(Default)]
pub(super) struct RosterStream {
    state: State,
    string: StringScan,
    /// The member name being read, against [`TOOL_CALLS_KEY`].
    member: String,
    children: Vec<Child>,
    /// Whether the element being read is the last of [`Self::children`].
    /// Everything past the cap is parsed for its structure and nothing else:
    /// those are the entries `batch` refuses to run anyway.
    tracked: bool,
    /// A row was added, or one changed, since the last publication.
    changed: bool,
}

impl RosterStream {
    /// `None` for every tool that is not `batch`. Names are matched the way
    /// [`super::tool_preview`] matches them, so `mcp_Batch` resolves too.
    pub(super) fn new(tool: &str) -> Option<Self> {
        candidates(tool)
            .any(|rest| same_key(BATCH_TOOL_NAME, rest))
            .then(Self::default)
    }

    /// The roster this fragment left behind, `None` when it changed no row.
    /// The list stops at the first child still waiting for its name, so an
    /// index always means the child it looks like.
    pub(super) fn absorb(&mut self, delta: &str) -> Option<Vec<BatchToolEntry>> {
        for c in delta.chars() {
            self.push(c);
        }
        self.refresh();
        std::mem::take(&mut self.changed)
            .then(|| self.children.iter().map_while(Child::entry).collect())
    }

    /// Re-reads the element still being written. Every earlier child has
    /// settled, so this is the only row a fragment can have changed.
    fn refresh(&mut self) {
        let Some(child) = self.children.last_mut().filter(|child| !child.settled) else {
            return;
        };
        let Some((tool, named)) = string_member(&child.text, TOOL_KEY) else {
            child.settled = past_scan_cap(child.text.len());
            return;
        };
        if !named {
            return;
        }
        // The flat shape is its own arguments, and the `tool` member the
        // scanner meets first there is not a preview key for any tool.
        let preview = preview_for(
            &tool,
            object_member(&child.text, PARAMETERS_KEY).unwrap_or(&child.text),
        );
        let summary = preview.as_ref().map(|p| p.text.clone()).unwrap_or_default();
        let changed = child.tool.as_deref() != Some(tool.as_str()) || child.summary != summary;
        child.tool = Some(tool);
        child.summary = summary;
        if preview.is_some_and(|p| p.complete) || past_scan_cap(child.text.len()) {
            child.settle();
        }
        self.changed |= changed;
    }

    fn open(&mut self) {
        self.tracked = self.children.len() < MAX_BATCH_SIZE;
        if self.tracked {
            self.children.push(Child::opened());
        }
    }

    fn accumulate(&mut self, c: char) {
        if self.tracked
            && let Some(child) = self.children.last_mut()
        {
            child.absorb(c);
        }
    }

    /// The element's own closing brace: a last read of a text that is now
    /// whole, then the buffer goes.
    fn close(&mut self) {
        self.refresh();
        if self.tracked
            && let Some(child) = self.children.last_mut()
        {
            child.settle();
        }
        self.tracked = false;
    }

    fn push(&mut self, c: char) {
        self.state = match self.state {
            State::Done => State::Done,
            State::Open => match c {
                '{' => State::Member,
                _ => State::Open,
            },
            State::Member => match c {
                '"' => {
                    self.member.clear();
                    self.string = StringScan::default();
                    State::Key
                }
                '}' => State::Done,
                _ => State::Member,
            },
            State::Key => match self.string.ends(c) {
                true => State::Colon,
                false => {
                    self.member.push(c);
                    State::Key
                }
            },
            State::Colon => match c {
                ':' => State::Value,
                _ => State::Colon,
            },
            State::Value if c.is_whitespace() => State::Value,
            State::Value => {
                self.string = StringScan::default();
                match c {
                    '[' if same_key(TOOL_CALLS_KEY, &self.member) => State::Array,
                    '"' => State::SkipString(0),
                    '{' | '[' => State::Skip(1),
                    _ => State::Skip(0),
                }
            }
            State::Array => match c {
                '{' => {
                    self.open();
                    State::Element(0)
                }
                ']' => State::Done,
                _ => State::Array,
            },
            State::Element(depth) => {
                self.accumulate(c);
                match c {
                    '"' => {
                        self.string = StringScan::default();
                        State::ElementString(depth)
                    }
                    '{' | '[' => State::Element(depth + 1),
                    '}' | ']' if depth > 0 => State::Element(depth - 1),
                    '}' => {
                        self.close();
                        State::Array
                    }
                    _ => State::Element(depth),
                }
            }
            State::ElementString(depth) => {
                self.accumulate(c);
                match self.string.ends(c) {
                    true => State::Element(depth),
                    false => State::ElementString(depth),
                }
            }
            State::SkipString(depth) => match self.string.ends(c) {
                true => State::Skip(depth),
                false => State::SkipString(depth),
            },
            // Depth zero means the value is over: what follows is either the
            // comma before the next member or the brace closing the object.
            State::Skip(0) => match c {
                ',' => State::Member,
                '}' => State::Done,
                _ => State::Skip(0),
            },
            State::Skip(depth) => match c {
                '"' => {
                    self.string = StringScan::default();
                    State::SkipString(depth)
                }
                '{' | '[' => State::Skip(depth + 1),
                '}' | ']' => State::Skip(depth - 1),
                _ => State::Skip(depth),
            },
        };
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_BATCH_SIZE, RosterStream, ToolEffect};
    use crate::types::{BatchToolEntry, BatchToolStatus};
    use test_case::test_case;

    const BATCH: &str = "batch";
    const READ: &str = "file_read";
    const GREP: &str = "file_grep";
    const SHELL: &str = "shell";
    const WRITE: &str = "file_write";
    /// Long enough that the element outgrows what is worth re-reading.
    const OVERSIZED_BODY: usize = 16 * 1024;

    /// What a roster draws: one tool and one header per row.
    fn rows(entries: Vec<BatchToolEntry>) -> Vec<(String, String)> {
        entries
            .into_iter()
            .map(|entry| (entry.tool, entry.summary))
            .collect()
    }

    fn row(tool: &str, summary: &str) -> (String, String) {
        (tool.to_owned(), summary.to_owned())
    }

    /// The last roster the fragments produced.
    fn roster(fragments: &[&str]) -> Vec<(String, String)> {
        published(fragments).pop().unwrap_or_default()
    }

    /// Every roster published, in order, so the growth itself can be asserted.
    fn published(fragments: &[&str]) -> Vec<Vec<(String, String)>> {
        let mut stream = RosterStream::new(BATCH).unwrap();
        fragments
            .iter()
            .filter_map(|fragment| stream.absorb(fragment))
            .map(rows)
            .collect()
    }

    #[test_case(BATCH, true ; "the_batch_tool_itself")]
    #[test_case("mcp_Batch", true ; "an_mcp_qualified_name")]
    #[test_case(SHELL, false ; "any_other_tool")]
    #[test_case("rebatch", false ; "a_name_merely_ending_in_batch")]
    fn only_a_batch_has_a_roster(tool: &str, expected: bool) {
        assert_eq!(RosterStream::new(tool).is_some(), expected);
    }

    #[test]
    fn a_child_is_named_once_its_tool_name_closes() {
        let published = published(&[
            r#"{"tool_calls": [{"tool": "file_re"#,
            r#"ad", "parameters": {"#,
            r#""filePath": "a.rs"}}"#,
        ]);
        assert_eq!(
            published,
            [vec![row(READ, "")], vec![row(READ, "a.rs")]],
            "a half-written name draws no row, and the argument fills in under it"
        );
    }

    #[test]
    fn a_nested_parameters_object_is_previewed() {
        let roster =
            roster(&[r#"{"tool_calls": [{"tool": "file_read", "parameters": {"filePath": "src/a"#]);
        assert_eq!(roster, [row(READ, "src/a")]);
    }

    #[test]
    fn a_flat_entry_is_previewed() {
        let roster = roster(&[r#"{"tool_calls": [{"tool": "shell", "command": "ls -la"#]);
        assert_eq!(roster, [row(SHELL, "ls -la")]);
    }

    #[test]
    fn every_child_of_a_finished_list_is_named() {
        let roster = roster(&[
            r#"{"tool_calls": [{"tool": "file_read", "parameters": {"filePath": "a.rs"}},"#,
            r#" {"tool": "file_grep", "parameters": {"pattern": "fn main"}}]}"#,
        ]);
        assert_eq!(roster, [row(READ, "a.rs"), row(GREP, "fn main")]);
    }

    #[test]
    fn a_list_arriving_whole_is_one_publication() {
        let published = published(&[
            r#"{"tool_calls": [{"tool": "file_read", "parameters": {"filePath": "a.rs"}}, "#,
        ]);
        assert_eq!(published.len(), 1);
    }

    #[test]
    fn a_preview_split_across_fragments_grows() {
        let published = published(&[
            r#"{"tool_calls": [{"tool": "shell", "parameters": {"command": "ec"#,
            "ho",
            r#" hi"}}"#,
        ]);
        assert_eq!(
            published,
            [
                vec![row(SHELL, "ec")],
                vec![row(SHELL, "echo")],
                vec![row(SHELL, "echo hi")],
            ]
        );
    }

    #[test]
    fn an_escape_split_across_fragments_does_not_end_the_element() {
        let roster = roster(&[
            r#"{"tool_calls": [{"tool": "shell", "parameters": {"command": "say \"#,
            r#"" }hi{ \"""#,
            r#"}}, {"tool": "file_grep", "parameters": {"pattern": "p"#,
        ]);
        assert_eq!(roster, [row(SHELL, r#"say " }hi{ ""#), row(GREP, "p")]);
    }

    #[test_case(r#""a]b""# ; "a_bracket")]
    #[test_case(r#""a}b""# ; "a_brace")]
    #[test_case(r#""tool_calls""# ; "the_list_key_itself")]
    fn a_string_value_does_not_break_element_boundaries(pattern: &str) {
        let json = format!(
            r#"{{"tool_calls": [{{"tool": "file_grep", "parameters": {{"pattern": {pattern}}}}}, {{"tool": "shell", "parameters": {{"command": "ls"#
        );
        let roster = roster(&[&json]);
        assert_eq!(roster.len(), 2, "{roster:?}");
        assert_eq!(roster[1], row(SHELL, "ls"));
    }

    #[test]
    fn a_member_before_the_list_is_skipped_whole() {
        let roster = roster(&[
            r#"{"note": {"tool_calls": [{"tool": "shell", "parameters": {"command": "decoy"}}]},"#,
            r#" "tool_calls": [{"tool": "file_read", "parameters": {"filePath": "real.rs""#,
        ]);
        assert_eq!(roster, [row(READ, "real.rs")]);
    }

    #[test]
    fn a_fragment_that_changes_no_row_publishes_nothing() {
        let mut stream = RosterStream::new(BATCH).unwrap();
        assert!(stream.absorb(r#"{"tool_calls": [{"too"#).is_none());
        let named = stream
            .absorb(r#"l": "shell", "parameters": {"command": "ls"}}"#)
            .expect("a named child is a row");
        assert_eq!(rows(named), [row(SHELL, "ls")]);
        assert!(
            stream.absorb(", ").is_none(),
            "the comma between children changes no row"
        );
    }

    /// A row carries the pending shape every reader of a roster expects, so a
    /// streamed child is drawn by the same path a dispatched one is.
    #[test]
    fn a_streamed_child_is_a_pending_roster_row() {
        let mut stream = RosterStream::new(BATCH).unwrap();
        let entries = stream
            .absorb(r#"{"tool_calls": [{"tool": "shell", "parameters": {"command": "ls"}}"#)
            .expect("a named child is a row");
        let [entry] = &entries[..] else {
            panic!("expected one child, got {}", entries.len());
        };
        assert_eq!(entry.status, BatchToolStatus::Pending);
        assert_eq!(entry.effect, ToolEffect::Unknown);
        assert!(entry.output.is_none() && entry.raw_input.is_none());
    }

    /// The entries `batch` itself refuses to run, which it reports in its own
    /// answer. Tracking them here would only cost memory the cap exists to
    /// bound.
    #[test]
    fn children_past_the_cap_are_not_tracked() {
        let over = MAX_BATCH_SIZE + 5;
        let mut json = String::from(r#"{"tool_calls": ["#);
        for index in 0..over {
            json.push_str(&format!(
                r#"{{"tool": "shell", "parameters": {{"command": "c{index}"}}}}, "#
            ));
        }
        let roster = roster(&[&json]);
        assert_eq!(roster.len(), MAX_BATCH_SIZE);
        assert_eq!(
            roster[MAX_BATCH_SIZE - 1].1,
            format!("c{}", MAX_BATCH_SIZE - 1)
        );
    }

    /// The walk that finds where an element ends has to keep running past the
    /// point where re-reading it stops being worth anything.
    #[test]
    fn a_child_past_the_scan_cap_stops_being_buffered_without_losing_its_siblings() {
        let body = "x".repeat(OVERSIZED_BODY);
        let json = format!(
            r#"{{"tool_calls": [{{"tool": "file_write", "parameters": {{"filePath": "big.rs", "content": "{body}"}}}}, {{"tool": "shell", "parameters": {{"command": "ls"#
        );
        let roster = roster(&[&json]);
        assert_eq!(roster, [row(WRITE, "big.rs"), row(SHELL, "ls")]);
    }

    /// A name cannot be read past a value that has not finished, so the row
    /// waits rather than being drawn under the wrong tool.
    #[test]
    fn a_child_writing_its_parameters_first_is_unnamed_until_they_close() {
        let unfinished = r#"{"tool_calls": [{"parameters": {"filePath": "a.rs""#;
        assert!(roster(&[unfinished]).is_empty());
        assert_eq!(
            roster(&[unfinished, r#"}, "tool": "file_read"}"#]),
            [row(READ, "a.rs")]
        );
    }

    /// An index has to mean the child it looks like, so a roster is a prefix
    /// of the list rather than the children that happen to have resolved.
    #[test]
    fn the_roster_stops_at_the_first_unnamed_child() {
        let roster = roster(&[
            r#"{"tool_calls": [{"tool": "shell", "parameters": {"command": "ls"}}, "#,
            r#"{"parameters": {"pattern": "fn"#,
        ]);
        assert_eq!(roster, [row(SHELL, "ls")]);
    }
}
