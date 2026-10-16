use caudra_agent::CancelToken;
use caudra_agent::code::output::CodeOutcome;
use caudra_agent::code::runner::{
    self as script_runner, ReplLease, ReplSlot, ScriptCall, ScriptHost, ScriptInvocation,
    ScriptName, ScriptRun, ScriptRunner, ScriptStatus,
};
use caudra_agent::tools::native::code::CodeTool;
use caudra_agent::tools::{CODE_TOOL_NAME, Tool, ToolRegistry, cli_tool_ctx};
use caudra_agent::types::ToolOutput;
use caudra_workcell::WorkcellHost;
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
use workcell::code::bundled_worker_available;

const MISSING_REQUIRED_WORKER: &str = "the required worker was not embedded";
const CODE: &str = "print('summing')\nsum([1, 2, 3, 4])";
const EXPECTED_RESULT: &str = "10";
const EXPECTED_STDOUT: &str = "summing\n";
const NAMESPACE: &str = "srv";
const METHOD: &str = "echo";
const TEXT_ARG: &str = "text";
const ECHOED: &str = "hi";
const STUBS: &str = "from typing import Any\nclass _S:\n    async def echo(self, *, text: str) -> Any: ...\nsrv: _S\n";
const SNIPPET: &str = "(await srv.echo(text='hi'))";

/// Plain computation runs through the `code` tool on the bundled worker's hosted pool, and
/// Workcell's own isolated `code` is not registered in its place.
#[test]
fn production_host_computes_through_the_code_tool_with_the_embedded_worker() {
    assert!(bundled_worker_available(), "{MISSING_REQUIRED_WORKER}");
    let root = TempDir::new().expect("tempdir");
    let host = WorkcellHost::new_production(root.path(), None).expect("Workcell host");
    assert!(host.warnings().is_empty(), "{:?}", host.warnings());
    let registry = ToolRegistry::new();
    host.register(&registry).expect("Workcell registration");
    assert!(registry.get(CODE_TOOL_NAME).is_none());
    script_runner::install(Arc::new(host.script_runner()));
    drop(host);

    let ctx = cli_tool_ctx(root.path());
    let invocation = CodeTool
        .parse(&json!({"code": CODE}))
        .expect("valid code input");
    let result = smol::block_on(invocation.execute(&ctx));

    assert!(!result.is_error, "{:?}", result.model_output);
    let Ok(ToolOutput::Code(output)) = result.output else {
        panic!("a code card, got {:?}", result.output);
    };
    assert_eq!(output.outcome, CodeOutcome::Completed);
    assert_eq!(output.result.as_deref(), Some(EXPECTED_RESULT));
    assert_eq!(output.stdout, EXPECTED_STDOUT);
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

/// `code` snippets run on the same bundled worker, through the host-call drive loop.
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
