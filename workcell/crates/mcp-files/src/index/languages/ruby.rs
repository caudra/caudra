use tree_sitter::Node;

use super::common::{ExtractResult, LanguageSpec, split_path, strip_delimited};
use crate::index::{
    model::{Entry, Section},
    render::{compact_whitespace, ranged, truncate},
    traversal::Context,
};

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new("/", extract_nodes);
    spec.is_doc_comment = Some(|node, _| node.kind() == "comment");
    spec
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    match node.kind() {
        "call" => Ok(extract_require(node, context)?.into_iter().collect()),
        "class" => Ok(extract_class(node, context)?.into_iter().collect()),
        "module" => extract_module(node, context),
        "method" => Ok(method_signature(node, context)?
            .map(|signature| Entry::item(Section::Function, node, signature))
            .into_iter()
            .collect()),
        "singleton_method" => Ok(method_signature(node, context)?
            .map(|signature| Entry::item(Section::Function, node, format!("self.{signature}")))
            .into_iter()
            .collect()),
        "assignment" => Ok(extract_assignment(node, context)?.into_iter().collect()),
        _ => {
            let _ = attrs;
            Ok(Vec::new())
        }
    }
}

fn extract_require(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(method) = context.field(node, "method")? else {
        return Ok(None);
    };
    if !matches!(context.text(method), "require" | "require_relative") {
        return Ok(None);
    }
    let Some(arguments) = context.child(node, "argument_list")? else {
        return Ok(None);
    };
    let Some(string) = context.child(arguments, "string")? else {
        return Ok(None);
    };
    let raw_path = context.text(string);
    let Some(path) = strip_delimited(raw_path, "\"").or_else(|| strip_delimited(raw_path, "'"))
    else {
        return Ok(None);
    };
    if path.is_empty() {
        return Ok(None);
    }
    Ok(Some(Entry::import(node, vec![split_path(path, '/')], None)))
}

fn method_signature(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<String>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let parameters = context
        .field(node, "parameters")?
        .map_or("()", |parameters| context.text(parameters));
    Ok(Some(compact_whitespace(&format!(
        "{}{parameters}",
        context.text(name)
    ))))
}

fn extract_class(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let mut label = context.text(name).to_owned();
    if let Some(superclass) = context.field(node, "superclass")? {
        label.push_str(" < ");
        label.push_str(context.text(superclass).trim_start_matches('<').trim());
    }
    let mut entry = Entry::item(Section::Class, node, label);
    if let Some(body) = context.field(node, "body")? {
        for child in context.children(body)? {
            let signature = match child.kind() {
                "method" => method_signature(child, context)?,
                "singleton_method" => {
                    method_signature(child, context)?.map(|signature| format!("self.{signature}"))
                }
                _ => None,
            };
            if let Some(signature) = signature {
                entry
                    .item_mut()
                    .children
                    .push(ranged(signature, context.range(child)));
            }
        }
    }
    Ok(Some(entry))
}

fn extract_module(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Vec<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(Vec::new());
    };
    let mut entries = vec![Entry::item(Section::Module, node, context.text(name))];
    if let Some(body) = context.field(node, "body")? {
        for child in context.children(body)? {
            entries.extend(extract_nodes(child, context, &[])?);
        }
    }
    Ok(entries)
}

fn extract_assignment(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(left) = context.field(node, "left")? else {
        return Ok(None);
    };
    let name = context.text(left);
    if name
        .bytes()
        .next()
        .is_none_or(|byte| !byte.is_ascii_uppercase())
    {
        return Ok(None);
    }
    let value = context
        .field(node, "right")?
        .map_or(String::new(), |right| {
            format!(" = {}", truncate(context.text(right), 60))
        });
    Ok(Some(Entry::item(
        Section::Constant,
        node,
        format!("{name}{value}"),
    )))
}
