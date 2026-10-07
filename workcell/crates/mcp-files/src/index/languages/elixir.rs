use tree_sitter::Node;

use super::common::{ExtractResult, LanguageSpec, split_path};
use crate::index::{
    model::{Entry, Section},
    render::{compact_whitespace, ranged},
    traversal::Context,
};

const IMPORT_KEYWORDS: &[&str] = &["alias", "import", "require", "use"];
const DOC_ATTRS: &[&str] = &["doc", "moduledoc", "typedoc", "spec"];

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new(".", extract_nodes);
    spec.is_doc_comment = Some(is_doc_comment);
    spec
}

fn is_doc_comment(node: Node<'_>, context: &Context<'_>) -> bool {
    node.kind() == "comment"
        || (node.kind() == "unary_operator"
            && attribute_name(node, context).is_some_and(|name| DOC_ATTRS.contains(&name)))
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    match node.kind() {
        "call" => {
            if let Some(import) = extract_import(node, context)? {
                return Ok(vec![import]);
            }
            if let Some((module, mut imports)) = extract_module(node, context)? {
                imports.push(module);
                return Ok(imports);
            }
            Ok(extract_function(node, context)?.into_iter().collect())
        }
        "unary_operator" => Ok(extract_attribute(node, context)?.into_iter().collect()),
        _ => Ok(Vec::new()),
    }
}

fn call_target<'a>(node: Node<'_>, context: &'a Context<'_>) -> Option<&'a str> {
    (node.kind() == "call")
        .then(|| node.child_by_field_name("target"))
        .flatten()
        .map(|target| context.text(target))
}

fn first_argument<'tree>(
    node: Node<'tree>,
    context: &Context<'_>,
) -> ExtractResult<Option<Node<'tree>>> {
    let Some(arguments) = context.child(node, "arguments")? else {
        return Ok(None);
    };
    Ok(context
        .children(arguments)?
        .into_iter()
        .find(|child| !matches!(child.kind(), "," | "(" | ")")))
}

fn extract_import(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(target) = call_target(node, context) else {
        return Ok(None);
    };
    if !IMPORT_KEYWORDS.contains(&target) {
        return Ok(None);
    }
    let Some(argument) = first_argument(node, context)? else {
        return Ok(None);
    };
    Ok(Some(Entry::import(
        node,
        vec![split_path(context.text(argument), '.')],
        Some(target.to_owned()),
    )))
}

fn function_signature(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<String>> {
    let Some(argument) = first_argument(node, context)? else {
        return Ok(None);
    };
    Ok(
        matches!(argument.kind(), "call" | "identifier" | "binary_operator")
            .then(|| context.text(argument).to_owned()),
    )
}

fn extract_function(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(target @ ("def" | "defp")) = call_target(node, context) else {
        return Ok(None);
    };
    Ok(function_signature(node, context)?.map(|signature| {
        Entry::item(
            Section::Function,
            node,
            compact_whitespace(&format!("{target} {signature}")),
        )
    }))
}

fn attribute_name<'a>(node: Node<'_>, context: &'a Context<'_>) -> Option<&'a str> {
    if node.kind() != "unary_operator"
        || node
            .child_by_field_name("operator")
            .is_none_or(|operator| context.text(operator) != "@")
    {
        return None;
    }
    let operand = node.child_by_field_name("operand")?;
    match operand.kind() {
        "identifier" | "alias" => Some(context.text(operand)),
        "call" => call_target(operand, context),
        _ => None,
    }
}

fn extract_attribute(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = attribute_name(node, context) else {
        return Ok(None);
    };
    if DOC_ATTRS.contains(&name)
        || name
            .bytes()
            .next()
            .is_none_or(|byte| !byte.is_ascii_uppercase())
    {
        return Ok(None);
    }
    Ok(Some(Entry::item(
        Section::Constant,
        node,
        format!("@{name}"),
    )))
}

fn extract_module(
    node: Node<'_>,
    context: &Context<'_>,
) -> ExtractResult<Option<(Entry, Vec<Entry>)>> {
    if call_target(node, context) != Some("defmodule") {
        return Ok(None);
    }
    let Some(name) = first_argument(node, context)? else {
        return Ok(None);
    };
    let mut entry = Entry::item(
        Section::Class,
        node,
        format!("defmodule {}", context.text(name)),
    );
    let mut imports = Vec::new();
    if let Some(block) = context.child(node, "do_block")? {
        for child in context.children(block)? {
            if child.kind() != "call" {
                continue;
            }
            if let Some(import) = extract_import(child, context)? {
                imports.push(import);
            } else if let Some(target @ ("def" | "defp")) = call_target(child, context)
                && let Some(signature) = function_signature(child, context)?
            {
                entry.item_mut().children.push(ranged(
                    compact_whitespace(&format!("{target} {signature}")),
                    context.range(child),
                ));
            }
        }
    }
    Ok(Some((entry, imports)))
}
