//! Gemini's `parametersJsonSchema` takes standard JSON Schema, yet refuses the
//! whole request over a few shapes that published tool schemas, MCP ones
//! especially, still use. Those are rewritten, and the rest goes unchanged.

use serde_json::{Map, Value, json};

/// Keywords whose value maps names to subschemas.
const SCHEMA_MAPS: [&str; 6] = [
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
    "dependencies",
];
/// Keywords whose value is data, never a schema.
const DATA: [&str; 5] = ["const", "default", "enum", "examples", "dependentRequired"];
const DEFINITIONS: [&str; 2] = ["$defs", "definitions"];
const PROPERTIES: &str = "properties";
const REQUIRED: &str = "required";
const ITEMS: &str = "items";
const PREFIX_ITEMS: &str = "prefixItems";
const ADDITIONAL_ITEMS: &str = "additionalItems";
const MINIMUM: &str = "minimum";
const MAXIMUM: &str = "maximum";
const EXCLUSIVE_MINIMUM: &str = "exclusiveMinimum";
const EXCLUSIVE_MAXIMUM: &str = "exclusiveMaximum";
const MIN_ITEMS: &str = "minItems";
const REF: &str = "$ref";
const ROOT_REF: &str = "#";

pub(super) fn normalize(schema: Map<String, Value>) -> Value {
    let mut root = normalize_object(schema);
    for key in DEFINITIONS {
        if let Some(Value::Object(definitions)) = root.get_mut(key) {
            for (name, definition) in definitions.iter_mut() {
                let target = format!("{ROOT_REF}/{key}/{name}");
                *definition = cut_loops(definition.take(), &target, false);
            }
        }
    }
    cut_loops(Value::Object(root), ROOT_REF, false)
}

fn normalize_node(schema: Value) -> Value {
    match schema {
        Value::Array(items) => Value::Array(items.into_iter().map(normalize_node).collect()),
        Value::Object(map) => Value::Object(normalize_object(map)),
        other => other,
    }
}

fn normalize_object(schema: Map<String, Value>) -> Map<String, Value> {
    let declared: Option<Vec<String>> = schema
        .get(PROPERTIES)
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect());
    let exclusive_minimum = schema.get(EXCLUSIVE_MINIMUM) == Some(&Value::Bool(true));
    let exclusive_maximum = schema.get(EXCLUSIVE_MAXIMUM) == Some(&Value::Bool(true));
    let minimum = schema
        .get(MINIMUM)
        .filter(|bound| bound.is_number())
        .cloned();
    let maximum = schema
        .get(MAXIMUM)
        .filter(|bound| bound.is_number())
        .cloned();
    let tuple = schema.get(ITEMS).is_some_and(Value::is_array);
    let mut out = Map::new();
    for (key, value) in schema {
        match key.as_str() {
            data if DATA.contains(&data) => {
                out.insert(key, value);
            }
            map if SCHEMA_MAPS.contains(&map) && value.is_object() => {
                out.insert(key, map_schemas(value, |_, child| normalize_node(child)));
            }
            // `required` may name only declared properties.
            REQUIRED if declared.is_some() && value.is_array() => {
                let declared = declared.as_deref().unwrap_or_default();
                let named = value
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|name| {
                        name.as_str()
                            .is_some_and(|name| declared.iter().any(|d| d == name))
                    })
                    .cloned()
                    .collect();
                out.insert(key, Value::Array(named));
            }
            // Draft-04 boolean bounds become the numeric form.
            EXCLUSIVE_MINIMUM if value.is_boolean() => {
                if let Some(bound) = minimum.clone().filter(|_| exclusive_minimum) {
                    out.insert(key, bound);
                }
            }
            EXCLUSIVE_MAXIMUM if value.is_boolean() => {
                if let Some(bound) = maximum.clone().filter(|_| exclusive_maximum) {
                    out.insert(key, bound);
                }
            }
            MINIMUM if exclusive_minimum => {}
            MAXIMUM if exclusive_maximum => {}
            // A draft-07 tuple is `prefixItems` plus `items` in 2020-12.
            ITEMS if value.is_array() => {
                out.insert(PREFIX_ITEMS.into(), normalize_node(value));
            }
            ADDITIONAL_ITEMS if tuple => {
                out.insert(ITEMS.into(), normalize_node(value));
            }
            _ => {
                out.insert(key, normalize_node(value));
            }
        }
    }
    out
}

/// Gemini accepts a `$ref` back to `target` only when the loop passes through
/// an optional property or an `items` that may be empty, so any other one
/// becomes an unconstrained schema.
fn cut_loops(schema: Value, target: &str, safe: bool) -> Value {
    match schema {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| cut_loops(item, target, safe))
                .collect(),
        ),
        Value::Object(map) => {
            if !safe && map.get(REF).and_then(Value::as_str) == Some(target) {
                return json!({});
            }
            let required: Vec<String> = map
                .get(REQUIRED)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|name| name.as_str().map(String::from))
                .collect();
            let never_empty = map
                .get(MIN_ITEMS)
                .and_then(Value::as_u64)
                .is_some_and(|min| min > 0);
            let cut = map
                .into_iter()
                .map(|(key, value)| {
                    let value = match key.as_str() {
                        data if DATA.contains(&data) => value,
                        ITEMS => cut_loops(value, target, safe || !never_empty),
                        PROPERTIES if value.is_object() => map_schemas(value, |name, child| {
                            cut_loops(child, target, safe || !required.iter().any(|r| r == name))
                        }),
                        map if SCHEMA_MAPS.contains(&map) && value.is_object() => {
                            map_schemas(value, |_, child| cut_loops(child, target, safe))
                        }
                        _ => cut_loops(value, target, safe),
                    };
                    (key, value)
                })
                .collect();
            Value::Object(cut)
        }
        other => other,
    }
}

fn map_schemas(value: Value, mut map: impl FnMut(&str, Value) -> Value) -> Value {
    let Value::Object(entries) = value else {
        return value;
    };
    Value::Object(
        entries
            .into_iter()
            .map(|(name, child)| {
                let child = map(&name, child);
                (name, child)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const UNCHANGED: &str = "a schema Gemini already accepts goes through as is";

    fn normalized(schema: Value) -> Value {
        let Value::Object(map) = schema else {
            panic!("test schemas are objects");
        };
        normalize(map)
    }

    #[test_case(
        json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a", "ghost"]}),
        json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]})
        ; "required_drops_an_undeclared_property"
    )]
    #[test_case(
        json!({"type": "number", "minimum": 0, "exclusiveMinimum": true}),
        json!({"type": "number", "exclusiveMinimum": 0})
        ; "a_draft_04_exclusive_minimum_becomes_numeric"
    )]
    #[test_case(
        json!({"type": "number", "maximum": 9, "exclusiveMaximum": false}),
        json!({"type": "number", "maximum": 9})
        ; "a_false_exclusive_bound_keeps_the_inclusive_one"
    )]
    #[test_case(
        json!({"type": "array", "items": [{"type": "string"}, {"type": "integer"}], "additionalItems": false}),
        json!({"type": "array", "prefixItems": [{"type": "string"}, {"type": "integer"}], "items": false})
        ; "a_draft_07_tuple_becomes_prefix_items"
    )]
    #[test_case(
        json!({"type": "object", "properties": {"child": {"$ref": "#"}}, "required": ["child"]}),
        json!({"type": "object", "properties": {"child": {}}, "required": ["child"]})
        ; "a_loop_through_a_required_property_is_cut"
    )]
    #[test_case(
        json!({"type": "array", "minItems": 1, "items": {"$ref": "#"}}),
        json!({"type": "array", "minItems": 1, "items": {}})
        ; "a_loop_through_items_that_cannot_be_empty_is_cut"
    )]
    #[test_case(
        json!({"$defs": {"Node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/Node"}}, "required": ["next"]}}, "$ref": "#/$defs/Node"}),
        json!({"$defs": {"Node": {"type": "object", "properties": {"next": {}}, "required": ["next"]}}, "$ref": "#/$defs/Node"})
        ; "a_definition_that_requires_itself_is_cut"
    )]
    fn rewrites_what_gemini_refuses(schema: Value, expected: Value) {
        assert_eq!(normalized(schema), expected);
    }

    #[test_case(json!({"type": "object", "properties": {"child": {"$ref": "#"}}}) ; "a_loop_through_an_optional_property")]
    #[test_case(json!({"type": "array", "items": {"$ref": "#"}}) ; "a_loop_through_items_that_may_be_empty")]
    #[test_case(json!({"$schema": "https://json-schema.org/draft/2020-12/schema", "type": "object", "additionalProperties": false}) ; "schema_and_additional_properties")]
    #[test_case(json!({"type": "object", "properties": {"mode": {"oneOf": [{"const": "a"}, {"const": "b"}]}}}) ; "one_of_and_const")]
    #[test_case(json!({"enum": [{"required": ["ghost"], "items": [1]}], "default": {"minimum": 0, "exclusiveMinimum": true}}) ; "data_is_never_rewritten")]
    #[test_case(json!({"type": "object", "properties": {"required": {"type": "string"}, "items": {"type": "array", "items": {"type": "string"}}}}) ; "properties_named_like_keywords")]
    fn keeps_what_gemini_accepts(schema: Value) {
        assert_eq!(normalized(schema.clone()), schema, "{UNCHANGED}");
    }
}
