//! Value rendering and bounded output capture.
//!
//! Two independent representations leave the sandbox. `result` is JSON, which agents can parse, and
//! `resultRepr` is Python's own rendering, which is only emitted when the JSON form loses type
//! identity or precision. Emitting the repr unconditionally would double every payload for the
//! common case of a number or a string.

use monty_types::{
    MontyObject, ObjectRef,
    unstable::{MontyNode, NodeId, child, graph_parts, node},
};
use serde_json::{Map, Value};

/// Independent of Monty's own value-depth cap: this bounds the JSON we build, not the frame we
/// received, so a deeply nested value degrades to its repr instead of recursing.
const MAX_RENDER_DEPTH: usize = 32;
const MAX_RENDER_COST: usize = 8 * 1024 * 1024;
const MAX_ESCAPE_EXPANSION: usize = 6;
const OMITTED_VALUE: &str = "<value omitted: expanded representation exceeds the rendering budget>";

/// A rendered value plus whether JSON lost anything a caller might need.
pub(crate) struct Rendered {
    pub(crate) json: Value,
    pub(crate) lossy: bool,
}

impl Rendered {
    const fn exact(json: Value) -> Self {
        Self { json, lossy: false }
    }
    const fn lossy(json: Value) -> Self {
        Self { json, lossy: true }
    }
}

/// Renders a returned value, pairing JSON with a repr only when JSON is not faithful.
pub(crate) fn render(object: &MontyObject) -> (Value, Option<String>) {
    if !within_render_budget(object) {
        return (Value::Null, Some(OMITTED_VALUE.to_owned()));
    }
    let rendered = render_inner(object.as_ref(), 0);
    let repr = rendered.lossy.then(|| object.to_string());
    (rendered.json, repr)
}

fn within_render_budget(object: &MontyObject) -> bool {
    let (graph, root) = graph_parts(object);
    let mut costs = Vec::<usize>::with_capacity(graph.len());
    for node in graph.nodes() {
        let mut cost = node.decoded_size().saturating_mul(MAX_ESCAPE_EXPANSION);
        node.for_each_child(|id| cost = cost.saturating_add(costs[id.index()]));
        costs.push(cost.min(MAX_RENDER_COST + 1));
    }
    costs[root.index()] <= MAX_RENDER_COST
}

fn render_inner(object: ObjectRef<'_>, depth: usize) -> Rendered {
    if depth >= MAX_RENDER_DEPTH {
        return Rendered::lossy(Value::Null);
    }
    match node(object) {
        // Faithful in JSON: the type survives the round trip.
        MontyNode::None => Rendered::exact(Value::Null),
        MontyNode::Bool(value) => Rendered::exact(Value::Bool(*value)),
        MontyNode::Int(value) => Rendered::exact(Value::Number((*value).into())),
        MontyNode::String(value) => Rendered::exact(Value::String(value.clone())),
        MontyNode::Float(value) => serde_json::Number::from_f64(*value).map_or_else(
            // NaN and the infinities have no JSON literal; only the repr can carry them.
            || Rendered::lossy(Value::Null),
            |number| Rendered::exact(Value::Number(number)),
        ),
        MontyNode::List(items) => render_sequence(object, items, depth, false),

        // Representable, but the JSON form drops the Python type.
        MontyNode::Tuple(items) | MontyNode::Set(items) | MontyNode::FrozenSet(items) => {
            render_sequence(object, items, depth, true)
        }
        // A big integer exceeds JSON's safe numeric range, so it crosses as a decimal string.
        MontyNode::BigInt(value) => Rendered::lossy(Value::String(value.to_string())),
        MontyNode::Dict(pairs) => {
            let mut map = Map::new();
            let mut lossy = false;
            let mut fallback = Vec::new();
            for (key, value) in pairs {
                let rendered = render_inner(child(object, *value), depth + 1);
                lossy |= rendered.lossy;
                match node(child(object, *key)) {
                    MontyNode::String(name) => {
                        map.insert(name.clone(), rendered.json);
                    }
                    // Non-string keys have no JSON object equivalent, so the whole dict degrades to
                    // an entry list rather than silently stringifying keys into a lossy object.
                    _ => {
                        fallback.push((child(object, *key).to_string(), rendered.json));
                    }
                }
            }
            if fallback.is_empty() {
                return Rendered {
                    json: Value::Object(map),
                    lossy,
                };
            }
            let entries = map
                .into_iter()
                .chain(fallback)
                .map(|(key, value)| Value::Array(vec![Value::String(key), value]))
                .collect();
            Rendered::lossy(Value::Array(entries))
        }
        MontyNode::NamedTuple {
            field_names,
            values,
            ..
        } => {
            let mut map = Map::new();
            for (name, value) in field_names.iter().zip(values) {
                map.insert(
                    name.clone(),
                    render_inner(child(object, *value), depth + 1).json,
                );
            }
            Rendered::lossy(Value::Object(map))
        }
        MontyNode::ClassInstance { attrs, .. } => {
            let mut map = Map::new();
            for (key, value) in attrs {
                let rendered = render_inner(child(object, *value), depth + 1);
                map.insert(child(object, *key).to_string(), rendered.json);
            }
            Rendered::lossy(Value::Object(map))
        }

        // No JSON analogue at all: the repr is the only faithful form, and the string rendering is
        // provided so a caller that only reads `result` still sees something meaningful.
        MontyNode::Bytes(_)
        | MontyNode::Date(_)
        | MontyNode::DateTime(_)
        | MontyNode::Time(_)
        | MontyNode::TimeDelta(_)
        | MontyNode::TimeZone(_)
        | MontyNode::Path(_)
        | MontyNode::Type(_)
        | MontyNode::ClassType(_)
        | MontyNode::BuiltinFunction(_)
        | MontyNode::FileHandle(_)
        | MontyNode::Function { .. }
        | MontyNode::Exception { .. }
        | MontyNode::Repr(_)
        | MontyNode::Cycle(_)
        | MontyNode::Ellipsis
        | MontyNode::NotImplemented => Rendered::lossy(Value::String(object.to_string())),
    }
}

fn render_sequence(
    object: ObjectRef<'_>,
    items: &[NodeId],
    depth: usize,
    type_lost: bool,
) -> Rendered {
    let mut lossy = type_lost;
    let values = items
        .iter()
        .map(|item| {
            let rendered = render_inner(child(object, *item), depth + 1);
            lossy |= rendered.lossy;
            rendered.json
        })
        .collect();
    Rendered {
        json: Value::Array(values),
        lossy,
    }
}

/// Bounded, ordered capture of one `print` stream.
///
/// The head is retained rather than the tail: a short script's output reads as a narrative from the
/// start, and the returned value travels in `result` regardless, so truncating the end cannot lose
/// the answer itself.
pub(crate) struct Capture {
    text: String,
    limit: usize,
    truncated: bool,
    total_bytes: u64,
}

impl Capture {
    pub(crate) const fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            limit,
            truncated: false,
            total_bytes: 0,
        }
    }

    pub(crate) fn push(&mut self, chunk: &str) {
        self.total_bytes = self.total_bytes.saturating_add(chunk.len() as u64);
        let remaining = self.limit.saturating_sub(self.text.len());
        if remaining == 0 {
            self.truncated = !chunk.is_empty() || self.truncated;
            return;
        }
        if chunk.len() <= remaining {
            self.text.push_str(chunk);
            return;
        }
        // Split on a character boundary so the retained head stays valid UTF-8.
        let mut end = remaining;
        while end > 0 && !chunk.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&chunk[..end]);
        self.truncated = true;
    }

    pub(crate) fn into_parts(self) -> (String, bool, u64) {
        (self.text, self.truncated, self.total_bytes)
    }
}

#[cfg(test)]
mod tests {
    use monty_types::unstable::{MontyGraph, object_from_graph};

    use super::*;

    #[test]
    fn plain_scalars_need_no_repr() {
        for object in [
            MontyObject::none(),
            MontyObject::bool(true),
            MontyObject::int(42),
            MontyObject::float(1.5),
            MontyObject::string("hi"),
        ] {
            let (_, repr) = render(&object);
            assert!(repr.is_none(), "{object:?} should render exactly");
        }
    }

    #[test]
    fn json_matches_python_values() {
        assert_eq!(render(&MontyObject::int(42)).0, serde_json::json!(42));
        assert_eq!(render(&MontyObject::none()).0, Value::Null);
        assert_eq!(
            render(&MontyObject::list(vec![
                MontyObject::int(1),
                MontyObject::string("a")
            ]))
            .0,
            serde_json::json!([1, "a"])
        );
    }

    #[test]
    fn lossy_values_carry_a_repr() {
        // A tuple survives as an array but loses its type, so the repr disambiguates it from a list.
        // The exact repr text is Monty's to define; only its presence is our contract.
        let (json, repr) = render(&MontyObject::tuple(vec![MontyObject::int(1)]));
        assert_eq!(json, serde_json::json!([1]));
        assert!(repr.is_some_and(|text| text.starts_with('(')));

        // A non-finite float has no JSON literal at all.
        let (json, repr) = render(&MontyObject::float(f64::INFINITY));
        assert_eq!(json, Value::Null);
        assert!(repr.is_some());
    }

    #[test]
    fn lossiness_propagates_out_of_containers() {
        let (_, repr) = render(&MontyObject::list(vec![MontyObject::bytes(vec![1])]));
        assert!(
            repr.is_some(),
            "a list holding a lossy element is itself lossy"
        );
    }

    #[test]
    fn string_keyed_dicts_become_objects() {
        let dict = MontyObject::dict(vec![(MontyObject::string("k"), MontyObject::int(1))]);
        let (json, repr) = render(&dict);
        assert_eq!(json, serde_json::json!({"k": 1}));
        assert!(repr.is_none());
    }

    #[test]
    fn non_string_keyed_dicts_degrade_to_entry_lists() {
        let dict = MontyObject::dict(vec![(MontyObject::int(1), MontyObject::int(2))]);
        let (json, repr) = render(&dict);
        assert_eq!(json, serde_json::json!([["1", 2]]));
        assert!(repr.is_some());
    }

    #[test]
    fn deep_nesting_degrades_instead_of_recursing() {
        let mut object = MontyObject::int(1);
        for _ in 0..(MAX_RENDER_DEPTH + 8) {
            object = MontyObject::list(vec![object]);
        }
        let (_, repr) = render(&object);
        assert!(repr.is_some());
    }

    #[test]
    fn shared_graphs_are_bounded_before_json_or_repr_expansion() {
        for leaf in [MontyNode::Int(1), MontyNode::Bytes(vec![1])] {
            let mut graph = MontyGraph::new();
            let mut root = graph.push(leaf);
            for _ in 0..16 {
                root = graph.push(MontyNode::List(vec![root, root]));
            }
            let value = object_from_graph(graph, root).expect("valid shared graph");
            let (json, repr) = render(&value);
            assert!(json.is_null());
            assert_eq!(repr.as_deref(), Some(OMITTED_VALUE));
        }
    }

    #[test]
    fn small_shared_values_render_each_reference_without_losing_types() {
        let mut graph = MontyGraph::new();
        let item = graph.push(MontyNode::Int(1));
        let tuple = graph.push(MontyNode::Tuple(vec![item, item]));
        let root = graph.push(MontyNode::List(vec![tuple, tuple]));
        let value = object_from_graph(graph, root).expect("valid shared graph");
        let (json, repr) = render(&value);
        assert_eq!(json, serde_json::json!([[1, 1], [1, 1]]));
        assert_eq!(repr.as_deref(), Some("[(1, 1), (1, 1)]"));
    }

    #[test]
    fn capture_retains_head_and_flags_truncation() {
        let mut capture = Capture::new(8);
        capture.push("abcd");
        capture.push("efghijkl");
        let (text, truncated, total) = capture.into_parts();
        assert_eq!(text, "abcdefgh");
        assert!(truncated);
        assert_eq!(total, 12);
    }

    #[test]
    fn capture_never_splits_a_character() {
        // "aé" is three bytes, so a two-byte budget must stop before the multi-byte character
        // rather than retaining half of it.
        let mut capture = Capture::new(2);
        capture.push("aé");
        let (text, truncated, total) = capture.into_parts();
        assert_eq!(text, "a");
        assert!(truncated);
        assert_eq!(total, 3);
    }

    #[test]
    fn capture_keeps_a_character_that_exactly_fits() {
        let mut capture = Capture::new(3);
        capture.push("aé");
        let (text, truncated, _) = capture.into_parts();
        assert_eq!(text, "aé");
        assert!(!truncated);
    }

    #[test]
    fn capture_under_limit_is_exact() {
        let mut capture = Capture::new(64);
        capture.push("hello\n");
        let (text, truncated, total) = capture.into_parts();
        assert_eq!(text, "hello\n");
        assert!(!truncated);
        assert_eq!(total, 6);
    }
}
