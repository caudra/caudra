//! The list a `todo_write` call is writing, read out of its arguments as they
//! arrive.
//!
//! A todo call spends its whole stream on one array, so until it runs the card
//! has nothing to show but a spinner, and then the whole plan lands at once.
//! This reader splits the array into items as the fragments go past: an item
//! joins the list once its `content` begins, its text grows token by token,
//! and it reads as pending until its own `status` has arrived whole.
//!
//! Only structure is decoded here. The open item's raw JSON is re-read from
//! the start on each fragment, which is affordable because an item is short
//! and the rescan stops at [`past_scan_cap`]. At the item's closing brace the
//! text is parsed the way the tool parses it, so an item that has closed is the
//! item the call will store.

use serde_json::Value;

use super::tool_preview::{candidates, literal_member, past_scan_cap, same_key};
use super::tool_roster::StringScan;
use crate::tools::TODOWRITE_TOOL_NAME;
use crate::tools::native::todo_write::{parse_item, parse_priority, parse_status};
use crate::types::{TodoItem, TodoStatus};
use caudra_providers::MAX_TOOL_INPUT_BYTES;

/// The argument holding the list.
const TODOS_KEY: &str = "todos";
const CONTENT_KEY: &str = "content";
const STATUS_KEY: &str = "status";
const PRIORITY_KEY: &str = "priority";
const OBJECT_OPEN: char = '{';
/// Every item is cloned into each publication, so a runaway list stops growing
/// on screen here. The finished call still draws all of it.
pub(super) const MAX_LIVE_TODOS: usize = 128;

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
    /// Between elements of `todos`.
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

/// The element being written.
struct OpenItem {
    /// Its raw JSON so far, `None` once it outgrew any tool input.
    text: Option<String>,
    /// Whether it already holds the last place in the list.
    drafted: bool,
}

/// The list of one `todo_write` call as its argument fragments arrive.
#[derive(Default)]
pub(super) struct TodoStream {
    state: State,
    string: StringScan,
    /// The member name being read, against [`TODOS_KEY`].
    member: String,
    items: Vec<TodoItem>,
    open: Option<OpenItem>,
    /// An item was added, or one changed, since the last publication.
    changed: bool,
}

impl TodoStream {
    /// `None` for every tool that is not `todo_write`. Names are matched the
    /// way [`super::tool_preview`] matches them, so `mcp_Todo_write` resolves
    /// too.
    pub(super) fn new(tool: &str) -> Option<Self> {
        candidates(tool)
            .any(|rest| same_key(TODOWRITE_TOOL_NAME, rest))
            .then(Self::default)
    }

    /// The whole list so far, `None` when this fragment changed no item.
    pub(super) fn absorb(&mut self, delta: &str) -> Option<Vec<TodoItem>> {
        for c in delta.chars() {
            self.push(c);
        }
        self.refresh();
        std::mem::take(&mut self.changed).then(|| self.items.clone())
    }

    /// Re-reads the element still being written. Every earlier item has
    /// closed, so this is the only one a fragment can have changed.
    fn refresh(&mut self) {
        let Some(text) = self.open_text().filter(|text| !past_scan_cap(text.len())) else {
            return;
        };
        let item = scanned(text);
        self.draft(item);
    }

    /// The element's own closing brace: a last, exact read of a text that is
    /// now whole, then the buffer goes.
    fn close(&mut self) {
        let item = self
            .open_text()
            .and_then(|text| parsed(text).or_else(|| scanned(text)));
        self.draft(item);
        self.open = None;
    }

    fn open_text(&self) -> Option<&str> {
        self.open.as_ref()?.text.as_deref()
    }

    /// Puts `item` in the open element's place, which it takes the first time
    /// the element names one.
    fn draft(&mut self, item: Option<TodoItem>) {
        let (Some(item), Some(open)) = (item, self.open.as_mut()) else {
            return;
        };
        match self.items.last_mut().filter(|_| open.drafted) {
            Some(last) if *last == item => return,
            Some(last) => *last = item,
            None => {
                open.drafted = true;
                self.items.push(item);
            }
        }
        self.changed = true;
    }

    fn start_element(&mut self) {
        self.open = (self.items.len() < MAX_LIVE_TODOS).then(|| OpenItem {
            text: Some(String::from(OBJECT_OPEN)),
            drafted: false,
        });
    }

    fn accumulate(&mut self, c: char) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        match open.text.as_mut() {
            Some(text) if text.len().saturating_add(c.len_utf8()) <= MAX_TOOL_INPUT_BYTES => {
                text.push(c);
            }
            _ => open.text = None,
        }
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
                    '[' if same_key(TODOS_KEY, &self.member) => State::Array,
                    '"' => State::SkipString(0),
                    '{' | '[' => State::Skip(1),
                    _ => State::Skip(0),
                }
            }
            State::Array => match c {
                '{' => {
                    self.start_element();
                    State::Element(0)
                }
                ']' => State::Done,
                c if c.is_whitespace() => State::Array,
                _ => State::Done,
            },
            State::ArraySeparator => match c {
                ',' => State::Array,
                ']' => State::Done,
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

/// A whole element, read exactly as the tool reads it.
fn parsed(text: &str) -> Option<TodoItem> {
    let value: Value = serde_json::from_str(text).ok()?;
    parse_item(&value).ok()
}

/// What a prefix of an element says so far. `None` until its `content` has
/// begun. A `status` or `priority` counts only once its closing quote arrives,
/// since half an enum names nothing.
fn scanned(text: &str) -> Option<TodoItem> {
    let (content, complete) = literal_member(text, CONTENT_KEY)?;
    if content.is_empty() && !complete {
        return None;
    }
    let finished = |key| {
        literal_member(text, key)
            .filter(|(_, complete)| *complete)
            .map(|(value, _)| value)
    };
    Some(TodoItem {
        content,
        status: finished(STATUS_KEY)
            .as_deref()
            .and_then(parse_status)
            .unwrap_or(TodoStatus::Pending),
        priority: finished(PRIORITY_KEY)
            .as_deref()
            .and_then(parse_priority)
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::{MAX_LIVE_TODOS, TodoStream};
    use crate::agent::tool_preview::PREVIEW_SCAN_CAP;
    use crate::types::{TodoItem, TodoPriority, TodoStatus};
    use serde_json::json;
    use test_case::test_case;

    const TODO_WRITE: &str = "todo_write";
    const FIRST: &str = "Read the parser";
    const SECOND: &str = "Write the reader";
    const SPLIT_MSG: &str = "any fragmentation must end on the list the tool itself parses";

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.to_owned(),
            status,
            priority: TodoPriority::default(),
        }
    }

    fn two_items() -> String {
        json!({ "todos": [
            { "content": FIRST, "status": "completed", "priority": "high" },
            { "content": SECOND, "status": "in_progress" },
        ]})
        .to_string()
    }

    fn expected_two() -> Vec<TodoItem> {
        vec![
            TodoItem {
                priority: TodoPriority::High,
                ..item(FIRST, TodoStatus::Completed)
            },
            item(SECOND, TodoStatus::InProgress),
        ]
    }

    /// Every list published, in order.
    fn published(fragments: &[&str]) -> Vec<Vec<TodoItem>> {
        let mut stream = TodoStream::new(TODO_WRITE).unwrap();
        fragments
            .iter()
            .filter_map(|fragment| stream.absorb(fragment))
            .collect()
    }

    fn last(fragments: &[&str]) -> Vec<TodoItem> {
        published(fragments).pop().unwrap_or_default()
    }

    fn chars(json: &str) -> Vec<String> {
        json.chars().map(String::from).collect()
    }

    #[test_case(TODO_WRITE, true ; "the_bare_name")]
    #[test_case("mcp_Todo_write", true ; "a_server_qualified_name")]
    #[test_case("batch", false ; "another_tool")]
    fn only_todo_write_is_read(tool: &str, read: bool) {
        assert_eq!(TodoStream::new(tool).is_some(), read);
    }

    #[test]
    fn a_whole_input_in_one_fragment_reads_as_the_tool_parses_it() {
        assert_eq!(last(&[&two_items()]), expected_two(), "{SPLIT_MSG}");
    }

    #[test]
    fn one_character_at_a_time_ends_on_the_same_list() {
        let fragments = chars(&two_items());
        let fragments: Vec<&str> = fragments.iter().map(String::as_str).collect();
        assert_eq!(last(&fragments), expected_two(), "{SPLIT_MSG}");
    }

    #[test]
    fn every_split_point_ends_on_the_same_list() {
        let json = two_items();
        for (at, _) in json.char_indices().skip(1) {
            let (head, tail) = json.split_at(at);
            assert_eq!(
                last(&[head, tail]),
                expected_two(),
                "split at {at}: {SPLIT_MSG}"
            );
        }
    }

    #[test]
    fn content_grows_while_its_item_is_open() {
        let lists = published(&[r#"{"todos":[{"content":"Rea"#, "d the", r#" parser""#]);
        let contents: Vec<&str> = lists.iter().map(|list| list[0].content.as_str()).collect();
        assert_eq!(contents, ["Rea", "Read the", FIRST]);
    }

    #[test_case(r#"{"todos":[{"content":"x","status":"in_pro"#, TodoStatus::Pending ; "half_a_status_names_nothing")]
    #[test_case(r#"{"todos":[{"content":"x","status":"in_progress""#, TodoStatus::InProgress ; "a_closed_status_counts")]
    #[test_case(r#"{"todos":[{"status":"completed","content":"x"#, TodoStatus::Completed ; "a_status_written_first_counts")]
    #[test_case(r#"{"todos":[{"content":"x","status":"blocked"}"#, TodoStatus::Pending ; "an_unknown_status_reads_as_pending")]
    fn the_status_reads_as_pending_until_it_arrives(json: &str, status: TodoStatus) {
        assert_eq!(last(&[json]), vec![item("x", status)]);
    }

    #[test]
    fn an_item_without_content_is_held_back() {
        assert!(published(&[r#"{"todos":[{"status":"pending","content":"#]).is_empty());
    }

    #[test]
    fn a_fragment_that_changes_nothing_publishes_nothing() {
        let mut stream = TodoStream::new(TODO_WRITE).unwrap();
        assert!(stream.absorb(r#"{"todos":[{"content":"x""#).is_some());
        assert_eq!(stream.absorb(r#","status":"pend"#), None);
    }

    #[test]
    fn a_unicode_escape_split_across_fragments_decodes() {
        let lists = published(&[r#"{"todos":[{"content":"caf\u00"#, r#"e9""#]);
        assert_eq!(lists.last().unwrap()[0].content, "café");
    }

    #[test]
    fn other_members_before_the_list_are_skipped() {
        let json = format!(
            r#"{{"note":"[{{\"content\":\"no\"}}]","n":{{"a":[1]}},"todos":[{{"content":"{FIRST}","status":"pending"}}]}}"#
        );
        assert_eq!(last(&[&json]), vec![item(FIRST, TodoStatus::Pending)]);
    }

    #[test]
    fn an_oversized_item_stops_rescanning_but_closes_exactly() {
        let content = "x".repeat(PREVIEW_SCAN_CAP * 2);
        let json = json!({ "todos": [{ "content": content, "status": "completed" }] }).to_string();
        let (head, tail) = json.split_at(PREVIEW_SCAN_CAP);
        let (middle, tail) = tail.split_at(PREVIEW_SCAN_CAP);
        let lists = published(&[head, middle, tail]);
        assert_eq!(lists.len(), 2, "the middle fragment is past the rescan cap");
        assert_eq!(lists[1], vec![item(&content, TodoStatus::Completed)]);
    }

    #[test]
    fn the_list_stops_growing_at_its_cap() {
        let items: Vec<_> = (0..=MAX_LIVE_TODOS)
            .map(|index| json!({ "content": index.to_string(), "status": "pending" }))
            .collect();
        let json = json!({ "todos": items }).to_string();
        assert_eq!(last(&[&json]).len(), MAX_LIVE_TODOS);
    }
}
