use caudra_agent::tools::{PYTHON_EXECUTION_TOOL_NAME, ToolRegistry, cli_tool_ctx};
use caudra_workcell::WorkcellHost;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;
use workcell::code::bundled_worker_available;

const MISSING_REQUIRED_WORKER: &str = "the required worker was not embedded";
const CODE: &str = "sum([1, 2, 3, 4])";
const EXPECTED_RESULT: i64 = 10;
const EXPECTED_MODEL_OUTPUT: &str = "result: 10";
const COMPLETED: &str = "completed";

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
