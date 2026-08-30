use tree_sitter::Node;

use super::common::{ExtractResult, LanguageSpec, extract_fields_truncated};
use crate::index::{
    model::{ChildKind, Entry, Section},
    render::{compact_whitespace, truncate},
    traversal::Context,
};

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new(".", extract_nodes);
    spec.is_doc_comment = Some(|node, _| matches!(node.kind(), "comment" | "marginalia"));
    spec
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    match node.kind() {
        "create_table"
        | "create_view"
        | "create_materialized_view"
        | "create_function"
        | "create_trigger"
        | "create_index"
        | "create_type"
        | "create_schema" => dispatch(node, context),
        "statement" => {
            for child in context.children(node)? {
                if child.kind().starts_with("create_") {
                    return dispatch(child, context);
                }
            }
            Ok(Vec::new())
        }
        "block" | "transaction" => {
            let mut entries = Vec::new();
            for child in context.children(node)? {
                if child.kind() == "statement" {
                    entries.extend(extract_nodes(child, context, &[])?);
                }
            }
            Ok(entries)
        }
        _ => Ok(Vec::new()),
    }
}

fn dispatch(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Vec<Entry>> {
    let entry = match node.kind() {
        "create_table" => Some(extract_table(node, context)?),
        "create_view" => Some(extract_view(node, context, false)?),
        "create_materialized_view" => Some(extract_view(node, context, true)?),
        "create_function" => Some(extract_function(node, context)?),
        "create_trigger" => extract_trigger(node, context)?,
        "create_index" => Some(extract_index(node, context)?),
        "create_type" => Some(extract_type(node, context)?),
        "create_schema" => Some(extract_schema(node, context)?),
        _ => None,
    };
    Ok(entry.into_iter().collect())
}

fn object_name<'a>(node: Node<'_>, context: &'a Context<'a>) -> ExtractResult<&'a str> {
    Ok(context
        .child(node, "object_reference")?
        .map_or("?", |name| context.text(name)))
}

fn columns(
    node: Node<'_>,
    context: &Context<'_>,
) -> ExtractResult<Vec<crate::index::model::Child>> {
    extract_fields_truncated(node, context, "column_definition", |column, context| {
        let name = context
            .field(column, "name")?
            .map_or("?", |name| context.text(name));
        let field_type = context
            .field(column, "type")?
            .map_or(String::new(), |field_type| {
                format!(" {}", context.text(field_type))
            });
        Ok(compact_whitespace(&format!("{name}{field_type}")))
    })
}

fn extract_table(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    let mut entry = Entry::item(
        Section::Class,
        node,
        format!("TABLE {}", object_name(node, context)?),
    );
    if let Some(body) = context.child(node, "column_definitions")? {
        entry.item_mut().children = columns(body, context)?;
    }
    Ok(entry)
}

fn extract_view(node: Node<'_>, context: &Context<'_>, materialized: bool) -> ExtractResult<Entry> {
    let mut entry = Entry::item(
        Section::Type,
        node,
        format!(
            "{}{}",
            if materialized {
                "MATERIALIZED VIEW "
            } else {
                "VIEW "
            },
            object_name(node, context)?
        ),
    );
    if let Some(query) = context.child(node, "create_query")? {
        entry
            .item_mut()
            .children
            .push(truncate(&compact_whitespace(context.text(query)), 80).into());
    }
    Ok(entry)
}

fn extract_function(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    let arguments = context
        .child(node, "function_arguments")?
        .map_or("()", |arguments| context.text(arguments));
    let language = context
        .child(node, "function_language")?
        .map_or(String::new(), |language| {
            format!(" {}", compact_whitespace(context.text(language)))
        });
    let mut entry = Entry::item(
        Section::Function,
        node,
        compact_whitespace(&format!(
            "FUNCTION {}{arguments}{language}",
            object_name(node, context)?
        )),
    );
    if let Some(body) = context.child(node, "function_body")? {
        entry
            .item_mut()
            .children
            .push(truncate(&compact_whitespace(context.text(body)), 80).into());
    }
    Ok(entry)
}

fn extract_trigger(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let references = context
        .children(node)?
        .into_iter()
        .filter(|child| child.kind() == "object_reference")
        .map(|child| context.text(child))
        .collect::<Vec<_>>();
    let Some(name) = references.first() else {
        return Ok(None);
    };
    Ok(Some(Entry::item(
        Section::Function,
        node,
        references.get(1).map_or_else(
            || format!("TRIGGER {name}"),
            |table| format!("TRIGGER {name} ON {table}"),
        ),
    )))
}

fn extract_index(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    let name = context.field(node, "column")?.and_then(|name| {
        (!context
            .text(node)
            .as_bytes()
            .get(name.end_byte().saturating_sub(node.start_byte()))
            .is_some_and(|byte| *byte == b'.'))
        .then(|| context.text(name))
    });
    let columns = context
        .child(node, "index_fields")?
        .map_or("()".to_owned(), |columns| {
            compact_whitespace(context.text(columns))
        });
    let table = object_name(node, context)?;
    Ok(Entry::item(
        Section::Function,
        node,
        name.map_or_else(
            || format!("INDEX ON {table}{columns}"),
            |name| format!("INDEX {name} ON {table}{columns}"),
        ),
    ))
}

fn extract_type(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    let mut entry = Entry::item(
        Section::Type,
        node,
        format!("TYPE {}", object_name(node, context)?),
    );
    if let Some(body) = context.child(node, "column_definitions")? {
        entry.item_mut().children = columns(body, context)?;
    } else if let Some(values) = context.child(node, "enum_elements")? {
        for value in context.children(values)? {
            if value.kind() == "literal" {
                entry
                    .item_mut()
                    .children
                    .push(context.text(value).to_owned().into());
            }
        }
        entry.item_mut().child_kind = ChildKind::Brief;
    }
    Ok(entry)
}

fn extract_schema(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    Ok(Entry::item(
        Section::Module,
        node,
        context
            .child(node, "identifier")?
            .map_or("?", |name| context.text(name)),
    ))
}
