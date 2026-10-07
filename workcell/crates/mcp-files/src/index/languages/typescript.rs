use tree_sitter::Node;

use super::common::{ExtractResult, LanguageSpec};
use crate::index::{
    model::{Entry, Section},
    render::{FIELD_TRUNCATE_THRESHOLD, compact_whitespace, ranged, truncate, truncated_message},
    traversal::Context,
};

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new("/", extract_nodes);
    spec.is_doc_comment = Some(is_doc_comment);
    spec
}

fn is_doc_comment(node: Node<'_>, context: &Context<'_>) -> bool {
    node.kind() == "comment" && context.text(node).starts_with("/**")
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    let entry = match node.kind() {
        "import_statement" => Some(extract_import(node, context)),
        "class_declaration" => extract_class(node, context)?,
        "function_declaration" => extract_function(node, context)?,
        "interface_declaration" => extract_interface(node, context)?,
        "type_alias_declaration" => extract_type_alias(node, context)?,
        "enum_declaration" => extract_enum(node, context)?,
        "lexical_declaration" => extract_const(node, context)?,
        "export_statement" => extract_export(node, context)?,
        _ => None,
    };
    Ok(entry.into_iter().collect())
}

fn exported(node: Node<'_>) -> bool {
    node.parent()
        .is_some_and(|parent| parent.kind() == "export_statement")
}

fn export_prefix(node: Node<'_>) -> &'static str {
    if exported(node) { "export " } else { "" }
}

fn extract_import(node: Node<'_>, context: &Context<'_>) -> Entry {
    let text = context.text(node);
    let cleaned = text
        .strip_prefix("import ")
        .unwrap_or(text)
        .trim_end_matches(';');
    Entry::import(node, vec![vec![cleaned.to_owned()]], None)
}

fn member_signature(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let name = context
        .field(node, "name")?
        .map_or("_", |name| context.text(name));
    let parameters = context
        .field(node, "parameters")?
        .map_or("", |parameters| context.text(parameters));
    let return_type = context
        .field(node, "return_type")?
        .map_or(String::new(), |return_type| {
            let text = context.text(return_type);
            if text.starts_with(':') {
                text.to_owned()
            } else {
                format!(": {text}")
            }
        });
    Ok(format!("{name}{parameters}{return_type}"))
}

fn extract_class(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let (Some(name), Some(body)) = (context.field(node, "name")?, context.field(node, "body")?)
    else {
        return Ok(None);
    };
    let mut entry = Entry::item(
        Section::Class,
        node,
        format!("{}{}", export_prefix(node), context.text(name)),
    );
    let mut fields = 0usize;
    for child in context.children(body)? {
        match child.kind() {
            "method_definition" => {
                let signature = member_signature(child, context)?;
                entry
                    .item_mut()
                    .children
                    .push(ranged(signature, context.range(child)));
            }
            "public_field_definition" | "property_definition" => {
                fields += 1;
                if fields <= FIELD_TRUNCATE_THRESHOLD {
                    let signature = member_signature(child, context)?;
                    entry
                        .item_mut()
                        .children
                        .push(ranged(signature, context.range(child)));
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
    Ok(Some(entry))
}

fn extract_function(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let parameters = context
        .field(node, "parameters")?
        .map_or("()", |parameters| context.text(parameters));
    let return_type = context
        .field(node, "return_type")?
        .map_or(String::new(), |return_type| {
            let text = context.text(return_type);
            if text.starts_with(':') {
                text.to_owned()
            } else {
                format!(": {text}")
            }
        });
    Ok(Some(Entry::item(
        Section::Function,
        node,
        compact_whitespace(&format!(
            "{}{}{parameters}{return_type}",
            export_prefix(node),
            context.text(name)
        )),
    )))
}

fn extract_interface(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let (Some(name), Some(body)) = (context.field(node, "name")?, context.field(node, "body")?)
    else {
        return Ok(None);
    };
    let mut entry = Entry::item(
        Section::Type,
        node,
        format!("{}interface {}", export_prefix(node), context.text(name)),
    );
    for child in context.children(body)? {
        if matches!(child.kind(), "property_signature" | "method_signature") {
            entry.item_mut().children.push(
                context
                    .text(child)
                    .trim_end_matches([',', ';'])
                    .to_owned()
                    .into(),
            );
        }
    }
    Ok(Some(entry))
}

fn extract_type_alias(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let value = context
        .field(node, "value")?
        .map_or(String::new(), |value| {
            format!(" = {}", truncate(context.text(value), 80))
        });
    Ok(Some(Entry::item(
        Section::Type,
        node,
        format!("{}type {}{value}", export_prefix(node), context.text(name)),
    )))
}

fn extract_enum(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    Ok(context.field(node, "name")?.map(|name| {
        Entry::item(
            Section::Type,
            node,
            format!("{}enum {}", export_prefix(node), context.text(name)),
        )
    }))
}

fn extract_const(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    if node
        .child(0)
        .is_none_or(|child| context.text(child) != "const")
    {
        return Ok(None);
    }
    let Some(declaration) = context.child(node, "variable_declarator")? else {
        return Ok(None);
    };
    let Some(name) = context.field(declaration, "name")? else {
        return Ok(None);
    };
    let type_text = context
        .field(declaration, "type")?
        .map_or(String::new(), |value| {
            let text = context.text(value);
            if text.starts_with(':') {
                text.to_owned()
            } else {
                format!(": {text}")
            }
        });
    let value = context
        .field(declaration, "value")?
        .map_or(String::new(), |value| {
            format!(" = {}", truncate(context.text(value), 60))
        });
    Ok(Some(Entry::item(
        Section::Constant,
        node,
        format!(
            "{}{}{type_text}{value}",
            export_prefix(node),
            context.text(name)
        ),
    )))
}

fn extract_export(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    for child in context.children(node)? {
        let entry = match child.kind() {
            "class_declaration" => extract_class(child, context)?,
            "function_declaration" => extract_function(child, context)?,
            "interface_declaration" => extract_interface(child, context)?,
            "type_alias_declaration" => extract_type_alias(child, context)?,
            "lexical_declaration" => extract_const(child, context)?,
            "enum_declaration" => extract_enum(child, context)?,
            _ => None,
        };
        if entry.is_some() {
            return Ok(entry);
        }
    }
    Ok(None)
}
