use tree_sitter::Node;

use super::common::{
    ExtractResult, LanguageSpec, split_path, strip_delimited, uppercase_identifier,
};
use crate::index::{
    model::{Entry, Section},
    render::{compact_whitespace, truncate},
    traversal::Context,
};

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new(".", extract_nodes);
    spec.is_doc_comment = Some(is_doc_comment);
    spec
}

fn is_doc_comment(node: Node<'_>, context: &Context<'_>) -> bool {
    node.kind() == "comment" && context.text(node).starts_with("---")
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    match node.kind() {
        "function_declaration" => {
            let Some(name) = context.field(node, "name")? else {
                return Ok(Vec::new());
            };
            let params = context
                .field(node, "parameters")?
                .map_or("()", |params| context.text(params));
            Ok(vec![Entry::item(
                Section::Function,
                node,
                compact_whitespace(&format!("{}{params}", context.text(name))),
            )])
        }
        "variable_declaration" => extract_variable(node, context),
        "function_call" => Ok(import_from_require(node, context)?.into_iter().collect()),
        _ => Ok(Vec::new()),
    }
}

fn extract_variable(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Vec<Entry>> {
    let Some(assignment) = context.child(node, "assignment_statement")? else {
        return Ok(Vec::new());
    };
    let (Some(variables), Some(expressions)) = (
        context.child(assignment, "variable_list")?,
        context.child(assignment, "expression_list")?,
    ) else {
        return Ok(Vec::new());
    };
    let expressions = context
        .children(expressions)?
        .into_iter()
        .filter(|child| child.kind() != ",")
        .collect::<Vec<_>>();
    let mut imports = Vec::new();
    for expression in &expressions {
        if expression.kind() == "function_call"
            && let Some(import) = import_from_require(*expression, context)?
        {
            imports.push(import);
        }
    }
    if !imports.is_empty() {
        return Ok(imports);
    }
    let variables = context
        .children(variables)?
        .into_iter()
        .filter(|child| child.kind() != ",")
        .collect::<Vec<_>>();
    if let [variable] = variables.as_slice()
        && uppercase_identifier(context.text(*variable))
    {
        let value = expressions.first().map_or(String::new(), |value| {
            format!(" = {}", truncate(context.text(*value), 60))
        });
        return Ok(vec![Entry::item(
            Section::Constant,
            node,
            format!("{}{value}", context.text(*variable)),
        )]);
    }
    Ok(Vec::new())
}

fn import_from_require(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    if context.text(name) != "require" {
        return Ok(None);
    }
    let Some(arguments) = context.field(node, "arguments")? else {
        return Ok(None);
    };
    for child in context.children(arguments)? {
        if child.kind() == "string" {
            let raw_module = context.text(child);
            let stripped = strip_delimited(raw_module, "\"")
                .or_else(|| strip_delimited(raw_module, "'"))
                .unwrap_or(raw_module);
            let module = if stripped.is_empty() {
                raw_module
            } else {
                stripped
            };
            return Ok(Some(Entry::import(
                node,
                vec![split_path(module, '.')],
                None,
            )));
        }
    }
    Ok(None)
}
