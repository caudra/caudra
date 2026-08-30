use tree_sitter::Node;

use super::common::{
    ExtractResult, simple_import, strip_keyword_whitespace, trim_ascii_whitespace,
};
use crate::index::{
    model::{Entry, ParsedSkeleton, Section},
    render::{compact_whitespace, format_skeleton, truncate},
    traversal::Context,
};

const LABEL_TRUNCATE: usize = 80;

pub(super) fn extract(root: Node<'_>, context: &Context<'_>) -> ExtractResult<ParsedSkeleton> {
    let mut entries = Vec::new();
    for child in context.children(root)? {
        if child.kind() == "import_statement" {
            let text = trim_ascii_whitespace(context.text(child));
            let cleaned = trim_ascii_whitespace(strip_keyword_whitespace(text, "@import"));
            let cleaned = trim_ascii_whitespace(cleaned.trim_end_matches(';'));
            entries.push(simple_import(child, cleaned, '/'));
        } else if let Some(entry) = top_entry(child, context)? {
            entries.push(entry);
        }
    }
    format_skeleton(&entries, &[], None, "/", context)
}

fn rule_entry(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let text = if node.kind() == "rule_set" {
        context
            .child(node, "selectors")?
            .map_or(context.text(node), |selectors| context.text(selectors))
    } else if matches!(
        node.kind(),
        "at_rule"
            | "keyframes_statement"
            | "media_statement"
            | "scope_statement"
            | "supports_statement"
    ) {
        context
            .text(node)
            .split_once('{')
            .map_or(context.text(node), |(label, _)| label)
    } else {
        return Ok(None);
    };
    Ok(Some(Entry::item(
        Section::Rule,
        node,
        truncate(
            trim_ascii_whitespace(&compact_whitespace(text)),
            LABEL_TRUNCATE,
        ),
    )))
}

fn top_entry(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(mut entry) = rule_entry(node, context)? else {
        return Ok(None);
    };
    if let Some(block) = context.child(node, "block")? {
        for child in context.children(block)? {
            if let Some(nested) = rule_entry(child, context)? {
                entry.item_mut().children.push(nested.into());
            }
        }
    }
    Ok(Some(entry))
}
