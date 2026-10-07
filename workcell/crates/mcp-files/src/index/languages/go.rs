use tree_sitter::Node;

use super::common::{
    ExtractResult, LanguageSpec, extract_fields_truncated, split_path, strip_delimited,
};
use crate::index::{
    model::{Entry, Section},
    render::{compact_whitespace, ranged, truncate},
    traversal::Context,
};

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new("/", extract_nodes);
    spec.is_doc_comment = Some(|node, _| node.kind() == "comment");
    spec
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    match node.kind() {
        "import_declaration" => Ok(extract_import(node, context)?.into_iter().collect()),
        "function_declaration" => Ok(extract_function(node, context)?.into_iter().collect()),
        "method_declaration" => Ok(extract_method(node, context)?.into_iter().collect()),
        "type_declaration" => extract_types(node, context),
        "const_declaration" => extract_constants(node, context, false),
        "var_declaration" => extract_constants(node, context, true),
        _ => Ok(Vec::new()),
    }
}

fn parameters_result(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let parameters = context
        .field(node, "parameters")?
        .map_or("()", |parameters| context.text(parameters));
    let result = context
        .field(node, "result")?
        .map_or(String::new(), |result| format!(" {}", context.text(result)));
    Ok(compact_whitespace(&format!("{parameters}{result}")))
}

fn extract_import(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let specs = if let Some(list) = context.child(node, "import_spec_list")? {
        context.children(list)?
    } else if let Some(spec) = context.child(node, "import_spec")? {
        vec![spec]
    } else {
        Vec::new()
    };
    let mut paths = Vec::new();
    for spec in specs {
        if spec.kind() == "import_spec"
            && let Some(path) = context.field(spec, "path")?
        {
            let raw_path = context.text(path);
            let stripped = strip_delimited(raw_path, "\"")
                .or_else(|| strip_delimited(raw_path, "'"))
                .unwrap_or(raw_path);
            let path = if stripped.is_empty() {
                raw_path
            } else {
                stripped
            };
            paths.push(split_path(path, '/'));
        }
    }
    Ok((!paths.is_empty()).then(|| Entry::import(node, paths, None)))
}

fn extract_function(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    Ok(context.field(node, "name")?.map(|name| {
        Entry::item(
            Section::Function,
            node,
            format!(
                "{}{}",
                context.text(name),
                parameters_result(node, context).unwrap_or_default()
            ),
        )
    }))
}

fn extract_method(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let receiver = context
        .field(node, "receiver")?
        .map_or(String::new(), |receiver| {
            format!("{} ", context.text(receiver))
        });
    Ok(Some(Entry::item(
        Section::Impl,
        node,
        format!(
            "{receiver}{}{}",
            context.text(name),
            parameters_result(node, context)?
        ),
    )))
}

fn extract_types(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Vec<Entry>> {
    let mut entries = Vec::new();
    for child in context.children(node)? {
        match child.kind() {
            "type_spec" => {
                let (Some(name), Some(field_type)) =
                    (context.field(child, "name")?, context.field(child, "type")?)
                else {
                    continue;
                };
                match field_type.kind() {
                    "struct_type" => {
                        let mut entry = Entry::item(
                            Section::Type,
                            child,
                            format!("struct {}", context.text(name)),
                        );
                        if let Some(body) = context.child(field_type, "field_declaration_list")? {
                            entry.item_mut().children = extract_fields_truncated(
                                body,
                                context,
                                "field_declaration",
                                |field, context| {
                                    let name = context
                                        .field(field, "name")?
                                        .map_or("_", |name| context.text(name));
                                    let field_type = context
                                        .field(field, "type")?
                                        .map_or("_", |field_type| context.text(field_type));
                                    Ok(format!("{name} {field_type}"))
                                },
                            )?;
                        }
                        entries.push(entry);
                    }
                    "interface_type" => {
                        let mut entry = Entry::item(
                            Section::Type,
                            child,
                            format!("interface {}", context.text(name)),
                        );
                        for member in context.children(field_type)? {
                            match member.kind() {
                                "method_elem" => {
                                    if let Some(name) = context.field(member, "name")? {
                                        entry.item_mut().children.push(ranged(
                                            compact_whitespace(&format!(
                                                "{}{}",
                                                context.text(name),
                                                parameters_result(member, context)?
                                            )),
                                            context.range(member),
                                        ));
                                    }
                                }
                                "type_elem" => entry.item_mut().children.push(ranged(
                                    truncate(context.text(member), 60),
                                    context.range(member),
                                )),
                                _ => {}
                            }
                        }
                        entries.push(entry);
                    }
                    _ => entries.push(Entry::item(
                        Section::Type,
                        child,
                        format!(
                            "{} {}",
                            context.text(name),
                            truncate(context.text(field_type), 60)
                        ),
                    )),
                }
            }
            "type_alias" => {
                if let (Some(name), Some(field_type)) =
                    (context.field(child, "name")?, context.field(child, "type")?)
                {
                    entries.push(Entry::item(
                        Section::Type,
                        child,
                        format!("type {} = {}", context.text(name), context.text(field_type)),
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(entries)
}

fn extract_constants(
    node: Node<'_>,
    context: &Context<'_>,
    variable: bool,
) -> ExtractResult<Vec<Entry>> {
    let list_kind = if variable {
        "var_spec_list"
    } else {
        "const_spec_list"
    };
    let spec_kind = if variable { "var_spec" } else { "const_spec" };
    let list = context.child(node, list_kind)?.unwrap_or(node);
    let mut entries = Vec::new();
    for child in context.children(list)? {
        if child.kind() == spec_kind
            && let Some(name) = context.field(child, "name")?
        {
            let field_type = context
                .field(child, "type")?
                .map_or(String::new(), |field_type| {
                    format!(" {}", context.text(field_type))
                });
            entries.push(Entry::item(
                Section::Constant,
                child,
                format!(
                    "{}{}{field_type}",
                    if variable { "var " } else { "" },
                    context.text(name)
                ),
            ));
        }
    }
    Ok(entries)
}
