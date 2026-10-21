//! The global names a snippet's host defines. They are reserved whether or not a run defines
//! them, so an MCP server's namespace never takes one and never shifts when a module appears.

use super::MODULE_DOCS;

/// The one global function the gateway defines beside the modules' namespaces.
pub const HELP: &str = "help";
pub const HISTORY_MODULE: &str = "history";
pub const MCP_MODULE: &str = "mcp";
pub const DECISIONS_MODULE: &str = "decisions";
pub const IMAGE_MODULE: &str = "image";
pub const HOST_MODULE: &str = "host";
pub const WORKFLOW_MODULE: &str = "workflow";
pub const AUTOMATION_MODULE: &str = "automation";
pub const SEARCH_HELPER: &str = "search";
pub const SERVERS_HELPER: &str = "servers";
pub const RESOURCES_HELPER: &str = "resources";
pub const READ_RESOURCE_HELPER: &str = "read_resource";
pub const DECIDE_HELPER: &str = "decide";
pub const RERANK_HELPER: &str = "rerank";
/// Globals earlier versions defined, with the call that replaced each. They stay reserved so the
/// hint is never shadowed by a server's namespace.
pub const RENAMED_GLOBALS: &[(&str, &str)] = &[
    (SEARCH_HELPER, "mcp.search"),
    (SERVERS_HELPER, "mcp.servers"),
    (RESOURCES_HELPER, "await mcp.resources"),
    (READ_RESOURCE_HELPER, "await mcp.read_resource"),
    (DECIDE_HELPER, "await decisions.decide"),
    (RERANK_HELPER, "await decisions.rerank"),
];
/// What the stubs import from `typing`, which no namespace may shadow.
pub const STUB_IMPORTS: &str = "from typing import Any, Literal, overload";

const TYPING_NAMES: &[&str] = &["Any", "Literal", "overload"];
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
/// The standard library modules the sandbox resolves.
const STDLIB_MODULES: &[&str] = &[
    "asyncio",
    "base64",
    "binascii",
    "bisect",
    "collections",
    "copy",
    "csv",
    "dataclasses",
    "datetime",
    "functools",
    "hashlib",
    "itertools",
    "json",
    "math",
    "os",
    "pathlib",
    "posixpath",
    "random",
    "re",
    "sys",
    "time",
    "tomllib",
    "typing",
    "unicodedata",
];

/// What an old global raises: the call that replaced it.
pub fn renamed(name: &str) -> Option<String> {
    RENAMED_GLOBALS
        .iter()
        .find(|(old, _)| *old == name)
        .map(|(_, new)| format!("name '{name}' is not defined; call `{new}(...)` instead"))
}

/// Whether `name` is a module's namespace, a global the host defines or once defined, or a name
/// Python or the stubs already bind.
pub fn is_reserved(name: &str) -> bool {
    name == HELP
        || MODULE_DOCS.iter().any(|doc| doc.namespace == name)
        || name == caudra_pstack::NAME
        || RENAMED_GLOBALS.iter().any(|(old, _)| *old == name)
        || [BUILTINS, TYPING_NAMES, STDLIB_MODULES]
            .iter()
            .any(|reserved| reserved.contains(&name))
}

#[cfg(test)]
mod tests {
    use super::{
        BUILTINS, DECIDE_HELPER, HISTORY_MODULE, STDLIB_MODULES, TYPING_NAMES, is_reserved, renamed,
    };
    use crate::code::MODULE_DOCS;
    use test_case::test_case;

    #[test_case("bisect")]
    #[test_case("csv")]
    #[test_case("hashlib")]
    #[test_case("posixpath")]
    #[test_case("tomllib")]
    #[test_case(HISTORY_MODULE)]
    #[test_case(DECIDE_HELPER)]
    fn reserved_names_cannot_be_shadowed_by_a_server(name: &str) {
        assert!(is_reserved(name));
    }

    #[test]
    fn no_module_namespace_shadows_python() {
        for doc in MODULE_DOCS {
            let clash = [BUILTINS, TYPING_NAMES, STDLIB_MODULES]
                .iter()
                .any(|names| names.contains(&doc.namespace));
            assert!(!clash, "{}", doc.namespace);
        }
    }

    /// Only the calls whose arguments are easy to get wrong wait for their reference, and a
    /// module's page carries the whole reference of each, so reading the module reads them.
    #[test]
    fn only_calls_easy_to_get_wrong_are_read_first() {
        let read_first: Vec<String> = MODULE_DOCS
            .iter()
            .flat_map(|doc| doc.read_first().map(|member| doc.qualified(member)))
            .collect();
        assert_eq!(
            read_first,
            [
                "history.search",
                "history.read",
                "image.generate",
                "workflow.start"
            ]
        );
        for doc in MODULE_DOCS {
            let help = doc.module_help();
            for member in doc.read_first() {
                assert!(help.contains(member.page), "{}", doc.qualified(member));
            }
        }
    }

    #[test]
    fn an_old_global_names_its_replacement() {
        assert_eq!(
            renamed(DECIDE_HELPER).as_deref(),
            Some("name 'decide' is not defined; call `await decisions.decide(...)` instead")
        );
        assert_eq!(renamed(HISTORY_MODULE), None);
    }
}
