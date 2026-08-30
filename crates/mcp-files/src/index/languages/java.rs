use tree_sitter::Node;

use super::common::{
    ExtractResult, LanguageSpec, extract_enum_variants, simple_import, strip_keyword_whitespace,
    trim_ascii_whitespace,
};
use crate::index::{
    model::{ChildKind, Entry, Section},
    render::{FIELD_TRUNCATE_THRESHOLD, compact_whitespace, ranged, truncated_message},
    traversal::Context,
};

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new(".", extract_nodes);
    spec.is_doc_comment = Some(|node, context| {
        node.kind() == "block_comment" && context.text(node).starts_with("/**")
    });
    spec
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    let entry = match node.kind() {
        "import_declaration" => {
            let text = context.text(node);
            let cleaned = trim_ascii_whitespace(strip_keyword_whitespace(text, "import"));
            let cleaned = trim_ascii_whitespace(cleaned.trim_end_matches(';'));
            Some(simple_import(node, cleaned, '.'))
        }
        "package_declaration" => {
            let text = context.text(node);
            Some(Entry::item(
                Section::Module,
                node,
                trim_ascii_whitespace(
                    trim_ascii_whitespace(strip_keyword_whitespace(text, "package"))
                        .trim_end_matches(';'),
                ),
            ))
        }
        "class_declaration" => extract_class(node, context)?,
        "interface_declaration" => extract_interface(node, context)?,
        "enum_declaration" => extract_enum(node, context)?,
        "record_declaration" => extract_record(node, context)?,
        "annotation_type_declaration" => extract_annotation(node, context)?,
        _ => None,
    };
    Ok(entry.into_iter().collect())
}

fn modifiers(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let Some(modifiers) = context.child(node, "modifiers")? else {
        return Ok(String::new());
    };
    let mut parts = Vec::new();
    for child in context.children(modifiers)? {
        let text = context.text(child);
        if matches!(child.kind(), "marker_annotation" | "annotation")
            || matches!(
                text,
                "public"
                    | "private"
                    | "protected"
                    | "static"
                    | "final"
                    | "abstract"
                    | "default"
                    | "synchronized"
            )
        {
            parts.push(text);
        }
    }
    Ok(parts.join(" "))
}

fn type_parameters<'a>(node: Node<'_>, context: &'a Context<'a>) -> ExtractResult<&'a str> {
    Ok(context
        .child(node, "type_parameters")?
        .map_or("", |parameters| context.text(parameters)))
}

fn method_signature(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<String>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let return_type = context
        .field(node, "type")?
        .map_or(String::new(), |return_type| {
            format!("{} ", context.text(return_type))
        });
    let parameters = context
        .field(node, "parameters")?
        .map_or("()", |parameters| context.text(parameters));
    let body = format!("{return_type}{}{parameters}", context.text(name));
    let modifiers = modifiers(node, context)?;
    Ok(Some(compact_whitespace(if modifiers.is_empty() {
        &body
    } else {
        return Ok(Some(compact_whitespace(&format!("{modifiers} {body}"))));
    })))
}

fn field_text(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let field_type = context
        .field(node, "type")?
        .map_or("", |field_type| context.text(field_type));
    let name = if let Some(declaration) = context.child(node, "variable_declarator")? {
        context
            .field(declaration, "name")?
            .map_or("_", |name| context.text(name))
    } else {
        "_"
    };
    let body = format!("{field_type} {name}");
    let modifiers = modifiers(node, context)?;
    Ok(compact_whitespace(if modifiers.is_empty() {
        &body
    } else {
        return Ok(compact_whitespace(&format!("{modifiers} {body}")));
    }))
}

fn members(
    body: Node<'_>,
    context: &Context<'_>,
    interface: bool,
) -> ExtractResult<Vec<crate::index::model::Child>> {
    let mut output = Vec::new();
    let mut fields = 0usize;
    for child in context.children(body)? {
        if matches!(
            child.kind(),
            "method_declaration" | "constructor_declaration"
        ) {
            if let Some(signature) = method_signature(child, context)? {
                output.push(ranged(signature, context.range(child)));
            }
        } else if child.kind() == "field_declaration"
            || (interface && child.kind() == "constant_declaration")
        {
            fields += 1;
            if fields <= FIELD_TRUNCATE_THRESHOLD {
                output.push(ranged(field_text(child, context)?, context.range(child)));
            }
        }
    }
    if fields > FIELD_TRUNCATE_THRESHOLD {
        output.push(truncated_message(fields).into());
    }
    Ok(output)
}

fn type_list(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    Ok(context.child(node, "type_list")?.map_or_else(
        || {
            context
                .text(node)
                .trim_start_matches("extends")
                .trim_start_matches("implements")
                .trim()
                .to_owned()
        },
        |list| context.text(list).to_owned(),
    ))
}

fn interfaces(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    Ok(
        if let Some(interfaces) = context.field(node, "interfaces")? {
            format!(" implements {}", type_list(interfaces, context)?)
        } else {
            String::new()
        },
    )
}

fn extract_class(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let superclass = if let Some(superclass) = context.field(node, "superclass")? {
        let value = context
            .child(superclass, "type_identifier")?
            .unwrap_or(superclass);
        format!(" extends {}", context.text(value))
    } else {
        String::new()
    };
    let body = format!(
        "class {}{}{superclass}{}",
        context.text(name),
        type_parameters(node, context)?,
        interfaces(node, context)?
    );
    let modifiers = modifiers(node, context)?;
    let mut entry = Entry::item(
        Section::Class,
        node,
        compact_whitespace(if modifiers.is_empty() {
            &body
        } else {
            return build_class_entry(
                node,
                context,
                compact_whitespace(&format!("{modifiers} {body}")),
                "class_body",
                false,
            );
        }),
    );
    if let Some(class_body) = context.child(node, "class_body")? {
        entry.item_mut().children = members(class_body, context, false)?;
    }
    Ok(Some(entry))
}

fn build_class_entry(
    node: Node<'_>,
    context: &Context<'_>,
    label: String,
    body_kind: &str,
    interface: bool,
) -> ExtractResult<Option<Entry>> {
    let mut entry = Entry::item(
        if interface {
            Section::Trait
        } else {
            Section::Class
        },
        node,
        label,
    );
    if let Some(body) = context.child(node, body_kind)? {
        entry.item_mut().children = members(body, context, interface)?;
    }
    Ok(Some(entry))
}

fn extract_interface(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let extension = if let Some(extension) = context.child(node, "extends_interfaces")? {
        format!(" extends {}", type_list(extension, context)?)
    } else {
        String::new()
    };
    let body = format!(
        "interface {}{}{extension}",
        context.text(name),
        type_parameters(node, context)?
    );
    let modifiers = modifiers(node, context)?;
    build_class_entry(
        node,
        context,
        compact_whitespace(if modifiers.is_empty() {
            &body
        } else {
            return build_class_entry(
                node,
                context,
                compact_whitespace(&format!("{modifiers} {body}")),
                "interface_body",
                true,
            );
        }),
        "interface_body",
        true,
    )
}

fn extract_enum(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let body = format!(
        "enum {}{}{}",
        context.text(name),
        type_parameters(node, context)?,
        interfaces(node, context)?
    );
    let modifiers = modifiers(node, context)?;
    let label = compact_whitespace(if modifiers.is_empty() {
        &body
    } else {
        return extract_enum_with_label(
            node,
            context,
            compact_whitespace(&format!("{modifiers} {body}")),
        );
    });
    extract_enum_with_label(node, context, label)
}

fn extract_enum_with_label(
    node: Node<'_>,
    context: &Context<'_>,
    label: String,
) -> ExtractResult<Option<Entry>> {
    let mut entry = Entry::item(Section::Type, node, label);
    if let Some(body) = context.child(node, "enum_body")? {
        entry.item_mut().children = extract_enum_variants(body, context, "enum_constant")?;
        entry.item_mut().child_kind = ChildKind::Brief;
    }
    Ok(Some(entry))
}

fn extract_record(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let parameters = context
        .child(node, "formal_parameters")?
        .map_or("", |parameters| context.text(parameters));
    let body = format!(
        "record {}{}{parameters}{}",
        context.text(name),
        type_parameters(node, context)?,
        interfaces(node, context)?
    );
    let modifiers = modifiers(node, context)?;
    Ok(Some(Entry::item(
        Section::Class,
        node,
        compact_whitespace(if modifiers.is_empty() {
            &body
        } else {
            return Ok(Some(Entry::item(
                Section::Class,
                node,
                compact_whitespace(&format!("{modifiers} {body}")),
            )));
        }),
    )))
}

fn extract_annotation(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let body = format!("@interface {}", context.text(name));
    let modifiers = modifiers(node, context)?;
    Ok(Some(Entry::item(
        Section::Type,
        node,
        compact_whitespace(if modifiers.is_empty() {
            &body
        } else {
            return Ok(Some(Entry::item(
                Section::Type,
                node,
                compact_whitespace(&format!("{modifiers} {body}")),
            )));
        }),
    )))
}
