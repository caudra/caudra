use tree_sitter::Node;

use super::common::{ExtractResult, strip_delimited, trim_ascii_whitespace};
use crate::index::{
    model::{Entry, ParsedSkeleton, Section},
    render::{compact_whitespace, format_skeleton},
    traversal::Context,
};

pub(super) fn extract(root: Node<'_>, context: &Context<'_>) -> ExtractResult<ParsedSkeleton> {
    let mut pairs = Vec::new();
    for child in context.children(root)? {
        if child.kind() == "document" {
            for document_child in context.children(child)? {
                collect_pairs(document_child, context, &mut pairs)?;
            }
        } else {
            collect_pairs(child, context, &mut pairs)?;
        }
    }
    let mut entries = Vec::new();
    for pair in pairs {
        if let Some(entry) = pair_entry(pair, context, true)? {
            entries.push(entry);
        }
    }
    format_skeleton(&entries, &[], None, ".", context)
}

fn collect_pairs<'tree>(
    node: Node<'tree>,
    context: &Context<'_>,
    output: &mut Vec<Node<'tree>>,
) -> ExtractResult<()> {
    match node.kind() {
        "block_node" | "flow_node" | "block_sequence" | "block_sequence_item" | "flow_sequence" => {
            for child in context.children(node)? {
                collect_pairs(child, context, output)?;
            }
        }
        "block_mapping" | "flow_mapping" => output.extend(
            context
                .children(node)?
                .into_iter()
                .filter(|child| matches!(child.kind(), "block_mapping_pair" | "flow_pair")),
        ),
        _ => {}
    }
    Ok(())
}

fn pair_entry(
    pair: Node<'_>,
    context: &Context<'_>,
    recurse: bool,
) -> ExtractResult<Option<Entry>> {
    let Some(key) = context.field(pair, "key")? else {
        return Ok(None);
    };
    let compacted = compact_whitespace(context.text(key));
    let key = trim_ascii_whitespace(&compacted);
    let key = strip_delimited(key, "\"").unwrap_or(key);
    let key = strip_delimited(key, "'").unwrap_or(key).to_owned();
    if key.is_empty() {
        return Ok(None);
    }
    let mut entry = Entry::item(Section::Constant, pair, key);
    if recurse && let Some(value) = context.field(pair, "value")? {
        let mut pairs = Vec::new();
        collect_pairs(value, context, &mut pairs)?;
        for child in pairs {
            if let Some(child) = pair_entry(child, context, false)? {
                entry.item_mut().children.push(child.into());
            }
        }
    }
    Ok(Some(entry))
}
