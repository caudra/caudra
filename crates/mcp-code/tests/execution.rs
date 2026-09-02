#![cfg(feature = "mcp")]

//! End-to-end execution against a real `monty` worker.
//!
//! These tests assert the behaviour the tool description promises: the value contract, the isolation
//! claims, and the failure classification an agent is steered by. They need the worker binary, which
//! is built separately from the workspace (`make code-worker`), so they skip with an explicit
//! message when it is absent rather than silently passing.

use std::{path::PathBuf, sync::OnceLock};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
#[cfg(feature = "bundled-worker")]
use workcell_mcp_code::bundled_worker_available;
use workcell_mcp_code::{
    CodeConfiguration, CodeInput, CodeToolGroup, SUBSET_MODULES, UNTYPED_BUILTINS,
    WITHHELD_BUILTINS, WORKER_FILE_NAME, WorkerSource, catalog,
};

/// Resolves the worker the same way the server does, plus the in-repo build location so a developer
/// who ran `make code-worker` needs no extra configuration.
fn worker() -> Option<PathBuf> {
    static WORKER: OnceLock<Option<PathBuf>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            if let Some(configured) = std::env::var_os("WORKCELL_MCP_CODE_WORKER") {
                let configured = PathBuf::from(configured);
                return usable_worker(&configured).then_some(configured);
            }
            let installed = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/code-worker/bin")
                .join(WORKER_FILE_NAME);
            if usable_worker(&installed) {
                return Some(installed);
            }
            if let Some(adjacent) = std::env::current_exe().ok().and_then(|executable| {
                executable
                    .parent()
                    .map(|directory| directory.join(WORKER_FILE_NAME))
            }) && usable_worker(&adjacent)
            {
                return Some(adjacent);
            }
            let path = std::env::var_os("PATH")?;
            std::env::split_paths(&path)
                .map(|directory| directory.join(WORKER_FILE_NAME))
                .find(|candidate| usable_worker(candidate))
        })
        .clone()
}

fn usable_worker(path: &std::path::Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

macro_rules! group_or_skip {
    () => {
        group_or_skip!(false)
    };
    ($type_check:expr) => {{
        let Some(worker) = worker() else {
            eprintln!(
                "skipping: no `monty` worker found. Run `make code-worker` or set WORKCELL_MCP_CODE_WORKER."
            );
            return;
        };
        CodeToolGroup::new(CodeConfiguration {
            worker: WorkerSource::Path(&worker),
            type_check: $type_check,
        })
            .await
            .expect("worker pool starts")
    }};
}

/// Runs one snippet and returns the structured envelope.
async fn run(group: &CodeToolGroup, code: &str) -> Value {
    run_with(group, json!({ "code": code })).await
}

async fn run_with(group: &CodeToolGroup, arguments: Value) -> Value {
    let result = group
        .dispatch("code_execution", arguments, CancellationToken::new())
        .await
        .expect("tool is claimed by this group")
        .expect("dispatch does not fault");
    result
        .structured_content
        .expect("every result carries the structured envelope")
}

#[tokio::test]
async fn returns_the_value_of_the_final_expression() {
    let group = group_or_skip!();
    let output = run(&group, "21 * 2").await;
    assert_eq!(output["outcome"], "completed");
    assert_eq!(output["result"], json!(42));
    assert_eq!(output["version"], json!(1));
    assert_eq!(output["kind"], "code");
    group.shutdown().await;
}

#[cfg(feature = "bundled-worker")]
#[tokio::test]
async fn bundled_worker_executes_without_an_external_path() {
    if !bundled_worker_available() {
        assert!(
            option_env!("WORKCELL_BUNDLED_MONTY_WORKER").is_none(),
            "the configured worker was not embedded"
        );
        return;
    }
    let cache = tempfile::tempdir().expect("temporary cache");
    let group = CodeToolGroup::new(CodeConfiguration {
        worker: WorkerSource::Bundled {
            cache_root: cache.path(),
        },
        type_check: true,
    })
    .await
    .expect("bundled worker pool starts");

    let output = run(&group, "21 * 2").await;

    assert_eq!(output["outcome"], "completed");
    assert_eq!(output["result"], json!(42));
    group.shutdown().await;
}

#[tokio::test]
async fn native_execute_returns_typed_output_and_exact_model_summary() {
    let group = group_or_skip!();
    let execution = group
        .execute(
            CodeInput {
                code: "21 * 2".into(),
                timeout: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("valid input")
        .expect("not cancelled");
    assert_eq!(execution.output.result, json!(42));
    assert_eq!(execution.model_text, "result: 42");

    let error = group
        .execute(
            CodeInput {
                code: "   ".into(),
                timeout: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("typed validation");
    assert_eq!(error, "Invalid arguments: code must not be empty");
    group.shutdown().await;
}

#[tokio::test]
async fn captures_print_output_in_order() {
    let group = group_or_skip!();
    let output = run(&group, "print('a')\nprint('b')\n7").await;
    assert_eq!(output["outcome"], "completed");
    assert_eq!(output["stdout"], "a\nb\n");
    assert_eq!(output["result"], json!(7));
    assert_eq!(output["stdoutTruncated"], json!(false));
    group.shutdown().await;
}

#[tokio::test]
async fn state_does_not_leak_between_calls() {
    let group = group_or_skip!();
    let first = run(&group, "carried_over = 5\ncarried_over").await;
    assert_eq!(first["result"], json!(5));

    // The description promises independence; a returned worker must not carry definitions forward.
    let second = run(&group, "carried_over").await;
    assert_eq!(second["outcome"], "exception");
    assert_eq!(second["exception"]["type"], "NameError");
    group.shutdown().await;
}

#[tokio::test]
async fn supports_the_advertised_stdlib_modules() {
    let group = group_or_skip!();
    let output = run(
        &group,
        "import json, math, re\nvalue = json.loads('{\"n\": 9}')\n[math.isqrt(value['n']), bool(re.match('a', 'abc'))]",
    )
    .await;
    assert_eq!(output["outcome"], "completed");
    assert_eq!(output["result"], json!([3, true]));
    group.shutdown().await;
}

/// The description and the import diagnostic both enumerate the modules, and for three of them —
/// `base64`, `binascii`, and `functools` — the enumeration was wrong for as long as it existed.
/// Spot-checking a handful of real modules could never catch that, so every advertised name is
/// imported here against a running worker.
#[tokio::test]
async fn every_advertised_module_imports() {
    let group = group_or_skip!();
    for module in SUBSET_MODULES {
        let output = run(&group, &format!("import {module}\nTrue")).await;
        assert_eq!(
            output["outcome"], "completed",
            "{module} is advertised but the worker answered {output}"
        );
    }
    group.shutdown().await;
}

/// The complement of the above: a name the worker does resolve must not be advertised as withheld,
/// and a name it does not resolve must be. `map` and `filter` were described as eager rather than
/// absent, which sent callers into a failure the description had told them to expect to work.
///
/// Bare references are used deliberately. Monty routes a *call* to an absent name through the OS
/// handler, so `open('x')` raises `PermissionError` rather than `NameError`; a bare reference is a
/// name lookup, which is the question being asked here.
#[tokio::test]
async fn every_withheld_builtin_is_actually_absent() {
    let group = group_or_skip!();
    let mut resolved = Vec::new();
    for name in WITHHELD_BUILTINS {
        let output = run(&group, name).await;
        if output["exception"]["type"] != "NameError" {
            resolved.push(format!("{name} -> {}", output["result"]));
        }
    }
    group.shutdown().await;
    // Reported together: fixing these one panic at a time is what let the lists drift in the first
    // place, and a name that resolves at runtime is advertised wrongly even if others also are.
    assert!(
        resolved.is_empty(),
        "advertised as withheld but the worker resolves them: {resolved:?}"
    );
}

/// The absent-module sentence is the other half of the import diagnostic's promise: naming a module
/// there that the worker can in fact import would send a caller to the shell tool for no reason.
#[tokio::test]
async fn modules_named_as_absent_really_are() {
    let group = group_or_skip!();
    for module in [
        "base64",
        "binascii",
        "functools",
        "random",
        "statistics",
        "hashlib",
        "urllib",
    ] {
        let output = run(&group, &format!("import {module}")).await;
        assert_eq!(
            output["outcome"], "exception",
            "{module} is described as absent but imported"
        );
        assert_eq!(output["exception"]["type"], "ModuleNotFoundError");
    }
    group.shutdown().await;
}

/// The description is the steering surface, so its claims about the subset have to be the ones the
/// worker honours. These are the divergences an agent is most likely to write code against.
#[tokio::test]
async fn the_described_cpython_divergences_hold() {
    let group = group_or_skip!();

    // Eager iterators, including generator expressions, which CPython keeps lazy.
    let eager = run(&group, "[type(zip([1], [2])) is list, type(enumerate('a')) is list, type(reversed([1])) is list, type(i for i in range(2)) is list]").await;
    assert_eq!(eager["result"], json!([true, true, true, true]), "{eager}");

    // Operators ignore user-defined dunders, but the method is still callable directly.
    let dunder = run(
        &group,
        "class A:\n    def __init__(self, v):\n        self.v = v\n    def __add__(self, o):\n        return A(self.v + o.v)\na = A(1)\ntry:\n    a + a\n    r = 'dispatched'\nexcept TypeError:\n    r = 'ignored'\n[r, a.__add__(a).v]",
    )
    .await;
    assert_eq!(dunder["result"], json!(["ignored", 2]), "{dunder}");

    // re.sub takes a string replacement only; a callable is refused rather than applied.
    let sub = run(
        &group,
        "import re\ntry:\n    re.sub(r'\\d', lambda m: m.group(), '1')\n    r = 'applied'\nexcept TypeError:\n    r = 'refused'\nr",
    )
    .await;
    assert_eq!(sub["result"], json!("refused"), "{sub}");

    // os carries constants but no path submodule; pathlib is the advertised replacement.
    let paths = run(
        &group,
        "import os\nfrom pathlib import Path\n[os.sep, str(Path('/a') / 'b.txt'), Path('/a/b.txt').suffix]",
    )
    .await;
    assert_eq!(paths["result"], json!(["/", "/a/b.txt", ".txt"]), "{paths}");

    group.shutdown().await;
}

/// Unpacking is nearly complete, and describing it as unsupported would cost far more than the one
/// gap does. The gap is that the parser accepts only a name, tuple, list, or starred name as a leaf,
/// so the ordinary element swap is refused; upstream reports that as a bare `SyntaxError`, which
/// reads as a mistake in the caller's own code. Both halves are pinned: the forms that work, and the
/// rewrite the diagnostic promises.
#[tokio::test]
async fn unpacking_works_except_into_subscripts_and_attributes() {
    let group = group_or_skip!(true);

    for (label, code, expected) in [
        ("sequence", "a, b = (1, 2)\n[a, b]", json!([1, 2])),
        (
            "starred",
            "a, *rest = [1, 2, 3]\n[a, rest]",
            json!([1, [2, 3]]),
        ),
        (
            "nested",
            "(a, (b, c)) = (1, (2, 3))\n[a, b, c]",
            json!([1, 2, 3]),
        ),
        (
            "loop target",
            "t = 0\nfor a, b in [(1, 2)]:\n    t = a + b\nt",
            json!(3),
        ),
        (
            "call site",
            "def add(a, b):\n    return a + b\nadd(*[1, 2])",
            json!(3),
        ),
        (
            "literal splat",
            "[*range(2), *'ab']",
            json!([0, 1, "a", "b"]),
        ),
        (
            "name swap",
            "i, j = 0, 1\ni, j = j, i\n[i, j]",
            json!([1, 0]),
        ),
    ] {
        let output = run(&group, code).await;
        assert_eq!(output["outcome"], "completed", "{label}: {output}");
        assert_eq!(output["result"], expected, "{label}");
    }

    // Both leaf kinds the parser refuses. Nothing ran, so the outcome is a rejection, not a raise.
    for (label, code) in [
        ("subscript swap", "x = [1, 2]\nx[0], x[1] = x[1], x[0]\nx"),
        (
            "computed index",
            "x = [1, 2]\ni, j = 0, 1\nx[i], x[j] = x[j], x[i]\nx",
        ),
        (
            "dict subscript",
            "d = {'a': 1, 'b': 2}\nd['a'], d['b'] = d['b'], d['a']\nd",
        ),
        (
            "attribute",
            "class P:\n    def __init__(self):\n        self.a = 1\n        self.b = 2\np = P()\np.a, p.b = p.b, p.a\n[p.a, p.b]",
        ),
    ] {
        let output = run(&group, code).await;
        assert_eq!(output["outcome"], "rejected", "{label}: {output}");
        assert_eq!(output["exception"]["type"], "SyntaxError", "{label}");
        let diagnostic = output["diagnostic"].as_str().expect("guidance");
        assert!(
            diagnostic.contains("did not run") && diagnostic.contains("temporary"),
            "{label} must be told the rewrite: {diagnostic}"
        );
    }

    // The rewrite the diagnostic names has to be one the worker actually accepts.
    let rewritten = run(&group, "x = [1, 2]\nt = x[0]\nx[0] = x[1]\nx[1] = t\nx").await;
    assert_eq!(rewritten["result"], json!([2, 1]), "{rewritten}");

    group.shutdown().await;
}

/// Type checking is on by default, so the description's claim about annotations is one an agent acts
/// on constantly. The claim is that annotations are optional and only ever add constraints, which is
/// the opposite of the usual instinct: the failing snippet here is the annotated one.
#[tokio::test]
async fn annotations_are_never_required_and_only_add_constraints() {
    let group = group_or_skip!(true);

    for (label, code) in [
        (
            "empty then mixed",
            "xs = []\nxs.append(1)\nxs.append('a')\nlen(xs)",
        ),
        (
            "widening an inferred list",
            "xs = [1, 2]\nxs.append('a')\nlen(xs)",
        ),
        ("empty dict", "d = {}\nd['a'] = 1\nd['b'] = 'x'\nlen(d)"),
        (
            "unannotated parameters",
            "def f(a, b):\n    return a + b\nf(1, 2)",
        ),
        ("heterogeneous literal", "xs = [1, None, 'a']\nlen(xs)"),
    ] {
        let output = run(&group, code).await;
        assert_eq!(
            output["outcome"], "completed",
            "unannotated code must pass the checker; {label}: {output}"
        );
    }

    // The constraint an annotation introduces is real, and the guidance must not answer it by
    // suggesting more annotations.
    let annotated = run(&group, "x: int = 'nope'\nx").await;
    assert_eq!(annotated["outcome"], "rejected", "{annotated}");
    let diagnostic = annotated["diagnostic"].as_str().expect("guidance");
    assert!(
        diagnostic.contains("Annotations are never required"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("Widen or drop"), "{diagnostic}");

    // An annotation is never evaluated, so the same expression is fine in a hint and fatal as a
    // value. This is the distinction the description draws, and it holds in both directions.
    let hint = run(&group, "x: list[int] = [1]\nx").await;
    assert_eq!(hint["outcome"], "completed", "{hint}");
    let value = run(&group, "y = list[int]\n1").await;
    assert_eq!(value["outcome"], "exception", "{value}");
    assert_eq!(value["exception"]["type"], "TypeError", "{value}");

    group.shutdown().await;
}

/// The checker resolves five modules the interpreter does not implement, because the stubs need
/// them. An agent told only that snippets are checked before running would read a clean check as a
/// guarantee, so the description says otherwise and this holds it to that.
#[tokio::test]
async fn some_modules_pass_the_type_check_and_then_fail_at_import() {
    let group = group_or_skip!(true);
    for module in [
        "abc",
        "types",
        "typing_extensions",
        "_collections_abc",
        "_typeshed",
    ] {
        let output = run(&group, &format!("import {module}\n1")).await;
        // `exception`, not `rejected`: the snippet was allowed to run and failed at the import.
        assert_eq!(
            output["outcome"], "exception",
            "{module} is described as type-checking clean: {output}"
        );
        assert_eq!(
            output["exception"]["type"], "ModuleNotFoundError",
            "{module}"
        );
    }
    group.shutdown().await;
}

/// Both formatting habits the subset omits arrive as unrelated exception types, and the `%` one
/// names neither formatting nor f-strings. Without guidance it reads as an arithmetic error.
#[tokio::test]
async fn both_formatting_habits_redirect_to_f_strings() {
    let group = group_or_skip!();
    for (label, code, exc) in [
        ("str.format", "'{}'.format(1)", "AttributeError"),
        ("percent", "'%s' % 'x'", "TypeError"),
    ] {
        let output = run(&group, code).await;
        assert_eq!(output["outcome"], "exception", "{label}: {output}");
        assert_eq!(output["exception"]["type"], exc, "{label}");
        let diagnostic = output["diagnostic"].as_str().expect("guidance");
        assert!(diagnostic.contains("f-strings"), "{label}: {diagnostic}");
    }

    // Integer `%` is modulo and must not be mistaken for the formatting operator.
    let modulo = run(&group, "7 % 3").await;
    assert_eq!(modulo["result"], json!(1), "{modulo}");
    assert!(modulo["diagnostic"].is_null(), "{modulo}");

    group.shutdown().await;
}

/// The arity failure surfaces as a `RuntimeError` describing an internal error in Monty, which tells
/// a caller nothing about the constructor it wrote.
#[tokio::test]
async fn multi_argument_exception_constructors_are_explained() {
    let group = group_or_skip!();
    let output = run(&group, "raise OSError(2, 'missing', 'f.txt')").await;
    assert_eq!(output["outcome"], "exception", "{output}");
    let diagnostic = output["diagnostic"].as_str().expect("guidance");
    assert!(diagnostic.contains("at most one string"), "{diagnostic}");

    // The single-argument form the guidance points at has to work.
    let single = run(
        &group,
        "try:\n    raise OSError('missing f.txt')\nexcept OSError as e:\n    r = str(e)\nr",
    )
    .await;
    assert_eq!(single["result"], json!("missing f.txt"), "{single}");

    group.shutdown().await;
}

/// A failed import must not hand back guidance naming modules that do not exist, which is what made
/// the drift expensive rather than merely untidy: the diagnostic invited the next failing attempt.
#[tokio::test]
async fn import_guidance_never_names_a_module_that_is_missing() {
    let group = group_or_skip!();
    let output = run(&group, "import functools").await;
    let diagnostic = output["diagnostic"].as_str().expect("guidance");
    // Only the enumeration is under test. The trailing parenthetical echoes Monty's own message,
    // which necessarily repeats the module the caller asked for.
    let offered = diagnostic
        .split_once("modules exist: ")
        .expect("the guidance enumerates the available modules")
        .1
        .split_once('.')
        .expect("the enumeration is a sentence")
        .0;
    for absent in ["base64", "binascii", "functools"] {
        assert!(
            !offered.contains(absent),
            "guidance offered {absent}, which the worker cannot import: {offered}"
        );
    }
    // The enumeration is still present rather than having been emptied out.
    assert!(offered.contains("unicodedata"), "{offered}");
    group.shutdown().await;
}

/// `map` and `filter` are the sharp edge of the subset: they run fine but the default type checking
/// rejects them, so both answers a caller could receive have to be right for the mode it is in.
#[tokio::test]
async fn unstubbed_builtins_run_but_are_rejected_by_type_checking() {
    let permissive = group_or_skip!(false);
    let mut absent = Vec::new();
    for name in UNTYPED_BUILTINS {
        let output = run(&permissive, name).await;
        if output["outcome"] != "completed" {
            absent.push(format!("{name} -> {}", output["exception"]));
        }
    }
    permissive.shutdown().await;
    assert!(
        absent.is_empty(),
        "described as existing at runtime but the worker does not define them: {absent:?}"
    );

    let checked = group_or_skip!(true);
    let output = run(&checked, "list(map(str, [1, 2]))").await;
    assert_eq!(output["outcome"], "rejected");
    let diagnostic = output["diagnostic"].as_str().expect("guidance");
    assert!(
        diagnostic.contains("missing from its type stubs"),
        "a name that exists must not be reported as undefined: {diagnostic}"
    );
    assert!(diagnostic.contains("comprehension"), "{diagnostic}");
    checked.shutdown().await;
}

/// The catalog description and the runtime diagnostics are separate strings built from one source.
/// This is what proves they were built from that source rather than kept in step by hand.
#[tokio::test]
async fn the_description_and_the_diagnostics_agree() {
    let tools = catalog();
    let description = tools[0].description.as_deref().expect("tool description");
    for module in SUBSET_MODULES {
        assert!(
            description.contains(module),
            "the description omits the advertised module {module}"
        );
    }
    for name in WITHHELD_BUILTINS.iter().chain(UNTYPED_BUILTINS.iter()) {
        assert!(
            description.contains(name),
            "the description omits the builtin {name}"
        );
    }

    let group = group_or_skip!();
    let output = run(&group, "import socket").await;
    let diagnostic = output["diagnostic"].as_str().expect("guidance");
    for module in SUBSET_MODULES {
        assert!(
            diagnostic.contains(module),
            "the import guidance omits {module}: {diagnostic}"
        );
    }
    group.shutdown().await;
}

#[tokio::test]
async fn filesystem_access_is_refused_and_redirected() {
    let group = group_or_skip!();
    let output = run(
        &group,
        "from pathlib import Path\nPath('/etc/passwd').read_text()",
    )
    .await;
    assert_eq!(output["outcome"], "exception");
    assert_eq!(output["exception"]["type"], "PermissionError");
    let diagnostic = output["diagnostic"].as_str().expect("guidance");
    assert!(diagnostic.contains("no filesystem, network, or environment access"));
    assert!(diagnostic.contains("file tools"));
    group.shutdown().await;
}

#[tokio::test]
async fn environment_reads_return_nothing_rather_than_leaking() {
    // PATH is present in this process, and the worker inherits an environment of its own, so a
    // visible PATH would prove the boundary leaks. An empty mapping is the whole contract.
    let host_path = std::env::var("PATH").expect("the test host has PATH set");
    assert!(!host_path.is_empty());

    let group = group_or_skip!();
    let output = run(&group, "import os\n[os.getenv('PATH'), len(os.environ)]").await;
    assert_eq!(output["outcome"], "completed");
    assert_eq!(output["result"], json!([Value::Null, 0]));
    assert!(
        !serde_json::to_string(&output)
            .expect("envelope serializes")
            .contains(&host_path),
        "no host environment value may appear in the result"
    );
    group.shutdown().await;
}

#[tokio::test]
async fn network_modules_are_absent() {
    let group = group_or_skip!();
    let output = run(&group, "import socket").await;
    assert_eq!(output["outcome"], "exception");
    assert_eq!(output["exception"]["type"], "ModuleNotFoundError");
    let diagnostic = output["diagnostic"].as_str().expect("guidance");
    assert!(diagnostic.contains("Do not retry this import"));
    assert!(
        diagnostic.contains("unicodedata"),
        "the available set must be enumerated"
    );
    group.shutdown().await;
}

#[tokio::test]
async fn unsupported_syntax_is_rejected_before_running() {
    let group = group_or_skip!();
    // A `match` statement is refused by the parser, so the preceding print must never run.
    let output = run(
        &group,
        "print('should not appear')\nmatch 1:\n    case 1:\n        pass",
    )
    .await;
    assert_eq!(output["outcome"], "rejected");
    assert_eq!(output["stdout"], "");
    let diagnostic = output["diagnostic"].as_str().expect("guidance");
    assert!(diagnostic.contains("did not run"));
    group.shutdown().await;
}

#[tokio::test]
async fn missing_builtins_are_explained() {
    let group = group_or_skip!();
    let output = run(&group, "eval('1 + 1')").await;
    assert_eq!(output["outcome"], "exception");
    assert_eq!(output["exception"]["type"], "NameError");
    let diagnostic = output["diagnostic"].as_str().expect("guidance");
    assert!(diagnostic.contains("eval"));
    group.shutdown().await;
}

#[tokio::test]
async fn ordinary_python_errors_are_returned_verbatim() {
    let group = group_or_skip!();
    let output = run(&group, "1 / 0").await;
    assert_eq!(output["outcome"], "exception");
    assert_eq!(output["exception"]["type"], "ZeroDivisionError");
    // Nothing about the subset is at fault here, so no guidance should be invented.
    assert_eq!(output["diagnostic"], Value::Null);
    group.shutdown().await;
}

#[tokio::test]
async fn runaway_loops_hit_the_time_budget() {
    let group = group_or_skip!();
    let output = run_with(
        &group,
        json!({ "code": "n = 0\nwhile True:\n    n += 1", "timeout": 1000 }),
    )
    .await;
    assert_eq!(output["outcome"], "limited");
    assert_eq!(output["timedOut"], json!(true));
    assert_eq!(output["timeoutMs"], json!(1000));
    let diagnostic = output["diagnostic"].as_str().expect("guidance");
    assert!(diagnostic.contains("time budget"));
    group.shutdown().await;
}

#[tokio::test]
async fn recursion_depth_is_bounded() {
    let group = group_or_skip!();
    let output = run(&group, "def f(n):\n    return f(n + 1)\nf(0)").await;
    assert_eq!(output["outcome"], "exception");
    assert_eq!(output["exception"]["type"], "RecursionError");
    assert!(
        output["diagnostic"]
            .as_str()
            .expect("guidance")
            .contains("1000")
    );
    group.shutdown().await;
}

#[tokio::test]
async fn lossy_values_carry_both_json_and_repr() {
    let group = group_or_skip!();
    let output = run(&group, "(1, 2)").await;
    assert_eq!(output["outcome"], "completed");
    assert_eq!(output["result"], json!([1, 2]));
    assert!(
        output["resultRepr"].is_string(),
        "a tuple loses its type in JSON"
    );

    let exact = run(&group, "{'a': 1}").await;
    assert_eq!(exact["result"], json!({"a": 1}));
    assert_eq!(exact["resultRepr"], Value::Null, "an object needs no repr");
    group.shutdown().await;
}

#[tokio::test]
async fn rejects_input_outside_the_advertised_schema() {
    let group = group_or_skip!();
    for arguments in [
        json!({ "code": "   " }),
        json!({ "code": "1", "timeout": 0 }),
        json!({ "code": "1", "timeout": 30_001 }),
        json!({ "code": "1", "unexpected": true }),
    ] {
        let result = group
            .dispatch(
                "code_execution",
                arguments.clone(),
                CancellationToken::new(),
            )
            .await
            .expect("claimed")
            .expect("no fault");
        assert_eq!(result.is_error, Some(true), "{arguments} must be refused");
    }
    group.shutdown().await;
}

#[tokio::test]
async fn foreign_tool_names_are_left_for_other_groups() {
    let group = group_or_skip!();
    assert!(
        group
            .dispatch("shell", json!({}), CancellationToken::new())
            .await
            .is_none()
    );
    group.shutdown().await;
}

#[tokio::test]
async fn type_checking_rejects_before_execution_when_enabled() {
    let Some(worker) = worker() else {
        eprintln!("skipping: no `monty` worker found. Run `make code-worker`.");
        return;
    };
    let group = CodeToolGroup::new(CodeConfiguration {
        worker: WorkerSource::Path(&worker),
        type_check: true,
    })
    .await
    .expect("worker pool starts");
    let output = run(&group, "print('should not appear')\n'text' + 1").await;
    assert_eq!(output["outcome"], "rejected");
    assert_eq!(output["typeChecked"], json!(true));
    assert_eq!(output["stdout"], "");
    assert!(
        output["diagnostic"]
            .as_str()
            .expect("guidance")
            .contains("nothing was executed")
    );
    group.shutdown().await;
}

/// Monty polls memory at execution checkpoints and raises an uncatchable `MemoryError` before the
/// allocator's hard ceiling. That graceful path is the common one, and it must read as a bounded
/// result rather than a fault or an ordinary exception the caller might try to handle.
#[tokio::test]
async fn memory_exhaustion_is_reported_as_a_limit() {
    let group = group_or_skip!();
    let output = run_with(
        &group,
        json!({ "code": "blob = []\nwhile True:\n    blob.append('x' * 1_000_000)", "timeout": 20_000 }),
    )
    .await;
    assert_eq!(output["outcome"], "limited");
    assert_eq!(output["timedOut"], json!(false));
    let diagnostic = output["diagnostic"].as_str().expect("guidance");
    assert!(
        diagnostic.contains("memory budget"),
        "guidance should name the limit that was hit, got {diagnostic:?}"
    );
    group.shutdown().await;
}

/// Upstream makes no guarantee about heap or refcount state after a resource limit fires, so the
/// interpreter state that hit the limit must never be reused. The next call has to land on clean
/// state and succeed, whether the pool recycled the session or replaced the worker outright.
#[tokio::test]
async fn the_group_keeps_serving_after_a_limit_poisons_a_session() {
    let group = group_or_skip!();
    let limited = run_with(
        &group,
        json!({ "code": "blob = []\nwhile True:\n    blob.append('x' * 1_000_000)", "timeout": 20_000 }),
    )
    .await;
    assert_eq!(limited["outcome"], "limited");

    let after = run(&group, "sum(range(10))").await;
    assert_eq!(after["outcome"], "completed");
    assert_eq!(after["result"], json!(45));
    // Nothing from the poisoned session survives into the replacement.
    let leaked = run(&group, "blob").await;
    assert_eq!(leaked["outcome"], "exception");
    assert_eq!(leaked["exception"]["type"], "NameError");
    group.shutdown().await;
}

/// Type checking is on by default, so these are the paths a client hits unless an operator turns it
/// off. The interpreter never runs, and the checker reports withheld capabilities as ordinary
/// unresolved names, so the redirection has to come from the rejection itself.
#[tokio::test]
async fn withheld_capabilities_are_redirected_under_the_default_type_checking() {
    let group = group_or_skip!(true);

    let file = run(&group, "open('/etc/passwd').read()").await;
    assert_eq!(file["outcome"], "rejected");
    let diagnostic = file["diagnostic"].as_str().expect("guidance");
    assert!(diagnostic.contains("file tools"), "{diagnostic}");
    assert!(
        !diagnostic.contains("disable type checking"),
        "{diagnostic}"
    );

    let package = run(&group, "import requests").await;
    assert_eq!(package["outcome"], "rejected");
    assert!(
        package["diagnostic"]
            .as_str()
            .expect("guidance")
            .contains("Third-party packages cannot be installed")
    );

    let builtin = run(&group, "eval('1 + 1')").await;
    assert_eq!(builtin["outcome"], "rejected");
    assert!(
        builtin["diagnostic"]
            .as_str()
            .expect("guidance")
            .contains("not implemented")
    );
    group.shutdown().await;
}

/// Type checking must not get in the way of ordinary correct code.
#[tokio::test]
async fn valid_snippets_pass_type_checking() {
    let group = group_or_skip!(true);
    let output = run(
        &group,
        "import json
rows = [{'n': i, 'sq': i * i} for i in range(4)]
json.dumps(rows)",
    )
    .await;
    assert_eq!(output["outcome"], "completed");
    assert_eq!(
        output["result"],
        json!(r#"[{"n": 0, "sq": 0}, {"n": 1, "sq": 1}, {"n": 2, "sq": 4}, {"n": 3, "sq": 9}]"#)
    );
    group.shutdown().await;
}
