//! Protocol-neutral contract metadata and optional MCP conversion for the shell executor.
//!
//! The JSON schema is an admission contract, not a security boundary; dispatch validates again.
//! An MCP client may use annotations and presentation metadata for UX, but the server never trusts
//! clients to enforce either the unsafe-execution warning or argument constraints.

use crate::types::{DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS};
#[cfg(feature = "mcp")]
use rmcp::model::{JsonObject, MetaObject, Tool, ToolAnnotations};
#[cfg(feature = "mcp")]
use serde_json::Value;
use serde_json::json;
#[cfg(feature = "mcp")]
use std::sync::Arc;
use workcell_tool_contract::{ToolAnnotations as NeutralAnnotations, ToolSpec};

/// Stable extension key consumed by Workcell renderers. Preserve this namespace across versions.
#[cfg(feature = "mcp")]
pub(crate) const PRESENTATION_KEY: &str = "ai.workcell/presentation-profile";

const DESCRIPTION: &str = r#"Execute a Bash command on the MCP server host.

Usage notes:
- The command parameter is required.
- Commands must be valid MCP JSON strings, are bounded to 65536 UTF-8 bytes, and are authorized by immutable operator policy before execution. Malformed JSON or non-UTF-8 request payloads are rejected by the MCP transport before tool dispatch.
- timeout is optional, measured in milliseconds, defaults to 120000, and is capped at 600000.
- Use workdir instead of embedding cd commands. It must resolve inside the configured root and defaults to ".".
- Only the initial working directory is root-confined. Execution is unsafe and unsandboxed: commands can mutate host files, access the network, and read inherited environment variables.
- Prefer the dedicated file tools when they are available and fit the operation, and prefer the code execution tool for pure computation such as arithmetic, statistics, string processing, and JSON reshaping, because it runs isolated from the host.
- Always quote file paths that contain spaces.
- Non-zero exits are completed results with an exit code so the caller can inspect and continue.
- Output is streamed through MCP progress notifications; the final result contains bounded tails and completion accounting.
- Background execution is unsupported; descendants that retain output pipes are terminated."#;

#[must_use]
#[cfg(feature = "mcp")]
pub fn catalog() -> Vec<Tool> {
    specs().iter().map(to_mcp_tool).collect()
}

#[must_use]
pub fn specs() -> Vec<ToolSpec> {
    // Reject unknown fields to keep client mistakes from silently changing execution semantics.
    let schema = json!({"type":"object","additionalProperties":false,"properties":{"command":{"type":"string","minLength":1,"description":"Bash command to execute on the MCP server host."},"timeout":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_MS,"default":DEFAULT_TIMEOUT_MS,"description":"Optional timeout in milliseconds. Defaults to 120000 and is capped at 600000."},"workdir":{"type":"string","minLength":1,"description":"Optional configured-root-relative or absolute initial working directory inside the configured root."}},"required":["command"],"$schema":"http://json-schema.org/draft-07/schema#"});
    // Destructive/idempotent annotations are presentation hints only. The explicit description is
    // the durable warning that arbitrary commands inherit files, network, and environment access.
    vec![ToolSpec::new(
        "shell",
        Some("Execute shell command"),
        DESCRIPTION,
        schema.as_object().expect("schema object").clone(),
        NeutralAnnotations {
            read_only_hint: Some(false),
            destructive_hint: Some(true),
            idempotent_hint: Some(false),
            open_world_hint: Some(true),
        },
        "shell.result.v1",
        "shell.execution.v1",
    )]
}

#[cfg(feature = "mcp")]
fn to_mcp_tool(spec: &ToolSpec) -> Tool {
    let mut meta = JsonObject::new();
    meta.insert(
        PRESENTATION_KEY.into(),
        Value::String(spec.presentation.to_owned()),
    );
    let tool = Tool::new(
        spec.name,
        spec.description.clone(),
        Arc::new(spec.input_schema.clone()),
    );
    let tool = match spec.title {
        Some(title) => tool.with_title(title),
        None => tool,
    };
    tool.with_annotations(ToolAnnotations::from_raw(
        None,
        spec.annotations.read_only_hint,
        spec.annotations.destructive_hint,
        spec.annotations.idempotent_hint,
        spec.annotations.open_world_hint,
    ))
    .with_meta(MetaObject(meta))
}

#[cfg(all(test, feature = "mcp"))]
mod tests {
    use super::*;
    #[test]
    fn uses_standard_presentation_key() {
        let tools = catalog();
        let specs = specs();
        assert_eq!(tools[0].name, "shell");
        let description = tools[0].description.as_deref().expect("tool description");
        assert!(description.contains("Only the initial working directory"));
        assert!(description.contains("unsafe and unsandboxed"));
        // Cross-tool steering only works when both descriptions agree on the preference.
        assert!(description.contains("prefer the code execution tool for pure computation"));
        assert_eq!(
            tools[0].input_schema["properties"]["timeout"]["description"],
            "Optional timeout in milliseconds. Defaults to 120000 and is capped at 600000."
        );
        assert_eq!(
            tools[0].meta.as_ref().unwrap().0[PRESENTATION_KEY],
            "shell.result.v1"
        );
        assert_eq!(specs[0].name, tools[0].name);
        assert_eq!(specs[0].input_schema, *tools[0].input_schema);
        assert_eq!(specs[0].contract_id, "shell.execution.v1");
    }
}
