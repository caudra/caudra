use tree_sitter::Node;

use super::common::{ExtractResult, trim_ascii_whitespace};
use crate::index::{
    model::{Entry, ParsedSkeleton, Section},
    render::{compact_whitespace, format_skeleton},
    traversal::Context,
};

pub(super) fn extract(root: Node<'_>, context: &Context<'_>) -> ExtractResult<ParsedSkeleton> {
    let mut headings = Vec::new();
    collect(root, context, &mut headings)?;
    let document_end = root.end_position().row + 1;
    for index in 0..headings.len() {
        let level = headings[index].0;
        let end = headings[index + 1..]
            .iter()
            .find(|(candidate, _)| *candidate <= level)
            .map_or(document_end, |(_, entry)| {
                entry.range.start.saturating_sub(1)
            });
        headings[index].1.range.end = end;
    }
    let entries = headings
        .into_iter()
        .map(|(_, entry)| entry)
        .collect::<Vec<_>>();
    format_skeleton(&entries, &[], None, "", context)
}

fn collect(
    node: Node<'_>,
    context: &Context<'_>,
    headings: &mut Vec<(usize, Entry)>,
) -> ExtractResult<()> {
    if matches!(node.kind(), "atx_heading" | "setext_heading")
        && let Some(content) = context.field(node, "heading_content")?
    {
        let level = if node.kind() == "atx_heading" {
            context
                .text(node)
                .bytes()
                .take_while(|byte| *byte == b'#')
                .count()
        } else if context
            .text(node)
            .trim_end_matches(|character: char| character.is_ascii_whitespace())
            .ends_with('=')
        {
            1
        } else {
            2
        };
        let compacted = compact_whitespace(context.text(content));
        let text = trim_ascii_whitespace(&compacted).to_owned();
        headings.push((
            level,
            Entry::item(
                Section::Heading,
                node,
                format!("{} {text}", "#".repeat(level)),
            ),
        ));
    }
    for child in context.children(node)? {
        collect(child, context, headings)?;
    }
    Ok(())
}
