use tree_sitter::Node;

use super::{
    c,
    common::{ExtractResult, LanguageSpec, extract_enum_variants, split_path},
};
use crate::index::{
    model::{ChildKind, Entry, Section},
    render::{FIELD_TRUNCATE_THRESHOLD, compact_whitespace, ranged, truncate, truncated_message},
    traversal::Context,
};

const VALUE_TRUNCATE: usize = 40;
const CUDA_FUNCTION_SPECIFIERS: [&str; 6] = [
    "__global__",
    "__device__",
    "__host__",
    "__forceinline__",
    "__noinline__",
    "launch_bounds",
];
const CUDA_MEMORY_SPACES: [&str; 4] = ["__constant__", "__device__", "__managed__", "__shared__"];

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
        "preproc_include" => Ok(vec![extract_include(node, context)?]),
        "using_declaration" => Ok(vec![extract_using(node, context)]),
        "namespace_definition" => extract_namespace(node, context),
        "class_specifier" => Ok(extract_class(node, context, true, None, None)?
            .into_iter()
            .collect()),
        "struct_specifier" => Ok(vec![
            match extract_class(node, context, false, None, None)? {
                Some(entry) => entry,
                None => c::extract_struct(node, context, "struct")?,
            },
        ]),
        "union_specifier" => Ok(vec![c::extract_struct(node, context, "union")?]),
        "enum_specifier" => Ok(extract_enum(node, context)?.into_iter().collect()),
        "function_definition" => Ok(method_signature(node, context)?
            .map(|signature| Entry::item(Section::Function, node, signature))
            .into_iter()
            .collect()),
        "template_declaration" => extract_template(node, context),
        "preproc_def" => Ok(extract_define(node, context)?.into_iter().collect()),
        "preproc_function_def" => Ok(c::extract_function_macro(node, context)?
            .into_iter()
            .collect()),
        "declaration" => Ok(match declaration_signature(node, context)? {
            Some(signature) => Some(Entry::item(Section::Function, node, signature)),
            None => cuda_memory_binding(node, context)?,
        }
        .into_iter()
        .collect()),
        "type_definition" => {
            let field_type = context.field(node, "type")?;
            if let Some(field_type) = field_type
                && let Some(entry) = c::typedef_with_body(node, field_type, context)?
            {
                return Ok(vec![entry]);
            }
            let field_type = field_type.map_or("_", |field_type| context.text(field_type));
            let declaration = context
                .field(node, "declarator")?
                .map_or("_", |declaration| context.text(declaration));
            Ok(vec![Entry::item(
                Section::Type,
                node,
                compact_whitespace(&format!("typedef {field_type} {declaration}")),
            )])
        }
        "alias_declaration" => {
            let name = context
                .field(node, "name")?
                .map_or("_", |name| context.text(name));
            let field_type = context
                .field(node, "type")?
                .map_or("_", |field_type| context.text(field_type));
            Ok(vec![Entry::item(
                Section::Type,
                node,
                compact_whitespace(&format!("using {name} = {field_type}")),
            )])
        }
        "linkage_specification"
        | "preproc_ifdef"
        | "preproc_if"
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

fn declarator_name<'a>(node: Node<'_>, context: &'a Context<'_>) -> &'a str {
    if matches!(
        node.kind(),
        "identifier"
            | "field_identifier"
            | "type_identifier"
            | "destructor_name"
            | "operator_name"
            | "qualified_identifier"
            | "parenthesized_declarator"
    ) {
        return context.text(node);
    }
    node.child_by_field_name("name")
        .or_else(|| inner_declarator(node))
        .map_or("_", |inner| declarator_name(inner, context))
}

/// The declarator a wrapper declarator decorates. A `reference_declarator` names it with no field,
/// and its first child is the anonymous `&` token, so the fallback is the first named child.
fn inner_declarator(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("declarator")
        .or_else(|| node.named_child(0))
}

fn declarator_signature(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<String>> {
    match node.kind() {
        "function_declarator" => {
            let Some(inner) = context.field(node, "declarator")? else {
                return Ok(None);
            };
            let parameters = context
                .field(node, "parameters")?
                .map_or("()", |parameters| context.text(parameters));
            Ok(Some(format!(
                "{}{parameters}",
                declarator_name(inner, context)
            )))
        }
        "reference_declarator" | "pointer_declarator" => match inner_declarator(node) {
            Some(inner) => declarator_signature(inner, context),
            None => Ok(None),
        },
        _ => Ok(Some(declarator_name(node, context).to_owned())),
    }
}

fn method_signature(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<String>> {
    let Some(declaration) = context.field(node, "declarator")? else {
        return Ok(None);
    };
    let Some(signature) = declarator_signature(declaration, context)? else {
        return Ok(None);
    };
    let return_type = context
        .field(node, "type")?
        .map_or("", |return_type| context.text(return_type));
    let signature = if return_type.is_empty() {
        signature
    } else {
        format!("{return_type} {signature}")
    };
    Ok(Some(compact_whitespace(&format!(
        "{}{signature}",
        cuda_function_specifiers(node, context)?
    ))))
}

/// CUDA execution-space specifiers and launch bounds, each followed by a space, so a kernel reads
/// `__global__ void k(…)`. The C++ grammar has none of these node kinds.
fn cuda_function_specifiers(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let mut specifiers = String::new();
    for child in context.children(node)? {
        if CUDA_FUNCTION_SPECIFIERS.contains(&child.kind()) {
            specifiers.push_str(context.text(child));
            specifiers.push(' ');
        }
    }
    Ok(specifiers)
}

/// A namespace-scope variable in a CUDA memory space, such as `__constant__ float table[64]`. The
/// grammar wraps most memory spaces in a `type_qualifier` but leaves `__device__` a bare token of
/// the declaration.
fn cuda_memory_binding(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let in_memory_space = context.children(node)?.into_iter().any(|child| {
        let token = if child.kind() == "type_qualifier" {
            child.child(0)
        } else {
            Some(child)
        };
        token.is_some_and(|token| CUDA_MEMORY_SPACES.contains(&token.kind()))
    });
    if !in_memory_space {
        return Ok(None);
    }
    let text = context.text(node).trim_end_matches(';');
    let initialized = match context.field(node, "declarator")? {
        Some(declarator) if declarator.kind() == "init_declarator" => {
            context.field(declarator, "value")?.and_then(|value| {
                text.get(..value.start_byte() - node.start_byte())
                    .map(|head| format!("{head}{}", truncate(context.text(value), VALUE_TRUNCATE)))
            })
        }
        _ => None,
    };
    Ok(Some(Entry::item(
        Section::Constant,
        node,
        compact_whitespace(initialized.as_deref().unwrap_or(text)),
    )))
}

fn declaration_signature(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<String>> {
    let mut declaration = context.field(node, "declarator")?;
    while let Some(wrapper) = declaration
        .filter(|node| matches!(node.kind(), "pointer_declarator" | "reference_declarator"))
    {
        declaration = inner_declarator(wrapper);
    }
    match declaration.map(|declaration| (declaration.kind(), declaration)) {
        Some(("function_declarator", _)) => method_signature(node, context),
        Some(("init_declarator", declaration)) => misread_prototype(node, declaration, context),
        _ => Ok(None),
    }
}

/// `void f(T *a, int (*cb)(T *a));` is a prototype whose parameter list also parses as an argument
/// list, and the C++ grammar reads it as a direct-initialised variable. No variable has type `void`,
/// so such a declaration is the prototype it looks like.
fn misread_prototype(
    node: Node<'_>,
    declaration: Node<'_>,
    context: &Context<'_>,
) -> ExtractResult<Option<String>> {
    let (Some(return_type), Some(name), Some(arguments)) = (
        context.field(node, "type")?,
        context.field(declaration, "declarator")?,
        context.field(declaration, "value")?,
    ) else {
        return Ok(None);
    };
    if context.text(return_type) != "void"
        || name.kind() != "identifier"
        || arguments.kind() != "argument_list"
    {
        return Ok(None);
    }
    Ok(Some(compact_whitespace(&format!(
        "void {}{}",
        context.text(name),
        context.text(arguments)
    ))))
}

fn extract_class(
    node: Node<'_>,
    context: &Context<'_>,
    class: bool,
    prefix: Option<&str>,
    range_node: Option<Node<'_>>,
) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let bases = context
        .field(node, "base_class_clause")?
        .map_or(String::new(), |bases| {
            format!(
                " : {}",
                context.text(bases).trim().trim_start_matches(':').trim()
            )
        });
    let keyword = if class { "class" } else { "struct" };
    let label = compact_whitespace(&format!(
        "{}{keyword} {}{bases}",
        prefix.map_or(String::new(), |prefix| format!("{prefix} ")),
        context.text(name)
    ));
    let mut entry = Entry::item(
        if class { Section::Class } else { Section::Type },
        range_node.unwrap_or(node),
        label,
    );
    if let Some(body) = context.field(node, "body")? {
        entry.item_mut().children = class_members(body, context)?;
    }
    Ok(Some(entry))
}

fn class_members(
    node: Node<'_>,
    context: &Context<'_>,
) -> ExtractResult<Vec<crate::index::model::Child>> {
    let mut members = Vec::new();
    let mut fields = 0usize;
    for child in context.children(node)? {
        match child.kind() {
            "function_definition" => {
                if let Some(signature) = method_signature(child, context)? {
                    members.push(ranged(signature, context.range(child)));
                }
            }
            "declaration" => {
                if let Some(signature) = declaration_signature(child, context)? {
                    members.push(ranged(signature, context.range(child)));
                }
            }
            "field_declaration" => {
                fields += 1;
                if fields <= FIELD_TRUNCATE_THRESHOLD {
                    members.push(ranged(
                        compact_whitespace(context.text(child).trim_end_matches(';')),
                        context.range(child),
                    ));
                }
            }
            _ => {}
        }
    }
    if fields > FIELD_TRUNCATE_THRESHOLD {
        members.push(truncated_message(fields).into());
    }
    Ok(members)
}

fn extract_include(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Entry> {
    let path = context
        .field(node, "path")?
        .map_or("", |path| context.text(path))
        .trim_matches(['"', '<', '>']);
    Ok(Entry::import(node, vec![split_path(path, '/')], None))
}

/// A `using` path is one segment: imports split on the include separator, `/`, and a `::` path
/// must render verbatim rather than as `std/vector`.
fn extract_using(node: Node<'_>, context: &Context<'_>) -> Entry {
    let text = context.text(node);
    let cleaned = text
        .strip_prefix("using namespace ")
        .or_else(|| text.strip_prefix("using "))
        .unwrap_or(text)
        .trim_end_matches(';')
        .trim();
    Entry::import(node, vec![vec![cleaned.to_owned()]], None)
}

fn extract_namespace(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Vec<Entry>> {
    let name = context
        .field(node, "name")?
        .map_or("(anonymous)", |name| context.text(name));
    let mut entries = vec![Entry::item(Section::Module, node, name)];
    if let Some(body) = context.child(node, "declaration_list")? {
        for child in context.children(body)? {
            entries.extend(extract_nodes(child, context, &[])?);
        }
    }
    Ok(entries)
}

fn extract_enum(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let name = context
        .field(node, "name")?
        .map_or("(anonymous)", |name| context.text(name));
    let Some(body) = context.field(node, "body")? else {
        return Ok(None);
    };
    let mut entry = Entry::item(Section::Type, node, format!("enum {name}"));
    entry.item_mut().children = extract_enum_variants(body, context, "enumerator")?;
    entry.item_mut().child_kind = ChildKind::Brief;
    Ok(Some(entry))
}

fn extract_template(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Vec<Entry>> {
    let parameters = context
        .field(node, "parameters")?
        .map_or("<>", |parameters| context.text(parameters));
    let prefix = format!("template{parameters}");
    for child in context.children(node)? {
        match child.kind() {
            "function_definition" => {
                return Ok(method_signature(child, context)?
                    .map(|signature| {
                        Entry::item(
                            Section::Function,
                            node,
                            compact_whitespace(&format!("{prefix} {signature}")),
                        )
                    })
                    .into_iter()
                    .collect());
            }
            "class_specifier" => {
                return Ok(
                    extract_class(child, context, true, Some(&prefix), Some(node))?
                        .into_iter()
                        .collect(),
                );
            }
            "struct_specifier" => {
                return Ok(
                    extract_class(child, context, false, Some(&prefix), Some(node))?
                        .into_iter()
                        .collect(),
                );
            }
            "declaration" => {
                return Ok(declaration_signature(child, context)?
                    .map(|signature| {
                        Entry::item(
                            Section::Function,
                            node,
                            compact_whitespace(&format!("{prefix} {signature}")),
                        )
                    })
                    .into_iter()
                    .collect());
            }
            _ => {}
        }
    }
    Ok(Vec::new())
}

fn extract_define(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Entry>> {
    let Some(name) = context.field(node, "name")? else {
        return Ok(None);
    };
    let value = context
        .field(node, "value")?
        .map_or(String::new(), |value| {
            truncate(context.text(value), VALUE_TRUNCATE)
        });
    let text = if value.is_empty() {
        context.text(name).to_owned()
    } else {
        compact_whitespace(&format!("{} {value}", context.text(name)))
    };
    Ok(Some(Entry::item(Section::Constant, node, text)))
}
