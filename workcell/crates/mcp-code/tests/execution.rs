#![cfg(feature = "mcp")]

//! End-to-end execution against a real `monty` worker.
//!
//! These tests assert the behaviour the tool description promises: the value contract, the isolation
//! claims, and the failure classification an agent is steered by. They need the worker binary, which
//! is built separately from the workspace (`make code-worker`). Local runs may skip without it;
//! CI sets WORKCELL_REQUIRE_CODE_WORKER so a missing worker fails the suite.

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
#[cfg(feature = "bundled-worker")]
use workcell_mcp_code::bundled_worker_available;
use workcell_mcp_code::{
    CodeConfiguration, CodeInput, CodeToolGroup, DEFAULT_TIMEOUT_MS, DEFAULT_TIMEOUT_SECS,
    MAX_TIMEOUT_SECS, SUBSET_MODULES, UNTYPED_BUILTINS, WITHHELD_BUILTINS, WORKER_FILE_NAME,
    WorkerSource, catalog,
};

/// Resolves the worker the same way the server does, plus the in-repo build location so a developer
/// who ran `make code-worker` needs no extra configuration.
fn worker() -> Option<PathBuf> {
    static WORKER: OnceLock<Option<PathBuf>> = OnceLock::new();
    let worker = WORKER
        .get_or_init(|| {
            if let Some(configured) = std::env::var_os("WORKCELL_MCP_CODE_WORKER") {
                let configured = PathBuf::from(configured);
                assert!(
                    usable_worker(&configured),
                    "configured monty worker is unusable"
                );
                return Some(configured);
            }
            let installed = option_env!("WORKCELL_BUNDLED_MONTY_WORKER")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("../../../target/code-worker/bin")
                        .join(WORKER_FILE_NAME)
                });
            if option_env!("WORKCELL_BUNDLED_MONTY_WORKER").is_some() {
                assert!(
                    usable_worker(&installed),
                    "build-input monty worker is unusable"
                );
            }
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
        .clone();
    assert!(
        worker.is_some() || std::env::var_os("WORKCELL_REQUIRE_CODE_WORKER").is_none(),
        "required monty worker is missing; run make code-worker"
    );
    worker
}

fn usable_worker(path: &Path) -> bool {
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
        .dispatch("python_execution", arguments, CancellationToken::new())
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

#[tokio::test]
async fn preparation_validates_and_binds_without_running_the_snippet() {
    let group = group_or_skip!(true);
    let prepared = group
        .prepare(CodeInput {
            code: "print('prepared once')\n7".to_owned(),
            timeout_sec: None,
        })
        .expect("prepare");
    assert_eq!(prepared.code(), "print('prepared once')\n7");
    assert_eq!(prepared.timeout_ms(), DEFAULT_TIMEOUT_MS);
    assert!(prepared.type_check());

    let execution = group
        .execute_prepared(prepared, CancellationToken::new())
        .await
        .expect("execute")
        .expect("not cancelled");
    assert_eq!(execution.output.result, json!(7));
    assert_eq!(execution.output.stdout, "prepared once\n");
    group.shutdown().await;
}

#[tokio::test]
async fn preparation_rejects_invalid_limits_before_worker_execution() {
    let group = group_or_skip!();
    for requested in [0, MAX_TIMEOUT_SECS + 1] {
        let error = group
            .prepare(CodeInput {
                code: "1".to_owned(),
                timeout_sec: Some(requested),
            })
            .expect_err("invalid timeout");
        assert!(
            error.contains(&format!("timeoutSec is {requested} seconds")),
            "{error}"
        );
        assert!(
            error.contains(&format!("between 1 and {MAX_TIMEOUT_SECS}")),
            "{error}"
        );
        assert!(
            error.contains(&format!("{DEFAULT_TIMEOUT_SECS} second default")),
            "{error}"
        );
    }
    group.shutdown().await;
}

#[cfg(feature = "bundled-worker")]
#[tokio::test]
async fn bundled_worker_executes_without_an_external_path() {
    if !bundled_worker_available() {
        assert!(
            option_env!("WORKCELL_BUNDLED_MONTY_WORKER").is_none()
                && std::env::var_os("WORKCELL_REQUIRE_CODE_WORKER").is_none(),
            "the required worker was not embedded"
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
                timeout_sec: None,
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
                timeout_sec: None,
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
async fn buffered_prints_remain_separate_bounded_utf8_streams() {
    const CAPTURE_BYTES: usize = 256 * 1024;
    const PRINTED_CHARACTERS: usize = CAPTURE_BYTES + 1;
    let group = group_or_skip!(true);
    let output = run(&group, &format!(
        "import sys\nprint('é' * {PRINTED_CHARACTERS})\nprint('x' * {PRINTED_CHARACTERS}, file=sys.stderr)\n42"
    )).await;
    assert_eq!(output["outcome"], "completed", "{output}");
    assert_eq!(output["result"], 42);
    assert_eq!(
        output["stdout"].as_str().expect("stdout").len(),
        CAPTURE_BYTES
    );
    assert_eq!(
        output["stderr"].as_str().expect("stderr").len(),
        CAPTURE_BYTES
    );
    assert!(
        output["stdout"]
            .as_str()
            .expect("stdout")
            .chars()
            .all(|c| c == 'é')
    );
    assert!(
        output["stderr"]
            .as_str()
            .expect("stderr")
            .chars()
            .all(|c| c == 'x')
    );
    assert_eq!(output["stdoutTruncated"], true);
    assert_eq!(output["stderrTruncated"], true);
    assert_eq!(
        output["stdoutUtf8Bytes"],
        PRINTED_CHARACTERS * 'é'.len_utf8() + 1
    );
    assert_eq!(output["stderrUtf8Bytes"], PRINTED_CHARACTERS + 1);
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
        "gc",
        "io",
        "string",
        "struct",
        "operator",
        "statistics",
        "enum",
        "contextlib",
        "hashlib",
        "uuid",
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

#[tokio::test]
async fn unpacking_supports_names_subscripts_and_attributes() {
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

    for (label, code, expected) in [
        (
            "subscript swap",
            "x = [1, 2]\nx[0], x[1] = x[1], x[0]\nx",
            json!([2, 1]),
        ),
        (
            "computed index",
            "x = [1, 2]\ni, j = 0, 1\nx[i], x[j] = x[j], x[i]\nx",
            json!([2, 1]),
        ),
        (
            "dict subscript",
            "d = {'a': 1, 'b': 2}\nd['a'], d['b'] = d['b'], d['a']\nd",
            json!({"a": 2, "b": 1}),
        ),
        (
            "attribute",
            "class P:\n    def __init__(self):\n        self.a = 1\n        self.b = 2\np = P()\np.a, p.b = p.b, p.a\n[p.a, p.b]",
            json!([2, 1]),
        ),
    ] {
        let output = run(&group, code).await;
        assert_eq!(output["outcome"], "completed", "{label}: {output}");
        assert_eq!(output["result"], expected, "{label}: {output}");
    }

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

    let hint = run(&group, "x: list[int] = [1]\nx").await;
    assert_eq!(hint["outcome"], "completed", "{hint}");
    let value = run(&group, "[str(list[int]), str(dict[str, int])]").await;
    assert_eq!(value["outcome"], "completed", "{value}");
    assert_eq!(
        value["result"],
        json!(["list[int]", "dict[str, int]"]),
        "{value}"
    );

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

#[tokio::test]
async fn formatting_works_and_bad_formats_are_ordinary_python_errors() {
    for type_check in [false, true] {
        let group = group_or_skip!(type_check);
        let output = run(&group, "['{}'.format(1), '%s' % 'x', f'{15:04x}', 7 % 3]").await;
        assert_eq!(output["outcome"], "completed", "{output}");
        assert_eq!(output["result"], json!(["1", "x", "000f", 1]), "{output}");
        for code in ["'{'.format()", "'{:invalid}'.format(1)"] {
            let output = run(&group, code).await;
            assert_eq!(output["outcome"], "exception", "{output}");
            assert_eq!(output["exception"]["type"], "ValueError", "{output}");
            assert!(output["diagnostic"].is_null(), "{output}");
        }
        group.shutdown().await;
    }
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
    let output = run(&group, "import socket").await;
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
    for absent in ["socket", "subprocess", "statistics"] {
        assert!(
            !offered.contains(absent),
            "guidance offered {absent}, which the worker cannot import: {offered}"
        );
    }
    for available in SUBSET_MODULES {
        assert!(offered.contains(available), "{available}: {offered}");
    }
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
    let formatted = run(&permissive, "format(15, '04x')").await;
    assert_eq!(formatted["result"], "000f", "{formatted}");
    permissive.shutdown().await;
    assert!(
        absent.is_empty(),
        "described as existing at runtime but the worker does not define them: {absent:?}"
    );

    let checked = group_or_skip!(true);
    for name in UNTYPED_BUILTINS {
        let output = run(&checked, name).await;
        assert_eq!(output["outcome"], "rejected", "{name}: {output}");
        assert!(
            output["diagnostic"]
                .as_str()
                .expect("guidance")
                .contains("missing from its type stubs"),
            "{name}: {output}"
        );
    }
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
async fn clocks_entropy_and_sleep_cannot_bypass_the_os_refusal() {
    for type_check in [false, true] {
        let group = group_or_skip!(type_check);
        for code in [
            "from datetime import date\ndate.today()",
            "from datetime import datetime\ndatetime.now()",
            "import time\ntime.time()",
            "import time\ntime.monotonic()",
            "import time\ntime.perf_counter()",
            "import time\ntime.sleep(0)",
            "import asyncio\nawait asyncio.sleep(0)",
            "import os\nos.urandom(1)",
            "import random\nrandom.random()",
            "import random\nrandom.Random().random()",
        ] {
            let output = run(&group, code).await;
            assert_eq!(output["outcome"], "exception", "{code}: {output}");
            assert_eq!(
                output["exception"]["type"], "RuntimeError",
                "{code}: {output}"
            );
            assert!(
                output["exception"]["message"]
                    .as_str()
                    .expect("message")
                    .contains("not supported in this environment"),
                "{code}: {output}"
            );
        }
        let output = run(
            &group,
            "import time\n[time.process_time(), time.thread_time(), time.timezone, time.tzname]",
        )
        .await;
        assert_eq!(
            output["result"],
            json!([0.0, 0.0, 0, ["UTC", "UTC"]]),
            "{output}"
        );
        group.shutdown().await;
    }
}

#[tokio::test]
async fn unmounted_reads_writes_and_process_modules_stay_unavailable() {
    let directory = tempfile::tempdir().expect("test directory");
    let path = directory.path().join("sentinel.txt");
    std::fs::write(&path, "unchanged").expect("sentinel");
    let path_literal = serde_json::to_string(&path.to_string_lossy()).expect("path literal");
    for type_check in [false, true] {
        let group = group_or_skip!(type_check);
        for operation in ["read_text()", "write_text('changed')", "unlink()", "stat()"] {
            let output = run(
                &group,
                &format!("from pathlib import Path\nPath({path_literal}).{operation}"),
            )
            .await;
            assert_eq!(output["outcome"], "exception", "{operation}: {output}");
            assert_eq!(
                output["exception"]["type"], "PermissionError",
                "{operation}: {output}"
            );
        }
        let output = run(&group, &format!("open({path_literal}).read()")).await;
        if type_check {
            assert_eq!(output["outcome"], "rejected", "{output}");
            assert!(
                output["diagnostic"]
                    .as_str()
                    .expect("guidance")
                    .contains("file tools")
            );
        } else {
            assert_eq!(output["exception"]["type"], "PermissionError", "{output}");
        }
        for module in ["socket", "subprocess", "requests"] {
            let output = run(&group, &format!("import {module}")).await;
            assert_eq!(
                output["outcome"],
                if type_check { "rejected" } else { "exception" },
                "{module}: {output}"
            );
        }
        group.shutdown().await;
    }
    assert_eq!(
        std::fs::read_to_string(&path).expect("sentinel survives"),
        "unchanged"
    );
}

#[tokio::test]
async fn shared_and_cyclic_wire_values_render_without_unbounded_expansion() {
    let group = group_or_skip!();
    let shared = run(&group, "a = [1, 2]\n[a, a]").await;
    assert_eq!(shared["result"], json!([[1, 2], [1, 2]]), "{shared}");
    assert!(shared["resultRepr"].is_null(), "{shared}");
    let cyclic = run(&group, "a = []\na.append(a)\na").await;
    assert_eq!(cyclic["result"], json!(["[...]"]), "{cyclic}");
    assert_eq!(cyclic["resultRepr"], "[[...]]", "{cyclic}");
    for leaf in ["1", "b'x'"] {
        let large = run(
            &group,
            &format!("a = {leaf}\nfor _ in range(20):\n    a = [a, a]\na"),
        )
        .await;
        assert_eq!(large["outcome"], "completed", "{large}");
        assert!(large["result"].is_null());
        assert!(
            large["resultRepr"]
                .as_str()
                .expect("omission notice")
                .contains("rendering budget")
        );
    }
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
    let output = run(&group, "compile('1 + 1', 'snippet', 'eval')").await;
    assert_eq!(output["outcome"], "exception");
    assert_eq!(output["exception"]["type"], "NameError");
    let diagnostic = output["diagnostic"].as_str().expect("guidance");
    assert!(diagnostic.contains("compile"));
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
        json!({ "code": "n = 0\nwhile True:\n    n += 1", "timeoutSec": 1 }),
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
        json!({ "code": "1", "timeoutSec": 0 }),
        json!({ "code": "1", "timeoutSec": MAX_TIMEOUT_SECS + 1 }),
        // In range for timeoutSec, so only the retired millisecond key can be what refuses it.
        json!({ "code": "1", "timeout": MAX_TIMEOUT_SECS }),
        json!({ "code": "1", "unexpected": true }),
    ] {
        let result = group
            .dispatch(
                "python_execution",
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
        json!({ "code": "blob = []\nwhile True:\n    blob.append('x' * 1_000_000)", "timeoutSec": 20 }),
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
        json!({ "code": "blob = []\nwhile True:\n    blob.append('x' * 1_000_000)", "timeoutSec": 20 }),
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

    let builtin = run(&group, "compile('1 + 1', 'snippet', 'eval')").await;
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

#[tokio::test]
async fn v1_computation_works_without_adding_host_capabilities() {
    for type_check in [false, true] {
        let group = group_or_skip!(type_check);
        for (code, expected) in [
            (
                "import base64, binascii\n[base64.b64encode(b'hello').decode(), binascii.hexlify(b'hi').decode()]",
                json!(["aGVsbG8=", "6869"]),
            ),
            (
                "import copy\na = [[1]]\nb = copy.deepcopy(a)\nb[0].append(2)\n[a, b]",
                json!([[[1]], [[1, 2]]]),
            ),
            (
                "import functools\nfunctools.reduce(lambda a, b: a + b, [1, 2, 3])",
                json!(6),
            ),
            (
                "import random\nrandom.seed(42)\n[random.randint(1, 10), random.Random(42).randint(1, 10)]",
                json!([2, 2]),
            ),
            (
                "scope = {'x': 2}\nexec('y = x + 3', scope)\n[eval('y * 2', scope), 'scope' in locals()]",
                json!([10, true]),
            ),
            (
                "import sys\n[sys.version_info.major, sys.version_info.minor, sys.maxsize > 0]",
                json!([3, 14, true]),
            ),
            (
                "from dataclasses import dataclass\n@dataclass\nclass A:\n    x: int\n    y: list[int]\nA(1, [2])",
                json!({"x": 1, "y": [2]}),
            ),
            ("str(object)", json!("<class 'object'>")),
        ] {
            let output = run(&group, code).await;
            assert_eq!(output["outcome"], "completed", "{code}: {output}");
            assert_eq!(output["result"], expected, "{code}: {output}");
        }
        let output = run(&group, "object()").await;
        assert_eq!(output["outcome"], "exception", "{output}");
        assert_eq!(output["exception"]["type"], "TypeError", "{output}");
        group.shutdown().await;
    }
}

#[tokio::test]
async fn dynamic_code_inherits_os_refusals_and_execution_limits() {
    let group = group_or_skip!(true);
    for (code, exception) in [
        ("eval(\"open('/etc/passwd').read()\")", "PermissionError"),
        (
            "exec(\"from pathlib import Path; Path('/etc/passwd').read_text()\")",
            "PermissionError",
        ),
        ("exec('import socket')", "ModuleNotFoundError"),
        ("exec('import subprocess')", "ModuleNotFoundError"),
        ("exec('import time; time.time()')", "RuntimeError"),
        ("exec('import random; random.random()')", "RuntimeError"),
    ] {
        let output = run(&group, code).await;
        assert_eq!(output["outcome"], "exception", "{code}: {output}");
        assert_eq!(output["exception"]["type"], exception, "{code}: {output}");
    }
    let output = run(&group, "scope = {}\nexec('import os; result = [os.getenv(\"PATH\"), len(os.environ)]', scope)\nscope['result']").await;
    assert_eq!(output["result"], json!([null, 0]), "{output}");
    let output = run_with(
        &group,
        json!({"code": "exec('while True: pass')", "timeoutSec": 1}),
    )
    .await;
    assert_eq!(output["outcome"], "limited", "{output}");
    assert_eq!(output["timedOut"], true, "{output}");
    assert_eq!(run(&group, "6 * 7").await["result"], 42);
    group.shutdown().await;
}

#[tokio::test]
async fn resuming_os_calls_does_not_reset_the_feed_budget() {
    let group = group_or_skip!();
    let output = run_with(&group, json!({
        "code": "import os\nwhile True:\n    for _ in range(1_000_000):\n        pass\n    os.getenv('PATH')",
        "timeoutSec": 1,
    })).await;
    assert_eq!(output["outcome"], "limited", "{output}");
    assert_eq!(output["timedOut"], true, "{output}");
    assert_eq!(output["suspensionLimitExceeded"], false, "{output}");
    group.shutdown().await;
}

#[tokio::test]
async fn repeated_denied_os_calls_are_stopped_by_the_suspension_cap() {
    let group = group_or_skip!();
    let output = run(&group, "import os\nwhile True:\n    os.getenv('PATH')").await;
    assert_eq!(output["outcome"], "limited", "{output}");
    assert_eq!(output["suspensionLimitExceeded"], true, "{output}");
    assert_eq!(output["timedOut"], false, "{output}");
    assert_eq!(run(&group, "6 * 7").await["result"], 42);
    group.shutdown().await;
}

#[tokio::test]
async fn cancellation_discards_an_inflight_call_and_keeps_the_pool_usable() {
    let group = group_or_skip!();
    let cancellation = CancellationToken::new();
    let running = group.execute(
        CodeInput {
            code: "while True: pass".to_owned(),
            timeout_sec: Some(MAX_TIMEOUT_SECS),
        },
        cancellation.clone(),
    );
    tokio::pin!(running);
    tokio::select! {
        biased;
        _ = &mut running => panic!("infinite snippet completed before cancellation"),
        () = tokio::task::yield_now() => {}
    }
    cancellation.cancel();
    assert!(running.await.expect("valid input").is_none());
    assert_eq!(run(&group, "6 * 7").await["result"], 42);
    group.shutdown().await;
}
