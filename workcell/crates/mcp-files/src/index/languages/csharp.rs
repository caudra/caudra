use tree_sitter::Node;

use super::common::{
    ExtractResult, LanguageSpec, extract_enum_variants, simple_import, strip_keyword_whitespace,
    trim_ascii_whitespace,
};
use crate::index::{
    model::{ChildKind, Entry, Section},
    render::{FIELD_TRUNCATE_THRESHOLD, compact_whitespace, prefixed, ranged, truncated_message},
    traversal::Context,
};

const MODIFIERS: &[&str] = &[
    "public",
    "private",
    "protected",
    "internal",
    "static",
    "abstract",
    "sealed",
    "override",
    "virtual",
    "async",
    "readonly",
    "extern",
    "partial",
    "new",
    "unsafe",
    "volatile",
];

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new(".", extract_nodes);
    spec.is_doc_comment = Some(|node, context| {
        node.kind() == "single_line_doc_comment"
            || (node.kind() == "comment" && context.text(node).starts_with("///"))
    });
    spec
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    let entry = match node.kind() {
        "using_directive" => {
            let text = context.text(node);
            Some(simple_import(
                node,
                trim_ascii_whitespace(
                    trim_ascii_whitespace(strip_keyword_whitespace(text, "using"))
                        .trim_end_matches(';'),
                ),
                '.',
            ))
        }
        "namespace_declaration" | "file_scoped_namespace_declaration" => context
            .field(node, "name")?
            .map(|name| Entry::item(Section::Module, node, context.text(name))),
        "class_declaration" => extract_class(node, context, Section::Class, "class")?,
        "struct_declaration" => extract_class(node, context, Section::Type, "struct")?,
        "interface_declaration" => extract_interface(node, context)?,
        "enum_declaration" => extract_enum(node, context)?,
        "record_declaration" => extract_record(node, context)?,
        _ => None,
    };
    Ok(entry.into_iter().collect())
}

fn modifiers(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let mut output = Vec::new();
    for child in context.children(node)? {
        if child.kind() == "attribute_list"
            || (child.kind() == "modifier" && MODIFIERS.contains(&context.text(child)))
        {
            output.push(context.text(child));
        }
    }
    Ok(output.join(" "))
}

fn bases(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    Ok(context
        .child(node, "base_list")?
        .map_or(String::new(), |base| {
            format!(
                " : {}",
                trim_ascii_whitespace(
                    trim_ascii_whitespace(context.text(base)).trim_start_matches(':')
                )
            )
        }))
}

fn method_signature(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<String>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let return_type = context
        .field(node, "returns")?
        .or(context.field(node, "type")?)
        .map_or(String::new(), |return_type| {
            format!("{} ", context.text(return_type))
        });
    let parameters = context
        .child(node, "parameter_list")?
        .map_or("()", |parameters| context.text(parameters));
    Ok(Some(compact_whitespace(&prefixed(
        &modifiers(node, context)?,
        format!("{return_type}{}{parameters}", context.text(name)),
    ))))
}

fn field_text(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let Some(declaration) = context.child(node, "variable_declaration")? else {
        return Ok(compact_whitespace(&prefixed(
            &modifiers(node, context)?,
            context.text(node),
        )));
    };
    let field_type = context
        .field(declaration, "type")?
        .map_or("_", |field_type| context.text(field_type));
    let mut names = Vec::new();
    for child in context.children(declaration)? {
        if child.kind() == "variable_declarator"
            && let Some(name) = context.field(child, "name")?
        {
            names.push(context.text(name));
        }
    }
    let names = if names.is_empty() {
        "_".to_owned()
    } else {
        names.join(", ")
    };
    Ok(compact_whitespace(&prefixed(
        &modifiers(node, context)?,
        format!("{field_type} {names}"),
    )))
}

fn property_text(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let field_type = context
        .field(node, "type")?
        .map_or(String::new(), |field_type| {
            format!("{} ", context.text(field_type))
        });
    let name = context
        .field(node, "name")?
        .map_or("_", |name| context.text(name));
    Ok(compact_whitespace(&prefixed(
        &modifiers(node, context)?,
        format!("{field_type}{name}"),
    )))
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
        } else if child.kind() == "property_declaration"
            || (!interface && child.kind() == "field_declaration")
        {
            fields += 1;
            if fields <= FIELD_TRUNCATE_THRESHOLD {
                output.push(ranged(
                    if child.kind() == "property_declaration" {
                        property_text(child, context)?
                    } else {
                        field_text(child, context)?
                    },
                    context.range(child),
                ));
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
    keyword: &str,
) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let mut entry = Entry::item(
        section,
        node,
        compact_whitespace(&prefixed(
            &modifiers(node, context)?,
            format!("{keyword} {} {}", context.text(name), bases(node, context)?),
        )),
    );
    if let Some(body) = context.child(node, "declaration_list")? {
        entry.item_mut().children = members(body, context, false)?;
    }
    Ok(Some(entry))
}

fn extract_interface(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let mut entry = Entry::item(
        Section::Trait,
        node,
        compact_whitespace(&prefixed(
            &modifiers(node, context)?,
            format!("interface {}{}", context.text(name), bases(node, context)?),
        )),
    );
    if let Some(body) = context.child(node, "declaration_list")? {
        entry.item_mut().children = members(body, context, true)?;
    }
    Ok(Some(entry))
}

fn extract_enum(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let mut entry = Entry::item(
        Section::Type,
        node,
        compact_whitespace(&prefixed(
            &modifiers(node, context)?,
            format!("enum {}", context.text(name)),
        )),
    );
    if let Some(body) = context.child(node, "enum_member_declaration_list")? {
        entry.item_mut().children =
            extract_enum_variants(body, context, "enum_member_declaration")?;
        entry.item_mut().child_kind = ChildKind::Brief;
    }
    Ok(Some(entry))
}

fn extract_record(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let parameters = context
        .child(node, "parameter_list")?
        .map_or("", |parameters| context.text(parameters));
    Ok(Some(Entry::item(
        Section::Type,
        node,
        compact_whitespace(&prefixed(
            &modifiers(node, context)?,
            format!(
                "record {}{parameters}{}",
                context.text(name),
                bases(node, context)?
            ),
        )),
    )))
}
