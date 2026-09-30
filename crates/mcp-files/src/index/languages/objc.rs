use tree_sitter::Node;

use super::{
    c,
    common::{ExtractResult, LanguageSpec},
};
use crate::index::{
    model::{Child, Entry, Section},
    render::{FIELD_TRUNCATE_THRESHOLD, compact_whitespace, ranged, truncated_message},
    traversal::Context,
};

const END_KEYWORD: &str = "@end";
const HEAD_KINDS: [&str; 4] = [
    "identifier",
    "parameterized_arguments",
    "generic_arguments",
    "protocol_reference_list",
];
const MEMBER_WRAPPERS: [&str; 2] = [
    "implementation_definition",
    "qualified_protocol_interface_declaration",
];

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new("/", extract_nodes);
    spec.is_doc_comment = Some(c::is_doc_comment);
    spec
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    let entry = match node.kind() {
        "module_import" => extract_module_import(node, context)?,
        "class_interface" => extract_container(node, context, Section::Class, "@interface")?,
        "class_implementation" => {
            extract_container(node, context, Section::Impl, "@implementation")?
        }
        "protocol_declaration" => extract_container(node, context, Section::Trait, "@protocol")?,
        // Foundation's unterminated macros, `NS_ASSUME_NONNULL_BEGIN` and `NS_ENUM` among them, can
        // leave the rest of a file's well-formed declarations under one top-level ERROR node.
        "preproc_ifdef" | "preproc_if" | "ERROR" => {
            let mut entries = Vec::new();
            for child in context.children(node)? {
                entries.extend(extract_nodes(child, context, &[])?);
            }
            return Ok(entries);
        }
        _ => return c::extract_nodes(node, context, attrs),
    };
    Ok(entry.into_iter().collect())
}

fn extract_module_import(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let module: String = context
        .fields(node, "path")?
        .into_iter()
        .map(|part| context.text(part))
        .collect();
    Ok((!module.is_empty()).then(|| Entry::import(node, vec![vec![module]], None)))
}

fn extract_container(
    node: Node<'_>,
    context: &Context<'_>,
    section: Section,
    keyword: &str,
) -> ExtractResult<Option<Entry>> {
    let Some(head) = head(node, context, keyword)? else {
        return Ok(None);
    };
    let mut entry = Entry::item(section, node, head);
    entry.item_mut().children = members(node, context)?;
    Ok(Some(entry))
}

/// The declaration line: the keyword through the name, superclass, category, and protocol list.
fn head(node: Node<'_>, context: &Context<'_>, keyword: &str) -> ExtractResult<Option<String>> {
    let children = context.children(node)?;
    let Some(start) = children.iter().position(|child| child.kind() == keyword) else {
        return Ok(None);
    };
    let end = children[start + 1..]
        .iter()
        .take_while(|child| {
            if child.is_named() {
                HEAD_KINDS.contains(&child.kind())
            } else {
                child.kind() != END_KEYWORD
            }
        })
        .last()
        .unwrap_or(&children[start])
        .end_byte();
    let offset = node.start_byte();
    Ok(context
        .text(node)
        .get(children[start].start_byte() - offset..end - offset)
        .map(compact_whitespace))
}

fn members(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Vec<Child>> {
    let mut members = Vec::new();
    for child in context.named_children(node)? {
        if MEMBER_WRAPPERS.contains(&child.kind()) {
            members.extend(context.named_children(child)?);
        } else {
            members.push(child);
        }
    }

    let mut rendered = Vec::new();
    let mut properties = 0usize;
    for member in members {
        match member.kind() {
            "method_declaration" | "method_definition" => {
                let text = context.text(member);
                let signature = match context.child(member, "compound_statement")? {
                    Some(body) => text
                        .get(..body.start_byte() - member.start_byte())
                        .unwrap_or(text),
                    None => text,
                };
                rendered.push(ranged(declaration_text(signature), context.range(member)));
            }
            "property_declaration" => {
                properties += 1;
                if properties <= FIELD_TRUNCATE_THRESHOLD {
                    rendered.push(ranged(
                        declaration_text(context.text(member)),
                        context.range(member),
                    ));
                }
            }
            _ => {}
        }
    }
    if properties > FIELD_TRUNCATE_THRESHOLD {
        rendered.push(truncated_message(properties).into());
    }
    Ok(rendered)
}

fn declaration_text(text: &str) -> String {
    compact_whitespace(text.trim_end().trim_end_matches(';').trim_end())
}
