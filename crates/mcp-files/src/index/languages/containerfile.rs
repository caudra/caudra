use tree_sitter::Node;

use super::common::ExtractResult;
use crate::index::{
    model::{Entry, ParsedSkeleton, Section},
    render::{compact_whitespace, format_skeleton, truncate},
    traversal::Context,
};

const INSTRUCTION_TRUNCATE: usize = 100;

pub(super) fn extract(root: Node<'_>, context: &Context<'_>) -> ExtractResult<ParsedSkeleton> {
    let mut entries = Vec::new();
    for child in context.children(root)? {
        if child.kind().ends_with("_instruction") {
            entries.push(Entry::item(
                Section::Instruction,
                child,
                truncate(
                    &compact_whitespace(context.text(child)),
                    INSTRUCTION_TRUNCATE,
                ),
            ));
        }
    }
    format_skeleton(&entries, &[], None, "", context)
}
