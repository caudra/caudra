//! JSON an inspector shows, such as a workflow call's request and result or an
//! automation's event and state, as rows a reader can fold. Such a value is
//! often a deep object, and one long pretty-printed dump buries every row
//! under it, so each container is a row that can be closed.
//!
//! A node is named by where it sits in a walk of the whole value, so the name
//! does not move when an ancestor closes and the fold survives a redraw. The
//! walk therefore visits a closed node's children as well, to keep counting,
//! and simply draws nothing for them.

use std::collections::HashSet;

use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::components::json_text::scalar_spans;
use crate::theme;

const INDENT: &str = "  ";
const OPEN_MARK: &str = "\u{25be} ";
const CLOSED_MARK: &str = "\u{25b8} ";
const NO_MARK: &str = "  ";
const OBJECT_OPEN: &str = "{";
const OBJECT_CLOSE: &str = "}";
const ARRAY_OPEN: &str = "[";
const ARRAY_CLOSE: &str = "]";
const FOLDED_OBJECT: &str = "{\u{2026}}";
const FOLDED_ARRAY: &str = "[\u{2026}]";
const EMPTY_OBJECT: &str = "{}";
const EMPTY_ARRAY: &str = "[]";
const KEY_UNIT: &str = " key";
const ITEM_UNIT: &str = " item";
const PLURAL: &str = "s";
const COMMA: &str = ",";
const KEY_SEPARATOR: &str = ": ";

/// One line of a body, and the node it opens when it opens one. A row that
/// opens nothing is a scalar, a closing brace, or a node whose parent is shut.
pub(crate) struct JsonRow {
    pub(crate) fold: Option<usize>,
    pub(crate) line: Line<'static>,
}

/// The value as rows, with every container named in `folded` drawn closed.
/// Only objects and arrays are worth folding, so anything else is refused and
/// the caller keeps whatever it was showing before.
pub(crate) fn rows(value: &Value, folded: &HashSet<usize>) -> Option<Vec<JsonRow>> {
    Some(paint(drafts(value, folded)?))
}

/// The nodes a reader can reach, in the order they draw. Painting is what
/// costs, and a caller placing a cursor does not need it.
pub(crate) fn folds(value: &Value, folded: &HashSet<usize>) -> Vec<usize> {
    drafts(value, folded)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|draft| draft.fold)
        .collect()
}

fn drafts(value: &Value, folded: &HashSet<usize>) -> Option<Vec<Draft>> {
    if !value.is_object() && !value.is_array() {
        return None;
    }
    let mut drafts = Vec::new();
    let mut walk = Walk {
        next: 0,
        folded,
        drafts: &mut drafts,
    };
    walk.push(value, None, 0, true, true);
    Some(drafts)
}

/// A row before it is painted, in the pieces it is painted from. A key and a
/// string value are one token to a JSON grammar and one colour to every theme
/// that reads it, so the two are kept apart here rather than left to be told
/// apart afterwards.
struct Draft {
    fold: Option<usize>,
    depth: usize,
    key: Option<String>,
    value: String,
    /// The comma that separates this row from the sibling after it.
    tail: &'static str,
    /// A closed container says how much it is hiding, which is the only
    /// reason to open it again.
    tally: Option<String>,
}

struct Walk<'a> {
    next: usize,
    folded: &'a HashSet<usize>,
    drafts: &'a mut Vec<Draft>,
}

impl Walk<'_> {
    /// `visible` is false inside a closed node, where the walk still runs so
    /// the nodes after it keep the names they had when it was open.
    fn push(&mut self, value: &Value, key: Option<&str>, depth: usize, last: bool, visible: bool) {
        let head = key.map(quoted);
        let tail = match last {
            true => "",
            false => COMMA,
        };
        match value {
            Value::Object(map) => {
                let entries: Vec<(&str, &Value)> = map
                    .iter()
                    .map(|(key, value)| (key.as_str(), value))
                    .collect();
                self.container(&entries, Bracket::Object, head, tail, depth, visible, true);
            }
            Value::Array(items) => {
                let entries: Vec<(&str, &Value)> = items.iter().map(|value| ("", value)).collect();
                self.container(&entries, Bracket::Array, head, tail, depth, visible, false);
            }
            scalar => {
                if visible {
                    self.emit(depth, None, head, scalar.to_string(), tail, None);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn container(
        &mut self,
        entries: &[(&str, &Value)],
        bracket: Bracket,
        head: Option<String>,
        tail: &'static str,
        depth: usize,
        visible: bool,
        keyed: bool,
    ) {
        let node = self.next;
        self.next += 1;
        if entries.is_empty() {
            if visible {
                self.emit(depth, None, head, bracket.empty().to_owned(), tail, None);
            }
            return;
        }
        let closed = self.folded.contains(&node);
        if visible {
            // An open container ends in a brace of its own, which is the row
            // that carries the comma.
            let (value, tail) = match closed {
                true => (bracket.folded(), tail),
                false => (bracket.open(), ""),
            };
            let tally = closed.then(|| bracket.tally(entries.len()));
            self.emit(depth, Some(node), head, value.to_owned(), tail, tally);
        }
        let inside = visible && !closed;
        for (index, (key, value)) in entries.iter().enumerate() {
            let key = keyed.then_some(*key);
            self.push(value, key, depth + 1, index + 1 == entries.len(), inside);
        }
        if inside {
            self.emit(depth, None, None, bracket.close().to_owned(), tail, None);
        }
    }

    fn emit(
        &mut self,
        depth: usize,
        fold: Option<usize>,
        key: Option<String>,
        value: String,
        tail: &'static str,
        tally: Option<String>,
    ) {
        self.drafts.push(Draft {
            fold,
            depth,
            key,
            value,
            tail,
            tally,
        });
    }
}

#[derive(Clone, Copy)]
enum Bracket {
    Object,
    Array,
}

impl Bracket {
    fn open(self) -> &'static str {
        match self {
            Self::Object => OBJECT_OPEN,
            Self::Array => ARRAY_OPEN,
        }
    }

    fn close(self) -> &'static str {
        match self {
            Self::Object => OBJECT_CLOSE,
            Self::Array => ARRAY_CLOSE,
        }
    }

    fn empty(self) -> &'static str {
        match self {
            Self::Object => EMPTY_OBJECT,
            Self::Array => EMPTY_ARRAY,
        }
    }

    fn folded(self) -> &'static str {
        match self {
            Self::Object => FOLDED_OBJECT,
            Self::Array => FOLDED_ARRAY,
        }
    }

    fn tally(self, count: usize) -> String {
        let unit = match self {
            Self::Object => KEY_UNIT,
            Self::Array => ITEM_UNIT,
        };
        let plural = match count {
            1 => "",
            _ => PLURAL,
        };
        format!(" {count}{unit}{plural}")
    }
}

fn quoted(key: &str) -> String {
    Value::String(key.to_owned()).to_string()
}

/// The drafts as painted rows. A key carries a colour of its own and the
/// punctuation around it is dim, because a JSON grammar scopes a key and a
/// string value alike and so every theme paints them alike. A scalar is left
/// to the grammar, which is where a string, a number and a literal do part
/// company.
fn paint(drafts: Vec<Draft>) -> Vec<JsonRow> {
    let t = theme::current();
    drafts
        .into_iter()
        .map(|draft| {
            let mark = match draft.fold {
                Some(_) if draft.tally.is_some() => CLOSED_MARK,
                Some(_) => OPEN_MARK,
                None => NO_MARK,
            };
            let mut spans = vec![
                Span::raw(INDENT.repeat(draft.depth)),
                Span::styled(mark, t.tool_dim),
            ];
            if let Some(key) = draft.key {
                spans.push(Span::styled(key, t.accent));
                spans.push(Span::styled(KEY_SEPARATOR, t.tool_dim));
            }
            match is_structure(&draft.value) {
                true => spans.push(Span::styled(draft.value, t.tool_dim)),
                false => spans.extend(scalar_spans(&draft.value)),
            }
            if !draft.tail.is_empty() {
                spans.push(Span::styled(draft.tail, t.tool_dim));
            }
            if let Some(tally) = draft.tally {
                spans.push(Span::styled(tally, t.tool_dim));
            }
            JsonRow {
                fold: draft.fold,
                line: Line::from(spans),
            }
        })
        .collect()
}

/// Braces and brackets are punctuation. Nothing else can open with one,
/// because a string is quoted before it is anything else.
fn is_structure(value: &str) -> bool {
    value.starts_with(['{', '[', '}', ']'])
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;
    use serde_json::json;

    use super::*;

    const NOT_A_TREE: &str = "only an object or an array is worth folding";
    const EVERY_NODE_SHOWS: &str = "an unfolded value draws every one of its nodes";
    const FOLD_HIDES_CHILDREN: &str = "a closed node draws no line for what it holds";
    const FOLD_KEEPS_NAMES: &str = "a node keeps its name when a node above it closes";
    const TALLY_READS_THE_COUNT: &str = "a closed node says how much it is hiding";
    const KEY_READS_APART: &str = "a key is not the colour of the string beside it";
    const KINDS_READ_APART: &str = "a string, a number and a literal are not one colour";
    const NO_STYLE: &str = "every span of the row carried a colour";

    fn text(rows: &[JsonRow]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                row.line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn nested() -> Value {
        json!({ "a": 1, "b": { "c": [2, 3] } })
    }

    /// The colour of the first span whose text is `needle`.
    fn colour_of(rows: &[JsonRow], needle: &str) -> Color {
        rows.iter()
            .flat_map(|row| row.line.spans.iter())
            .find(|span| span.content.as_ref() == needle)
            .and_then(|span| span.style.fg)
            .expect(NO_STYLE)
    }

    /// A JSON grammar scopes a key and a string value the same way, so a theme
    /// paints them the same colour and a reader cannot tell which side of the
    /// colon they are looking at. The key is painted here instead.
    #[test]
    fn a_key_does_not_read_as_the_string_beside_it() {
        crate::highlight::refresh_syntax_theme();
        let value = json!({ "name": "value", "count": 7, "flag": true });

        let rows = rows(&value, &HashSet::new()).expect(NOT_A_TREE);

        let key = colour_of(&rows, "\"name\"");
        let string = colour_of(&rows, "value");
        let number = colour_of(&rows, "7");
        let literal = colour_of(&rows, "true");
        assert_ne!(key, string, "{KEY_READS_APART}");
        assert_ne!(string, number, "{KINDS_READ_APART}");
        assert_ne!(number, literal, "{KINDS_READ_APART}");
    }

    #[test]
    fn a_scalar_is_not_a_tree() {
        assert!(
            rows(&json!("plain"), &HashSet::new()).is_none(),
            "{NOT_A_TREE}"
        );
        assert!(rows(&json!(7), &HashSet::new()).is_none(), "{NOT_A_TREE}");
    }

    #[test]
    fn an_open_value_draws_every_node() {
        let rows = rows(&nested(), &HashSet::new()).expect(NOT_A_TREE);

        assert_eq!(
            text(&rows),
            vec![
                "\u{25be} {",
                "    \"a\": 1,",
                "  \u{25be} \"b\": {",
                "    \u{25be} \"c\": [",
                "        2,",
                "        3",
                "      ]",
                "    }",
                "  }",
            ],
            "{EVERY_NODE_SHOWS}"
        );
    }

    #[test]
    fn a_closed_node_hides_what_it_holds_and_says_how_much() {
        let rows = rows(&nested(), &HashSet::from([1])).expect(NOT_A_TREE);

        let text = text(&rows);
        assert!(
            text.iter().all(|line| !line.contains("\"c\"")),
            "{FOLD_HIDES_CHILDREN}: {text:?}"
        );
        assert!(
            text.iter().any(|line| line.contains("1 key")),
            "{TALLY_READS_THE_COUNT}: {text:?}"
        );
    }

    /// The node named 2 is the array inside `b`. Closing `b` must not hand its
    /// name to something else, or reopening `b` would open a different node.
    #[test]
    fn a_node_keeps_its_name_when_an_ancestor_closes() {
        let open = rows(&nested(), &HashSet::new()).expect(NOT_A_TREE);
        let folds: Vec<usize> = open.iter().filter_map(|row| row.fold).collect();

        let closed = rows(&nested(), &HashSet::from([1])).expect(NOT_A_TREE);

        let after: Vec<usize> = closed.iter().filter_map(|row| row.fold).collect();
        assert_eq!(folds, vec![0, 1, 2], "{FOLD_KEEPS_NAMES}");
        assert_eq!(after, vec![0, 1], "{FOLD_KEEPS_NAMES}");
    }
}
