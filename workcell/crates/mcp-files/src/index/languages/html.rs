use tree_sitter::Node;

use super::common::{ExtractResult, plain_skeleton};
use crate::index::{model::ParsedSkeleton, render::format_range, traversal::Context};

const STRUCTURAL: &[&str] = &[
    "html", "head", "body", "header", "footer", "nav", "main", "section", "article", "aside",
    "div", "form", "table", "thead", "tbody", "tfoot", "tr", "ul", "ol", "dl", "details", "dialog",
    "template", "slot", "fieldset",
];
type TagInfo = (String, Option<String>, Vec<String>);

pub(super) fn extract(root: Node<'_>, context: &Context<'_>) -> ExtractResult<ParsedSkeleton> {
    let mut lines = Vec::new();
    walk(root, context, 0, &mut lines)?;
    context.check()?;
    Ok(plain_skeleton(if lines.is_empty() {
        String::new()
    } else {
        format!("structure:\n{}\n", lines.join("\n"))
    }))
}

fn walk(
    node: Node<'_>,
    context: &Context<'_>,
    depth: usize,
    output: &mut Vec<String>,
) -> ExtractResult<()> {
    match node.kind() {
        "element" | "self_closing_tag" => {
            let tag = if node.kind() == "element" {
                context
                    .child(node, "start_tag")?
                    .or(context.child(node, "self_closing_tag")?)
            } else {
                Some(node)
            };
            let Some(tag) = tag else {
                return Ok(());
            };
            let Some((name, id, classes)) = tag_info(tag, context)? else {
                return Ok(());
            };
            if STRUCTURAL.contains(&name.as_str()) || id.is_some() {
                emit(output, depth, &name, id.as_deref(), &classes, node);
                if node.kind() == "element" {
                    for child in context.children(node)? {
                        walk(child, context, depth + 1, output)?;
                    }
                }
            }
        }
        "script_element" | "style_element" => {
            let name = if node.kind() == "script_element" {
                "script"
            } else {
                "style"
            };
            let (id, classes) = if let Some(tag) = context.child(node, "start_tag")? {
                tag_info(tag, context)?
                    .map(|(_, id, classes)| (id, classes))
                    .unwrap_or_default()
            } else {
                (None, Vec::new())
            };
            emit(output, depth, name, id.as_deref(), &classes, node);
        }
        "document" => {
            for child in context.children(node)? {
                walk(child, context, depth, output)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn tag_info(tag: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<TagInfo>> {
    let Some(name) = context.child(tag, "tag_name")? else {
        return Ok(None);
    };
    let mut id = None;
    let mut classes = Vec::new();
    for child in context.children(tag)? {
        if child.kind() != "attribute" {
            continue;
        }
        let Some(attribute_name) = context.child(child, "attribute_name")? else {
            continue;
        };
        let attribute_name = context.text(attribute_name);
        if !matches!(attribute_name, "id" | "class") {
            continue;
        }
        let value = if let Some(quoted) = context.child(child, "quoted_attribute_value")? {
            context.child(quoted, "attribute_value")?
        } else {
            context.child(child, "attribute_value")?
        };
        let Some(value) = value else {
            continue;
        };
        if attribute_name == "id" {
            id = Some(context.text(value).to_owned());
        } else {
            classes.extend(
                context
                    .text(value)
                    .split(|character: char| character.is_ascii_whitespace())
                    .filter(|class| !class.is_empty())
                    .map(str::to_owned),
            );
        }
    }
    Ok(Some((context.text(name).to_owned(), id, classes)))
}

fn emit(
    output: &mut Vec<String>,
    depth: usize,
    name: &str,
    id: Option<&str>,
    classes: &[String],
    node: Node<'_>,
) {
    let mut tag = format!("<{name}");
    if let Some(id) = id {
        tag.push('#');
        tag.push_str(id);
    }
    for class in classes {
        tag.push('.');
        tag.push_str(class);
    }
    tag.push('>');
    output.push(format!(
        "{}{tag} {}",
        "  ".repeat(depth + 1),
        format_range(crate::index::model::LineRange::from_node(node))
    ));
}
