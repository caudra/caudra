//! MCP tool discovery metadata for the isolated code executor.
//!
//! The description is the primary steering surface. It front-loads the subset's negative space
//! because an agent that does not know what is missing spends turns rediscovering it, and Monty's
//! divergences from CPython are numerous enough that guessing is expensive.
//!
//! The JSON schema is an admission contract, not a security boundary; dispatch validates again.

use crate::subset::{available_modules, untyped_builtins, withheld_builtins};
use crate::types::{DEFAULT_TIMEOUT_MS, MAX_CODE_BYTES, MAX_TIMEOUT_MS};
use rmcp::model::{JsonObject, MetaObject, Tool, ToolAnnotations};
use serde_json::{Value, json};
use std::sync::Arc;

/// Stable extension key consumed by Workcell renderers. Preserve this namespace across versions.
pub(crate) const PRESENTATION_KEY: &str = "ai.workcell/presentation-profile";

/// Spliced from `subset` rather than written inline: the module and builtin lists have to be the
/// same ones the diagnostics quote, or a failed call steers the caller back into the same failure.
const DESCRIPTION: &str = concat!(
    r#"Execute a short Python script in an isolated interpreter and return its value and printed output.

Use this for computation: arithmetic and math, summary statistics computed directly since there is no statistics module, date arithmetic, string and text processing, JSON reshaping, regex extraction, sorting and aggregation. Prefer this over the shell tool for anything that is pure computation, because this tool cannot reach the host.

Isolation:
- No filesystem, no network, no environment variables, and no subprocesses. Use the file tools, webfetch, or shell when the task needs any of those.
- Runs in a separate worker process under enforced time, memory, and recursion limits.

This is a Python subset, not CPython. It rejects or fails on:
- Class inheritance, metaclasses, super(), and decorators on methods, so @classmethod, @staticmethod, and @property are unavailable. Simple classes and @dataclass do work.
- yield and generator functions, match statements, del, try*/except* groups, async with, async for, PEP 695 type aliases, wildcard imports, complex literals, and t-strings.
- str.format(), %-formatting, str.translate(), and str.maketrans(). Use f-strings.
- These builtins, which are undefined and raise NameError: "#,
    withheld_builtins!(),
    r#".
- User-defined exception classes, and function attributes such as __name__.

Only these standard library modules exist, each covering part of its CPython surface: "#,
    available_modules!(),
    r#". There are no third-party packages, and no base64, binascii, functools, random, time, io, copy, string, struct, operator, statistics, enum, contextlib, hashlib, uuid, or urllib.

Behaviour that differs from CPython even where the API exists:
- "#,
    untyped_builtins!(),
    r#" exist at runtime but are missing from the type stubs, so type checking rejects them before the snippet runs. Use a comprehension instead of map or filter. getattr and hasattr also cannot see methods, so hasattr returns False for attributes that do exist.
- enumerate, zip, reversed, and generator expressions are eager and return lists, so an infinite iterator never terminates. iter() and the itertools functions stay lazy and single-use.
- Operators do not dispatch to user-defined dunders, so +, -, <, len(), [], and () ignore __add__, __neg__, __lt__, __len__, __getitem__, and __call__ on your own classes. __init__, __repr__, __str__, __eq__, __hash__, __bool__, __iter__, and __contains__ do work; reach anything else by calling the method directly.
- re is backed by fancy-regex: no bytes patterns, no VERBOSE flag, no re.subn, and re.sub takes a string replacement only, never a callable.
- os exposes constants but no os.path, and sys exposes only version, platform, and the streams. Use pathlib for path manipulation.
- dataclasses provides @dataclass with no arguments plus is_dataclass; field, asdict, astuple, fields, replace, and options such as frozen= are absent.
- Only the utf-8, ascii, utf-16, and utf-32 codecs exist.

Usage notes:
- The code parameter is required and is bounded to 65536 UTF-8 bytes.
- The value of the final expression is returned. Use print() for intermediate output.
- timeout is optional, measured in milliseconds, defaults to 5000, and is capped at 30000.
- Each call is independent. No variables, definitions, or imports persist between calls.
- Snippets are type checked before running unless the operator disables it, so unsupported APIs and unavailable names usually fail before any output is produced.
- Raised exceptions are completed results carrying the exception type and message, so the caller can correct the script and retry."#
);

#[must_use]
pub fn catalog() -> Vec<Tool> {
    // Reject unknown fields to keep client mistakes from silently changing execution semantics.
    let schema = json!({"type":"object","additionalProperties":false,"properties":{"code":{"type":"string","minLength":1,"maxLength":MAX_CODE_BYTES,"description":"Python source to execute. The value of the final expression is returned."},"timeout":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_MS,"default":DEFAULT_TIMEOUT_MS,"description":"Optional timeout in milliseconds. Defaults to 5000 and is capped at 30000."}},"required":["code"],"$schema":"http://json-schema.org/draft-07/schema#"});
    let mut meta = JsonObject::new();
    meta.insert(
        PRESENTATION_KEY.into(),
        Value::String("code.result.v1".into()),
    );
    // The read-only and closed-world annotations are the inverse of the shell tool's and are
    // accurate: without mounts or host functions the interpreter reaches no file, socket, or
    // environment value. They are still presentation hints; the isolation is enforced by the worker.
    vec![
        Tool::new(
            "code_execution",
            DESCRIPTION,
            Arc::new(schema.as_object().expect("schema object").clone()),
        )
        .with_title("Execute Python code")
        .with_annotations(ToolAnnotations::from_raw(
            None,
            Some(true),
            Some(false),
            Some(false),
            Some(false),
        ))
        .with_meta(MetaObject(meta)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_standard_presentation_key() {
        let tools = catalog();
        assert_eq!(tools[0].name, "code_execution");
        let description = tools[0].description.as_deref().expect("tool description");
        assert!(description.contains("No filesystem, no network"));
        assert!(description.contains("Prefer this over the shell tool"));
        assert!(description.contains("eager and return lists"));
        assert_eq!(
            tools[0].input_schema["properties"]["timeout"]["description"],
            "Optional timeout in milliseconds. Defaults to 5000 and is capped at 30000."
        );
        assert_eq!(
            tools[0].meta.as_ref().unwrap().0[PRESENTATION_KEY],
            "code.result.v1"
        );
    }

    #[test]
    fn advertises_read_only_closed_world_execution() {
        let tools = catalog();
        let annotations = tools[0].annotations.as_ref().expect("annotations");
        // These are what distinguish the tool from `shell` for a client deciding whether to prompt.
        assert_eq!(annotations.read_only_hint, Some(true));
        assert_eq!(annotations.destructive_hint, Some(false));
        assert_eq!(annotations.open_world_hint, Some(false));
    }
}
