//! Failure classification for agent steering.
//!
//! Monty's exceptions are accurate but assume a reader who already knows the subset. An agent does
//! not, so a bare `ModuleNotFoundError: No module named 'statistics'` costs a retry loop. Every arm
//! here converts a failure into the specific fact the caller needs to change its next attempt.
//!
//! Diagnostics are fixed strings built from Monty-supplied exception text. Nothing else about the
//! host, the worker, or its configuration is disclosed.

use monty_types::{ExcType, MontyException};

use crate::subset::{
    UNTYPED_BUILTINS, WITHHELD_BUILTINS, available_modules, oxford, withheld_builtins,
};
use crate::types::Outcome;

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
        // The parser accepts only a name, tuple, list, or single starred name as an unpacking leaf,
        // so the ordinary element-swap idiom is refused before anything runs. Upstream reports it as
        // a plain `SyntaxError`, which reads as a mistake in the caller's own code; without the
        // rewrite spelled out an agent retries the same line. Sandbox code can raise `SyntaxError`
        // itself, so this matches the parser's wording rather than the type.
        ExcType::SyntaxError if message.contains("invalid unpacking target") => Diagnosis {
            outcome: Outcome::Rejected,
            diagnostic: Some(format!(
                "Unpacking cannot assign into a subscript or an attribute, and the snippet did not run. Assign through a temporary instead: replace `x[i], x[j] = x[j], x[i]` with `t = x[i]`, `x[i] = x[j]`, `x[j] = t`. Only plain names, tuples, lists, and one starred name are valid unpacking targets; every other form of unpacking works. ({message})"
            )),
        },
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
                "The memory budget was exhausted. Process the data in smaller pieces, and remember that enumerate, zip, reversed, and generator expressions are eager and materialise whole lists. ({message})"
            )),
        },
        ExcType::TimeoutError => Diagnosis {
            outcome: Outcome::Limited,
            diagnostic: Some(timeout_guidance(message)),
        },
        // The two CPython formatting habits the subset omits. `str.format` surfaces as a missing
        // attribute; `%` surfaces as a missing operator, because `str` has no `__mod__`. They need
        // the same redirection, and the operator message names neither formatting nor f-strings, so
        // without this arm the most mechanical of the two failures is the one left unexplained.
        ExcType::AttributeError if message.contains("format") => Diagnosis {
            outcome: Outcome::Exception,
            diagnostic: Some(formatting_guidance(message)),
        },
        ExcType::TypeError if message.contains("for %: 'str'") => Diagnosis {
            outcome: Outcome::Exception,
            diagnostic: Some(formatting_guidance(message)),
        },
        // Exception constructors take at most one string, and the arity failure arrives as an
        // internal error naming Monty rather than the call the caller wrote.
        ExcType::RuntimeError
            if message
                .contains("exceptions can only be called with zero or one string argument") =>
        {
            Diagnosis {
                outcome: Outcome::Exception,
                diagnostic: Some(format!(
                    "Exception constructors accept at most one string argument, so multi-argument forms such as OSError(errno, message, filename) are unavailable. Pass a single message, building it with an f-string if it needs detail. ({message})"
                )),
            }
        }
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
        "Execution exceeded its time budget and was stopped. Reduce the work, or raise timeout up to its cap. Note that enumerate, zip, reversed, and generator expressions are eager, so an infinite source such as (f(x) for x in itertools.count()) never terminates.{suffix}"
    )
}

/// Shared wording for the two formatting mechanisms the subset omits.
fn formatting_guidance(detail: &str) -> String {
    format!("str.format() and %-formatting are not implemented. Use f-strings instead. ({detail})")
}

/// Guidance for an import the subset does not provide.
///
/// The list must stay exactly the set the worker resolves. Naming a module Monty does not implement
/// turns this from a redirection into an instruction to retry a failure.
fn module_guidance(detail: &str) -> String {
    format!(
        "Only these standard library modules exist: {}. Third-party packages cannot be installed or imported. Do not retry this import; compute the result with the available modules, or use a different tool. ({detail})",
        available_modules!()
    )
}

/// Guidance for a name the subset does not provide.
fn undefined_name_guidance(detail: &str) -> String {
    format!(
        "Undefined name. Note that {} are not implemented, and nothing persists between code_execution calls. ({detail})",
        withheld_builtins!()
    )
}

/// Guidance for a name the interpreter defines but the type stubs omit.
///
/// Neither of the other two answers fits. Calling it undefined is false, and it would send the
/// caller looking for a different algorithm when the one it wrote is fine. Saying nothing leaves the
/// rejection reading as a type error in the caller's own code.
fn untyped_name_guidance(name: &str) -> String {
    let others: Vec<&str> = UNTYPED_BUILTINS
        .into_iter()
        .filter(|candidate| *candidate != name)
        .collect();
    format!(
        "`{name}` exists in the interpreter but is missing from its type stubs, so type checking rejects it before the snippet runs. The same is true of {}. Use a comprehension in place of map or filter, or ask the operator to disable type checking.",
        oxford(&others)
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
        // `open` is checked first: it is also unstubbed, but the isolation refusal it would hit at
        // runtime is the more useful thing to tell the caller than anything about type stubs.
        let guidance = if name == "open" {
            isolation_guidance("`open` is not available")
        } else if UNTYPED_BUILTINS.contains(&name) {
            untyped_name_guidance(name)
        } else if WITHHELD_BUILTINS.contains(&name) {
            undefined_name_guidance(&format!("`{name}` is not implemented"))
        } else {
            undefined_name_guidance(&format!("`{name}` is not defined"))
        };
        return format!("{guidance}\n{trimmed}");
    }
    // Annotations are optional and only ever add constraints, so the instinctive reaction to a
    // rejected annotation — annotate harder — is the one reliable way to make this worse.
    format!(
        "Type checking rejected this snippet before running it, so nothing was executed. Annotations are never required: unannotated code is inferred permissively, and an annotation only adds a constraint the checker then enforces. Widen or drop the annotation the diagnostic names rather than adding more. Correct the reported problems, or ask the operator to disable type checking if the code is intentionally dynamic.\n{trimmed}"
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

    /// `map` runs; only the checker refuses it. Reporting it as undefined would send the caller
    /// looking for a different algorithm when the one it wrote was fine.
    #[test]
    fn an_unstubbed_builtin_is_not_reported_as_undefined() {
        let text = diagnose_type_errors(
            "snippet.py:1:6: error[unresolved-reference] Name `map` used when not defined",
        );
        assert!(text.contains("missing from its type stubs"), "{text}");
        assert!(text.contains("comprehension"), "{text}");
        assert!(
            !text.contains("are not implemented"),
            "the undefined-name wording must not be used here: {text}"
        );
        // The name under discussion must not also appear in the list of the others.
        let (_, others) = text
            .split_once("The same is true of ")
            .expect("sibling list");
        assert!(
            !others.starts_with("map") && !others.contains(", map"),
            "guidance repeats the name it is already explaining: {text}"
        );
        assert!(
            others.starts_with("filter, getattr, setattr, and hasattr"),
            "{text}"
        );
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

    /// The parser refuses the target before anything runs, so this is a rejection rather than a
    /// raise, and the guidance has to carry the rewrite: the caller cannot derive it from the
    /// message, which names only the offending leaf kind.
    #[test]
    fn an_unsupported_unpacking_target_is_rejected_with_the_rewrite() {
        for leaf in ["subscript", "attribute"] {
            let diagnosis = diagnose(&exception(
                ExcType::SyntaxError,
                &format!("invalid unpacking target: {leaf}"),
            ));
            assert_eq!(diagnosis.outcome, Outcome::Rejected);
            let text = diagnosis.diagnostic.expect("guidance");
            assert!(text.contains("did not run"), "{text}");
            assert!(text.contains("temporary"), "{text}");
            // Saying only what fails would imply unpacking is broken generally, which it is not.
            assert!(
                text.contains("every other form of unpacking works"),
                "{text}"
            );
        }
    }

    /// Sandbox code can raise `SyntaxError` itself, so the type alone must not imply a rejection.
    #[test]
    fn a_plain_syntax_error_stays_an_exception() {
        let diagnosis = diagnose(&exception(ExcType::SyntaxError, "bad input from a caller"));
        assert_eq!(diagnosis.outcome, Outcome::Exception);
        assert!(diagnosis.diagnostic.is_none());
    }

    /// The two formatting habits arrive as unrelated exception types and need one answer.
    #[test]
    fn both_formatting_mechanisms_point_at_f_strings() {
        let attribute = diagnose(&exception(
            ExcType::AttributeError,
            "'str' object has no attribute 'format'",
        ));
        let operator = diagnose(&exception(
            ExcType::TypeError,
            "unsupported operand type(s) for %: 'str' and 'str'",
        ));
        for diagnosis in [attribute, operator] {
            assert_eq!(diagnosis.outcome, Outcome::Exception);
            assert!(
                diagnosis
                    .diagnostic
                    .expect("guidance")
                    .contains("f-strings")
            );
        }
    }

    /// Integer `%` is modulo. Matching the operator alone would annotate ordinary arithmetic errors
    /// with formatting advice, so the arm keys on the left operand being a string.
    #[test]
    fn modulo_type_errors_are_not_mistaken_for_formatting() {
        let diagnosis = diagnose(&exception(
            ExcType::TypeError,
            "unsupported operand type(s) for %: 'int' and 'str'",
        ));
        assert!(diagnosis.diagnostic.is_none());
    }

    /// The arity failure names Monty's internals rather than the constructor the caller wrote.
    #[test]
    fn exception_constructor_arity_is_translated_into_the_callers_terms() {
        let diagnosis = diagnose(&exception(
            ExcType::RuntimeError,
            "Internal error in monty: exceptions can only be called with zero or one string argument",
        ));
        assert_eq!(diagnosis.outcome, Outcome::Exception);
        let text = diagnosis.diagnostic.expect("guidance");
        assert!(text.contains("at most one string"), "{text}");
        assert!(text.contains("f-string"), "{text}");
    }

    /// An annotation is the only thing that can produce `invalid-assignment`, so answering it by
    /// suggesting more annotations would be the one reliably wrong move.
    #[test]
    fn generic_type_guidance_does_not_recommend_more_annotations() {
        let text = diagnose_type_errors(
            "snippet.py:1:10: error[invalid-assignment] Object of type `Literal[\"x\"]` is not assignable to `int`",
        );
        assert!(text.contains("Annotations are never required"), "{text}");
        assert!(text.contains("Widen or drop"), "{text}");
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
