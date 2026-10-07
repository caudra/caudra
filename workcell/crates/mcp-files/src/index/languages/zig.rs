use tree_sitter::Node;

use super::common::{
    ExtractResult, LanguageSpec, extract_fields_truncated, split_path, strip_delimited,
};
use crate::index::{
    model::{ChildKind, Entry, Section},
    render::compact_whitespace,
    traversal::Context,
};

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new("/", extract_nodes);
    spec.is_doc_comment =
        Some(|node, context| node.kind() == "comment" && context.text(node).starts_with("///"));
    spec.is_module_doc =
        Some(|node, context| node.kind() == "comment" && context.text(node).starts_with("//!"));
    spec.is_test_node = Some(|node, _, _| node.kind() == "test_declaration");
    spec
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    match node.kind() {
        "function_declaration" => Ok(extract_function(node, context)?.into_iter().collect()),
        "variable_declaration" => extract_variable_with_value(node, context),
        "using_namespace_declaration" => Ok(extract_import(node, context)?.into_iter().collect()),
        _ => Ok(Vec::new()),
    }
}

fn first_identifier<'tree>(
    node: Node<'tree>,
    context: &Context<'_>,
) -> ExtractResult<Option<Node<'tree>>> {
    Ok(context
        .children(node)?
        .into_iter()
        .find(|child| child.kind() == "identifier"))
}

fn extract_import(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    for child in context.children(node)? {
        if child.kind() != "builtin_function" {
            continue;
        }
        if context
            .child(child, "builtin_identifier")?
            .is_none_or(|identifier| context.text(identifier) != "@import")
        {
            continue;
        }
        if let Some(arguments) = context.child(child, "arguments")? {
            for argument in context.children(arguments)? {
                if argument.kind() == "string" {
                    let raw_path = context.text(argument);
                    let path = strip_delimited(raw_path, "\"").unwrap_or(raw_path);
                    let segments = split_path(path, '/');
                    if !segments.is_empty() {
                        return Ok(Some(Entry::import(node, vec![segments], None)));
                    }
                }
            }
        }
    }
    Ok(None)
}

fn extract_function(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let parameters = context
        .child(node, "parameters")?
        .map_or("()".to_owned(), |parameters| {
            compact_whitespace(context.text(parameters))
        });
    let return_type = context
        .field(node, "type")?
        .map_or(String::new(), |return_type| {
            let text = context.text(return_type);
            if text == "void" {
                String::new()
            } else {
                format!(" {text}")
            }
        });
    Ok(Some(Entry::item(
        Section::Function,
        node,
        format!("{}{parameters}{return_type}", context.text(name)),
    )))
}

fn extract_variable(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = first_identifier(node, context)? else {
        return Ok(None);
    };
    let field_type = context
        .field(node, "type")?
        .map_or(String::new(), |field_type| {
            format!(": {}", context.text(field_type))
        });
    let constant = context
        .children(node)?
        .iter()
        .any(|child| child.kind() == "const");
    Ok(Some(Entry::item(
        Section::Constant,
        node,
        format!(
            "{} {}{field_type}",
            if constant { "const" } else { "var" },
            context.text(name)
        ),
    )))
}

fn extract_variable_with_value(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Vec<Entry>> {
    if let Some(import) = extract_import(node, context)? {
        return Ok(vec![import]);
    }
    let assigned_name = first_identifier(node, context)?.map(|name| context.text(name));
    for child in context.children(node)? {
        let entry = match child.kind() {
            "struct_declaration" => {
                Some(extract_container(child, context, "struct", assigned_name)?)
            }
            "enum_declaration" => Some(extract_enum(child, context, assigned_name)?),
            "union_declaration" => Some(extract_container(child, context, "union", assigned_name)?),
            "opaque_declaration" => Some(Entry::item(
                Section::Type,
                node,
                format!(
                    "opaque{}",
                    assigned_name.map_or(String::new(), |name| format!(" {name}"))
                ),
            )),
            "error_set_declaration" => Some(extract_error(child, context, assigned_name)?),
            _ => None,
        };
        if let Some(entry) = entry {
            return Ok(vec![entry]);
        }
    }
    Ok(extract_variable(node, context)?.into_iter().collect())
}

fn extract_container(
    node: Node<'_>,
    context: &Context<'_>,
    keyword: &str,
    assigned_name: Option<&str>,
) -> ExtractResult<Entry> {
    let name = context
        .field(node, "name")?
        .map(|name| context.text(name))
        .or(assigned_name)
        .unwrap_or_default();
    let mut entry = Entry::item(
        Section::Type,
        node,
        format!(
            "{keyword}{}",
            if name.is_empty() {
                String::new()
            } else {
                format!(" {name}")
            }
        ),
    );
    entry.item_mut().children =
        extract_fields_truncated(node, context, "container_field", |field, context| {
            let name = context
                .field(field, "name")?
                .map_or("_", |name| context.text(name));
            let field_type = context
                .field(field, "type")?
                .map_or("", |field_type| context.text(field_type));
            Ok(if field_type.is_empty() {
                name.to_owned()
            } else {
                format!("{name}: {field_type}")
            })
        })?;
    Ok(entry)
}

fn extract_enum(
    node: Node<'_>,
    context: &Context<'_>,
    assigned_name: Option<&str>,
) -> ExtractResult<Entry> {
    let mut entry = extract_container(node, context, "enum", assigned_name)?;
    entry.item_mut().children =
        extract_fields_truncated(node, context, "container_field", |field, context| {
            Ok(context
                .field(field, "name")?
                .map_or("_", |name| context.text(name))
                .to_owned())
        })?;
    entry.item_mut().child_kind = ChildKind::Brief;
    Ok(entry)
}

fn extract_error(
    node: Node<'_>,
    context: &Context<'_>,
    assigned_name: Option<&str>,
) -> ExtractResult<Entry> {
    let name = context
        .field(node, "name")?
        .map(|name| context.text(name))
        .or(assigned_name)
        .unwrap_or_default();
    let mut entry = Entry::item(
        Section::Type,
        node,
        format!(
            "error{}",
            if name.is_empty() {
                String::new()
            } else {
                format!(" {name}")
            }
        ),
    );
    entry.item_mut().children =
        extract_fields_truncated(node, context, "identifier", |field, context| {
            Ok(context.text(field).to_owned())
        })?;
    entry.item_mut().child_kind = ChildKind::Brief;
    Ok(entry)
}
