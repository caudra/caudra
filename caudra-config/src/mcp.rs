//! `mcp.toml` constants and field metadata. The schema lives in caudra-agent,
//! which reads these so the reference cannot drift from the loader.

use crate::migration::{ConfigFormat, MigrationError, Step, Upgrade};
use crate::{ConfigField, ConfigValue};

pub const MCP_FILE: &str = "mcp.toml";
pub const MCP_VERSION: u32 = 2;
pub const MCP_FORMAT: ConfigFormat = ConfigFormat {
    file: MCP_FILE,
    latest: MCP_VERSION,
    steps: &[Step {
        from: 1,
        why: Some(DEFERRAL_REMOVED),
        apply: upgrade_from_1,
    }],
    requires_version: false,
};
const SERVERS_KEY: &str = "mcp";
const DEFERRAL_REMOVED: &str =
    "MCP tools are reached through the `code` tool, which loads nothing up front";
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const MAX_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_CALLBACK_PATH: &str = "/mcp/oauth/callback";
pub const DEFAULT_CALLBACK_HOSTNAME: &str = "127.0.0.1";
const MIN_TIMEOUT_MS: u64 = 1;

/// Keys every server takes, whatever its transport.
pub const SERVER_FIELDS: &[ConfigField] = &[
    ConfigField {
        name: "enabled",
        ty: "bool",
        default: ConfigValue::Bool(true),
        min: None,
        max: None,
        env: None,
        choices: &[],
        description: "Start the server. `/mcp` sets this key when it turns a server on or off",
    },
    ConfigField {
        name: "timeout",
        ty: "integer",
        default: ConfigValue::U64(DEFAULT_TIMEOUT_MS),
        min: Some(MIN_TIMEOUT_MS),
        max: Some(MAX_TIMEOUT_MS),
        env: None,
        choices: &[],
        description: "Milliseconds to wait for each response from the server",
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
        choices: &[],
        description: "Stdio servers: the program and its arguments. It must not be empty. When `url` is also set, `command` wins",
    },
    ConfigField {
        name: "environment",
        ty: "table",
        default: ConfigValue::Toml("{}"),
        min: None,
        max: None,
        env: None,
        choices: &[],
        description: "Stdio servers: environment variables for the server process, such as `{ GITHUB_TOKEN = \"{env:GITHUB_TOKEN}\" }`. In the global file, `{env:NAME}` reads a value from your environment and `{file:PATH}` reads a file. Other values are stored as plain text",
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
        choices: &[],
        description: "HTTP servers: the server URL. It must start with `http://` or `https://`",
    },
    ConfigField {
        name: "headers",
        ty: "table",
        default: ConfigValue::Toml("{}"),
        min: None,
        max: None,
        env: None,
        choices: &[],
        description: "HTTP servers: headers sent with every request, such as `{ Authorization = \"Bearer {env:GITHUB_TOKEN}\" }`. In the global file, `{env:NAME}` reads a value from your environment and `{file:PATH}` reads a file. Other values are stored as plain text",
    },
    ConfigField {
        name: "oauth",
        ty: "table",
        default: ConfigValue::Unset,
        min: None,
        max: None,
        env: None,
        choices: &[],
        description: "HTTP servers: a static OAuth client, for a server Caudra cannot get a client from on its own",
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
        choices: &[],
        description: "The client ID of the app you registered with the server",
    },
    ConfigField {
        name: "client_secret",
        ty: "string",
        default: ConfigValue::Unset,
        min: None,
        max: None,
        env: None,
        choices: &[],
        description: "The client secret, for a confidential client. It is stored as plain text unless the global file reads it with `{env:NAME}` from your environment or with `{file:PATH}` from a file",
    },
    ConfigField {
        name: "callback_port",
        ty: "integer",
        default: ConfigValue::Unset,
        min: None,
        max: Some(u16::MAX as u64),
        env: None,
        choices: &[],
        description: "Pin the loopback port of the redirect URI, so you can register the URI in advance. Unset tries the default port, then any free port, so the URI can change between runs",
    },
    ConfigField {
        name: "callback_path",
        ty: "string",
        default: ConfigValue::Str(DEFAULT_CALLBACK_PATH),
        min: None,
        max: None,
        env: None,
        choices: &[],
        description: "The path of the redirect URI. It must start with `/`",
    },
    ConfigField {
        name: "callback_hostname",
        ty: "string",
        default: ConfigValue::Str(DEFAULT_CALLBACK_HOSTNAME),
        min: None,
        max: None,
        env: None,
        choices: &[],
        description: "The host name of the redirect URI, such as `localhost` when the server registered that form. The listener still binds to 127.0.0.1",
    },
];

fn upgrade_from_1(upgrade: &mut Upgrade<'_>) -> Result<(), MigrationError> {
    upgrade.retire_key(&[], "defer_tools");
    let servers: Vec<String> = upgrade
        .document()
        .get(SERVERS_KEY)
        .and_then(|servers| servers.as_table_like())
        .map(|servers| servers.iter().map(|(name, _)| name.to_owned()).collect())
        .unwrap_or_default();
    for server in servers {
        upgrade.retire_key(&[SERVERS_KEY, &server], "always_load");
    }
    Ok(())
}
