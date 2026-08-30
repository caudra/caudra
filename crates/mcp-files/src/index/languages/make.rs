use tree_sitter::Node;

use super::common::{ExtractResult, simple_import, trim_ascii_whitespace};
use crate::index::{
    model::{Entry, ParsedSkeleton, Section},
    render::{compact_whitespace, format_skeleton, truncate},
    traversal::Context,
};

const VALUE_TRUNCATE: usize = 80;

pub(super) fn extract(root: Node<'_>, context: &Context<'_>) -> ExtractResult<ParsedSkeleton> {
    let mut entries = Vec::new();
    scan(root, context, &mut entries)?;
    format_skeleton(&entries, &[], None, "/", context)
}

fn scan(node: Node<'_>, context: &Context<'_>, entries: &mut Vec<Entry>) -> ExtractResult<()> {
    for child in context.children(node)? {
        if matches!(
            child.kind(),
            "conditional" | "else_directive" | "elsif_directive"
        ) {
            scan(child, context, entries)?;
        } else if let Some(mut entry) = entry_for(child, context)? {
            let text = context
                .text(child)
                .trim_end_matches(|character: char| character.is_ascii_whitespace());
            entry.range.end =
                entry.range.start + text.bytes().filter(|byte| *byte == b'\n').count();
            entries.push(entry);
        }
    }
    Ok(())
}

fn entry_for(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let compacted = compact_whitespace(context.text(node));
    let trimmed = trim_ascii_whitespace(&compacted).to_owned();
    Ok(match node.kind() {
        "rule" => context.child(node, "targets")?.map(|targets| {
            Entry::item(
                Section::Target,
                node,
                format!(
                    "{}:",
                    trim_ascii_whitespace(&compact_whitespace(context.text(targets)))
                ),
            )
        }),
        "variable_assignment" => Some(Entry::item(
            Section::Constant,
            node,
            truncate(&trimmed, VALUE_TRUNCATE),
        )),
        "define_directive" => context.field(node, "name")?.map(|name| {
            Entry::item(
                Section::Constant,
                node,
                format!("define {}", context.text(name)),
            )
        }),
        "include_directive" => {
            let cleaned = ["-include", "sinclude", "include"]
                .into_iter()
                .find_map(|prefix| trimmed.strip_prefix(prefix))
                .unwrap_or(&trimmed)
                .trim();
            Some(simple_import(node, cleaned, '/'))
        }
        "recipe_line" if looks_like_assignment(&trimmed) => Some(Entry::item(
            Section::Constant,
            node,
            truncate(&trimmed, VALUE_TRUNCATE),
        )),
        _ => None,
    })
}

fn looks_like_assignment(value: &str) -> bool {
    let Some(position) = value.find('=') else {
        return false;
    };
    let name = value[..position].trim();
    let name = [':', '+', '?', '!']
        .into_iter()
        .find_map(|operator| name.strip_suffix(operator))
        .unwrap_or(name)
        .trim();
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}
