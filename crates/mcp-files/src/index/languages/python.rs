use tree_sitter::Node;

use super::common::{ExtractResult, LanguageSpec, split_path, uppercase_identifier};
use crate::index::{
    model::{Entry, Section},
    render::{compact_whitespace, ranged, truncate},
    traversal::Context,
};

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new(".", extract_nodes);
    spec.is_module_doc = Some(is_module_doc);
    spec
}

fn is_module_doc(node: Node<'_>, context: &Context<'_>) -> bool {
    node.kind() == "expression_statement"
        && node.child(0).is_some_and(|child| {
            child.kind() == "string" && context.text(child).starts_with("\"\"\"")
        })
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    match node.kind() {
        "import_statement" | "import_from_statement" => Ok(vec![extract_import(node, context)]),
        "class_definition" => Ok(extract_class(node, context)?.into_iter().collect()),
        "function_definition" => Ok(extract_function(node, node, context)?.into_iter().collect()),
        "decorated_definition" => {
            if let Some(class) = context.child(node, "class_definition")? {
                let mut entry = extract_class(class, context)?;
                if let Some(entry) = &mut entry {
                    entry.range.start = node.start_position().row + 1;
                }
                Ok(entry.into_iter().collect())
            } else if let Some(function) = context.child(node, "function_definition")? {
                Ok(extract_function(node, function, context)?
                    .into_iter()
                    .collect())
            } else {
                Ok(Vec::new())
            }
        }
        "expression_statement" => {
            let assignment = node.child(0).filter(|child| child.kind() == "assignment");
            Ok(assignment
                .map(|assignment| extract_assignment(assignment, context))
                .transpose()?
                .flatten()
                .into_iter()
                .collect())
        }
        _ => Ok(Vec::new()),
    }
}

fn extract_import(node: Node<'_>, context: &Context<'_>) -> Entry {
    let text = context.text(node);
    let cleaned = text
        .strip_prefix("import ")
        .or_else(|| text.strip_prefix("from "))
        .unwrap_or(text)
        .trim();
    let paths = if let Some((base, names)) = cleaned.split_once(" import ") {
        let base = split_path(base, '.');
        names
            .split(',')
            .map(|name| {
                let mut path = base.clone();
                path.push(name.trim().to_owned());
                path
            })
            .collect()
    } else {
        vec![split_path(cleaned, '.')]
    };
    Entry::import(node, paths, None)
}

fn extract_class(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let (Some(name), Some(body)) = (context.field(node, "name")?, context.field(node, "body")?)
    else {
        return Ok(None);
    };
    let mut entry = Entry::item(Section::Class, node, context.text(name));
    for child in context.children(body)? {
        let (function, decorated) = if child.kind() == "decorated_definition" {
            (context.child(child, "function_definition")?, true)
        } else if child.kind() == "function_definition" {
            (Some(child), false)
        } else {
            (None, false)
        };
        let Some(function) = function else {
            continue;
        };
        if decorated {
            for decorator in context.children(child)? {
                if decorator.kind() == "decorator" {
                    entry
                        .item_mut()
                        .children
                        .push(context.text(decorator).to_owned().into());
                }
            }
        }
        if let Some(signature) = function_signature(function, context)? {
            entry
                .item_mut()
                .children
                .push(ranged(signature, context.range(function)));
        }
    }
    Ok(Some(entry))
}

fn extract_function(
    range_node: Node<'_>,
    function: Node<'_>,
    context: &Context<'_>,
) -> ExtractResult<Option<Entry>> {
    Ok(function_signature(function, context)?
        .map(|signature| Entry::item(Section::Function, range_node, signature)))
}

fn function_signature(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<String>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let parameters = context
        .field(node, "parameters")?
        .map_or("()", |parameters| context.text(parameters));
    let return_type = context
        .field(node, "return_type")?
        .map_or(String::new(), |return_type| {
            format!(" -> {}", context.text(return_type))
        });
    Ok(Some(compact_whitespace(&format!(
        "{}{parameters}{return_type}",
        context.text(name)
    ))))
}

fn extract_assignment(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(left) = node.child(0) else {
        return Ok(None);
    };
    let name = context.text(left);
    if !uppercase_identifier(name) {
        return Ok(None);
    }
    let children = context.children(node)?;
    let value = children
        .windows(2)
        .find(|pair| context.text(pair[0]) == "=")
        .map_or(String::new(), |pair| {
            format!(" = {}", truncate(context.text(pair[1]), 60))
        });
    Ok(Some(Entry::item(
        Section::Constant,
        node,
        format!("{name}{value}"),
    )))
}
