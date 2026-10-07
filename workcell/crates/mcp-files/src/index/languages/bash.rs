use tree_sitter::Node;

use super::common::{ExtractResult, LanguageSpec, uppercase_identifier};
use crate::index::{
    model::{Entry, Section},
    render::truncate,
    traversal::Context,
};

pub(super) fn spec() -> LanguageSpec {
    LanguageSpec::new("/", extract_nodes)
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    let entry = match node.kind() {
        "function_definition" => context
            .field(node, "name")?
            .map(|name| Entry::item(Section::Function, node, format!("{}()", context.text(name)))),
        "variable_assignment" => {
            let Some(name) = context.field(node, "name")? else {
                return Ok(Vec::new());
            };
            if !uppercase_identifier(context.text(name)) {
                return Ok(Vec::new());
            }
            let value = context
                .field(node, "value")?
                .map_or(String::new(), |value| {
                    format!(" = {}", truncate(context.text(value), 60))
                });
            Some(Entry::item(
                Section::Constant,
                node,
                format!("{}{value}", context.text(name)),
            ))
        }
        _ => None,
    };
    Ok(entry.into_iter().collect())
}
