use caudra_agent::CancelToken;
use caudra_agent::mcp::code::runner::{
    ReplLease, ReplSlot, ScriptCall, ScriptHost, ScriptInvocation, ScriptName, ScriptRun,
    ScriptRunner, ScriptStatus,
};
use caudra_agent::tools::{PYTHON_EXECUTION_TOOL_NAME, ToolRegistry, cli_tool_ctx};
use caudra_workcell::WorkcellHost;
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
use workcell::code::bundled_worker_available;

const MISSING_REQUIRED_WORKER: &str = "the required worker was not embedded";
const CODE: &str = "sum([1, 2, 3, 4])";
const EXPECTED_RESULT: i64 = 10;
const EXPECTED_MODEL_OUTPUT: &str = "result: 10";
const COMPLETED: &str = "completed";
const NAMESPACE: &str = "srv";
const METHOD: &str = "echo";
const TEXT_ARG: &str = "text";
const ECHOED: &str = "hi";
const STUBS: &str = "from typing import Any\nclass _S:\n    async def echo(self, *, text: str) -> Any: ...\nsrv: _S\n";
const SNIPPET: &str = "(await srv.echo(text='hi'))";

#[test]
fn production_host_executes_code_with_the_embedded_worker() {
    assert!(bundled_worker_available(), "{MISSING_REQUIRED_WORKER}");
    let root = TempDir::new().expect("tempdir");
    let host = WorkcellHost::new_production(root.path(), None).expect("Workcell host");
    assert!(host.warnings().is_empty(), "{:?}", host.warnings());
    let registry = Arc::new(ToolRegistry::new());
    host.register(&registry).expect("Workcell registration");
    drop(host);

    let mut ctx = cli_tool_ctx(root.path());
    ctx.registry = registry;
    let invocation = ctx
        .registry
        .get(PYTHON_EXECUTION_TOOL_NAME)
        .expect("registered code execution")
        .tool
        .parse(&json!({"code": CODE}))
        .expect("valid code input");

    smol::block_on(invocation.preflight(&ctx)).expect("code preflight");
    let result = smol::block_on(invocation.execute(&ctx));

    assert!(!result.is_error, "{:?}", result.model_output);
    assert_eq!(result.model_output.as_deref(), Some(EXPECTED_MODEL_OUTPUT));
    let output = result.output.expect("successful tool output");
    let state = output.state().expect("structured code output");
    assert_eq!(state["outcome"], COMPLETED);
    assert_eq!(state["result"], EXPECTED_RESULT);
}

struct Echo;

impl ScriptHost for Echo {
    fn lookup(&self, namespace: Option<&str>, name: &str) -> ScriptName {
        match (namespace, name) {
            (None, NAMESPACE) => ScriptName::Namespace,
            (Some(NAMESPACE), METHOD) => ScriptName::Function,
            _ => ScriptName::Undefined(None),
        }
    }

    /// A server call is awaited in the snippet, so it answers as a future.
    fn call(&self, call: ScriptCall) -> ScriptInvocation {
        let text = call.kwargs.get(TEXT_ARG).cloned().unwrap_or(Value::Null);
        ScriptInvocation::Deferred(Box::pin(async move { Ok(text) }))
    }

    fn stubs(&self) -> String {
        STUBS.to_owned()
    }
}

/// `mcp` snippets run on the same bundled worker, through the host-call drive loop.
#[test]
fn production_host_runs_mcp_snippets_with_the_embedded_worker() {
    assert!(bundled_worker_available(), "{MISSING_REQUIRED_WORKER}");
    let root = TempDir::new().expect("tempdir");
    let host = WorkcellHost::new_production(root.path(), None).expect("Workcell host");
    let repl = ReplSlot::shared(false);
    let outcome = smol::block_on(async {
        host.script_runner()
            .run(ScriptRun {
                code: SNIPPET.to_owned(),
                host: Arc::new(Echo),
                state: ReplLease::acquire(&repl).await,
                cancel: CancelToken::none(),
            })
            .await
    });

    let output = outcome.output.expect("the snippet ran");
    assert_eq!(output.status, ScriptStatus::Completed, "{output:?}");
    assert_eq!(output.result, json!(ECHOED));
    assert!(smol::block_on(ReplLease::acquire(&repl)).take().is_some());
}
