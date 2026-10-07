use tree_sitter::Node;

use super::common::ExtractResult;
use crate::index::{
    model::{Entry, ParsedSkeleton, Section},
    render::{FIELD_TRUNCATE_THRESHOLD, format_skeleton, truncated_message},
    traversal::Context,
};

pub(super) fn extract(root: Node<'_>, context: &Context<'_>) -> ExtractResult<ParsedSkeleton> {
    let mut pairs = Vec::new();
    for child in context.children(root)? {
        collect_pairs(child, context, &mut pairs)?;
    }
    let mut entries = Vec::new();
    for pair in pairs {
        if let Some(entry) = top_entry(pair, context)? {
            entries.push(entry);
        }
    }
    format_skeleton(&entries, &[], None, "", context)
}

fn collect_pairs<'tree>(
    node: Node<'tree>,
    context: &Context<'_>,
    output: &mut Vec<Node<'tree>>,
) -> ExtractResult<()> {
    match node.kind() {
        "object" => {
            output.extend(
                context
                    .children(node)?
                    .into_iter()
                    .filter(|child| child.kind() == "pair"),
            );
        }
        "array" => {
            for child in context.children(node)? {
                collect_pairs(child, context, output)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn key_entry(pair: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    Ok(context
        .field(pair, "key")?
        .map(|key| Entry::item(Section::Constant, pair, context.text(key))))
}

fn top_entry(pair: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(mut entry) = key_entry(pair, context)? else {
        return Ok(None);
    };
    let Some(value) = context.field(pair, "value")? else {
        return Ok(Some(entry));
    };
    let mut nested = Vec::new();
    collect_pairs(value, context, &mut nested)?;
    let mut total = 0usize;
    for pair in nested {
        if let Some(child) = key_entry(pair, context)? {
            total += 1;
            if total <= FIELD_TRUNCATE_THRESHOLD {
                entry.item_mut().children.push(child.into());
            }
        }
    }
    if total > FIELD_TRUNCATE_THRESHOLD {
        entry
            .item_mut()
            .children
            .push(truncated_message(total).into());
    }
    Ok(Some(entry))
}
