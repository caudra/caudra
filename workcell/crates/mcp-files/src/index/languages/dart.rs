use tree_sitter::Node;

use super::common::{ExtractResult, LanguageSpec};
use crate::index::{
    model::{Entry, Section},
    render::{compact_whitespace, ranged},
    traversal::Context,
};

const SIGNATURES: &[&str] = &[
    "function_signature",
    "getter_signature",
    "setter_signature",
    "constructor_signature",
    "constant_constructor_signature",
    "factory_constructor_signature",
    "redirecting_factory_constructor_signature",
    "operator_signature",
];

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new(".", extract_nodes);
    spec.is_doc_comment =
        Some(|node, context| node.kind() == "comment" && context.text(node).starts_with("///"));
    spec
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    let entry = match node.kind() {
        "class_declaration" => extract_class(node, context, "class")?,
        "mixin_declaration" => extract_class(node, context, "mixin")?,
        "extension_type_declaration" => extract_class(node, context, "extension type")?,
        "extension_declaration" => extract_extension(node, context)?,
        "enum_declaration" => context
            .field(node, "name")?
            .map(|name| Entry::item(Section::Type, node, format!("enum {}", context.text(name)))),
        "function_declaration"
        | "external_function_declaration"
        | "getter_declaration"
        | "external_getter_declaration"
        | "setter_declaration"
        | "external_setter_declaration" => extract_function(node, context)?,
        _ => None,
    };
    Ok(entry.into_iter().collect())
}

fn type_parameters<'a>(node: Node<'_>, context: &'a Context<'a>) -> ExtractResult<&'a str> {
    Ok(context
        .field(node, "type_parameters")?
        .map_or("", |parameters| context.text(parameters)))
}

fn signature(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<String>> {
    if node.kind() == "operator_signature" {
        return Ok(Some(context.text(node).to_owned()));
    }
    let names = context.fields(node, "name")?;
    if names.is_empty() {
        return Ok(None);
    }
    let name = names
        .into_iter()
        .map(|name| context.text(name))
        .collect::<String>();
    let parameters = context
        .child(node, "formal_parameter_list")?
        .map_or("()", |parameters| context.text(parameters));
    let type_parameters = context
        .child(node, "type_parameters")?
        .map_or("", |parameters| context.text(parameters));
    let return_type = context
        .field(node, "return_type")?
        .map(|return_type| context.text(return_type));
    Ok(Some(match node.kind() {
        "getter_signature" => compact_whitespace(&format!(
            "get {name}{}",
            return_type.map_or(String::new(), |value| format!(" {value}"))
        )),
        "setter_signature" => compact_whitespace(&format!("set {name}{parameters}")),
        _ if return_type == Some("set") => compact_whitespace(&format!("set {name}{parameters}")),
        _ if return_type == Some("get") && parameters == "()" => {
            compact_whitespace(&format!("get {name}"))
        }
        _ => compact_whitespace(&format!(
            "{name}{type_parameters}{parameters}{}",
            return_type.map_or(String::new(), |value| format!(" {value}"))
        )),
    }))
}

fn find_signature<'tree>(
    node: Node<'tree>,
    context: &Context<'_>,
) -> ExtractResult<Option<Node<'tree>>> {
    let container = if node.kind() == "method_declaration" {
        let Some(signature) = context.field(node, "signature")? else {
            return Ok(None);
        };
        signature
    } else {
        node
    };
    Ok(context
        .children(container)?
        .into_iter()
        .find(|child| SIGNATURES.contains(&child.kind())))
}

fn unwrap_member<'tree>(
    node: Node<'tree>,
    context: &Context<'_>,
) -> ExtractResult<Option<Node<'tree>>> {
    if node.kind() != "class_member" {
        return Ok(Some(node));
    }
    Ok(context
        .children(node)?
        .into_iter()
        .find(|child| matches!(child.kind(), "method_declaration" | "declaration")))
}

fn body_members(
    node: Node<'_>,
    context: &Context<'_>,
) -> ExtractResult<Vec<crate::index::model::Child>> {
    let mut output = Vec::new();
    for member in context.children(node)? {
        let Some(member) = unwrap_member(member, context)? else {
            continue;
        };
        if matches!(member.kind(), "method_declaration" | "declaration")
            && let Some(signature_node) = find_signature(member, context)?
            && let Some(text) = signature(signature_node, context)?
        {
            output.push(ranged(text, context.range(member)));
        } else if member.kind() == "declaration" {
            extract_fields(member, context, &mut output)?;
        }
    }
    Ok(output)
}

fn extract_fields(
    node: Node<'_>,
    context: &Context<'_>,
    output: &mut Vec<crate::index::model::Child>,
) -> ExtractResult<()> {
    let field_type = context.child(node, "type")?;
    for child in context.children(node)? {
        let item_kind = match child.kind() {
            "initialized_identifier_list" => Some("initialized_identifier"),
            "identifier_list" => Some("identifier"),
            "static_final_declaration_list" => Some("static_final_declaration"),
            _ => None,
        };
        if let Some(item_kind) = item_kind {
            for item in context.children(child)? {
                if item.kind() == item_kind {
                    add_field(item, field_type, context, output)?;
                }
            }
        } else if matches!(
            child.kind(),
            "initialized_identifier" | "static_final_declaration" | "identifier"
        ) {
            add_field(child, field_type, context, output)?;
        }
    }
    Ok(())
}

fn add_field(
    node: Node<'_>,
    field_type: Option<Node<'_>>,
    context: &Context<'_>,
    output: &mut Vec<crate::index::model::Child>,
) -> ExtractResult<()> {
    let name = if node.kind() == "identifier" {
        Some(node)
    } else {
        context.field(node, "name")?
    };
    if let Some(name) = name {
        let text = field_type.map_or_else(
            || context.text(name).to_owned(),
            |field_type| format!("{} {}", context.text(name), context.text(field_type)),
        );
        output.push(ranged(text, context.range(node)));
    }
    Ok(())
}

fn extract_class(
    node: Node<'_>,
    context: &Context<'_>,
    prefix: &str,
) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let mut entry = Entry::item(
        Section::Class,
        node,
        format!(
            "{prefix} {}{}",
            context.text(name),
            type_parameters(node, context)?
        ),
    );
    if let Some(body) = context.field(node, "body")? {
        entry.item_mut().children = body_members(body, context)?;
    }
    Ok(Some(entry))
}

fn extract_extension(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(body) = context.field(node, "body")? else {
        return Ok(None);
    };
    let name = context
        .field(node, "name")?
        .map_or("_", |name| context.text(name));
    let mut entry = Entry::item(Section::Type, node, format!("extension {name}"));
    entry.item_mut().children = body_members(body, context)?;
    Ok(Some(entry))
}

fn extract_function(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(signature_node) = context.field(node, "signature")? else {
        return Ok(None);
    };
    Ok(signature(signature_node, context)?
        .map(|signature| Entry::item(Section::Function, node, signature)))
}
