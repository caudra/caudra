//! The global names a snippet's host defines. They are reserved whether or not a run defines
//! them, so an MCP server's namespace never takes one and never shifts when a helper appears.

pub const SEARCH_HELPER: &str = "search";
pub const SERVERS_HELPER: &str = "servers";
pub const RESOURCES_HELPER: &str = "resources";
pub const READ_RESOURCE_HELPER: &str = "read_resource";
pub const DECIDE_HELPER: &str = "decide";
pub const RERANK_HELPER: &str = "rerank";
/// The functions the MCP module defines beside its server namespaces.
pub const MCP_HELPERS: &[&str] = &[
    SEARCH_HELPER,
    SERVERS_HELPER,
    RESOURCES_HELPER,
    READ_RESOURCE_HELPER,
];
/// The gateway's own functions, which exist only with a decision engine.
pub const DECISION_HELPERS: &[&str] = &[DECIDE_HELPER, RERANK_HELPER];
/// The namespace of the `history` module.
pub const HISTORY_MODULE: &str = "history";
/// What the stubs import from `typing`, which no namespace may shadow.
pub const STUB_IMPORTS: &str = "from typing import Any, Literal, overload";

/// Namespaces host modules own.
const MODULE_NAMESPACES: &[&str] = &[HISTORY_MODULE];
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

/// Whether `name` is a helper, a module's namespace, or a name Python or the stubs already bind.
pub fn is_reserved(name: &str) -> bool {
    [
        MCP_HELPERS,
        DECISION_HELPERS,
        MODULE_NAMESPACES,
        BUILTINS,
        TYPING_NAMES,
        STDLIB_MODULES,
    ]
    .iter()
    .any(|reserved| reserved.contains(&name))
}

#[cfg(test)]
mod tests {
    use super::is_reserved;
    use test_case::test_case;

    #[test_case("bisect")]
    #[test_case("csv")]
    #[test_case("hashlib")]
    #[test_case("posixpath")]
    #[test_case("tomllib")]
    fn stdlib_modules_cannot_be_shadowed_by_host_names(name: &str) {
        assert!(is_reserved(name));
    }
}
