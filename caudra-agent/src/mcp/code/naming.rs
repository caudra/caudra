//! Python names for servers and tools.
//!
//! A server's configured name and a tool's raw MCP name may hold characters Python rejects, so each
//! gets an identifier. Assignment walks servers and tools in sorted order, so the same servers
//! always get the same names whatever order they connected in.

use std::collections::{BTreeMap, HashMap};

use super::view::{ServerView, ToolView};

pub const SEARCH_HELPER: &str = "search";
pub const SERVERS_HELPER: &str = "servers";
pub const RESOURCES_HELPER: &str = "resources";
pub const READ_RESOURCE_HELPER: &str = "read_resource";
/// Functions the snippet's globals hold beside the server namespaces.
pub const HELPERS: &[&str] = &[
    SEARCH_HELPER,
    SERVERS_HELPER,
    RESOURCES_HELPER,
    READ_RESOURCE_HELPER,
];
/// Prefix of the stub class that types a namespace, `_McpServer_<namespace>`.
pub const CLASS_PREFIX: &str = "_McpServer_";

const PLACEHOLDER: char = '_';
const RESERVED_MARK: char = '_';
const FIRST_DUPLICATE: usize = 2;
const MIN_SUGGESTION_DISTANCE: usize = 2;
const SUGGESTION_LENGTH_DIVISOR: usize = 3;

const KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];
/// Legal identifiers, but a name chosen here avoids them so it never reads as syntax.
const SOFT_KEYWORDS: &[&str] = &["match", "case", "type"];
const BUILTINS: &[&str] = &[
    "abs",
    "aiter",
    "all",
    "anext",
    "any",
    "ascii",
    "bin",
    "bool",
    "breakpoint",
    "bytearray",
    "bytes",
    "callable",
    "chr",
    "classmethod",
    "compile",
    "complex",
    "copyright",
    "credits",
    "delattr",
    "dict",
    "dir",
    "divmod",
    "enumerate",
    "eval",
    "exec",
    "exit",
    "filter",
    "float",
    "format",
    "frozenset",
    "getattr",
    "globals",
    "hasattr",
    "hash",
    "help",
    "hex",
    "id",
    "input",
    "int",
    "isinstance",
    "issubclass",
    "iter",
    "len",
    "license",
    "list",
    "locals",
    "map",
    "max",
    "memoryview",
    "min",
    "next",
    "object",
    "oct",
    "open",
    "ord",
    "pow",
    "print",
    "property",
    "quit",
    "range",
    "repr",
    "reversed",
    "round",
    "set",
    "setattr",
    "slice",
    "sorted",
    "staticmethod",
    "str",
    "sum",
    "super",
    "tuple",
    "type",
    "vars",
    "zip",
    "Ellipsis",
    "NotImplemented",
    "BaseException",
    "BaseExceptionGroup",
    "Exception",
    "ExceptionGroup",
    "ArithmeticError",
    "AssertionError",
    "AttributeError",
    "BlockingIOError",
    "BrokenPipeError",
    "BufferError",
    "ChildProcessError",
    "ConnectionAbortedError",
    "ConnectionError",
    "ConnectionRefusedError",
    "ConnectionResetError",
    "EOFError",
    "EnvironmentError",
    "FileExistsError",
    "FileNotFoundError",
    "FloatingPointError",
    "GeneratorExit",
    "IOError",
    "ImportError",
    "IndentationError",
    "IndexError",
    "InterruptedError",
    "IsADirectoryError",
    "KeyError",
    "KeyboardInterrupt",
    "LookupError",
    "MemoryError",
    "ModuleNotFoundError",
    "NameError",
    "NotADirectoryError",
    "NotImplementedError",
    "OSError",
    "OverflowError",
    "PermissionError",
    "ProcessLookupError",
    "RecursionError",
    "ReferenceError",
    "RuntimeError",
    "StopAsyncIteration",
    "StopIteration",
    "SyntaxError",
    "SystemError",
    "SystemExit",
    "TabError",
    "TimeoutError",
    "TypeError",
    "UnboundLocalError",
    "UnicodeDecodeError",
    "UnicodeEncodeError",
    "UnicodeError",
    "UnicodeTranslateError",
    "ValueError",
    "ZeroDivisionError",
    "Warning",
    "BytesWarning",
    "DeprecationWarning",
    "EncodingWarning",
    "FutureWarning",
    "ImportWarning",
    "PendingDeprecationWarning",
    "ResourceWarning",
    "RuntimeWarning",
    "SyntaxWarning",
    "UnicodeWarning",
    "UserWarning",
];
/// What the stubs import from `typing`, which a namespace must not shadow.
const TYPING_NAMES: &[&str] = &["Any", "Literal"];
/// The standard library modules the sandbox resolves.
const MODULES: &[&str] = &[
    "asyncio",
    "base64",
    "binascii",
    "collections",
    "copy",
    "dataclasses",
    "datetime",
    "functools",
    "itertools",
    "json",
    "math",
    "os",
    "pathlib",
    "random",
    "re",
    "sys",
    "time",
    "typing",
    "unicodedata",
];

/// The Python form of `raw`: invalid characters become `_`, a leading digit or an empty name is
/// prefixed with `_`, and a keyword gets a trailing `_`.
pub fn identifier(raw: &str) -> String {
    let mut name: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == PLACEHOLDER {
                c
            } else {
                PLACEHOLDER
            }
        })
        .collect();
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        name.insert(0, PLACEHOLDER);
    }
    if KEYWORDS.contains(&name.as_str()) || SOFT_KEYWORDS.contains(&name.as_str()) {
        name.push(RESERVED_MARK);
    }
    name
}

/// Whether `name` can be used as-is as a parameter name.
pub fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == PLACEHOLDER)
        && chars.all(|c| c.is_ascii_alphanumeric() || c == PLACEHOLDER)
        && !KEYWORDS.contains(&name)
}

#[derive(Clone, Debug, Default)]
struct Namespace {
    server: String,
    /// Method name to raw tool name, so iteration is in method order.
    methods: BTreeMap<String, String>,
    by_raw: HashMap<String, String>,
}

/// Every server's namespace and every tool's method name.
#[derive(Clone, Debug, Default)]
pub struct Names {
    by_namespace: BTreeMap<String, Namespace>,
    by_server: HashMap<String, String>,
}

impl Names {
    pub fn new(servers: &[ServerView]) -> Self {
        let mut sorted: Vec<&ServerView> = servers.iter().collect();
        sorted.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        let mut names = Self::default();
        for server in sorted {
            let namespace = unique(namespace_base(&server.name), |candidate| {
                names.by_namespace.contains_key(candidate)
            });
            let mut raw_tools: Vec<&str> =
                server.tools.iter().map(|tool| tool.name.as_str()).collect();
            raw_tools.sort_unstable();
            let mut entry = Namespace {
                server: server.name.clone(),
                ..Namespace::default()
            };
            for raw in raw_tools {
                let method = unique(identifier(raw), |candidate| {
                    entry.methods.contains_key(candidate)
                });
                entry.by_raw.insert(raw.to_owned(), method.clone());
                entry.methods.insert(method, raw.to_owned());
            }
            names
                .by_server
                .insert(server.name.clone(), namespace.clone());
            names.by_namespace.insert(namespace, entry);
        }
        names
    }

    pub fn namespace(&self, server: &str) -> Option<&str> {
        self.by_server.get(server).map(String::as_str)
    }

    pub fn server(&self, namespace: &str) -> Option<&str> {
        self.by_namespace
            .get(namespace)
            .map(|entry| entry.server.as_str())
    }

    pub fn tool(&self, server: &str, raw_tool: &str) -> Option<&str> {
        let namespace = self.by_server.get(server)?;
        self.by_namespace
            .get(namespace)?
            .by_raw
            .get(raw_tool)
            .map(String::as_str)
    }

    pub fn raw_tool(&self, namespace: &str, method: &str) -> Option<&str> {
        self.by_namespace
            .get(namespace)?
            .methods
            .get(method)
            .map(String::as_str)
    }

    /// `(namespace, server)` pairs in namespace order.
    pub fn namespaces(&self) -> impl Iterator<Item = (&str, &str)> {
        self.by_namespace
            .iter()
            .map(|(namespace, entry)| (namespace.as_str(), entry.server.as_str()))
    }

    /// `(method, raw tool)` pairs of one namespace in method order.
    pub fn methods(&self, namespace: &str) -> impl Iterator<Item = (&str, &str)> {
        self.by_namespace
            .get(namespace)
            .into_iter()
            .flat_map(|entry| &entry.methods)
            .map(|(method, raw)| (method.as_str(), raw.as_str()))
    }

    /// `server`'s tools paired with their method names, in method order.
    pub fn tools<'a>(&'a self, server: &'a ServerView) -> Vec<(&'a str, &'a ToolView)> {
        let mut tools: Vec<(&str, &ToolView)> = server
            .tools
            .iter()
            .filter_map(|tool| Some((self.tool(&server.name, &tool.name)?, tool)))
            .collect();
        tools.sort_unstable_by_key(|(method, _)| *method);
        tools
    }

    pub fn suggest_namespace(&self, name: &str) -> Option<&str> {
        closest(name, self.by_namespace.keys().map(String::as_str))
    }

    pub fn suggest_tool(&self, namespace: &str, name: &str) -> Option<&str> {
        closest(name, self.methods(namespace).map(|(method, _)| method))
    }
}

fn namespace_base(server: &str) -> String {
    let mut base = identifier(server);
    if base.starts_with(CLASS_PREFIX) {
        base.insert(0, RESERVED_MARK);
    } else if [HELPERS, BUILTINS, TYPING_NAMES, MODULES]
        .iter()
        .any(|reserved| reserved.contains(&base.as_str()))
    {
        base.push(RESERVED_MARK);
    }
    base
}

fn unique(base: String, taken: impl Fn(&str) -> bool) -> String {
    if !taken(&base) {
        return base;
    }
    let mut suffix = FIRST_DUPLICATE;
    loop {
        let candidate = format!("{base}_{suffix}");
        if !taken(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

/// The nearest candidate, first in order on a tie, when it is close enough to be a typo.
fn closest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let limit = (name.chars().count() / SUGGESTION_LENGTH_DIVISOR).max(MIN_SUGGESTION_DISTANCE);
    candidates
        .map(|candidate| (edit_distance(name, candidate), candidate))
        .filter(|(distance, _)| *distance <= limit)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

fn edit_distance(from: &str, to: &str) -> usize {
    let to: Vec<char> = to.chars().collect();
    let mut row: Vec<usize> = (0..=to.len()).collect();
    for (index, from_char) in from.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = index + 1;
        for (column, to_char) in to.iter().enumerate() {
            let above = row[column + 1];
            row[column + 1] = (above + 1)
                .min(row[column] + 1)
                .min(diagonal + usize::from(from_char != *to_char));
            diagonal = above;
        }
    }
    row[to.len()]
}

#[cfg(test)]
pub(super) mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::mcp::code::view::ServerState;

    pub(in crate::mcp::code) fn tool(name: &str) -> ToolView {
        ToolView {
            name: name.to_owned(),
            title: None,
            description: String::new(),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            search_only: false,
        }
    }

    pub(in crate::mcp::code) fn server(name: &str, tools: Vec<ToolView>) -> ServerView {
        ServerView {
            name: name.to_owned(),
            title: None,
            state: ServerState::Connected,
            instructions: None,
            tools,
            resources: false,
        }
    }

    #[test_case("list_issues", "list_issues" ; "already_valid")]
    #[test_case("list-issues", "list_issues" ; "dash")]
    #[test_case("files.read", "files_read" ; "dot")]
    #[test_case("1password", "_1password" ; "leading_digit")]
    #[test_case("class", "class_" ; "keyword")]
    #[test_case("match", "match_" ; "soft_keyword")]
    #[test_case("", "_" ; "empty")]
    #[test_case("_", "_" ; "underscore")]
    #[test_case("café", "caf_" ; "non_ascii")]
    fn identifier_maps_to_python(raw: &str, expected: &str) {
        assert_eq!(identifier(raw), expected);
    }

    #[test_case("owner", true ; "plain")]
    #[test_case("type", true ; "soft_keyword_is_legal")]
    #[test_case("_private", true ; "leading_underscore")]
    #[test_case("from", false ; "keyword")]
    #[test_case("per-page", false ; "dash")]
    #[test_case("2fa", false ; "leading_digit")]
    #[test_case("", false ; "empty")]
    fn is_identifier_accepts_only_legal_names(name: &str, expected: bool) {
        assert_eq!(is_identifier(name), expected);
    }

    #[test_case("search", "search_" ; "helper")]
    #[test_case("print", "print_" ; "builtin")]
    #[test_case("Exception", "Exception_" ; "builtin_exception")]
    #[test_case("json", "json_" ; "module")]
    #[test_case("Any", "Any_" ; "stub_import")]
    #[test_case("-McpServer-x", "__McpServer_x" ; "class_prefix")]
    #[test_case("my-server", "my_server" ; "dash")]
    fn a_namespace_avoids_reserved_names(server_name: &str, expected: &str) {
        let names = Names::new(&[server(server_name, Vec::new())]);
        assert_eq!(names.namespace(server_name), Some(expected));
        assert_eq!(names.server(expected), Some(server_name));
    }

    #[test_case(&["a-b", "a_b"] ; "sorted")]
    #[test_case(&["a_b", "a-b"] ; "reversed")]
    fn colliding_namespaces_are_numbered_in_name_order(order: &[&str]) {
        let servers: Vec<ServerView> = order.iter().map(|name| server(name, Vec::new())).collect();
        let names = Names::new(&servers);
        assert_eq!(names.namespace("a-b"), Some("a_b"));
        assert_eq!(names.namespace("a_b"), Some("a_b_2"));
    }

    #[test_case(&["get-item", "get.item", "get_item"] ; "sorted")]
    #[test_case(&["get_item", "get.item", "get-item"] ; "reversed")]
    fn colliding_methods_are_numbered_in_raw_name_order(order: &[&str]) {
        let names = Names::new(&[server("srv", order.iter().copied().map(tool).collect())]);
        assert_eq!(names.tool("srv", "get-item"), Some("get_item"));
        assert_eq!(names.tool("srv", "get.item"), Some("get_item_2"));
        assert_eq!(names.tool("srv", "get_item"), Some("get_item_3"));
        assert_eq!(names.raw_tool("srv", "get_item_2"), Some("get.item"));
    }

    #[test]
    fn namespaces_iterate_in_namespace_order() {
        let names = Names::new(&[server("zeta", Vec::new()), server("alpha", Vec::new())]);
        let listed: Vec<(&str, &str)> = names.namespaces().collect();
        assert_eq!(listed, [("alpha", "alpha"), ("zeta", "zeta")]);
    }

    #[test_case("githb", Some("github") ; "typo")]
    #[test_case("linear", None ; "too_far")]
    #[test_case("print", Some("print_") ; "renamed_builtin")]
    fn suggest_namespace_finds_near_misses(name: &str, expected: Option<&str>) {
        let names = Names::new(&[server("github", Vec::new()), server("print", Vec::new())]);
        assert_eq!(names.suggest_namespace(name), expected);
    }

    #[test_case("list_issue", Some("list_issues") ; "typo")]
    #[test_case("create_pull_request", None ; "too_far")]
    fn suggest_tool_finds_near_misses(name: &str, expected: Option<&str>) {
        let names = Names::new(&[server(
            "github",
            vec![tool("list_issues"), tool("get_issue")],
        )]);
        assert_eq!(names.suggest_tool("github", name), expected);
    }
}
