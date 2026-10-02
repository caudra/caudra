//! Bidirectional JSON <-> `MontyObject` conversion.
//! Lossy corners: NaN floats become `null` (JSON can't represent NaN),
//! BigInts that overflow `i64` become strings, and tuples become arrays.

use monty_types::unstable::{self, MontyNode};
use monty_types::{CallArgs, MontyObject, ObjectRef};
use serde_json::Value;

use crate::error::InterpreterError;

const MAX_VALUE_DEPTH: usize = 100;
pub(crate) const MAX_EXPANDED_VALUE_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const VALUE_TOO_LARGE: &str = "Python value exceeds JSON conversion limits";
const MAX_REPR_BYTE_EXPANSION: usize = 4;

#[derive(Debug)]
pub(crate) struct JsonArgs {
    pub args: Vec<Value>,
    pub kwargs: Vec<(String, Value)>,
    pub bytes: usize,
}

pub fn json_to_monty(value: Value) -> MontyObject {
    match value {
        Value::Null => MontyObject::none(),
        Value::Bool(b) => MontyObject::bool(b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                MontyObject::int(i)
            } else {
                MontyObject::float(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => MontyObject::string(s),
        Value::Array(arr) => MontyObject::list(arr.into_iter().map(json_to_monty)),
        Value::Object(map) => MontyObject::dict(
            map.into_iter()
                .map(|(k, v)| (MontyObject::string(k), json_to_monty(v))),
        ),
    }
}

pub fn monty_to_json(obj: ObjectRef<'_>) -> Result<Value, InterpreterError> {
    let mut remaining = MAX_EXPANDED_VALUE_BYTES;
    check_expansion(obj, MAX_VALUE_DEPTH, &mut remaining)?;
    Ok(convert_value(obj))
}

pub(crate) fn call_args_to_json(
    args: &CallArgs,
    budget: usize,
) -> Result<JsonArgs, InterpreterError> {
    let mut remaining = budget;
    for value in args
        .args()
        .chain(args.kwargs().flat_map(|(key, value)| [key, value]))
    {
        check_expansion(value, MAX_VALUE_DEPTH, &mut remaining)?;
    }
    Ok(JsonArgs {
        args: args.args().map(convert_value).collect(),
        kwargs: args
            .kwargs()
            .map(|(key, value)| (key.to_string(), convert_value(value)))
            .collect(),
        bytes: budget - remaining,
    })
}

fn check_expansion(
    obj: ObjectRef<'_>,
    depth: usize,
    remaining: &mut usize,
) -> Result<(), InterpreterError> {
    let node = unstable::node(obj);
    let Some(depth) = depth.checked_sub(1) else {
        return Err(InterpreterError::Runtime(VALUE_TOO_LARGE.into()));
    };
    let expanded_bytes = match node {
        MontyNode::Bytes(bytes) => node
            .decoded_size()
            .saturating_add(bytes.len().saturating_mul(size_of::<Value>())),
        _ => node.decoded_size().saturating_mul(MAX_REPR_BYTE_EXPANSION),
    };
    *remaining = remaining
        .checked_sub(expanded_bytes)
        .ok_or_else(|| InterpreterError::Runtime(VALUE_TOO_LARGE.into()))?;
    let mut result = Ok(());
    node.for_each_child(|id| {
        if result.is_ok() {
            result = check_expansion(unstable::child(obj, id), depth, remaining);
        }
    });
    result
}

fn convert_value(obj: ObjectRef<'_>) -> Value {
    match unstable::node(obj) {
        MontyNode::None => Value::Null,
        MontyNode::Bool(b) => Value::Bool(*b),
        MontyNode::Int(i) => Value::Number((*i).into()),
        MontyNode::Float(f) => serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number),
        MontyNode::String(s) => Value::String(s.clone()),
        MontyNode::List(items) | MontyNode::Tuple(items) => Value::Array(
            items
                .iter()
                .map(|&id| convert_value(unstable::child(obj, id)))
                .collect(),
        ),
        MontyNode::Dict(pairs) => {
            let map: serde_json::Map<String, Value> = pairs
                .iter()
                .map(|&(k, v)| {
                    (
                        unstable::child(obj, k).to_string(),
                        convert_value(unstable::child(obj, v)),
                    )
                })
                .collect();
            Value::Object(map)
        }
        MontyNode::Bytes(b) => {
            Value::Array(b.iter().map(|&byte| Value::Number(byte.into())).collect())
        }
        MontyNode::BigInt(bi) => {
            if let Ok(i) = i64::try_from(bi) {
                Value::Number(i.into())
            } else {
                Value::String(bi.to_string())
            }
        }
        MontyNode::Repr(s) | MontyNode::Cycle(s) => Value::String(s.clone()),
        _ => Value::String(obj.py_repr()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_VALUE_DEPTH, VALUE_TOO_LARGE, call_args_to_json, json_to_monty, monty_to_json,
    };
    use crate::error::InterpreterError;
    use monty_types::MontyObject;
    use monty_types::unstable::{self, MontyGraph, MontyNode};
    use serde_json::{Value, json};
    use test_case::test_case;

    const ARGUMENT_BUDGET: usize = 8 * 1024;
    const ALIASED_STRING_BYTES: usize = 1024;
    const BYTE_ARRAY_LENGTH: usize = 128;

    #[test_case(json!(null),    MontyObject::none()          ; "null_to_none")]
    #[test_case(json!(true),    MontyObject::bool(true)      ; "bool_true")]
    #[test_case(json!(false),   MontyObject::bool(false)     ; "bool_false")]
    #[test_case(json!(42),      MontyObject::int(42)         ; "integer")]
    #[test_case(json!(-1),      MontyObject::int(-1)         ; "negative_int")]
    #[test_case(json!(2.5),     MontyObject::float(2.5)       ; "float")]
    #[test_case(json!("hello"), MontyObject::string("hello") ; "string")]
    fn json_to_monty_scalars(input: Value, expected: MontyObject) {
        assert_eq!(json_to_monty(input), expected);
    }

    #[test_case(json!(null)                       ; "null")]
    #[test_case(json!(true)                       ; "bool")]
    #[test_case(json!(42)                         ; "int")]
    #[test_case(json!(2.5)                        ; "float")]
    #[test_case(json!("text")                     ; "string")]
    #[test_case(json!([1, 2, 3])                  ; "array")]
    #[test_case(json!({"a": 1, "b": [true, null]}); "nested_object")]
    #[test_case(json!([])                         ; "empty_array")]
    #[test_case(json!({})                         ; "empty_object")]
    #[test_case(json!({"key": [1, "two", null]})  ; "mixed_nested")]
    #[test_case(json!(i64::MAX)                   ; "large_i64")]
    fn roundtrip_preserves_value(input: Value) {
        let back = monty_to_json(json_to_monty(input.clone()).as_ref()).unwrap();
        assert_eq!(back, input);
    }

    #[test]
    fn nan_float_becomes_null() {
        let obj = MontyObject::float(f64::NAN);
        assert_eq!(monty_to_json(obj.as_ref()).unwrap(), Value::Null);
    }

    #[test_case(2, 0 ; "positional_aliases")]
    #[test_case(0, 2 ; "keyword_aliases")]
    #[test_case(1, 1 ; "positional_and_keyword_aliases")]
    fn aggregate_call_budget_counts_every_alias(positional: usize, keyword: usize) {
        let mut graph = MontyGraph::new();
        let payload = graph.push(MontyNode::String("x".repeat(ALIASED_STRING_BYTES)));
        let kwargs = (0..keyword)
            .map(|index| {
                let key = graph.push(MontyNode::String(format!("key_{index}")));
                (key, payload)
            })
            .collect();
        let args =
            unstable::call_args_from_parts(graph, vec![payload; positional], kwargs).unwrap();
        let err = call_args_to_json(&args, ARGUMENT_BUDGET).unwrap_err();
        assert!(
            matches!(err, InterpreterError::Runtime(ref message) if message == VALUE_TOO_LARGE)
        );
        let converted = call_args_to_json(&args, ARGUMENT_BUDGET * 2).unwrap();
        assert_eq!(converted.args.len(), positional);
        assert_eq!(converted.kwargs.len(), keyword);
        assert!(converted.bytes > ARGUMENT_BUDGET);
    }

    #[test]
    fn byte_array_budget_accounts_for_json_elements() {
        let mut graph = MontyGraph::new();
        let payload = graph.push(MontyNode::Bytes(vec![0; BYTE_ARRAY_LENGTH]));
        let args = unstable::call_args_from_parts(graph, vec![payload], vec![]).unwrap();
        let err = call_args_to_json(&args, BYTE_ARRAY_LENGTH * size_of::<Value>()).unwrap_err();
        assert!(
            matches!(err, InterpreterError::Runtime(ref message) if message == VALUE_TOO_LARGE)
        );
    }

    #[test_case(false ; "deep_value")]
    #[test_case(true ; "exponentially_shared_value")]
    fn flat_values_cannot_overflow_or_explode_json_conversion(shared: bool) {
        let mut graph = MontyGraph::new();
        let mut root = graph.push(MontyNode::None);
        let depth = if shared {
            MAX_VALUE_DEPTH / 2
        } else {
            MAX_VALUE_DEPTH
        };
        for _ in 0..depth {
            root = graph.push(MontyNode::List(if shared {
                vec![root, root]
            } else {
                vec![root]
            }));
        }
        let value = unstable::object_from_graph(graph, root).unwrap();
        let err = monty_to_json(value.as_ref()).unwrap_err();
        assert!(
            matches!(err, InterpreterError::Runtime(ref message) if message == VALUE_TOO_LARGE)
        );
    }
}
