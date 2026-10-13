use std::ops::RangeInclusive;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::kind::McpServerKind;

/// The modern, stateless revision: every request carries its own `_meta`.
pub const LATEST_PROTOCOL_VERSION: &str = "2026-07-28";
/// What Caudra offers in `initialize` to a server of the handshake era.
pub const LEGACY_PROTOCOL_VERSION: &str = "2025-11-25";
pub const INITIALIZE_METHOD: &str = "initialize";
pub const INITIALIZED_NOTIFICATION: &str = "notifications/initialized";
pub const DISCOVER_METHOD: &str = "server/discover";
pub const LISTEN_METHOD: &str = "subscriptions/listen";
pub const TOOLS_LIST_METHOD: &str = "tools/list";
pub const TOOLS_CALL_METHOD: &str = "tools/call";
pub const PROMPTS_LIST_METHOD: &str = "prompts/list";
pub const PROMPTS_GET_METHOD: &str = "prompts/get";
pub const RESOURCES_LIST_METHOD: &str = "resources/list";
pub const RESOURCES_TEMPLATES_LIST_METHOD: &str = "resources/templates/list";
pub const RESOURCES_READ_METHOD: &str = "resources/read";
pub const COMPLETION_COMPLETE_METHOD: &str = "completion/complete";
pub const ELICITATION_CREATE_METHOD: &str = "elicitation/create";
pub const CANCELLED_NOTIFICATION: &str = "notifications/cancelled";
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
/// What servers before 2026-07-28 answered for a resource that is not there.
pub const LEGACY_RESOURCE_NOT_FOUND: i64 = -32002;
pub const HEADER_MISMATCH: i64 = -32020;
pub const MISSING_REQUIRED_CLIENT_CAPABILITY: i64 = -32021;
pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;
pub const URL_ELICITATION_REQUIRED: i64 = -32042;
pub const META_KEY: &str = "_meta";
pub const PROTOCOL_VERSION_META: &str = "io.modelcontextprotocol/protocolVersion";
pub const CLIENT_CAPABILITIES_META: &str = "io.modelcontextprotocol/clientCapabilities";
pub const CLIENT_INFO_META: &str = "io.modelcontextprotocol/clientInfo";
pub const SERVER_INFO_META: &str = "io.modelcontextprotocol/serverInfo";
pub const SUBSCRIPTION_ID_META: &str = "io.modelcontextprotocol/subscriptionId";
pub const TOOLS_LIST_CHANGED_FILTER: &str = "toolsListChanged";
pub const PROMPTS_LIST_CHANGED_FILTER: &str = "promptsListChanged";
pub const RESOURCES_LIST_CHANGED_FILTER: &str = "resourcesListChanged";
const CLIENT_NAME: &str = "caudra";
const CLIENT_TITLE: &str = "Caudra";
const MODERN_ERROR_CODES: RangeInclusive<i64> = -32099..=-32020;
const TTL_KEY: &str = "ttlMs";
const CACHE_SCOPE_KEY: &str = "cacheScope";

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

/// Roots, Sampling and Logging are deprecated and deliberately never
/// declared, so a server has no reason to ask for them.
pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": LEGACY_PROTOCOL_VERSION,
        "capabilities": {
            "elicitation": elicitation_capability(),
        },
        "clientInfo": client_info(),
    })
}

pub fn client_info() -> Value {
    json!({
        "name": CLIENT_NAME,
        "title": CLIENT_TITLE,
        "version": env!("CARGO_PKG_VERSION"),
    })
}

pub fn elicitation_capability() -> Value {
    json!({"form": {}, "url": {}})
}

/// A server feature with a list that can change under the client.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListCapability {
    pub list_changed: bool,
}

/// What a server declared it offers. A feature is declared by an object,
/// which is what sloppy servers get wrong most, so anything else reads as
/// absent instead of failing the connection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerCapabilities {
    pub tools: Option<ListCapability>,
    pub prompts: Option<ListCapability>,
    pub resources: Option<ListCapability>,
    pub completions: bool,
    pub logging: bool,
}

impl ServerCapabilities {
    pub fn parse(capabilities: &Value) -> Self {
        let list = |name: &str| {
            capabilities[name]
                .as_object()
                .map(|feature| ListCapability {
                    list_changed: feature.get("listChanged").and_then(Value::as_bool) == Some(true),
                })
        };
        Self {
            tools: list("tools"),
            prompts: list("prompts"),
            resources: list("resources"),
            completions: capabilities["completions"].is_object(),
            logging: capabilities["logging"].is_object(),
        }
    }

    /// Whether the server may tell the client a list changed, which is what
    /// a standing stream from it is for.
    pub fn any_list_changed(&self) -> bool {
        [self.tools, self.prompts, self.resources]
            .into_iter()
            .flatten()
            .any(|feature| feature.list_changed)
    }

    /// The `subscriptions/listen` filter asking for exactly the list changes
    /// the server declared.
    pub fn listen_filter(&self) -> Value {
        let filter: Map<String, Value> = [
            (TOOLS_LIST_CHANGED_FILTER, self.tools),
            (PROMPTS_LIST_CHANGED_FILTER, self.prompts),
            (RESOURCES_LIST_CHANGED_FILTER, self.resources),
        ]
        .into_iter()
        .filter(|(_, feature)| feature.is_some_and(|feature| feature.list_changed))
        .map(|(key, _)| (key.to_owned(), Value::Bool(true)))
        .collect();
        Value::Object(filter)
    }
}

/// The codes the MCP specification reserves for itself. One marks a modern
/// server, whatever the HTTP status it came with.
pub fn is_modern_error(code: i64) -> bool {
    MODERN_ERROR_CODES.contains(&code)
}

/// Who may keep a cacheable result: anyone, or only the same credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheScope {
    Public,
    Private,
}

/// How long a modern server says a result stays fresh. Kept, never polled:
/// changes arrive through `subscriptions/listen`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheHint {
    pub ttl: Duration,
    pub scope: Option<CacheScope>,
}

impl CacheHint {
    /// An absent or negative `ttlMs` is stale at once.
    pub fn parse(result: &Value) -> Self {
        Self {
            ttl: Duration::from_millis(result[TTL_KEY].as_u64().unwrap_or_default()),
            scope: CacheScope::deserialize(&result[CACHE_SCOPE_KEY]).ok(),
        }
    }
}

/// What a server said about itself when the connection was set up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerDiscovery {
    pub protocol_version: String,
    pub server_info: ServerInfo,
    pub capabilities: ServerCapabilities,
    pub instructions: Option<String>,
    pub cache: CacheHint,
}

impl ServerDiscovery {
    pub fn from_initialize(result: &Value) -> Self {
        Self {
            protocol_version: result["protocolVersion"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            server_info: serde_json::from_value(result["serverInfo"].clone()).unwrap_or_default(),
            capabilities: ServerCapabilities::parse(&result["capabilities"]),
            instructions: instructions(result),
            cache: CacheHint::default(),
        }
    }

    /// A `server/discover` result, for the modern `version` chosen from it.
    pub fn from_discover(result: &Value, version: &str) -> Self {
        Self {
            protocol_version: version.to_owned(),
            server_info: serde_json::from_value(result[META_KEY][SERVER_INFO_META].clone())
                .unwrap_or_default(),
            capabilities: ServerCapabilities::parse(&result["capabilities"]),
            instructions: instructions(result),
            cache: CacheHint::parse(result),
        }
    }

    pub fn kind(&self) -> Option<McpServerKind> {
        McpServerKind::from_server_name(&self.server_info.name)
    }
}

fn instructions(result: &Value) -> Option<String> {
    result["instructions"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}

#[derive(Deserialize, Default)]
pub struct ToolInfo {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(default, rename = "outputSchema")]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub annotations: ToolAnnotations,
    #[serde(default, rename = "_meta")]
    pub meta: Option<Value>,
}

/// What a server says about a tool's behaviour. Hints only: `None` is "not
/// said", which the spec reads as the cautious default for each.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

impl ToolAnnotations {
    pub fn read_only(&self) -> bool {
        self.read_only_hint == Some(true)
    }

    pub fn idempotent(&self) -> bool {
        self.read_only() || self.idempotent_hint == Some(true)
    }
}

/// Who a server says it is: `serverInfo` from `initialize`, or the
/// `io.modelcontextprotocol/serverInfo` of a discover result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub version: String,
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
    /// Shaped like a tool result's content: text, an image, or an embedded
    /// resource.
    pub content: CallToolContent,
}

#[derive(Deserialize)]
pub struct GetPromptResult {
    pub messages: Vec<PromptMessage>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use test_case::test_case;

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
        let raw = json!({"name": "read_file", "description": "Read a file", "inputSchema": {"type": "object"}});
        let tool: ToolInfo = serde_json::from_value(raw).unwrap();
        assert_eq!(tool.name, "read_file");
        assert_eq!(tool.input_schema["type"], "object");
        assert_eq!(tool.annotations, ToolAnnotations::default());
    }

    #[test_case(json!({"readOnlyHint": true}), true, true ; "read_only")]
    #[test_case(json!({"idempotentHint": true}), false, true ; "idempotent")]
    #[test_case(json!({"readOnlyHint": false, "destructiveHint": true}), false, false ; "destructive")]
    #[test_case(json!({}), false, false ; "unsaid")]
    fn tool_info_keeps_annotations(annotations: Value, read_only: bool, idempotent: bool) {
        let raw = json!({"name": "t", "title": "T", "annotations": annotations, "outputSchema": {"type": "object"}});
        let tool: ToolInfo = serde_json::from_value(raw).unwrap();
        assert_eq!(tool.title.as_deref(), Some("T"));
        assert_eq!(tool.annotations.read_only(), read_only);
        assert_eq!(tool.annotations.idempotent(), idempotent);
        assert!(tool.output_schema.is_some());
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
    fn prompt_info_deserializes() {
        let raw = json!({
            "name": "code-review",
            "description": "Review code changes",
            "arguments": [
                {"name": "diff", "description": "The diff to review", "required": true},
                {"name": "style", "required": false}
            ]
        });
        let prompt: PromptInfo = serde_json::from_value(raw).unwrap();
        assert_eq!(prompt.name, "code-review");
        assert_eq!(prompt.description.as_deref(), Some("Review code changes"));
        assert_eq!(prompt.arguments.len(), 2);
        assert!(prompt.arguments[0].required);
        assert!(!prompt.arguments[1].required);
    }

    #[test]
    fn prompt_info_defaults() {
        let prompt: PromptInfo = serde_json::from_value(json!({"name": "simple"})).unwrap();
        assert!(prompt.description.is_none());
        assert!(prompt.arguments.is_empty());
    }

    #[test]
    fn initialize_declares_elicitation_and_nothing_deprecated() {
        let params = initialize_params();
        assert_eq!(params["protocolVersion"], LEGACY_PROTOCOL_VERSION);
        let capabilities = params["capabilities"].as_object().unwrap();
        assert_eq!(capabilities.keys().collect::<Vec<_>>(), ["elicitation"]);
        assert_eq!(
            params["capabilities"]["elicitation"],
            json!({"form": {}, "url": {}})
        );
        assert_eq!(params["clientInfo"]["name"], CLIENT_NAME);
        assert_eq!(params["clientInfo"]["title"], CLIENT_TITLE);
    }

    #[test_case(json!({"tools": {}, "prompts": {}}), Some(false), Some(false), false ; "both")]
    #[test_case(json!({"tools": {"listChanged": true}}), Some(true), None, true ; "tools_list_changed")]
    #[test_case(json!({"prompts": {"listChanged": false}, "tools": true}), None, Some(false), false ; "non_object_is_absent")]
    #[test_case(json!(null), None, None, false ; "no_capabilities")]
    fn parses_capabilities(
        raw: Value,
        tools: Option<bool>,
        prompts: Option<bool>,
        list_changed: bool,
    ) {
        let capabilities = ServerCapabilities::parse(&raw);
        let changed = |feature: Option<ListCapability>| feature.map(|f| f.list_changed);
        assert_eq!(changed(capabilities.tools), tools);
        assert_eq!(changed(capabilities.prompts), prompts);
        assert_eq!(capabilities.any_list_changed(), list_changed);
    }

    #[test_case(json!({"tools": {"listChanged": true}, "prompts": {}, "resources": {"listChanged": true}}), json!({TOOLS_LIST_CHANGED_FILTER: true, RESOURCES_LIST_CHANGED_FILTER: true}) ; "declared_only")]
    #[test_case(json!({"prompts": {"listChanged": true}}), json!({PROMPTS_LIST_CHANGED_FILTER: true}) ; "prompts")]
    #[test_case(json!({"tools": {}}), json!({}) ; "none_declared")]
    fn listen_filter_asks_for_declared_changes(raw: Value, filter: Value) {
        assert_eq!(ServerCapabilities::parse(&raw).listen_filter(), filter);
    }

    #[test_case(-32020, true ; "header_mismatch")]
    #[test_case(-32022, true ; "unsupported_version")]
    #[test_case(-32099, true ; "lowest_reserved")]
    #[test_case(-32601, false ; "method_not_found")]
    #[test_case(-32002, false ; "legacy_resource_not_found")]
    #[test_case(-32100, false ; "below_reserved")]
    fn recognises_modern_error_codes(code: i64, modern: bool) {
        assert_eq!(is_modern_error(code), modern);
    }

    #[test]
    fn discovery_keeps_identity_capabilities_and_instructions() {
        let discovery = ServerDiscovery::from_initialize(&json!({
            "protocolVersion": LEGACY_PROTOCOL_VERSION,
            "serverInfo": {"name": "executor", "title": "Executor", "version": "1.2"},
            "capabilities": {"completions": {}, "logging": {}, "resources": {"listChanged": true}},
            "instructions": "Use execute for scripts.",
        }));
        assert_eq!(discovery.protocol_version, LEGACY_PROTOCOL_VERSION);
        assert_eq!(discovery.server_info.title.as_deref(), Some("Executor"));
        assert_eq!(discovery.kind(), Some(McpServerKind::Executor));
        assert!(discovery.capabilities.completions && discovery.capabilities.logging);
        assert!(discovery.capabilities.any_list_changed());
        assert_eq!(
            discovery.instructions.as_deref(),
            Some("Use execute for scripts.")
        );
        assert_eq!(discovery.cache, CacheHint::default());
    }

    #[test]
    fn a_discover_result_reads_identity_from_its_meta() {
        let discovery = ServerDiscovery::from_discover(
            &json!({
                "resultType": "complete",
                "supportedVersions": [LATEST_PROTOCOL_VERSION],
                "capabilities": {"tools": {"listChanged": true}},
                META_KEY: {SERVER_INFO_META: {"name": "home-assistant", "version": "2026.10"}},
                "instructions": "Turn lights on.",
                TTL_KEY: 60_000,
                CACHE_SCOPE_KEY: "public",
            }),
            LATEST_PROTOCOL_VERSION,
        );
        assert_eq!(discovery.protocol_version, LATEST_PROTOCOL_VERSION);
        assert_eq!(discovery.kind(), Some(McpServerKind::HomeAssistant));
        assert_eq!(discovery.server_info.version, "2026.10");
        assert!(discovery.capabilities.any_list_changed());
        assert_eq!(discovery.instructions.as_deref(), Some("Turn lights on."));
        assert_eq!(
            discovery.cache,
            CacheHint {
                ttl: Duration::from_secs(60),
                scope: Some(CacheScope::Public),
            }
        );
    }

    #[test_case(json!({TTL_KEY: 1500, CACHE_SCOPE_KEY: "private"}), Duration::from_millis(1500), Some(CacheScope::Private) ; "private")]
    #[test_case(json!({}), Duration::ZERO, None ; "absent_is_stale")]
    #[test_case(json!({TTL_KEY: -5, CACHE_SCOPE_KEY: "shared"}), Duration::ZERO, None ; "negative_and_unknown_ignored")]
    fn cache_hints_default_to_stale(raw: Value, ttl: Duration, scope: Option<CacheScope>) {
        assert_eq!(CacheHint::parse(&raw), CacheHint { ttl, scope });
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
        assert_eq!(result.messages[0].content.text, "Review this code");
        assert_eq!(result.messages[1].role, PromptRole::Assistant);
    }
}
