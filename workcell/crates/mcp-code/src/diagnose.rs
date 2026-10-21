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
    SUBSET_MODULES, UNTYPED_BUILTINS, WITHHELD_BUILTINS, available_modules, oxford,
    withheld_builtins,
};
use crate::types::Outcome;

pub(crate) const UNRESOLVED_REFERENCE_RULE: &str = "[unresolved-reference]";
const UNRESOLVED_REFERENCE_NAME: &str = "[unresolved-reference] Name `";
const UNRESOLVED_ATTRIBUTE_RULE: &str = "[unresolved-attribute]";
const INVALID_SYNTAX_RULE: &str = "[invalid-syntax]";
const RETURN_OUTSIDE_FUNCTION: &str = "`return` statement outside of a function";
const TOP_LEVEL_RETURN_GUIDANCE: &str = "A snippet is module code, not a function body, so nothing ran. End on the value you want back instead of returning it, and keep `return` inside functions.";
const SYNTAX_GUIDANCE: &str = "The snippet is not valid Python, so nothing ran.";
/// The parser's words for an indentation error, as `Unexpected indentation`, `unindent does not
/// match any outer indentation level`, and `Expected an indented block after ...`.
const INDENTATION_ERRORS: [&str; 3] = [
    "Unexpected indentation",
    "unindent does not match",
    "Expected an indented block",
];
const INDENTATION_GUIDANCE: &str = "The snippet's indentation is not valid Python, so nothing ran. A line goes deeper only after one ending in `:`, every line of a block starts at the same column, and a dedent returns to a column an outer block used. Indent with spaces.";
/// How the interpreter's `NameError` starts, before the name.
const NAME_ERROR_PREFIX: &str = "name '";
const NAME_ERROR_QUOTE: char = '\'';
/// The withheld builtin a hosted run's host answers itself.
const HOSTED_HELP: &str = "help";

/// Classification of a raised exception plus the guidance that accompanies it.
pub(crate) struct Diagnosis {
    pub(crate) outcome: Outcome,
    pub(crate) diagnostic: Option<String>,
}

/// Which execution a failure came from. A hosted snippet reaches its host on purpose, so the
/// isolation advice an isolated snippet needs would send its caller the wrong way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Surface {
    Isolated,
    Hosted,
}

/// Maps a sandbox exception onto an outcome and, where the subset is the cause, an explanation.
pub(crate) fn diagnose_for(exception: &MontyException, surface: Surface) -> Diagnosis {
    let message = exception.message().unwrap_or_default();
    match (exception.exc_type(), surface) {
        (ExcType::NameError | ExcType::UnboundLocalError, Surface::Hosted) => {
            return Diagnosis {
                outcome: Outcome::Exception,
                diagnostic: Some(unimported_module(message).map_or_else(
                    || hosted_name_guidance(message),
                    |module| unimported_module_guidance(module, message),
                )),
            };
        }
        // The host raised it to refuse a call and its message already says why.
        (ExcType::PermissionError, Surface::Hosted) => {
            return Diagnosis {
                outcome: Outcome::Exception,
                diagnostic: None,
            };
        }
        (ExcType::TimeoutError, Surface::Hosted) => {
            return Diagnosis {
                outcome: Outcome::Limited,
                diagnostic: Some(hosted_timeout_guidance(message)),
            };
        }
        _ => {}
    }
    match exception.exc_type() {
        // Monty rejects unsupported syntax during compilation, so nothing ran and nothing printed.
        ExcType::NotImplementedError if message.contains("syntax parser does not yet support") => {
            Diagnosis {
                outcome: Outcome::Rejected,
                diagnostic: Some(format!(
                    "This Python construct is not supported and the snippet did not run. Unavailable: class inheritance and metaclasses, super(), decorators on methods (@classmethod, @staticmethod, @property), yield and generator functions, del, try*/except* groups, async with, async for, PEP 695 type aliases, wildcard imports, and t-strings. Rewrite without it, or use the shell tool if the task genuinely needs full Python. ({message})"
                )),
            }
        }
        ExcType::ModuleNotFoundError | ExcType::ImportError => Diagnosis {
            outcome: Outcome::Exception,
            diagnostic: Some(module_guidance(message)),
        },
        ExcType::NameError | ExcType::UnboundLocalError => Diagnosis {
            outcome: Outcome::Exception,
            diagnostic: Some(unimported_module(message).map_or_else(
                || undefined_name_guidance(message),
                |module| unimported_module_guidance(module, message),
            )),
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

/// The hosted form of [`timeout_guidance`]: there is no timeout to raise, and waiting on the host
/// is not what ran out.
pub(crate) fn hosted_timeout_guidance(detail: &str) -> String {
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(" ({detail})")
    };
    format!(
        "Computation exceeded its time budget and was stopped. Time spent waiting for calls to return does not count against it; loops in the snippet do. Reduce the work, and note that enumerate, zip, reversed, and generator expressions are eager.{suffix}"
    )
}

fn hosted_name_guidance(detail: &str) -> String {
    let withheld: Vec<&str> = WITHHELD_BUILTINS
        .into_iter()
        .filter(|name| *name != HOSTED_HELP)
        .collect();
    format!(
        "Undefined name. Use only the names the catalog lists, or find one with help(). Note that {} are not implemented. ({detail})",
        oxford(&withheld)
    )
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

/// Guidance for one of the subset's modules, used without importing it.
fn unimported_module_guidance(module: &str, detail: &str) -> String {
    format!(
        "`{module}` is a standard library module the interpreter provides, but it must be imported first: add `import {module}`. ({detail})"
    )
}

/// The module a `NameError` names, when it is one of the subset's.
fn unimported_module(message: &str) -> Option<&str> {
    let (name, _) = message
        .strip_prefix(NAME_ERROR_PREFIX)?
        .split_once(NAME_ERROR_QUOTE)?;
    SUBSET_MODULES.contains(&name).then_some(name)
}

/// Guidance for a name the subset does not provide.
fn undefined_name_guidance(detail: &str) -> String {
    format!(
        "Undefined name. Note that {} are not implemented, and nothing persists between code calls. ({detail})",
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
        "`{name}` exists in the interpreter but is missing from its type stubs, so type checking rejects it before the snippet runs. The same is true of {}. Use a comprehension in place of map or filter, f-strings in place of format(), or ask the operator to disable type checking.",
        oxford(&others)
    )
}

/// Guidance for an attempt to reach the host.
fn isolation_guidance(detail: &str) -> String {
    format!(
        "The code tool has no filesystem, network, or environment access. Read or write files with the file tools, fetch URLs with webfetch, or use the shell tool when host access is genuinely required. ({detail})"
    )
}

/// Explains a type-checking rejection.
///
/// Type checking is on by default, so it — not the interpreter — is what an agent usually hits
/// first. It reports a withheld capability as an ordinary unresolved name or import, which reads as
/// a typo and invites either a pointless retry or a request to disable type checking. Recognising
/// those two rules lets the caller receive the same redirection it would have received at runtime.
pub(crate) fn diagnose_type_errors_for(diagnostics: &str, surface: Surface) -> String {
    let trimmed = diagnostics.trim();
    if surface == Surface::Hosted && trimmed.contains(INVALID_SYNTAX_RULE) {
        let guidance = if trimmed.contains(RETURN_OUTSIDE_FUNCTION) {
            TOP_LEVEL_RETURN_GUIDANCE
        } else if INDENTATION_ERRORS
            .iter()
            .any(|error| trimmed.contains(error))
        {
            INDENTATION_GUIDANCE
        } else {
            SYNTAX_GUIDANCE
        };
        return format!("{guidance}\n{trimmed}");
    }
    if surface == Surface::Hosted {
        if trimmed.contains(UNRESOLVED_ATTRIBUTE_RULE) {
            return format!(
                "A name the snippet calls does not exist, so nothing was executed. Check the exact name and signature with help(\"module\") before calling it.\n{trimmed}"
            );
        }
        if let Some(name) = unresolved_reference(trimmed)
            && !UNTYPED_BUILTINS.contains(&name)
        {
            let detail = format!("`{name}` is not defined");
            let guidance = if SUBSET_MODULES.contains(&name) {
                unimported_module_guidance(name, &detail)
            } else {
                hosted_name_guidance(&detail)
            };
            return format!("{guidance}\n{trimmed}");
        }
        if unresolved_import(trimmed).is_none() && !trimmed.contains(UNRESOLVED_REFERENCE_RULE) {
            return format!(
                "Type checking rejected this snippet before running it, so nothing was executed. Arguments are checked against each tool's signature; help(\"module.name\") shows the exact signature. Annotations are never required, so widen or drop an annotation the diagnostic names rather than adding more.\n{trimmed}"
            );
        }
    }
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
        } else if SUBSET_MODULES.contains(&name) {
            unimported_module_guidance(name, &format!("`{name}` is not defined"))
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
        .split_once(UNRESOLVED_REFERENCE_NAME)?
        .1
        .split_once('`')
        .map(|(name, _)| name)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    fn diagnose(exception: &MontyException) -> Diagnosis {
        diagnose_for(exception, Surface::Isolated)
    }

    fn diagnose_type_errors(diagnostics: &str) -> String {
        diagnose_type_errors_for(diagnostics, Surface::Isolated)
    }

    #[test]
    fn hosted_refusals_keep_the_hosts_own_message() {
        let diagnosis = diagnose_for(
            &MontyException::new(ExcType::PermissionError, Some("denied".to_owned())),
            Surface::Hosted,
        );
        assert_eq!(diagnosis.outcome, Outcome::Exception);
        assert!(diagnosis.diagnostic.is_none());
    }

    #[test]
    fn hosted_unknown_attributes_point_at_help() {
        let text = diagnose_type_errors_for(
            "snippet.py:1:7: error[unresolved-attribute] Object of type `_Github` has no attribute `nope`",
            Surface::Hosted,
        );
        assert!(text.contains("help("), "{text}");
        assert!(!text.contains("between code calls"), "{text}");
    }

    #[test]
    fn a_hosted_undefined_name_never_calls_help_withheld() {
        let text = diagnose_type_errors_for(
            "snippet.py:1:1: error[unresolved-reference] Name `decide` used when not defined",
            Surface::Hosted,
        );
        assert!(text.contains("find one with help()"), "{text}");
        assert!(!text.contains("breakpoint, help"), "{text}");
    }

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
            "snippet.py:1:1: error[unresolved-reference] Name `compile` used when not defined",
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
            others.starts_with("filter, getattr, setattr, hasattr, and format"),
            "{text}"
        );
    }

    /// Syntax errors get no advice about arguments or annotations, which cannot be their cause.
    #[test_case("snippet.py:3:1: error[invalid-syntax] `return` statement outside of a function", TOP_LEVEL_RETURN_GUIDANCE ; "a_return_outside_a_function")]
    #[test_case("snippet.py:1:7: error[invalid-syntax] Simple statements must be separated by newlines or semicolons", SYNTAX_GUIDANCE ; "other_syntax")]
    #[test_case("snippet.py:3:1: error[invalid-syntax] unindent does not match any outer indentation level", INDENTATION_GUIDANCE ; "a_dedent_between_levels")]
    #[test_case("snippet.py:2:1: error[invalid-syntax] Unexpected indentation", INDENTATION_GUIDANCE ; "an_unexpected_indent")]
    #[test_case("snippet.py:2:1: error[invalid-syntax] Expected an indented block after `if` statement", INDENTATION_GUIDANCE ; "an_empty_block")]
    fn a_hosted_syntax_error_says_what_kind_of_code_a_snippet_is(
        diagnostics: &str,
        guidance: &str,
    ) {
        let text = diagnose_type_errors_for(diagnostics, Surface::Hosted);
        assert_eq!(text, format!("{guidance}\n{diagnostics}"));
    }

    const MISSING_ASYNCIO: &str =
        "name 'asyncio' is not defined. Did you forget to import 'asyncio'?";
    const IMPORT_ASYNCIO: &str = "add `import asyncio`";

    /// A module the subset provides is a missing import, not a name the subset lacks, whichever
    /// execution reports it and whether the checker or the interpreter finds it.
    #[test_case(Surface::Isolated ; "isolated")]
    #[test_case(Surface::Hosted ; "hosted")]
    fn an_unimported_module_asks_for_its_import(surface: Surface) {
        let raised = diagnose_for(&exception(ExcType::NameError, MISSING_ASYNCIO), surface)
            .diagnostic
            .expect("guidance");
        let checked = diagnose_type_errors_for(
            "snippet.py:1:17: error[unresolved-reference] Name `asyncio` used when not defined",
            surface,
        );
        for text in [raised, checked] {
            assert!(text.contains(IMPORT_ASYNCIO), "{text}");
            assert!(!text.contains("are not implemented"), "{text}");
        }
    }

    fn exception(exc_type: ExcType, message: &str) -> MontyException {
        MontyException::new(exc_type, Some(message.to_owned()))
    }

    #[test]
    fn unsupported_syntax_is_rejected_not_raised() {
        let diagnosis = diagnose(&exception(
            ExcType::NotImplementedError,
            "The monty syntax parser does not yet support del statements",
        ));
        assert_eq!(diagnosis.outcome, Outcome::Rejected);
        let text = diagnosis.diagnostic.expect("guidance");
        assert!(text.contains("did not run"));
        assert!(text.contains("del"));
        assert!(!text.contains("match"));
    }

    #[test]
    fn a_plain_not_implemented_error_stays_an_exception() {
        // Sandbox code can raise NotImplementedError itself; only the parser prefix means rejection.
        let diagnosis = diagnose(&exception(ExcType::NotImplementedError, "subclass me"));
        assert_eq!(diagnosis.outcome, Outcome::Exception);
        assert!(diagnosis.diagnostic.is_none());
    }

    /// Sandbox code can raise `SyntaxError` itself, so the type alone must not imply a rejection.
    #[test]
    fn a_plain_syntax_error_stays_an_exception() {
        let diagnosis = diagnose(&exception(ExcType::SyntaxError, "bad input from a caller"));
        assert_eq!(diagnosis.outcome, Outcome::Exception);
        assert!(diagnosis.diagnostic.is_none());
    }

    #[test]
    fn formatting_errors_do_not_claim_formatting_is_unimplemented() {
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
            assert!(diagnosis.diagnostic.is_none());
        }
    }

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
