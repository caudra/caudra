//! `mcp.toml` constants and field metadata. The schema lives in caudra-agent,
//! which reads these so the reference cannot drift from the loader.

use crate::{ConfigField, ConfigValue};

pub const MCP_FILE: &str = "mcp.toml";
pub const MCP_VERSION: u32 = 1;
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const MAX_TIMEOUT_MS: u64 = 300_000;
/// Below this many deferrable tools, a search round-trip plus its
/// prompt-cache miss cost more than a handful of upfront definitions.
pub const DEFAULT_DEFER_TOOLS: usize = 10;
pub const DEFAULT_CALLBACK_PATH: &str = "/mcp/oauth/callback";
pub const DEFAULT_CALLBACK_HOSTNAME: &str = "127.0.0.1";
const MIN_TIMEOUT_MS: u64 = 1;

pub const TOP_LEVEL_FIELDS: &[ConfigField] = &[ConfigField {
    name: "defer_tools",
    ty: "integer",
    default: ConfigValue::U64(DEFAULT_DEFER_TOOLS as u64),
    min: None,
    max: None,
    env: None,
    description: "Defer MCP tools behind `tool_search` only when the servers offer more than this many. `0` always defers. A project value replaces the global one",
}];

/// Keys every server takes, whatever its transport.
pub const SERVER_FIELDS: &[ConfigField] = &[
    ConfigField {
        name: "enabled",
        ty: "bool",
        default: ConfigValue::Bool(true),
        min: None,
        max: None,
        env: None,
        description: "Start the server. `/mcp` sets this key when it turns a server on or off",
    },
    ConfigField {
        name: "timeout",
        ty: "integer",
        default: ConfigValue::U64(DEFAULT_TIMEOUT_MS),
        min: Some(MIN_TIMEOUT_MS),
        max: Some(MAX_TIMEOUT_MS),
        env: None,
        description: "Milliseconds to wait for each response from the server",
    },
    ConfigField {
        name: "always_load",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        max: None,
        env: None,
        description: "Load every tool of the server up front instead of through `tool_search`",
    },
];

pub const STDIO_FIELDS: &[ConfigField] = &[
    ConfigField {
        name: "command",
        ty: "string[]",
        default: ConfigValue::Required(
            r#"["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"]"#,
        ),
        min: None,
        max: None,
        env: None,
        description: "Stdio servers: the program and its arguments. It must not be empty. When `url` is also set, `command` wins",
    },
    ConfigField {
        name: "environment",
        ty: "table",
        default: ConfigValue::Toml("{}"),
        min: None,
        max: None,
        env: None,
        description: "Stdio servers: environment variables for the server process, such as `{ GITHUB_TOKEN = \"...\" }`. Values are stored as plain text",
    },
];

pub const HTTP_FIELDS: &[ConfigField] = &[
    ConfigField {
        name: "url",
        ty: "string",
        default: ConfigValue::Required(r#""https://mcp.example.com/mcp""#),
        min: None,
        max: None,
        env: None,
        description: "HTTP servers: the server URL. It must start with `http://` or `https://`",
    },
    ConfigField {
        name: "headers",
        ty: "table",
        default: ConfigValue::Toml("{}"),
        min: None,
        max: None,
        env: None,
        description: "HTTP servers: headers sent with every request, such as `{ Authorization = \"Bearer ...\" }`. Values are stored as plain text",
    },
    ConfigField {
        name: "oauth",
        ty: "table",
        default: ConfigValue::Unset,
        min: None,
        max: None,
        env: None,
        description: "HTTP servers: a static OAuth client, for a server that has no dynamic client registration",
    },
];

pub const OAUTH_FIELDS: &[ConfigField] = &[
    ConfigField {
        name: "client_id",
        ty: "string",
        default: ConfigValue::Required(r#""analytics-client""#),
        min: None,
        max: None,
        env: None,
        description: "The client ID of the app you registered with the server",
    },
    ConfigField {
        name: "client_secret",
        ty: "string",
        default: ConfigValue::Unset,
        min: None,
        max: None,
        env: None,
        description: "The client secret, for a confidential client. It is stored as plain text",
    },
    ConfigField {
        name: "callback_port",
        ty: "integer",
        default: ConfigValue::Unset,
        min: None,
        max: Some(u16::MAX as u64),
        env: None,
        description: "Pin the loopback port of the redirect URI, so you can register the URI in advance. Unset tries the default port, then any free port, so the URI can change between runs",
    },
    ConfigField {
        name: "callback_path",
        ty: "string",
        default: ConfigValue::Str(DEFAULT_CALLBACK_PATH),
        min: None,
        max: None,
        env: None,
        description: "The path of the redirect URI. It must start with `/`",
    },
    ConfigField {
        name: "callback_hostname",
        ty: "string",
        default: ConfigValue::Str(DEFAULT_CALLBACK_HOSTNAME),
        min: None,
        max: None,
        env: None,
        description: "The host name of the redirect URI, such as `localhost` when the server registered that form. The listener still binds to 127.0.0.1",
    },
];
