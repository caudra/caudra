use tree_sitter::Node;

use super::common::ExtractResult;
use crate::index::{
    model::{Child, ChildKind, Entry, ParsedSkeleton, Section},
    render::{compact_whitespace, format_skeleton, ranged, truncate},
    traversal::Context,
};

const VALUE_TRUNCATE: usize = 80;
const INSTANCE_KEYWORD: &str = "instance";
const LOCAL_PREFIX: &str = "LOCAL ";

pub(super) fn extract(root: Node<'_>, context: &Context<'_>) -> ExtractResult<ParsedSkeleton> {
    let mut entries = Vec::new();
    scan(root, context, &mut entries)?;
    format_skeleton(&entries, &[], None, "/", context)
}

/// Walks a file or a module. A module nests further modules, and PlusCal lives in a block comment.
fn scan(node: Node<'_>, context: &Context<'_>, entries: &mut Vec<Entry>) -> ExtractResult<()> {
    for child in context.named_children(node)? {
        match child.kind() {
            "module" => {
                if let Some(name) = context.field(child, "name")? {
                    entries.push(Entry::item(Section::Module, child, context.text(name)));
                }
                scan(child, context, entries)?;
            }
            "block_comment" => scan(child, context, entries)?,
            "local_definition" => {
                for definition in context.named_children(child)? {
                    entries.extend(unit(definition, context, LOCAL_PREFIX)?);
                }
            }
            _ => entries.extend(unit(child, context, "")?),
        }
    }
    Ok(())
}

/// A junction list ends where the next token starts, so a definition's node runs through the blank
/// lines after it. Its range ends at its last non-blank line instead.
fn unit(node: Node<'_>, context: &Context<'_>, prefix: &str) -> ExtractResult<Option<Entry>> {
    let mut entry = definition(node, context, prefix)?;
    if let Some(entry) = &mut entry {
        let text = context.text(node).trim_end();
        entry.range.end = entry.range.start + text.bytes().filter(|byte| *byte == b'\n').count();
    }
    Ok(entry)
}

fn definition(node: Node<'_>, context: &Context<'_>, prefix: &str) -> ExtractResult<Option<Entry>> {
    Ok(match node.kind() {
        "extends" => Some(Entry::import(node, module_names(node, context)?, None)),
        "instance" => Some(Entry::import(
            node,
            module_names(node, context)?.into_iter().take(1).collect(),
            Some(INSTANCE_KEYWORD.to_owned()),
        )),
        "constant_declaration" | "variable_declaration" => Some(declaration(node, context)?),
        "operator_definition" | "function_definition" => Some(Entry::item(
            Section::Function,
            node,
            format!("{prefix}{}", head(node, context)?),
        )),
        "module_definition" => Some(Entry::item(
            Section::Function,
            node,
            format!(
                "{prefix}{}",
                truncate(&compact_whitespace(context.text(node)), VALUE_TRUNCATE)
            ),
        )),
        "theorem" | "assumption" => Some(Entry::item(Section::Rule, node, fact(node, context)?)),
        "pcal_algorithm" => Some(algorithm(node, context)?),
        _ => None,
    })
}

fn module_names(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Vec<Vec<String>>> {
    Ok(context
        .named_children(node)?
        .into_iter()
        .filter(|child| child.kind() == "identifier_ref")
        .map(|name| vec![context.text(name).to_owned()])
        .collect())
}

/// `CONSTANTS` or `VARIABLES`, with every declared name listed beneath.
fn declaration(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    let mut entry = Entry::item(Section::Constant, node, keyword(node, context)?);
    let item = entry.item_mut();
    item.children = context
        .named_children(node)?
        .into_iter()
        .map(|name| Child::from(compact_whitespace(context.text(name))))
        .collect();
    item.child_kind = ChildKind::Brief;
    Ok(entry)
}

/// `THEOREM Name` or `ASSUME Name`, or the statement itself when the fact is unnamed.
fn fact(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let keyword = keyword(node, context)?;
    let subject = match context.field(node, "name")? {
        Some(name) => context.text(name).to_owned(),
        None => context
            .named_children(node)?
            .first()
            .map_or_else(String::new, |statement| {
                truncate(
                    &compact_whitespace(context.text(*statement)),
                    VALUE_TRUNCATE,
                )
            }),
    };
    Ok(format!("{keyword} {subject}"))
}

fn keyword(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    Ok(context
        .children(node)?
        .first()
        .map_or_else(String::new, |token| context.text(*token).to_owned()))
}

/// The left-hand side of a definition: `Op(a, b)`, `f[x \in S]`, or `a \oplus b`.
fn head(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let text = context.text(node);
    let end = context
        .child(node, "def_eq")?
        .map_or(text.len(), |def_eq| def_eq.start_byte() - node.start_byte());
    Ok(compact_whitespace(text[..end].trim_end()))
}

fn algorithm(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    let start = context
        .child(node, "pcal_algorithm_start")?
        .map_or("--algorithm", |start| context.text(start).trim());
    let name = context
        .field(node, "name")?
        .map_or("", |name| context.text(name));
    let mut entry = Entry::item(Section::Class, node, format!("{start} {name}"));
    let mut children = Vec::new();
    for child in context.named_children(node)? {
        match child.kind() {
            "pcal_var_decls" => children.push(Child::from(truncate(
                &statement(context.text(child)),
                VALUE_TRUNCATE,
            ))),
            "pcal_definitions" => {
                for definition in context.named_children(child)? {
                    children.push(
                        Entry::item(
                            Section::Function,
                            definition,
                            format!("define {}", head(definition, context)?),
                        )
                        .into(),
                    );
                }
            }
            "pcal_macro" => {
                if let Some(declaration) = context.child(child, "pcal_macro_decl")? {
                    children.push(
                        Entry::item(Section::Macro, child, statement(context.text(declaration)))
                            .into(),
                    );
                }
            }
            "pcal_procedure" => {
                if let Some(declaration) = context.child(child, "pcal_proc_decl")? {
                    children.push(
                        with_labels(child, statement(context.text(declaration)), context)?.into(),
                    );
                }
            }
            "pcal_process" => {
                children.push(with_labels(child, process_head(child, context)?, context)?.into())
            }
            "pcal_algorithm_body" => labels(child, context, &mut children)?,
            _ => {}
        }
    }
    entry.item_mut().children = children;
    Ok(entry)
}

/// `process Name \in S`, up to its local variables or its body.
fn process_head(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let text = context.text(node);
    let end = context
        .named_children(node)?
        .into_iter()
        .find(|child| matches!(child.kind(), "pcal_var_decls" | "pcal_algorithm_body"))
        .map_or(text.len(), |child| child.start_byte() - node.start_byte());
    Ok(compact_whitespace(text[..end].trim_end()))
}

fn with_labels(node: Node<'_>, text: String, context: &Context<'_>) -> ExtractResult<Entry> {
    let mut entry = Entry::item(Section::Function, node, text);
    let mut children = Vec::new();
    for child in context.named_children(node)? {
        if child.kind() == "pcal_algorithm_body" {
            labels(child, context, &mut children)?;
        }
    }
    let item = entry.item_mut();
    item.children = children;
    item.child_kind = ChildKind::Brief;
    Ok(entry)
}

/// Every label in a PlusCal body, including those nested in `if`, `while`, `either`, and `with`.
fn labels(node: Node<'_>, context: &Context<'_>, output: &mut Vec<Child>) -> ExtractResult<()> {
    if node.kind() == "pcal_algorithm_body" {
        for label in context.fields(node, "label")? {
            if label.kind() == "identifier" {
                output.push(ranged(context.text(label), context.range(label)));
            }
        }
    }
    for child in context.named_children(node)? {
        labels(child, context, output)?;
    }
    Ok(())
}

/// A declaration on one line, without its terminating semicolon.
fn statement(text: &str) -> String {
    compact_whitespace(text.trim_end().trim_end_matches(';').trim_end())
}
