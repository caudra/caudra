use tree_sitter::Node;

use super::common::{
    ExtractResult, LanguageSpec, extract_enum_variants, extract_fields_truncated, split_path,
};
use crate::index::{
    model::{ChildKind, Entry, Section},
    render::{compact_whitespace, truncate},
    traversal::Context,
};

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new("/", extract_nodes);
    spec.is_doc_comment = Some(|node, context| {
        node.kind() == "comment"
            && (context.text(node).starts_with("/**") || context.text(node).starts_with("///"))
    });
    spec
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    match node.kind() {
        "preproc_include" => Ok(extract_include(node, context)?.into_iter().collect()),
        "preproc_def" => Ok(extract_define(node, context)?.into_iter().collect()),
        "preproc_function_def" => Ok(extract_function_macro(node, context)?.into_iter().collect()),
        "function_definition" => Ok(function_signature(node, context)?
            .map(|signature| Entry::item(Section::Function, node, signature))
            .into_iter()
            .collect()),
        "struct_specifier" => Ok(vec![extract_struct(node, context, "struct")?]),
        "union_specifier" => Ok(vec![extract_struct(node, context, "union")?]),
        "enum_specifier" => Ok(vec![extract_enum(node, context)?]),
        "type_definition" => Ok(extract_typedef(node, context)?.into_iter().collect()),
        "declaration" => Ok(function_signature(node, context)?
            .map(|signature| Entry::item(Section::Function, node, signature))
            .into_iter()
            .collect()),
        "preproc_ifdef"
        | "preproc_if"
        | "linkage_specification"
        | "declaration_list"
        | "translation_unit" => extract_children(node, context),
        _ => Ok(Vec::new()),
    }
}

fn extract_children(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Vec<Entry>> {
    let mut entries = Vec::new();
    for child in context.children(node)? {
        entries.extend(extract_nodes(child, context, &[])?);
    }
    Ok(entries)
}

fn extract_include(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    Ok(context.field(node, "path")?.map(|path| {
        let cleaned = context.text(path).replace(['<', '>', '"', '\''], "");
        Entry::import(node, vec![split_path(&cleaned, '/')], None)
    }))
}

fn function_declarator<'tree>(mut node: Node<'tree>) -> Option<Node<'tree>> {
    loop {
        match node.kind() {
            "function_declarator" => return Some(node),
            "pointer_declarator" => node = node.child_by_field_name("declarator")?,
            _ => return None,
        }
    }
}

fn function_signature(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<String>> {
    let Some(declaration) = context
        .field(node, "declarator")?
        .and_then(function_declarator)
    else {
        return Ok(None);
    };
    let Some(name) = context.field(declaration, "declarator")? else {
        return Ok(None);
    };
    let parameters = context
        .field(declaration, "parameters")?
        .map_or("()", |parameters| context.text(parameters));
    let return_type = context
        .field(node, "type")?
        .map_or(String::new(), |return_type| {
            format!("{} ", context.text(return_type))
        });
    Ok(Some(compact_whitespace(&format!(
        "{return_type}{}{parameters}",
        context.text(name)
    ))))
}

pub(super) fn extract_function_macro(
    node: Node<'_>,
    context: &Context<'_>,
) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let parameters = context
        .field(node, "parameters")?
        .map_or("", |parameters| context.text(parameters));
    Ok(Some(Entry::item(
        Section::Macro,
        node,
        format!("{}{parameters}", context.text(name)),
    )))
}

fn field_text(field: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    Ok(
        compact_whitespace(context.text(field).trim_end_matches(';'))
            .trim()
            .to_owned(),
    )
}

pub(super) fn extract_struct(
    node: Node<'_>,
    context: &Context<'_>,
    keyword: &str,
) -> ExtractResult<Entry> {
    let name = context
        .field(node, "name")?
        .map_or("", |name| context.text(name));
    let mut entry = Entry::item(
        Section::Type,
        node,
        format!(
            "{keyword}{}",
            if name.is_empty() {
                String::new()
            } else {
                format!(" {name}")
            }
        ),
    );
    if let Some(body) = context.child(node, "field_declaration_list")? {
        entry.item_mut().children =
            extract_fields_truncated(body, context, "field_declaration", field_text)?;
    }
    Ok(entry)
}

fn extract_enum(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    let name = context
        .field(node, "name")?
        .map_or("", |name| context.text(name));
    let mut entry = Entry::item(
        Section::Type,
        node,
        format!(
            "enum{}",
            if name.is_empty() {
                String::new()
            } else {
                format!(" {name}")
            }
        ),
    );
    if let Some(body) = context.child(node, "enumerator_list")? {
        entry.item_mut().children = extract_enum_variants(body, context, "enumerator")?;
        entry.item_mut().child_kind = ChildKind::Brief;
    }
    Ok(entry)
}

fn extract_typedef(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(field_type) = context.field(node, "type")? else {
        return Ok(None);
    };
    if let Some(entry) = typedef_with_body(node, field_type, context)? {
        return Ok(Some(entry));
    }
    Ok(Some(match field_type.kind() {
        "struct_specifier" | "union_specifier" => extract_struct(field_type, context, "struct")?,
        "enum_specifier" => extract_enum(field_type, context)?,
        _ => {
            let declaration = context
                .field(node, "declarator")?
                .map_or("", |declaration| context.text(declaration));
            Entry::item(
                Section::Type,
                node,
                compact_whitespace(&format!(
                    "typedef {} {declaration}",
                    context.text(field_type)
                )),
            )
        }
    }))
}

/// A `typedef` whose struct, union, or enum carries its body, rendered with the body's fields or
/// variants as children. `None` for every other typedef, which each language renders its own way.
pub(super) fn typedef_with_body(
    node: Node<'_>,
    field_type: Node<'_>,
    context: &Context<'_>,
) -> ExtractResult<Option<Entry>> {
    let (keyword, body_kind) = match field_type.kind() {
        "struct_specifier" => ("struct", "field_declaration_list"),
        "union_specifier" => ("union", "field_declaration_list"),
        "enum_specifier" => ("enum", "enumerator_list"),
        _ => return Ok(None),
    };
    let Some(body) = context.child(field_type, body_kind)? else {
        return Ok(None);
    };
    let name = context
        .field(field_type, "name")?
        .map_or("", |name| context.text(name));
    let inner = if name.is_empty() {
        keyword.to_owned()
    } else {
        format!("{keyword} {name}")
    };
    let declaration = context
        .field(node, "declarator")?
        .map_or("", |declaration| context.text(declaration));
    let mut entry = Entry::item(
        Section::Type,
        node,
        format!("typedef {inner} {declaration}"),
    );
    if keyword == "enum" {
        entry.item_mut().children = extract_enum_variants(body, context, "enumerator")?;
        entry.item_mut().child_kind = ChildKind::Brief;
    } else {
        entry.item_mut().children =
            extract_fields_truncated(body, context, "field_declaration", field_text)?;
    }
    Ok(Some(entry))
}

fn extract_define(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let value = context
        .field(node, "value")?
        .map_or(String::new(), |value| {
            format!(" {}", truncate(context.text(value), 40))
        });
    Ok(Some(Entry::item(
        Section::Constant,
        node,
        format!("{}{value}", context.text(name)),
    )))
}
