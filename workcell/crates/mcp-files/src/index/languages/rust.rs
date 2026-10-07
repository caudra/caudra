use tree_sitter::Node;

use super::common::{ExtractResult, LanguageSpec};
use crate::index::{
    model::{ChildKind, Entry, Section},
    render::{
        FIELD_TRUNCATE_THRESHOLD, compact_whitespace, expand_import, prefixed, ranged,
        truncated_message,
    },
    traversal::Context,
};

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new("::", extract_nodes);
    spec.is_attr = Some(is_attr);
    spec.is_doc_comment = Some(is_doc_comment);
    spec.is_module_doc = Some(is_module_doc);
    spec.is_test_node = Some(is_test_node);
    spec
}

fn is_attr(node: Node<'_>, _context: &Context<'_>) -> bool {
    node.kind() == "attribute_item"
}

fn is_doc_comment(node: Node<'_>, context: &Context<'_>) -> bool {
    let text = context.text(node);
    node.kind() == "line_comment" && text.starts_with("///") && !text.starts_with("////")
}

fn is_module_doc(node: Node<'_>, context: &Context<'_>) -> bool {
    node.kind() == "line_comment" && context.text(node).starts_with("//!")
}

fn is_test_node(node: Node<'_>, context: &Context<'_>, attrs: &[Node<'_>]) -> bool {
    matches!(node.kind(), "mod_item" | "function_item")
        && attrs.iter().any(|attr| {
            let text = context.text(*attr);
            matches!(text, "#[test]" | "#[cfg(test)]") || text.ends_with("::test]")
        })
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    let entry = match node.kind() {
        "use_declaration" => Some(extract_import(node, context)?),
        "struct_item" | "enum_item" | "union_item" => extract_type(node, context, attrs)?,
        "function_item" => function_signature(node, context)?.map(|signature| {
            Entry::item(
                Section::Function,
                node,
                prefixed(&visibility(node, context).unwrap_or_default(), signature),
            )
        }),
        "trait_item" => extract_trait(node, context)?,
        "impl_item" => extract_impl(node, context)?,
        "const_item" | "static_item" => extract_constant(node, context)?,
        "mod_item" => context.field(node, "name")?.map(|name| {
            Entry::item(
                Section::Module,
                node,
                prefixed(
                    &visibility(node, context).unwrap_or_default(),
                    context.text(name),
                ),
            )
        }),
        "macro_definition" => context
            .field(node, "name")?
            .map(|name| Entry::item(Section::Macro, node, format!("{}!", context.text(name)))),
        "type_item" => extract_alias(node, context)?,
        _ => None,
    };
    Ok(entry.into_iter().collect())
}

fn visibility(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    Ok(context
        .children(node)?
        .into_iter()
        .find(|child| child.kind() == "visibility_modifier")
        .map_or(String::new(), |child| context.text(child).to_owned()))
}

fn function_signature(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<String>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let parameters = context
        .child(node, "parameters")?
        .map_or("()", |parameters| context.text(parameters));
    let return_type = context
        .field(node, "return_type")?
        .map_or(String::new(), |return_type| {
            let text = context.text(return_type);
            if text.starts_with("->") {
                format!(" {text}")
            } else {
                format!(" -> {text}")
            }
        });
    Ok(Some(compact_whitespace(&format!(
        "{}{parameters}{return_type}",
        context.text(name)
    ))))
}

fn extract_import(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    let argument = context.children(node)?.into_iter().find(|child| {
        matches!(
            child.kind(),
            "scoped_identifier" | "use_wildcard" | "use_list" | "scoped_use_list" | "identifier"
        )
    });
    let text = argument.map_or_else(
        || {
            context
                .text(node)
                .strip_prefix("use ")
                .unwrap_or(context.text(node))
                .trim_end_matches(';')
                .to_owned()
        },
        |argument| context.text(argument).to_owned(),
    );
    Ok(Entry::import(
        node,
        expand_import(&text, "::", context)?,
        None,
    ))
}

fn extract_type(
    node: Node<'_>,
    context: &Context<'_>,
    attrs: &[Node<'_>],
) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let generics = context
        .child(node, "type_parameters")?
        .map_or("", |generics| context.text(generics));
    let kind = node.kind().trim_end_matches("_item");
    let mut entry = Entry::item(
        Section::Type,
        node,
        prefixed(
            &visibility(node, context)?,
            format!("{kind} {}{generics}", context.text(name)),
        ),
    );
    entry.item_mut().attrs = attrs
        .iter()
        .map(|attr| context.text(*attr))
        .filter(|text| text.contains("derive") || text.contains("cfg"))
        .map(str::to_owned)
        .collect();
    let body = context
        .child(node, "field_declaration_list")?
        .or(context.child(node, "enum_variant_list")?);
    if let Some(body) = body {
        let mut total = 0usize;
        for child in context.children(body)? {
            match child.kind() {
                "field_declaration" => {
                    total += 1;
                    let visibility = visibility(child, context)?;
                    if total <= FIELD_TRUNCATE_THRESHOLD || !visibility.is_empty() {
                        let name = context
                            .field(child, "name")?
                            .map_or("_", |name| context.text(name));
                        let field_type = context
                            .field(child, "type")?
                            .map_or("_", |field_type| context.text(field_type));
                        entry
                            .item_mut()
                            .children
                            .push(prefixed(&visibility, format!("{name}: {field_type}")).into());
                    }
                }
                "enum_variant" => {
                    total += 1;
                    if total <= FIELD_TRUNCATE_THRESHOLD {
                        entry.item_mut().children.push(
                            context
                                .field(child, "name")?
                                .map_or("_", |name| context.text(name))
                                .to_owned()
                                .into(),
                        );
                    }
                }
                _ => {}
            }
        }
        if total > FIELD_TRUNCATE_THRESHOLD && entry.item_mut().children.len() < total {
            entry
                .item_mut()
                .children
                .push(truncated_message(total).into());
        }
        if node.kind() == "enum_item" {
            entry.item_mut().child_kind = ChildKind::Brief;
        }
    }
    Ok(Some(entry))
}

fn methods(
    node: Node<'_>,
    context: &Context<'_>,
    include_visibility: bool,
) -> ExtractResult<Vec<crate::index::model::Child>> {
    let Some(body) = context.child(node, "declaration_list")? else {
        return Ok(Vec::new());
    };
    let mut methods = Vec::new();
    for child in context.children(body)? {
        if matches!(child.kind(), "function_item" | "function_signature_item")
            && let Some(signature) = function_signature(child, context)?
        {
            let signature = if include_visibility {
                prefixed(&visibility(child, context)?, signature)
            } else {
                signature
            };
            methods.push(ranged(signature, context.range(child)));
        }
    }
    Ok(methods)
}

fn extract_trait(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let generics = context
        .child(node, "type_parameters")?
        .map_or("", |generics| context.text(generics));
    let mut entry = Entry::item(
        Section::Trait,
        node,
        prefixed(
            &visibility(node, context)?,
            format!("{}{generics}", context.text(name)),
        ),
    );
    entry.item_mut().children = methods(node, context, false)?;
    Ok(Some(entry))
}

fn extract_impl(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(impl_type) = context
        .field(node, "type")?
        .or(context.child(node, "type_identifier")?)
    else {
        return Ok(None);
    };
    let text = context.field(node, "trait")?.map_or_else(
        || context.text(impl_type).to_owned(),
        |trait_node| {
            format!(
                "{} for {}",
                context.text(trait_node),
                context.text(impl_type)
            )
        },
    );
    let mut entry = Entry::item(Section::Impl, node, text);
    entry.item_mut().children = methods(node, context, true)?;
    Ok(Some(entry))
}

fn extract_constant(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let field_type = context
        .field(node, "type")?
        .map_or(String::new(), |field_type| {
            format!(": {}", context.text(field_type))
        });
    let static_prefix = if node.kind() == "static_item" {
        "static "
    } else {
        ""
    };
    Ok(Some(Entry::item(
        Section::Constant,
        node,
        prefixed(
            &visibility(node, context)?,
            format!("{static_prefix}{}{field_type}", context.text(name)),
        ),
    )))
}

fn extract_alias(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let value = context
        .field(node, "type")?
        .map_or(String::new(), |value| format!(" = {}", context.text(value)));
    Ok(Some(Entry::item(
        Section::Type,
        node,
        prefixed(
            &visibility(node, context)?,
            format!("type {}{value}", context.text(name)),
        ),
    )))
}
