use tree_sitter::Node;

use super::common::{ExtractResult, LanguageSpec, split_path};
use crate::index::{
    model::{ChildKind, Entry, Section},
    render::{FIELD_TRUNCATE_THRESHOLD, compact_whitespace, prefixed, ranged, truncated_message},
    traversal::Context,
};

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new(".", extract_nodes);
    spec.is_doc_comment = Some(|node, context| {
        matches!(node.kind(), "comment" | "multiline_comment")
            && (context.text(node).starts_with("///") || context.text(node).starts_with("/**"))
    });
    spec
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    let entry = match node.kind() {
        "import_declaration" => Some(extract_import(node, context)),
        "class_declaration" => extract_class_declaration(node, context)?,
        "function_declaration" => Some(Entry::item(
            Section::Function,
            node,
            function_signature(node, context)?,
        )),
        "property_declaration" => extract_property(node, context)?,
        "typealias_declaration" => extract_alias(node, context)?,
        "protocol_declaration" => extract_protocol(node, context)?,
        _ => None,
    };
    Ok(entry.into_iter().collect())
}

fn modifiers(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let Some(modifiers) = context.child(node, "modifiers")? else {
        return Ok(String::new());
    };
    let mut parts = Vec::new();
    for child in context.children(modifiers)? {
        match child.kind() {
            "visibility_modifier" => parts.push(
                context
                    .text(child)
                    .split_whitespace()
                    .next()
                    .unwrap_or_default(),
            ),
            "function_modifier" | "member_modifier" | "mutation_modifier" => {
                parts.push(context.text(child));
            }
            _ => {}
        }
    }
    Ok(parts.join(" "))
}

fn parameters(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let mut parameters = Vec::new();
    for child in context.children(node)? {
        if child.kind() == "parameter" {
            let external = context
                .field(child, "external_name")?
                .map_or(String::new(), |external| {
                    format!("{} ", context.text(external))
                });
            let name = context
                .field(child, "name")?
                .map_or("_", |name| context.text(name));
            let field_type = context
                .field(child, "type")?
                .map_or(String::new(), |field_type| {
                    format!(": {}", context.text(field_type))
                });
            parameters.push(format!("{external}{name}{field_type}"));
        }
    }
    Ok(format!("({})", parameters.join(", ")))
}

fn function_signature(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let keyword = context
        .children(node)?
        .into_iter()
        .find(|child| matches!(child.kind(), "func" | "init"))
        .map_or("func", |child| child.kind());
    let name = context
        .field(node, "name")?
        .map_or("", |name| context.text(name));
    let parameters =
        if let Some(parameters_node) = context.child(node, "function_value_parameters")? {
            parameters(parameters_node, context)?
        } else {
            "()".into()
        };
    let throws = context
        .child(node, "throws")?
        .map_or(String::new(), |throws| format!(" {}", context.text(throws)));
    let return_type = context
        .field(node, "return_type")?
        .map_or(String::new(), |return_type| {
            format!(" -> {}", context.text(return_type))
        });
    Ok(compact_whitespace(&prefixed(
        &modifiers(node, context)?,
        format!("{keyword} {name}{parameters}{throws}{return_type}"),
    )))
}

fn property_text(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let keyword = context
        .children(node)?
        .into_iter()
        .find(|child| matches!(child.kind(), "let" | "var"))
        .map_or("var", |child| child.kind());
    let name = context
        .field(node, "name")?
        .or(context.child(node, "bound_identifier")?)
        .map_or("_", |name| context.text(name));
    let field_type = context
        .child(node, "type_annotation")?
        .map_or(String::new(), |field_type| {
            format!(" {}", context.text(field_type))
        });
    Ok(compact_whitespace(&prefixed(
        &modifiers(node, context)?,
        format!("{keyword} {name}{field_type}"),
    )))
}

fn inheritance(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let Some(inheritance) = context.child(node, "inheritance_specifier")? else {
        return Ok(String::new());
    };
    let parts = context
        .children(inheritance)?
        .into_iter()
        .filter(|child| !matches!(child.kind(), "," | ":"))
        .map(|child| context.text(child).trim())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    Ok(if parts.is_empty() {
        String::new()
    } else {
        format!(": {}", parts.join(", "))
    })
}

fn class_members(
    node: Node<'_>,
    context: &Context<'_>,
) -> ExtractResult<Vec<crate::index::model::Child>> {
    let mut output = Vec::new();
    let mut fields = 0usize;
    for child in context.children(node)? {
        if matches!(child.kind(), "function_declaration" | "init_declaration") {
            output.push(ranged(
                function_signature(child, context)?,
                context.range(child),
            ));
        } else if child.kind() == "property_declaration" {
            fields += 1;
            if fields <= FIELD_TRUNCATE_THRESHOLD {
                output.push(ranged(property_text(child, context)?, context.range(child)));
            }
        }
    }
    if fields > FIELD_TRUNCATE_THRESHOLD {
        output.push(truncated_message(fields).into());
    }
    Ok(output)
}

fn extract_class(
    node: Node<'_>,
    context: &Context<'_>,
    section: Section,
    keyword: &str,
) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let mut entry = Entry::item(
        section,
        node,
        compact_whitespace(&prefixed(
            &modifiers(node, context)?,
            format!(
                "{keyword} {}{}",
                context.text(name),
                inheritance(node, context)?
            ),
        )),
    );
    if let Some(body) = context.child(node, "class_body")? {
        entry.item_mut().children = class_members(body, context)?;
    }
    Ok(Some(entry))
}

fn extract_enum(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let mut entry = Entry::item(
        Section::Type,
        node,
        compact_whitespace(&prefixed(
            &modifiers(node, context)?,
            format!("enum {}{}", context.text(name), inheritance(node, context)?),
        )),
    );
    if let Some(body) = context.child(node, "enum_class_body")? {
        for child in context.children(body)? {
            match child.kind() {
                "enum_entry" => {
                    let cases = context
                        .children(child)?
                        .into_iter()
                        .filter(|case| case.kind() == "simple_identifier")
                        .map(|case| format!("case {}", context.text(case)))
                        .collect::<Vec<_>>();
                    if !cases.is_empty() {
                        entry
                            .item_mut()
                            .children
                            .push(ranged(cases.join(", "), context.range(child)));
                    }
                }
                "function_declaration" => entry.item_mut().children.push(ranged(
                    function_signature(child, context)?,
                    context.range(child),
                )),
                _ => {}
            }
        }
        entry.item_mut().child_kind = ChildKind::Brief;
    }
    Ok(Some(entry))
}

fn extract_import(node: Node<'_>, context: &Context<'_>) -> Entry {
    let text = context.text(node);
    let cleaned = text.strip_prefix("import ").unwrap_or(text).trim();
    let last = cleaned.split_whitespace().last().unwrap_or(cleaned);
    Entry::import(node, vec![split_path(last, '.')], None)
}

fn extract_property(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    if !context
        .children(node)?
        .iter()
        .any(|child| child.kind() == "let")
    {
        return Ok(None);
    }
    Ok(Some(Entry::item(
        Section::Constant,
        node,
        property_text(node, context)?,
    )))
}

fn extract_alias(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let value = context
        .field(node, "value")?
        .map_or(String::new(), |value| format!(" = {}", context.text(value)));
    Ok(Some(Entry::item(
        Section::Type,
        node,
        compact_whitespace(&prefixed(
            &modifiers(node, context)?,
            format!("typealias {}{value}", context.text(name)),
        )),
    )))
}

fn extract_protocol(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let mut entry = Entry::item(
        Section::Trait,
        node,
        compact_whitespace(&prefixed(
            &modifiers(node, context)?,
            format!(
                "protocol {}{}",
                context.text(name),
                inheritance(node, context)?
            ),
        )),
    );
    if let Some(body) = context.child(node, "protocol_body")? {
        let mut fields = 0usize;
        for child in context.children(body)? {
            match child.kind() {
                "protocol_function_declaration" => entry.item_mut().children.push(ranged(
                    function_signature(child, context)?,
                    context.range(child),
                )),
                "protocol_property_declaration" => {
                    fields += 1;
                    if fields <= FIELD_TRUNCATE_THRESHOLD {
                        let name = context
                            .child(child, "simple_identifier")?
                            .or(context.field(child, "name")?)
                            .map_or("_", |name| context.text(name));
                        let field_type = context
                            .child(child, "type_annotation")?
                            .map_or(String::new(), |field_type| {
                                format!(" {}", context.text(field_type))
                            });
                        entry.item_mut().children.push(ranged(
                            compact_whitespace(&format!("var {name}{field_type}")),
                            context.range(child),
                        ));
                    }
                }
                _ => {}
            }
        }
        if fields > FIELD_TRUNCATE_THRESHOLD {
            entry
                .item_mut()
                .children
                .push(truncated_message(fields).into());
        }
    }
    Ok(Some(entry))
}

fn extract_class_declaration(
    node: Node<'_>,
    context: &Context<'_>,
) -> ExtractResult<Option<Entry>> {
    match context
        .field(node, "declaration_kind")?
        .map_or("", |kind| context.text(kind))
    {
        kind @ ("class" | "actor") => extract_class(node, context, Section::Class, kind),
        "struct" => extract_class(node, context, Section::Type, "struct"),
        "enum" => extract_enum(node, context),
        "extension" => extract_class(node, context, Section::Impl, "extension"),
        _ => Ok(None),
    }
}
