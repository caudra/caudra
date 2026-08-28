//! Suspension answering.
//!
//! A feed suspends whenever the sandbox reaches outside itself. Workcell exposes no host functions
//! and no mounts, so every suspension is answered by refusing it. This is where the tool's isolation
//! claim is actually enforced: not by configuration the worker could misread, but by the parent
//! having no handler to offer.
//!
//! Environment reads are the one case answered with a value rather than a refusal. Returning an
//! empty environment makes `os.getenv` deterministically `None` instead of raising something the
//! caller has to interpret, and it guarantees no host environment value can cross the boundary.

use monty_pool::{ResumeValue, TurnEvent};
use monty_types::MontyObject;

/// OS-call names that read the process environment. Monty routes `os.getenv` and `os.environ`
/// through the host so the host can curate them; Workcell curates them to nothing.
const ENVIRONMENT_CALLS: [&str; 2] = ["os.getenv", "os.environ"];

/// How a suspension should be answered.
pub(crate) enum Answer {
    /// Hand the sandbox a value.
    Resume(ResumeValue),
    /// Resolve an undefined name as genuinely undefined, raising `NameError`.
    NameError,
}

/// Decides the answer for one suspension without consulting anything outside this process.
pub(crate) fn answer(event: &TurnEvent) -> Answer {
    match event {
        TurnEvent::OsCall { function_name, .. } if is_environment_call(function_name) => {
            Answer::Resume(ResumeValue::Return(empty_environment(function_name)))
        }
        // `NotHandled` makes the sandbox raise the call's own no-handler default, which for a
        // filesystem call is `PermissionError`. Refusing here rather than inventing an error keeps
        // the message consistent with what Monty documents for an unmounted path. It is only valid
        // for an OS call.
        TurnEvent::OsCall { .. } => Answer::Resume(ResumeValue::NotHandled),
        // Calling an unknown name suspends as a function call rather than a name lookup, so this is
        // the arm that catches `eval(...)`, `open(...)`, and every other absent callable. `NotFound`
        // raises `NameError`, matching what Monty documents for the unimplemented builtins.
        TurnEvent::FunctionCall { .. } => Answer::Resume(ResumeValue::NotFound),
        // No host functions are exposed, so an undefined name is exactly that.
        TurnEvent::NameLookup { .. } => Answer::NameError,
        // Unreachable without host futures, but a worker is untrusted input: refuse rather than panic.
        TurnEvent::ResolveFutures { .. } | TurnEvent::Complete(_) => {
            Answer::Resume(ResumeValue::NotFound)
        }
    }
}

fn is_environment_call(function_name: &str) -> bool {
    ENVIRONMENT_CALLS.contains(&function_name)
}

/// `os.environ` expects a mapping; `os.getenv` expects the looked-up value.
fn empty_environment(function_name: &str) -> MontyObject {
    if function_name == "os.environ" {
        MontyObject::dict(Vec::new())
    } else {
        MontyObject::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_call(name: &str) -> TurnEvent {
        TurnEvent::OsCall {
            function_name: name.to_owned(),
            args: Vec::new(),
            kwargs: Vec::new(),
            call_id: 1,
        }
    }

    #[test]
    fn filesystem_calls_are_refused() {
        for name in ["Path.read_text", "Path.iterdir", "Path.mkdir", "open"] {
            assert!(
                matches!(
                    answer(&os_call(name)),
                    Answer::Resume(ResumeValue::NotHandled)
                ),
                "{name} must be refused so the sandbox raises PermissionError"
            );
        }
    }

    #[test]
    fn environment_reads_yield_an_empty_environment() {
        assert!(matches!(
            answer(&os_call("os.getenv")),
            Answer::Resume(ResumeValue::Return(MontyObject::None))
        ));
        let Answer::Resume(ResumeValue::Return(MontyObject::Dict(pairs))) =
            answer(&os_call("os.environ"))
        else {
            panic!("os.environ must resolve to a mapping");
        };
        assert!(
            pairs.is_empty(),
            "no host environment value may cross the boundary"
        );
    }

    #[test]
    fn undefined_names_stay_undefined() {
        let event = TurnEvent::NameLookup {
            name: "requests".to_owned(),
        };
        assert!(matches!(answer(&event), Answer::NameError));
    }

    #[test]
    fn calls_to_absent_names_raise_name_error() {
        // Monty surfaces `eval(...)` and friends as a function call, not a name lookup, so this arm
        // is what makes the unimplemented builtins report `NameError` as documented.
        for name in ["eval", "exec", "compile", "fetch_from_host"] {
            let event = TurnEvent::FunctionCall {
                function_name: name.to_owned(),
                args: Vec::new(),
                kwargs: Vec::new(),
                call_id: 1,
                method_call: false,
            };
            assert!(
                matches!(answer(&event), Answer::Resume(ResumeValue::NotFound)),
                "{name} must resolve to NameError"
            );
        }
    }
}
