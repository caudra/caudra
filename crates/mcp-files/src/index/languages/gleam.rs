use tree_sitter::Node;

use super::common::{ExtractResult, LanguageSpec, extract_fields_truncated, split_path};
use crate::index::{
    model::{ChildKind, Entry, Section},
    render::compact_whitespace,
    traversal::Context,
};

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new("/", extract_nodes);
    spec.is_doc_comment = Some(|node, _| node.kind() == "statement_comment");
    spec.is_module_doc = Some(|node, _| node.kind() == "module_comment");
    spec.is_attr = Some(|node, _| node.kind() == "attribute");
    spec
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    let entry = match node.kind() {
        "import" => extract_import(node, context)?,
        "constant" => extract_constant(node, context)?,
        "function" => extract_function(node, context, "fn ")?,
        "external_function" => extract_function(node, context, "external fn ")?,
        "type_definition" => Some(extract_type_definition(node, context)?),
        "type_alias" => Some(extract_named_type(node, context, "type ")?),
        "external_type" => Some(extract_named_type(node, context, "external type ")?),
        _ => None,
    };
    Ok(entry.into_iter().collect())
}

fn extract_import(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(module) = context.field(node, "module")? else {
        return Ok(None);
    };
    let base = split_path(context.text(module), '/');
    let imports = context.field(node, "imports")?;
    let alias = context.field(node, "alias")?;
    let paths = if let Some(imports) = imports {
        let mut names = Vec::new();
        for child in context.children(imports)? {
            if child.kind() == "unqualified_import"
                && let Some(name) = context.field(child, "name")?
            {
                let mut name = context.text(name).to_owned();
                if let Some(child_alias) = context.field(child, "alias")? {
                    name.push_str(" as ");
                    name.push_str(context.text(child_alias));
                }
                names.push(name);
            }
        }
        if names.is_empty() {
            Vec::new()
        } else {
            let mut path = base;
            let mut label = names.join(", ");
            if let Some(alias) = alias {
                label.push_str(" as ");
                label.push_str(context.text(alias));
            }
            path.push(label);
            vec![path]
        }
    } else if let Some(alias) = alias {
        let mut path = base;
        path.push(format!("as {}", context.text(alias)));
        vec![path]
    } else {
        vec![base]
    };
    Ok(Some(Entry::import(node, paths, None)))
}

fn extract_constant(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let field_type = context
        .field(node, "type")?
        .map_or(String::new(), |field_type| {
            format!(": {}", compact_whitespace(context.text(field_type)))
        });
    Ok(Some(Entry::item(
        Section::Constant,
        node,
        format!("const {}{field_type}", context.text(name)),
    )))
}

fn extract_function(
    node: Node<'_>,
    context: &Context<'_>,
    prefix: &str,
) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let parameters = context
        .field(node, "parameters")?
        .map_or("()".to_owned(), |parameters| {
            compact_whitespace(context.text(parameters))
        });
    let return_type = context
        .field(node, "return_type")?
        .map_or(String::new(), |return_type| {
            format!(" -> {}", compact_whitespace(context.text(return_type)))
        });
    Ok(Some(Entry::item(
        Section::Function,
        node,
        format!("{prefix}{}{parameters}{return_type}", context.text(name)),
    )))
}

fn type_label(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let name = context
        .field(node, "name")?
        .map_or("", |name| context.text(name));
    let parameters = context
        .child(node, "type_parameters")?
        .map_or("", |parameters| context.text(parameters));
    Ok(format!("{name}{parameters}"))
}

fn extract_type_definition(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    let opaque = context
        .children(node)?
        .iter()
        .any(|child| child.kind() == "opacity_modifier");
    let name = context
        .child(node, "type_name")?
        .map(|name| type_label(name, context))
        .transpose()?
        .unwrap_or_default();
    let mut entry = Entry::item(
        Section::Type,
        node,
        format!("{}{name}", if opaque { "opaque type " } else { "type " }),
    );
    if let Some(constructors) = context.child(node, "data_constructors")? {
        entry.item_mut().children = extract_fields_truncated(
            constructors,
            context,
            "data_constructor",
            |field, context| {
                Ok(context
                    .field(field, "name")?
                    .map_or("_", |name| context.text(name))
                    .to_owned())
            },
        )?;
        entry.item_mut().child_kind = ChildKind::Brief;
    }
    Ok(entry)
}

fn extract_named_type(node: Node<'_>, context: &Context<'_>, prefix: &str) -> ExtractResult<Entry> {
    let name = context
        .child(node, "type_name")?
        .map(|name| type_label(name, context))
        .transpose()?
        .unwrap_or_default();
    Ok(Entry::item(Section::Type, node, format!("{prefix}{name}")))
}
