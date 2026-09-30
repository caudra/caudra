use tree_sitter::Node;

use super::common::{ExtractResult, LanguageSpec, simple_import, trim_ascii_whitespace};
use crate::index::{
    model::{Entry, Section},
    render::{compact_whitespace, truncate},
    traversal::Context,
};

const VALUE_TRUNCATE: usize = 80;
const PROJECT_COMMAND: &str = "project";
const IMPORT_COMMANDS: [&str; 3] = ["include", "find_package", "add_subdirectory"];
const TARGET_COMMANDS: [&str; 3] = ["add_library", "add_executable", "add_custom_target"];
const CONSTANT_COMMANDS: [&str; 2] = ["set", "option"];
const FILE_SCOPE_BLOCKS: [&str; 5] = [
    "if_condition",
    "foreach_loop",
    "while_loop",
    "block_def",
    "body",
];

pub(super) fn spec() -> LanguageSpec {
    LanguageSpec::new("/", extract_nodes)
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    let entry = match node.kind() {
        "normal_command" => extract_command(node, context)?,
        "function_def" => extract_definition(node, context, "function_command", Section::Function)?,
        "macro_def" => extract_definition(node, context, "macro_command", Section::Macro)?,
        kind if FILE_SCOPE_BLOCKS.contains(&kind) => {
            let mut entries = Vec::new();
            for child in context.named_children(node)? {
                entries.extend(extract_nodes(child, context, &[])?);
            }
            return Ok(entries);
        }
        _ => None,
    };
    Ok(entry.into_iter().collect())
}

fn extract_command(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.child(node, "identifier")? else {
        return Ok(None);
    };
    let command = context.text(name);
    let is_one_of = |commands: &[&str]| {
        commands
            .iter()
            .any(|candidate| command.eq_ignore_ascii_case(candidate))
    };
    if is_one_of(&IMPORT_COMMANDS) {
        return Ok(first_argument(node, context)?.map(|path| simple_import(node, path, '/')));
    }
    if command.eq_ignore_ascii_case(PROJECT_COMMAND) {
        return Ok(first_argument(node, context)?
            .map(|project| Entry::item(Section::Module, node, project)));
    }
    let section = if is_one_of(&TARGET_COMMANDS) {
        Section::Target
    } else if is_one_of(&CONSTANT_COMMANDS) {
        Section::Constant
    } else {
        return Ok(None);
    };
    Ok(Some(Entry::item(
        section,
        node,
        truncate(&invocation(node, context)?, VALUE_TRUNCATE),
    )))
}

fn first_argument<'source>(
    node: Node<'_>,
    context: &Context<'source>,
) -> ExtractResult<Option<&'source str>> {
    let Some(arguments) = context.child(node, "argument_list")? else {
        return Ok(None);
    };
    Ok(context
        .child(arguments, "argument")?
        .map(|argument| context.text(argument).trim_matches('"')))
}

fn extract_definition(
    node: Node<'_>,
    context: &Context<'_>,
    header_kind: &str,
    section: Section,
) -> ExtractResult<Option<Entry>> {
    let Some(header) = context.child(node, header_kind)? else {
        return Ok(None);
    };
    Ok(Some(Entry::item(
        section,
        node,
        invocation(header, context)?,
    )))
}

/// `name(arguments)` on one line, however the source breaks and pads the argument list.
fn invocation(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let name = context
        .named_children(node)?
        .first()
        .map_or("", |name| context.text(*name));
    let arguments = context
        .child(node, "argument_list")?
        .map(|arguments| compact_whitespace(trim_ascii_whitespace(context.text(arguments))))
        .unwrap_or_default();
    Ok(format!("{name}({arguments})"))
}
