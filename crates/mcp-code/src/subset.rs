//! Canonical statement of what the Monty subset provides.
//!
//! The tool description and the failure diagnostics both enumerate the subset. They were
//! independent hand-written prose until `base64`, `binascii`, and `functools` — modules Monty has
//! never implemented — survived in both, so an agent whose import failed received guidance naming
//! the same three absent modules and retried into a loop. Nothing here is derived from CPython's
//! library or from Monty's changelog; it is derived from the worker, and the live-worker tests in
//! `tests/execution.rs` fail if any list drifts from what the interpreter actually does.
//!
//! The prose forms are macros rather than constants because the description splices them into a
//! string literal at compile time and `concat!` accepts only literals.

/// Standard library modules the interpreter resolves.
///
/// Verified name by name against a running worker by `every_advertised_module_imports`.
macro_rules! available_modules {
    () => {
        "asyncio, collections, dataclasses, datetime, itertools, json, math, os, pathlib, re, sys, typing, unicodedata"
    };
}

/// Builtins the interpreter does not define at all, so referencing one raises `NameError`.
macro_rules! withheld_builtins {
    () => {
        "eval, exec, compile, __import__, globals, locals, vars, dir, input, breakpoint, help, exit, super, callable, issubclass, delattr, staticmethod, classmethod, bytearray, complex, memoryview, object, format, ascii, aiter, and anext"
    };
}

/// Builtins the interpreter defines but its type stubs omit.
///
/// These are the subtlest case in the subset and the reason this file exists in its current shape.
/// The description previously called `map` and `filter` eager, which is true of the interpreter, but
/// type checking is on by default and rejects them before anything runs — so the advice held only
/// under a configuration most callers never see.
macro_rules! untyped_builtins {
    () => {
        "map, filter, getattr, setattr, and hasattr"
    };
}

pub(crate) use {available_modules, untyped_builtins, withheld_builtins};

/// The advertised modules as individual names, in the order the prose lists them.
///
/// Public because the contract belongs to this crate and a host restating it — or a test holding the
/// description to the worker — must not retype the list, which is how the lists diverged before.
pub const SUBSET_MODULES: [&str; 13] = [
    "asyncio",
    "collections",
    "dataclasses",
    "datetime",
    "itertools",
    "json",
    "math",
    "os",
    "pathlib",
    "re",
    "sys",
    "typing",
    "unicodedata",
];

/// The undefined builtins as individual names, in the order the prose lists them.
///
/// Type checking reports each as an unresolved reference, which reads as a typo unless the caller is
/// told it is withheld on purpose, so `diagnose` uses this to pick its wording. Every entry is
/// asserted to raise `NameError` on a live worker by `every_withheld_builtin_is_actually_absent`.
pub const WITHHELD_BUILTINS: [&str; 26] = [
    "eval",
    "exec",
    "compile",
    "__import__",
    "globals",
    "locals",
    "vars",
    "dir",
    "input",
    "breakpoint",
    "help",
    "exit",
    "super",
    "callable",
    "issubclass",
    "delattr",
    "staticmethod",
    "classmethod",
    "bytearray",
    "complex",
    "memoryview",
    "object",
    "format",
    "ascii",
    "aiter",
    "anext",
];

/// Builtins that exist at runtime but are absent from the type stubs, so the default type checking
/// rejects them. They need their own guidance: telling a caller `map` is undefined is wrong, and
/// telling it `map` is available is useless while type checking is on.
///
/// `open` belongs to this set behaviourally but is deliberately excluded: it is answered by the
/// isolation refusal, which is a more useful thing to say than anything about type stubs.
pub const UNTYPED_BUILTINS: [&str; 5] = ["map", "filter", "getattr", "setattr", "hasattr"];

/// Renders a name list the way the prose does, so a diagnostic that has to build one at runtime
/// reads the same as the ones spliced into the description.
pub(crate) fn oxford(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [head @ .., last] => format!("{}, and {last}", head.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_module_prose_and_the_module_list_are_the_same_set() {
        assert_eq!(SUBSET_MODULES.join(", "), available_modules!());
    }

    #[test]
    fn the_withheld_prose_and_the_withheld_list_are_the_same_set() {
        assert_eq!(oxford(&WITHHELD_BUILTINS), withheld_builtins!());
    }

    #[test]
    fn the_untyped_prose_and_the_untyped_list_are_the_same_set() {
        assert_eq!(oxford(&UNTYPED_BUILTINS), untyped_builtins!());
    }

    /// A name is either undefined or merely unstubbed. Claiming both would make the two diagnostics
    /// contradict each other depending on which arm matched first.
    #[test]
    fn no_builtin_is_both_undefined_and_merely_unstubbed() {
        for name in UNTYPED_BUILTINS {
            assert!(
                !WITHHELD_BUILTINS.contains(&name),
                "{name} cannot be both undefined and defined-but-unstubbed"
            );
        }
    }

    /// The three modules that were advertised for as long as the description existed and have never
    /// been resolvable. Guarding the regression by name is cheap and states the lesson.
    #[test]
    fn modules_monty_does_not_implement_are_never_advertised() {
        for absent in ["base64", "binascii", "functools", "statistics", "random"] {
            assert!(
                !SUBSET_MODULES.contains(&absent),
                "{absent} is not resolvable by the worker and must not be advertised"
            );
        }
    }
}
