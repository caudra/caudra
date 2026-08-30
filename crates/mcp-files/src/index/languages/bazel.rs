use std::collections::HashMap;

use tree_sitter::Node;

use super::common::{ExtractResult, plain_skeleton};
use crate::index::{
    model::{LineRange, ParsedSkeleton},
    render::{compact_whitespace, format_range, truncate},
    traversal::Context,
};

const BINDING_TRUNCATE: usize = 60;

#[derive(Clone)]
struct Value {
    kind: ValueKind,
    text: String,
    quoted: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ValueKind {
    String,
    Identifier,
    Other,
}

#[derive(Clone)]
enum Argument {
    Positional(Value),
    Keyword { name: String, value: Value },
    DictionarySplat(Vec<String>),
}

#[derive(Clone)]
struct Call {
    target: String,
    positional: Vec<Value>,
    kwargs: HashMap<String, Value>,
    args: Vec<Argument>,
    range: LineRange,
}

impl Call {
    fn kwarg(&self, name: &str) -> Option<&Value> {
        self.kwargs.get(name)
    }

    fn value(&self, position: usize, name: &str) -> Option<&Value> {
        self.positional
            .get(position.saturating_sub(1))
            .or_else(|| self.kwarg(name))
    }

    fn bool(&self, name: &str) -> Option<bool> {
        match self.kwarg(name)?.text.as_str() {
            "True" => Some(true),
            "False" => Some(false),
            _ => None,
        }
    }
}

enum Statement {
    Load {
        module: String,
        quoted: bool,
        names: Vec<String>,
        range: LineRange,
    },
    Assignment {
        name: String,
        raw: String,
        compact: String,
        value_call: Option<Call>,
        range: LineRange,
    },
    Function {
        name: String,
        parameters: String,
        range: LineRange,
    },
    Call(Call),
}

struct Collected {
    doc: Option<LineRange>,
    statements: Vec<Statement>,
}

pub(super) fn extract_build(
    root: Node<'_>,
    context: &Context<'_>,
) -> ExtractResult<ParsedSkeleton> {
    let collected = collect(root, context)?;
    let mut loads = Vec::new();
    let mut package_groups = Vec::new();
    let mut exports = Vec::new();
    let mut bindings = Vec::new();
    let mut targets = Vec::new();
    for statement in &collected.statements {
        context.check()?;
        match statement {
            Statement::Load {
                module,
                quoted,
                names,
                range,
            } => loads.push((module.clone(), *quoted, names.clone(), *range)),
            Statement::Assignment {
                name,
                compact,
                range,
                ..
            } => bindings.push((name.clone(), truncate(compact, BINDING_TRUNCATE), *range)),
            Statement::Call(call) if call.target == "package_group" => {
                if let Some(name) = call.kwarg("name") {
                    package_groups.push((name.text.clone(), call.range));
                }
            }
            Statement::Call(call) if call.target == "exports_files" => {
                if let Some(files) = call.value(1, "srcs") {
                    exports.push((files.text.clone(), call.range));
                }
            }
            Statement::Call(call) if call.target != "package" => {
                if let Some(name) = call.kwarg("name") {
                    targets.push((
                        name.text.clone(),
                        call.target.clone(),
                        call.kwarg("deprecation").is_some(),
                        call.range,
                    ));
                }
            }
            _ => {}
        }
    }
    let mut sections = Vec::new();
    push_doc(&mut sections, "build", collected.doc);
    push_section(
        &mut sections,
        "loads",
        loads.into_iter().map(|(module, quoted, names, range)| {
            label_line(
                &if quoted { quote(&module) } else { module },
                Some(&names.join(", ")),
                range,
                1,
            )
        }),
        context,
    )?;
    push_section(
        &mut sections,
        "package_groups",
        package_groups
            .into_iter()
            .map(|(name, range)| label_line(&name, None, range, 2)),
        context,
    )?;
    push_section(
        &mut sections,
        "exports_files",
        exports
            .into_iter()
            .map(|(files, range)| item_line(&files, range, 2)),
        context,
    )?;
    push_section(
        &mut sections,
        "variable bindings",
        bindings
            .into_iter()
            .map(|(name, value, range)| label_line(&name, Some(&value), range, 2)),
        context,
    )?;
    push_section(
        &mut sections,
        "targets",
        targets.into_iter().map(|(name, rule, deprecated, range)| {
            label_line(
                &name,
                Some(&format!(
                    "{rule}{}",
                    if deprecated { ", deprecated=True" } else { "" }
                )),
                range,
                2,
            )
        }),
        context,
    )?;
    context.check()?;
    Ok(plain_skeleton(sections.join("\n")))
}

pub(super) fn extract_bzl(root: Node<'_>, context: &Context<'_>) -> ExtractResult<ParsedSkeleton> {
    let collected = collect(root, context)?;
    let mut loads = Vec::new();
    let mut bindings = Vec::new();
    let mut functions = Vec::new();
    for statement in &collected.statements {
        context.check()?;
        match statement {
            Statement::Load {
                module,
                quoted: true,
                names,
                range,
            } => loads.push((module.clone(), names.clone(), *range)),
            Statement::Assignment {
                name, raw, range, ..
            } => bindings.push((name.clone(), truncate(raw, BINDING_TRUNCATE), *range)),
            Statement::Function {
                name,
                parameters,
                range,
            } => functions.push((name.clone(), parameters.clone(), *range)),
            _ => {}
        }
    }
    let mut sections = Vec::new();
    push_doc(&mut sections, "module", collected.doc);
    push_section(
        &mut sections,
        "loads",
        loads.into_iter().map(|(module, names, range)| {
            label_line(&quote(&module), Some(&names.join(", ")), range, 1)
        }),
        context,
    )?;
    push_section(
        &mut sections,
        "variable bindings",
        bindings
            .into_iter()
            .map(|(name, value, range)| item_line(&format!("{name} = {value}"), range, 1)),
        context,
    )?;
    push_section(
        &mut sections,
        "functions",
        functions
            .into_iter()
            .map(|(name, parameters, range)| item_line(&format!("{name}{parameters}"), range, 1)),
        context,
    )?;
    context.check()?;
    Ok(plain_skeleton(sections.join("\n")))
}

#[derive(Default)]
struct ModuleState {
    module: Option<(String, LineRange)>,
    deps: Vec<(String, Option<String>, String, bool, LineRange)>,
    extensions: Vec<(String, String, String, bool, LineRange)>,
    tags: Vec<(String, String, Option<Value>, LineRange)>,
    repos: Vec<(String, Vec<String>, bool, LineRange)>,
    toolchains: Vec<(String, bool, LineRange)>,
    platforms: Vec<(String, bool, LineRange)>,
    variables: Vec<(String, LineRange)>,
    includes: Vec<(String, bool, LineRange)>,
    aliases: HashMap<String, AliasKind>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum AliasKind {
    Extension,
    RepoRule,
}

pub(super) fn extract_module(
    root: Node<'_>,
    context: &Context<'_>,
) -> ExtractResult<ParsedSkeleton> {
    let collected = collect(root, context)?;
    let mut state = ModuleState::default();
    for statement in &collected.statements {
        context.check()?;
        match statement {
            Statement::Assignment {
                name,
                value_call,
                range,
                ..
            } => handle_module_assignment(name, value_call.as_ref(), *range, &mut state),
            Statement::Call(call) => handle_module_call(call, &mut state),
            _ => {}
        }
    }
    let mut sections = Vec::new();
    push_doc(&mut sections, "module", collected.doc);
    if let Some((name, range)) = state.module {
        push_section(
            &mut sections,
            "module",
            [item_line(&quote(&format!("@{name}")), range, 1)],
            context,
        )?;
    }
    push_section(
        &mut sections,
        "bazel_deps",
        state
            .deps
            .into_iter()
            .map(|(module, apparent, version, dev, range)| {
                let label = apparent
                    .as_ref()
                    .map_or_else(|| quote(&module), |name| quote(&format!("@{name}")));
                let value = format!(
                    "{version}{}{}",
                    if apparent.is_some() {
                        ""
                    } else {
                        ", repo_name=None"
                    },
                    if dev { ", dev=True" } else { "" }
                );
                label_line(&label, Some(&value), range, 1)
            }),
        context,
    )?;
    push_section(
        &mut sections,
        "module_extensions",
        state
            .extensions
            .into_iter()
            .map(|(alias, path, name, dev, range)| {
                label_line(
                    &alias,
                    Some(&format!(
                        "{path}, {name}{}",
                        if dev { ", dev=True" } else { "" }
                    )),
                    range,
                    1,
                )
            }),
        context,
    )?;
    push_section(
        &mut sections,
        "module_extensions.tags",
        state.tags.into_iter().map(|(alias, tag, name, range)| {
            let value = name.as_ref().map(render_value);
            label_line(&format!("{alias}.{tag}"), value.as_deref(), range, 1)
        }),
        context,
    )?;
    push_section(
        &mut sections,
        "repos",
        state.repos.into_iter().map(|(rule, names, dev, range)| {
            label_line(
                &rule,
                Some(&format!(
                    "{}{}",
                    names.join(", "),
                    if dev { ", dev=True" } else { "" }
                )),
                range,
                1,
            )
        }),
        context,
    )?;
    push_targets(
        &mut sections,
        "register_toolchains",
        state.toolchains,
        context,
    )?;
    push_targets(
        &mut sections,
        "register_execution_platforms",
        state.platforms,
        context,
    )?;
    push_section(
        &mut sections,
        "vars",
        state
            .variables
            .into_iter()
            .map(|(name, range)| label_line(&name, None, range, 1)),
        context,
    )?;
    push_targets(&mut sections, "includes", state.includes, context)?;
    context.check()?;
    Ok(plain_skeleton(sections.join("\n")))
}

fn handle_module_assignment(
    name: &str,
    call: Option<&Call>,
    range: LineRange,
    state: &mut ModuleState,
) {
    state.aliases.remove(name);
    if let Some(call) = call {
        if call.target == "use_extension" {
            if let (Some(path), Some(extension)) = (
                call.value(1, "extension_bzl_file"),
                call.value(2, "extension_name"),
            ) {
                state.aliases.insert(name.to_owned(), AliasKind::Extension);
                state.extensions.push((
                    name.to_owned(),
                    render_value(path),
                    render_value(extension),
                    call.bool("dev_dependency").unwrap_or(false),
                    range,
                ));
            }
            return;
        } else if call.target == "use_repo_rule" {
            if call.value(1, "repo_rule_bzl_file").is_some()
                && call.value(2, "repo_rule_name").is_some()
            {
                state.aliases.insert(name.to_owned(), AliasKind::RepoRule);
            }
            return;
        }
    }
    if is_constant_name(name) {
        state.variables.push((name.to_owned(), range));
    }
}

fn handle_module_call(call: &Call, state: &mut ModuleState) {
    match call.target.as_str() {
        "module" => {
            if let Some(name) = call.value(1, "name").filter(|value| value.quoted) {
                state.module = Some((name.text.clone(), call.range));
            }
        }
        "bazel_dep" => handle_dependency(call, state),
        "use_repo" => handle_use_repo(call, state),
        "register_toolchains" => push_call_targets(call, &mut state.toolchains, true),
        "register_execution_platforms" => push_call_targets(call, &mut state.platforms, true),
        "include" => push_call_targets(call, &mut state.includes, false),
        "single_version_override"
        | "multiple_version_override"
        | "archive_override"
        | "git_override"
        | "local_path_override"
        | "override_repo"
        | "flag_alias"
        | "inject_repo" => {}
        _ => handle_alias_call(call, state),
    }
}

fn handle_dependency(call: &Call, state: &mut ModuleState) {
    let Some(name) = call.value(1, "name").filter(|value| value.quoted) else {
        return;
    };
    let repository = match call.kwarg("repo_name") {
        Some(value) if value.kind == ValueKind::String && !value.text.is_empty() => {
            Some(value.text.clone())
        }
        Some(value) if !value.quoted && value.text == "None" => None,
        _ => Some(name.text.clone()),
    };
    state.deps.push((
        name.text.clone(),
        repository,
        call.value(2, "version")
            .map_or_else(|| "\"\"".into(), render_value),
        call.bool("dev_dependency").unwrap_or(false),
        call.range,
    ));
}

fn handle_use_repo(call: &Call, state: &mut ModuleState) {
    let Some(Argument::Positional(proxy)) = call.args.first() else {
        return;
    };
    if proxy.kind != ValueKind::Identifier
        || state.aliases.get(&proxy.text) != Some(&AliasKind::Extension)
    {
        return;
    }
    let mut names = Vec::new();
    for argument in call.args.iter().skip(1) {
        match argument {
            Argument::DictionarySplat(keys) => {
                names.extend(keys.iter().map(|key| quote(&format!("@{key}"))));
            }
            Argument::Keyword { name, .. } => names.push(quote(&format!("@{name}"))),
            Argument::Positional(value) if value.kind == ValueKind::String => {
                names.push(quote(&format!("@{}", value.text)));
            }
            Argument::Positional(value) => names.push(value.text.clone()),
        }
    }
    if !names.is_empty() {
        state
            .repos
            .push((proxy.text.clone(), names, false, call.range));
    }
}

fn handle_alias_call(call: &Call, state: &mut ModuleState) {
    if let Some((alias, tag)) = call.target.rsplit_once('.')
        && state.aliases.get(alias) == Some(&AliasKind::Extension)
    {
        state.tags.push((
            alias.to_owned(),
            tag.to_owned(),
            call.kwarg("name").cloned(),
            call.range,
        ));
        return;
    }
    if state.aliases.get(&call.target) == Some(&AliasKind::RepoRule)
        && let Some(name) = call.kwarg("name")
    {
        let repository = if name.quoted {
            quote(&format!("@{}", name.text))
        } else {
            name.text.clone()
        };
        state.repos.push((
            call.target.clone(),
            vec![repository],
            call.bool("dev_dependency").unwrap_or(false),
            call.range,
        ));
    }
}

fn push_call_targets(call: &Call, output: &mut Vec<(String, bool, LineRange)>, dev: bool) {
    let is_dev = dev && call.bool("dev_dependency").unwrap_or(false);
    output.extend(call.args.iter().filter_map(|argument| {
        let Argument::Positional(value) = argument else {
            return None;
        };
        Some((render_value(value), is_dev, call.range))
    }));
}

fn push_targets(
    sections: &mut Vec<String>,
    header: &str,
    values: Vec<(String, bool, LineRange)>,
    context: &Context<'_>,
) -> ExtractResult<()> {
    push_section(
        sections,
        header,
        values
            .into_iter()
            .map(|(target, dev, range)| label_line(&target, dev.then_some("dev=True"), range, 1)),
        context,
    )
}

fn collect(root: Node<'_>, context: &Context<'_>) -> ExtractResult<Collected> {
    let nodes = context.named_children(root)?;
    let mut index = 0usize;
    while nodes
        .get(index)
        .is_some_and(|node| node.kind() == "comment")
    {
        index += 1;
    }
    let doc = if nodes
        .get(index)
        .is_some_and(|node| is_doc_string(*node, context))
    {
        let range = LineRange::from_node(nodes[index]);
        index += 1;
        Some(range)
    } else {
        None
    };
    let mut statements = Vec::new();
    for node in nodes.into_iter().skip(index) {
        if let Some(statement) = classify(node, context)? {
            statements.push(statement);
        }
    }
    Ok(Collected { doc, statements })
}

fn classify(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Option<Statement>> {
    let value = unwrap_expression(node, context)?;
    if value.kind() == "call" {
        let call = call_record(value, context)?;
        if call.target == "load" {
            let Some(module) = call.args.first().and_then(argument_value) else {
                return Ok(None);
            };
            let names = call
                .args
                .iter()
                .skip(1)
                .filter_map(|argument| match argument {
                    Argument::Keyword { name, .. } => Some(name.clone()),
                    _ => argument_value(argument).map(|value| value.text.clone()),
                })
                .collect();
            return Ok(Some(Statement::Load {
                module: module.text.clone(),
                quoted: module.quoted,
                names,
                range: call.range,
            }));
        }
        return Ok(Some(Statement::Call(call)));
    }
    if value.kind() == "assignment" {
        let (Some(left), Some(right)) = (
            context.field(value, "left")?,
            context.field(value, "right")?,
        ) else {
            return Ok(None);
        };
        return Ok(Some(Statement::Assignment {
            name: compact_whitespace(context.text(left)),
            raw: context.text(right).to_owned(),
            compact: compact_whitespace(context.text(right)),
            value_call: (right.kind() == "call")
                .then(|| call_record(right, context))
                .transpose()?,
            range: LineRange::from_node(value),
        }));
    }
    if value.kind() == "function_definition" {
        let Some(name) = context.field(value, "name")? else {
            return Ok(None);
        };
        let parameters = context
            .field(value, "parameters")?
            .map(|parameters| compact_parameters(parameters, context))
            .transpose()?
            .unwrap_or_else(|| "()".into());
        return Ok(Some(Statement::Function {
            name: context.text(name).to_owned(),
            parameters,
            range: LineRange::from_node(value),
        }));
    }
    Ok(None)
}

fn unwrap_expression<'tree>(
    node: Node<'tree>,
    context: &Context<'_>,
) -> ExtractResult<Node<'tree>> {
    if node.kind() != "expression_statement" {
        return Ok(node);
    }
    Ok(context
        .named_children(node)?
        .into_iter()
        .next()
        .unwrap_or(node))
}

fn call_record(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Call> {
    let target = context
        .field(node, "function")?
        .map_or(String::new(), |target| {
            context
                .text(target)
                .chars()
                .filter(|character| !character.is_whitespace() && *character != '\\')
                .collect()
        });
    let mut positional = Vec::new();
    let mut kwargs = HashMap::new();
    let mut args = Vec::new();
    if let Some(arguments) = context.field(node, "arguments")? {
        for argument in context.named_children(arguments)? {
            if argument.kind() == "comment" {
                continue;
            }
            if argument.kind() == "keyword_argument" {
                if let (Some(name), Some(value)) = (
                    context.field(argument, "name")?,
                    context.field(argument, "value")?,
                ) {
                    let name = context.text(name).to_owned();
                    let value = value_record(value, context)?;
                    kwargs.insert(name.clone(), value.clone());
                    args.push(Argument::Keyword { name, value });
                }
            } else if argument.kind() == "dictionary_splat" {
                args.push(Argument::DictionarySplat(dictionary_keys(
                    argument, context,
                )?));
            } else {
                let value = value_record(argument, context)?;
                positional.push(value.clone());
                args.push(Argument::Positional(value));
            }
        }
    }
    Ok(Call {
        target,
        positional,
        kwargs,
        args,
        range: LineRange::from_node(node),
    })
}

fn value_record(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Value> {
    Ok(match node.kind() {
        "identifier" => Value {
            kind: ValueKind::Identifier,
            text: context.text(node).to_owned(),
            quoted: false,
        },
        "string" => raw_string(context.text(node)).map_or_else(
            || Value {
                kind: ValueKind::Other,
                text: compact_whitespace(context.text(node)),
                quoted: false,
            },
            |text| Value {
                kind: ValueKind::String,
                text,
                quoted: true,
            },
        ),
        "list" => Value {
            kind: ValueKind::Other,
            text: compact_list(node, context)?,
            quoted: false,
        },
        _ => Value {
            kind: ValueKind::Other,
            text: compact_whitespace(context.text(node)),
            quoted: false,
        },
    })
}

fn dictionary_keys(node: Node<'_>, context: &Context<'_>) -> ExtractResult<Vec<String>> {
    let Some(dictionary) = context.named_children(node)?.into_iter().next() else {
        return Ok(Vec::new());
    };
    if dictionary.kind() != "dictionary" {
        return Ok(Vec::new());
    }
    let mut keys = Vec::new();
    for pair in context.named_children(dictionary)? {
        if pair.kind() == "pair"
            && let Some(key) = context.field(pair, "key")?
            && let Some(key) = raw_string(context.text(key))
        {
            keys.push(key);
        }
    }
    Ok(keys)
}

fn compact_list(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let mut values = Vec::new();
    for child in context.named_children(node)? {
        if child.kind() == "comment" {
            continue;
        }
        values.push(if child.kind() == "list" {
            compact_list(child, context)?
        } else {
            compact_whitespace(context.text(child))
        });
    }
    Ok(format!("[{}]", values.join(", ")))
}

fn compact_parameters(node: Node<'_>, context: &Context<'_>) -> ExtractResult<String> {
    let mut values = Vec::new();
    for child in context.named_children(node)? {
        if child.kind() == "comment" {
            continue;
        }
        if matches!(
            child.kind(),
            "default_parameter" | "typed_default_parameter"
        ) && let (Some(name), Some(value)) = (
            context.field(child, "name")?,
            context.field(child, "value")?,
        ) {
            let field_type = context
                .field(child, "type")?
                .map_or(String::new(), |field_type| {
                    format!(": {}", compact_whitespace(context.text(field_type)))
                });
            values.push(format!(
                "{}{field_type}={}",
                compact_whitespace(context.text(name)),
                compact_whitespace(context.text(value))
            ));
            continue;
        }
        values.push(compact_whitespace(context.text(child)));
    }
    Ok(format!("({})", values.join(", ")))
}

fn raw_string(value: &str) -> Option<String> {
    let value = compact_whitespace(value);
    let quote_start = value
        .char_indices()
        .find(|(_, character)| matches!(character, '\'' | '"'))?
        .0;
    let quote = value.as_bytes()[quote_start] as char;
    let rest = &value[quote_start..];
    let triple = rest.starts_with(&quote.to_string().repeat(3));
    if triple {
        rest.strip_prefix(&quote.to_string().repeat(3))?
            .strip_suffix(&quote.to_string().repeat(3))
            .map(str::to_owned)
    } else {
        rest.strip_prefix(quote)?
            .strip_suffix(quote)
            .map(str::to_owned)
    }
}

fn is_doc_string(node: Node<'_>, context: &Context<'_>) -> bool {
    if node.kind() != "expression_statement" {
        return false;
    }
    node.named_child(0).is_some_and(|child| {
        if child.kind() != "string" {
            return false;
        }
        let text = context.text(child).trim_start_matches(|character| {
            matches!(character, 'B' | 'F' | 'R' | 'U' | 'b' | 'f' | 'r' | 'u')
        });
        ["\"\"\"", "'''"].into_iter().any(|delimiter| {
            text.len() >= delimiter.len() * 2
                && text.starts_with(delimiter)
                && text.ends_with(delimiter)
        })
    })
}

fn argument_value(argument: &Argument) -> Option<&Value> {
    match argument {
        Argument::Positional(value) | Argument::Keyword { value, .. } => Some(value),
        Argument::DictionarySplat(_) => None,
    }
}

fn quote(value: &str) -> String {
    format!("\"{value}\"")
}

fn render_value(value: &Value) -> String {
    if value.quoted {
        quote(&value.text)
    } else {
        value.text.clone()
    }
}

fn is_constant_name(value: &str) -> bool {
    let value = value.trim_start_matches('_');
    value
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_uppercase())
        && value
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

fn push_doc(sections: &mut Vec<String>, kind: &str, range: Option<LineRange>) {
    if let Some(range) = range {
        sections.push(format!("{kind} doc: {}\n", format_range(range)));
    }
}

fn push_section(
    sections: &mut Vec<String>,
    header: &str,
    lines: impl IntoIterator<Item = String>,
    context: &Context<'_>,
) -> ExtractResult<()> {
    let mut guarded = Vec::new();
    for line in lines {
        context.check()?;
        guarded.push(line);
    }
    if guarded.is_empty() {
        return Ok(());
    }
    sections.push(format!("{header}:\n{}\n", guarded.join("\n")));
    Ok(())
}

fn item_line(value: &str, range: LineRange, indent: usize) -> String {
    let prefix = "  ".repeat(indent);
    let value = value.replace('\n', &format!("\n{prefix}"));
    insert_range(&format!("{prefix}{value}"), range)
}

fn label_line(label: &str, value: Option<&str>, range: LineRange, indent: usize) -> String {
    let prefix = "  ".repeat(indent);
    let text = value.map_or_else(
        || format!("{prefix}{label}:"),
        |value| {
            format!(
                "{prefix}{label}: {}",
                value.replace('\n', &format!("\n{prefix}"))
            )
        },
    );
    insert_range(&text, range)
}

fn insert_range(value: &str, range: LineRange) -> String {
    let range = format_range(range);
    value.find('\n').map_or_else(
        || format!("{value} {range}"),
        |newline| format!("{} {range}{}", &value[..newline], &value[newline..]),
    )
}
