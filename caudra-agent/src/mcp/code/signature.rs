//! Python signatures and type stubs derived from tool JSON Schemas.
//!
//! The catalog and `search()` show a tool as the signature its stub declares, so what the model
//! reads and what the type checker enforces never disagree.

use std::collections::HashMap;
use std::iter;

use serde_json::{Map, Value};

use super::naming::{CLASS_PREFIX, Names, is_identifier};
use super::view::{ServerState, ServerView, ToolView};

const ANY: &str = "Any";
const NONE: &str = "None";
const TRUE: &str = "True";
const FALSE: &str = "False";
const STR: &str = "str";
const INT: &str = "int";
const FLOAT: &str = "float";
const BOOL: &str = "bool";
const DICT: &str = "dict[str, Any]";
const ARGUMENTS: &str = "arguments";
/// The receiver every stub method takes, so no parameter may be called this.
const RECEIVER: &str = "self";
const KEYWORD_ONLY: &str = "*";
const POSITIONAL_ONLY: &str = "/";
const INDENT: &str = "    ";
const OVERLOAD: &str = "@overload";

const PROPERTIES: &str = "properties";
const REQUIRED: &str = "required";
const DEFAULT: &str = "default";
const DESCRIPTION: &str = "description";
const TYPE: &str = "type";
const ITEMS: &str = "items";
const ENUM: &str = "enum";
const CONST: &str = "const";
const REF: &str = "$ref";
const ANY_OF: &str = "anyOf";
const ONE_OF: &str = "oneOf";
const ALL_OF: &str = "allOf";
const LOCAL_REF_PREFIX: char = '#';
const JSON_STRING: &str = "string";
const JSON_INTEGER: &str = "integer";
const JSON_NUMBER: &str = "number";
const JSON_BOOLEAN: &str = "boolean";
const JSON_NULL: &str = "null";
const JSON_ARRAY: &str = "array";
const JSON_OBJECT: &str = "object";

/// How many `$ref`s one type may follow, so a recursive schema still ends.
const MAX_REF_DEPTH: usize = 4;
/// A longer default is left out: the signature is a catalog line, not a fixture.
const MAX_DEFAULT_CHARS: usize = 48;
const MAX_SUMMARY_CHARS: usize = 160;
const ELLIPSIS: char = '…';
const SENTENCE_ENDS: [char; 3] = ['.', '!', '?'];

/// Stands in for a property that is required but never described.
static UNDESCRIBED: Value = Value::Bool(true);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    /// The schema's property name.
    pub name: String,
    pub ty: String,
    pub required: bool,
    /// The rendered Python default, present exactly when the parameter is optional.
    pub default: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// Every schema property, required ones first, each group in schema order.
    pub params: Vec<Param>,
    /// Some property is not a usable Python name, so the tool takes one `arguments` dict.
    pub dict_only: bool,
    pub returns: String,
    /// `method(*, a: int, b: str | None = None) -> Any`.
    pub text: String,
}

pub fn signature(method: &str, tool: &ToolView) -> Signature {
    let root = &tool.input_schema;
    let properties = root.get(PROPERTIES).and_then(Value::as_object);
    let required: Vec<&str> = root
        .get(REQUIRED)
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let declared = properties
        .into_iter()
        .flatten()
        .map(|(name, schema)| (name.as_str(), schema));
    let undeclared = required
        .iter()
        .filter(|name| !properties.is_some_and(|properties| properties.contains_key(**name)))
        .map(|name| (*name, &UNDESCRIBED));
    let mut params: Vec<Param> = declared
        .chain(undeclared)
        .map(|(name, schema)| param(name, schema, root, required.contains(&name)))
        .collect();
    params.sort_by_key(|param| !param.required);
    let dict_only = params
        .iter()
        .any(|param| !is_identifier(&param.name) || param.name == RECEIVER);
    let returns = tool.output_schema.as_ref().map_or_else(
        || ANY.to_owned(),
        |schema| python_type(schema, schema).render(),
    );
    let text = format!(
        "{method}({}) -> {returns}",
        parameter_list(&params, dict_only)
    );
    Signature {
        params,
        dict_only,
        returns,
        text,
    }
}

/// The first sentence of the description, or the title when there is none, on one line.
pub fn summary(tool: &ToolView) -> String {
    let paragraph: Vec<&str> = described(tool)
        .lines()
        .skip_while(|line| line.trim().is_empty())
        .take_while(|line| !line.trim().is_empty())
        .collect();
    let collapsed = collapse(&paragraph.join(" "));
    cap(first_sentence(&collapsed), MAX_SUMMARY_CHARS)
}

/// The full description, then one `name: description` line per described parameter.
pub fn doc(signature: &Signature, tool: &ToolView) -> String {
    let description = described(tool).trim();
    let params = signature.params.iter().filter_map(|param| {
        param
            .description
            .as_ref()
            .map(|text| format!("{}: {text}", param.name))
    });
    iter::once(description.to_owned())
        .filter(|text| !text.is_empty())
        .chain(params)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Stubs declaring every server's namespace, for the type checker.
pub fn stubs(names: &Names, servers: &[ServerView]) -> String {
    let by_name: HashMap<&str, &ServerView> = servers
        .iter()
        .map(|server| (server.name.as_str(), server))
        .collect();
    let mut lines = Vec::new();
    for (namespace, server_name) in names.namespaces() {
        let Some(server) = by_name.get(server_name) else {
            continue;
        };
        if server.state != ServerState::Connected {
            lines.push(format!("{namespace}: {ANY}"));
            continue;
        }
        let class = format!("{CLASS_PREFIX}{namespace}");
        lines.push(format!("class {class}:"));
        let tools = names.tools(server);
        if tools.is_empty() {
            lines.push(format!("{INDENT}pass"));
        }
        for (method, tool) in tools {
            let signature = signature(method, tool);
            let parameters = parameter_list(&signature.params, signature.dict_only);
            let method_stub = |parameters: &str| {
                let separator = if parameters.is_empty() { "" } else { ", " };
                format!(
                    "{INDENT}async def {method}({RECEIVER}{separator}{parameters}) -> {}: ...",
                    signature.returns
                )
            };
            if signature.dict_only {
                lines.push(method_stub(&parameters));
                continue;
            }
            // The gateway also takes the arguments as one dict, as the docs promise.
            lines.extend([
                format!("{INDENT}{OVERLOAD}"),
                method_stub(&parameters),
                format!("{INDENT}{OVERLOAD}"),
                method_stub(&format!("{ARGUMENTS}: {DICT}, /")),
            ]);
        }
        lines.push(format!("{namespace}: {class}"));
    }
    let mut stubs = lines.join("\n");
    stubs.push('\n');
    stubs
}

/// `text` on one line, cut to `max_chars` with an ellipsis.
pub(super) fn one_line(text: &str, max_chars: usize) -> String {
    cap(&collapse(text), max_chars)
}

fn described(tool: &ToolView) -> &str {
    if tool.description.trim().is_empty() {
        tool.title.as_deref().unwrap_or_default()
    } else {
        &tool.description
    }
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cap(text: &str, max_chars: usize) -> String {
    if text.chars().nth(max_chars).is_none() {
        return text.to_owned();
    }
    let cut = text
        .char_indices()
        .nth(max_chars.saturating_sub(1))
        .map_or(text.len(), |(index, _)| index);
    format!("{}{ELLIPSIS}", text[..cut].trim_end())
}

fn first_sentence(text: &str) -> &str {
    text.char_indices()
        .map(|(index, c)| index + c.len_utf8())
        .find(|&end| {
            text[..end].ends_with(SENTENCE_ENDS) && text[end..].starts_with(char::is_whitespace)
        })
        .map_or(text, |end| &text[..end])
}

fn param(name: &str, schema: &Value, root: &Value, required: bool) -> Param {
    let mut ty = python_type(schema, root);
    let default = (!required).then(|| {
        schema_default(schema).unwrap_or_else(|| {
            ty.push(NONE.to_owned());
            NONE.to_owned()
        })
    });
    Param {
        name: name.to_owned(),
        ty: ty.render(),
        required,
        default,
        description: schema
            .get(DESCRIPTION)
            .and_then(Value::as_str)
            .map(collapse)
            .filter(|text| !text.is_empty()),
    }
}

/// What follows the receiver in a method stub, or follows `(` in the signature text.
fn parameter_list(params: &[Param], dict_only: bool) -> String {
    if dict_only {
        let argument = if params.iter().any(|param| param.required) {
            format!("{ARGUMENTS}: {DICT}")
        } else {
            format!("{ARGUMENTS}: {DICT} | {NONE} = {NONE}")
        };
        return format!("{argument}, {POSITIONAL_ONLY}");
    }
    if params.is_empty() {
        return String::new();
    }
    iter::once(KEYWORD_ONLY.to_owned())
        .chain(params.iter().map(render_param))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_param(param: &Param) -> String {
    match &param.default {
        Some(default) => format!("{}: {} = {default}", param.name, param.ty),
        None => format!("{}: {}", param.name, param.ty),
    }
}

/// The schema's own default as a Python literal, unless it would not type-check or read well.
fn schema_default(schema: &Value) -> Option<String> {
    let schema = schema.as_object()?;
    let value = schema
        .get(DEFAULT)
        .filter(|value| default_fits(schema, value))?;
    Some(python_literal(value)).filter(|literal| literal.chars().count() <= MAX_DEFAULT_CHARS)
}

fn default_fits(schema: &Map<String, Value>, value: &Value) -> bool {
    if let Some(values) = schema.get(ENUM).and_then(Value::as_array) {
        return values.contains(value);
    }
    if let Some(constant) = schema.get(CONST) {
        return constant == value;
    }
    match schema.get(TYPE) {
        Some(Value::String(kind)) => kind_fits(kind, value),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .any(|kind| kind_fits(kind, value)),
        _ => true,
    }
}

fn kind_fits(kind: &str, value: &Value) -> bool {
    match kind {
        JSON_STRING => value.is_string(),
        JSON_INTEGER => value.is_i64() || value.is_u64(),
        JSON_NUMBER => value.is_number(),
        JSON_BOOLEAN => value.is_boolean(),
        JSON_NULL => value.is_null(),
        JSON_ARRAY => value.is_array(),
        JSON_OBJECT => value.is_object(),
        _ => true,
    }
}

/// A union of Python types, deduplicated in first-seen order.
#[derive(Default)]
struct Union(Vec<String>);

impl Union {
    fn push(&mut self, member: String) {
        if !self.0.contains(&member) {
            self.0.push(member);
        }
    }

    fn render(self) -> String {
        if self.0.is_empty() || self.0.iter().any(|member| member == ANY) {
            ANY.to_owned()
        } else {
            self.0.join(" | ")
        }
    }
}

fn python_type(schema: &Value, root: &Value) -> Union {
    let mut union = Union::default();
    collect_type(schema, root, 0, &mut union);
    union
}

fn collect_type(schema: &Value, root: &Value, depth: usize, union: &mut Union) {
    let Some(object) = schema.as_object() else {
        union.push(ANY.to_owned());
        return;
    };
    if let Some(reference) = object.get(REF).and_then(Value::as_str) {
        match local_ref(root, reference) {
            Some(target) if depth < MAX_REF_DEPTH => collect_type(target, root, depth + 1, union),
            _ => union.push(ANY.to_owned()),
        }
    } else if let Some(constant) = object.get(CONST).and_then(literal_type) {
        union.push(constant);
    } else if let Some(values) = object.get(ENUM).and_then(Value::as_array)
        && let Some(members) = enum_members(values)
    {
        members.into_iter().for_each(|member| union.push(member));
    } else if let Some(branches) = object
        .get(ANY_OF)
        .or_else(|| object.get(ONE_OF))
        .and_then(Value::as_array)
    {
        for branch in branches {
            collect_type(branch, root, depth, union);
        }
    } else if let Some([only]) = object
        .get(ALL_OF)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
    {
        collect_type(only, root, depth, union);
    } else {
        match object.get(TYPE) {
            Some(Value::String(kind)) => union.push(json_type(kind, object, root, depth)),
            Some(Value::Array(kinds)) => kinds
                .iter()
                .filter_map(Value::as_str)
                .for_each(|kind| union.push(json_type(kind, object, root, depth))),
            _ => union.push(ANY.to_owned()),
        }
    }
}

fn json_type(kind: &str, schema: &Map<String, Value>, root: &Value, depth: usize) -> String {
    match kind {
        JSON_STRING => STR.to_owned(),
        JSON_INTEGER => INT.to_owned(),
        JSON_NUMBER => FLOAT.to_owned(),
        JSON_BOOLEAN => BOOL.to_owned(),
        JSON_NULL => NONE.to_owned(),
        JSON_OBJECT => DICT.to_owned(),
        JSON_ARRAY => {
            let mut items = Union::default();
            match schema.get(ITEMS).filter(|items| items.is_object()) {
                Some(schema) => collect_type(schema, root, depth, &mut items),
                None => items.push(ANY.to_owned()),
            }
            format!("list[{}]", items.render())
        }
        _ => ANY.to_owned(),
    }
}

fn local_ref<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    root.pointer(reference.strip_prefix(LOCAL_REF_PREFIX)?)
}

/// `Literal[...]` for an enum of strings, integers and booleans, with `None` for a null member.
fn enum_members(values: &[Value]) -> Option<Vec<String>> {
    let literals: Vec<String> = values
        .iter()
        .filter(|value| !value.is_null())
        .map(literal)
        .collect::<Option<_>>()?;
    if literals.is_empty() {
        return None;
    }
    let literal_type = format!("Literal[{}]", literals.join(", "));
    let nullable = values.iter().any(Value::is_null).then(|| NONE.to_owned());
    Some(iter::once(literal_type).chain(nullable).collect())
}

fn literal_type(value: &Value) -> Option<String> {
    if value.is_null() {
        return Some(NONE.to_owned());
    }
    literal(value).map(|literal| format!("Literal[{literal}]"))
}

/// A value `Literal[...]` accepts.
fn literal(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(python_string(text)),
        Value::Number(number) if number.is_i64() || number.is_u64() => Some(number.to_string()),
        Value::Bool(flag) => Some(python_bool(*flag).to_owned()),
        _ => None,
    }
}

fn python_literal(value: &Value) -> String {
    match value {
        Value::Null => NONE.to_owned(),
        Value::Bool(flag) => python_bool(*flag).to_owned(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => python_string(text),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(python_literal).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(entries) => {
            let entries: Vec<String> = entries
                .iter()
                .map(|(key, value)| format!("{}: {}", python_string(key), python_literal(value)))
                .collect();
            format!("{{{}}}", entries.join(", "))
        }
    }
}

fn python_bool(flag: bool) -> &'static str {
    if flag { TRUE } else { FALSE }
}

/// A single-quoted Python string literal.
fn python_string(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for c in text.chars() {
        match c {
            '\\' => quoted.push_str("\\\\"),
            '\'' => quoted.push_str("\\'"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            c if c.is_control() => quoted.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => quoted.push(c),
        }
    }
    quoted.push('\'');
    quoted
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::mcp::code::naming::tests::{server, tool};

    const METHOD: &str = "run";
    const DESCRIBED: &str = "List issues in a repository. Results are paginated.";

    fn with_schema(input_schema: Value) -> ToolView {
        ToolView {
            input_schema,
            ..tool(METHOD)
        }
    }

    fn text(input_schema: Value) -> String {
        signature(METHOD, &with_schema(input_schema)).text
    }

    #[test_case(json!({"type": "object"}), "run() -> Any" ; "no_params")]
    #[test_case(json!({"type": "object", "properties": {"b": {"type": "integer"}, "a": {"type": "string"}}, "required": ["a"]}), "run(*, a: str, b: int | None = None) -> Any" ; "required_first_then_schema_order")]
    #[test_case(json!({"properties": {"n": {"type": "integer", "default": 30}}}), "run(*, n: int = 30) -> Any" ; "integer_default")]
    #[test_case(json!({"properties": {"q": {"type": "string", "default": "it's"}}}), r"run(*, q: str = 'it\'s') -> Any" ; "string_default_is_escaped")]
    #[test_case(json!({"properties": {"f": {"type": "boolean", "default": false}}}), "run(*, f: bool = False) -> Any" ; "boolean_default")]
    #[test_case(json!({"properties": {"l": {"type": "array", "items": {"type": "string"}, "default": ["a"]}}}), "run(*, l: list[str] = ['a']) -> Any" ; "list_default")]
    #[test_case(json!({"properties": {"n": {"type": "integer", "default": "many"}}}), "run(*, n: int | None = None) -> Any" ; "mistyped_default_is_dropped")]
    #[test_case(json!({"properties": {"s": {"enum": ["open", "closed"]}}, "required": ["s"]}), "run(*, s: Literal['open', 'closed']) -> Any" ; "enum_literal")]
    #[test_case(json!({"properties": {"s": {"enum": ["a", null]}}, "required": ["s"]}), "run(*, s: Literal['a'] | None) -> Any" ; "nullable_enum")]
    #[test_case(json!({"properties": {"s": {"const": 1}}, "required": ["s"]}), "run(*, s: Literal[1]) -> Any" ; "const_literal")]
    #[test_case(json!({"properties": {"s": {"type": ["string", "null"]}}}), "run(*, s: str | None = None) -> Any" ; "nullable_type_array")]
    #[test_case(json!({"properties": {"s": {"anyOf": [{"type": "integer"}, {"type": "string"}, {"type": "integer"}]}}, "required": ["s"]}), "run(*, s: int | str) -> Any" ; "any_of_deduped")]
    #[test_case(json!({"properties": {"s": {"type": "array", "items": {"type": "number"}}}, "required": ["s"]}), "run(*, s: list[float]) -> Any" ; "array_items")]
    #[test_case(json!({"properties": {"s": {"type": "array"}}, "required": ["s"]}), "run(*, s: list[Any]) -> Any" ; "array_without_items")]
    #[test_case(json!({"properties": {"s": {"type": "object"}}, "required": ["s"]}), "run(*, s: dict[str, Any]) -> Any" ; "object")]
    #[test_case(json!({"properties": {"s": {"not": {}}}}), "run(*, s: Any = None) -> Any" ; "unknown_schema_is_any")]
    #[test_case(json!({"properties": {"s": {"$ref": "#/$defs/state"}}, "required": ["s"], "$defs": {"state": {"enum": ["on", "off"]}}}), "run(*, s: Literal['on', 'off']) -> Any" ; "defs_ref")]
    #[test_case(json!({"properties": {"s": {"$ref": "#/definitions/n"}}, "required": ["s"], "definitions": {"n": {"type": "integer"}}}), "run(*, s: int) -> Any" ; "definitions_ref")]
    #[test_case(json!({"properties": {"s": {"$ref": "#/$defs/node"}}, "required": ["s"], "$defs": {"node": {"type": "array", "items": {"$ref": "#/$defs/node"}}}}), "run(*, s: list[list[list[list[Any]]]]) -> Any" ; "ref_depth_cap")]
    #[test_case(json!({"properties": {"s": {"$ref": "https://example.com/s.json"}}, "required": ["s"]}), "run(*, s: Any) -> Any" ; "remote_ref_is_any")]
    #[test_case(json!({"required": ["id"]}), "run(*, id: Any) -> Any" ; "undeclared_required")]
    #[test_case(json!({"properties": {"per-page": {"type": "integer"}}, "required": ["per-page"]}), "run(arguments: dict[str, Any], /) -> Any" ; "dict_only_required")]
    #[test_case(json!({"properties": {"from": {"type": "string"}}}), "run(arguments: dict[str, Any] | None = None, /) -> Any" ; "dict_only_optional")]
    #[test_case(json!({"properties": {"self": {"type": "string"}}}), "run(arguments: dict[str, Any] | None = None, /) -> Any" ; "dict_only_receiver_name")]
    fn signature_text_renders_the_schema(input_schema: Value, expected: &str) {
        assert_eq!(text(input_schema), expected);
    }

    #[test_case(Some(json!({"type": "object", "properties": {"id": {"type": "integer"}}})), "dict[str, Any]" ; "object_output")]
    #[test_case(Some(json!({"type": "array", "items": {"type": "string"}})), "list[str]" ; "array_output")]
    #[test_case(None, "Any" ; "no_output_schema")]
    fn the_return_type_comes_from_the_output_schema(output_schema: Option<Value>, expected: &str) {
        let tool = ToolView {
            output_schema,
            ..tool(METHOD)
        };
        assert_eq!(signature(METHOD, &tool).returns, expected);
    }

    #[test]
    fn a_dict_only_tool_keeps_its_real_params() {
        let signature = signature(
            METHOD,
            &with_schema(
                json!({"properties": {"per-page": {"type": "integer", "description": "Page size."}}}),
            ),
        );
        assert!(signature.dict_only);
        assert_eq!(signature.params[0].name, "per-page");
        assert_eq!(
            signature.params[0].description.as_deref(),
            Some("Page size.")
        );
    }

    #[test_case(DESCRIBED, None, "List issues in a repository." ; "first_sentence")]
    #[test_case("Lists   issues\nacross repos\n\nDetails follow.", None, "Lists issues across repos" ; "first_paragraph_collapsed")]
    #[test_case("Version 1.2 is current", None, "Version 1.2 is current" ; "a_dot_inside_a_word")]
    #[test_case("", Some("Issue lister"), "Issue lister" ; "title_without_description")]
    fn summary_is_the_first_sentence(description: &str, title: Option<&str>, expected: &str) {
        let tool = ToolView {
            description: description.to_owned(),
            title: title.map(str::to_owned),
            ..tool(METHOD)
        };
        assert_eq!(summary(&tool), expected);
    }

    #[test]
    fn summary_is_capped_with_an_ellipsis() {
        let tool = ToolView {
            description: "word ".repeat(MAX_SUMMARY_CHARS),
            ..tool(METHOD)
        };
        let summary = summary(&tool);
        assert_eq!(summary.chars().count(), MAX_SUMMARY_CHARS);
        assert!(summary.ends_with(ELLIPSIS));
    }

    #[test]
    fn doc_lists_described_params_after_the_description() {
        let tool = ToolView {
            description: DESCRIBED.to_owned(),
            input_schema: json!({"properties": {
                "owner": {"type": "string", "description": "The repository\nowner."},
                "page": {"type": "integer"},
            }}),
            ..tool(METHOD)
        };
        let signature = signature(METHOD, &tool);
        assert_eq!(
            doc(&signature, &tool),
            format!("{DESCRIBED}\nowner: The repository owner.")
        );
    }

    #[test]
    fn stubs_declare_classes_helpers_and_untyped_unavailable_servers() {
        let github = server(
            "github",
            vec![
                with_schema(
                    json!({"properties": {"owner": {"type": "string"}}, "required": ["owner"]}),
                ),
                tool("zap"),
            ],
        );
        let empty = server("empty", Vec::new());
        let linear = ServerView {
            state: ServerState::NeedsLogin,
            ..server("linear", vec![tool("list")])
        };
        let servers = [linear, github, empty];
        let stubs = stubs(&Names::new(&servers), &servers);
        let expected = [
            "class _McpServer_empty:",
            "    pass",
            "empty: _McpServer_empty",
            "class _McpServer_github:",
            "    @overload",
            "    async def run(self, *, owner: str) -> Any: ...",
            "    @overload",
            "    async def run(self, arguments: dict[str, Any], /) -> Any: ...",
            "    @overload",
            "    async def zap(self) -> Any: ...",
            "    @overload",
            "    async def zap(self, arguments: dict[str, Any], /) -> Any: ...",
            "github: _McpServer_github",
            "linear: Any",
        ];
        assert_eq!(stubs.lines().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn a_dict_only_stub_takes_the_dict_positionally() {
        let servers = [server(
            "srv",
            vec![with_schema(
                json!({"properties": {"per-page": {"type": "integer"}}}),
            )],
        )];
        let stubs = stubs(&Names::new(&servers), &servers);
        assert!(
            stubs.contains(
                "    async def run(self, arguments: dict[str, Any] | None = None, /) -> Any: ..."
            ),
            "{stubs}"
        );
    }
}
