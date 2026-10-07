use tree_sitter::Node;

use super::common::{ExtractResult, LanguageSpec};
use crate::index::{
    model::{ChildKind, Entry, Section},
    render::{
        FIELD_TRUNCATE_THRESHOLD, compact_whitespace, expand_import, prefixed, ranged, truncate,
        truncated_message,
    },
    traversal::Context,
};

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new(".", extract_nodes);
    spec.is_doc_comment = Some(|node, context| {
        node.kind() == "block_comment" && context.text(node).starts_with("/**")
    });
    spec
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    let entry = match node.kind() {
        "import_declaration" => Some(extract_import(node, context)?),
        "package_clause" => Some(extract_package(node, context)?),
        "class_definition" => extract_class(node, context, "class", Section::Class)?,
        "object_definition" => extract_class(node, context, "object", Section::Class)?,
        "trait_definition" => extract_class(node, context, "trait", Section::Trait)?,
        "function_definition" | "function_declaration" => extract_function(node, context)?,
        "val_definition" | "val_declaration" | "var_definition" | "var_declaration" => Some(
            Entry::item(Section::Constant, node, value_text(node, context)?),
        ),
        "type_definition" => extract_type(node, context)?,
        "enum_definition" => extract_enum(node, context)?,
        _ => None,
    };
    Ok(entry.into_iter().collect())
}

fn modifiers(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let Some(modifiers) = context.child(node, "modifiers")? else {
        return Ok(String::new());
    };
    Ok(context
        .children(modifiers)?
        .into_iter()
        .filter(|child| {
            matches!(
                child.kind(),
                "access_modifier"
                    | "case"
                    | "abstract"
                    | "sealed"
                    | "final"
                    | "implicit"
                    | "lazy"
                    | "override"
                    | "open"
            )
        })
        .map(|child| context.text(child))
        .collect::<Vec<_>>()
        .join(" "))
}

fn type_parameters(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    Ok(context
        .field(node, "type_parameters")?
        .map_or_else(String::new, |parameters| {
            context.text(parameters).to_owned()
        }))
}

fn function_signature(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<String>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let parameters = context
        .child(node, "parameters")?
        .map_or("()", |parameters| context.text(parameters));
    let return_type = context
        .field(node, "return_type")?
        .map_or(String::new(), |return_type| {
            format!(": {}", context.text(return_type))
        });
    Ok(Some(compact_whitespace(&prefixed(
        &modifiers(node, context)?,
        format!(
            "def {}{}{parameters}{return_type}",
            context.text(name),
            type_parameters(node, context)?
        ),
    ))))
}

fn value_text(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let keyword = context
        .children(node)?
        .into_iter()
        .find(|child| matches!(child.kind(), "val" | "var"))
        .map_or("val", |child| child.kind());
    let pattern = context
        .field(node, "pattern")?
        .map_or("_", |pattern| context.text(pattern));
    let field_type = context
        .field(node, "type")?
        .map_or(String::new(), |field_type| {
            format!(": {}", context.text(field_type))
        });
    Ok(truncate(
        &compact_whitespace(&prefixed(
            &modifiers(node, context)?,
            format!("{keyword} {pattern}{field_type}"),
        )),
        80,
    ))
}

fn extract_class(
    node: Node<'_>,
    context: &Context<'_>,
    keyword: &str,
    section: Section,
) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let extension = context
        .field(node, "extend")?
        .map_or(String::new(), |extension| {
            format!(" {}", context.text(extension))
        });
    let mut entry = Entry::item(
        section,
        node,
        compact_whitespace(&prefixed(
            &modifiers(node, context)?,
            format!(
                "{keyword} {}{}{extension}",
                context.text(name),
                type_parameters(node, context)?
            ),
        )),
    );
    if let Some(body) = context.child(node, "template_body")? {
        let mut values = 0usize;
        for child in context.children(body)? {
            match child.kind() {
                "function_definition" | "function_declaration" => {
                    if let Some(signature) = function_signature(child, context)? {
                        entry
                            .item_mut()
                            .children
                            .push(ranged(signature, context.range(child)));
                    }
                }
                "val_definition" | "var_definition" | "val_declaration" | "var_declaration" => {
                    values += 1;
                    if values <= FIELD_TRUNCATE_THRESHOLD {
                        entry
                            .item_mut()
                            .children
                            .push(ranged(value_text(child, context)?, context.range(child)));
                    }
                }
                _ => {}
            }
        }
        if values > FIELD_TRUNCATE_THRESHOLD {
            entry
                .item_mut()
                .children
                .push(truncated_message(values).into());
        }
    }
    Ok(Some(entry))
}

fn extract_function(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    Ok(function_signature(node, context)?
        .map(|signature| Entry::item(Section::Function, node, signature)))
}

fn extract_type(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let value = context
        .field(node, "type")?
        .map_or(String::new(), |value| format!("= {}", context.text(value)));
    Ok(Some(Entry::item(
        Section::Type,
        node,
        compact_whitespace(&prefixed(
            &modifiers(node, context)?,
            format!(
                "type {}{} {value}",
                context.text(name),
                type_parameters(node, context)?
            ),
        )),
    )))
}

fn extract_enum(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let extension = context
        .field(node, "extend")?
        .map_or(String::new(), |extension| {
            format!(" {}", context.text(extension))
        });
    let mut entry = Entry::item(
        Section::Type,
        node,
        compact_whitespace(&prefixed(
            &modifiers(node, context)?,
            format!(
                "{}{}{extension}",
                context.text(name),
                type_parameters(node, context)?
            ),
        )),
    );
    if let Some(body) = context.child(node, "enum_body")? {
        for group in context.children(body)? {
            if group.kind() == "enum_case_definitions" {
                for case in context.children(group)? {
                    if matches!(case.kind(), "simple_enum_case" | "full_enum_case")
                        && let Some(name) = context.field(case, "name")?
                    {
                        entry
                            .item_mut()
                            .children
                            .push(context.text(name).to_owned().into());
                    }
                }
            }
        }
        entry.item_mut().child_kind = ChildKind::Brief;
    }
    Ok(Some(entry))
}

fn extract_import(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    let text = context.text(node);
    let cleaned = text
        .strip_prefix("import ")
        .unwrap_or(text)
        .trim_end_matches(';')
        .trim();
    Ok(Entry::import(
        node,
        expand_import(cleaned, ".", context)?,
        None,
    ))
}

fn extract_package(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    let name = context
        .field(node, "name")?
        .map_or_else(
            || {
                context
                    .text(node)
                    .strip_prefix("package ")
                    .unwrap_or(context.text(node))
                    .trim()
            },
            |name| context.text(name),
        )
        .to_owned();
    Ok(Entry::item(Section::Module, node, name))
}
