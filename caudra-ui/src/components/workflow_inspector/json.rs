//! The JSON a workflow call was given or answered with, as rows a reader can
//! fold. A result is often a deep object, and one long pretty-printed dump
//! buries every row under it, so each container is a row that can be closed.
//!
//! A node is named by where it sits in a walk of the whole value, so the name
//! does not move when an ancestor closes and the fold survives a redraw. The
//! walk therefore visits a closed node's children as well, to keep counting,
//! and simply draws nothing for them.

use std::collections::HashSet;

use ratatui::text::{Line, Span};
use serde_json::Value;

use crate::highlight::highlight_line;
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
const JSON_TOKEN: &str = "json";

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

/// A row before it is painted: what it opens, and the text it draws once the
/// fold marker is put in front of it.
struct Draft {
    fold: Option<usize>,
    depth: usize,
    text: String,
    /// A closed container says how much it is hiding, which is the only reason
    /// to open it again.
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
        let head = match key {
            Some(key) => format!("{}{KEY_SEPARATOR}", quoted(key)),
            None => String::new(),
        };
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
                self.container(&entries, Bracket::Object, &head, tail, depth, visible, true);
            }
            Value::Array(items) => {
                let entries: Vec<(&str, &Value)> = items.iter().map(|value| ("", value)).collect();
                self.container(&entries, Bracket::Array, &head, tail, depth, visible, false);
            }
            scalar => {
                if visible {
                    self.emit(depth, None, format!("{head}{scalar}{tail}"), None);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn container(
        &mut self,
        entries: &[(&str, &Value)],
        bracket: Bracket,
        head: &str,
        tail: &str,
        depth: usize,
        visible: bool,
        keyed: bool,
    ) {
        let node = self.next;
        self.next += 1;
        if entries.is_empty() {
            if visible {
                self.emit(
                    depth,
                    None,
                    format!("{head}{}{tail}", bracket.empty()),
                    None,
                );
            }
            return;
        }
        let closed = self.folded.contains(&node);
        if visible {
            let text = match closed {
                true => format!("{head}{}{tail}", bracket.folded()),
                false => format!("{head}{}", bracket.open()),
            };
            let tally = closed.then(|| bracket.tally(entries.len()));
            self.emit(depth, Some(node), text, tally);
        }
        let inside = visible && !closed;
        for (index, (key, value)) in entries.iter().enumerate() {
            let key = keyed.then_some(*key);
            self.push(value, key, depth + 1, index + 1 == entries.len(), inside);
        }
        if inside {
            self.emit(depth, None, format!("{}{tail}", bracket.close()), None);
        }
    }

    fn emit(&mut self, depth: usize, fold: Option<usize>, text: String, tally: Option<String>) {
        self.drafts.push(Draft {
            fold,
            depth,
            text,
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

/// The drafts as painted rows. Every line is highlighted on its own rather
/// than as one document, because a closed node leaves a gap that no grammar
/// can carry state across.
fn paint(drafts: Vec<Draft>) -> Vec<JsonRow> {
    let t = theme::current();
    let mut highlighter = caudra_highlight::Highlighter::for_token(JSON_TOKEN);
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
            spans.extend(highlight_line(&mut highlighter, &draft.text));
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const NOT_A_TREE: &str = "only an object or an array is worth folding";
    const EVERY_NODE_SHOWS: &str = "an unfolded value draws every one of its nodes";
    const FOLD_HIDES_CHILDREN: &str = "a closed node draws no line for what it holds";
    const FOLD_KEEPS_NAMES: &str = "a node keeps its name when a node above it closes";
    const TALLY_READS_THE_COUNT: &str = "a closed node says how much it is hiding";

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
