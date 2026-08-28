//! MCP tool discovery metadata for the shell executor.
//!
//! The JSON schema is an admission contract, not a security boundary; dispatch validates again.
//! An MCP client may use annotations and presentation metadata for UX, but the server never trusts
//! clients to enforce either the unsafe-execution warning or argument constraints.

use crate::types::{DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS};
use rmcp::model::{JsonObject, MetaObject, Tool, ToolAnnotations};
use serde_json::{Value, json};
use std::sync::Arc;

/// Stable extension key consumed by Workcell renderers. Preserve this namespace across versions.
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
pub fn catalog() -> Vec<Tool> {
    // Reject unknown fields to keep client mistakes from silently changing execution semantics.
    let schema = json!({"type":"object","additionalProperties":false,"properties":{"command":{"type":"string","minLength":1,"description":"Bash command to execute on the MCP server host."},"timeout":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_MS,"default":DEFAULT_TIMEOUT_MS,"description":"Optional timeout in milliseconds. Defaults to 120000 and is capped at 600000."},"workdir":{"type":"string","minLength":1,"description":"Optional configured-root-relative or absolute initial working directory inside the configured root."}},"required":["command"],"$schema":"http://json-schema.org/draft-07/schema#"});
    let mut meta = JsonObject::new();
    meta.insert(
        PRESENTATION_KEY.into(),
        Value::String("shell.result.v1".into()),
    );
    // Destructive/idempotent annotations are presentation hints only. The explicit description is
    // the durable warning that arbitrary commands inherit files, network, and environment access.
    vec![
        Tool::new(
            "shell",
            DESCRIPTION,
            Arc::new(schema.as_object().expect("schema object").clone()),
        )
        .with_title("Execute shell command")
        .with_annotations(ToolAnnotations::from_raw(
            None,
            Some(false),
            Some(true),
            Some(false),
            Some(true),
        ))
        .with_meta(MetaObject(meta)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uses_standard_presentation_key() {
        let tools = catalog();
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
    }
}
