use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";

#[derive(Serialize)]
pub struct JsonRpcRequest<'a> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl<'a> JsonRpcRequest<'a> {
    pub fn new(id: u64, method: &'a str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

#[derive(Serialize)]
pub struct JsonRpcNotification<'a> {
    pub jsonrpc: &'static str,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl<'a> JsonRpcNotification<'a> {
    pub fn new(method: &'a str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            method,
            params,
        }
    }
}

#[derive(Deserialize)]
pub struct JsonRpcResponse {
    pub id: Option<u64>,
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

pub fn initialize_params() -> Value {
    serde_json::json!({
        "protocolVersion": LATEST_PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {
            "name": "caudra",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

#[derive(Deserialize)]
pub struct ToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Deserialize)]
pub struct ToolsListResult {
    pub tools: Vec<ToolInfo>,
}

/// One content block of a `tools/call` result. Every field is optional so a
/// block of a kind this client has never heard of still parses, and still
/// contributes its `text` the way it always did.
#[derive(Deserialize, Default)]
pub struct CallToolContent {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub resource: Option<EmbeddedResource>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct EmbeddedResource {
    #[serde(default)]
    pub uri: String,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub blob: Option<String>,
}

#[derive(Deserialize)]
pub struct CallToolResult {
    pub content: Vec<CallToolContent>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
    #[serde(default, rename = "structuredContent")]
    pub structured_content: Option<Value>,
}

impl CallToolResult {
    pub fn joined_text(&self) -> String {
        self.content
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Deserialize, Clone)]
pub struct PromptArgument {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

#[derive(Deserialize)]
pub struct PromptInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub arguments: Vec<PromptArgument>,
}

#[derive(Deserialize)]
pub struct PromptsListResult {
    pub prompts: Vec<PromptInfo>,
}

#[derive(Deserialize)]
pub struct PromptMessageContent {
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PromptRole {
    #[default]
    User,
    Assistant,
}

#[derive(Deserialize)]
pub struct PromptMessage {
    #[serde(default)]
    pub role: PromptRole,
    pub content: PromptMessageContent,
}

#[derive(Deserialize)]
pub struct GetPromptResult {
    pub messages: Vec<PromptMessage>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_skips_none_params() {
        let with =
            serde_json::to_value(JsonRpcRequest::new(1, "init", Some(json!({"k": 1})))).unwrap();
        assert_eq!(with["params"]["k"], 1);

        let without = serde_json::to_value(JsonRpcRequest::new(2, "tools/list", None)).unwrap();
        assert!(without.get("params").is_none());
    }

    #[test]
    fn tool_info_honours_input_schema_rename() {
        let raw = json!({"tools": [{"name": "read_file", "description": "Read a file", "inputSchema": {"type": "object"}}]});
        let result: ToolsListResult = serde_json::from_value(raw).unwrap();
        assert_eq!(result.tools[0].name, "read_file");
        assert_eq!(result.tools[0].input_schema["type"], "object");
    }

    #[test]
    fn call_tool_result_honours_is_error_rename() {
        let raw = json!({"content": [{"text": "hello"}], "isError": true});
        let result: CallToolResult = serde_json::from_value(raw).unwrap();
        assert!(result.is_error);
        assert_eq!(result.joined_text(), "hello");
    }

    #[test]
    fn call_tool_result_parses_every_block_kind_and_joins_only_text() {
        let raw = json!({
            "content": [
                {"type": "text", "text": "first"},
                {"type": "image", "data": "aGk=", "mimeType": "image/png"},
                {"type": "audio", "data": "aGk=", "mimeType": "audio/wav"},
                {"type": "resource", "resource": {"uri": "file:///a", "text": "body"}},
                {"type": "resource_link", "uri": "file:///b", "name": "b"},
                {"text": "untyped"}
            ],
            "structuredContent": {"k": 1}
        });
        let result: CallToolResult = serde_json::from_value(raw).unwrap();
        assert_eq!(result.content.len(), 6);
        assert_eq!(result.content[1].mime_type.as_deref(), Some("image/png"));
        assert_eq!(
            result.content[3].resource.as_ref().unwrap().text.as_deref(),
            Some("body")
        );
        assert_eq!(result.content[4].name.as_deref(), Some("b"));
        assert_eq!(result.structured_content, Some(json!({"k": 1})));
        assert_eq!(result.joined_text(), "first\n\n\n\n\nuntyped");
    }

    #[test]
    fn prompts_list_result_deserializes() {
        let raw = json!({
            "prompts": [{
                "name": "code-review",
                "description": "Review code changes",
                "arguments": [
                    {"name": "diff", "description": "The diff to review", "required": true},
                    {"name": "style", "required": false}
                ]
            }]
        });
        let result: PromptsListResult = serde_json::from_value(raw).unwrap();
        assert_eq!(result.prompts.len(), 1);
        assert_eq!(result.prompts[0].name, "code-review");
        assert_eq!(
            result.prompts[0].description.as_deref(),
            Some("Review code changes")
        );
        assert_eq!(result.prompts[0].arguments.len(), 2);
        assert!(result.prompts[0].arguments[0].required);
        assert!(!result.prompts[0].arguments[1].required);
    }

    #[test]
    fn prompts_list_result_defaults() {
        let raw = json!({"prompts": [{"name": "simple"}]});
        let result: PromptsListResult = serde_json::from_value(raw).unwrap();
        assert!(result.prompts[0].description.is_none());
        assert!(result.prompts[0].arguments.is_empty());
    }

    #[test]
    fn get_prompt_result_deserializes() {
        let raw = json!({
            "messages": [
                {"role": "user", "content": {"text": "Review this code"}},
                {"role": "assistant", "content": {"text": "I'll review it"}}
            ]
        });
        let result: GetPromptResult = serde_json::from_value(raw).unwrap();
        assert_eq!(result.messages.len(), 2);
        assert_eq!(result.messages[0].role, PromptRole::User);
        assert_eq!(
            result.messages[0].content.text.as_deref(),
            Some("Review this code")
        );
        assert_eq!(result.messages[1].role, PromptRole::Assistant);
    }
}
