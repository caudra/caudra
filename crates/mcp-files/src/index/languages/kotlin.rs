use tree_sitter::Node;

use super::common::{ExtractResult, LanguageSpec};
use crate::index::{
    model::{Child, ChildKind, Entry, Section},
    render::{FIELD_TRUNCATE_THRESHOLD, compact_whitespace, ranged, truncated_message},
    traversal::Context,
};

pub(super) fn spec() -> LanguageSpec {
    let mut spec = LanguageSpec::new(".", extract_nodes);
    spec.is_doc_comment = Some(|node, context| {
        node.kind() == "multiline_comment" && context.text(node).starts_with("/**")
    });
    spec
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    match node.kind() {
        "import_header" | "import" => Ok(vec![extract_import(node, context)?]),
        "import_list" => {
            let mut imports = Vec::new();
            for child in context.children(node)? {
                if matches!(child.kind(), "import_header" | "import") {
                    imports.push(extract_import(child, context)?);
                }
            }
            Ok(imports)
        }
        "package_header" => Ok(vec![extract_package(node, context)?]),
        "class_declaration"
        | "object_declaration"
        | "function_declaration"
        | "property_declaration"
        | "type_alias" => Ok(extract_declaration(node, context)?.into_iter().collect()),
        "statements" => {
            let mut entries = Vec::new();
            for child in context.children(node)? {
                if matches!(
                    child.kind(),
                    "class_declaration"
                        | "object_declaration"
                        | "function_declaration"
                        | "property_declaration"
                        | "type_alias"
                ) && let Some(entry) = extract_declaration(child, context)?
                {
                    entries.push(entry);
                }
            }
            Ok(entries)
        }
        _ => Ok(Vec::new()),
    }
}

fn modifiers(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let Some(modifiers) = context.child(node, "modifiers")? else {
        return Ok(String::new());
    };
    Ok(context
        .children(modifiers)?
        .into_iter()
        .filter(|child| child.kind() != "annotation")
        .map(|child| context.text(child))
        .collect::<Vec<_>>()
        .join(" "))
}

fn type_parameters<'a>(node: Node<'_>, context: &'a Context<'a>) -> ExtractResult<&'a str> {
    Ok(context
        .child(node, "type_parameters")?
        .map_or("", |parameters| context.text(parameters)))
}

fn delegation(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    Ok(context
        .child(node, "delegation_specifiers")?
        .map_or(String::new(), |delegation| {
            format!(" : {}", context.text(delegation))
        }))
}

fn extract_import(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    let identifier = context
        .child(node, "qualified_identifier")?
        .or(context.child(node, "identifier")?);
    let Some(identifier) = identifier else {
        let text = context.text(node);
        return Ok(Entry::import(
            node,
            vec![vec![
                text.strip_prefix("import ")
                    .unwrap_or(text)
                    .trim()
                    .to_owned(),
            ]],
            None,
        ));
    };
    let mut parts = context
        .children(identifier)?
        .into_iter()
        .filter(|child| child.kind() == "identifier")
        .map(|child| context.text(child).to_owned())
        .collect::<Vec<_>>();
    if context
        .children(node)?
        .iter()
        .any(|child| context.text(*child) == "*")
    {
        parts.push("*".into());
    }
    Ok(Entry::import(node, vec![parts], None))
}

fn extract_package(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    let identifier = context
        .child(node, "qualified_identifier")?
        .or(context.child(node, "identifier")?);
    let name = identifier.map_or_else(
        || {
            context
                .text(node)
                .strip_prefix("package ")
                .unwrap_or(context.text(node))
                .trim()
        },
        |identifier| context.text(identifier),
    );
    Ok(Entry::item(Section::Module, node, name))
}

fn name_node<'tree>(
    node: Node<'tree>,
    context: &Context<'_>,
) -> ExtractResult<Option<Node<'tree>>> {
    Ok(context
        .field(node, "simple_identifier")?
        .or(context.field(node, "name")?)
        .or(context.child(node, "identifier")?))
}

fn function_signature(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<String>> {
    let Some(name) = name_node(node, context)? else {
        return Ok(None);
    };
    let parameters = context
        .child(node, "function_value_parameters")?
        .map_or("()", |parameters| context.text(parameters));
    let return_type = context
        .field(node, "type")?
        .or(context.child(node, "type")?)
        .map_or(String::new(), |return_type| {
            format!(" : {}", context.text(return_type))
        });
    let mut parts = Vec::new();
    let modifiers = modifiers(node, context)?;
    if !modifiers.is_empty() {
        parts.push(modifiers);
    }
    parts.push("fun".into());
    let parameters_type = type_parameters(node, context)?;
    if !parameters_type.is_empty() {
        parts.push(parameters_type.into());
    }
    parts.push(format!("{}{parameters}{return_type}", context.text(name)));
    Ok(Some(compact_whitespace(&parts.join(" "))))
}

fn property_text(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<String>> {
    let keyword = context
        .children(node)?
        .into_iter()
        .find_map(|child| matches!(context.text(child), "var" | "val").then(|| context.text(child)))
        .unwrap_or("val");
    let Some(declaration) = context
        .field(node, "variable_declaration")?
        .or(context.child(node, "variable_declaration")?)
    else {
        return Ok(None);
    };
    let name = context
        .field(declaration, "simple_identifier")?
        .or(context.child(declaration, "simple_identifier")?)
        .or(context.child(declaration, "identifier")?)
        .map_or("_", |name| context.text(name));
    let field_type = context
        .field(declaration, "type")?
        .or(context.child(declaration, "type")?)
        .map_or(String::new(), |field_type| {
            format!(" : {}", context.text(field_type))
        });
    let modifiers = modifiers(node, context)?;
    Ok(Some(compact_whitespace(&format!(
        "{}{keyword} {name}{field_type}",
        if modifiers.is_empty() {
            String::new()
        } else {
            format!("{modifiers} ")
        }
    ))))
}

fn class_members(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Vec<Child>> {
    let mut members = Vec::new();
    let mut properties = 0usize;
    for child in context.children(node)? {
        match child.kind() {
            "function_declaration" => {
                if let Some(signature) = function_signature(child, context)? {
                    members.push(ranged(signature, context.range(child)));
                }
            }
            "property_declaration" => {
                properties += 1;
                if properties <= FIELD_TRUNCATE_THRESHOLD
                    && let Some(text) = property_text(child, context)?
                {
                    members.push(ranged(text, context.range(child)));
                }
            }
            "companion_object" => {
                if let Some(body) = context.child(child, "class_body")? {
                    for member in class_members(body, context)? {
                        members.push(match member {
                            Child::Text(text) => format!("companion.{text}").into(),
                            Child::Ranged { body, range } => {
                                ranged(format!("companion.{body}"), range)
                            }
                            Child::Entry(entry) => Child::Entry(entry),
                        });
                    }
                }
            }
            "enum_entry" => {
                if let Some(name) = context.field(child, "simple_identifier")? {
                    members.push(context.text(name).to_owned().into());
                }
            }
            _ => {}
        }
    }
    if properties > FIELD_TRUNCATE_THRESHOLD {
        members.push(truncated_message(properties).into());
    }
    Ok(members)
}

fn extract_class(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = name_node(node, context)? else {
        return Ok(None);
    };
    let modifiers = modifiers(node, context)?;
    let mut keyword = if context
        .children(node)?
        .iter()
        .any(|child| context.text(*child) == "interface")
    {
        "interface"
    } else {
        "class"
    };
    if modifiers.contains("enum") {
        keyword = "enum class";
    }
    let constructor = if let Some(constructor) = context.child(node, "primary_constructor")? {
        let parameters = context
            .child(constructor, "class_parameters")?
            .unwrap_or(constructor);
        context.text(parameters)
    } else {
        ""
    };
    let mut parts = Vec::new();
    if !modifiers.is_empty() {
        parts.push(modifiers);
    }
    parts.push(keyword.into());
    let parameters = type_parameters(node, context)?;
    if !parameters.is_empty() {
        parts.push(parameters.into());
    }
    parts.push(format!(
        "{}{constructor}{}",
        context.text(name),
        delegation(node, context)?
    ));
    let mut entry = Entry::item(Section::Class, node, compact_whitespace(&parts.join(" ")));
    if let Some(body) = context
        .child(node, "class_body")?
        .or(context.child(node, "enum_class_body")?)
    {
        entry.item_mut().children = class_members(body, context)?;
        if keyword == "enum class" {
            entry.item_mut().child_kind = ChildKind::Brief;
        }
    }
    Ok(Some(entry))
}

fn extract_object(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = name_node(node, context)? else {
        return Ok(None);
    };
    let modifiers = modifiers(node, context)?;
    let mut entry = Entry::item(
        Section::Class,
        node,
        compact_whitespace(&format!(
            "{}object {}{}",
            if modifiers.is_empty() {
                String::new()
            } else {
                format!("{modifiers} ")
            },
            context.text(name),
            delegation(node, context)?
        )),
    );
    if let Some(body) = context.child(node, "class_body")? {
        entry.item_mut().children = class_members(body, context)?;
    }
    Ok(Some(entry))
}

fn extract_alias(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context
        .field(node, "type_alias_name")?
        .or(context.field(node, "name")?)
        .or(context.child(node, "identifier")?)
    else {
        return Ok(None);
    };
    let right = context
        .children(node)?
        .into_iter()
        .rev()
        .find(|child| !matches!(child.kind(), "simple_identifier" | "=" | "type_alias"))
        .map_or(String::new(), |right| format!(" = {}", context.text(right)));
    let modifiers = context
        .child(node, "modifiers")?
        .map_or("", |modifiers| context.text(modifiers));
    Ok(Some(Entry::item(
        Section::Type,
        node,
        compact_whitespace(&format!(
            "{}typealias {}{}{right}",
            if modifiers.is_empty() {
                String::new()
            } else {
                format!("{modifiers} ")
            },
            context.text(name),
            type_parameters(node, context)?
        )),
    )))
}

fn extract_property(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(declaration) = context
        .field(node, "variable_declaration")?
        .or(context.child(node, "variable_declaration")?)
    else {
        return Ok(None);
    };
    let name = context
        .field(declaration, "simple_identifier")?
        .or(context.child(declaration, "simple_identifier")?)
        .or(context.child(declaration, "identifier")?)
        .map_or("", |name| context.text(name));
    let modifiers = modifiers(node, context)?;
    if !modifiers.contains("const")
        && !name.is_empty()
        && !name
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        return Ok(None);
    }
    Ok(property_text(node, context)?.map(|text| Entry::item(Section::Constant, node, text)))
}

fn extract_declaration(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    match node.kind() {
        "class_declaration" => extract_class(node, context),
        "object_declaration" => extract_object(node, context),
        "function_declaration" => Ok(function_signature(node, context)?
            .map(|signature| Entry::item(Section::Function, node, signature))),
        "property_declaration" => extract_property(node, context),
        "type_alias" => extract_alias(node, context),
        _ => Ok(None),
    }
}
