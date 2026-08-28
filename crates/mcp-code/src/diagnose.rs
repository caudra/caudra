//! Failure classification for agent steering.
//!
//! Monty's exceptions are accurate but assume a reader who already knows the subset. An agent does
//! not, so a bare `ModuleNotFoundError: No module named 'statistics'` costs a retry loop. Every arm
//! here converts a failure into the specific fact the caller needs to change its next attempt.
//!
//! Diagnostics are fixed strings built from Monty-supplied exception text. Nothing else about the
//! host, the worker, or its configuration is disclosed.

use monty_types::{ExcType, MontyException};

use crate::types::Outcome;

/// The set an agent is most likely to reach for and not find. Kept short deliberately: an
/// exhaustive list of absent modules would bury the available ones.
const AVAILABLE_MODULES: &str = "asyncio, base64, binascii, collections, dataclasses, datetime, functools, itertools, json, math, os, pathlib, re, sys, typing, unicodedata";

/// Builtins Monty does not implement. Type checking reports these as unresolved references, which
/// reads as a typo unless the caller is told they are withheld on purpose.
const WITHHELD_BUILTINS: [&str; 12] = [
    "eval",
    "exec",
    "compile",
    "__import__",
    "globals",
    "locals",
    "vars",
    "dir",
    "input",
    "super",
    "breakpoint",
    "help",
];

/// Classification of a raised exception plus the guidance that accompanies it.
pub(crate) struct Diagnosis {
    pub(crate) outcome: Outcome,
    pub(crate) diagnostic: Option<String>,
}

/// Maps a sandbox exception onto an outcome and, where the subset is the cause, an explanation.
pub(crate) fn diagnose(exception: &MontyException) -> Diagnosis {
    let message = exception.message().unwrap_or_default();
    match exception.exc_type() {
        // Monty rejects unsupported syntax during compilation, so nothing ran and nothing printed.
        ExcType::NotImplementedError if message.contains("syntax parser does not yet support") => {
            Diagnosis {
                outcome: Outcome::Rejected,
                diagnostic: Some(format!(
                    "This Python construct is not supported and the snippet did not run. Unavailable: class inheritance and metaclasses, super(), decorators on methods (@classmethod, @staticmethod, @property), yield and generator functions, match, del, try*/except* groups, async with, async for, PEP 695 type aliases, wildcard imports, complex literals, and t-strings. Rewrite without it, or use the shell tool if the task genuinely needs full Python. ({message})"
                )),
            }
        }
        ExcType::ModuleNotFoundError | ExcType::ImportError => Diagnosis {
            outcome: Outcome::Exception,
            diagnostic: Some(module_guidance(message)),
        },
        ExcType::NameError | ExcType::UnboundLocalError => Diagnosis {
            outcome: Outcome::Exception,
            diagnostic: Some(undefined_name_guidance(message)),
        },
        // Without mounts every filesystem call is refused, which is the isolation working.
        ExcType::PermissionError => Diagnosis {
            outcome: Outcome::Exception,
            diagnostic: Some(isolation_guidance(message)),
        },
        ExcType::RecursionError => Diagnosis {
            outcome: Outcome::Exception,
            diagnostic: Some(format!(
                "Maximum call depth of 1000 frames was exceeded. Rewrite the algorithm iteratively. ({message})"
            )),
        },
        // Resource errors are uncatchable in the sandbox, so reaching the host means execution stopped.
        ExcType::MemoryError => Diagnosis {
            outcome: Outcome::Limited,
            diagnostic: Some(format!(
                "The memory budget was exhausted. Process the data in smaller pieces, and remember that enumerate, zip, map, filter, and reversed are eager and materialise whole lists. ({message})"
            )),
        },
        ExcType::TimeoutError => Diagnosis {
            outcome: Outcome::Limited,
            diagnostic: Some(timeout_guidance(message)),
        },
        // `str.format` is the single most common CPython habit the subset does not support.
        ExcType::AttributeError if message.contains("format") => Diagnosis {
            outcome: Outcome::Exception,
            diagnostic: Some(format!(
                "str.format() and %-formatting are not implemented. Use f-strings instead. ({message})"
            )),
        },
        // Everything else is an ordinary Python error the caller can read and fix unaided.
        _ => Diagnosis {
            outcome: Outcome::Exception,
            diagnostic: None,
        },
    }
}

/// Shared wording for both the sandbox-reported timeout and the parent-side deadline.
pub(crate) fn timeout_guidance(detail: &str) -> String {
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(" ({detail})")
    };
    format!(
        "Execution exceeded its time budget and was stopped. Reduce the work, or raise timeout up to its cap. Note that enumerate, zip, map, filter, and reversed are eager, so an infinite iterator such as map(f, itertools.count()) never terminates.{suffix}"
    )
}

/// Guidance for an import the subset does not provide.
fn module_guidance(detail: &str) -> String {
    format!(
        "Only these standard library modules exist: {AVAILABLE_MODULES}. Third-party packages cannot be installed or imported. Do not retry this import; compute the result with the available modules, or use a different tool. ({detail})"
    )
}

/// Guidance for a name the subset does not provide.
fn undefined_name_guidance(detail: &str) -> String {
    format!(
        "Undefined name. Note that eval, exec, compile, globals, locals, vars, dir, input, super, callable, issubclass, bytearray, complex, memoryview, object, format, and ascii are not implemented, and nothing persists between code_execution calls. ({detail})"
    )
}

/// Guidance for an attempt to reach the host.
fn isolation_guidance(detail: &str) -> String {
    format!(
        "code_execution has no filesystem, network, or environment access. Read or write files with the file tools, fetch URLs with webfetch, or use the shell tool when host access is genuinely required. ({detail})"
    )
}

/// Explains a type-checking rejection.
///
/// Type checking is on by default, so it — not the interpreter — is what an agent usually hits
/// first. It reports a withheld capability as an ordinary unresolved name or import, which reads as
/// a typo and invites either a pointless retry or a request to disable type checking. Recognising
/// those two rules lets the caller receive the same redirection it would have received at runtime.
pub(crate) fn diagnose_type_errors(diagnostics: &str) -> String {
    let trimmed = diagnostics.trim();
    if let Some(module) = unresolved_import(trimmed) {
        return format!(
            "{}\n{trimmed}",
            module_guidance(&format!("unresolved import `{module}`"))
        );
    }
    if let Some(name) = unresolved_reference(trimmed) {
        let guidance = if name == "open" {
            isolation_guidance("`open` is not available")
        } else if WITHHELD_BUILTINS.contains(&name) {
            undefined_name_guidance(&format!("`{name}` is not implemented"))
        } else {
            undefined_name_guidance(&format!("`{name}` is not defined"))
        };
        return format!("{guidance}\n{trimmed}");
    }
    format!(
        "Type checking rejected this snippet before running it, so nothing was executed. Correct the reported problems, or ask the operator to disable type checking if the code is intentionally dynamic.\n{trimmed}"
    )
}

/// Extracts the module from the checker's `unresolved-import` rule, if that is the first failure.
fn unresolved_import(diagnostics: &str) -> Option<&str> {
    diagnostics
        .split_once("[unresolved-import] Cannot resolve imported module `")?
        .1
        .split_once('`')
        .map(|(module, _)| module)
}

/// Extracts the name from the checker's `unresolved-reference` rule, if that is the first failure.
fn unresolved_reference(diagnostics: &str) -> Option<&str> {
    diagnostics
        .split_once("[unresolved-reference] Name `")?
        .1
        .split_once('`')
        .map(|(name, _)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Type checking is the default, so these are the messages an agent actually receives. Each must
    /// redirect the same way the runtime path does rather than reading as a typo the caller should
    /// fix or as a reason to ask for type checking to be turned off.
    #[test]
    fn withheld_capabilities_redirect_instead_of_blaming_type_checking() {
        let file = diagnose_type_errors(
            "snippet.py:1:1: error[unresolved-reference] Name `open` used when not defined",
        );
        assert!(file.contains("file tools"), "{file}");
        assert!(!file.contains("disable type checking"), "{file}");

        let builtin = diagnose_type_errors(
            "snippet.py:1:1: error[unresolved-reference] Name `eval` used when not defined",
        );
        assert!(builtin.contains("not implemented"), "{builtin}");
        assert!(!builtin.contains("disable type checking"), "{builtin}");

        let module = diagnose_type_errors(
            "snippet.py:1:8: error[unresolved-import] Cannot resolve imported module `requests`",
        );
        assert!(
            module.contains("Third-party packages cannot be installed"),
            "{module}"
        );
        assert!(!module.contains("disable type checking"), "{module}");
    }

    #[test]
    fn the_checker_output_is_always_preserved_verbatim() {
        let raw = "snippet.py:1:1: error[unresolved-reference] Name `open` used when not defined";
        assert!(diagnose_type_errors(raw).contains(raw));
    }

    /// A genuine type error is not a withheld capability, so the generic guidance is correct there
    /// and the escape hatch should still be mentioned.
    #[test]
    fn ordinary_type_errors_keep_the_generic_guidance() {
        let text = diagnose_type_errors(
            "snippet.py:1:10: error[invalid-assignment] Object of type `Literal[\"x\"]` is not assignable to `int`",
        );
        assert!(text.contains("disable type checking"), "{text}");
    }

    /// An undefined name that is not withheld is an ordinary mistake, but the caller still needs to
    /// know that nothing carries over between calls, which is a common cause of it.
    #[test]
    fn an_ordinary_undefined_name_still_explains_the_lack_of_state() {
        let text = diagnose_type_errors(
            "snippet.py:1:1: error[unresolved-reference] Name `total` used when not defined",
        );
        assert!(text.contains("nothing persists between"), "{text}");
    }

    fn exception(exc_type: ExcType, message: &str) -> MontyException {
        MontyException::new(exc_type, Some(message.to_owned()))
    }

    #[test]
    fn unsupported_syntax_is_rejected_not_raised() {
        let diagnosis = diagnose(&exception(
            ExcType::NotImplementedError,
            "The monty syntax parser does not yet support match statements",
        ));
        assert_eq!(diagnosis.outcome, Outcome::Rejected);
        let text = diagnosis.diagnostic.expect("guidance");
        assert!(text.contains("did not run"));
        assert!(text.contains("match"));
    }

    #[test]
    fn a_plain_not_implemented_error_stays_an_exception() {
        // Sandbox code can raise NotImplementedError itself; only the parser prefix means rejection.
        let diagnosis = diagnose(&exception(ExcType::NotImplementedError, "subclass me"));
        assert_eq!(diagnosis.outcome, Outcome::Exception);
        assert!(diagnosis.diagnostic.is_none());
    }

    #[test]
    fn missing_module_lists_what_exists_and_forbids_retry() {
        let diagnosis = diagnose(&exception(
            ExcType::ModuleNotFoundError,
            "No module named 'statistics'",
        ));
        assert_eq!(diagnosis.outcome, Outcome::Exception);
        let text = diagnosis.diagnostic.expect("guidance");
        assert!(text.contains("unicodedata"));
        assert!(text.contains("Do not retry this import"));
    }

    #[test]
    fn permission_error_redirects_to_the_right_tool() {
        let diagnosis = diagnose(&exception(
            ExcType::PermissionError,
            "no mount for '/etc/passwd'",
        ));
        let text = diagnosis.diagnostic.expect("guidance");
        assert!(text.contains("file tools"));
        assert!(text.contains("webfetch"));
        assert!(text.contains("shell"));
    }

    #[test]
    fn resource_errors_are_limited_not_exceptions() {
        assert_eq!(
            diagnose(&exception(ExcType::MemoryError, "limit exceeded")).outcome,
            Outcome::Limited
        );
        assert_eq!(
            diagnose(&exception(ExcType::TimeoutError, "budget exhausted")).outcome,
            Outcome::Limited
        );
    }

    #[test]
    fn ordinary_errors_get_no_invented_guidance() {
        for exc_type in [
            ExcType::ValueError,
            ExcType::TypeError,
            ExcType::ZeroDivisionError,
        ] {
            let diagnosis = diagnose(&exception(exc_type, "boom"));
            assert_eq!(diagnosis.outcome, Outcome::Exception);
            assert!(
                diagnosis.diagnostic.is_none(),
                "{exc_type} should not be annotated"
            );
        }
    }

    #[test]
    fn timeout_guidance_warns_about_eager_iterators() {
        let text = timeout_guidance("");
        assert!(text.contains("eager"));
        assert!(text.contains("itertools.count()"));
        // An absent detail must not leave a dangling empty parenthetical on the end.
        assert!(text.ends_with("never terminates."));
        assert!(timeout_guidance("budget exhausted").ends_with("(budget exhausted)"));
    }
}
