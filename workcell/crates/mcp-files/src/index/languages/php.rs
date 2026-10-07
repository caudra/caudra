use tree_sitter::Node;

use super::common::{ExtractResult, LanguageSpec, extract_enum_variants, split_path};
use crate::index::{
    model::{ChildKind, Entry, Section},
    render::{FIELD_TRUNCATE_THRESHOLD, compact_whitespace, prefixed, ranged, truncated_message},
    traversal::Context,
};

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new("\\", extract_nodes);
    spec.is_doc_comment =
        Some(|node, context| node.kind() == "comment" && context.text(node).starts_with("/**"));
    spec
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    let entry = match node.kind() {
        "namespace_use_declaration" => Some(extract_use(node, context)?),
        "namespace_definition" => context
            .field(node, "name")?
            .map(|name| Entry::item(Section::Module, node, context.text(name))),
        "class_declaration" => extract_class(node, context, Section::Class, false)?,
        "interface_declaration" => extract_class(node, context, Section::Trait, true)?,
        "trait_declaration" => extract_class(node, context, Section::Trait, false)?,
        "function_definition" => extract_function(node, context)?,
        "const_declaration" => extract_constant(node, context)?,
        "enum_declaration" => extract_enum(node, context)?,
        _ => None,
    };
    Ok(entry.into_iter().collect())
}

fn modifiers(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    Ok(context
        .children(node)?
        .into_iter()
        .filter(|child| {
            matches!(
                child.kind(),
                "visibility_modifier"
                    | "static_modifier"
                    | "abstract_modifier"
                    | "final_modifier"
                    | "readonly_modifier"
            )
        })
        .map(|child| context.text(child))
        .collect::<Vec<_>>()
        .join(" "))
}

fn use_path(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Vec<String>>> {
    Ok(context
        .children(node)?
        .into_iter()
        .find(|child| matches!(child.kind(), "qualified_name" | "name"))
        .map(|name| split_path(context.text(name), '\\')))
}

fn extract_use(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    let mut paths = Vec::new();
    let mut prefix = Vec::new();
    for child in context.children(node)? {
        match child.kind() {
            "namespace_use_clause" => {
                if let Some(path) = use_path(child, context)? {
                    paths.push(path);
                }
            }
            "namespace_name" => prefix = split_path(context.text(child), '\\'),
            "namespace_use_group" => {
                for clause in context.children(child)? {
                    if clause.kind() == "namespace_use_clause"
                        && let Some(path) = use_path(clause, context)?
                    {
                        let mut full = prefix.clone();
                        full.extend(path);
                        paths.push(full);
                    }
                }
            }
            _ => {}
        }
    }
    if paths.is_empty() && !prefix.is_empty() {
        paths.push(prefix);
    }
    Ok(Entry::import(node, paths, None))
}

fn parameters(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    Ok(context.child(node, "formal_parameters")?.map_or_else(
        || "()".to_owned(),
        |parameters| context.text(parameters).to_owned(),
    ))
}

fn return_type(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    Ok(context
        .field(node, "return_type")?
        .map_or(String::new(), |return_type| {
            format!(": {}", context.text(return_type))
        }))
}

fn method_signature(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<String>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    Ok(Some(compact_whitespace(&prefixed(
        &modifiers(node, context)?,
        format!(
            "function {}{}{}",
            context.text(name),
            parameters(node, context)?,
            return_type(node, context)?
        ),
    ))))
}

fn property_text(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let field_type = context
        .field(node, "type")?
        .map_or(String::new(), |field_type| {
            format!("{} ", context.text(field_type))
        });
    let name = if let Some(element) = context.child(node, "property_element")? {
        context
            .field(element, "name")?
            .map_or("_", |name| context.text(name))
    } else {
        "_"
    };
    Ok(compact_whitespace(&prefixed(
        &modifiers(node, context)?,
        format!("{field_type}{name}"),
    )))
}

fn members(
    node: Node<'_>,
    context: &Context<'_>,
) -> ExtractResult<Vec<crate::index::model::Child>> {
    let mut output = Vec::new();
    let mut fields = 0usize;
    for child in context.children(node)? {
        if child.kind() == "method_declaration" {
            if let Some(signature) = method_signature(child, context)? {
                output.push(ranged(signature, context.range(child)));
            }
        } else if child.kind() == "property_declaration" {
            fields += 1;
            if fields <= FIELD_TRUNCATE_THRESHOLD {
                output.push(ranged(property_text(child, context)?, context.range(child)));
            }
        }
    }
    if fields > FIELD_TRUNCATE_THRESHOLD {
        output.push(truncated_message(fields).into());
    }
    Ok(output)
}

fn extract_class(
    node: Node<'_>,
    context: &Context<'_>,
    section: Section,
    interface: bool,
) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let mut extension = String::new();
    if let Some(base) = context.child(node, "base_clause")?
        && let Some(name) = context
            .children(base)?
            .into_iter()
            .find(|child| matches!(child.kind(), "qualified_name" | "name"))
    {
        extension = format!(" extends {}", context.text(name));
    }
    let mut interfaces = Vec::new();
    if let Some(clause) = context.child(node, "class_interface_clause")? {
        interfaces.extend(
            context
                .children(clause)?
                .into_iter()
                .filter(|child| matches!(child.kind(), "qualified_name" | "name"))
                .map(|child| context.text(child)),
        );
    }
    let interface_text = if interfaces.is_empty() {
        String::new()
    } else {
        format!(" implements {}", interfaces.join(", "))
    };
    let label = if interface {
        format!("{}{extension}", context.text(name))
    } else {
        compact_whitespace(&prefixed(
            &modifiers(node, context)?,
            format!("{}{extension}{interface_text}", context.text(name)),
        ))
    };
    let mut entry = Entry::item(section, node, label);
    if let Some(body) = context.child(node, "declaration_list")? {
        entry.item_mut().children = members(body, context)?;
    }
    Ok(Some(entry))
}

fn extract_function(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    Ok(Some(Entry::item(
        Section::Function,
        node,
        compact_whitespace(&format!(
            "function {}{}{}",
            context.text(name),
            parameters(node, context)?,
            return_type(node, context)?
        )),
    )))
}

fn extract_constant(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let mut names = Vec::new();
    for child in context.children(node)? {
        if child.kind() == "const_element"
            && let Some(name) = context.child(child, "name")?
        {
            names.push(context.text(name));
        }
    }
    if names.is_empty() {
        return Ok(None);
    }
    Ok(Some(Entry::item(
        Section::Constant,
        node,
        compact_whitespace(&prefixed(&modifiers(node, context)?, names.join(", "))),
    )))
}

fn extract_enum(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let backing = context
        .child(node, "primitive_type")?
        .map_or(String::new(), |backing| {
            format!(": {}", context.text(backing))
        });
    let interfaces = if let Some(clause) = context.child(node, "class_interface_clause")? {
        let names = context
            .children(clause)?
            .into_iter()
            .filter(|child| matches!(child.kind(), "qualified_name" | "name"))
            .map(|child| context.text(child))
            .collect::<Vec<_>>();
        if names.is_empty() {
            String::new()
        } else {
            format!(" implements {}", names.join(", "))
        }
    } else {
        String::new()
    };
    let mut entry = Entry::item(
        Section::Type,
        node,
        compact_whitespace(&format!("enum {}{backing}{interfaces}", context.text(name))),
    );
    if let Some(body) = context.child(node, "enum_declaration_list")? {
        entry.item_mut().children = extract_enum_variants(body, context, "enum_case")?;
        entry.item_mut().child_kind = ChildKind::Brief;
    }
    Ok(Some(entry))
}
