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

use super::tool_delegation::{Delegated, DelegationStream};
use super::tool_preview::{
    candidates, literal_member, object_member, past_scan_cap, preview_for, same_key, script_arg,
    string_member,
};
use crate::tools::native::batch::MAX_BATCH_SIZE;
use crate::tools::{BATCH_TOOL_NAME, ToolEffect};
use crate::types::{BatchToolEntry, BatchToolStatus, ToolInput};
use caudra_providers::MAX_TOOL_INPUT_BYTES;

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
    ArraySeparator,
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
    /// The same JSON, kept whole until the element closes so the call can be
    /// dispatched from it. `None` unless the reader was built to dispatch,
    /// since every other reader settles long before the element ends.
    raw: Option<String>,
    /// `None` until the name's closing quote arrives: a half-written name
    /// resolves to no tool, and the row it would draw is worse than no row.
    tool: Option<String>,
    summary: String,
    /// The whole script of a child called with one, uncapped, kept once the
    /// row settles so the body it drew while streaming is the body it keeps.
    script: Option<String>,
    settled: bool,
    /// Present only for a child that delegates, from the moment it names
    /// itself. A prompt outgrows anything worth rescanning, so it is read one
    /// character at a time instead.
    delegation: Option<DelegationStream>,
    /// What the delegation revealed since the last publication.
    revealed: Delegated,
}

impl Child {
    fn opened(dispatching: bool) -> Self {
        Self {
            text: String::from(OBJECT_OPEN),
            raw: dispatching.then(|| String::from(OBJECT_OPEN)),
            ..Self::default()
        }
    }

    fn absorb(&mut self, c: char) {
        if !self.settled {
            self.text.push(c);
        }
        if let Some(raw) = self.raw.as_mut() {
            if raw.len().saturating_add(c.len_utf8()) <= MAX_TOOL_INPUT_BYTES {
                raw.push(c);
            } else {
                self.raw = None;
            }
        }
        if let Some(stream) = self.delegation.as_mut() {
            stream.push(c, &mut self.revealed);
        }
    }

    /// Hands the element read so far to a delegation reader and stops
    /// buffering it: from here the child is read character by character, which
    /// is the only affordable way to carry a prompt of any length.
    fn delegate(&mut self, tool: &str) {
        let Some(mut stream) = DelegationStream::new(tool) else {
            return;
        };
        for c in self.text.chars() {
            stream.push(c, &mut self.revealed);
        }
        self.delegation = Some(stream);
        self.settle();
    }

    /// What this child's delegation revealed, `None` when it revealed nothing.
    /// The row's own header follows the description, since the rescan that
    /// used to keep it current stopped at the hand-off.
    fn take_revealed(&mut self) -> Option<Delegated> {
        if self.revealed.is_empty() {
            return None;
        }
        let revealed = std::mem::take(&mut self.revealed);
        if let Some(name) = &revealed.name {
            self.summary.clone_from(name);
        }
        Some(revealed)
    }

    fn settle(&mut self) {
        self.settled = true;
        self.text = String::new();
    }

    /// The roster row this child draws, `None` while it has no name to draw
    /// it under.
    fn entry(&self) -> Option<BatchToolEntry> {
        let tool = self.tool.clone()?;
        // The same value the call itself stamps at dispatch -- Workcell's
        // `input_start_input`, or a native tool's own `start_input` -- so the
        // script the reader is watching does not change the moment it starts.
        let input = self.script.as_ref().and_then(|code| {
            script_arg(&tool).map(|arg| ToolInput::Code {
                language: arg.language.to_owned(),
                code: code.clone(),
            })
        });
        Some(BatchToolEntry {
            model_suffix: None,
            tool,
            effect: ToolEffect::Unknown,
            summary: self.summary.clone(),
            status: BatchToolStatus::Pending,
            input,
            raw_input: None,
            output: None,
            annotation: None,
        })
    }
}

/// What one fragment of a `batch` call left behind.
#[derive(Default)]
pub(super) struct Rostered {
    /// The roster, `None` when no row changed.
    pub(super) entries: Option<Vec<BatchToolEntry>>,
    /// What each delegating child revealed, by its index in the list.
    pub(super) delegated: Vec<(usize, Delegated)>,
    /// The elements that closed in this fragment, whole, by index. Empty
    /// unless the reader was built to dispatch.
    pub(super) ready: Vec<(usize, String)>,
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
    /// Whether an element is kept whole so the call can be dispatched before
    /// the message it belongs to is finished.
    dispatching: bool,
    /// The elements that closed since the last publication.
    ready: Vec<(usize, String)>,
}

impl RosterStream {
    /// `None` for every tool that is not `batch`. Names are matched the way
    /// [`super::tool_preview`] matches them, so `mcp_Batch` resolves too.
    pub(super) fn new(tool: &str, dispatching: bool) -> Option<Self> {
        candidates(tool)
            .any(|rest| same_key(BATCH_TOOL_NAME, rest))
            .then(|| Self {
                dispatching,
                ..Self::default()
            })
    }

    /// What this fragment left behind: the roster, when it changed a row, and
    /// whatever any delegating child revealed. The list stops at the first
    /// child still waiting for its name, so an index always means the child it
    /// looks like.
    pub(super) fn absorb(&mut self, delta: &str) -> Rostered {
        for c in delta.chars() {
            self.push(c);
        }
        self.refresh();
        let delegated: Vec<(usize, Delegated)> = self
            .children
            .iter_mut()
            .enumerate()
            .filter_map(|(index, child)| Some((index, child.take_revealed()?)))
            .collect();
        self.changed |= delegated.iter().any(|(_, d)| d.name.is_some());
        Rostered {
            entries: std::mem::take(&mut self.changed).then(|| self.entries()),
            delegated,
            ready: std::mem::take(&mut self.ready),
        }
    }

    /// Every named child as a row. The element still open is being written,
    /// which is the one thing about it a queued row would misstate.
    fn entries(&self) -> Vec<BatchToolEntry> {
        let mut entries: Vec<BatchToolEntry> =
            self.children.iter().map_while(Child::entry).collect();
        if self.tracked
            && entries.len() == self.children.len()
            && let Some(open) = entries.last_mut()
        {
            open.status = BatchToolStatus::Drafting;
        }
        entries
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
        let (script, summary, complete) = {
            let params = object_member(&child.text, PARAMETERS_KEY).unwrap_or(&child.text);
            let arg = script_arg(&tool);
            let carried = arg.and_then(|arg| literal_member(params, arg.key));
            let preview = preview_for(&tool, params);
            let names_row = arg.is_some_and(|arg| arg.names_row);
            // A command's row is its own first line, which is what the settled
            // header reports too, so neither the row nor the body under it
            // moves when the call is dispatched. The tidied preview would cut
            // both to a header's width and fold their newlines. A prompt names
            // no row, so its child is headed by the preview like any other.
            let summary = match carried.as_ref().filter(|_| names_row) {
                Some((code, _)) => code.lines().next().unwrap_or_default().to_owned(),
                None => preview.as_ref().map(|p| p.text.clone()).unwrap_or_default(),
            };
            let script_done = carried.as_ref().is_some_and(|(_, done)| *done);
            let preview_done = preview.is_some_and(|p| p.complete);
            // A script's own closing quote settles the row for a tool that has
            // no preview key to settle it, which is every script tool whose
            // argument reads as a blob. One that names no row needs both: the
            // preview naming it, and the script it is still being handed.
            let complete = match arg {
                None => preview_done,
                Some(arg) if arg.names_row => script_done,
                Some(_) => script_done && preview_done,
            };
            (carried.map(|(code, _)| code), summary, complete)
        };
        let changed = child.tool.as_deref() != Some(tool.as_str())
            || child.summary != summary
            || child.script != script;
        child.tool = Some(tool.clone());
        child.summary = summary;
        child.script = script;
        self.changed |= changed;
        // A delegating child owns the rest of its element: it stops being
        // rescanned and starts being decoded, which is what carries a prompt
        // no preview would ever hold.
        child.delegate(&tool);
        if child.settled {
            return;
        }
        if complete || past_scan_cap(child.text.len()) {
            child.settle();
        }
    }

    fn open(&mut self) {
        self.tracked = self.children.len() < MAX_BATCH_SIZE;
        if self.tracked {
            self.children.push(Child::opened(self.dispatching));
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
    /// whole, then the buffer goes. The element itself is handed over here,
    /// which is the earliest moment the call it describes can be run.
    fn close(&mut self) {
        self.refresh();
        if self.tracked
            && let Some((index, child)) = self.children.iter_mut().enumerate().next_back()
        {
            child.settle();
            // A named row stops drafting at this brace, which may be all that
            // changed about it.
            self.changed |= child.tool.is_some();
            if let Some(raw) = child.raw.take() {
                self.ready.push((index, raw));
            }
        }
        self.tracked = false;
    }

    fn push(&mut self, c: char) {
        self.state = match self.state {
            State::Done => State::Done,
            State::Open => match c {
                '{' => State::Member,
                c if c.is_whitespace() => State::Open,
                _ => State::Done,
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
                c if c.is_whitespace() => State::Array,
                _ => State::Done,
            },
            State::ArraySeparator => match c {
                ',' => State::Array,
                c if c.is_whitespace() => State::ArraySeparator,
                _ => State::Done,
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
                        State::ArraySeparator
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
    use crate::types::{BatchToolEntry, BatchToolStatus, ToolInput};
    use test_case::test_case;

    const BATCH: &str = "batch";
    const READ: &str = "file_read";
    const GREP: &str = "file_grep";
    const SHELL: &str = "shell";
    const WRITE: &str = "file_write";
    const TASK: &str = "task";
    /// Long enough that the element outgrows what is worth re-reading.
    const OVERSIZED_BODY: usize = 16 * 1024;

    /// Every brief the fragments revealed, folded per child index.
    fn delegated(fragments: &[&str]) -> Vec<(usize, String, String)> {
        let mut stream = RosterStream::new(BATCH, false).unwrap();
        let mut folded: Vec<(usize, String, String)> = Vec::new();
        for fragment in fragments {
            for (index, revealed) in stream.absorb(fragment).delegated {
                let slot = match folded.iter_mut().find(|(at, ..)| *at == index) {
                    Some(slot) => slot,
                    None => {
                        folded.push((index, String::new(), String::new()));
                        folded.last_mut().expect("just pushed")
                    }
                };
                if let Some(name) = revealed.name {
                    slot.1 = name;
                }
                slot.2
                    .push_str(revealed.prompt.as_deref().unwrap_or_default());
            }
        }
        folded
    }

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
        let mut stream = RosterStream::new(BATCH, false).unwrap();
        fragments
            .iter()
            .filter_map(|fragment| stream.absorb(fragment).entries)
            .map(rows)
            .collect()
    }

    /// The last roster the fragments produced, whole, for the rows whose body
    /// matters as much as their header.
    fn entries(fragments: &[&str]) -> Vec<BatchToolEntry> {
        let mut stream = RosterStream::new(BATCH, false).unwrap();
        fragments
            .iter()
            .filter_map(|fragment| stream.absorb(fragment).entries)
            .last()
            .unwrap_or_default()
    }

    fn call(tool: &str, key: &str, value: &str) -> String {
        serde_json::json!({ "tool_calls": [{ "tool": tool, "parameters": { key: value } }] })
            .to_string()
    }

    fn script(language: &str, code: &str) -> Option<ToolInput> {
        Some(ToolInput::Code {
            language: language.to_owned(),
            code: code.to_owned(),
        })
    }

    const SCRIPT_MSG: &str = "a script tool's child carries its whole command, newlines and all, \
        so the body the reader watches while it streams is the body the call is dispatched with";
    const MULTILINE_COMMAND: &str = "set -e\ncargo build\ncargo test";
    const PYTHON: &str = "python_execution";
    const BASH_LANG: &str = "bash";
    const PYTHON_LANG: &str = "python";

    #[test]
    fn a_shell_childs_whole_command_streams_as_its_script() {
        let entries = entries(&[&call(SHELL, "command", MULTILINE_COMMAND)]);
        assert_eq!(
            entries[0].input,
            script(BASH_LANG, MULTILINE_COMMAND),
            "{SCRIPT_MSG}"
        );
    }

    #[test]
    fn a_python_childs_script_names_its_own_language() {
        const CODE: &str = "import sys\nprint(sys.version)";
        let entries = entries(&[&call(PYTHON, "code", CODE)]);
        assert_eq!(entries[0].input, script(PYTHON_LANG, CODE), "{SCRIPT_MSG}");
    }

    /// The row is the command's first line, which is what the settled header
    /// reports too, so dispatching the call moves neither the row nor the body.
    #[test]
    fn a_script_childs_row_is_its_commands_first_line() {
        let entries = entries(&[&call(SHELL, "command", MULTILINE_COMMAND)]);
        assert_eq!(entries[0].summary, "set -e", "{SCRIPT_MSG}");
    }

    /// The cap that keeps a header a header must not reach the body.
    #[test]
    fn a_long_command_is_cut_in_neither_the_row_nor_the_script() {
        let command = format!("git add -- {}", ["caudra/src/lib.rs"; 20].join(" "));
        let entries = entries(&[&call(SHELL, "command", &command)]);
        assert!(command.chars().count() > 160, "{SCRIPT_MSG}");
        assert_eq!(
            entries[0].input,
            script(BASH_LANG, &command),
            "{SCRIPT_MSG}"
        );
        assert_eq!(entries[0].summary, command, "{SCRIPT_MSG}");
    }

    #[test]
    fn a_child_called_with_arguments_rather_than_a_script_carries_none() {
        let entries = entries(&[&call(READ, "filePath", "a.rs")]);
        assert_eq!(entries[0].input, None, "{SCRIPT_MSG}");
        assert_eq!(entries[0].summary, "a.rs", "{SCRIPT_MSG}");
    }

    /// A settled child drops the text it was scanned from, and the script has
    /// to outlive it or every row but the last would lose its body.
    #[test]
    fn a_settled_childs_script_survives_the_text_it_came_from() {
        let entries = entries(&[
            r#"{"tool_calls": [{"tool": "shell", "parameters": {"command": "echo one"}}, "#,
            r#"{"tool": "file_read", "parameters": {"filePath": "b.r"#,
        ]);
        assert_eq!(
            entries[0].input,
            script(BASH_LANG, "echo one"),
            "{SCRIPT_MSG}"
        );
    }

    const IMAGE: &str = "image_generate";
    const IMAGE_LANG: &str = "markdown";
    const IMAGE_PROMPT: &str = "A wide cinematic shot\nof a lighthouse";
    const IMAGE_OUT: &str = "assets/hero.png";
    const PROMPT_ROW_MSG: &str = "a generating child draws its prompt as a body under a row that \
        still names the file it writes, because the prompt is the call and the path is its header";

    #[test]
    fn a_generating_childs_prompt_streams_under_a_row_naming_its_file() {
        let json = serde_json::json!({ "tool_calls": [{
            "tool": IMAGE,
            "parameters": { "prompt": IMAGE_PROMPT, "out": IMAGE_OUT },
        }] })
        .to_string();

        let entries = entries(&[&json]);

        assert_eq!(
            entries[0].input,
            script(IMAGE_LANG, IMAGE_PROMPT),
            "{PROMPT_ROW_MSG}"
        );
        assert_eq!(entries[0].summary, IMAGE_OUT, "{PROMPT_ROW_MSG}");
    }

    /// A child that names its row from something other than its script settles
    /// on both. Stopping at the path would leave the prompt frozen wherever
    /// the fragment carrying that path happened to end.
    #[test]
    fn a_generating_child_keeps_reading_its_prompt_past_the_path() {
        let entries = entries(&[
            concat!(
                r#"{"tool_calls": [{"tool": "image_generate", "parameters": "#,
                r#"{"out": "assets/hero.png", "prompt": "A wide cinematic shot"#
            ),
            r#"\nof a lighthouse"}}]}"#,
        ]);

        assert_eq!(
            entries[0].input,
            script(IMAGE_LANG, IMAGE_PROMPT),
            "{PROMPT_ROW_MSG}"
        );
        assert_eq!(entries[0].summary, IMAGE_OUT, "{PROMPT_ROW_MSG}");
    }

    #[test_case(BATCH, true ; "the_batch_tool_itself")]
    #[test_case("mcp_Batch", true ; "an_mcp_qualified_name")]
    #[test_case(SHELL, false ; "any_other_tool")]
    #[test_case("rebatch", false ; "a_name_merely_ending_in_batch")]
    fn only_a_batch_has_a_roster(tool: &str, expected: bool) {
        assert_eq!(RosterStream::new(tool, false).is_some(), expected);
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
        let mut stream = RosterStream::new(BATCH, false).unwrap();
        assert!(stream.absorb(r#"{"tool_calls": [{"too"#).entries.is_none());
        let named = stream
            .absorb(r#"l": "shell", "parameters": {"command": "ls"}}"#)
            .entries
            .expect("a named child is a row");
        assert_eq!(rows(named), [row(SHELL, "ls")]);
        assert!(
            stream.absorb(", ").entries.is_none(),
            "the comma between children changes no row"
        );
    }

    /// A row carries the pending shape every reader of a roster expects, so a
    /// streamed child is drawn by the same path a dispatched one is.
    #[test]
    fn a_streamed_child_is_a_pending_roster_row() {
        let mut stream = RosterStream::new(BATCH, false).unwrap();
        let entries = stream
            .absorb(r#"{"tool_calls": [{"tool": "shell", "parameters": {"command": "ls"}}"#)
            .entries
            .expect("a named child is a row");
        let [entry] = &entries[..] else {
            panic!("expected one child, got {}", entries.len());
        };
        assert_eq!(entry.status, BatchToolStatus::Pending);
        assert_eq!(entry.effect, ToolEffect::Unknown);
        assert!(entry.output.is_none() && entry.raw_input.is_none());
    }

    /// Every roster published, in order, as the status of each row.
    fn statuses(fragments: &[&str]) -> Vec<Vec<BatchToolStatus>> {
        let mut stream = RosterStream::new(BATCH, false).unwrap();
        fragments
            .iter()
            .filter_map(|fragment| stream.absorb(fragment).entries)
            .map(|entries| entries.into_iter().map(|entry| entry.status).collect())
            .collect()
    }

    /// Only the element still being written drafts, and the brace that ends
    /// it is a publication even when nothing else about the row moved.
    #[test]
    fn the_open_element_drafts_until_its_own_brace() {
        use BatchToolStatus::{Drafting, Pending};
        let published = statuses(&[
            r#"{"tool_calls": [{"tool": "file_read", "parameters": {"filePath": "a.rs""#,
            "}}",
            r#", {"tool": "file_grep", "parameters": {"pattern": "fn""#,
            "}}]}",
        ]);
        assert_eq!(
            published,
            [
                vec![Drafting],
                vec![Pending],
                vec![Pending, Drafting],
                vec![Pending, Pending],
            ]
        );
    }

    /// An element past the cap is never a row, so the open element it is does
    /// not make the last tracked child, long since closed, read as drafting.
    #[test]
    fn an_element_past_the_cap_drafts_no_row() {
        let mut json = String::from(r#"{"tool_calls": ["#);
        for index in 0..MAX_BATCH_SIZE {
            json.push_str(&format!(
                r#"{{"tool": "shell", "parameters": {{"command": "c{index}"}}}}, "#
            ));
        }
        json.push_str(r#"{"tool": "shell", "parameters": {"command": "over"#);
        let statuses: Vec<BatchToolStatus> =
            entries(&[&json]).iter().map(|entry| entry.status).collect();
        assert_eq!(statuses, [BatchToolStatus::Pending; MAX_BATCH_SIZE]);
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

    #[test]
    fn every_delegating_child_reveals_its_brief_under_its_own_index() {
        let json = concat!(
            r#"{"tool_calls": [{"tool": "task", "parameters": {"description": "first", "#,
            r#""prompt": "do one"}}, {"tool": "shell", "parameters": {"command": "ls"}}, "#,
            r#"{"tool": "task", "parameters": {"description": "second", "prompt": "do two"}}]}"#
        );
        assert_eq!(
            delegated(&[json]),
            [
                (0, "first".to_owned(), "do one".to_owned()),
                (2, "second".to_owned(), "do two".to_owned()),
            ]
        );
    }

    /// One fragment can close a child and open the next, and a brief must not
    /// spill from the chat it belongs to into its sibling's.
    #[test]
    fn a_fragment_spanning_two_children_splits_their_briefs() {
        let fragments = [
            r#"{"tool_calls": [{"tool": "task", "parameters": {"prompt": "do "#,
            r#"one"}}, {"tool": "task", "parameters": {"prompt": "do two"#,
        ];
        assert_eq!(
            delegated(&fragments),
            [
                (0, String::new(), "do one".to_owned()),
                (1, String::new(), "do two".to_owned()),
            ]
        );
    }

    #[test]
    fn a_child_that_does_not_delegate_reveals_nothing() {
        let json = r#"{"tool_calls": [{"tool": "shell", "parameters": {"command": "ls"}}]}"#;
        assert!(delegated(&[json]).is_empty());
    }

    /// A description settles the preview, but the prompt behind it is the
    /// whole point, so the child keeps being read past that closing quote.
    #[test]
    fn a_delegating_child_keeps_streaming_past_its_settled_preview() {
        let body = "x".repeat(OVERSIZED_BODY);
        let json = format!(
            r#"{{"tool_calls": [{{"tool": "task", "parameters": {{"description": "big", "prompt": "{body}"}}}}, {{"tool": "shell", "parameters": {{"command": "ls"#
        );
        assert_eq!(delegated(&[&json]), [(0, "big".to_owned(), body)]);
        assert_eq!(roster(&[&json]), [row(TASK, "big"), row(SHELL, "ls")]);
    }

    /// The rescan that used to keep the row current stops at the hand-off, so
    /// the header has to follow the description the delegation decodes.
    #[test]
    fn a_delegating_child_names_its_row_from_the_brief_it_streams() {
        let published = published(&[
            r#"{"tool_calls": [{"tool": "task", "parameters": {"description": "Rename "#,
            r#"the workflow"}}"#,
        ]);
        assert_eq!(
            published.last().map(Vec::as_slice),
            Some([row(TASK, "Rename the workflow")].as_slice())
        );
    }

    /// Every element the fragments closed, whole, by index. What a reader
    /// built to dispatch has to be handed, and in the fragment that closed it.
    fn ready(fragments: &[&str]) -> Vec<Vec<(usize, String)>> {
        let mut stream = RosterStream::new(BATCH, true).unwrap();
        fragments
            .iter()
            .map(|fragment| stream.absorb(fragment).ready)
            .collect()
    }

    #[test]
    fn a_closed_element_is_handed_over_whole_and_only_once() {
        let ready = ready(&[
            r#"{"tool_calls": [{"tool": "shell", "parameters": {"command": "ls"#,
            r#""}}, {"tool": "file_read", "parameters": {"filePath": "a.rs"}}]}"#,
        ]);
        assert_eq!(
            ready,
            [
                vec![],
                vec![
                    (
                        0,
                        r#"{"tool": "shell", "parameters": {"command": "ls"}}"#.to_owned()
                    ),
                    (
                        1,
                        r#"{"tool": "file_read", "parameters": {"filePath": "a.rs"}}"#.to_owned()
                    ),
                ],
            ],
            "an element is handed over in the fragment that closed it, and never again"
        );
    }

    #[test_case("[", "]"; "nested_array")]
    #[test_case("[[", "]]"; "deeply_nested_array")]
    #[test_case("\"", "\""; "string_containing_json")]
    #[test_case("null,", ""; "null_before_object")]
    #[test_case("true,", ""; "boolean_before_object")]
    #[test_case("42,", ""; "number_before_object")]
    fn invalid_direct_elements_stop_admission_without_renumbering(prefix: &str, suffix: &str) {
        let child = r#"{"tool":"shell","parameters":{"command":"ls"}}"#;
        let inner = if prefix == "\"" {
            child.replace('"', "\\\"")
        } else {
            child.to_owned()
        };
        for valid_prefix in [false, true] {
            let mut stream = RosterStream::new(BATCH, true).unwrap();
            let first = if valid_prefix {
                format!("{child},")
            } else {
                String::new()
            };
            let input = format!(r#"{{"tool_calls":[{first}{prefix}{inner}{suffix},{child}]}}"#);
            let mut admitted = Vec::new();
            for c in input.chars() {
                admitted.extend(stream.absorb(&c.to_string()).ready);
            }
            assert_eq!(
                admitted,
                if valid_prefix {
                    vec![(0, child.to_owned())]
                } else {
                    vec![]
                }
            );
        }
    }

    #[test_case("[", "]"; "array_parent")]
    #[test_case("null,", ""; "primitive_parent")]
    fn non_object_parents_do_not_promote_inner_batches(prefix: &str, suffix: &str) {
        let input = format!(
            r#"{prefix}{{"tool_calls":[{{"tool":"shell","parameters":{{"command":"ls"}}}}]}}{suffix}"#
        );
        assert!(ready(&[&input]).concat().is_empty());
    }

    /// The buffer is the one thing a dispatching reader cannot economise on,
    /// and a delegating child is exactly the one that stops being rescanned.
    #[test]
    fn a_delegating_child_is_handed_over_with_its_whole_prompt() {
        let body = "x".repeat(OVERSIZED_BODY);
        let element = format!(r#"{{"tool": "task", "parameters": {{"prompt": "{body}"}}}}"#);
        let ready = ready(&[&format!(r#"{{"tool_calls": [{element}]}}"#)]);
        assert_eq!(ready, [vec![(0, element)]]);
    }

    /// The cap is the batch's to report, so nothing past it is ever started.
    #[test]
    fn children_past_the_cap_are_never_handed_over() {
        let mut json = String::from(r#"{"tool_calls": ["#);
        for index in 0..MAX_BATCH_SIZE + 5 {
            json.push_str(&format!(
                r#"{{"tool": "shell", "parameters": {{"command": "c{index}"}}}}, "#
            ));
        }
        let handed: Vec<usize> = ready(&[&json])
            .concat()
            .into_iter()
            .map(|(index, _)| index)
            .collect();
        assert_eq!(handed, (0..MAX_BATCH_SIZE).collect::<Vec<_>>());
    }

    /// A reader nobody dispatches from keeps its economy: the element buffer
    /// is the one cost worth avoiding when there is no call to start.
    #[test]
    fn a_reader_that_does_not_dispatch_hands_nothing_over() {
        let mut stream = RosterStream::new(BATCH, false).unwrap();
        let json = r#"{"tool_calls": [{"tool": "shell", "parameters": {"command": "ls"}}]}"#;
        assert!(stream.absorb(json).ready.is_empty());
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
