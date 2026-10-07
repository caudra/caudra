use tree_sitter::Node;

use super::common::{ExtractResult, LanguageSpec, extract_fields_truncated, simple_import};
use crate::index::{
    model::{Child, ChildKind, Entry, Section},
    render::{FIELD_TRUNCATE_THRESHOLD, compact_whitespace, ranged, truncate, truncated_message},
    traversal::Context,
};

const VALUE_TRUNCATE: usize = 80;
const FIELD_KINDS: [&str; 4] = ["field", "map_field", "oneof", "group"];
const TYPE_KINDS: [&str; 2] = ["message", "enum"];

pub(super) fn spec() -> LanguageSpec {
    LanguageSpec::new("/", extract_nodes)
}

fn extract_nodes(
    node: Node<'_>,
    context: &Context<'_>,
    _attrs: &[Node<'_>],
) -> ExtractResult<Vec<Entry>> {
    let entry = match node.kind() {
        "package" => context
            .child(node, "full_ident")?
            .map(|name| Entry::item(Section::Module, node, context.text(name))),
        "import" => context
            .field(node, "path")?
            .map(|path| simple_import(node, context.text(path).trim_matches('"'), '/')),
        "option" => Some(Entry::item(
            Section::Constant,
            node,
            truncate(&statement(context.text(node)), VALUE_TRUNCATE),
        )),
        kind if TYPE_KINDS.contains(&kind) => return extract_type(node, context, None),
        "extend" => extract_extend(node, context)?,
        "service" => extract_service(node, context)?,
        _ => None,
    };
    Ok(entry.into_iter().collect())
}

/// A message or enum, followed by the types nested in it, each named by its path from file scope.
fn extract_type(
    node: Node<'_>,
    context: &Context<'_>,
    scope: Option<&str>,
) -> ExtractResult<Vec<Entry>> {
    let keyword = node.kind();
    let Some(name) = context.child(node, &format!("{keyword}_name"))? else {
        return Ok(Vec::new());
    };
    let name = match scope {
        Some(scope) => format!("{scope}.{}", context.text(name)),
        None => context.text(name).to_owned(),
    };
    let mut entries = vec![Entry::item(
        Section::Type,
        node,
        format!("{keyword} {name}"),
    )];
    let Some(body) = context.child(node, &format!("{keyword}_body"))? else {
        return Ok(entries);
    };
    if keyword == "enum" {
        let item = entries[0].item_mut();
        item.children = extract_fields_truncated(body, context, "enum_field", |value, context| {
            Ok(context
                .child(value, "identifier")?
                .map_or("_", |name| context.text(name))
                .to_owned())
        })?;
        item.child_kind = ChildKind::Brief;
        return Ok(entries);
    }
    entries[0].item_mut().children = fields(body, context)?;
    for child in context.named_children(body)? {
        if TYPE_KINDS.contains(&child.kind()) {
            entries.extend(extract_type(child, context, Some(&name))?);
        }
    }
    Ok(entries)
}

fn extract_extend(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(extended) = context.child(node, "full_ident")? else {
        return Ok(None);
    };
    let mut entry = Entry::item(
        Section::Type,
        node,
        format!("extend {}", context.text(extended)),
    );
    if let Some(body) = context.child(node, "message_body")? {
        entry.item_mut().children = fields(body, context)?;
    }
    Ok(Some(entry))
}

fn extract_service(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.child(node, "service_name")? else {
        return Ok(None);
    };
    let mut entry = Entry::item(
        Section::Trait,
        node,
        format!("service {}", context.text(name)),
    );
    for rpc in context.named_children(node)? {
        if rpc.kind() == "rpc" {
            entry
                .item_mut()
                .children
                .push(ranged(head(context.text(rpc)), context.range(rpc)));
        }
    }
    Ok(Some(entry))
}

fn fields(body: Node<'_>, context: &Context<'_>) -> ExtractResult<Vec<Child>> {
    let mut fields = Vec::new();
    let mut total = 0usize;
    for field in context.named_children(body)? {
        if !FIELD_KINDS.contains(&field.kind()) {
            continue;
        }
        total += 1;
        if total <= FIELD_TRUNCATE_THRESHOLD {
            fields.push(truncate(&field_text(field, context)?, VALUE_TRUNCATE).into());
        }
    }
    if total > FIELD_TRUNCATE_THRESHOLD {
        fields.push(truncated_message(total).into());
    }
    Ok(fields)
}

fn field_text(field: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    Ok(match field.kind() {
        "oneof" => {
            let name = context
                .child(field, "identifier")?
                .map_or("_", |name| context.text(name));
            let members: Vec<_> = context
                .named_children(field)?
                .into_iter()
                .filter(|member| member.kind() == "oneof_field")
                .map(|member| statement(context.text(member)))
                .collect();
            format!("oneof {name} {{ {} }}", members.join("; "))
        }
        "group" => head(context.text(field)),
        _ => statement(context.text(field)),
    })
}

/// A statement on one line, without its terminating semicolon.
fn statement(text: &str) -> String {
    compact_whitespace(text.trim_end().trim_end_matches(';').trim_end())
}

/// The part of a declaration before its body or terminating semicolon, on one line.
fn head(text: &str) -> String {
    compact_whitespace(text.split(['{', ';']).next().unwrap_or(text).trim_end())
}
