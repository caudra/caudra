//! The neutral tool specs and their MCP projection.
//!
//! `specs` is the source of truth. `catalog` is a lowering of it, not a second table, so a native
//! host and an MCP client are answering to the same contract by construction.

#[cfg(feature = "mcp")]
use rmcp::model::{MetaObject, Tool, ToolAnnotations};
use schemars::{JsonSchema, SchemaGenerator, generate::SchemaSettings};
use serde_json::{Map, Value, json};
use workcell_tool_contract::{ToolAnnotations as NeutralAnnotations, ToolContract, ToolSpec};

use crate::{
    group::MAX_REFS_DEPTH,
    types::{CodeContextOutput, CodeRefsOutput, SelectorRefusal},
};

const DRAFT_07: &str = "http://json-schema.org/draft-07/schema#";

const CONTEXT_DESCRIPTION: &str = r#"Return the symbols worth reading before making a specific change. Describe the change in your own words.

- `task` is prose: "where do we invalidate the cache after a write" works better than a bare identifier.
- A router classifies the task as identifier-shaped or prose-shaped and reports which, and why.
- Results fuse a lexical match against symbol names, paths and doc comments with the graph ranking. Only symbols that actually matched participate, so an unrelated task returns nothing rather than the repository's busiest utility.
- `confidence` is derived from how far the top result separates from the rest. It is a measure of separation, never a claim of correctness, and a single result is always low.
- Counts are FLOORS: edges are recovered from source text by name, so dynamic dispatch, callbacks, trait objects and macro-generated calls contribute none.
- The graph stops at a directory that is its own repository and at files the repository's .gitignore excludes; pass a nested repository as `path` to search it."#;

const REFS_DESCRIPTION: &str = r#"List what calls a symbol and from which lines, or what it calls. With `depth`, show what a change to it could reach and which tests already cover it. Use it instead of grep for a symbol's callers and call sites.

- `direction` is `callers` (default) or `callees`. Callers come most important first; callees in the order the body calls them.
- Each direct row lists its call sites as `at=` lines, so one call finds every place a change must touch.
- `depth` walks callers further back (default 1, host-capped). Rows report their hop, nearest first, with a count per hop and how many of them are tests.
- `symbol` may be a bare name or `path::name` to disambiguate. A name matching several definitions returns their union and says so, rather than silently picking one.
- An unknown symbol is REFUSED with did-you-mean candidates, not answered with zero. A symbol that exists and has no callers returns zero.
- Counts are FLOORS: dynamic dispatch, callbacks, trait objects and macro-generated calls contribute no edge, so 0 means none was found, never that none exists. A call whose receiver or path names a type outside the tree is never linked by name, and a link made by name alone is marked."#;

/// The neutral specs, in catalog order.
///
/// Order is a compatibility contract: the task-shaped search first, because it is where a caller
/// with no symbol in hand starts, then the walk from a symbol the caller already has.
#[must_use]
pub fn specs() -> Vec<ToolSpec> {
    vec![
        spec(
            "code_context",
            "Find code for a task",
            CONTEXT_DESCRIPTION,
            context_schema(),
            "code.map.v1",
            "code.context.v1",
        ),
        spec(
            "code_refs",
            "List callers, call sites and reach",
            REFS_DESCRIPTION,
            refs_schema(),
            "code.map.v1",
            "code.refs.v1",
        ),
    ]
}

/// Returns fresh values so a composing server may augment its own copy.
#[cfg(feature = "mcp")]
#[must_use]
pub fn catalog() -> Vec<Tool> {
    specs().iter().map(to_mcp_tool).collect()
}

fn spec(
    name: &'static str,
    title: &'static str,
    description: &'static str,
    input_schema: Map<String, Value>,
    presentation: &'static str,
    contract_id: &'static str,
) -> ToolSpec {
    ToolSpec::new(
        name,
        Some(title),
        description,
        input_schema,
        read_annotations(),
        presentation,
        ToolContract::new(contract_id, "v1", "v1"),
    )
    .with_output_schema(match name {
        "code_context" => output_schema::<CodeContextOutput>(),
        "code_refs" => output_union::<CodeRefsOutput, SelectorRefusal>(),
        _ => unreachable!("canonical code-graph spec"),
    })
}

/// Every tool here reads. None mutates, none is destructive, and none reaches the network.
fn read_annotations() -> NeutralAnnotations {
    NeutralAnnotations {
        read_only_hint: Some(true),
        destructive_hint: Some(false),
        idempotent_hint: Some(true),
        open_world_hint: Some(false),
    }
}

#[cfg(feature = "mcp")]
fn to_mcp_tool(spec: &ToolSpec) -> Tool {
    let tool = Tool::new(
        spec.name,
        spec.description.clone(),
        std::sync::Arc::new(spec.input_schema.clone()),
    );
    let tool = match spec.title {
        Some(title) => tool.with_title(title),
        None => tool,
    };
    let tool = tool.with_raw_output_schema(std::sync::Arc::new(
        spec.output_schema
            .clone()
            .expect("code-graph output schema"),
    ));
    tool.with_annotations(ToolAnnotations::from_raw(
        None,
        spec.annotations.read_only_hint,
        spec.annotations.destructive_hint,
        spec.annotations.idempotent_hint,
        spec.annotations.open_world_hint,
    ))
    .with_meta(MetaObject(spec.extension_metadata()))
}

fn output_schema<T: JsonSchema>() -> Map<String, Value> {
    Value::from(SchemaGenerator::new(SchemaSettings::draft07()).into_root_schema_for::<T>())
        .as_object()
        .expect("output schema is an object")
        .clone()
}

fn output_union<A: JsonSchema, B: JsonSchema>() -> Map<String, Value> {
    let mut variants = [output_schema::<A>(), output_schema::<B>()];
    let mut definitions = Map::new();
    for variant in &mut variants {
        variant.remove("$schema");
        if let Some(Value::Object(nested)) = variant.remove("definitions") {
            definitions.extend(nested);
        }
    }
    let mut output = schema(json!({
        "$schema": DRAFT_07,
        "oneOf": variants,
    }));
    if !definitions.is_empty() {
        output.insert("definitions".to_owned(), Value::Object(definitions));
    }
    output
}

fn path_property() -> Value {
    json!({
        "type": "string",
        "description": "Root-relative subdirectory to scope the graph to. Absent means the whole configured root, and an empty string is the same as absent."
    })
}

fn limit_property() -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "description": "Maximum rows to return. Narrows the result; it can never widen it past the host ceiling."
    })
}

fn symbol_property() -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "description": "A symbol name, optionally qualified as `path::name` to disambiguate."
    })
}

fn context_schema() -> Map<String, Value> {
    schema(json!({
        "$schema": DRAFT_07,
        "type": "object",
        "properties": {
            "task": {
                "type": "string",
                "minLength": 1,
                "description": "The change you are about to make, in your own words."
            },
            "path": path_property(),
            "limit": limit_property()
        },
        "required": ["task"],
        "additionalProperties": false
    }))
}

fn refs_schema() -> Map<String, Value> {
    schema(json!({
        "$schema": DRAFT_07,
        "type": "object",
        "properties": {
            "symbol": symbol_property(),
            "direction": {
                "type": "string",
                "enum": ["callers", "callees"],
                "default": "callers",
                "description": "`callers` lists symbols referencing this one; `callees` lists the ones it references."
            },
            "depth": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_REFS_DEPTH,
                "default": 1,
                "description": "Hops to walk back along call edges, callers only. Above 1 the result is what a change could reach. Beyond a few hops a reachability set describes the repository rather than a blast radius."
            },
            "path": path_property(),
            "limit": limit_property()
        },
        "required": ["symbol"],
        "additionalProperties": false
    }))
}

fn schema(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_order_is_stable_and_names_are_the_contract() {
        let names: Vec<&str> = specs().iter().map(|spec| spec.name).collect();
        assert_eq!(names, ["code_context", "code_refs"]);
    }

    #[test]
    fn every_tool_is_read_only_and_closed_world() {
        for spec in specs() {
            assert_eq!(spec.annotations.read_only_hint, Some(true), "{}", spec.name);
            assert_eq!(
                spec.annotations.destructive_hint,
                Some(false),
                "{}",
                spec.name
            );
            assert_eq!(
                spec.annotations.open_world_hint,
                Some(false),
                "{}: nothing here reaches the network",
                spec.name
            );
        }
    }

    #[test]
    fn no_schema_exposes_a_host_limit_as_an_input() {
        // A model that can raise a ceiling can raise it until the work no longer fits. `limit`
        // narrows only; every other bound is startup configuration and must not be nameable here.
        let forbidden = [
            "maxFiles",
            "maxSourceBytes",
            "maxTotalBytes",
            "crawlDeadline",
            "maxResultLimit",
        ];
        for spec in specs() {
            let rendered = serde_json::to_string(&spec.input_schema).expect("schema serializes");
            for name in forbidden {
                assert!(
                    !rendered.contains(name),
                    "{} exposes the host limit {name}",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn every_description_states_that_counts_are_floors() {
        for spec in specs() {
            let text = spec.description.to_lowercase();
            assert!(
                text.contains("floor"),
                "{} must carry the floor caveat: a caller reading a count without it will treat \
                 zero as proof",
                spec.name
            );
        }
    }
}
