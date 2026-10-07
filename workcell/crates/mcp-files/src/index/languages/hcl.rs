use tree_sitter::Node;

use super::common::ExtractResult;
use crate::index::{
    model::{Entry, ParsedSkeleton, Section},
    render::{compact_whitespace, format_skeleton, truncate},
    traversal::Context,
};

const LABEL_TRUNCATE: usize = 80;

pub(super) fn extract(root: Node<'_>, context: &Context<'_>) -> ExtractResult<ParsedSkeleton> {
    let mut entries = Vec::new();
    if let Some(body) = context.child(root, "body")? {
        for child in context.children(body)? {
            match child.kind() {
                "block" => entries.push(top_entry(child, context)?),
                "attribute" => entries.push(Entry::item(
                    Section::Constant,
                    child,
                    truncate(&compact_whitespace(context.text(child)), LABEL_TRUNCATE),
                )),
                _ => {}
            }
        }
    }
    format_skeleton(&entries, &[], None, "", context)
}

fn block_entry(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    let parts = context
        .children(node)?
        .into_iter()
        .filter(|child| matches!(child.kind(), "identifier" | "string_lit"))
        .map(|child| compact_whitespace(context.text(child)))
        .collect::<Vec<_>>();
    Ok(Entry::item(
        Section::Block,
        node,
        truncate(&parts.join(" "), LABEL_TRUNCATE),
    ))
}

fn top_entry(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    let mut entry = block_entry(node, context)?;
    if let Some(body) = context.child(node, "body")? {
        for child in context.children(body)? {
            if child.kind() == "block" {
                entry
                    .item_mut()
                    .children
                    .push(block_entry(child, context)?.into());
            } else if let Some(name) = context.child(child, "identifier")? {
                entry
                    .item_mut()
                    .children
                    .push(Entry::item(Section::Block, child, context.text(name)).into());
            }
        }
    }
    Ok(entry)
}
