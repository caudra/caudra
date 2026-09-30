use std::collections::HashMap;

use tree_sitter::Node;

use super::common::{ExtractResult, plain_skeleton, strip_delimited};
use crate::index::{
    model::{LineRange, ParsedSkeleton},
    render::{compact_whitespace, format_range, truncate},
    traversal::Context,
};

const MAX_DEPTH: usize = 3;
const VALUE_TRUNCATE: usize = 60;
const IDENTIFYING_ATTRIBUTES: [&str; 4] = ["id", "name", "key", "include"];
const TAG_KINDS: [&str; 2] = ["STag", "EmptyElemTag"];
const QUOTES: [&str; 2] = ["\"", "'"];

struct Row<'tree> {
    element: Node<'tree>,
    label: String,
    count: usize,
    range: LineRange,
}

pub(super) fn extract(root: Node<'_>, context: &Context<'_>) -> ExtractResult<ParsedSkeleton> {
    let mut lines = Vec::new();
    for element in context.named_children(root)? {
        if element.kind() == "element"
            && let Some(label) = element_label(element, context)?
        {
            visit(element, &label, context, 0, &mut lines)?;
        }
    }
    context.check()?;
    Ok(plain_skeleton(if lines.is_empty() {
        String::new()
    } else {
        format!("structure:\n{}\n", lines.join("\n"))
    }))
}

/// Emits an element, then its child elements down to `MAX_DEPTH` levels.
///
/// Children that would show nothing beneath them collapse by label into one row, counted and
/// spanning the first to the last, so fifty `<dependency>` elements cost one line.
fn visit(
    element: Node<'_>,
    label: &str,
    context: &Context<'_>,
    depth: usize,
    lines: &mut Vec<String>,
) -> ExtractResult<()> {
    emit(lines, depth, label, LineRange::from_node(element));
    let child_depth = depth + 1;
    if child_depth >= MAX_DEPTH {
        return Ok(());
    }
    let mut rows: Vec<Row<'_>> = Vec::new();
    let mut leaf_rows: HashMap<String, usize> = HashMap::new();
    for child in child_elements(element, context)? {
        let Some(label) = element_label(child, context)? else {
            continue;
        };
        let range = LineRange::from_node(child);
        if child_depth + 1 >= MAX_DEPTH || child_elements(child, context)?.is_empty() {
            if let Some(&index) = leaf_rows.get(&label) {
                rows[index].count += 1;
                rows[index].range.end = range.end;
                continue;
            }
            leaf_rows.insert(label.clone(), rows.len());
        }
        rows.push(Row {
            element: child,
            label,
            count: 1,
            range,
        });
    }
    for row in rows {
        if row.count == 1 {
            visit(row.element, &row.label, context, child_depth, lines)?;
        } else {
            let label = format!("{} ×{}", row.label, row.count);
            emit(lines, child_depth, &label, row.range);
        }
    }
    Ok(())
}

fn child_elements<'tree>(
    element: Node<'tree>,
    context: &Context<'_>,
) -> ExtractResult<Vec<Node<'tree>>> {
    let Some(content) = context.child(element, "content")? else {
        return Ok(Vec::new());
    };
    Ok(context
        .named_children(content)?
        .into_iter()
        .filter(|child| child.kind() == "element")
        .collect())
}

/// `<tag>`, or `<tag attribute=value>` for the first attribute whose local name identifies it.
fn element_label(element: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<String>> {
    let Some(tag) = context
        .named_children(element)?
        .into_iter()
        .find(|child| TAG_KINDS.contains(&child.kind()))
    else {
        return Ok(None);
    };
    let Some(name) = context.child(tag, "Name")? else {
        return Ok(None);
    };
    let name = context.text(name);
    for attribute in context.named_children(tag)? {
        if attribute.kind() != "Attribute" {
            continue;
        }
        let (Some(key), Some(value)) = (
            context.child(attribute, "Name")?,
            context.child(attribute, "AttValue")?,
        ) else {
            continue;
        };
        let key = context.text(key);
        let local = key.rsplit_once(':').map_or(key, |(_, local)| local);
        if IDENTIFYING_ATTRIBUTES
            .iter()
            .any(|identifying| local.eq_ignore_ascii_case(identifying))
        {
            let value = context.text(value);
            let value = QUOTES
                .iter()
                .find_map(|quote| strip_delimited(value, quote))
                .unwrap_or(value);
            let value = truncate(&compact_whitespace(value), VALUE_TRUNCATE);
            return Ok(Some(format!("<{name} {key}={value}>")));
        }
    }
    Ok(Some(format!("<{name}>")))
}

fn emit(lines: &mut Vec<String>, depth: usize, label: &str, range: LineRange) {
    lines.push(format!(
        "{}{label} {}",
        "  ".repeat(depth + 1),
        format_range(range)
    ));
}
