//! End-to-end execution against a real `monty` worker.
//!
//! These tests assert the behaviour the tool description promises: the value contract, the isolation
//! claims, and the failure classification an agent is steered by. They need the worker binary, which
//! is built separately from the workspace (`make code-worker`), so they skip with an explicit
//! message when it is absent rather than silently passing.

use std::{path::PathBuf, sync::OnceLock};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use workcell_mcp_code::{CodeConfiguration, CodeToolGroup, WORKER_FILE_NAME, resolve_worker};

/// Resolves the worker the same way the server does, plus the in-repo build location so a developer
/// who ran `make code-worker` needs no extra configuration.
fn worker() -> Option<PathBuf> {
    static WORKER: OnceLock<Option<PathBuf>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            if let Some(configured) = std::env::var_os("WORKCELL_MCP_CODE_WORKER") {
                return resolve_worker(Some(&PathBuf::from(configured)));
            }
            let installed = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/code-worker/bin")
                .join(WORKER_FILE_NAME);
            resolve_worker(Some(&installed)).or_else(|| resolve_worker(None))
        })
        .clone()
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
        CodeToolGroup::new(CodeConfiguration { worker: Some(&worker), type_check: $type_check })
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
        worker: Some(&worker),
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
