use tree_sitter::Node;

use super::common::{ExtractResult, strip_delimited};
use crate::index::{
    model::{Child, Entry, ParsedSkeleton, Section},
    render::{FIELD_TRUNCATE_THRESHOLD, format_range, format_skeleton, truncated_message},
    traversal::Context,
};

const MAX_DEPTH: usize = 5;
const IMPORT_LIKE: &[&str] = &[
    "builtins.fetchTarball",
    "fetchTarball",
    "builtins.fetchurl",
    "fetchurl",
    "builtins.fetchGit",
    "fetchGit",
    "builtins.fetchClosure",
    "fetchClosure",
];

pub(super) fn extract(root: Node<'_>, context: &Context<'_>) -> ExtractResult<ParsedSkeleton> {
    let mut entries = collect(root, context, MAX_DEPTH)?;
    entries.extend(collect_imports(root, context)?);
    format_skeleton(&entries, &[], None, "/", context)
}

fn collect(node: Node<'_>, context: &Context<'_>, depth: usize) -> ExtractResult<Vec<Entry>> {
    if depth == 0 {
        return Ok(Vec::new());
    }
    match node.kind() {
        "source_code" => {
            if let Some(expression) = context.field(node, "expression")? {
                collect(expression, context, depth)
            } else {
                Ok(Vec::new())
            }
        }
        "function_expression" => {
            let mut entries = vec![Entry::item(
                Section::Function,
                node,
                function_signature("fns", node, context)?,
            )];
            if let Some(body) = context.field(node, "body")? {
                entries.extend(collect(body, context, depth - 1)?);
            }
            Ok(entries)
        }
        "attrset_expression" | "rec_attrset_expression" | "let_attrset_expression" => {
            collect_binding_set(node, context, depth)
        }
        "let_expression" => {
            let mut entries = collect_binding_set(node, context, depth - 1)?;
            if let Some(body) = context.field(node, "body")? {
                entries.extend(collect(body, context, depth - 1)?);
            }
            Ok(entries)
        }
        "with_expression" => {
            if let Some(body) = context.field(node, "body")? {
                collect(body, context, depth)
            } else {
                Ok(Vec::new())
            }
        }
        "parenthesized_expression" => {
            if let Some(expression) = context.field(node, "expression")? {
                collect(expression, context, depth)
            } else {
                Ok(Vec::new())
            }
        }
        "list_expression" => {
            let mut entries = Vec::new();
            for element in context.fields(node, "element")? {
                entries.extend(collect(element, context, depth)?);
            }
            Ok(entries)
        }
        _ => Ok(Vec::new()),
    }
}

fn collect_binding_set(
    node: Node<'_>,
    context: &Context<'_>,
    depth: usize,
) -> ExtractResult<Vec<Entry>> {
    let Some(bindings) = context.child(node, "binding_set")? else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for_binding(bindings, context, |name, value, binding, context| {
        if let Some(entry) = dispatch_binding(name, value, binding, context, depth)? {
            entries.push(entry);
        }
        Ok(())
    })?;
    Ok(entries)
}

fn for_binding<F>(bindings: Node<'_>, context: &Context<'_>, mut callback: F) -> ExtractResult<()>
where
    F: FnMut(&str, Node<'_>, Node<'_>, &Context<'_>) -> ExtractResult<()>,
{
    for binding in context.children(bindings)? {
        if binding.kind() == "binding" {
            if let (Some(path), Some(value)) = (
                context.field(binding, "attrpath")?,
                context.field(binding, "expression")?,
            ) {
                callback(context.text(path), value, binding, context)?;
            }
        } else if matches!(binding.kind(), "inherit" | "inherit_from") {
            for attribute in context.fields(binding, "attrs")? {
                let name = context.text(attribute);
                if !name.is_empty() {
                    callback(name, attribute, binding, context)?;
                }
            }
        }
    }
    Ok(())
}

fn dispatch_binding(
    name: &str,
    value: Node<'_>,
    node: Node<'_>,
    context: &Context<'_>,
    depth: usize,
) -> ExtractResult<Option<Entry>> {
    if name == "imports" {
        return Ok(None);
    }
    let mut entry = match value.kind() {
        "function_expression" => {
            let mut entry = Entry::item(
                Section::Function,
                node,
                function_signature(name, value, context)?,
            );
            if let Some(body) = context.field(value, "body")? {
                add_children(
                    &mut entry,
                    collect(body, context, depth.saturating_sub(1))?,
                    depth.saturating_sub(1),
                );
            }
            entry
        }
        "attrset_expression" | "rec_attrset_expression" | "let_attrset_expression" => {
            let mut entry = Entry::item(Section::Constant, node, name);
            add_children(
                &mut entry,
                collect(value, context, depth.saturating_sub(1))?,
                depth.saturating_sub(1),
            );
            entry
        }
        "let_expression" => {
            let mut entry = Entry::item(Section::Constant, node, name);
            if let Some(bindings) = context.child(value, "binding_set")? {
                for_binding(
                    bindings,
                    context,
                    |sub_name, sub_value, sub_node, context| {
                        if let Some(sub) = dispatch_binding(
                            sub_name,
                            sub_value,
                            sub_node,
                            context,
                            depth.saturating_sub(1),
                        )? {
                            add_children(&mut entry, vec![sub], depth.saturating_sub(1));
                        }
                        Ok(())
                    },
                )?;
            }
            if let Some(body) = context.field(value, "body")? {
                add_children(
                    &mut entry,
                    collect(body, context, depth.saturating_sub(1))?,
                    depth.saturating_sub(1),
                );
            }
            entry
        }
        "apply_expression" if is_import_apply(value, context)? => return Ok(None),
        "apply_expression" => {
            let label = derivation_name(value, context)?
                .map_or_else(|| name.to_owned(), |package| format!("{name} ({package})"));
            Entry::item(Section::Constant, node, label)
        }
        _ => Entry::item(Section::Constant, node, name),
    };
    entry.range = crate::index::model::LineRange::from_node(node);
    Ok(Some(entry))
}

fn add_children(parent: &mut Entry, nested: Vec<Entry>, depth: usize) {
    let total = nested.len();
    let mut nested = nested.into_iter();
    parent.item_mut().children.extend(
        nested
            .by_ref()
            .take(FIELD_TRUNCATE_THRESHOLD)
            .map(Child::from),
    );
    if total <= FIELD_TRUNCATE_THRESHOLD {
        return;
    }
    if depth <= 1 {
        parent
            .item_mut()
            .children
            .push(truncated_message(total).into());
    } else {
        parent.item_mut().children.extend(
            nested.map(|entry| format!("{} {}", entry.text(), format_range(entry.range)).into()),
        );
    }
}

fn function_signature(name: &str, node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let mut parts = Vec::new();
    if let Some(universal) = context.field(node, "universal")? {
        parts.push(context.text(universal));
    }
    if let Some(formals) = context.field(node, "formals")? {
        for child in context.children(formals)? {
            if child.kind() == "formal" {
                if let Some(name) = context.field(child, "name")? {
                    parts.push(context.text(name));
                }
            } else if child.kind() == "ellipses" {
                parts.push("...");
            }
        }
    }
    Ok(if parts.is_empty() {
        name.to_owned()
    } else {
        format!("{name}({})", parts.join(", "))
    })
}

fn collect_imports(root: Node<'_>, context: &Context<'_>) -> ExtractResult<Vec<Entry>> {
    let mut imports = Vec::new();
    collect_imports_from(root, context, &mut imports)?;
    Ok(imports)
}

fn collect_imports_from(
    node: Node<'_>,
    context: &Context<'_>,
    imports: &mut Vec<Entry>,
) -> ExtractResult<()> {
    if node.kind() == "binding" && is_imports_binding(node, context)? {
        if let Some(value) = context.field(node, "expression")?
            && value.kind() == "list_expression"
        {
            let paths = collect_list_paths(value, context)?;
            if !paths.is_empty() {
                imports.push(Entry::import(node, paths, None));
            }
        }
        return Ok(());
    }
    if node.kind() == "apply_expression" {
        let function = context.field(node, "function")?;
        let argument = context.field(node, "argument")?;
        if is_import_apply(node, context)? {
            if let Some(argument) = argument
                && let Some(path) = segments_from_element(argument, context)?
            {
                imports.push(Entry::import(node, vec![path], None));
            }
            return Ok(());
        }
        if let Some(function) = function {
            collect_imports_from(function, context, imports)?;
        }
        if let Some(argument) = argument {
            collect_imports_from(argument, context, imports)?;
        }
        return Ok(());
    }
    for child in context.children(node)? {
        collect_imports_from(child, context, imports)?;
    }
    Ok(())
}

fn is_imports_binding(node: Node<'_>, context: &Context<'_>) -> ExtractResult<bool> {
    Ok(context
        .field(node, "attrpath")?
        .is_some_and(|path| path.kind() == "attrpath" && context.text(path) == "imports"))
}

fn collect_list_paths(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Vec<Vec<String>>> {
    let mut paths = Vec::new();
    for element in context.fields(node, "element")? {
        if let Some(path) = segments_from_element(element, context)? {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn segments_from_element(
    node: Node<'_>,
    context: &Context<'_>,
) -> ExtractResult<Option<Vec<String>>> {
    match node.kind() {
        "path_expression"
        | "spath_expression"
        | "string_expression"
        | "indented_string_expression"
        | "hpath_expression"
        | "uri_expression" => Ok(clean_path(context.text(node))),
        "parenthesized_expression" => {
            for child in context.children(node)? {
                if let Some(path) = segments_from_element(child, context)? {
                    return Ok(Some(path));
                }
            }
            Ok(None)
        }
        "apply_expression" => {
            let Some(function) = context.field(node, "function")? else {
                return Ok(None);
            };
            let function_text = context.text(function);
            let accepted = match function.kind() {
                "variable_expression" => {
                    function_text == "import" || IMPORT_LIKE.contains(&function_text)
                }
                "select_expression" => {
                    IMPORT_LIKE.contains(&function_text) || function_text == "path"
                }
                _ => false,
            };
            if accepted && let Some(argument) = context.field(node, "argument")? {
                return segments_from_element(argument, context);
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn clean_path(value: &str) -> Option<Vec<String>> {
    let value = value.strip_prefix("path:").unwrap_or(value);
    let value = strip_delimited(value, "\"").unwrap_or(value);
    let value = strip_delimited(value, "''").unwrap_or(value);
    (!value.is_empty()).then(|| value.split_terminator('/').map(str::to_owned).collect())
}

fn is_import_apply(node: Node<'_>, context: &Context<'_>) -> ExtractResult<bool> {
    let Some(function) = context.field(node, "function")? else {
        return Ok(false);
    };
    let text = context.text(function);
    Ok(match function.kind() {
        "variable_expression" => text == "import" || IMPORT_LIKE.contains(&text),
        "select_expression" => IMPORT_LIKE.contains(&text),
        _ => false,
    })
}

fn derivation_name(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<String>> {
    let Some(function) = context.field(node, "function")? else {
        return Ok(None);
    };
    if function.kind() != "select_expression" || !context.text(function).ends_with(".mkDerivation")
    {
        return Ok(None);
    }
    let Some(argument) = context.field(node, "argument")? else {
        return Ok(None);
    };
    if !matches!(
        argument.kind(),
        "attrset_expression" | "rec_attrset_expression"
    ) {
        return Ok(None);
    }
    Ok(attrset_lookup(argument, context, "pname")?.or(attrset_lookup(argument, context, "name")?))
}

fn attrset_lookup(
    node: Node<'_>,
    context: &Context<'_>,
    target: &str,
) -> ExtractResult<Option<String>> {
    let Some(bindings) = context.child(node, "binding_set")? else {
        return Ok(None);
    };
    for binding in context.children(bindings)? {
        if binding.kind() == "binding"
            && context
                .field(binding, "attrpath")?
                .is_some_and(|path| context.text(path) == target)
            && let Some(value) = context.field(binding, "expression")?
        {
            let raw_value = context.text(value);
            let value = strip_delimited(raw_value, "\"").unwrap_or(raw_value);
            let value = strip_delimited(value, "''").unwrap_or(value);
            return Ok(Some(value.to_owned()));
        }
    }
    Ok(None)
}
