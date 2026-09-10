//! The instruction a delegating call spends its stream writing.
//!
//! A `task` call is nearly all prompt: a few words of description, then the
//! whole brief for a subagent that does not exist yet. Reading that value as
//! its fragments go past is what lets the task's own chat open while the
//! parent is still dictating it, instead of appearing whole once the call
//! runs.
//!
//! Three members are decoded and nothing else. `description` names the chat,
//! `prompt` is its first message, and `task_id` says the call continues a
//! subagent that already has one. The decode is resumable for the same reason
//! [`super::tool_body`] is: a prompt is the long part, so rescanning it once
//! per fragment is quadratic, and an escape can straddle two fragments.
//!
//! A `batch` child writes the same three members either flat beside its `tool`
//! or wrapped in a `parameters` object, so the reader descends into that one
//! key and reads both shapes with the same walk.

use super::tool_body::{LIVE_BODY_MAX_BYTES, Piece, StringReader};
use super::tool_preview::{candidates, same_key};
use crate::tools::TASK_TOOL_NAME;

const DESCRIPTION_KEY: &str = "description";
const PROMPT_KEY: &str = "prompt";
const TASK_ID_KEY: &str = "task_id";
/// The object a `batch` child may wrap its arguments in.
const PARAMETERS_KEY: &str = "parameters";
/// A description is a handful of words and an id is shorter still. Anything
/// past this is not a label, and buffering it would only shove the chat list
/// around.
const LABEL_MAX_CHARS: usize = 120;

/// What one fragment of a delegating call revealed. Each field is `Some` only
/// when it changed, so a fragment that moved nothing costs no repaint.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Delegated {
    /// The description so far, republished as it grows: a task's name filling
    /// in reads better than a row that stays blank until the value closes.
    pub(super) name: Option<String>,
    /// What this fragment added to the prompt, decoded.
    pub(super) prompt: Option<String>,
    /// Published once, at the id's closing quote: half an id names the wrong
    /// subagent, and no reader can tell that it is half.
    pub(super) task_id: Option<String>,
}

impl Delegated {
    pub(super) fn is_empty(&self) -> bool {
        self.name.is_none() && self.prompt.is_none() && self.task_id.is_none()
    }
}

/// The member being decoded. Everything else is skipped by structure alone.
#[derive(Clone, Copy)]
enum Field {
    Name,
    Prompt,
    TaskId,
}

impl Field {
    fn of(member: &str) -> Option<Self> {
        if same_key(DESCRIPTION_KEY, member) {
            Some(Self::Name)
        } else if same_key(PROMPT_KEY, member) {
            Some(Self::Prompt)
        } else if same_key(TASK_ID_KEY, member) {
            Some(Self::TaskId)
        } else {
            None
        }
    }
}

/// Where the reader is in the argument object.
#[derive(Clone, Copy)]
enum State {
    /// Before the opening brace.
    Open,
    /// Between members: a quote starts a key, a closing brace ends the object.
    Member,
    Key,
    /// A key has been read; waiting for its colon.
    Colon,
    /// Waiting for the first character of a value.
    Value,
    /// Decoding one of the three members worth decoding.
    Field(Field),
    /// Discarding a string, which is either an unwanted value or one nested
    /// inside one, so its quotes and braces cannot be mistaken for structure.
    SkipString(usize),
    /// Discarding a non-string value, counting the containers still open.
    Skip(usize),
    /// The object closed.
    Done,
}

/// The arguments of one `task` call as its fragments arrive, retaining only
/// what has not been handed to the caller yet.
pub(super) struct DelegationStream {
    state: State,
    string: StringReader,
    /// The member name being read, against the three keys above.
    member: String,
    name: String,
    task_id: String,
    /// Decoded prompt bytes handed out so far, against [`LIVE_BODY_MAX_BYTES`].
    emitted: usize,
}

impl DelegationStream {
    /// `None` for every tool that does not delegate. Names are matched the way
    /// [`super::tool_preview`] matches them, so `mcp_Task` resolves too.
    pub(super) fn new(tool: &str) -> Option<Self> {
        candidates(tool)
            .any(|rest| same_key(TASK_TOOL_NAME, rest))
            .then(|| Self {
                state: State::Open,
                string: StringReader::default(),
                member: String::new(),
                name: String::new(),
                task_id: String::new(),
                emitted: 0,
            })
    }

    /// What this fragment revealed, `None` when it revealed nothing.
    pub(super) fn absorb(&mut self, delta: &str) -> Option<Delegated> {
        let mut changed = Delegated::default();
        for c in delta.chars() {
            self.push(c, &mut changed);
        }
        (!changed.is_empty()).then_some(changed)
    }

    /// One character, folded into a caller-owned accumulator. A reader that
    /// already walks the stream character by character uses this instead of
    /// [`Self::absorb`], so carrying a prompt costs no allocation per token.
    pub(super) fn push(&mut self, c: char, changed: &mut Delegated) {
        self.state = match self.state {
            State::Done => State::Done,
            State::Open => match c {
                '{' => State::Member,
                _ => State::Open,
            },
            State::Member => match c {
                '"' => {
                    self.member.clear();
                    self.string = StringReader::default();
                    State::Key
                }
                '}' => State::Done,
                _ => State::Member,
            },
            State::Key => match self.string.push(c) {
                Piece::Char(c) => {
                    self.member.push(c);
                    State::Key
                }
                Piece::Pending => State::Key,
                Piece::End => State::Colon,
            },
            State::Colon => match c {
                ':' => State::Value,
                _ => State::Colon,
            },
            State::Value if c.is_whitespace() => State::Value,
            State::Value => {
                self.string = StringReader::default();
                match (c, Field::of(&self.member)) {
                    ('"', Some(field)) => State::Field(field),
                    ('"', None) => State::SkipString(0),
                    // The wrapper is not a value: its members are the
                    // arguments, so reading continues inside it and its
                    // closing brace ends the walk.
                    ('{', None) if same_key(PARAMETERS_KEY, &self.member) => State::Member,
                    ('{' | '[', _) => State::Skip(1),
                    _ => State::Skip(0),
                }
            }
            State::Field(field) => match (self.string.push(c), field) {
                (Piece::Char(c), Field::Name) => {
                    if self.name.chars().count() < LABEL_MAX_CHARS {
                        self.name.push(c);
                        changed.name = Some(self.name.clone());
                    }
                    State::Field(field)
                }
                (Piece::Char(c), Field::Prompt) => {
                    if self.emitted < LIVE_BODY_MAX_BYTES {
                        self.emitted += c.len_utf8();
                        changed.prompt.get_or_insert_default().push(c);
                    }
                    State::Field(field)
                }
                (Piece::Char(c), Field::TaskId) => {
                    if self.task_id.chars().count() < LABEL_MAX_CHARS {
                        self.task_id.push(c);
                    }
                    State::Field(field)
                }
                (Piece::Pending, _) => State::Field(field),
                (Piece::End, Field::TaskId) => {
                    if !self.task_id.is_empty() {
                        changed.task_id = Some(self.task_id.clone());
                    }
                    State::Skip(0)
                }
                (Piece::End, _) => State::Skip(0),
            },
            State::SkipString(depth) => match self.string.push(c) {
                Piece::End => State::Skip(depth),
                _ => State::SkipString(depth),
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
                    self.string = StringReader::default();
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
    use super::{Delegated, DelegationStream, LABEL_MAX_CHARS, LIVE_BODY_MAX_BYTES};
    use test_case::test_case;

    const TASK: &str = "task";
    const NAME: &str = "Find auth middleware";
    const PROMPT: &str = "Search the codebase.";
    const TASK_ID: &str = "toolu_01";

    /// Everything the fragments revealed, folded into one value the way a
    /// reader accumulating them would see it.
    fn absorbed(fragments: &[&str]) -> (Option<String>, String, Option<String>) {
        let mut stream = DelegationStream::new(TASK).expect("task delegates");
        let (mut name, mut prompt, mut task_id) = (None, String::new(), None);
        for fragment in fragments {
            let Some(delegated) = stream.absorb(fragment) else {
                continue;
            };
            name = delegated.name.or(name);
            prompt.push_str(delegated.prompt.as_deref().unwrap_or_default());
            task_id = delegated.task_id.or(task_id);
        }
        (name, prompt, task_id)
    }

    #[test_case(TASK, true ; "the_task_tool_itself")]
    #[test_case("mcp_Task", true ; "an_mcp_qualified_name")]
    #[test_case("shell", false ; "any_other_tool")]
    #[test_case("subtask", false ; "a_name_merely_ending_in_task")]
    fn only_a_delegating_call_is_read(tool: &str, expected: bool) {
        assert_eq!(DelegationStream::new(tool).is_some(), expected);
    }

    #[test]
    fn a_whole_call_yields_its_three_members() {
        let json =
            format!(r#"{{"description": "{NAME}", "prompt": "{PROMPT}", "task_id": "{TASK_ID}"}}"#);
        assert_eq!(
            absorbed(&[&json]),
            (
                Some(NAME.to_owned()),
                PROMPT.to_owned(),
                Some(TASK_ID.to_owned())
            )
        );
    }

    #[test]
    fn members_out_of_schema_order_still_resolve() {
        let json =
            format!(r#"{{"prompt": "{PROMPT}", "task_id": "{TASK_ID}", "description": "{NAME}"}}"#);
        assert_eq!(
            absorbed(&[&json]),
            (
                Some(NAME.to_owned()),
                PROMPT.to_owned(),
                Some(TASK_ID.to_owned())
            )
        );
    }

    #[test]
    fn a_prompt_split_across_fragments_arrives_in_order() {
        let mut stream = DelegationStream::new(TASK).expect("task delegates");
        let published: Vec<String> = [r#"{"prompt": "Sea"#, "rch the", " codebase."]
            .iter()
            .filter_map(|fragment| stream.absorb(fragment)?.prompt)
            .collect();
        assert_eq!(published, ["Sea", "rch the", " codebase."]);
    }

    #[test]
    fn a_name_grows_as_it_arrives() {
        let mut stream = DelegationStream::new(TASK).expect("task delegates");
        let published: Vec<String> = [r#"{"description": "Find au"#, "th\", "]
            .iter()
            .filter_map(|fragment| stream.absorb(fragment)?.name)
            .collect();
        assert_eq!(published, ["Find au", "Find auth"]);
    }

    /// A half-written id names the wrong subagent, and nothing downstream can
    /// tell that it is half.
    #[test]
    fn an_id_is_published_only_once_it_closes() {
        let mut stream = DelegationStream::new(TASK).expect("task delegates");
        assert!(
            stream
                .absorb(r#"{"task_id": "toolu_"#)
                .and_then(|d| d.task_id)
                .is_none()
        );
        assert_eq!(
            stream.absorb("01\"").and_then(|d| d.task_id),
            Some(TASK_ID.to_owned())
        );
    }

    #[test_case(&[r#"{"prompt": "say \"#, r#""hi\""}"#], "say \"hi\"" ; "an_escape")]
    #[test_case(&[r#"{"prompt": "a\u00"#, r#"e9b"}"#], "a\u{e9}b" ; "a_unicode_escape")]
    #[test_case(&[r#"{"prompt": "one\"#, r#"ntwo"}"#], "one\ntwo" ; "a_newline_escape")]
    fn an_escape_straddling_a_fragment_survives(fragments: &[&str], expected: &str) {
        assert_eq!(absorbed(fragments).1, expected);
    }

    #[test]
    fn a_description_closing_names_the_call_before_any_prompt_arrives() {
        let (name, prompt, _) = absorbed(&[&format!(r#"{{"description": "{NAME}", "prom"#)]);
        assert_eq!(name.as_deref(), Some(NAME));
        assert!(prompt.is_empty());
    }

    #[test]
    fn a_child_wrapping_its_arguments_reads_the_same_as_a_flat_one() {
        let nested = format!(
            r#"{{"tool": "task", "parameters": {{"description": "{NAME}", "prompt": "{PROMPT}"}}}}"#
        );
        let flat = format!(r#"{{"tool": "task", "description": "{NAME}", "prompt": "{PROMPT}"}}"#);
        assert_eq!(absorbed(&[&nested]), absorbed(&[&flat]));
        assert_eq!(
            absorbed(&[&nested]),
            (Some(NAME.to_owned()), PROMPT.to_owned(), None)
        );
    }

    #[test]
    fn an_unrelated_member_containing_the_keys_is_skipped_whole() {
        let json = format!(
            r#"{{"output_schema": {{"prompt": "decoy", "description": "decoy"}}, "description": "{NAME}"}}"#
        );
        assert_eq!(absorbed(&[&json]).0.as_deref(), Some(NAME));
    }

    #[test]
    fn a_prompt_past_the_cap_stops_publishing_without_losing_the_rest() {
        let oversized = "x".repeat(LIVE_BODY_MAX_BYTES + 512);
        let json = format!(
            r#"{{"prompt": "{oversized}", "description": "{NAME}", "task_id": "{TASK_ID}"}}"#
        );
        let (name, prompt, task_id) = absorbed(&[&json]);
        assert_eq!(prompt.len(), LIVE_BODY_MAX_BYTES);
        assert_eq!(name.as_deref(), Some(NAME));
        assert_eq!(task_id.as_deref(), Some(TASK_ID));
    }

    #[test]
    fn a_runaway_description_is_bounded() {
        let oversized = "n".repeat(LABEL_MAX_CHARS * 2);
        let json = format!(r#"{{"description": "{oversized}", "prompt": "{PROMPT}"}}"#);
        let (name, prompt, _) = absorbed(&[&json]);
        assert_eq!(name.map(|name| name.chars().count()), Some(LABEL_MAX_CHARS));
        assert_eq!(prompt, PROMPT);
    }

    #[test]
    fn a_fragment_revealing_nothing_publishes_nothing() {
        let mut stream = DelegationStream::new(TASK).expect("task delegates");
        assert_eq!(stream.absorb(r#"{"mode": "build""#), None);
        assert_eq!(stream.absorb(", "), None);
        assert_eq!(
            stream.absorb(r#""description": "a""#),
            Some(Delegated {
                name: Some("a".to_owned()),
                ..Delegated::default()
            })
        );
    }
}
