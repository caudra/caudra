//! Executor, a recognised server whose own tool runs code: Caudra trusts its read-only marks.

use std::path::PathBuf;

use caudra_agent::AgentMode;
use caudra_agent::mcp::code::output::McpCodeCallStatus;
use caudra_agent::permissions::PermissionMode;

use crate::support::{
    EXECUTOR, EXECUTOR_IMAGE, PLAN_FILE, PLAN_REFUSAL, Servers, completed, stdio,
};

const ANSWER: &str = "42";
const EXECUTE: &str = "execute";
const SKILLS: &str = "skills";

fn executor(mode: AgentMode) -> Servers {
    Servers::connect(
        vec![stdio(EXECUTOR, EXECUTOR_IMAGE)],
        mode,
        PermissionMode::Yolo,
    )
}

#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn execute_runs_typescript_in_the_executor_sandbox() {
    let servers = executor(AgentMode::Build);
    let output = servers.run("await executor.execute(code='return 6 * 7')");
    assert!(completed(&output).contains(ANSWER), "{output:?}");
    assert_eq!(output.calls[0].tool, EXECUTE);
    assert_eq!(output.calls[0].status, McpCodeCallStatus::Success);
}

#[test]
#[ignore = "needs Docker: make test-mcp-servers"]
fn plan_mode_reads_skills_and_refuses_execute() {
    let servers = executor(AgentMode::Plan(PathBuf::from(PLAN_FILE)));
    let output = servers.run(
        "await executor.skills()\n\
         try:\n    await executor.execute(code='return 1')\n    refused = ''\n\
         except PermissionError as error:\n    refused = str(error)\n\
         refused",
    );
    assert!(completed(&output).contains(PLAN_REFUSAL), "{output:?}");
    let statuses: Vec<_> = output
        .calls
        .iter()
        .map(|call| (call.tool.as_str(), call.status))
        .collect();
    assert_eq!(
        statuses,
        [
            (SKILLS, McpCodeCallStatus::Success),
            (EXECUTE, McpCodeCallStatus::Refused)
        ]
    );
}
