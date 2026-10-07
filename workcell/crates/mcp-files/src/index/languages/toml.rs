use tree_sitter::Node;

use super::common::ExtractResult;
use crate::index::{
    model::{Entry, ParsedSkeleton, Section},
    render::{FIELD_TRUNCATE_THRESHOLD, compact_whitespace, format_skeleton, truncate},
    traversal::Context,
};

const VALUE_TRUNCATE: usize = 60;
const KEY_KINDS: &[&str] = &["bare_key", "dotted_key", "quoted_key"];
const VALUE_KINDS: &[&str] = &[
    "string",
    "integer",
    "float",
    "boolean",
    "offset_date_time",
    "local_date_time",
    "local_date",
    "local_time",
    "array",
    "inline_table",
];

pub(super) fn extract(root: Node<'_>, context: &Context<'_>) -> ExtractResult<ParsedSkeleton> {
    let mut entries = Vec::new();
    for child in context.children(root)? {
        match child.kind() {
            "pair" => {
                if let Some(text) = format_pair(child, context, true)? {
                    entries.push(Entry::item(Section::Constant, child, text));
                }
            }
            "table" | "table_array_element" => {
                entries.push(table_entry(
                    child,
                    context,
                    child.kind() == "table_array_element",
                )?);
            }
            _ => {}
        }
    }
    format_skeleton(&entries, &[], None, "", context)
}

fn pair_parts<'tree>(
    node: Node<'tree>,
    context: &Context<'_>,
) -> ExtractResult<(Option<Node<'tree>>, Option<Node<'tree>>)> {
    let mut key = None;
    let mut value = None;
    for child in context.children(node)? {
        if key.is_none() && KEY_KINDS.contains(&child.kind()) {
            key = Some(child);
        } else if key.is_some() && value.is_none() && VALUE_KINDS.contains(&child.kind()) {
            value = Some(child);
        }
    }
    Ok((key, value))
}

fn format_pair(
    node: Node<'_>,
    context: &Context<'_>,
    include_value: bool,
) -> ExtractResult<Option<String>> {
    let (key, value) = pair_parts(node, context)?;
    let Some(key) = key else {
        return Ok(None);
    };
    let key = context.text(key);
    Ok(Some(if include_value {
        value.map_or_else(
            || key.to_owned(),
            |value| {
                format!(
                    "{key} = {}",
                    truncate(&compact_whitespace(context.text(value)), VALUE_TRUNCATE)
                )
            },
        )
    } else {
        key.to_owned()
    }))
}

fn table_entry(node: Node<'_>, context: &Context<'_>, array: bool) -> ExtractResult<Entry> {
    let header = context
        .children(node)?
        .into_iter()
        .find(|child| KEY_KINDS.contains(&child.kind()));
    let path = header.map_or("?", |header| context.text(header));
    let label = if array {
        format!("[[{path}]]")
    } else {
        format!("[{path}]")
    };
    let mut entry = Entry::item(Section::Constant, node, label);
    let pairs = context
        .children(node)?
        .into_iter()
        .filter(|child| child.kind() == "pair")
        .collect::<Vec<_>>();
    for (index, pair) in pairs.into_iter().enumerate() {
        if let Some(text) = format_pair(pair, context, index < FIELD_TRUNCATE_THRESHOLD)? {
            entry.item_mut().children.push(text.into());
        }
    }
    Ok(entry)
}
