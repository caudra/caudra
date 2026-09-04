use std::sync::Arc;

use rmcp::model::{JsonObject, MetaObject, Tool, ToolAnnotations};
use serde_json::{Map, Value, json};
use workcell_tool_contract::{ToolAnnotations as NeutralAnnotations, ToolSpec};

const PRESENTATION_KEY: &str = "ai.workcell/presentation-profile";
const DRAFT_07: &str = "http://json-schema.org/draft-07/schema#";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

const DOWNLOAD_DESCRIPTION: &str = r#"Prepare a download so the harness can retrieve a file's raw bytes over HTTP.

- This tool transfers no bytes. It authorizes the path and returns the URL the harness must fetch.
- The harness issues `GET <url>` against the same origin as its MCP endpoint, reusing the credentials it already sends, then writes the response body wherever the user asked for it.
- `url` is relative on purpose: only the harness knows the externally reachable origin, which may differ from this process's bind address behind a proxy or port mapping.
- Use this for binary or large files. `file_read` is still the right tool for reading text into context.
- Directories are rejected. Use `file_read` to list one.
- Reporting the file as delivered before the harness performs the fetch is incorrect."#;

const UPLOAD_DESCRIPTION: &str = r#"Prepare an upload so the harness can send raw bytes into this execution environment over HTTP.

- This tool transfers no bytes. It authorizes the destination and returns the URL the harness must post to.
- The harness issues `POST <url>` against the same origin as its MCP endpoint with `Content-Type: application/octet-stream`, the raw bytes as the request body, and the credentials it already sends.
- `url` is relative on purpose: only the harness knows the externally reachable origin, which may differ from this process's bind address behind a proxy or port mapping.
- Missing parent directories are created. An existing file is replaced only once the transfer completes, so an interrupted upload never leaves a truncated file behind.
- Bodies larger than `maxBytes` are rejected.
- Reporting the file as written before the harness performs the post is incorrect."#;

#[must_use]
pub fn catalog(allow_write: bool) -> Vec<Tool> {
    specs(allow_write).iter().map(to_mcp_tool).collect()
}

/// `allow_write` must be the write authority of the group that will serve these tools. A prepared
/// upload the endpoint would refuse costs a model turn to discover, so a read-only deployment omits
/// `file_upload` entirely rather than advertising it.
#[must_use]
pub fn specs(allow_write: bool) -> Vec<ToolSpec> {
    let mut specs = vec![
        ToolSpec::new(
            "file_download",
            Some("Prepare file download"),
            DOWNLOAD_DESCRIPTION,
            download_schema(),
            NeutralAnnotations {
                read_only_hint: Some(true),
                destructive_hint: Some(false),
                idempotent_hint: Some(true),
                open_world_hint: Some(false),
            },
            "transfer.download.v1",
            "transfer.download.v1",
        )
        .with_output_schema(download_output_schema()),
    ];
    if allow_write {
        specs.push(
            ToolSpec::new(
                "file_upload",
                Some("Prepare file upload"),
                UPLOAD_DESCRIPTION,
                upload_schema(),
                NeutralAnnotations {
                    read_only_hint: Some(false),
                    destructive_hint: Some(true),
                    idempotent_hint: Some(true),
                    open_world_hint: Some(false),
                },
                "transfer.upload.v1",
                "transfer.upload.v1",
            )
            .with_output_schema(upload_output_schema()),
        );
    }
    specs
}

// `workcell-tool-contract` must never link a protocol SDK, so the neutral spec cannot project
// itself. This mirrors the projection in `crates/mcp-files/src/catalog.rs`; the duplication is the
// cost of keeping `ToolSpec` protocol-free.
fn to_mcp_tool(spec: &ToolSpec) -> Tool {
    let mut meta = JsonObject::new();
    meta.insert(
        PRESENTATION_KEY.to_owned(),
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
    let tool = match &spec.output_schema {
        Some(output_schema) => tool.with_raw_output_schema(Arc::new(output_schema.clone())),
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

fn download_schema() -> Map<String, Value> {
    schema(json!({
        "$schema": DRAFT_07,
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "minLength": 1,
                "description": "Root-relative or absolute path of the file to download."
            }
        },
        "required": ["path"],
        "additionalProperties": false
    }))
}

fn upload_schema() -> Map<String, Value> {
    schema(json!({
        "$schema": DRAFT_07,
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "minLength": 1,
                "description": "Root-relative or absolute destination path for the uploaded file."
            }
        },
        "required": ["path"],
        "additionalProperties": false
    }))
}

fn download_output_schema() -> Map<String, Value> {
    schema(json!({
        "$schema": DRAFT_07,
        "type": "object",
        "properties": {
            "method": { "type": "string", "const": "GET" },
            "url": {
                "type": "string",
                "description": "Relative URL to resolve against the origin of the MCP endpoint."
            },
            "path": { "type": "string", "description": "Root-relative path of the file." },
            "name": { "type": "string", "description": "File name." },
            "bytes": {
                "type": "integer",
                "minimum": 0,
                "maximum": MAX_SAFE_INTEGER,
                "description": "Size of the file in bytes."
            }
        },
        "required": ["method", "url", "path", "name", "bytes"],
        "additionalProperties": false
    }))
}

fn upload_output_schema() -> Map<String, Value> {
    schema(json!({
        "$schema": DRAFT_07,
        "type": "object",
        "properties": {
            "method": { "type": "string", "const": "POST" },
            "url": {
                "type": "string",
                "description": "Relative URL to resolve against the origin of the MCP endpoint."
            },
            "path": { "type": "string", "description": "Root-relative destination path." },
            "name": { "type": "string", "description": "Destination file name." },
            "contentType": { "type": "string", "const": "application/octet-stream" },
            "maxBytes": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_SAFE_INTEGER,
                "description": "Largest body the endpoint will accept."
            }
        },
        "required": ["method", "url", "path", "name", "contentType", "maxBytes"],
        "additionalProperties": false
    }))
}

fn schema(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}
