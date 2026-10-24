use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fmt::{self, Write};
use std::fs;
use std::mem;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use caudra_config_macro::ConfigSection;
use caudra_storage::paths;
use caudra_storage::retention::{GroupBy, KeepPolicy};
use caudra_storage::sessions::{SOURCE_CODE_TOOL_RENAMES, StoredMode};
use caudra_storage::thinking::{StoredThinking, ThinkingParseError};
use caudra_storage::version::UpdateChannel;
use caudra_terminal_host::PANE_ENVIRONMENT;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::warn;
use url::Url;

use crate::config_file::SOURCE_CODE_WHY;
use crate::config_version::ConfigVersion;
use crate::interpolation::Lookup;
use crate::migration::{ConfigFormat, MigrationError, Step, Upgrade};
use crate::shell_env::{ShellEnvConfig, ShellEnvError, ShellEnvFileConfig};
use crate::websearch::WebsearchConfig;

pub const PROJECT_DIR: &str = ".caudra";
const PERMISSIONS_FILE: &str = "permissions.toml";
const ENV_FILE: &str = ".env";
pub const PERMISSIONS_VERSION: u32 = 2;
pub const PERMISSIONS_FORMAT: ConfigFormat = ConfigFormat {
    file: PERMISSIONS_FILE,
    latest: PERMISSIONS_VERSION,
    steps: &[Step {
        from: 1,
        why: None,
        apply: upgrade_permissions_from_1,
    }],
    requires_version: false,
};
const PYTHON_EXECUTION_RENAMED: &str = "the `python_execution` tool is now `code`";
const BASH_RENAMED: &str = "the `bash` tool is now `shell`";
const SHELL_PERMISSION_TOOLS: &[&str] = &["bash", "shell"];
const UNSET_DEFAULT: &str = "unset";
const REQUIRED_DEFAULT: &str = "required";
/// An `[mcp.SERVER]` rule entry that covers every tool of the server.
const MCP_WHOLE_SERVER: &str = "*";
/// Tools whose card never opens on its own. A truncated prefix of one of
/// these bodies carries nothing: a read and a fetch are windows into a
/// document, and a glob, a grep and an index are ordered by path or by source
/// rather than by relevance, so the visible lines are the alphabetically
/// first ones rather than the answer. Each already states its result in the
/// row annotation, so the fold loses no summary. The source-code lookups, the
/// history and session lookups, a loaded skill or saved output, and the reads
/// of the stores and runtimes are lookups too: each row says what it found,
/// and the whole answer is a click away.
const DEFAULT_ALWAYS_COLLAPSED: &[&str] = &[
    "file_read",
    "file_glob",
    "file_grep",
    "file_index",
    "webfetch",
    "source_code_context",
    "source_code_refs",
    "history.search",
    "history.read",
    "history.sessions",
    "tool_output",
    "skill",
    "execution_environment",
    "list_sessions",
    "read_topic",
    "automation",
    MEMORY_READ_ENTRY,
    PLAN_READ_ENTRY,
    WORKFLOW_READ_ENTRY,
    TASK_CONTROL_READ_ENTRY,
];
const MEMORY_READ_ENTRY: &str = "memory_read";
const PLAN_READ_ENTRY: &str = "plan_read";
const WORKFLOW_READ_ENTRY: &str = "workflow_read";
const TASK_CONTROL_READ_ENTRY: &str = "task_control_read";
/// `ui.always_collapsed` entries for the reading calls of a tool that both
/// reads and changes things, paired with the tool they cover. Its other calls
/// stay open, since their card is the record of what they changed or started.
pub const READ_ENTRIES: &[(&str, &str)] = &[
    (MEMORY_READ_ENTRY, "memory"),
    (PLAN_READ_ENTRY, "plan"),
    (WORKFLOW_READ_ENTRY, "workflow"),
    (TASK_CONTROL_READ_ENTRY, "task_control"),
];
/// Set only by the process that starts Caudra, never by a `.env` file, like
/// every terminal host's pane variables.
const PROCESS_ONLY_ENV_VARS: &[&str] = &["WORKCELL_MCP_CODE_WORKER"];
static PROJECT_ENV_FALLBACKS: Mutex<Option<HashMap<String, Option<String>>>> = Mutex::new(None);
static ENV_FILE_KEYS: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

pub mod atomic_file;
pub mod config_file;
pub mod config_version;
pub mod decision_features_file;
pub mod decisions;
pub mod example;
pub mod experimental;
pub mod files;
pub mod global_file;
pub mod interpolation;
pub mod mcp;
pub mod migration;
mod network;
pub mod own_writes;
pub mod profile_tools;
pub mod providers;
pub mod sandbox;
pub mod settings_file;
pub mod shell_env;
pub mod steering;
pub mod toml_document;
pub mod web;
pub mod websearch;
pub mod workcell;

pub use caudra_proxy::{ProxyEnv, ProxyRoute};
pub use decisions::{DecisionsConfig, FeatureMode};
pub use experimental::{Feature, FeatureDisabled, FeatureFlags};
pub use profile_tools::{
    LAZY_REMOVED, ProfileToolDefault, ProfileToolExposure, ProfileToolPolicy, ProfileToolSource,
    TOOL_POLICY_GROUPS, ToolPolicyGroup,
};
pub use steering::SteeringConfig;
pub use web::{WebConfig, WebFileConfig, WebHost, WebNetwork};

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 50 * 1024;
pub const DEFAULT_MAX_OUTPUT_LINES: usize = 2000;
pub const DEFAULT_FLASH_DURATION_MS: u64 = 10_000;
pub const DEFAULT_TYPEWRITER_MS_PER_CHAR: u64 = 4;
pub const DEFAULT_WHICH_KEY_DELAY_MS: u64 = 250;
pub const DEFAULT_MOUSE_SCROLL_LINES: u32 = 3;
pub const DEFAULT_SCROLL_CARD_LINES: u32 = 10;
pub const DEFAULT_THINKING_LINES: u32 = 10;
pub const DEFAULT_MAX_INPUT_LINES: u32 = 20;

pub const MIN_MAX_INPUT_LINES: u32 = 1;

pub const MAX_SERVER_NAME_LEN: usize = 64;

pub const DEFAULT_COMPACTION_BUFFER: CompactionBuffer = CompactionBuffer::Percent(20);
pub const DEFAULT_BACKGROUND_REMINDER_TURNS: u32 = 0;
pub const DEFAULT_WORK_NUDGE_COMPACTIONS: u32 = 2;
pub const DEFAULT_SHELL_ASYNC_THRESHOLD_SECS: u64 = 120;
pub const DEFAULT_RESUME_MAX_AGE_HOURS: u64 = 24;
const MIN_RESUME_MAX_AGE_HOURS: u64 = 1;
pub const MIN_SHELL_ASYNC_THRESHOLD_SECS: u64 = 1;
const TASK_SYNC_REQUIRED: &str =
    "this session cannot run background tasks; omit background or set it to false";
pub const CODE_REQUIRED: &str = "`code` cannot be disabled: it hosts the `image`, `host`, `workflow`, `automation`, `history`, `mcp` and `decisions` modules; turn off a module's tool group instead (`images`, `execution`, `delegation`)";
/// Windows that already exclude output need less held back, since the reserve
/// only has to absorb estimation drift rather than a whole response.
pub const DEFAULT_INPUT_BUDGET_COMPACTION_BUFFER: CompactionBuffer = CompactionBuffer::Percent(10);

pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_STREAM_TIMEOUT_SECS: u64 = 300;

pub const DEFAULT_MAX_LOG_BYTES_MB: u64 = 200;
pub const DEFAULT_MAX_LOG_FILES: u32 = 10;
pub const DEFAULT_MAX_EAGER_LOAD_MB: u64 = 1024;
pub const DEFAULT_LOG_LEVEL: LogLevel = LogLevel::Info;
pub const DEFAULT_INPUT_HISTORY_SIZE: usize = 100;
pub const DEFAULT_EPHEMERAL: bool = false;
pub const DEFAULT_RETENTION_SWEEP_INTERVAL_HOURS: u64 = 24;
pub const DEFAULT_SNAPSHOTS_ENABLED: bool = true;
pub const DEFAULT_SNAPSHOT_MAX_BYTES_MB: u64 = 512;
pub const DEFAULT_SNAPSHOT_MAX_FILES: u64 = 50_000;
pub const DEFAULT_SNAPSHOT_MAX_FILE_BYTES_MB: u64 = 100;
/// The change store refuses a zero limit, which would leave every call
/// unrecorded, so each `[storage.snapshots]` limit is at least this.
pub const MIN_SNAPSHOT_LIMIT: u64 = 1;
const SNAPSHOTS_SECTION: &str = "storage.snapshots";
const BYTES_PER_MB: u64 = 1024 * 1024;
const DEFAULT_STORE_BUDGET: NonZeroU64 =
    NonZeroU64::new(DEFAULT_SNAPSHOT_MAX_BYTES_MB * BYTES_PER_MB).unwrap();

pub const MIN_OUTPUT_BYTES: usize = 1024;
pub const MIN_OUTPUT_LINES: usize = 10;
pub const DEFAULT_INBOUND_PER_MINUTE: usize = 64;
pub const DEFAULT_SENDER_PER_MINUTE: usize = 16;
pub const DEFAULT_PUBLISH_PER_MINUTE: usize = 16;
pub const DEFAULT_MAX_FANOUT: usize = 32;
pub const MIN_MESSAGE_RATE: usize = 1;
pub const DEFAULT_HISTORY_DAYS: u64 = 30;
pub const DEFAULT_HISTORY_MAX_MESSAGES: u64 = 50_000;
pub const MIN_HISTORY_LIMIT: u64 = 1;
const HISTORY_DAYS_KEY: &str = "agent.messaging.history_days";
const HISTORY_MAX_MESSAGES_KEY: &str = "agent.messaging.history_max_messages";
pub const MIN_PER_TOOL_OUTPUT_BYTES: usize = 256;
pub const MIN_PER_TOOL_OUTPUT_LINES: usize = 4;
pub const MIN_COMPACTION_BUFFER: u32 = 1_000;
const MAX_COMPACTION_PERCENT: u8 = 99;
const COMPACTION_BUFFER_EXPECTED: &str =
    r#"a token count (e.g. 12000) or a percent of the context window (e.g. "20%")"#;
pub const MIN_MOUSE_SCROLL_LINES: u32 = 1;
pub const MIN_TOOL_OUTPUT_LINES: usize = 1;
pub const MIN_MAX_LOG_BYTES_MB: u64 = 1;
pub const MIN_MAX_LOG_FILES: u32 = 1;
pub const MIN_MAX_EAGER_LOAD_MB: u64 = 64;
pub const MIN_INPUT_HISTORY_SIZE: usize = 10;
pub const MIN_CONNECT_TIMEOUT_SECS: u64 = 1;
pub const MIN_STREAM_TIMEOUT_SECS: u64 = 10;
/// Off by default: writing Caudra plugins is a niche task, and the skill's
/// entry costs description tokens in every session that never writes one.
pub const DEFAULT_SKILL_PLUGIN_DEV: bool = false;
/// On by default: the skill is how the model learns to write a workflow for
/// the session it is in, and a workflow is the answer to many multi-step asks.
pub const DEFAULT_SKILL_WORKFLOW_DEV: bool = true;
/// On by default, like `workflow_dev`: the skill is how the model learns to
/// write an automation, and it stays out of the catalog until
/// `experimental.automations` is on.
pub const DEFAULT_SKILL_AUTOMATION_DEV: bool = true;
/// On by default: questions about Caudra itself are common, the catalog entry
/// is one line, and the pages load a section at a time only when asked for.
pub const DEFAULT_SKILL_DOCS: bool = true;
/// Off for now: pstack is an opinionated third-party workflow, and turning it
/// on also adds its `/pstack:` commands, task profiles and `code` calls.
pub const DEFAULT_SKILL_PSTACK: bool = false;
const SKILL_PLUGIN_DEV_FIELD: &str = "plugin_dev";
const SKILL_WORKFLOW_DEV_FIELD: &str = "workflow_dev";
const SKILL_AUTOMATION_DEV_FIELD: &str = "automation_dev";
const SKILL_DOCS_FIELD: &str = "docs";
const SKILL_PSTACK_FIELD: &str = "pstack";
const SKILL_FIELDS: [&str; 5] = [
    SKILL_PLUGIN_DEV_FIELD,
    SKILL_WORKFLOW_DEV_FIELD,
    SKILL_AUTOMATION_DEV_FIELD,
    SKILL_DOCS_FIELD,
    SKILL_PSTACK_FIELD,
];
const TASK_MAX_CONCURRENT_FIELD: &str = "max_concurrent";
pub const DEFAULT_TASK_MAX_CONCURRENT: usize = 8;
pub const MIN_TASK_MAX_CONCURRENT: usize = 1;
const INDEX_MAX_FILE_SIZE_FIELD: &str = "max_file_size_mb";
pub const DEFAULT_INDEX_MAX_FILE_SIZE_MB: usize = 2;
pub const MIN_INDEX_MAX_FILE_SIZE_MB: usize = 1;
/// Workcell briefly holds the input bytes and parser-owned source together,
/// so this caps those two buffers at 32 MiB before tree allocation.
pub const MAX_INDEX_MAX_FILE_SIZE_MB: usize = 16;
const AUTOMATIONS_SECTION: &str = "automations";
const TURNS_PER_HOUR_FIELD: &str = "turns_per_hour";
const MAX_UNATTENDED_TURNS_FIELD: &str = "max_unattended_turns";
const ALLOW_PRIVATE_NETWORK_FIELD: &str = "allow_private_network";
pub const DEFAULT_AUTOMATION_TURNS_PER_HOUR: u32 = 20;
pub const MIN_AUTOMATION_TURNS_PER_HOUR: u32 = 1;
pub const MAX_AUTOMATION_TURNS_PER_HOUR: u32 = 600;
pub const MIN_MAX_UNATTENDED_TURNS: u32 = 1;
pub const MAX_MAX_UNATTENDED_TURNS: u32 = 10_000;
pub const DEFAULT_ALLOW_PRIVATE_NETWORK: bool = false;
const AUTOPILOT_SECTION: &str = "autopilot";
const MAX_CONTINUATIONS_FIELD: &str = "max_continuations";
const MAX_COST_FIELD: &str = "max_cost";
const MAX_MINUTES_FIELD: &str = "max_minutes";
const ERROR_RETRIES_FIELD: &str = "error_retries";
pub const DEFAULT_AUTOPILOT_MAX_CONTINUATIONS: u32 = 100;
pub const UNCAPPED_AUTOPILOT_CONTINUATIONS: u32 = 0;
pub const MIN_AUTOPILOT_MAX_CONTINUATIONS: u32 = UNCAPPED_AUTOPILOT_CONTINUATIONS;
pub const MAX_AUTOPILOT_MAX_CONTINUATIONS: u32 = 10_000;
pub const DEFAULT_AUTOPILOT_ERROR_RETRIES: u32 = 5;
pub const MIN_AUTOPILOT_ERROR_RETRIES: u32 = 0;
pub const MAX_AUTOPILOT_ERROR_RETRIES: u32 = 20;
pub const MIN_AUTOPILOT_MAX_MINUTES: u32 = 1;
pub const MAX_AUTOPILOT_MAX_MINUTES: u32 = 525_600;

pub const DEFAULT_BUILTINS: &[&str] = &[
    "bash",
    "batch",
    "edit",
    "glob",
    "grep",
    "index",
    "list",
    "memory",
    "question",
    "read",
    "sessions",
    "skill",
    "task",
    "todo_write",
    "tool_output",
    "view_image",
    "webfetch",
    "websearch",
    "write",
];

/// [`DEFAULT_BUILTINS`] keys whose Lua plugin registered its tool under a
/// different name than the native tool that replaced it. `enabled = false` has
/// to disable what the user means, not the historical plugin id.
const LEGACY_PLUGIN_TOOLS: &[(&str, &[&str])] = &[
    ("bash", &["shell"]),
    ("edit", &["file_edit", "file_apply_patch"]),
    ("glob", &["file_glob"]),
    ("grep", &["file_grep"]),
    ("index", &["file_index"]),
    ("read", &["file_read"]),
    ("write", &["file_write"]),
];

/// [`DEFAULT_BUILTINS`] keys that never produced a tool of their own, or whose
/// tools are internal companions. Disabling them is a no-op, so their names
/// stay out of the resolved list instead of sitting in it matching nothing.
const TOOLLESS_PLUGINS: &[&str] = &["list", "sessions", "tool_output"];

/// The builtin skills `[plugins.skill]` turns on and off.
pub const SKILL_OPTIONS: &[ConfigField] = &[
    ConfigField {
        name: SKILL_PLUGIN_DEV_FIELD,
        ty: "boolean",
        default: ConfigValue::Bool(DEFAULT_SKILL_PLUGIN_DEV),
        min: None,
        max: None,
        env: None,
        choices: &[],
        description: "Offer the builtin caudra-plugin-dev skill for writing caudra plugins. Needs `experimental.lua_plugins`.",
    },
    ConfigField {
        name: SKILL_WORKFLOW_DEV_FIELD,
        ty: "boolean",
        default: ConfigValue::Bool(DEFAULT_SKILL_WORKFLOW_DEV),
        min: None,
        max: None,
        env: None,
        choices: &[],
        description: "Offer the builtin caudra-workflow-dev skill for writing and running workflows. Needs `experimental.workflows`.",
    },
    ConfigField {
        name: SKILL_AUTOMATION_DEV_FIELD,
        ty: "boolean",
        default: ConfigValue::Bool(DEFAULT_SKILL_AUTOMATION_DEV),
        min: None,
        max: None,
        env: None,
        choices: &[],
        description: "Offer the builtin caudra-automation-dev skill for writing automations. Needs `experimental.automations`.",
    },
    ConfigField {
        name: SKILL_DOCS_FIELD,
        ty: "boolean",
        default: ConfigValue::Bool(DEFAULT_SKILL_DOCS),
        min: None,
        max: None,
        env: None,
        choices: &[],
        description: "Offer the builtin caudra-docs skill: this build's user documentation, loaded one page or section at a time.",
    },
    ConfigField {
        name: SKILL_PSTACK_FIELD,
        ty: "boolean",
        default: ConfigValue::Bool(DEFAULT_SKILL_PSTACK),
        min: None,
        max: None,
        env: None,
        choices: &[],
        description: "Offer the bundled pstack: its skills as the `pstack` skill, the `/pstack:` commands, the `pstack-poteto-agent` and `pstack-comment-sicko` task profiles, and its scripts as `pstack` calls in `code`.",
    },
];

/// Options that native tools still read from the `plugins.<name>` table their
/// Lua plugin was configured under, besides `enabled`.
pub const NATIVE_PLUGIN_OPTIONS: &[(&str, &[ConfigField])] = &[
    (
        "index",
        &[ConfigField {
            name: INDEX_MAX_FILE_SIZE_FIELD,
            ty: "integer",
            default: ConfigValue::U64(DEFAULT_INDEX_MAX_FILE_SIZE_MB as u64),
            min: Some(MIN_INDEX_MAX_FILE_SIZE_MB as u64),
            max: Some(MAX_INDEX_MAX_FILE_SIZE_MB as u64),
            env: None,
            choices: &[],
            description: "Refuse to index files larger than this many MiB.",
        }],
    ),
    ("skill", SKILL_OPTIONS),
    (
        "task",
        &[ConfigField {
            name: TASK_MAX_CONCURRENT_FIELD,
            ty: "integer",
            default: ConfigValue::U64(DEFAULT_TASK_MAX_CONCURRENT as u64),
            min: Some(MIN_TASK_MAX_CONCURRENT as u64),
            max: None,
            env: None,
            choices: &[],
            description: "Max concurrently running subagents.",
        }],
    ),
];

/// Which of [`DEFAULT_BUILTINS`] production still loads from Lua. Empty: every
/// built-in is native now. The sources stay in the tree as Lua-API coverage
/// and as worked examples for plugin authors, so tests and docgen can still
/// load them by name.
pub const ACTIVE_DEFAULT_LUA_PLUGINS: &[&str] = &[];

/// Caudra's own native tools: session-shaped work, orchestration, and the
/// interactive surfaces. Workcell owns everything protocol-neutral.
pub const CAUDRA_NATIVE_TOOL_NAMES: &[&str] = &[
    "automation",
    "batch",
    "code",
    "image_generate",
    "list_sessions",
    "memory",
    "plan",
    "publish_message",
    "question",
    "read_topic",
    "send_message",
    "skill",
    "task",
    "task_control",
    "todo_write",
    "tool_output",
    "view_image",
    "work_assignment",
    "workflow",
];

pub const WORKCELL_NATIVE_TOOL_NAMES: &[&str] = &[
    "file_apply_patch",
    "file_edit",
    "file_glob",
    "file_grep",
    "file_read",
    "file_write",
    "file_index",
    "websearch",
    "webfetch",
    "shell",
    "source_code_context",
    "source_code_refs",
    "execution_environment",
];

/// Tools the agent reaches for on its own to page through an oversized result.
/// They stay enabled whatever the filters say, or a truncated result becomes
/// unreadable.
pub const INTERNAL_COMPANION_TOOL_NAMES: &[&str] = &["tool_output"];

/// Hosts the modules that replaced tools of their own, so no filter or profile can withhold it.
pub const CODE_TOOL_NAME: &str = "code";

/// Built-ins the model reaches only through a `code` module, never declared as tools of their
/// own. They stay registered, so their permission keys, rules, ledger names and cards are the
/// same as when the model called them.
pub const CODE_HOSTED_TOOLS: &[&str] = &[
    "image_generate",
    "execution_environment",
    "workflow",
    "automation",
];

/// `INTERNAL_COMPANION_TOOL_NAMES` overlaps the native list: it marks tools
/// that stay enabled regardless of `disabled_tools`, which is orthogonal to
/// who implements them. Dedupe so `--help` never prints a name twice.
pub fn all_builtin_tool_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = WORKCELL_NATIVE_TOOL_NAMES
        .iter()
        .chain(CAUDRA_NATIVE_TOOL_NAMES)
        .chain(ACTIVE_DEFAULT_LUA_PLUGINS)
        .chain(INTERNAL_COMPANION_TOOL_NAMES)
        .copied()
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

pub fn is_builtin_tool(name: &str) -> bool {
    all_builtin_tool_names().contains(&name)
}

/// A tool is enabled unless named in `disabled_tools` (config, CLI, or the raw
/// list a Lua caller holds, e.g. `caudra.api.get_tools`).
pub fn is_tool_enabled(disabled_tools: &[String], name: &str) -> bool {
    INTERNAL_COMPANION_TOOL_NAMES.contains(&name)
        || !disabled_tools
            .iter()
            .any(|pattern| tool_pattern_matches(pattern, name))
}

/// Entries in a disabled list are exact tool names, except `server.*`, which
/// covers every tool an MCP server publishes. Built-in names never contain a
/// dot, so one list holds both kinds without ambiguity.
pub fn tool_pattern_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

/// Which tools a `plugins.<name>` key disables. Empty for the keys that never
/// registered a tool of their own.
fn plugin_tools(plugin: &str) -> &'static [&'static str] {
    if let Some((_, tools)) = LEGACY_PLUGIN_TOOLS.iter().find(|(name, _)| *name == plugin) {
        tools
    } else if TOOLLESS_PLUGINS.contains(&plugin) {
        &[]
    } else {
        DEFAULT_BUILTINS
            .iter()
            .find(|builtin| **builtin == plugin)
            .map(std::slice::from_ref)
            .unwrap_or_default()
    }
}

/// A disabled list accepts a built-in tool name, an MCP `server.tool`, or a
/// whole server as `server.*`. A bare `*` is refused: silently dropping every
/// tool is never what a config meant to say.
pub fn is_disableable_tool(tool: &str) -> bool {
    match ToolKey::parse(tool) {
        Ok(ToolKey::Native(name)) => is_builtin_tool(&name),
        Ok(ToolKey::McpTool { .. } | ToolKey::McpServer { .. }) => true,
        Ok(ToolKey::Wildcard) | Err(_) => false,
    }
}

fn validate_disabled_tool(tool: &str) -> Result<(), ConfigError> {
    if tool == CODE_TOOL_NAME {
        return Err(ConfigError::CodeRequired);
    }
    if is_disableable_tool(tool) {
        return Ok(());
    }
    Err(ConfigError::UnknownTool {
        tool: tool.to_owned(),
        valid: all_builtin_tool_names().join(", "),
    })
}

pub const FILE_WRITE_TOOLS: &[&str] = &[
    "file_apply_patch",
    "file_edit",
    "file_write",
    "image_generate",
    "write",
    "edit",
    "multiedit",
    "edit_lines",
    "insert_lines",
];

#[derive(Debug, Clone, Copy)]
pub enum ConfigValue {
    Bool(bool),
    U64(u64),
    F64(f64),
    /// A string, quoted when written as TOML.
    Str(&'static str),
    /// A value already spelled as TOML, such as an array or an inline table.
    Toml(&'static str),
    /// No value until one is set.
    Unset,
    /// A default that depends on where Caudra runs, so only prose can state it.
    Varies(&'static str),
    /// No default, because every record has to set it. The payload is a
    /// sample value spelled as TOML.
    Required(&'static str),
}

impl ConfigValue {
    pub fn format_default(&self) -> String {
        match self {
            Self::Bool(value) => value.to_string(),
            Self::U64(value) => value.to_string(),
            Self::F64(value) => value.to_string(),
            Self::Str(text) | Self::Toml(text) | Self::Varies(text) => (*text).to_string(),
            Self::Unset => UNSET_DEFAULT.to_string(),
            Self::Required(_) => REQUIRED_DEFAULT.to_string(),
        }
    }

    /// The default as a TOML value, or `None` when no value states it.
    pub fn toml(&self) -> Option<String> {
        match self {
            Self::Bool(_) | Self::U64(_) | Self::Toml(_) => Some(self.format_default()),
            Self::F64(value) => Some(format!("{value:?}")),
            Self::Str(text) => Some(toml::Value::from(*text).to_string()),
            Self::Unset | Self::Varies(_) | Self::Required(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ConfigField {
    pub name: &'static str,
    pub ty: &'static str,
    pub default: ConfigValue,
    pub min: Option<u64>,
    pub max: Option<u64>,
    pub env: Option<&'static str>,
    /// Every value a string key accepts, as the file spells them; empty when any string goes.
    pub choices: &'static [&'static str],
    pub description: &'static str,
}

pub const TOP_LEVEL_FIELDS: &[ConfigField] = &[
    ConfigField {
        name: "always_yolo",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        max: None,
        env: None,
        choices: &[],
        description: "Start every session with YOLO mode (skip permission prompts, deny rules still apply); global config only",
    },
    ConfigField {
        name: "always_auto",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        max: None,
        env: None,
        choices: &[],
        description: "Start every session with Auto permission mode (preserve required prompts and screen unmatched calls); global config only. Screening runs only while a decision endpoint is active",
    },
    ConfigField {
        name: "always_fast",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        max: None,
        env: None,
        choices: &[],
        description: "Start every session with fast mode, on the models that sell a fast tier (ignored otherwise)",
    },
    ConfigField {
        name: "always_thinking",
        ty: "bool | string",
        default: ConfigValue::Unset,
        min: None,
        max: None,
        env: None,
        choices: &[],
        description: "Start every session with extended thinking (true/\"adaptive\", \"off\", an effort level (\"minimal\" to \"max\"), or a token budget)",
    },
];

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid project config: {0} is global-only; projects cannot change permission modes")]
    ProjectPermissionMode(&'static str),
    #[error("invalid project config: {0} is global-only; every project shares one message history")]
    ProjectMessageHistory(&'static str),
    #[error(
        "invalid project config: [automations] is global-only; move it to the global caudra.toml"
    )]
    ProjectAutomations,
    #[error(
        "invalid project config: [autopilot] is global-only; move it to the global caudra.toml"
    )]
    ProjectAutopilot,
    #[error("invalid project config: [web] is global-only; move it to the global caudra.toml")]
    ProjectWeb,
    #[error(
        "invalid project config: [agent.shell_env] is global-only; move it to the global caudra.toml"
    )]
    ProjectShellEnv,
    #[error(
        "invalid project config: agent.cache_warming is global-only because it spends quota; move it to the global caudra.toml"
    )]
    ProjectCacheWarming,
    #[error(
        "invalid project config: [websearch] is global-only because it holds credentials and decides where search queries go; move it to the global caudra.toml"
    )]
    ProjectWebsearch,
    #[error("invalid config: websearch.backends lists {0} more than once")]
    WebsearchDuplicateBackend(websearch::WebsearchBackendName),
    #[error("invalid config: {key}: {source}")]
    ShellEnv {
        key: String,
        #[source]
        source: ShellEnvError,
    },
    #[error(transparent)]
    Decisions(#[from] decisions::DecisionsConfigError),
    #[error(transparent)]
    Web(#[from] web::WebConfigError),
    #[error("invalid config: agent.steering.{field}: {message}")]
    InvalidSteering { field: String, message: String },
    #[error("invalid config: {section}.{field} = {value} is below minimum ({min})")]
    BelowMinimum {
        section: &'static str,
        field: &'static str,
        value: u64,
        min: u64,
    },
    #[error("invalid config: {section}.{field} = {value} is out of range ({min} to {max})")]
    OutOfRange {
        section: &'static str,
        field: &'static str,
        value: u32,
        min: u32,
        max: u32,
    },
    #[error("invalid config: {section}.{field} = {value} must be positive and finite")]
    OutOfRangeFloat {
        section: &'static str,
        field: &'static str,
        value: f64,
    },
    #[error("invalid config: always_thinking: {0}")]
    Thinking(#[from] ThinkingParseError),
    #[error(
        "invalid config: plugins.{plugin}: no bundled plugin is named \"{plugin}\" \
         (bundled plugins: {valid})"
    )]
    UnknownPlugin { plugin: String, valid: String },
    #[error(
        "invalid config: agent.disabled_tools: no tool is named \"{tool}\" \
         (built-in tools: {valid}; MCP tools use `server.tool` or `server.*`)"
    )]
    UnknownTool { tool: String, valid: String },
    #[error("invalid config: agent.disabled_tools: {}", CODE_REQUIRED)]
    CodeRequired,
    /// A `plugins.<name>` table whose tool is now native: the key is kept for
    /// compatibility, but Rust validates it instead of the plugin.
    #[error("invalid config: plugins.{plugin}.{field}: {message}")]
    InvalidNativeToolOption {
        plugin: &'static str,
        field: String,
        message: String,
    },
    #[error("invalid config: provider.{field} contains invalid glob pattern `{pattern}`: {source}")]
    InvalidModelPattern {
        field: &'static str,
        pattern: String,
        #[source]
        source: globset::Error,
    },
}

fn check(
    section: &'static str,
    field: &'static str,
    value: u64,
    min: u64,
) -> Result<(), ConfigError> {
    if value < min {
        return Err(ConfigError::BelowMinimum {
            section,
            field,
            value,
            min,
        });
    }
    Ok(())
}

fn check_range(
    section: &'static str,
    field: &'static str,
    value: u32,
    min: u32,
    max: u32,
) -> Result<(), ConfigError> {
    if (min..=max).contains(&value) {
        return Ok(());
    }
    Err(ConfigError::OutOfRange {
        section,
        field,
        value,
        min,
        max,
    })
}

macro_rules! merge_option {
    ($self:ident, $overlay:ident, $($field:ident),+) => {
        $(if $overlay.$field.is_some() { $self.$field = $overlay.$field; })+
    };
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum AlwaysThinking {
    Toggle(bool),
    Budget(u32),
    Mode(String),
}

impl AlwaysThinking {
    fn resolve(self) -> Result<StoredThinking, ThinkingParseError> {
        match self {
            Self::Toggle(true) => Ok(StoredThinking::Adaptive),
            Self::Toggle(false) => Ok(StoredThinking::Off),
            Self::Budget(n) => StoredThinking::parse_setting(&n.to_string()),
            Self::Mode(s) => StoredThinking::parse_setting(&s),
        }
    }
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct RawConfig {
    pub decisions: decisions::RawDecisionsConfig,
    pub always_yolo: Option<bool>,
    pub always_auto: Option<bool>,
    #[serde(skip)]
    #[doc(hidden)]
    pub project_permission_mode_override: Option<&'static str>,
    #[serde(skip)]
    #[doc(hidden)]
    pub project_message_history_override: Option<&'static str>,
    #[serde(skip)]
    #[doc(hidden)]
    pub project_automations_override: bool,
    #[serde(skip)]
    #[doc(hidden)]
    pub project_autopilot_override: bool,
    #[serde(skip)]
    #[doc(hidden)]
    pub project_web_override: bool,
    #[serde(skip)]
    #[doc(hidden)]
    pub project_shell_env_override: bool,
    #[serde(skip)]
    #[doc(hidden)]
    pub project_cache_warming_override: bool,
    #[serde(skip)]
    #[doc(hidden)]
    pub project_websearch_override: bool,
    pub always_fast: Option<bool>,
    pub always_thinking: Option<AlwaysThinking>,
    #[serde(default)]
    pub ui: UiFileConfig,
    pub agent: AgentFileConfig,
    pub provider: ProviderFileConfig,
    pub storage: StorageFileConfig,
    pub telemetry: TelemetryConfig,
    pub worktrees: WorktreesConfig,
    /// `Some` for any `[automations]` table, even an empty one, so a project
    /// layer that holds one is refused.
    pub automations: Option<AutomationsFileConfig>,
    /// `Some` for any `[autopilot]` table, even an empty one, so a project
    /// layer that holds one is refused.
    pub autopilot: Option<AutopilotFileConfig>,
    /// `Some` for any `[web]` table, even an empty one, so a project layer
    /// that holds one is refused.
    pub web: Option<WebFileConfig>,
    /// `Some` for any `[websearch]` table, even an empty one, so a project
    /// layer that holds one is refused.
    pub websearch: Option<WebsearchConfig>,
    pub plugins: HashMap<String, PluginFileConfig>,
}

impl RawConfig {
    /// Applies a project layer: global-only settings are refused and the
    /// decision engine and inbound messaging policy may only be tightened.
    pub fn merge(&mut self, mut overlay: RawConfig) {
        self.project_permission_mode_override = self
            .project_permission_mode_override
            .or(overlay.project_permission_mode_override)
            .or(overlay.always_yolo.map(|_| "always_yolo"))
            .or(overlay.always_auto.map(|_| "always_auto"));
        self.project_automations_override |=
            overlay.project_automations_override || overlay.automations.is_some();
        self.project_autopilot_override |=
            overlay.project_autopilot_override || overlay.autopilot.is_some();
        self.project_web_override |= overlay.project_web_override || overlay.web.is_some();
        let holds_shell_env = overlay.agent.shell_env.take().is_some();
        self.project_shell_env_override |= overlay.project_shell_env_override || holds_shell_env;
        let holds_cache_warming = overlay.agent.cache_warming.take().is_some();
        self.project_cache_warming_override |=
            overlay.project_cache_warming_override || holds_cache_warming;
        self.project_websearch_override |=
            overlay.project_websearch_override || overlay.websearch.take().is_some();
        self.decisions.restrict(mem::take(&mut overlay.decisions));
        let messaging = &mut overlay.agent.messaging;
        self.project_message_history_override = self
            .project_message_history_override
            .or(overlay.project_message_history_override)
            .or(messaging.history_days.take().map(|_| HISTORY_DAYS_KEY))
            .or(messaging
                .history_max_messages
                .take()
                .map(|_| HISTORY_MAX_MESSAGES_KEY));
        messaging.project_inbound = messaging
            .project_inbound
            .take()
            .max(messaging.inbound.clone());
        messaging.project_inbound_per_minute = lowest(
            messaging.project_inbound_per_minute,
            messaging.inbound_per_minute.take(),
        );
        messaging.project_sender_per_minute = lowest(
            messaging.project_sender_per_minute,
            messaging.sender_per_minute.take(),
        );
        messaging.project_publish_per_minute = lowest(
            messaging.project_publish_per_minute,
            messaging.publish_per_minute.take(),
        );
        messaging.project_max_fanout =
            lowest(messaging.project_max_fanout, messaging.max_fanout.take());
        if let Some(inbound) = messaging.inbound.take() {
            self.agent.messaging.inbound = Some(
                self.agent
                    .messaging
                    .inbound
                    .take()
                    .unwrap_or_default()
                    .max(inbound),
            );
        }
        self.merge_shared(overlay);
    }

    /// Applies a layer with the same authority, such as the global `init.lua`
    /// over the global `caudra.toml`: every setting it names wins.
    pub fn merge_global(&mut self, mut overlay: RawConfig) {
        self.project_permission_mode_override = self
            .project_permission_mode_override
            .or(overlay.project_permission_mode_override);
        self.project_message_history_override = self
            .project_message_history_override
            .or(overlay.project_message_history_override);
        self.project_automations_override |= overlay.project_automations_override;
        self.project_autopilot_override |= overlay.project_autopilot_override;
        self.project_web_override |= overlay.project_web_override;
        self.project_shell_env_override |= overlay.project_shell_env_override;
        self.project_cache_warming_override |= overlay.project_cache_warming_override;
        self.project_websearch_override |= overlay.project_websearch_override;
        merge_option!(self, overlay, always_yolo, always_auto);
        if let Some(websearch) = overlay.websearch.take() {
            self.websearch.get_or_insert_default().merge(websearch);
        }
        if let Some(automations) = overlay.automations.take() {
            self.automations.get_or_insert_default().merge(automations);
        }
        if let Some(autopilot) = overlay.autopilot.take() {
            self.autopilot.get_or_insert_default().merge(autopilot);
        }
        if let Some(web) = overlay.web.take() {
            self.web.get_or_insert_default().merge(web);
        }
        self.decisions.overlay(mem::take(&mut overlay.decisions));
        self.merge_shared(overlay);
    }

    fn merge_shared(&mut self, overlay: RawConfig) {
        merge_option!(self, overlay, always_fast, always_thinking);
        self.ui.merge(overlay.ui);
        self.agent.merge(overlay.agent);
        self.provider.merge(overlay.provider);
        self.storage.merge(overlay.storage);
        self.telemetry.merge(overlay.telemetry);
        self.worktrees.merge(overlay.worktrees);
        for (name, plugin) in overlay.plugins {
            let entry = self.plugins.entry(name).or_default();
            if plugin.enabled.is_some() {
                entry.enabled = plugin.enabled;
            }
            entry.opts.extend(plugin.opts);
        }
    }

    pub fn into_config(mut self, no_rtk: bool) -> Result<Config, ConfigError> {
        if let Some(field) = self.project_permission_mode_override {
            return Err(ConfigError::ProjectPermissionMode(field));
        }
        if let Some(field) = self.project_message_history_override {
            return Err(ConfigError::ProjectMessageHistory(field));
        }
        if self.project_automations_override {
            return Err(ConfigError::ProjectAutomations);
        }
        if self.project_autopilot_override {
            return Err(ConfigError::ProjectAutopilot);
        }
        if self.project_web_override {
            return Err(ConfigError::ProjectWeb);
        }
        if self.project_shell_env_override {
            return Err(ConfigError::ProjectShellEnv);
        }
        if self.project_cache_warming_override {
            return Err(ConfigError::ProjectCacheWarming);
        }
        if self.project_websearch_override {
            return Err(ConfigError::ProjectWebsearch);
        }
        let websearch = self.websearch.take().unwrap_or_default();
        websearch.validate()?;
        let shell_env = ShellEnvConfig::from_file(self.agent.shell_env.take().unwrap_or_default())?;
        self.validate_plugin_tables()?;
        let index_max_file_size_mb = self.index_max_file_size_mb()?;
        let task_max_concurrent = self.task_max_concurrent()?;
        let builtin_skills = self.builtin_skills()?;
        let disabled_tools = self.resolve_disabled_tools()?;
        let config = Config {
            decisions: self.decisions.resolve()?,
            always_yolo: self.always_yolo.unwrap_or(false),
            always_auto: self.always_auto.unwrap_or(false),
            always_fast: self.always_fast.unwrap_or(false),
            always_thinking: self
                .always_thinking
                .map(AlwaysThinking::resolve)
                .transpose()?,
            ui: UiConfig::from_file(self.ui),
            agent: AgentConfig::from_file(
                self.agent,
                no_rtk,
                disabled_tools,
                index_max_file_size_mb,
                task_max_concurrent,
                builtin_skills,
                shell_env,
            ),
            provider: ProviderConfig::from_file(self.provider)?,
            storage: StorageConfig::from_file(self.storage),
            telemetry: self.telemetry,
            worktrees: self.worktrees,
            automations: AutomationsConfig::from_file(self.automations.unwrap_or_default()),
            autopilot: AutopilotConfig::from_file(self.autopilot.unwrap_or_default()),
            web: WebConfig::from_file(self.web.unwrap_or_default())?,
            permissions: PermissionsConfig::default(),
            plugins: PluginsConfig::from_plugins(self.plugins),
            websearch,
        };
        config.automations.validate()?;
        config.autopilot.validate()?;
        // Validate merged steering for every loader, without extending legacy field validation.
        config.agent.steering.validate()?;
        Ok(config)
    }

    /// A `plugins.<name>` key that matches no bundled plugin is a typo or an
    /// old config, so fail loudly instead of letting it silently drift.
    fn validate_plugin_tables(&self) -> Result<(), ConfigError> {
        let mut unknown: Vec<&String> = self
            .plugins
            .keys()
            .filter(|name| !DEFAULT_BUILTINS.contains(&name.as_str()))
            .collect();
        unknown.sort();
        if let Some(&plugin) = unknown.first() {
            return Err(ConfigError::UnknownPlugin {
                plugin: plugin.clone(),
                valid: DEFAULT_BUILTINS.join(", "),
            });
        }
        Ok(())
    }

    /// One resolved kill switch: what `agent.disabled_tools` names, plus every
    /// plugin turned off in the `plugins` table mapped to the names its tools
    /// are actually registered under.
    fn resolve_disabled_tools(&self) -> Result<Vec<String>, ConfigError> {
        let mut disabled: Vec<String> = Vec::new();
        for tool in self.agent.disabled_tools.iter().flatten() {
            validate_disabled_tool(tool)?;
            if !disabled.contains(tool) {
                disabled.push(tool.clone());
            }
        }
        let mut turned_off: Vec<&str> = self
            .plugins
            .iter()
            .filter(|(_, cfg)| cfg.enabled == Some(false))
            .map(|(name, _)| name.as_str())
            .collect();
        turned_off.sort_unstable();
        for tool in turned_off.into_iter().flat_map(plugin_tools) {
            if !disabled.iter().any(|name| name == tool) {
                disabled.push((*tool).to_owned());
            }
        }
        Ok(disabled)
    }

    /// Rejects any key a native tool does not declare, so a typo in a
    /// `plugins.<name>` table that no longer reaches a Lua validator still
    /// fails loudly.
    fn native_tool_opts<'a>(
        &'a self,
        plugin: &'static str,
        known: &[&str],
    ) -> Result<Option<&'a JsonMap<String, JsonValue>>, ConfigError> {
        let Some(table) = self.plugins.get(plugin) else {
            return Ok(None);
        };
        if let Some(field) = table.opts.keys().find(|f| !known.contains(&f.as_str())) {
            return Err(ConfigError::InvalidNativeToolOption {
                plugin,
                field: field.clone(),
                message: format!("unknown option (expected {})", known.join(", ")),
            });
        }
        Ok(Some(&table.opts))
    }

    fn index_max_file_size_mb(&self) -> Result<usize, ConfigError> {
        let invalid = |message: String| ConfigError::InvalidNativeToolOption {
            plugin: "index",
            field: INDEX_MAX_FILE_SIZE_FIELD.into(),
            message,
        };
        let Some(opts) = self.native_tool_opts("index", &[INDEX_MAX_FILE_SIZE_FIELD])? else {
            return Ok(DEFAULT_INDEX_MAX_FILE_SIZE_MB);
        };
        let Some(value) = opts.get(INDEX_MAX_FILE_SIZE_FIELD) else {
            return Ok(DEFAULT_INDEX_MAX_FILE_SIZE_MB);
        };
        let value = value
            .as_u64()
            .ok_or_else(|| invalid("expected an integer".into()))?;
        if value < MIN_INDEX_MAX_FILE_SIZE_MB as u64 {
            return Err(invalid(format!(
                "{value} is below minimum ({MIN_INDEX_MAX_FILE_SIZE_MB})"
            )));
        }
        if value > MAX_INDEX_MAX_FILE_SIZE_MB as u64 {
            return Err(invalid(format!(
                "{value} exceeds maximum ({MAX_INDEX_MAX_FILE_SIZE_MB})"
            )));
        }
        Ok(value as usize)
    }

    fn task_max_concurrent(&self) -> Result<usize, ConfigError> {
        let Some(opts) = self.native_tool_opts("task", &[TASK_MAX_CONCURRENT_FIELD])? else {
            return Ok(DEFAULT_TASK_MAX_CONCURRENT);
        };
        let Some(value) = opts.get(TASK_MAX_CONCURRENT_FIELD) else {
            return Ok(DEFAULT_TASK_MAX_CONCURRENT);
        };
        let invalid = |message: String| ConfigError::InvalidNativeToolOption {
            plugin: "task",
            field: TASK_MAX_CONCURRENT_FIELD.into(),
            message,
        };
        let value = value
            .as_u64()
            .ok_or_else(|| invalid("expected an integer".into()))?;
        if value < MIN_TASK_MAX_CONCURRENT as u64 {
            return Err(invalid(format!(
                "{value} is below minimum ({MIN_TASK_MAX_CONCURRENT})"
            )));
        }
        Ok(value as usize)
    }

    fn builtin_skills(&self) -> Result<BuiltinSkills, ConfigError> {
        Ok(BuiltinSkills {
            plugin_dev: self.skill_flag(SKILL_PLUGIN_DEV_FIELD, DEFAULT_SKILL_PLUGIN_DEV)?,
            workflow_dev: self.skill_flag(SKILL_WORKFLOW_DEV_FIELD, DEFAULT_SKILL_WORKFLOW_DEV)?,
            automation_dev: self
                .skill_flag(SKILL_AUTOMATION_DEV_FIELD, DEFAULT_SKILL_AUTOMATION_DEV)?,
            docs: self.skill_flag(SKILL_DOCS_FIELD, DEFAULT_SKILL_DOCS)?,
            pstack: self.skill_flag(SKILL_PSTACK_FIELD, DEFAULT_SKILL_PSTACK)?,
        })
    }

    fn skill_flag(&self, field: &'static str, default: bool) -> Result<bool, ConfigError> {
        let Some(opts) = self.native_tool_opts("skill", &SKILL_FIELDS)? else {
            return Ok(default);
        };
        match opts.get(field) {
            None => Ok(default),
            Some(JsonValue::Bool(value)) => Ok(*value),
            Some(_) => Err(ConfigError::InvalidNativeToolOption {
                plugin: "skill",
                field: field.into(),
                message: "expected a boolean".into(),
            }),
        }
    }
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default)]
pub struct PluginFileConfig {
    pub enabled: Option<bool>,
    /// Plugin-specific options passed through opaquely; each plugin declares
    /// and validates its own via `caudra.api.register_options`.
    #[serde(flatten)]
    pub opts: JsonMap<String, JsonValue>,
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct UiFileConfig {
    pub splash_animation: Option<bool>,
    pub scrollbar: Option<bool>,
    pub touch: Option<TouchMode>,
    pub notifications: Option<NotificationMethod>,
    pub math: Option<MathStyle>,
    pub mermaid: Option<MermaidStyle>,
    inline_images: Option<InlineImagesSetting>,
    pub flash_duration_ms: Option<u64>,
    pub which_key_delay_ms: Option<u64>,
    pub tooltips: Option<bool>,
    pub typewriter_ms_per_char: Option<u64>,
    pub mouse_scroll_lines: Option<u32>,
    pub scroll_card_lines: Option<u32>,
    pub always_collapsed: Option<Vec<String>>,
    pub show_thinking: Option<bool>,
    pub thinking_lines: Option<u32>,
    pub show_reminders: Option<bool>,
    pub show_decisions: Option<bool>,
    pub theme: Option<String>,
    pub theme_light: Option<String>,
    pub clock_format: Option<ClockFormat>,
    pub tool_output_lines: Option<ToolOutputLinesFile>,
    pub max_input_lines: Option<u32>,
    pub update_check: Option<bool>,
    pub update_channel: Option<UpdateChannel>,
    pub changelog_on_update: Option<bool>,
    pub default_mode: Option<StoredMode>,
}

impl UiFileConfig {
    fn merge(&mut self, overlay: UiFileConfig) {
        merge_option!(
            self,
            overlay,
            splash_animation,
            scrollbar,
            touch,
            math,
            mermaid,
            inline_images,
            notifications,
            flash_duration_ms,
            which_key_delay_ms,
            tooltips,
            typewriter_ms_per_char,
            mouse_scroll_lines,
            scroll_card_lines,
            always_collapsed,
            show_thinking,
            thinking_lines,
            show_reminders,
            show_decisions,
            theme,
            theme_light,
            clock_format,
            max_input_lines,
            update_check,
            update_channel,
            changelog_on_update,
            default_mode
        );
        match (self.tool_output_lines.as_mut(), overlay.tool_output_lines) {
            (Some(base), Some(over)) => base.merge(over),
            (None, Some(over)) => self.tool_output_lines = Some(over),
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MathStyle {
    #[default]
    Unicode,
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MermaidStyle {
    #[default]
    Unicode,
    Off,
}

/// How the transcript draws images: Kitty graphics placeholders where the
/// terminal answers for them, coloured block characters anywhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InlineImages {
    #[default]
    Auto,
    Kitty,
    /// Quadrant blocks, which every font has.
    Blocks,
    /// Sextant blocks, sharper, for fonts and terminals that draw them.
    Sextants,
    Off,
}

/// `inline_images` as written: a mode, or `false` for off and `true` for auto.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(untagged)]
enum InlineImagesSetting {
    Switch(bool),
    Mode(InlineImages),
}

impl From<InlineImagesSetting> for InlineImages {
    fn from(setting: InlineImagesSetting) -> Self {
        match setting {
            InlineImagesSetting::Switch(true) => Self::Auto,
            InlineImagesSetting::Switch(false) => Self::Off,
            InlineImagesSetting::Mode(mode) => mode,
        }
    }
}

/// Minimum severity written to the log file. `RUST_LOG` overrides it when set,
/// so a one-off debugging session needs no config edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotificationMethod {
    #[default]
    Auto,
    Osc9,
    Bell,
    Off,
}

/// What happens when a shell command does nothing a native tool could not.
///
/// A model that reaches for `rg` or `cat` out of habit pays for unstructured
/// text the native tool would have bounded, and the habit survives every
/// instruction in the prompt. Refusing the call is the only feedback that
/// reliably lands, so `Enforce` answers with the tool to use instead.
///
/// The detector only fires when every flag on the line has a native
/// equivalent, so a search the native tool genuinely cannot express is never
/// refused. `Annotate` logs the same finding without refusing, which is how a
/// run measures the habit before changing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellNativeRedirect {
    #[default]
    Enforce,
    Annotate,
    Off,
}

/// When Caudra re-sends the last request with no output to keep its prompt
/// cache from expiring. `Running` covers only a run that is still working, such
/// as one waiting on a long tool; `Idle` also covers the wait for the next
/// prompt after the run ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheWarmingMode {
    #[default]
    Off,
    Running,
    Idle,
}

impl CacheWarmingMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Running => "running",
            Self::Idle => "idle",
        }
    }
}

/// What reopening a session does with the work Caudra stopping cut off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResumeInterrupted {
    /// Resume it at once, unless a guard asks first.
    #[default]
    Auto,
    /// Offer to resume it.
    Ask,
    /// Leave it, saying only what was cut off.
    Off,
}

/// Where continual learning may write what it learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LearningTarget {
    /// The project's committed instruction file, shared with everyone.
    Project,
    /// The project's gitignored `AGENTS.local.md`, for this machine.
    Local,
    /// `~/.config/caudra/AGENTS.md`, for every project.
    Global,
    /// The project's memory notes.
    Memory,
}

impl LearningTarget {
    pub const ALL: [Self; 4] = [Self::Project, Self::Local, Self::Global, Self::Memory];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Local => "local",
            Self::Global => "global",
            Self::Memory => "memory",
        }
    }
}

pub fn resolve_task_background(
    background_supported: bool,
    requested: Option<bool>,
) -> Result<bool, &'static str> {
    match (background_supported, requested) {
        (false, Some(true)) => Err(TASK_SYNC_REQUIRED),
        (supported, requested) => Ok(supported && requested.unwrap_or(false)),
    }
}

pub fn resolve_shell_background(
    config: &AgentConfig,
    background_supported: bool,
    effective_timeout_secs: u64,
) -> bool {
    background_supported && effective_timeout_secs > config.shell_async_threshold_secs
}

/// Which GPT Image 2.5 model the hosted `image_generation` tool runs.
///
/// `Sunburst` is the more capable of the two and is built for editing
/// precision, which is what `image_generate` does whenever it is handed
/// reference images. `Flare` trades that for lower latency at the same price.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageModel {
    #[default]
    Sunburst,
    Flare,
}

impl ImageModel {
    /// Distinct from an `as_str` because the configured name and the wire id
    /// differ: users write `sunburst`, the backend wants the full model id.
    pub const fn model_id(self) -> &'static str {
        match self {
            Self::Sunburst => "gpt-image-2.5-sunburst",
            Self::Flare => "gpt-image-2.5-flare",
        }
    }
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct ToolOutputLinesFile {
    pub bash: Option<usize>,
    pub code: Option<usize>,
    pub task: Option<usize>,
    pub index: Option<usize>,
    pub grep: Option<usize>,
    pub read: Option<usize>,
    pub write: Option<usize>,
    pub web: Option<usize>,
    pub other: Option<usize>,
}

impl ToolOutputLinesFile {
    fn merge(&mut self, overlay: ToolOutputLinesFile) {
        merge_option!(
            self, overlay, bash, code, task, index, grep, read, write, web, other
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionBuffer {
    Tokens(u32),
    Percent(u8),
}

impl CompactionBuffer {
    pub fn resolve(self, context_window: u32) -> u32 {
        match self {
            Self::Tokens(n) => n,
            Self::Percent(p) => (u64::from(context_window) * u64::from(p) / 100) as u32,
        }
    }
}

impl Serialize for CompactionBuffer {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Tokens(n) => s.serialize_u32(*n),
            Self::Percent(p) => s.collect_str(&format_args!("{p}%")),
        }
    }
}

impl<'de> Deserialize<'de> for CompactionBuffer {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct BufferVisitor;

        impl serde::de::Visitor<'_> for BufferVisitor {
            type Value = CompactionBuffer;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(COMPACTION_BUFFER_EXPECTED)
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                u32::try_from(v)
                    .ok()
                    .filter(|n| *n >= MIN_COMPACTION_BUFFER)
                    .map(CompactionBuffer::Tokens)
                    .ok_or_else(|| {
                        E::custom(format!(
                            "compaction_buffer must be at least {MIN_COMPACTION_BUFFER} tokens"
                        ))
                    })
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                self.visit_u64(u64::try_from(v).unwrap_or(0))
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Self::Value, E> {
                s.strip_suffix('%')
                    .and_then(|n| n.trim().parse::<u8>().ok())
                    .filter(|p| (1..=MAX_COMPACTION_PERCENT).contains(p))
                    .map(CompactionBuffer::Percent)
                    .ok_or_else(|| {
                        E::custom(format!(
                            "invalid compaction_buffer {s:?}: expected {COMPACTION_BUFFER_EXPECTED}"
                        ))
                    })
            }
        }

        d.deserialize_any(BufferVisitor)
    }
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct AgentFileConfig {
    pub steering: Option<SteeringConfig>,
    pub messaging: MessagingFileConfig,
    pub system_prompt_profile: Option<String>,
    pub max_output_bytes: Option<usize>,
    pub max_output_lines: Option<usize>,
    pub compaction_buffer: Option<CompactionBuffer>,
    pub compaction_instructions: Option<String>,
    pub post_compaction_instructions: Option<String>,
    pub compaction_requirements: Option<bool>,
    pub background_reminder_turns: Option<u32>,
    pub todo_reminder: Option<bool>,
    pub work_nudge_compactions: Option<u32>,
    pub task_model_choice: Option<bool>,
    pub deferred_questions: Option<bool>,
    pub shell_async_threshold_secs: Option<u64>,
    pub shell_eta: Option<bool>,
    pub generate_titles: Option<bool>,
    pub summarize_memory: Option<bool>,
    pub stale_read_check: Option<bool>,
    pub tool_json_repair: Option<bool>,
    pub eager_batch_dispatch: Option<bool>,
    pub eager_tool_dispatch: Option<bool>,
    pub shell_output_filter: Option<bool>,
    pub shell_workdir_redirect: Option<bool>,
    pub shell_native_redirect: Option<ShellNativeRedirect>,
    pub image_model: Option<ImageModel>,
    pub cache_warming: Option<CacheWarmingMode>,
    pub resume_interrupted: Option<ResumeInterrupted>,
    pub resume_max_age_hours: Option<u64>,
    pub continual_learning: Option<bool>,
    pub learning_targets: Option<Vec<LearningTarget>>,
    pub disabled_tools: Option<Vec<String>>,
    /// `Some` for any `[agent.shell_env]` table, even an empty one, so a
    /// project layer that holds one is refused.
    pub shell_env: Option<ShellEnvFileConfig>,
}

impl AgentFileConfig {
    fn merge(&mut self, overlay: AgentFileConfig) {
        if let Some(steering) = overlay.steering {
            self.steering.get_or_insert_default().merge(steering);
        }
        if let Some(shell_env) = overlay.shell_env {
            self.shell_env.get_or_insert_default().merge(shell_env);
        }
        self.messaging.merge(overlay.messaging);
        merge_option!(
            self,
            overlay,
            system_prompt_profile,
            max_output_bytes,
            max_output_lines,
            compaction_buffer,
            compaction_instructions,
            post_compaction_instructions,
            compaction_requirements,
            background_reminder_turns,
            todo_reminder,
            work_nudge_compactions,
            task_model_choice,
            deferred_questions,
            shell_async_threshold_secs,
            shell_eta,
            generate_titles,
            summarize_memory,
            stale_read_check,
            tool_json_repair,
            eager_batch_dispatch,
            eager_tool_dispatch,
            shell_output_filter,
            shell_workdir_redirect,
            shell_native_redirect,
            image_model,
            cache_warming,
            resume_interrupted,
            resume_max_age_hours
        );
        // Restrictions only: an overlay can stop learning or narrow where it
        // writes, never start it or widen it, because it spends and it edits
        // files people share.
        if overlay.continual_learning == Some(false) {
            self.continual_learning = Some(false);
        }
        if let Some(overlay) = overlay.learning_targets {
            let narrowed = match self.learning_targets.take() {
                Some(base) => base.into_iter().filter(|t| overlay.contains(t)).collect(),
                None => overlay,
            };
            self.learning_targets = Some(narrowed);
        }
        // Restriction only, unlike every other list here: a project must not be
        // able to hand itself back a tool the global config took away.
        if let Some(overlay) = overlay.disabled_tools {
            self.disabled_tools.get_or_insert_default().extend(overlay);
        }
    }
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct MessagingFileConfig {
    pub inbound: Option<InboundPolicy>,
    pub inbound_per_minute: Option<usize>,
    pub sender_per_minute: Option<usize>,
    pub publish_per_minute: Option<usize>,
    pub max_fanout: Option<usize>,
    pub history_days: Option<u64>,
    pub history_max_messages: Option<u64>,
    #[serde(skip)]
    pub project_inbound: Option<InboundPolicy>,
    #[serde(skip)]
    pub project_inbound_per_minute: Option<usize>,
    #[serde(skip)]
    pub project_sender_per_minute: Option<usize>,
    #[serde(skip)]
    pub project_publish_per_minute: Option<usize>,
    #[serde(skip)]
    pub project_max_fanout: Option<usize>,
}

impl MessagingFileConfig {
    fn merge(&mut self, overlay: Self) {
        merge_option!(
            self,
            overlay,
            inbound,
            inbound_per_minute,
            sender_per_minute,
            publish_per_minute,
            max_fanout,
            history_days,
            history_max_messages
        );
        self.project_inbound_per_minute = lowest(
            self.project_inbound_per_minute,
            overlay.project_inbound_per_minute,
        );
        self.project_sender_per_minute = lowest(
            self.project_sender_per_minute,
            overlay.project_sender_per_minute,
        );
        self.project_publish_per_minute = lowest(
            self.project_publish_per_minute,
            overlay.project_publish_per_minute,
        );
        self.project_max_fanout = lowest(self.project_max_fanout, overlay.project_max_fanout);
        self.project_inbound = self.project_inbound.take().max(overlay.project_inbound);
        if let Some(floor) = &self.project_inbound {
            self.inbound = Some(self.inbound.take().unwrap_or_default().max(floor.clone()));
        }
    }

    fn resolve(self) -> MessagingConfig {
        let rate = |value: Option<usize>, ceiling: Option<usize>, default: usize| {
            value.unwrap_or(default).min(ceiling.unwrap_or(usize::MAX))
        };
        MessagingConfig {
            inbound: self.inbound.unwrap_or_default(),
            inbound_per_minute: rate(
                self.inbound_per_minute,
                self.project_inbound_per_minute,
                DEFAULT_INBOUND_PER_MINUTE,
            ),
            sender_per_minute: rate(
                self.sender_per_minute,
                self.project_sender_per_minute,
                DEFAULT_SENDER_PER_MINUTE,
            ),
            publish_per_minute: rate(
                self.publish_per_minute,
                self.project_publish_per_minute,
                DEFAULT_PUBLISH_PER_MINUTE,
            ),
            max_fanout: rate(self.max_fanout, self.project_max_fanout, DEFAULT_MAX_FANOUT),
            history_days: self.history_days.unwrap_or(DEFAULT_HISTORY_DAYS),
            history_max_messages: self
                .history_max_messages
                .unwrap_or(DEFAULT_HISTORY_MAX_MESSAGES),
            project_inbound: self.project_inbound,
        }
    }
}

/// The lower of two optional limits, where `None` sets no limit.
fn lowest(left: Option<usize>, right: Option<usize>) -> Option<usize> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left, right) => left.or(right),
    }
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderFileConfig {
    pub default_model: Option<String>,
    pub allowed_models: Option<Vec<String>>,
    pub excluded_models: Option<Vec<String>>,
    pub connect_timeout_secs: Option<u64>,
    pub stream_timeout_secs: Option<u64>,
}

impl ProviderFileConfig {
    fn merge(&mut self, overlay: ProviderFileConfig) {
        merge_option!(
            self,
            overlay,
            default_model,
            allowed_models,
            excluded_models,
            connect_timeout_secs,
            stream_timeout_secs
        );
    }
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct StorageFileConfig {
    pub max_log_bytes_mb: Option<u64>,
    pub max_log_files: Option<u32>,
    pub max_eager_load_mb: Option<u64>,
    pub log_level: Option<LogLevel>,
    pub input_history_size: Option<usize>,
    pub ephemeral: Option<bool>,
    pub retention: Option<RetentionFileConfig>,
    pub snapshots: Option<SnapshotsFileConfig>,
}

impl StorageFileConfig {
    fn merge(&mut self, overlay: StorageFileConfig) {
        merge_option!(
            self,
            overlay,
            max_log_bytes_mb,
            max_log_files,
            max_eager_load_mb,
            log_level,
            input_history_size,
            ephemeral
        );
        match (self.retention.as_mut(), overlay.retention) {
            (Some(base), Some(over)) => base.merge(over),
            (None, Some(over)) => self.retention = Some(over),
            _ => {}
        }
        match (self.snapshots.as_mut(), overlay.snapshots) {
            (Some(base), Some(over)) => base.merge(over),
            (None, Some(over)) => self.snapshots = Some(over),
            _ => {}
        }
    }
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct SnapshotsFileConfig {
    pub enabled: Option<bool>,
    pub max_bytes_mb: Option<u64>,
    pub max_files: Option<u64>,
    pub max_file_bytes_mb: Option<u64>,
}

impl SnapshotsFileConfig {
    fn merge(&mut self, overlay: SnapshotsFileConfig) {
        merge_option!(
            self,
            overlay,
            enabled,
            max_bytes_mb,
            max_files,
            max_file_bytes_mb
        );
    }
}

/// A project policy replaces the matching global policy whole, the way
/// `provider.allowed_models` does, so a project cannot inherit half a rule set.
#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionFileConfig {
    pub group_by: Option<GroupBy>,
    pub sweep_interval_hours: Option<u64>,
    pub trim: Option<KeepPolicy>,
    pub forget: Option<KeepPolicy>,
}

impl RetentionFileConfig {
    fn merge(&mut self, overlay: RetentionFileConfig) {
        merge_option!(self, overlay, group_by, sweep_interval_hours, trim, forget);
    }
}

/// One rule of an `[mcp.SERVER]` table. MCP rules name whole tools, so none
/// carries a scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPermissionRule {
    pub tool: ToolKey,
    pub effect: Effect,
}

/// The `[mcp]` table of a permissions file. The local loader and the remote
/// project context both read it through [`McpPermissions::parse`], so a rule
/// that works in one works in the other.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct McpPermissions {
    pub rules: Vec<McpPermissionRule>,
    pub defaults: HashMap<ToolKey, DefaultEffect>,
}

#[derive(Debug, Error)]
pub enum McpPermissionsError {
    #[error("[mcp] is not a table")]
    NotATable,
    #[error("[mcp.{0}] is not a table")]
    ServerNotATable(String),
    #[error("invalid MCP server name {0}; expected only alphanumeric characters and hyphens")]
    ServerName(String),
    #[error("[mcp.{server}].{key} entries must be strings")]
    NonStringEntry { server: String, key: String },
    #[error("[mcp.{server}].{key} must be an array of tool names, a tool name, or a boolean")]
    RuleType { server: String, key: String },
    #[error("invalid MCP tool name: {0}")]
    ToolName(#[from] ToolKeyParseError),
    #[error("invalid [mcp.{0}].default; expected allow, deny, or prompt")]
    Default(String),
    #[error("unknown key [mcp.{server}].{key}")]
    UnknownKey { server: String, key: String },
}

#[derive(Default)]
struct PermissionsFileConfig {
    default: Option<DefaultEffect>,
    tools: HashMap<String, ToolPermissions>,
    mcp: McpPermissions,
    loaded_file: Option<(PathBuf, String)>,
}

impl PermissionsFileConfig {
    /// Keys beside the `[TOOL]` and `[mcp.SERVER]` tables.
    const FIELDS: &[ConfigField] = &[ConfigField {
        name: "default",
        ty: "string",
        default: ConfigValue::Str("prompt"),
        min: None,
        max: None,
        env: None,
        choices: &[],
        description: "What a call that no rule matches does: `allow`, `deny`, or `prompt`. `allow` acts as `prompt` and waits in /permissions for review, and a project cannot weaken a global `deny`",
    }];
}

impl ToolPermissions {
    const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: "allow",
            ty: "bool | string[]",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Scopes the tool may use without asking, such as `[\"git status *\"]`, or `true` for every call. Only shell allows grant access, and a project shell allow waits until you trust the project policy. Other allows wait in /permissions for review",
        },
        ConfigField {
            name: "ask",
            ty: "bool | string[]",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Scopes that always ask, such as `[\"git push *\"]`, or `true` for every call",
        },
        ConfigField {
            name: "deny",
            ty: "bool | string[]",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Scopes the tool may never use, such as `[\"rm -rf *\"]`, or `true` for every call. A deny in either file blocks the whole call",
        },
        ConfigField {
            name: "default",
            ty: "string",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "What a call of this tool that no rule matches does: `allow`, `deny`, or `prompt`. Unset follows the top-level `default`",
        },
    ];
}

impl McpPermissions {
    const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: "allow",
            ty: "bool | string | string[]",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Tools to allow without asking: a list of names, one name, `\"*\"` for every tool, or `true` for every tool. MCP allows wait in /permissions for review",
        },
        ConfigField {
            name: "ask",
            ty: "bool | string | string[]",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Tools that always ask, in the same forms as `allow`",
        },
        ConfigField {
            name: "deny",
            ty: "bool | string | string[]",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Tools the model may never call, in the same forms as `allow`. `false` in any of the three adds nothing",
        },
        ConfigField {
            name: "default",
            ty: "string",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "What a call to a tool of this server that no rule matches does: `allow`, `deny`, or `prompt`. Unset follows the top-level `default`",
        },
    ];
}

/// Renames the tool tables whose tool was renamed: under its old name a rule
/// would silently stop applying. `[mcp]` holds the rules of MCP servers, not
/// the retired `mcp` tool, so it keeps its name.
fn upgrade_permissions_from_1(upgrade: &mut Upgrade<'_>) -> Result<(), MigrationError> {
    upgrade.because(SOURCE_CODE_WHY);
    for (old, new) in SOURCE_CODE_TOOL_RENAMES {
        upgrade.rename_key(&[], old, new)?;
    }
    upgrade.because(PYTHON_EXECUTION_RENAMED).rename_key(
        &[],
        "python_execution",
        CODE_TOOL_NAME,
    )?;
    upgrade
        .because(BASH_RENAMED)
        .rename_key(&[], "bash", "shell")
}

impl<'de> Deserialize<'de> for PermissionsFileConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut table = toml::Table::deserialize(deserializer)?;
        ConfigVersion::<PERMISSIONS_VERSION>::take(&mut table).map_err(serde::de::Error::custom)?;
        let default = table
            .get("default")
            .map(|value| {
                DefaultEffect::deserialize(value.clone()).map_err(serde::de::Error::custom)
            })
            .transpose()?;

        let mut tools = HashMap::new();
        let mut mcp = McpPermissions::default();

        for (k, v) in table.iter() {
            if k == "default" {
                continue;
            }
            if k == "mcp" {
                mcp = McpPermissions::parse(v).map_err(serde::de::Error::custom)?;
            } else {
                if k != "*" && !is_valid_wire_name(k) {
                    return Err(serde::de::Error::custom(format!(
                        "invalid native tool section [{k}]"
                    )));
                }
                let tp = v.clone().try_into::<ToolPermissions>().map_err(|error| {
                    serde::de::Error::custom(format!("invalid tool section [{k}]: {error}"))
                })?;
                tools.insert(k.clone(), tp);
            }
        }

        Ok(Self {
            default,
            tools,
            mcp,
            loaded_file: None,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolPermissions {
    allow: Option<ScopeSet>,
    ask: Option<ScopeSet>,
    deny: Option<ScopeSet>,
    default: Option<DefaultEffect>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ScopeSet {
    All(bool),
    Scopes(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultEffect {
    Allow,
    Deny,
    #[default]
    Prompt,
}

impl From<Effect> for DefaultEffect {
    fn from(e: Effect) -> Self {
        match e {
            Effect::Allow => DefaultEffect::Allow,
            Effect::Ask => DefaultEffect::Prompt,
            Effect::Deny => DefaultEffect::Deny,
        }
    }
}

#[derive(Debug, Clone)]
pub enum PermissionTarget {
    Global,
    Project(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ToolKey {
    Wildcard,
    Native(Arc<str>),
    McpServer { server: Arc<str> },
    McpTool { server: Arc<str>, tool: Arc<str> },
}

/// NOTE: `ToolKey` deliberately does not implement `serde::Deserialize`.
/// Use `ToolKey::parse(&str)` at deserialization boundaries — it performs
/// validation (wire format, server name, length) that a blanket Deserialize
/// would skip. All current deserialization paths go through `parse`.
impl serde::Serialize for ToolKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Check if a name matches the LLM wire format: `^[a-zA-Z0-9_-]{1,64}$`.
/// Tool names with dots, over 64 chars, or special characters are rejected.
pub fn is_valid_wire_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl ToolKey {
    /// Parse a qualified tool name into a `ToolKey`.
    ///
    /// Returns `Err` for malformed input (empty names, empty server/tool parts,
    /// tool names that don't match the wire format `^[a-zA-Z0-9_-]{1,64}$`).
    /// Use this at config/dispatch boundaries where input is untrusted.
    pub fn parse(name: &str) -> Result<Self, ToolKeyParseError> {
        if name.is_empty() {
            return Err(ToolKeyParseError::EmptyName);
        }
        if name == "*" {
            return Ok(Self::Wildcard);
        }
        match name.split_once('.') {
            Some(("", _)) | Some((_, "")) => {
                Err(ToolKeyParseError::MalformedParts(name.to_string()))
            }
            Some((server, "*")) => {
                if !is_valid_server_name(server) {
                    return Err(ToolKeyParseError::InvalidServerName(server.to_string()));
                }
                Ok(Self::McpServer {
                    server: server.into(),
                })
            }
            Some((server, tool)) => {
                if !is_valid_server_name(server) {
                    return Err(ToolKeyParseError::InvalidServerName(server.to_string()));
                }
                if !is_valid_wire_name(tool) {
                    return Err(ToolKeyParseError::InvalidToolName(tool.to_string()));
                }
                // Wire format is server__tool — check total length fits LLM API limits
                let wire_len = server.len() + 2 + tool.len();
                if wire_len > 64 {
                    return Err(ToolKeyParseError::WireNameTooLong {
                        server: server.to_string(),
                        tool: tool.to_string(),
                        len: wire_len,
                    });
                }
                Ok(Self::McpTool {
                    server: server.into(),
                    tool: tool.into(),
                })
            }
            None => {
                if !is_valid_wire_name(name) {
                    return Err(ToolKeyParseError::InvalidToolName(name.to_string()));
                }
                Ok(Self::Native(name.into()))
            }
        }
    }

    /// Create a `ToolKey` from a known-valid native tool name.
    ///
    /// # Panics
    ///
    /// Panics if `name` is empty or contains dots. Use `ToolKey::parse` for
    /// untrusted input or MCP tool names.
    pub fn native(name: &str) -> Self {
        match name {
            "*" => Self::Wildcard,
            _ => {
                assert!(!name.is_empty(), "native tool name must not be empty");
                assert!(
                    !name.contains('.'),
                    "native tool name must not contain dots: {name:?} - use ToolKey::parse for MCP tools"
                );
                Self::Native(name.into())
            }
        }
    }

    pub fn is_mcp(&self) -> bool {
        matches!(self, Self::McpServer { .. } | Self::McpTool { .. })
    }
}

impl std::fmt::Display for ToolKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wildcard => write!(f, "*"),
            Self::Native(name) => write!(f, "{name}"),
            Self::McpServer { server } => write!(f, "{server}.*"),
            Self::McpTool { server, tool } => write!(f, "{server}.{tool}"),
        }
    }
}

/// Error returned when a tool key string fails validation.
#[derive(Debug, thiserror::Error)]
pub enum ToolKeyParseError {
    #[error("tool name is empty")]
    EmptyName,
    #[error("malformed tool key: empty server or tool part in {0:?}")]
    MalformedParts(String),
    #[error("invalid server name {0:?}: must match [a-zA-Z0-9-]{{1,64}}")]
    InvalidServerName(String),
    #[error("invalid tool name {0:?}: must match [a-zA-Z0-9_-]{{1,64}}")]
    InvalidToolName(String),
    #[error("wire name {server}__{tool} is {len} chars, max 64")]
    WireNameTooLong {
        server: String,
        tool: String,
        len: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRule {
    pub tool: ToolKey,
    pub scope: Option<String>,
    pub effect: Effect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionSource {
    Global,
    Project,
    Conversation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionReviewKind {
    Rule,
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionReviewCandidate {
    pub source: PermissionSource,
    pub kind: PermissionReviewKind,
    pub tool: Option<ToolKey>,
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PermissionsConfig {
    pub default: DefaultEffect,
    pub tool_defaults: HashMap<ToolKey, DefaultEffect>,
    /// Active rules: all denies and asks, plus validated global shell allows.
    pub rules: Vec<PermissionRule>,
    /// Validated project shell allows stay separate until the project is trusted.
    pub project_allow_rules: Vec<PermissionRule>,
    /// Project denies and asks bind trust to the effective project allow boundary.
    pub project_restrictive_rules: Vec<PermissionRule>,
    pub review_candidates: Vec<PermissionReviewCandidate>,
    pub loaded_sources: Vec<LoadedPermissionSource>,
    pub yolo: bool,
}

#[derive(Debug, Clone)]
pub struct LoadedPermissionSource {
    rule: PermissionRule,
    path: PathBuf,
    content_digest: String,
}

impl LoadedPermissionSource {
    pub fn rule(&self) -> &PermissionRule {
        &self.rule
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn content_digest(&self) -> &str {
        &self.content_digest
    }
}

#[derive(Clone)]
pub struct Config {
    pub decisions: DecisionsConfig,
    pub always_yolo: bool,
    pub always_auto: bool,
    pub always_fast: bool,
    pub always_thinking: Option<StoredThinking>,
    pub ui: UiConfig,
    pub agent: AgentConfig,
    pub provider: ProviderConfig,
    pub storage: StorageConfig,
    pub telemetry: TelemetryConfig,
    pub worktrees: WorktreesConfig,
    pub automations: AutomationsConfig,
    pub autopilot: AutopilotConfig,
    pub web: WebConfig,
    pub permissions: PermissionsConfig,
    pub plugins: PluginsConfig,
    pub websearch: WebsearchConfig,
}

/// Whether the pointer driving Caudra is a finger. Tri-state because the only
/// signal is an environment variable, and a Bluetooth mouse in Termux or an SSH
/// session out of one both make the guess wrong in opposite directions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum TouchMode {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "on")]
    On,
    #[serde(rename = "off")]
    Off,
}

impl TouchMode {
    pub fn enabled(self, detect: impl FnOnce() -> bool) -> bool {
        match self {
            Self::Auto => detect(),
            Self::On => true,
            Self::Off => false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum ClockFormat {
    #[serde(rename = "12h")]
    Hour12,
    #[serde(rename = "24h")]
    Hour24,
    #[default]
    #[serde(rename = "system")]
    System,
}

#[derive(Debug, Clone, ConfigSection)]
#[config(section = "ui")]
pub struct UiConfig {
    #[config(default = true, desc = "Show splash animation on startup")]
    pub splash_animation: bool,

    #[config(default = true, desc = "Show vertical scrollbar in scrollable areas")]
    pub scrollbar: bool,

    #[config(
        default = TouchMode::Auto,
        ty = "string",
        default_doc = "auto",
        desc = "Touch-friendly pointer handling: auto, on, or off. Widens the scrollbar's hit zone so a finger can tap it, scrolls one line per wheel event instead of mouse_scroll_lines, and leaves text selection to the terminal. Auto detects Termux around Caudra itself, which SSH does not carry, so set this to on when reaching Caudra from a phone over SSH",
        choices = ["auto", "on", "off"],
    )]
    pub touch: TouchMode,

    #[config(
        default = NotificationMethod::Auto,
        ty = "string",
        default_doc = "auto",
        desc = "Terminal notification method: auto, osc9, bell, or off. Auto reports OSC 7501 program status when supported, otherwise uses OSC 9 or BEL. Native Herdr or Orca reporting takes precedence in their panes. Explicit osc9, bell, and off skip program status detection",
        choices = ["auto", "osc9", "bell", "off"],
    )]
    pub notifications: NotificationMethod,

    #[config(
        default = MathStyle::Unicode,
        ty = "string",
        default_doc = "unicode",
        desc = "How LaTeX maths renders: unicode (approximate with Unicode) or raw (show the LaTeX source)",
        choices = ["unicode", "raw"],
    )]
    pub math: MathStyle,

    #[config(
        default = MermaidStyle::Unicode,
        ty = "string",
        default_doc = "unicode",
        desc = "How mermaid flowcharts render: unicode (draw them with box-drawing characters) or off (leave the fence as code)",
        choices = ["unicode", "off"],
    )]
    pub mermaid: MermaidStyle,

    #[config(
        default = InlineImages::Auto,
        ty = "string",
        default_doc = "auto",
        desc = "How the transcript draws images that `view_image` reads, `image_generate` saves, and markdown names with `![alt](path)`: auto (Kitty graphics where the terminal supports them, coloured quadrant blocks anywhere else), kitty, blocks, sextants (sharper blocks, for fonts and terminals that draw Unicode 13 sextants, such as Ghostty, kitty, WezTerm, and foot), or off (show only the caption; `false` also turns them off). Markdown images are read from local files only and never fetched",
        choices = ["auto", "kitty", "blocks", "sextants", "off"],
    )]
    pub inline_images: InlineImages,

    #[config(
        default = DEFAULT_FLASH_DURATION_MS,
        desc = "Duration of ordinary status-bar messages (ms). Confirmation prompts use a fixed 3-second window"
    )]
    pub flash_duration_ms: u64,

    #[config(
        default = DEFAULT_WHICH_KEY_DELAY_MS,
        desc = "How long Ctrl+X waits before listing the chords it can still reach (ms). 0 shows the list at once"
    )]
    pub which_key_delay_ms: u64,

    #[config(
        default = true,
        desc = "Show a tooltip when the mouse rests on a footer chip, a composer, queue or permission prompt control, an icon, a cut-off name or a paste token"
    )]
    pub tooltips: bool,

    #[config(default = DEFAULT_TYPEWRITER_MS_PER_CHAR, desc = "Typewriter effect speed (ms/char)")]
    pub typewriter_ms_per_char: u64,

    #[config(default = DEFAULT_MOUSE_SCROLL_LINES, min = MIN_MOUSE_SCROLL_LINES, desc = "Lines per mouse wheel scroll")]
    pub mouse_scroll_lines: u32,

    #[config(
        default = DEFAULT_SCROLL_CARD_LINES,
        desc = "Rows of body a shell, code or task card draws. The window follows new output while it sits at the bottom and pauses when scrolled up. Click inside a window to give it the wheel, which passes back to the transcript at either edge, and drag the bar in its last column to move it directly. `0` turns scrolling off, restoring the `ui.tool_output_lines` budget for those tools. A write is never windowed: it is drawn whole at any setting, as the file it created or as the diff of what it replaced"
    )]
    pub scroll_card_lines: u32,

    #[config(
        ty = "string[]",
        default = "DEFAULT_ALWAYS_COLLAPSED.iter().map(|t| (*t).to_owned()).collect()",
        default_doc = "[\"file_read\", \"file_glob\", \"file_grep\", \"file_index\", \"webfetch\", \"source_code_context\", \"source_code_refs\", \"history.search\", \"history.read\", \"history.sessions\", \"tool_output\", \"skill\", \"execution_environment\", \"list_sessions\", \"read_topic\", \"automation\", \"memory_read\", \"plan_read\", \"workflow_read\", \"task_control_read\"]",
        desc = "Tools whose card never opens on its own: the call stays a single row in every view mode until you click it. A server-qualified name still matches, so `file_read` also covers `mcp_File_read`. A `code` snippet's history calls are named `history.search`, `history.read`, and `history.sessions`, and its `host.environment` call is `execution_environment`. `memory_read`, `plan_read`, `workflow_read`, and `task_control_read` cover only the calls of `memory`, `plan`, `workflow`, and `task_control` that read, and leave the calls that change something open. The bare tool name covers all of its calls. Set to `[]` to opt out"
    )]
    pub always_collapsed: Vec<String>,

    #[config(default = DEFAULT_MAX_INPUT_LINES, min = MIN_MAX_INPUT_LINES, desc = "Maximum visible input lines")]
    pub max_input_lines: u32,

    #[config(
        default = true,
        desc = "Show full model reasoning live and persisted. Turn this off to start every reasoning block collapsed behind a Thinking or Thought header that can be clicked to expand"
    )]
    pub show_thinking: bool,

    #[config(
        default = DEFAULT_THINKING_LINES,
        desc = "Rows of body an open reasoning block draws. The window follows the reasoning while it streams and pauses when scrolled up, and a footer reports how much sits above and below. Click inside a window to give it the wheel, which passes back to the transcript at either edge, and drag the bar in its last column to move it directly. Click the footer to follow again. A finished block rests on its last rows until you move it. `0` draws every block whole"
    )]
    pub thinking_lines: u32,

    #[config(
        default = true,
        desc = "Show the messages Caudra writes into the conversation on your behalf: standing reminders, goal check-ins, nudges, and continuations. Each is one dim row that expands on click to the exact text the model was sent. Turn this off to keep the transcript to the conversation alone"
    )]
    pub show_reminders: bool,

    #[config(
        default = true,
        desc = "Show what the decision engine changed or is deciding. A decision about a tool call is a chip on the call's card, such as `◆ reranking…` while the engine works and `◆ reranked` once it answers, and clicking the chip shows the candidates and scores behind it under the card's header. A decision about a turn, such as a skill suggestion or a skipped goal check, is a row that expands on click. Failures say the default applied, and `shadow` marks what a feature in shadow mode would have done. The model never sees them"
    )]
    pub show_decisions: bool,

    #[config(default = ClockFormat::System, ty = "String", default_doc = "system", desc = "Clock format for timestamps: \"12h\", \"24h\", or \"system\" (follow the OS preference, 24h when unknown)", choices = ["12h", "24h", "system"])]
    pub clock_format: ClockFormat,

    #[config(
        default = true,
        env = "CAUDRA_ENABLE_UPDATE_CHECK",
        desc = "Check GitHub releases in the background at interactive startup and show a banner above the prompt when a newer version is available. Uses a shared 24-hour cache and never installs automatically. Set false to disable"
    )]
    pub update_check: bool,

    #[config(default = UpdateChannel::Auto, ty = "string", default_doc = "auto", desc = "Release channel: `auto` follows stable from a stable build and preview from a prerelease, including graduation to stable. `stable` excludes prereleases. `preview` includes prereleases and stable releases", choices = ["auto", "stable", "preview"])]
    pub update_channel: UpdateChannel,

    #[config(
        default = true,
        desc = "After an update, show what is new in the releases since the one you last ran, once, the next time the terminal UI starts with nothing else to show. `/changelog` shows the notes at any time. Set false to keep them to `/changelog`"
    )]
    pub changelog_on_update: bool,

    #[config(
        default = StoredMode::Plan,
        ty = "string",
        default_doc = "plan",
        desc = "Mode a new terminal UI session opens in: `plan`, where the agent writes only the plan file until you switch, or `build`. A resumed session reopens in the mode it was left in, and `/new` in an idle tab keeps that tab's mode. `--print` and ACP always start in build",
        choices = ["plan", "build"],
    )]
    pub default_mode: StoredMode,

    #[config(
        ty = "string",
        default = "None",
        desc = "Name of the color theme to load at startup, overriding the theme you last picked with `/theme`. Unset keeps your last pick"
    )]
    pub theme: Option<String>,

    #[config(
        ty = "string",
        default = "None",
        desc = "Light theme to pair with `theme`, in place of the one from the pairing table or for a theme that has no pair. `theme` becomes the dark half"
    )]
    pub theme_light: Option<String>,

    #[config(skip, default = "ToolOutputLines::default()")]
    pub tool_output_lines: ToolOutputLines,
}

impl UiConfig {
    pub fn flash_duration(&self) -> Duration {
        Duration::from_millis(self.flash_duration_ms)
    }

    pub fn which_key_delay(&self) -> Duration {
        Duration::from_millis(self.which_key_delay_ms)
    }

    fn from_file(f: UiFileConfig) -> Self {
        Self {
            splash_animation: f.splash_animation.unwrap_or(true),
            scrollbar: f.scrollbar.unwrap_or(true),
            touch: f.touch.unwrap_or_default(),
            notifications: f.notifications.unwrap_or_default(),
            math: f.math.unwrap_or_default(),
            mermaid: f.mermaid.unwrap_or_default(),
            inline_images: f.inline_images.map(InlineImages::from).unwrap_or_default(),
            flash_duration_ms: f.flash_duration_ms.unwrap_or(DEFAULT_FLASH_DURATION_MS),
            which_key_delay_ms: f.which_key_delay_ms.unwrap_or(DEFAULT_WHICH_KEY_DELAY_MS),
            tooltips: f.tooltips.unwrap_or(true),
            typewriter_ms_per_char: f
                .typewriter_ms_per_char
                .unwrap_or(DEFAULT_TYPEWRITER_MS_PER_CHAR),
            mouse_scroll_lines: f.mouse_scroll_lines.unwrap_or(DEFAULT_MOUSE_SCROLL_LINES),
            scroll_card_lines: f.scroll_card_lines.unwrap_or(DEFAULT_SCROLL_CARD_LINES),
            always_collapsed: f.always_collapsed.unwrap_or_else(|| {
                DEFAULT_ALWAYS_COLLAPSED
                    .iter()
                    .map(|tool| (*tool).to_owned())
                    .collect()
            }),
            max_input_lines: f.max_input_lines.unwrap_or(DEFAULT_MAX_INPUT_LINES),
            show_thinking: f.show_thinking.unwrap_or(true),
            thinking_lines: f.thinking_lines.unwrap_or(DEFAULT_THINKING_LINES),
            show_reminders: f.show_reminders.unwrap_or(true),
            show_decisions: f.show_decisions.unwrap_or(true),
            clock_format: f.clock_format.unwrap_or_default(),
            update_check: f.update_check.unwrap_or(true),
            update_channel: f.update_channel.unwrap_or_default(),
            changelog_on_update: f.changelog_on_update.unwrap_or(true),
            default_mode: f.default_mode.unwrap_or(StoredMode::Plan),
            theme: f.theme,
            theme_light: f.theme_light,
            tool_output_lines: ToolOutputLines::from_file(f.tool_output_lines),
        }
    }

    pub fn validate_all(&self) -> Result<(), ConfigError> {
        self.validate()?;
        self.tool_output_lines.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolOutputLines {
    pub bash: usize,
    pub code: usize,
    pub task: usize,
    pub index: usize,
    pub grep: usize,
    pub read: usize,
    pub write: usize,
    pub web: usize,
    pub other: usize,
}

impl ToolOutputLines {
    pub const DEFAULT: Self = Self {
        bash: 5,
        code: 5,
        task: 12,
        index: 3,
        grep: 3,
        read: 3,
        write: 7,
        web: 3,
        other: 3,
    };

    pub const FIELD_DEFAULTS: &[(&'static str, usize)] = &[
        ("bash", Self::DEFAULT.bash),
        ("code", Self::DEFAULT.code),
        ("task", Self::DEFAULT.task),
        ("index", Self::DEFAULT.index),
        ("grep", Self::DEFAULT.grep),
        ("read", Self::DEFAULT.read),
        ("write", Self::DEFAULT.write),
        ("web", Self::DEFAULT.web),
        ("other", Self::DEFAULT.other),
    ];

    /// Which tools each budget covers, named as they are registered. The docs
    /// read it and the tests hold `get` to it, so a renamed tool cannot go on
    /// quietly falling through to `other` the way `file_grep` once did.
    pub const FIELD_TOOLS: &[(&'static str, &'static [&'static str])] = &[
        ("bash", &["shell"]),
        ("code", &["code"]),
        ("task", &["task", "task_control"]),
        (
            "index",
            &["file_index", "source_code_context", "source_code_refs"],
        ),
        ("grep", &["file_grep", "file_glob"]),
        ("read", &["file_read"]),
        (
            "write",
            &[
                "file_write",
                "file_edit",
                "file_apply_patch",
                "image_generate",
                "memory",
                "plan",
            ],
        ),
        ("web", &["webfetch", "websearch"]),
        (
            "other",
            &[
                "automation",
                "batch",
                "execution_environment",
                "list_sessions",
                "publish_message",
                "question",
                "read_topic",
                "send_message",
                "skill",
                "todo_write",
                "tool_output",
                "view_image",
                "work_assignment",
                "workflow",
            ],
        ),
    ];

    fn from_file(f: Option<ToolOutputLinesFile>) -> Self {
        let d = Self::DEFAULT;
        let f = f.unwrap_or_default();
        Self {
            bash: f.bash.unwrap_or(d.bash),
            code: f.code.unwrap_or(d.code),
            task: f.task.unwrap_or(d.task),
            index: f.index.unwrap_or(d.index),
            grep: f.grep.unwrap_or(d.grep),
            read: f.read.unwrap_or(d.read),
            write: f.write.unwrap_or(d.write),
            web: f.web.unwrap_or(d.web),
            other: f.other.unwrap_or(d.other),
        }
    }

    fn fields(&self) -> [(&'static str, usize); 9] {
        [
            ("bash", self.bash),
            ("code", self.code),
            ("task", self.task),
            ("index", self.index),
            ("grep", self.grep),
            ("read", self.read),
            ("write", self.write),
            ("web", self.web),
            ("other", self.other),
        ]
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        for (name, value) in self.fields() {
            check(
                "ui.tool_output_lines",
                name,
                value as u64,
                MIN_TOOL_OUTPUT_LINES as u64,
            )?;
        }
        Ok(())
    }

    pub fn get(&self, name: &str) -> usize {
        match name {
            "bash" | "shell" => self.bash,
            "code" => self.code,
            "task" | "task_control" => self.task,
            "index" | "file_index" | "source_code_context" | "source_code_refs" => self.index,
            "file_grep" | "file_glob" | "grep" | "glob" => self.grep,
            "file_read" | "read" => self.read,
            "memory" | "plan" => self.write,
            name if FILE_WRITE_TOOLS.contains(&name) => self.write,
            "webfetch" | "websearch" => self.web,
            _ => self.other,
        }
    }
}

impl Default for ToolOutputLines {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Ordered from least to most restrictive so project layers can only tighten it.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboundPolicy {
    Accept,
    #[default]
    Auto,
    Hold,
    Refuse,
}

#[derive(Debug, Clone, PartialEq, Eq, ConfigSection, Serialize, Deserialize)]
#[config(section = "agent.messaging")]
#[serde(default, deny_unknown_fields)]
pub struct MessagingConfig {
    #[config(
        default = InboundPolicy::Auto,
        ty = "string",
        default_doc = "auto",
        desc = "Inbound cross-session messages: `auto` accepts only compatible trusted peers, `accept` allows wider delivery, `hold` requires approval, `refuse` rejects messages. Project settings may only tighten policy: accept < auto < hold < refuse. Needs `experimental.cross_session_messaging`; accepting messages can start billable turns",
        choices = ["accept", "auto", "hold", "refuse"],
    )]
    pub inbound: InboundPolicy,

    #[config(
        default = DEFAULT_INBOUND_PER_MINUTE,
        min = MIN_MESSAGE_RATE,
        desc = "Most peer messages a session admits per minute from all senders together. Project settings may only lower it"
    )]
    pub inbound_per_minute: usize,

    #[config(
        default = DEFAULT_SENDER_PER_MINUTE,
        min = MIN_MESSAGE_RATE,
        desc = "Most peer messages a session admits per minute from one sending session. Project settings may only lower it"
    )]
    pub sender_per_minute: usize,

    #[config(
        default = DEFAULT_PUBLISH_PER_MINUTE,
        min = MIN_MESSAGE_RATE,
        desc = "Most topic and broadcast publications a session sends per minute. Project settings may only lower it"
    )]
    pub publish_per_minute: usize,

    #[config(
        default = DEFAULT_MAX_FANOUT,
        min = MIN_MESSAGE_RATE,
        desc = "Most live sessions one topic or broadcast publication reaches. Extra recipients are skipped and counted. Project settings may only lower it"
    )]
    pub max_fanout: usize,

    #[config(
        default = DEFAULT_HISTORY_DAYS,
        min = MIN_HISTORY_LIMIT,
        desc = "Days the shared message history keeps a message. The newest message on each topic outlives this until `history_max_messages` evicts it. Global config only"
    )]
    pub history_days: u64,

    #[config(
        default = DEFAULT_HISTORY_MAX_MESSAGES,
        min = MIN_HISTORY_LIMIT,
        desc = "Most messages the shared message history keeps; the oldest go first. Global config only"
    )]
    pub history_max_messages: u64,

    /// The strictest explicit project policy. No project policy leaves session
    /// controls free to choose `Accept`, even when the global default is `Auto`.
    #[config(skip, default = "None")]
    #[serde(skip)]
    pub project_inbound: Option<InboundPolicy>,
}

#[derive(Debug, Clone, ConfigSection, Serialize)]
#[config(section = "agent")]
pub struct AgentConfig {
    // Sharing immutable policy keeps tool contexts from cloning the full model map.
    #[config(skip, default = "Arc::default()")]
    pub steering: Arc<SteeringConfig>,

    #[config(skip, default = "MessagingConfig::default()")]
    pub messaging: MessagingConfig,

    #[config(skip, default = "ShellEnvConfig::default()")]
    pub shell_env: ShellEnvConfig,

    #[config(
        ty = "String",
        default = "None",
        default_doc = "builtin",
        desc = "Default user system prompt profile from the system-prompts config directory"
    )]
    pub system_prompt_profile: Option<String>,

    #[config(default = DEFAULT_MAX_OUTPUT_BYTES, min = MIN_OUTPUT_BYTES, desc = "Host-enforced default max tool-result size (bytes)")]
    pub max_output_bytes: usize,

    #[config(default = DEFAULT_MAX_OUTPUT_LINES, min = MIN_OUTPUT_LINES, desc = "Host-enforced default max tool-result lines")]
    pub max_output_lines: usize,

    #[config(
        default = "None",
        ty = "u32 | string",
        default_varies = "20%, or 10% when the model's window excludes output. On a window that includes output, at least the model's output allowance, up to half the window",
        desc = "Context reserved for compaction: token count or percent of the context window (e.g. \"20%\")"
    )]
    pub compaction_buffer: Option<CompactionBuffer>,

    #[config(
        ty = "String",
        default = "None",
        desc = "Extra instructions appended to the compaction summary prompt"
    )]
    pub compaction_instructions: Option<String>,

    #[config(
        ty = "String",
        default = "None",
        desc = "Extra instructions the agent receives after any compaction (e.g. re-read plan.md)"
    )]
    pub post_compaction_instructions: Option<String>,

    #[config(
        default = true,
        desc = "Append a `# User requirements` section to every compaction summary: what the user asked for, constrained, and decided, read from their own messages and answered questions across every earlier compaction, and extracted by the Extract model so the conversation model never sees the request"
    )]
    pub compaction_requirements: bool,

    #[config(
        default = DEFAULT_BACKGROUND_REMINDER_TURNS,
        desc = "Committed main-agent response groups between unchanged active background-work reminders; 0 disables periodic refresh only, not state-change or post-compaction reminders"
    )]
    pub background_reminder_turns: u32,

    #[config(
        default = true,
        desc = "Before the main agent hands control back with pending or in-progress todos and no background work running, remind it once per run, repeating the full todo list, to verify the work and update the list"
    )]
    pub todo_reminder: bool,

    #[config(
        default = DEFAULT_WORK_NUDGE_COMPACTIONS,
        desc = "Compactions within one request, with no work started since it, before Caudra reminds the agent to act: make a change or delegate one in build mode, write the plan or ask in plan mode, report in a read-only task. Repeated at every compaction after that. 0 disables"
    )]
    pub work_nudge_compactions: u32,

    #[config(
        default = true,
        desc = "Let the main agent choose a model and reasoning level for each `task` call, such as when you ask it to run work on a named model. When off, `task` has no `model` or `thinking` argument and tasks run on the Subagent model or a profile's pin"
    )]
    pub task_model_choice: bool,

    #[config(
        default = true,
        desc = "Let the main agent ask a `question` without waiting for the answer, so it keeps working while the form waits for you. The answer reaches it later, and you can also put a waiting question off with F4. When off, every question waits for its answer"
    )]
    pub deferred_questions: bool,

    #[config(default = DEFAULT_SHELL_ASYNC_THRESHOLD_SECS, min = MIN_SHELL_ASYNC_THRESHOLD_SECS, desc = "Requested shell timeout above which a shell call returns an admission receipt instead of waiting; independent of the enforced execution deadline")]
    pub shell_async_threshold_secs: u64,

    #[config(
        default = true,
        desc = "Show an ETA on a shell card and in `/shells`, from a small model trained on this machine's past shell runs. It only labels the command: timeouts and delivery never change. When off, nothing is predicted or recorded"
    )]
    pub shell_eta: bool,

    #[config(
        default = true,
        desc = "Name a new session by summarizing its first prompt with the Title model"
    )]
    pub generate_titles: bool,

    #[config(
        default = true,
        desc = "Summarize memory notes in the background with the Memory model, so the memory view in the system prompt stays within its budget. When off, new notes reach the view only as titles and `memory search` still finds them"
    )]
    pub summarize_memory: bool,

    #[config(
        default = true,
        desc = "Block a write to a file that changed on disk since it was read, and point a failed edit or patch at the change"
    )]
    pub stale_read_check: bool,

    #[config(
        default = true,
        desc = "Repair malformed tool JSON syntax locally, with one bounded isolated model fallback; independent of eager dispatch"
    )]
    pub tool_json_repair: bool,

    #[config(
        default = true,
        desc = "Start tools and batch children as soon as their complete arguments arrive, instead of waiting for the whole message"
    )]
    pub eager_tool_dispatch: bool,

    #[config(
        default = true,
        desc = "Filter completed model-facing shell output with built-in rules"
    )]
    pub shell_output_filter: bool,

    #[config(
        default = true,
        desc = "Refuse shell commands with a leading literal `cd ... &&` in favor of the shell `workdir` parameter. Set to `false` to disable this nudge independently of `shell_native_redirect`"
    )]
    pub shell_workdir_redirect: bool,

    #[config(
        default = ShellNativeRedirect::Enforce,
        ty = "string",
        default_doc = "enforce",
        desc = "What happens when a shell command only re-implements a native tool, such as bare `rg` or `cat`: `enforce` refuses it and names the tool to call instead, `annotate` only logs the finding, `off` disables the check. A command using any flag the native tool cannot express is never affected",
        choices = ["enforce", "annotate", "off"],
    )]
    pub shell_native_redirect: ShellNativeRedirect,

    #[config(
        default = ImageModel::Sunburst,
        ty = "string",
        default_doc = "sunburst",
        desc = "GPT Image 2.5 model behind `image_generate`: `sunburst` is the most capable and the better editor, `flare` is faster at the same price",
        choices = ["sunburst", "flare"],
    )]
    pub image_model: ImageModel,

    #[config(
        default = CacheWarmingMode::Off,
        ty = "string",
        default_doc = "off",
        desc = "Keep the prompt cache from expiring by re-sending the last request with no output, on the Anthropic API only: `running` while a run waits on a long tool, subagent, question or permission prompt, `idle` also for up to 30 minutes after it ends, `off` never. Each refresh is billed as a cache read; global config only",
        choices = ["off", "running", "idle"],
    )]
    pub cache_warming: CacheWarmingMode,

    #[config(
        default = ResumeInterrupted::Auto,
        ty = "string",
        default_doc = "auto",
        desc = "What reopening a session does with work Caudra stopping cut off (a crash, a kill, or quitting mid-turn): `auto` resumes the main turn and its tasks at once, `ask` offers to, `off` only says what was interrupted. `auto` asks instead when a resume already crashed once, when the work is older than `resume_max_age_hours`, or when automatic wakes are suppressed",
        choices = ["auto", "ask", "off"],
    )]
    pub resume_interrupted: ResumeInterrupted,

    #[config(
        default = DEFAULT_RESUME_MAX_AGE_HOURS,
        min = MIN_RESUME_MAX_AGE_HOURS,
        desc = "Hours after which `resume_interrupted = \"auto\"` asks before resuming interrupted work instead of resuming it"
    )]
    pub resume_max_age_hours: u64,

    #[config(
        default = true,
        desc = "Learn from your conversations in the background: after enough new prompts, the Learn model reads them and keeps durable conventions, setup facts and preferences in the instruction files and memory notes. A project config can turn it off, never on"
    )]
    pub continual_learning: bool,

    #[config(
        ty = "string[]",
        default = "LearningTarget::ALL.to_vec()",
        default_doc = "[\"project\", \"local\", \"global\", \"memory\"]",
        desc = "Where continual learning may write: `project` (the committed AGENTS.md), `local` (AGENTS.local.md), `global` (~/.config/caudra/AGENTS.md), `memory` (memory notes). A project list can only narrow the global one",
        choices = ["project", "local", "global", "memory"],
    )]
    pub learning_targets: Vec<LearningTarget>,

    /// Set on the learner's own runs, whose `history` calls stay in the
    /// session's project whatever scope they ask for.
    #[config(skip, default = false)]
    pub history_project_only: bool,

    /// Set on the learner's own runs, which judge notes themselves and must
    /// not be steered by a recall of them.
    #[config(skip, default = false)]
    pub no_memory_recall: bool,

    #[config(skip, default = false)]
    pub no_rtk: bool,

    #[config(skip, default = "None")]
    pub max_turns: Option<u32>,

    #[config(skip, default = "Vec::new()")]
    pub allowed_tools: Vec<String>,

    #[config(
        ty = "string[]",
        default = "Vec::new()",
        default_doc = "[]",
        desc = "Tools to withhold from the model: built-in names, `server.tool`, or `server.*` for a whole MCP server. A project list extends the global one"
    )]
    pub disabled_tools: Vec<String>,

    #[config(skip, default = DEFAULT_INDEX_MAX_FILE_SIZE_MB)]
    pub index_max_file_size_mb: usize,

    #[config(skip, default = DEFAULT_TASK_MAX_CONCURRENT)]
    pub task_max_concurrent: usize,

    #[config(skip, default = "BuiltinSkills::default()")]
    pub builtin_skills: BuiltinSkills,

    /// The process's startup snapshot of `[experimental]`; never read from a
    /// settings layer, so neither a project nor `init.lua` can opt in.
    #[config(skip, default = "FeatureFlags::NONE")]
    #[serde(skip)]
    pub features: FeatureFlags,
}

impl AgentConfig {
    fn from_file(
        file: AgentFileConfig,
        no_rtk: bool,
        disabled_tools: Vec<String>,
        index_max_file_size_mb: usize,
        task_max_concurrent: usize,
        builtin_skills: BuiltinSkills,
        shell_env: ShellEnvConfig,
    ) -> Self {
        Self {
            no_rtk,
            steering: Arc::new(file.steering.unwrap_or_default()),
            messaging: file.messaging.resolve(),
            shell_env,
            system_prompt_profile: file
                .system_prompt_profile
                .filter(|profile| profile != "builtin"),
            max_output_bytes: file.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES),
            max_output_lines: file.max_output_lines.unwrap_or(DEFAULT_MAX_OUTPUT_LINES),
            compaction_buffer: file.compaction_buffer,
            compaction_instructions: file.compaction_instructions,
            post_compaction_instructions: file.post_compaction_instructions,
            compaction_requirements: file.compaction_requirements.unwrap_or(true),
            task_model_choice: file.task_model_choice.unwrap_or(true),
            deferred_questions: file.deferred_questions.unwrap_or(true),
            shell_async_threshold_secs: file
                .shell_async_threshold_secs
                .unwrap_or(DEFAULT_SHELL_ASYNC_THRESHOLD_SECS),
            shell_eta: file.shell_eta.unwrap_or(true),
            background_reminder_turns: file
                .background_reminder_turns
                .unwrap_or(DEFAULT_BACKGROUND_REMINDER_TURNS),
            todo_reminder: file.todo_reminder.unwrap_or(true),
            work_nudge_compactions: file
                .work_nudge_compactions
                .unwrap_or(DEFAULT_WORK_NUDGE_COMPACTIONS),
            generate_titles: file.generate_titles.unwrap_or(true),
            summarize_memory: file.summarize_memory.unwrap_or(true),
            stale_read_check: file.stale_read_check.unwrap_or(true),
            tool_json_repair: file.tool_json_repair.unwrap_or(true),
            eager_tool_dispatch: file
                .eager_tool_dispatch
                .or(file.eager_batch_dispatch)
                .unwrap_or(true),
            shell_output_filter: !no_rtk && file.shell_output_filter.unwrap_or(true),
            shell_workdir_redirect: file.shell_workdir_redirect.unwrap_or(true),
            shell_native_redirect: file.shell_native_redirect.unwrap_or_default(),
            image_model: file.image_model.unwrap_or_default(),
            cache_warming: file.cache_warming.unwrap_or_default(),
            resume_interrupted: file.resume_interrupted.unwrap_or_default(),
            resume_max_age_hours: file
                .resume_max_age_hours
                .unwrap_or(DEFAULT_RESUME_MAX_AGE_HOURS),
            continual_learning: file.continual_learning.unwrap_or(true),
            learning_targets: file
                .learning_targets
                .unwrap_or_else(|| LearningTarget::ALL.to_vec()),
            history_project_only: false,
            no_memory_recall: false,
            max_turns: None,
            allowed_tools: Vec::new(),
            disabled_tools,
            index_max_file_size_mb,
            task_max_concurrent,
            builtin_skills,
            features: FeatureFlags::NONE,
        }
    }
}

/// The builtin skills `[plugins.skill]` offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuiltinSkills {
    pub plugin_dev: bool,
    pub workflow_dev: bool,
    pub automation_dev: bool,
    pub docs: bool,
    pub pstack: bool,
}

impl Default for BuiltinSkills {
    fn default() -> Self {
        Self {
            plugin_dev: DEFAULT_SKILL_PLUGIN_DEV,
            workflow_dev: DEFAULT_SKILL_WORKFLOW_DEV,
            automation_dev: DEFAULT_SKILL_AUTOMATION_DEV,
            docs: DEFAULT_SKILL_DOCS,
            pstack: DEFAULT_SKILL_PSTACK,
        }
    }
}

#[derive(Debug, Clone, ConfigSection)]
#[config(section = "provider", fields_only)]
pub struct ProviderConfig {
    #[config(
        ty = "String",
        desc = "Default model identifier (e.g. `anthropic/claude-sonnet-4-6`)"
    )]
    pub default_model: Option<String>,

    #[config(
        ty = "string[]",
        default_doc = "[]",
        desc = "Glob patterns for permitted qualified model specs; empty permits all models"
    )]
    pub allowed_models: Vec<String>,

    #[config(
        ty = "string[]",
        default_doc = "[]",
        desc = "Glob patterns for excluded qualified model specs; exclusions take precedence"
    )]
    pub excluded_models: Vec<String>,

    #[config(skip)]
    pub model_policy: ModelPolicy,

    #[config(key = "connect_timeout_secs", ty = "u64", default = DEFAULT_CONNECT_TIMEOUT_SECS,
             min = MIN_CONNECT_TIMEOUT_SECS, val = "self.connect_timeout.as_secs()",
             desc = "HTTP connect timeout (seconds)")]
    pub connect_timeout: Duration,

    #[config(key = "stream_timeout_secs", ty = "u64", default = DEFAULT_STREAM_TIMEOUT_SECS,
             min = MIN_STREAM_TIMEOUT_SECS, val = "self.stream_timeout.as_secs()",
             desc = "Longest the server may send nothing before the request is abandoned (seconds)")]
    pub stream_timeout: Duration,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            default_model: None,
            allowed_models: Vec::new(),
            excluded_models: Vec::new(),
            model_policy: ModelPolicy::allow_all(),
            connect_timeout: Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS),
            stream_timeout: Duration::from_secs(DEFAULT_STREAM_TIMEOUT_SECS),
        }
    }
}

impl ProviderConfig {
    fn from_file(f: ProviderFileConfig) -> Result<Self, ConfigError> {
        let allowed_models = f.allowed_models.unwrap_or_default();
        let excluded_models = f.excluded_models.unwrap_or_default();
        let model_policy = ModelPolicy::new(&allowed_models, &excluded_models)?;
        Ok(Self {
            default_model: f.default_model,
            allowed_models,
            excluded_models,
            model_policy,
            connect_timeout: Duration::from_secs(
                f.connect_timeout_secs
                    .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS),
            ),
            stream_timeout: Duration::from_secs(
                f.stream_timeout_secs.unwrap_or(DEFAULT_STREAM_TIMEOUT_SECS),
            ),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ModelPolicy {
    allowed: GlobSet,
    excluded: GlobSet,
    has_allowed_models: bool,
}

impl Default for ModelPolicy {
    fn default() -> Self {
        Self::allow_all()
    }
}

impl ModelPolicy {
    fn allow_all() -> Self {
        Self::new(&[], &[]).expect("empty model policy is valid")
    }

    pub fn new(allowed_models: &[String], excluded_models: &[String]) -> Result<Self, ConfigError> {
        Ok(Self {
            allowed: Self::compile("allowed_models", allowed_models)?,
            excluded: Self::compile("excluded_models", excluded_models)?,
            has_allowed_models: !allowed_models.is_empty(),
        })
    }

    fn compile(field: &'static str, patterns: &[String]) -> Result<GlobSet, ConfigError> {
        let mut globset = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = GlobBuilder::new(pattern)
                .literal_separator(false)
                .build()
                .map_err(|source| ConfigError::InvalidModelPattern {
                    field,
                    pattern: pattern.clone(),
                    source,
                })?;
            globset.add(glob);
        }
        globset
            .build()
            .map_err(|source| ConfigError::InvalidModelPattern {
                field,
                pattern: String::new(),
                source,
            })
    }

    pub fn is_restrictive(&self) -> bool {
        self.has_allowed_models || !self.excluded.is_empty()
    }

    pub fn allows(&self, spec: &str) -> bool {
        (!self.has_allowed_models || self.allowed.is_match(spec)) && !self.excluded.is_match(spec)
    }
}

#[derive(Debug, Clone, Copy, ConfigSection)]
#[config(section = "storage", fields_only)]
pub struct StorageConfig {
    #[config(key = "max_log_bytes_mb", ty = "u64", default = DEFAULT_MAX_LOG_BYTES_MB,
             min = MIN_MAX_LOG_BYTES_MB, val = "self.max_log_bytes / (1024 * 1024)",
             desc = "Max total log size (MB)")]
    pub max_log_bytes: u64,

    #[config(default = DEFAULT_MAX_LOG_FILES, min = MIN_MAX_LOG_FILES,
             desc = "Max number of log files to keep")]
    pub max_log_files: u32,

    #[config(key = "max_eager_load_mb", ty = "u64", default = DEFAULT_MAX_EAGER_LOAD_MB,
             min = MIN_MAX_EAGER_LOAD_MB, val = "self.max_eager_load_bytes / (1024 * 1024)",
             env = "CAUDRA_MAX_EAGER_LOAD_MB",
             desc = "Largest session Caudra will hydrate when opening one (MB), counted in uncompressed payload bytes rather than disk or memory. A session past this refuses to load; trim it or raise this")]
    pub max_eager_load_bytes: u64,

    #[config(default = DEFAULT_LOG_LEVEL, ty = "string", default_doc = "info",
             desc = "Minimum severity written to the log file: trace, debug, info, warn, or error. RUST_LOG overrides it",
             choices = ["trace", "debug", "info", "warn", "error"])]
    pub log_level: LogLevel,

    #[config(default = DEFAULT_INPUT_HISTORY_SIZE, min = MIN_INPUT_HISTORY_SIZE,
             desc = "Number of input history entries to retain")]
    pub input_history_size: usize,

    #[config(default = DEFAULT_EPHEMERAL,
             desc = "Store session data in a temporary directory removed when Caudra exits")]
    pub ephemeral: bool,

    #[config(skip)]
    pub retention: RetentionConfig,

    #[config(skip)]
    pub snapshots: SnapshotsConfig,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            max_log_bytes: DEFAULT_MAX_LOG_BYTES_MB * 1024 * 1024,
            max_log_files: DEFAULT_MAX_LOG_FILES,
            max_eager_load_bytes: DEFAULT_MAX_EAGER_LOAD_MB * 1024 * 1024,
            log_level: DEFAULT_LOG_LEVEL,
            input_history_size: DEFAULT_INPUT_HISTORY_SIZE,
            ephemeral: DEFAULT_EPHEMERAL,
            retention: RetentionConfig::default(),
            snapshots: SnapshotsConfig::default(),
        }
    }
}

impl StorageConfig {
    fn from_file(f: StorageFileConfig) -> Self {
        Self {
            max_log_bytes: f.max_log_bytes_mb.unwrap_or(DEFAULT_MAX_LOG_BYTES_MB) * 1024 * 1024,
            max_log_files: f.max_log_files.unwrap_or(DEFAULT_MAX_LOG_FILES),
            max_eager_load_bytes: f.max_eager_load_mb.unwrap_or(DEFAULT_MAX_EAGER_LOAD_MB)
                * 1024
                * 1024,
            log_level: f.log_level.unwrap_or(DEFAULT_LOG_LEVEL),
            input_history_size: f.input_history_size.unwrap_or(DEFAULT_INPUT_HISTORY_SIZE),
            ephemeral: f.ephemeral.unwrap_or(DEFAULT_EPHEMERAL),
            retention: RetentionConfig::from_file(f.retention.unwrap_or_default()),
            snapshots: SnapshotsConfig::from_file(f.snapshots.unwrap_or_default()),
        }
    }
}

/// Whether each tool call's file changes are recorded for file revert, and
/// what one change record may cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotsConfig {
    pub enabled: bool,
    /// Also the size each workspace's change store is trimmed to.
    pub max_bytes: u64,
    pub max_files: u64,
    pub max_file_bytes: u64,
}

impl Default for SnapshotsConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_SNAPSHOTS_ENABLED,
            max_bytes: DEFAULT_SNAPSHOT_MAX_BYTES_MB * 1024 * 1024,
            max_files: DEFAULT_SNAPSHOT_MAX_FILES,
            max_file_bytes: DEFAULT_SNAPSHOT_MAX_FILE_BYTES_MB * 1024 * 1024,
        }
    }
}

impl SnapshotsConfig {
    pub const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: "enabled",
            ty: "bool",
            default: ConfigValue::Bool(DEFAULT_SNAPSHOTS_ENABLED),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Record each tool call's file changes so file revert can undo them, locally and remotely. `false` turns recording and file revert off and keeps records already made. `--no-snapshots` overrides this for one run",
        },
        ConfigField {
            name: "max_bytes_mb",
            ty: "u64",
            default: ConfigValue::U64(DEFAULT_SNAPSHOT_MAX_BYTES_MB),
            min: Some(MIN_SNAPSHOT_LIMIT),
            max: None,
            env: None,
            choices: &[],
            description: "Most file data one change record may cover, and the size each workspace's change store is trimmed to. A record over it is refused and its call runs unrecorded. Values above the store's limit are lowered to it, and locally that limit is the default",
        },
        ConfigField {
            name: "max_files",
            ty: "u64",
            default: ConfigValue::U64(DEFAULT_SNAPSHOT_MAX_FILES),
            min: Some(MIN_SNAPSHOT_LIMIT),
            max: None,
            env: None,
            choices: &[],
            description: "Most files one change record may cover, counted after ignore rules. A record over it is refused and its call runs unrecorded. Values above the store's limit are lowered to it, and locally that limit is the default",
        },
        ConfigField {
            name: "max_file_bytes_mb",
            ty: "u64",
            default: ConfigValue::U64(DEFAULT_SNAPSHOT_MAX_FILE_BYTES_MB),
            min: Some(MIN_SNAPSHOT_LIMIT),
            max: None,
            env: None,
            choices: &[],
            description: "Largest file a change record stores. A larger file is left unrecorded, and a file revert across a call that changed it stops with a conflict. Values above the store's limit are lowered to it, and locally that limit is the default",
        },
    ];

    pub fn validate(&self) -> Result<(), ConfigError> {
        for (field, value) in [
            ("max_bytes_mb", self.max_bytes / BYTES_PER_MB),
            ("max_files", self.max_files),
            ("max_file_bytes_mb", self.max_file_bytes / BYTES_PER_MB),
        ] {
            check(SNAPSHOTS_SECTION, field, value, MIN_SNAPSHOT_LIMIT)?;
        }
        Ok(())
    }

    /// The size each workspace's change store is cleaned down to. Load
    /// refuses zero, which would evict every record, so the default stands in
    /// only for a config that skipped validation.
    pub fn store_budget(&self) -> NonZeroU64 {
        NonZeroU64::new(self.max_bytes).unwrap_or(DEFAULT_STORE_BUDGET)
    }

    fn from_file(f: SnapshotsFileConfig) -> Self {
        Self {
            enabled: f.enabled.unwrap_or(DEFAULT_SNAPSHOTS_ENABLED),
            max_bytes: f.max_bytes_mb.unwrap_or(DEFAULT_SNAPSHOT_MAX_BYTES_MB) * 1024 * 1024,
            max_files: f.max_files.unwrap_or(DEFAULT_SNAPSHOT_MAX_FILES),
            max_file_bytes: f
                .max_file_bytes_mb
                .unwrap_or(DEFAULT_SNAPSHOT_MAX_FILE_BYTES_MB)
                * 1024
                * 1024,
        }
    }
}

/// Which sessions the background sweep trims and forgets. Policies use the
/// `restic forget` vocabulary; an empty `forget` policy disables deletion
/// rather than deleting everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionConfig {
    pub group_by: GroupBy,
    /// Zero disables the background sweep. The CLI commands still work.
    pub sweep_interval_hours: u64,
    pub trim: KeepPolicy,
    pub forget: KeepPolicy,
}

impl RetentionConfig {
    pub const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: "group_by",
            ty: "string",
            default: ConfigValue::Str("directory"),
            min: None,
            max: None,
            env: None,
            choices: &["directory", "none"],
            description: "Evaluate policies per working directory (`directory`) or across every session (`none`)",
        },
        ConfigField {
            name: "sweep_interval_hours",
            ty: "u64",
            default: ConfigValue::U64(DEFAULT_RETENTION_SWEEP_INTERVAL_HOURS),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Hours between background sweeps. A sweep reclaims freed space, and applies `trim` and `forget` when they are set. `0` disables the sweep; `caudra storage` commands still work",
        },
        ConfigField {
            name: "trim",
            ty: "table",
            default: ConfigValue::Toml("{}"),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Sessions outside this policy lose file revert, tool output files, archives, and large rich outputs but stay resumable. Empty means never trim automatically",
        },
        ConfigField {
            name: "forget",
            ty: "table",
            default: ConfigValue::Toml("{}"),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Sessions outside this policy are deleted. Empty means never delete automatically",
        },
    ];

    fn from_file(f: RetentionFileConfig) -> Self {
        Self {
            group_by: f.group_by.unwrap_or_default(),
            sweep_interval_hours: f
                .sweep_interval_hours
                .unwrap_or(DEFAULT_RETENTION_SWEEP_INTERVAL_HOURS),
            trim: f.trim.unwrap_or_default(),
            forget: f.forget.unwrap_or_default(),
        }
    }
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self::from_file(RetentionFileConfig::default())
    }
}

/// OpenTelemetry export settings. Every field also has an `OTEL_*` (or
/// `CAUDRA_*`) environment variable, which wins over what is set here.
///
/// Fields stay optional: `caudra-otel` owns the defaults, resolution and
/// validation, so the meaning of "unset" is decided in one place.
#[derive(Deserialize, Debug, Clone, ConfigSection)]
#[serde(default, deny_unknown_fields)]
#[config(section = "telemetry")]
pub struct TelemetryConfig {
    #[config(default = None, ty = "bool", default_doc = "false",
             env = "CAUDRA_ENABLE_TELEMETRY",
             desc = "Master switch")]
    pub enabled: Option<bool>,

    #[config(default = None, ty = "string", default_doc = "none",
             env = "OTEL_METRICS_EXPORTER",
             desc = "Where metrics go: `otlp`, `console`, `none`, or a comma-separated mix")]
    pub metrics_exporter: Option<String>,

    #[config(default = None, ty = "string", default_doc = "none",
             env = "OTEL_LOGS_EXPORTER",
             desc = "Where events go: `otlp`, `console`, `none`, or a comma-separated mix")]
    pub logs_exporter: Option<String>,

    #[config(default = None, ty = "string",
             env = "OTEL_EXPORTER_OTLP_PROTOCOL",
             desc = "OTLP protocol: `grpc`, `http/protobuf`, or `http/json`. Required when an exporter is `otlp`")]
    pub protocol: Option<String>,

    #[config(default = None, ty = "string",
             env = "OTEL_EXPORTER_OTLP_ENDPOINT",
             desc = "Collector endpoint. HTTP appends `/v1/metrics` and `/v1/logs`")]
    pub endpoint: Option<String>,

    #[config(default = None, ty = "table", default_doc = "{}",
             env = "OTEL_EXPORTER_OTLP_HEADERS",
             desc = "Extra headers sent with every export")]
    pub headers: Option<BTreeMap<String, String>>,

    #[config(default = None, ty = "integer", default_doc = "10000",
             env = "OTEL_EXPORTER_OTLP_TIMEOUT",
             desc = "Per-export request timeout (ms)")]
    pub timeout_ms: Option<u64>,

    #[config(default = None, ty = "string", default_doc = "none",
             env = "OTEL_EXPORTER_OTLP_COMPRESSION",
             desc = "Payload compression: `gzip` or `none`")]
    pub compression: Option<String>,

    #[config(default = None, ty = "string",
             env = "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
             desc = "Metrics-only protocol override")]
    pub metrics_protocol: Option<String>,

    #[config(default = None, ty = "string",
             env = "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
             desc = "Metrics-only endpoint, used verbatim with no path appended")]
    pub metrics_endpoint: Option<String>,

    #[config(default = None, ty = "table", default_doc = "{}",
             env = "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
             desc = "Metrics-only headers, merged over `headers`")]
    pub metrics_headers: Option<BTreeMap<String, String>>,

    #[config(default = None, ty = "integer",
             env = "OTEL_EXPORTER_OTLP_METRICS_TIMEOUT",
             desc = "Metrics-only request timeout (ms)")]
    pub metrics_timeout_ms: Option<u64>,

    #[config(default = None, ty = "string",
             env = "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
             desc = "Logs-only protocol override")]
    pub logs_protocol: Option<String>,

    #[config(default = None, ty = "string",
             env = "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
             desc = "Logs-only endpoint, used verbatim with no path appended")]
    pub logs_endpoint: Option<String>,

    #[config(default = None, ty = "table", default_doc = "{}",
             env = "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
             desc = "Logs-only headers, merged over `headers`")]
    pub logs_headers: Option<BTreeMap<String, String>>,

    #[config(default = None, ty = "integer",
             env = "OTEL_EXPORTER_OTLP_LOGS_TIMEOUT",
             desc = "Logs-only request timeout (ms)")]
    pub logs_timeout_ms: Option<u64>,

    #[config(default = None, ty = "integer", default_doc = "60000",
             env = "OTEL_METRIC_EXPORT_INTERVAL",
             desc = "How often metrics are exported (ms)")]
    pub metrics_interval_ms: Option<u64>,

    #[config(default = None, ty = "integer", default_doc = "30000",
             env = "OTEL_METRIC_EXPORT_TIMEOUT",
             desc = "Deadline for one metrics export, retries included (ms)")]
    pub metrics_export_timeout_ms: Option<u64>,

    #[config(default = None, ty = "integer", default_doc = "5000",
             env = "OTEL_LOGS_EXPORT_INTERVAL, OTEL_BLRP_SCHEDULE_DELAY",
             desc = "How often queued events are flushed (ms)")]
    pub logs_interval_ms: Option<u64>,

    #[config(default = None, ty = "integer", default_doc = "2048",
             env = "OTEL_BLRP_MAX_QUEUE_SIZE",
             desc = "Event queue capacity. Events are dropped and counted when it is full")]
    pub logs_max_queue_size: Option<usize>,

    #[config(default = None, ty = "integer", default_doc = "512",
             env = "OTEL_BLRP_MAX_EXPORT_BATCH_SIZE",
             desc = "Maximum events per export request")]
    pub logs_max_export_batch_size: Option<usize>,

    #[config(default = None, ty = "integer", default_doc = "30000",
             env = "OTEL_BLRP_EXPORT_TIMEOUT",
             desc = "Deadline for one events export, retries included (ms)")]
    pub logs_export_timeout_ms: Option<u64>,

    #[config(default = None, ty = "string", default_doc = "delta",
             env = "OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE",
             desc = "Metric temporality: `delta` or `cumulative`")]
    pub metrics_temporality: Option<String>,

    #[config(default = None, ty = "string", default_doc = "caudra",
             env = "OTEL_SERVICE_NAME",
             desc = "`service.name` on the exported resource")]
    pub service_name: Option<String>,

    #[config(default = None, ty = "table", default_doc = "{}",
             env = "OTEL_RESOURCE_ATTRIBUTES",
             desc = "Extra resource attributes, your place for team or environment labels")]
    pub resource_attributes: Option<BTreeMap<String, String>>,

    #[config(default = None, ty = "bool", default_doc = "true",
             env = "OTEL_METRICS_INCLUDE_SESSION_ID",
             desc = "Attach `session.id` to metrics. Turn off to keep metric cardinality low")]
    pub metrics_include_session_id: Option<bool>,

    #[config(default = None, ty = "bool", default_doc = "false",
             env = "OTEL_METRICS_INCLUDE_VERSION",
             desc = "Attach `app.version` to metrics")]
    pub metrics_include_version: Option<bool>,

    #[config(default = None, ty = "bool", default_doc = "false",
             env = "OTEL_LOG_USER_PROMPTS",
             desc = "Include prompt text in `caudra.user_prompt` events. Off by default")]
    pub log_user_prompts: Option<bool>,

    #[config(default = None, ty = "bool", default_doc = "false",
             env = "OTEL_LOG_TOOL_DETAILS",
             desc = "Include tool input in `caudra.tool_result` events. Off by default")]
    pub log_tool_details: Option<bool>,

    #[config(default = None, ty = "integer", default_doc = "10240",
             env = "CAUDRA_OTEL_CONTENT_MAX_LENGTH",
             desc = "Character cap on any logged prompt or tool input")]
    pub content_max_length: Option<usize>,
}

impl TelemetryConfig {
    fn merge(&mut self, overlay: TelemetryConfig) {
        merge_option!(
            self,
            overlay,
            enabled,
            metrics_exporter,
            logs_exporter,
            protocol,
            endpoint,
            headers,
            timeout_ms,
            compression,
            metrics_protocol,
            metrics_endpoint,
            metrics_headers,
            metrics_timeout_ms,
            logs_protocol,
            logs_endpoint,
            logs_headers,
            logs_timeout_ms,
            metrics_interval_ms,
            metrics_export_timeout_ms,
            logs_interval_ms,
            logs_max_queue_size,
            logs_max_export_batch_size,
            logs_export_timeout_ms,
            metrics_temporality,
            service_name,
            resource_attributes,
            metrics_include_session_id,
            metrics_include_version,
            log_user_prompts,
            log_tool_details,
            content_max_length
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeBackend {
    /// The terminal host (Herdr or Orca) inside its pane, git everywhere else.
    #[default]
    Auto,
    Git,
}

#[derive(Deserialize, Debug, Clone, ConfigSection)]
#[serde(default, deny_unknown_fields)]
#[config(section = "worktrees")]
pub struct WorktreesConfig {
    #[config(default = None, ty = "string", default_doc = "auto",
             desc = "What creates and removes worktrees for `/worktree`: `auto` uses Herdr inside a Herdr pane, Orca inside an Orca terminal, and git elsewhere, `git` always runs git",
             choices = ["auto", "git"])]
    pub backend: Option<WorktreeBackend>,

    #[config(default = None, ty = "string", default_varies = "`<data dir>/worktrees`",
             desc = "Where git-created worktrees go, as `<directory>/<repository>/<branch>`. A leading `~/` is your home directory")]
    pub directory: Option<String>,
}

impl WorktreesConfig {
    fn merge(&mut self, overlay: WorktreesConfig) {
        merge_option!(self, overlay, backend, directory);
    }
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct AutomationsFileConfig {
    pub turns_per_hour: Option<u32>,
    pub max_unattended_turns: Option<u32>,
    pub allow_private_network: Option<bool>,
}

impl AutomationsFileConfig {
    fn merge(&mut self, overlay: Self) {
        merge_option!(
            self,
            overlay,
            turns_per_hour,
            max_unattended_turns,
            allow_private_network
        );
    }
}

/// Session-wide limits on automations, set only in the global config so a
/// repository cannot raise them or open private networks to `http()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationsConfig {
    pub turns_per_hour: u32,
    pub max_unattended_turns: Option<u32>,
    pub allow_private_network: bool,
}

impl AutomationsConfig {
    pub const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: TURNS_PER_HOUR_FIELD,
            ty: "u32",
            default: ConfigValue::U64(DEFAULT_AUTOMATION_TURNS_PER_HOUR as u64),
            min: Some(MIN_AUTOMATION_TURNS_PER_HOUR as u64),
            max: Some(MAX_AUTOMATION_TURNS_PER_HOUR as u64),
            env: None,
            choices: &[],
            description: "Most turns automations may start in one session per rolling hour, shared by all of its automations",
        },
        ConfigField {
            name: MAX_UNATTENDED_TURNS_FIELD,
            ty: "u32",
            default: ConfigValue::Unset,
            min: Some(MIN_MAX_UNATTENDED_TURNS as u64),
            max: Some(MAX_MAX_UNATTENDED_TURNS as u64),
            env: None,
            choices: &[],
            description: "Stop automation-started turns after this many since the last human input. Human input resets the count, and unset means no cap",
        },
        ConfigField {
            name: ALLOW_PRIVATE_NETWORK_FIELD,
            ty: "bool",
            default: ConfigValue::Bool(DEFAULT_ALLOW_PRIVATE_NETWORK),
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Let `http()` in automations reach loopback and private network hosts. Without it, automations reach public hosts only",
        },
    ];

    pub fn validate(&self) -> Result<(), ConfigError> {
        check_range(
            AUTOMATIONS_SECTION,
            TURNS_PER_HOUR_FIELD,
            self.turns_per_hour,
            MIN_AUTOMATION_TURNS_PER_HOUR,
            MAX_AUTOMATION_TURNS_PER_HOUR,
        )?;
        if let Some(turns) = self.max_unattended_turns {
            check_range(
                AUTOMATIONS_SECTION,
                MAX_UNATTENDED_TURNS_FIELD,
                turns,
                MIN_MAX_UNATTENDED_TURNS,
                MAX_MAX_UNATTENDED_TURNS,
            )?;
        }
        Ok(())
    }

    fn from_file(f: AutomationsFileConfig) -> Self {
        Self {
            turns_per_hour: f
                .turns_per_hour
                .unwrap_or(DEFAULT_AUTOMATION_TURNS_PER_HOUR),
            max_unattended_turns: f.max_unattended_turns,
            allow_private_network: f
                .allow_private_network
                .unwrap_or(DEFAULT_ALLOW_PRIVATE_NETWORK),
        }
    }
}

impl Default for AutomationsConfig {
    fn default() -> Self {
        Self::from_file(AutomationsFileConfig::default())
    }
}

#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct AutopilotFileConfig {
    pub max_continuations: Option<u32>,
    pub max_cost: Option<f64>,
    pub max_minutes: Option<u32>,
    pub error_retries: Option<u32>,
}

impl AutopilotFileConfig {
    fn merge(&mut self, overlay: Self) {
        merge_option!(
            self,
            overlay,
            max_continuations,
            max_cost,
            max_minutes,
            error_retries
        );
    }
}

/// Session-wide limits on autopilot, set only in the global config so a
/// repository cannot change them or start continuous runs.
#[derive(Debug, Clone, PartialEq)]
pub struct AutopilotConfig {
    /// `None` when the configured limit is
    /// [`UNCAPPED_AUTOPILOT_CONTINUATIONS`], so only the other limits end an
    /// engagement.
    pub max_continuations: Option<u32>,
    pub max_cost: Option<f64>,
    pub max_minutes: Option<u32>,
    pub error_retries: u32,
}

impl AutopilotConfig {
    pub const FIELDS: &[ConfigField] = &[
        ConfigField {
            name: MAX_CONTINUATIONS_FIELD,
            ty: "u32",
            default: ConfigValue::U64(DEFAULT_AUTOPILOT_MAX_CONTINUATIONS as u64),
            min: Some(MIN_AUTOPILOT_MAX_CONTINUATIONS as u64),
            max: Some(MAX_AUTOPILOT_MAX_CONTINUATIONS as u64),
            env: None,
            choices: &[],
            description: "Most continuations autopilot may send per engagement before it ends. `0` removes the limit",
        },
        ConfigField {
            name: MAX_COST_FIELD,
            ty: "f64",
            default: ConfigValue::Unset,
            min: None,
            max: None,
            env: None,
            choices: &[],
            description: "Most spending in USD per autopilot engagement, counting API-billed spend only. Must be positive, and unset means no cap",
        },
        ConfigField {
            name: MAX_MINUTES_FIELD,
            ty: "u32",
            default: ConfigValue::Unset,
            min: Some(MIN_AUTOPILOT_MAX_MINUTES as u64),
            max: Some(MAX_AUTOPILOT_MAX_MINUTES as u64),
            env: None,
            choices: &[],
            description: "Most wall-clock minutes per autopilot engagement while the session is open. Unset means no cap",
        },
        ConfigField {
            name: ERROR_RETRIES_FIELD,
            ty: "u32",
            default: ConfigValue::U64(DEFAULT_AUTOPILOT_ERROR_RETRIES as u64),
            min: Some(MIN_AUTOPILOT_ERROR_RETRIES as u64),
            max: Some(MAX_AUTOPILOT_ERROR_RETRIES as u64),
            env: None,
            choices: &[],
            description: "Retries for transient provider errors during an autopilot engagement",
        },
    ];

    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(continuations) = self.max_continuations {
            check_range(
                AUTOPILOT_SECTION,
                MAX_CONTINUATIONS_FIELD,
                continuations,
                MIN_AUTOPILOT_MAX_CONTINUATIONS,
                MAX_AUTOPILOT_MAX_CONTINUATIONS,
            )?;
        }
        check_range(
            AUTOPILOT_SECTION,
            ERROR_RETRIES_FIELD,
            self.error_retries,
            MIN_AUTOPILOT_ERROR_RETRIES,
            MAX_AUTOPILOT_ERROR_RETRIES,
        )?;
        if let Some(cost) = self.max_cost
            && (!cost.is_finite() || cost <= 0.0)
        {
            return Err(ConfigError::OutOfRangeFloat {
                section: AUTOPILOT_SECTION,
                field: MAX_COST_FIELD,
                value: cost,
            });
        }
        if let Some(minutes) = self.max_minutes {
            check_range(
                AUTOPILOT_SECTION,
                MAX_MINUTES_FIELD,
                minutes,
                MIN_AUTOPILOT_MAX_MINUTES,
                MAX_AUTOPILOT_MAX_MINUTES,
            )?;
        }
        Ok(())
    }

    fn from_file(f: AutopilotFileConfig) -> Self {
        Self {
            max_continuations: match f.max_continuations {
                Some(UNCAPPED_AUTOPILOT_CONTINUATIONS) => None,
                configured => Some(configured.unwrap_or(DEFAULT_AUTOPILOT_MAX_CONTINUATIONS)),
            },
            max_cost: f.max_cost,
            max_minutes: f.max_minutes,
            error_retries: f.error_retries.unwrap_or(DEFAULT_AUTOPILOT_ERROR_RETRIES),
        }
    }
}

impl Default for AutopilotConfig {
    fn default() -> Self {
        Self::from_file(AutopilotFileConfig::default())
    }
}

#[derive(Debug, Clone, Default)]
pub struct PluginsConfig {
    pub enabled: bool,
    pub names: Vec<String>,
    /// Per-plugin option tables, without `enabled`. Each plugin validates its
    /// own via `caudra.api.register_options` at load time.
    pub opts: HashMap<String, JsonMap<String, JsonValue>>,
}

impl PluginsConfig {
    pub fn from_plugins(plugins: HashMap<String, PluginFileConfig>) -> Self {
        let mut all: Vec<String> = DEFAULT_BUILTINS
            .iter()
            .filter(|name| plugins.get(**name).and_then(|t| t.enabled).unwrap_or(true))
            .map(|s| s.to_string())
            .collect();

        let mut extra: Vec<&String> = plugins
            .iter()
            .filter(|(name, cfg)| {
                !DEFAULT_BUILTINS.contains(&name.as_str()) && cfg.enabled.unwrap_or(false)
            })
            .map(|(name, _)| name)
            .collect();
        extra.sort();
        all.extend(extra.into_iter().cloned());

        let opts = plugins
            .iter()
            .filter(|(_, cfg)| !cfg.opts.is_empty())
            .map(|(name, cfg)| (name.clone(), cfg.opts.clone()))
            .collect();

        Self {
            enabled: true,
            names: all,
            opts,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.decisions.validate()?;
        self.ui.validate_all()?;
        self.agent.validate()?;
        self.agent.messaging.validate()?;
        self.agent.steering.validate()?;
        self.provider.validate()?;
        self.storage.validate()?;
        self.storage.snapshots.validate()?;
        self.automations.validate()?;
        Ok(())
    }
}

fn push_rules(
    rules: &mut Vec<PermissionRule>,
    tools: &HashMap<String, ToolPermissions>,
    effect: Effect,
) {
    for (tool, perms) in tools {
        let scope_set = match effect {
            Effect::Allow => &perms.allow,
            Effect::Ask => &perms.ask,
            Effect::Deny => &perms.deny,
        };
        let Some(scope_set) = scope_set else {
            continue;
        };
        let tool = ToolKey::native(tool);
        if effect == Effect::Allow && !is_shell_permission_tool(&tool) {
            continue;
        }
        match scope_set {
            ScopeSet::All(true) if effect == Effect::Allow => rules.push(PermissionRule {
                tool,
                scope: Some("*".into()),
                effect,
            }),
            ScopeSet::All(true) => rules.push(PermissionRule {
                tool,
                scope: None,
                effect,
            }),
            ScopeSet::Scopes(scopes) => {
                for scope in scopes {
                    if let Some(error) = rule_scope_error(&tool, Some(scope), effect) {
                        warn!(
                            tool = %tool,
                            ?effect,
                            error,
                            "skipping invalid shell command permission pattern"
                        );
                        continue;
                    }
                    rules.push(PermissionRule {
                        tool: tool.clone(),
                        scope: Some(scope.clone()),
                        effect,
                    });
                }
            }
            ScopeSet::All(false) => {}
        }
    }
}

fn is_shell_permission_tool(tool: &ToolKey) -> bool {
    matches!(tool, ToolKey::Native(tool) if SHELL_PERMISSION_TOOLS.contains(&tool.as_ref()))
}

fn rule_scope_error(tool: &ToolKey, scope: Option<&str>, effect: Effect) -> Option<String> {
    if !matches!(effect, Effect::Allow | Effect::Ask) {
        return None;
    }
    if !is_shell_permission_tool(tool) {
        return None;
    }
    scope.and_then(|pattern| validate_shell_command_pattern(pattern).err())
}

fn validate_shell_command_pattern(pattern: &str) -> Result<(), String> {
    caudra_storage::permission_state::validate_command_pattern(pattern)
}

pub fn is_valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SERVER_NAME_LEN
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl McpPermissions {
    /// Reads the `[mcp]` value of a permissions file: one table per server,
    /// each holding `allow`, `ask`, and `deny` rules and a `default`.
    pub fn parse(mcp: &toml::Value) -> Result<Self, McpPermissionsError> {
        let servers = mcp.as_table().ok_or(McpPermissionsError::NotATable)?;
        let mut permissions = Self::default();
        for (server, table) in servers {
            let table = table
                .as_table()
                .ok_or_else(|| McpPermissionsError::ServerNotATable(server.clone()))?;
            permissions.add_server(server, table)?;
        }
        Ok(permissions)
    }

    fn add_server(&mut self, server: &str, table: &toml::Table) -> Result<(), McpPermissionsError> {
        if !is_valid_server_name(server) {
            return Err(McpPermissionsError::ServerName(server.to_owned()));
        }
        let whole_server = ToolKey::McpServer {
            server: server.into(),
        };
        for (key, value) in table {
            let effect = match key.as_str() {
                "allow" => Effect::Allow,
                "ask" => Effect::Ask,
                "deny" => Effect::Deny,
                "default" => {
                    let default = value
                        .clone()
                        .try_into()
                        .map_err(|_| McpPermissionsError::Default(server.to_owned()))?;
                    self.defaults.insert(whole_server.clone(), default);
                    continue;
                }
                _ => {
                    return Err(McpPermissionsError::UnknownKey {
                        server: server.to_owned(),
                        key: key.clone(),
                    });
                }
            };
            let tools = match value {
                toml::Value::Boolean(true) => vec![MCP_WHOLE_SERVER],
                toml::Value::Boolean(false) => Vec::new(),
                toml::Value::String(tool) => vec![tool.as_str()],
                toml::Value::Array(entries) => entries
                    .iter()
                    .map(|entry| {
                        entry
                            .as_str()
                            .ok_or_else(|| McpPermissionsError::NonStringEntry {
                                server: server.to_owned(),
                                key: key.clone(),
                            })
                    })
                    .collect::<Result<_, _>>()?,
                _ => {
                    return Err(McpPermissionsError::RuleType {
                        server: server.to_owned(),
                        key: key.clone(),
                    });
                }
            };
            for tool in tools {
                let tool = if tool == MCP_WHOLE_SERVER {
                    whole_server.clone()
                } else {
                    ToolKey::parse(&format!("{server}.{tool}"))?
                };
                self.rules.push(McpPermissionRule { tool, effect });
            }
        }
        Ok(())
    }
}

fn build_permissions(
    global: PermissionsFileConfig,
    project: PermissionsFileConfig,
) -> PermissionsConfig {
    if has_invalid_shell_patterns(&global) || has_invalid_shell_patterns(&project) {
        warn!("permissions contain an invalid shell allow or ask pattern; failing closed");
        return fail_closed_permissions();
    }
    let global_default = match global.default {
        Some(DefaultEffect::Deny) => DefaultEffect::Deny,
        Some(DefaultEffect::Prompt | DefaultEffect::Allow) | None => DefaultEffect::Prompt,
    };
    let default = match (global_default, project.default) {
        (DefaultEffect::Deny, _) | (_, Some(DefaultEffect::Deny)) => DefaultEffect::Deny,
        (_, Some(DefaultEffect::Prompt | DefaultEffect::Allow) | None) => DefaultEffect::Prompt,
    };

    let mut tool_defaults = HashMap::new();
    for (tool, perms) in &global.tools {
        if let Some(d @ (DefaultEffect::Deny | DefaultEffect::Prompt)) = perms.default {
            let key = ToolKey::native(tool);
            if matches!(key, ToolKey::Wildcard) {
                tracing::warn!(
                    tool = tool,
                    "ignoring [\"*\"].default — use the top-level `default` field instead \
                     for global fallback behavior"
                );
            } else {
                tool_defaults.insert(key, d);
            }
        }
    }
    for (key, d) in &global.mcp.defaults {
        if *d != DefaultEffect::Allow {
            tool_defaults.insert(key.clone(), *d);
        }
    }
    for (tool, perms) in &project.tools {
        if let Some(d) = perms.default
            && d != DefaultEffect::Allow
        {
            let key = ToolKey::native(tool);
            if matches!(key, ToolKey::Wildcard) {
                tracing::warn!(
                    tool = tool,
                    "ignoring project [\"*\"].default — use the top-level `default` field instead"
                );
            } else {
                let inherited = tool_defaults.get(&key).copied().unwrap_or(global_default);
                tool_defaults.insert(
                    key,
                    if inherited == DefaultEffect::Deny || d == DefaultEffect::Deny {
                        DefaultEffect::Deny
                    } else {
                        DefaultEffect::Prompt
                    },
                );
            }
        }
    }
    for (key, d) in &project.mcp.defaults {
        if *d != DefaultEffect::Allow {
            let inherited = tool_defaults.get(key).copied().unwrap_or(global_default);
            tool_defaults.insert(
                key.clone(),
                if inherited == DefaultEffect::Deny || *d == DefaultEffect::Deny {
                    DefaultEffect::Deny
                } else {
                    DefaultEffect::Prompt
                },
            );
        }
    }

    let mut rules = Vec::new();
    push_parsed_rules(&mut rules, &global.mcp.rules, Effect::Deny);
    push_rules(&mut rules, &global.tools, Effect::Deny);
    push_rules(&mut rules, &project.tools, Effect::Deny);
    push_parsed_rules(&mut rules, &project.mcp.rules, Effect::Deny);
    for config in [&global, &project] {
        push_rules(&mut rules, &config.tools, Effect::Ask);
        push_parsed_rules(&mut rules, &config.mcp.rules, Effect::Ask);
    }
    push_rules(&mut rules, &global.tools, Effect::Allow);

    let mut project_allow_rules = Vec::new();
    push_rules(&mut project_allow_rules, &project.tools, Effect::Allow);

    let mut project_restrictive_rules = Vec::new();
    push_rules(&mut project_restrictive_rules, &project.tools, Effect::Deny);
    push_parsed_rules(
        &mut project_restrictive_rules,
        &project.mcp.rules,
        Effect::Deny,
    );
    push_rules(&mut project_restrictive_rules, &project.tools, Effect::Ask);
    push_parsed_rules(
        &mut project_restrictive_rules,
        &project.mcp.rules,
        Effect::Ask,
    );

    let mut review_candidates = Vec::new();
    push_review_candidates(&mut review_candidates, PermissionSource::Global, &global);
    push_review_candidates(&mut review_candidates, PermissionSource::Project, &project);
    let mut loaded_sources = Vec::new();
    for config in [&global, &project] {
        let Some((path, digest)) = &config.loaded_file else {
            continue;
        };
        let mut loaded_rules = Vec::new();
        for effect in [Effect::Deny, Effect::Ask, Effect::Allow] {
            push_rules(&mut loaded_rules, &config.tools, effect);
            if effect != Effect::Allow {
                push_parsed_rules(&mut loaded_rules, &config.mcp.rules, effect);
            }
        }
        loaded_sources.extend(loaded_rules.into_iter().map(|rule| LoadedPermissionSource {
            rule,
            path: path.clone(),
            content_digest: digest.clone(),
        }));
    }
    PermissionsConfig {
        default,
        tool_defaults,
        rules,
        project_allow_rules,
        project_restrictive_rules,
        review_candidates,
        loaded_sources,
        yolo: false,
    }
}

fn has_invalid_shell_patterns(config: &PermissionsFileConfig) -> bool {
    config.tools.iter().any(|(tool, permissions)| {
        is_shell_permission_tool(&ToolKey::native(tool))
            && [&permissions.allow, &permissions.ask]
                .into_iter()
                .flatten()
                .any(|scopes| match scopes {
                    ScopeSet::Scopes(scopes) => scopes
                        .iter()
                        .any(|pattern| validate_shell_command_pattern(pattern).is_err()),
                    ScopeSet::All(_) => false,
                })
    })
}

fn push_parsed_rules(
    rules: &mut Vec<PermissionRule>,
    parsed_rules: &[McpPermissionRule],
    effect: Effect,
) {
    rules.extend(
        parsed_rules
            .iter()
            .filter(|rule| rule.effect == effect)
            .map(|rule| PermissionRule {
                tool: rule.tool.clone(),
                scope: None,
                effect,
            }),
    );
}

fn push_review_candidates(
    candidates: &mut Vec<PermissionReviewCandidate>,
    source: PermissionSource,
    config: &PermissionsFileConfig,
) {
    if config.default == Some(DefaultEffect::Allow) {
        candidates.push(PermissionReviewCandidate {
            source,
            kind: PermissionReviewKind::Default,
            tool: None,
            scope: None,
        });
    }
    for (tool, permissions) in &config.tools {
        let tool = ToolKey::native(tool);
        if permissions.default == Some(DefaultEffect::Allow) {
            candidates.push(PermissionReviewCandidate {
                source,
                kind: PermissionReviewKind::Default,
                tool: Some(tool.clone()),
                scope: None,
            });
        }
        let Some(scopes) = &permissions.allow else {
            continue;
        };
        match scopes {
            ScopeSet::All(true) if is_shell_permission_tool(&tool) => {
                if source != PermissionSource::Global {
                    candidates.push(PermissionReviewCandidate {
                        source,
                        kind: PermissionReviewKind::Rule,
                        tool: Some(tool),
                        scope: None,
                    });
                }
            }
            ScopeSet::All(true) => candidates.push(PermissionReviewCandidate {
                source,
                kind: PermissionReviewKind::Rule,
                tool: Some(tool),
                scope: None,
            }),
            ScopeSet::Scopes(scopes) => {
                for scope in scopes {
                    if is_shell_permission_tool(&tool) {
                        if rule_scope_error(&tool, Some(scope), Effect::Allow).is_some() {
                            continue;
                        }
                        if source == PermissionSource::Global {
                            continue;
                        }
                    }
                    candidates.push(PermissionReviewCandidate {
                        source,
                        kind: PermissionReviewKind::Rule,
                        tool: Some(tool.clone()),
                        scope: Some(scope.clone()),
                    });
                }
            }
            ScopeSet::All(false) => {}
        }
    }
    for (tool, default) in &config.mcp.defaults {
        if *default == DefaultEffect::Allow {
            candidates.push(PermissionReviewCandidate {
                source,
                kind: PermissionReviewKind::Default,
                tool: Some(tool.clone()),
                scope: None,
            });
        }
    }
    for rule in &config.mcp.rules {
        if rule.effect == Effect::Allow {
            candidates.push(PermissionReviewCandidate {
                source,
                kind: PermissionReviewKind::Rule,
                tool: Some(rule.tool.clone()),
                scope: None,
            });
        }
    }
}

fn global_dir() -> Option<PathBuf> {
    paths::config_dir().ok()
}

fn env_file_var_is_allowed(key: &str) -> bool {
    !PROCESS_ONLY_ENV_VARS.contains(&key) && !PANE_ENVIRONMENT.contains(&key)
}

fn load_env_files_with_global(cwd: &Path, global: Option<&Path>) {
    load_env_files_scoped(cwd, global, true);
}

fn load_env_files_scoped(cwd: &Path, global: Option<&Path>, include_project: bool) {
    let (vars, mut project_fallbacks) = env_file_layers(cwd, global, include_project);
    let mut project_env_fallbacks = PROJECT_ENV_FALLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut env_file_keys = ENV_FILE_KEYS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for (key, value) in vars {
        if env_file_var_is_allowed(&key) && std::env::var_os(&key).is_none() {
            if let Some(fallback) = project_fallbacks.remove(&key) {
                project_env_fallbacks
                    .get_or_insert_with(HashMap::new)
                    .insert(key.clone(), fallback);
            } else if let Some(fallbacks) = project_env_fallbacks.as_mut() {
                fallbacks.remove(&key);
            }
            // SAFETY: single-threaded at startup, before any async runtime
            unsafe { std::env::set_var(&key, &value) };
            env_file_keys.insert(key);
        }
    }
}

fn env_file_layers(
    cwd: &Path,
    global: Option<&Path>,
    include_project: bool,
) -> (HashMap<String, String>, HashMap<String, Option<String>>) {
    let mut vars = HashMap::new();
    if let Some(path) = global {
        collect_env_vars(&path.join(ENV_FILE), &mut vars);
    }
    let mut project_vars = HashMap::new();
    if include_project {
        collect_env_vars(&cwd.join(PROJECT_DIR).join(ENV_FILE), &mut project_vars);
    }
    let project_fallbacks = project_vars
        .keys()
        .map(|key| (key.clone(), vars.remove(key)))
        .collect();
    vars.extend(project_vars);
    (vars, project_fallbacks)
}

/// `key` as the process environment or the global `.env` set it. A key the project's `.env` set
/// reads as its shadowed global value or unset, so a repository cannot plant a credential or a secret URL.
pub fn global_env_value(key: &str) -> Result<Option<String>, std::env::VarError> {
    if let Some(fallback) = PROJECT_ENV_FALLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .and_then(|fallbacks| fallbacks.get(key))
    {
        return Ok(fallback.clone());
    }
    match std::env::var(key) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The proxies Caudra's own requests use. A value only the project's `.env`
/// set never applies, so a repository cannot route Caudra's traffic.
pub fn proxy_env() -> ProxyEnv {
    ProxyEnv::from_lookup(|name| global_env_value(name).ok().flatten())
}

pub fn proxy_route(url: &Url) -> ProxyRoute {
    proxy_env().route(url)
}

pub fn https_proxy_route() -> ProxyRoute {
    proxy_env().https_route()
}

/// Where Caudra's own value of a variable came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvSource {
    Process,
    GlobalEnvFile,
    ProjectEnvFile,
}

pub fn env_source(name: &str) -> EnvSource {
    let from_project = PROJECT_ENV_FALLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .is_some_and(|fallbacks| fallbacks.contains_key(name));
    if from_project {
        return EnvSource::ProjectEnvFile;
    }
    let from_env_file = ENV_FILE_KEYS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains(name);
    if from_env_file {
        EnvSource::GlobalEnvFile
    } else {
        EnvSource::Process
    }
}

/// The process environment as shell commands may receive it, sorted by name. A variable the
/// project's `.env` set reads as the global value it shadowed or is left out, so a repository
/// never chooses the value of a variable the user forwards.
pub fn shell_visible_environment() -> Vec<(String, OsString)> {
    let mut visible: BTreeMap<String, OsString> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value)))
        .collect();
    if let Some(fallbacks) = PROJECT_ENV_FALLBACKS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
    {
        for (name, fallback) in fallbacks {
            match fallback {
                Some(value) => visible.insert(name.clone(), value.into()),
                None => visible.remove(name),
            };
        }
    }
    visible.into_iter().collect()
}

/// `key` as the process inherited it. A key a `.env` file set reads as unset, so a reparse reads
/// that file as it is now rather than its startup copy.
fn inherited_env_value(key: &str) -> Result<Option<String>, std::env::VarError> {
    let from_env_file = ENV_FILE_KEYS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains(key);
    if from_env_file {
        return Ok(None);
    }
    global_env_value(key)
}

/// `key` as the process inherited it, else as the global `.env` holds it now. Unlike
/// [`global_env_value`], an edit to the global `.env` shows on the next read, and the process
/// environment stays as it was. A project `.env` never supplies a value.
pub fn current_global_env_value(key: &str) -> Result<Option<String>, std::env::VarError> {
    current_env_value_in(key, global_dir().as_deref())
}

fn current_env_value_in(
    key: &str,
    global: Option<&Path>,
) -> Result<Option<String>, std::env::VarError> {
    if let Some(value) = inherited_env_value(key)? {
        return Ok(Some(value));
    }
    let mut vars = HashMap::new();
    if let Some(dir) = global {
        collect_env_vars(&dir.join(ENV_FILE), &mut vars);
    }
    Ok(vars.remove(key).filter(|_| env_file_var_is_allowed(key)))
}

pub fn load_global_env_file() {
    load_env_files_scoped(Path::new("."), global_dir().as_deref(), false);
}

fn collect_env_vars(path: &Path, vars: &mut HashMap<String, String>) {
    let Ok(iter) = dotenvy::from_path_iter(path) else {
        return;
    };
    for item in iter.flatten() {
        vars.insert(item.0, item.1);
    }
}

pub fn load_env_files(cwd: &Path) {
    load_env_files_with_global(cwd, global_dir().as_deref());
}

pub fn load_permissions(cwd: &Path) -> PermissionsConfig {
    load_permissions_inner(cwd, global_dir().as_deref())
}

pub fn load_global_permissions() -> PermissionsConfig {
    load_permissions_scoped(Path::new("."), global_dir().as_deref(), false)
}

fn load_permissions_inner(cwd: &Path, global_dir: Option<&Path>) -> PermissionsConfig {
    load_permissions_scoped(cwd, global_dir, true)
}

fn load_permissions_scoped(
    cwd: &Path,
    global_dir: Option<&Path>,
    include_project: bool,
) -> PermissionsConfig {
    try_load_permissions_scoped(cwd, global_dir, include_project).unwrap_or_else(|error| {
        warn!(error, "permissions failed closed");
        fail_closed_permissions()
    })
}

/// [`load_permissions`], with the reason a file failed to load instead of
/// the policy that denies every tool call, for a reload to report it.
pub fn try_load_permissions(cwd: &Path) -> Result<PermissionsConfig, String> {
    try_load_permissions_scoped(cwd, global_dir().as_deref(), true)
}

fn try_load_permissions_scoped(
    cwd: &Path,
    global_dir: Option<&Path>,
    include_project: bool,
) -> Result<PermissionsConfig, String> {
    fn read(path: &Path, lookup: Lookup<'_>) -> Result<Option<PermissionsFileConfig>, String> {
        read_permissions_file(path, lookup).map_err(|error| format!("{}: {error}", path.display()))
    }
    let global_perms = match global_dir {
        Some(dir) => {
            let path = dir.join(PERMISSIONS_FILE);
            read(&path, Lookup::Global(&path))?.unwrap_or_default()
        }
        None => PermissionsFileConfig::default(),
    };
    if !include_project {
        return Ok(build_permissions(
            global_perms,
            PermissionsFileConfig::default(),
        ));
    }
    let project_path = cwd.join(PROJECT_DIR).join(PERMISSIONS_FILE);
    let project_perms = read(&project_path, Lookup::Refused)?.unwrap_or_default();
    Ok(build_permissions(global_perms, project_perms))
}

/// Whether `text`, the global permissions file at `path` in the latest
/// format, loads.
pub fn validate_global_permissions(path: &Path, text: &str) -> Result<(), String> {
    interpolation::from_str::<PermissionsFileConfig>(text, Lookup::Global(path))
        .map(drop)
        .map_err(|error| format!("cannot parse permissions: {error}"))
}

/// Whether `text`, a project permissions file in the latest format, loads.
pub fn validate_project_permissions(_path: &Path, text: &str) -> Result<(), String> {
    interpolation::from_str::<PermissionsFileConfig>(text, Lookup::Refused)
        .map(drop)
        .map_err(|error| format!("cannot parse permissions: {error}"))
}

/// `lookup` is settled on the path as named, since a symlinked file still
/// belongs to the directory that holds the link.
fn read_permissions_file(
    path: &Path,
    lookup: Lookup<'_>,
) -> Result<Option<PermissionsFileConfig>, String> {
    let path = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read permissions: {error}")),
    };
    let content =
        fs::read_to_string(&path).map_err(|error| format!("cannot read permissions: {error}"))?;
    let digest = permission_source_digest(content.as_bytes());
    let loaded = migration::upgrade_text(content, &PERMISSIONS_FORMAT)
        .map_err(|error| format!("cannot parse permissions: {error}"))?;
    let mut config: PermissionsFileConfig = interpolation::from_str(&loaded.text, lookup)
        .map_err(|error| format!("cannot parse permissions: {error}"))?;
    config.loaded_file = Some((path, digest));
    Ok(Some(config))
}

fn permission_source_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// The policy a permissions file that fails to load leaves: every tool call
/// denied.
pub fn fail_closed_permissions() -> PermissionsConfig {
    PermissionsConfig {
        default: DefaultEffect::Deny,
        rules: vec![PermissionRule {
            tool: ToolKey::Wildcard,
            scope: None,
            effect: Effect::Deny,
        }],
        ..Default::default()
    }
}

pub fn global_config_dir() -> Option<PathBuf> {
    global_dir()
}

/// The global `caudra.toml`, resolved without creating its directory.
pub fn global_config_path() -> Option<PathBuf> {
    paths::config_dir_path()
        .ok()
        .map(|directory| config_file::global_config_path(&directory))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_version::{CONFIG_VERSION_KEY, ConfigVersionError};
    use crate::decisions::{DecisionEndpoint, DecisionProtocol};
    use std::{env, fs};
    use tempfile::TempDir;
    use test_case::test_case;

    const EXPECTED_DEFAULT_FLASH_DURATION: Duration = Duration::from_secs(10);
    const CUSTOM_FLASH_DURATION_MS: u64 = 1500;
    const ZERO_FLASH_DURATION_MS: u64 = 0;
    const NO_DEFAULT_DELETION: &str = "retention must delete nothing until a user opts in";
    const BACKGROUND_REMINDER_FIELD: &str = "background_reminder_turns";
    const INBOUND_RATE_FIELD: &str = "inbound_per_minute";
    const SENDER_RATE_FIELD: &str = "sender_per_minute";
    const PUBLISH_RATE_FIELD: &str = "publish_per_minute";
    const FANOUT_FIELD: &str = "max_fanout";
    const HISTORY_DAYS_FIELD: &str = "history_days";
    const HISTORY_MAX_FIELD: &str = "history_max_messages";
    const CUSTOM_HISTORY_LIMIT: u64 = 7;
    const LOW_RATE: usize = 4;
    const MIDDLE_RATE: usize = 32;
    const HIGH_RATE: usize = 128;
    const CUSTOM_BACKGROUND_REMINDER_TURNS: u32 = 13;
    const UNSIGNED_REMINDER_ERROR: &str = "expected u32";
    const TODO_REMINDER_FIELD: &str = "todo_reminder";
    const TASK_MODEL_CHOICE_FIELD: &str = "task_model_choice";
    const DEFERRED_QUESTIONS_FIELD: &str = "deferred_questions";
    const WORK_NUDGE_FIELD: &str = "work_nudge_compactions";
    const BOOLEAN_EXPECTED_ERROR: &str = "expected a boolean";
    const SHELL_THRESHOLD_FIELD: &str = "shell_async_threshold_secs";
    const SHELL_WORKDIR_REDIRECT_FIELD: &str = "shell_workdir_redirect";
    const EMPTY_SOURCE_DIGEST: &str =
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const ABC_SOURCE_DIGEST: &str =
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const AUTOMATIONS_TABLE: &str = "[automations]";
    const UNKNOWN_FIELD_ERROR: &str = "unknown field";
    const CUSTOM_TURNS_PER_HOUR: u32 = 30;
    const CUSTOM_UNATTENDED_TURNS: u32 = 50;
    const AUTOPILOT_TABLE: &str = "[autopilot]";
    const WEB_TABLE: &str = "[web]";
    const PRIVATE_HOSTS_ENTRY: &str = "allow_private_hosts = ['localhost:3000']";
    const PRIVATE_NETWORKS_ENTRY: &str = "allow_private_networks = ['loopback']";
    const PROJECT_WEB_ERROR: &str =
        "invalid project config: [web] is global-only; move it to the global caudra.toml";
    const CUSTOM_AUTOPILOT_CONTINUATIONS: u32 = 40;
    const CUSTOM_AUTOPILOT_COST: f64 = 2.5;
    const DECISION_ENDPOINT_NAME: &str = "typesafe";
    const DECISION_BASE_URL: &str = "https://decisions.example.test";

    #[test_case(b"", EMPTY_SOURCE_DIGEST; "empty_source")]
    #[test_case(b"abc", ABC_SOURCE_DIGEST; "known_source")]
    fn permission_source_digest_uses_lowercase_bytewise_hex(bytes: &[u8], expected: &str) {
        assert_eq!(permission_source_digest(bytes), expected);
    }

    fn plugin_enabled(enabled: bool) -> PluginFileConfig {
        PluginFileConfig {
            enabled: Some(enabled),
            opts: JsonMap::new(),
        }
    }

    #[test_case("", InboundPolicy::Auto; "missing_agent")]
    #[test_case("[agent]", InboundPolicy::Auto; "missing_messaging")]
    #[test_case("[agent.messaging]", InboundPolicy::Auto; "missing_inbound")]
    #[test_case("[agent.messaging]\ninbound = 'accept'", InboundPolicy::Accept; "accept")]
    #[test_case("[agent.messaging]\ninbound = 'auto'", InboundPolicy::Auto; "auto")]
    #[test_case("[agent.messaging]\ninbound = 'hold'", InboundPolicy::Hold; "hold")]
    #[test_case("[agent.messaging]\ninbound = 'refuse'", InboundPolicy::Refuse; "refuse")]
    fn messaging_config_resolves_and_serializes(source: &str, expected: InboundPolicy) {
        let raw: RawConfig = toml::from_str(source).unwrap();
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.agent.messaging.inbound, expected);
        assert_eq!(config.agent.messaging.project_inbound, None);
        assert_eq!(config.agent.features, FeatureFlags::NONE);
        let serialized = serde_json::to_value(&config.agent).unwrap();
        let messaging: MessagingConfig =
            serde_json::from_value(serialized["messaging"].clone()).unwrap();
        assert_eq!(messaging, config.agent.messaging);
    }

    #[test_case("inbound = 'unknown'"; "unknown_policy")]
    #[test_case("inbound = true"; "boolean_policy")]
    #[test_case("inbound = 1"; "numeric_policy")]
    #[test_case("inboud = 'hold'"; "unknown_field")]
    fn invalid_messaging_config_is_rejected(source: &str) {
        assert!(toml::from_str::<RawConfig>(&format!("[agent.messaging]\n{source}")).is_err());
    }

    fn messaging_layer(inbound: Option<&str>) -> RawConfig {
        let field = inbound.map_or_else(String::new, |value| format!("inbound = '{value}'"));
        toml::from_str(&format!("[agent.messaging]\n{field}")).unwrap()
    }

    #[test_case("accept"; "accept")]
    #[test_case("auto"; "auto")]
    #[test_case("hold"; "hold")]
    #[test_case("refuse"; "refuse")]
    fn internal_project_messaging_policy_cannot_be_deserialized(policy: &str) {
        let source = format!("project_inbound = '{policy}'");
        assert!(toml::from_str::<MessagingConfig>(&source).is_err());
        assert!(toml::from_str::<RawConfig>(&format!("[agent.messaging]\n{source}")).is_err());
    }

    #[test_case(Some("accept"), Some("accept"), InboundPolicy::Accept; "accept_accept")]
    #[test_case(Some("accept"), Some("auto"), InboundPolicy::Auto; "accept_auto")]
    #[test_case(Some("accept"), Some("hold"), InboundPolicy::Hold; "accept_hold")]
    #[test_case(Some("accept"), Some("refuse"), InboundPolicy::Refuse; "accept_refuse")]
    #[test_case(Some("auto"), Some("accept"), InboundPolicy::Auto; "auto_accept")]
    #[test_case(Some("auto"), Some("auto"), InboundPolicy::Auto; "auto_auto")]
    #[test_case(Some("auto"), Some("hold"), InboundPolicy::Hold; "auto_hold")]
    #[test_case(Some("auto"), Some("refuse"), InboundPolicy::Refuse; "auto_refuse")]
    #[test_case(Some("hold"), Some("accept"), InboundPolicy::Hold; "hold_accept")]
    #[test_case(Some("hold"), Some("auto"), InboundPolicy::Hold; "hold_auto")]
    #[test_case(Some("hold"), Some("hold"), InboundPolicy::Hold; "hold_hold")]
    #[test_case(Some("hold"), Some("refuse"), InboundPolicy::Refuse; "hold_refuse")]
    #[test_case(Some("refuse"), Some("accept"), InboundPolicy::Refuse; "refuse_accept")]
    #[test_case(Some("refuse"), Some("auto"), InboundPolicy::Refuse; "refuse_auto")]
    #[test_case(Some("refuse"), Some("hold"), InboundPolicy::Refuse; "refuse_hold")]
    #[test_case(Some("refuse"), Some("refuse"), InboundPolicy::Refuse; "refuse_refuse")]
    #[test_case(None, Some("accept"), InboundPolicy::Auto; "implicit_auto_accept")]
    #[test_case(None, Some("auto"), InboundPolicy::Auto; "implicit_auto_auto")]
    #[test_case(None, Some("hold"), InboundPolicy::Hold; "implicit_auto_hold")]
    #[test_case(None, Some("refuse"), InboundPolicy::Refuse; "implicit_auto_refuse")]
    #[test_case(Some("accept"), None, InboundPolicy::Accept; "missing_preserves_accept")]
    #[test_case(Some("auto"), None, InboundPolicy::Auto; "missing_preserves_auto")]
    #[test_case(Some("hold"), None, InboundPolicy::Hold; "missing_preserves_hold")]
    #[test_case(Some("refuse"), None, InboundPolicy::Refuse; "missing_preserves_refuse")]
    #[test_case(None, None, InboundPolicy::Auto; "both_missing")]
    fn project_messaging_can_only_tighten(
        global: Option<&str>,
        project: Option<&str>,
        expected: InboundPolicy,
    ) {
        let mut raw = messaging_layer(global);
        let project = messaging_layer(project);
        let expected_floor = project.agent.messaging.inbound.clone();
        raw.merge(project);
        raw.merge(RawConfig::default());
        let messaging = raw.into_config(false).unwrap().agent.messaging;
        assert_eq!(messaging.inbound, expected);
        assert_eq!(messaging.project_inbound, expected_floor);
    }

    #[test_case(Some("refuse"), Some("accept"), InboundPolicy::Accept; "relax_refuse_to_accept")]
    #[test_case(Some("hold"), Some("auto"), InboundPolicy::Auto; "relax_hold_to_auto")]
    #[test_case(Some("accept"), Some("hold"), InboundPolicy::Hold; "tighten_accept_to_hold")]
    #[test_case(None, Some("accept"), InboundPolicy::Accept; "replace_implicit_auto")]
    #[test_case(Some("accept"), None, InboundPolicy::Accept; "missing_preserves_accept")]
    #[test_case(Some("hold"), None, InboundPolicy::Hold; "missing_preserves_hold")]
    fn global_messaging_overlay_replaces_explicit_policy(
        base: Option<&str>,
        overlay: Option<&str>,
        expected: InboundPolicy,
    ) {
        let mut raw = messaging_layer(base);
        raw.merge_global(messaging_layer(overlay));
        raw.merge_global(RawConfig::default());
        let messaging = raw.into_config(false).unwrap().agent.messaging;
        assert_eq!(messaging.inbound, expected);
        assert_eq!(messaging.project_inbound, None);
    }

    #[test_case("hold", InboundPolicy::Hold; "hold")]
    #[test_case("refuse", InboundPolicy::Refuse; "refuse")]
    fn later_project_layers_cannot_relax_messaging(restriction: &str, expected: InboundPolicy) {
        let mut raw = messaging_layer(Some("accept"));
        raw.merge(messaging_layer(Some(restriction)));
        raw.merge(messaging_layer(Some("auto")));
        raw.merge(messaging_layer(Some("accept")));
        let messaging = raw.into_config(false).unwrap().agent.messaging;
        assert_eq!(messaging.inbound, expected);
        assert_eq!(messaging.project_inbound, Some(expected));
    }

    #[test_case("hold", InboundPolicy::Hold; "hold")]
    #[test_case("refuse", InboundPolicy::Refuse; "refuse")]
    fn project_messaging_floor_survives_global_overlays(project: &str, floor: InboundPolicy) {
        for global in ["accept", "auto", "hold", "refuse"] {
            let mut raw = messaging_layer(Some(global));
            let global_policy = raw.agent.messaging.inbound.clone().unwrap();
            raw.merge(messaging_layer(Some(project)));
            raw.merge_global(messaging_layer(Some(global)));
            let messaging = raw.into_config(false).unwrap().agent.messaging;
            assert_eq!(messaging.inbound, global_policy.max(floor.clone()));
            assert_eq!(messaging.project_inbound, Some(floor.clone()));
            assert!(
                serde_json::to_value(messaging)
                    .unwrap()
                    .get("project_inbound")
                    .is_none()
            );
        }
    }

    fn rate_layer(field: &str, value: Option<usize>) -> RawConfig {
        value.map_or_else(RawConfig::default, |value| {
            toml::from_str(&format!("[agent.messaging]\n{field} = {value}")).unwrap()
        })
    }

    fn resolved_rate(field: &str, messaging: &MessagingConfig) -> usize {
        match field {
            INBOUND_RATE_FIELD => messaging.inbound_per_minute,
            SENDER_RATE_FIELD => messaging.sender_per_minute,
            PUBLISH_RATE_FIELD => messaging.publish_per_minute,
            FANOUT_FIELD => messaging.max_fanout,
            _ => unreachable!(),
        }
    }

    #[test_case(INBOUND_RATE_FIELD, None, None, None, DEFAULT_INBOUND_PER_MINUTE; "inbound_default")]
    #[test_case(SENDER_RATE_FIELD, None, None, None, DEFAULT_SENDER_PER_MINUTE; "sender_default")]
    #[test_case(PUBLISH_RATE_FIELD, None, None, None, DEFAULT_PUBLISH_PER_MINUTE; "publish_default")]
    #[test_case(FANOUT_FIELD, None, None, None, DEFAULT_MAX_FANOUT; "fanout_default")]
    #[test_case(PUBLISH_RATE_FIELD, None, Some(LOW_RATE), None, LOW_RATE; "project_lowers_publish")]
    #[test_case(FANOUT_FIELD, None, Some(HIGH_RATE), None, DEFAULT_MAX_FANOUT; "project_cannot_raise_fanout")]
    #[test_case(FANOUT_FIELD, None, Some(LOW_RATE), Some(HIGH_RATE), LOW_RATE; "global_overlay_cannot_lift_fanout_ceiling")]
    #[test_case(INBOUND_RATE_FIELD, Some(MIDDLE_RATE), None, None, MIDDLE_RATE; "global_lowers")]
    #[test_case(INBOUND_RATE_FIELD, Some(HIGH_RATE), None, None, HIGH_RATE; "global_raises")]
    #[test_case(SENDER_RATE_FIELD, None, Some(LOW_RATE), None, LOW_RATE; "project_lowers_default")]
    #[test_case(SENDER_RATE_FIELD, None, Some(HIGH_RATE), None, DEFAULT_SENDER_PER_MINUTE; "project_cannot_raise_default")]
    #[test_case(INBOUND_RATE_FIELD, Some(LOW_RATE), Some(MIDDLE_RATE), None, LOW_RATE; "project_cannot_raise_global")]
    #[test_case(INBOUND_RATE_FIELD, None, Some(LOW_RATE), Some(HIGH_RATE), LOW_RATE; "global_overlay_cannot_lift_project_ceiling")]
    #[test_case(SENDER_RATE_FIELD, Some(LOW_RATE), None, Some(MIDDLE_RATE), MIDDLE_RATE; "global_overlay_replaces_global")]
    fn message_rates_only_fall_in_project_layers(
        field: &str,
        global: Option<usize>,
        project: Option<usize>,
        overlay: Option<usize>,
        expected: usize,
    ) {
        let mut raw = rate_layer(field, global);
        raw.merge(rate_layer(field, project));
        raw.merge_global(rate_layer(field, overlay));
        let config = raw.into_config(false).unwrap();
        config.validate().unwrap();
        assert_eq!(resolved_rate(field, &config.agent.messaging), expected);
    }

    #[test_case(INBOUND_RATE_FIELD, false; "global_inbound")]
    #[test_case(SENDER_RATE_FIELD, false; "global_sender")]
    #[test_case(INBOUND_RATE_FIELD, true; "project_inbound")]
    #[test_case(SENDER_RATE_FIELD, true; "project_sender")]
    #[test_case(PUBLISH_RATE_FIELD, false; "global_publish")]
    #[test_case(FANOUT_FIELD, true; "project_fanout")]
    fn zero_message_rate_is_rejected(field: &str, project: bool) {
        let below = Some(MIN_MESSAGE_RATE - 1);
        let mut raw = RawConfig::default();
        if project {
            raw.merge(rate_layer(field, below));
        } else {
            raw = rate_layer(field, below);
        }
        let error = raw.into_config(false).unwrap().validate().unwrap_err();
        assert!(
            matches!(error, ConfigError::BelowMinimum { field: refused, .. } if refused == field),
            "{error}"
        );
    }

    fn history_layer(field: &str, value: u64) -> RawConfig {
        toml::from_str(&format!("[agent.messaging]\n{field} = {value}")).unwrap()
    }

    #[test_case(HISTORY_DAYS_FIELD, None, DEFAULT_HISTORY_DAYS; "days_default")]
    #[test_case(HISTORY_MAX_FIELD, None, DEFAULT_HISTORY_MAX_MESSAGES; "max_default")]
    #[test_case(HISTORY_DAYS_FIELD, Some(CUSTOM_HISTORY_LIMIT), CUSTOM_HISTORY_LIMIT; "days_global")]
    #[test_case(HISTORY_MAX_FIELD, Some(CUSTOM_HISTORY_LIMIT), CUSTOM_HISTORY_LIMIT; "max_global")]
    fn message_history_limits_resolve_from_global_config(
        field: &str,
        global: Option<u64>,
        expected: u64,
    ) {
        let raw = global.map_or_else(RawConfig::default, |value| history_layer(field, value));
        let config = raw.into_config(false).unwrap();
        config.validate().unwrap();
        let messaging = &config.agent.messaging;
        let resolved = match field {
            HISTORY_DAYS_FIELD => messaging.history_days,
            _ => messaging.history_max_messages,
        };
        assert_eq!(resolved, expected);
    }

    #[test_case(HISTORY_DAYS_FIELD, HISTORY_DAYS_KEY; "days")]
    #[test_case(HISTORY_MAX_FIELD, HISTORY_MAX_MESSAGES_KEY; "max_messages")]
    fn project_layers_cannot_set_message_history(field: &str, key: &str) {
        let mut raw = history_layer(field, CUSTOM_HISTORY_LIMIT);
        raw.merge(history_layer(field, CUSTOM_HISTORY_LIMIT));
        raw.merge_global(RawConfig::default());
        assert!(matches!(
            raw.into_config(false),
            Err(ConfigError::ProjectMessageHistory(refused)) if refused == key
        ));
    }

    #[test_case(HISTORY_DAYS_FIELD; "days")]
    #[test_case(HISTORY_MAX_FIELD; "max_messages")]
    fn zero_message_history_limit_is_rejected(field: &str) {
        let error = history_layer(field, MIN_HISTORY_LIMIT - 1)
            .into_config(false)
            .unwrap()
            .validate()
            .unwrap_err();
        assert!(
            matches!(error, ConfigError::BelowMinimum { field: refused, .. } if refused == field),
            "{error}"
        );
    }

    fn automations_layer(options: &str) -> RawConfig {
        toml::from_str(&format!("{AUTOMATIONS_TABLE}\n{options}")).unwrap()
    }

    #[test_case(""; "no_table")]
    #[test_case(AUTOMATIONS_TABLE; "empty_table")]
    fn automations_default_to_no_unattended_cap_and_public_hosts(source: &str) {
        let raw: RawConfig = toml::from_str(source).unwrap();
        let automations = raw.into_config(false).unwrap().automations;
        assert_eq!(
            automations.turns_per_hour,
            DEFAULT_AUTOMATION_TURNS_PER_HOUR
        );
        assert_eq!(automations.max_unattended_turns, None);
        assert!(!automations.allow_private_network);
    }

    #[test_case(TURNS_PER_HOUR_FIELD, MIN_AUTOMATION_TURNS_PER_HOUR; "turns_per_hour_minimum")]
    #[test_case(TURNS_PER_HOUR_FIELD, MAX_AUTOMATION_TURNS_PER_HOUR; "turns_per_hour_maximum")]
    #[test_case(MAX_UNATTENDED_TURNS_FIELD, MIN_MAX_UNATTENDED_TURNS; "unattended_minimum")]
    #[test_case(MAX_UNATTENDED_TURNS_FIELD, MAX_MAX_UNATTENDED_TURNS; "unattended_maximum")]
    fn automation_limits_accept_their_bounds(field: &str, value: u32) {
        let config = automations_layer(&format!("{field} = {value}"))
            .into_config(false)
            .unwrap();
        config.validate().unwrap();
        let resolved = if field == TURNS_PER_HOUR_FIELD {
            Some(config.automations.turns_per_hour)
        } else {
            config.automations.max_unattended_turns
        };
        assert_eq!(resolved, Some(value));
    }

    #[test_case(TURNS_PER_HOUR_FIELD, MIN_AUTOMATION_TURNS_PER_HOUR - 1; "turns_per_hour_below_minimum")]
    #[test_case(TURNS_PER_HOUR_FIELD, MAX_AUTOMATION_TURNS_PER_HOUR + 1; "turns_per_hour_above_maximum")]
    #[test_case(MAX_UNATTENDED_TURNS_FIELD, MIN_MAX_UNATTENDED_TURNS - 1; "unattended_below_minimum")]
    #[test_case(MAX_UNATTENDED_TURNS_FIELD, MAX_MAX_UNATTENDED_TURNS + 1; "unattended_above_maximum")]
    fn automation_limits_out_of_range_are_rejected(field: &str, value: u32) {
        let error = automations_layer(&format!("{field} = {value}"))
            .into_config(false)
            .err()
            .expect("out-of-range automation limit");
        assert!(
            matches!(
                error,
                ConfigError::OutOfRange { section, field: refused, value: rejected, .. }
                    if section == AUTOMATIONS_SECTION && refused == field && rejected == value
            ),
            "{error}"
        );
    }

    #[test_case("turn_per_hour = 5", UNKNOWN_FIELD_ERROR; "unknown_field")]
    #[test_case("allow_private_network = 'yes'", BOOLEAN_EXPECTED_ERROR; "string_switch")]
    #[test_case("turns_per_hour = -1", UNSIGNED_REMINDER_ERROR; "negative_rate")]
    fn invalid_automations_config_is_rejected(source: &str, expected: &str) {
        let error = toml::from_str::<RawConfig>(&format!("{AUTOMATIONS_TABLE}\n{source}"))
            .expect_err("invalid automations option");
        assert!(error.to_string().contains(expected), "{error}");
    }

    #[test_case(""; "empty_table")]
    #[test_case("turns_per_hour = 1"; "lower_rate")]
    #[test_case("max_unattended_turns = 1"; "unattended_cap")]
    #[test_case("allow_private_network = true"; "private_network")]
    fn project_layers_cannot_hold_automations(options: &str) {
        let mut raw = automations_layer("");
        raw.merge(automations_layer(options));
        raw.merge_global(RawConfig::default());
        assert!(matches!(
            raw.into_config(false),
            Err(ConfigError::ProjectAutomations)
        ));
    }

    #[test]
    fn global_layers_merge_automations_field_by_field() {
        let mut raw =
            automations_layer(&format!("{TURNS_PER_HOUR_FIELD} = {CUSTOM_TURNS_PER_HOUR}"));
        raw.merge_global(automations_layer(&format!(
            "{MAX_UNATTENDED_TURNS_FIELD} = {CUSTOM_UNATTENDED_TURNS}\n{ALLOW_PRIVATE_NETWORK_FIELD} = true"
        )));
        raw.merge(RawConfig::default());
        assert_eq!(
            raw.into_config(false).unwrap().automations,
            AutomationsConfig {
                turns_per_hour: CUSTOM_TURNS_PER_HOUR,
                max_unattended_turns: Some(CUSTOM_UNATTENDED_TURNS),
                allow_private_network: true,
            }
        );
    }

    fn web_layer(options: &str) -> RawConfig {
        toml::from_str(&format!("{WEB_TABLE}\n{options}")).unwrap()
    }

    #[test_case(""; "empty_table")]
    #[test_case(PRIVATE_HOSTS_ENTRY; "hosts")]
    #[test_case(PRIVATE_NETWORKS_ENTRY; "networks")]
    fn project_layers_cannot_hold_web(options: &str) {
        let mut raw = RawConfig::default();
        raw.merge(web_layer(options));
        let error = raw.into_config(false).err().expect("project [web] refused");
        assert_eq!(error.to_string(), PROJECT_WEB_ERROR);
    }

    #[test]
    fn global_layers_merge_web_key_by_key() {
        let mut raw = web_layer(PRIVATE_HOSTS_ENTRY);
        raw.merge_global(web_layer(PRIVATE_NETWORKS_ENTRY));
        raw.merge(RawConfig::default());
        assert_eq!(
            raw.into_config(false).unwrap().web,
            WebConfig {
                allow_private_hosts: vec![WebHost {
                    host: url::Host::Domain("localhost".into()),
                    port: 3000,
                }],
                allow_private_networks: vec![WebNetwork::Loopback],
            }
        );
    }

    #[test_case("allow_private_host = []", UNKNOWN_FIELD_ERROR; "unknown_field")]
    #[test_case("allow_private_hosts = 'localhost:3000'", "expected a sequence"; "not_a_list")]
    fn invalid_web_config_is_rejected(source: &str, expected: &str) {
        let error = toml::from_str::<RawConfig>(&format!("{WEB_TABLE}\n{source}"))
            .expect_err("invalid web option");
        assert!(error.to_string().contains(expected), "{error}");
    }

    fn autopilot_layer(options: &str) -> RawConfig {
        toml::from_str(&format!("{AUTOPILOT_TABLE}\n{options}")).unwrap()
    }

    #[test_case(""; "no_table")]
    #[test_case(AUTOPILOT_TABLE; "empty_table")]
    fn autopilot_defaults_to_continuations_and_retries_only(source: &str) {
        let raw: RawConfig = toml::from_str(source).unwrap();
        assert_eq!(
            raw.into_config(false).unwrap().autopilot,
            AutopilotConfig {
                max_continuations: Some(DEFAULT_AUTOPILOT_MAX_CONTINUATIONS),
                max_cost: None,
                max_minutes: None,
                error_retries: DEFAULT_AUTOPILOT_ERROR_RETRIES,
            }
        );
    }

    #[test_case(MAX_CONTINUATIONS_FIELD, MIN_AUTOPILOT_MAX_CONTINUATIONS + 1; "continuations_minimum")]
    #[test_case(MAX_CONTINUATIONS_FIELD, MAX_AUTOPILOT_MAX_CONTINUATIONS; "continuations_maximum")]
    #[test_case(MAX_MINUTES_FIELD, MIN_AUTOPILOT_MAX_MINUTES; "minutes_minimum")]
    #[test_case(MAX_MINUTES_FIELD, MAX_AUTOPILOT_MAX_MINUTES; "minutes_maximum")]
    #[test_case(ERROR_RETRIES_FIELD, MIN_AUTOPILOT_ERROR_RETRIES; "retries_minimum")]
    #[test_case(ERROR_RETRIES_FIELD, MAX_AUTOPILOT_ERROR_RETRIES; "retries_maximum")]
    fn autopilot_limits_accept_their_bounds(field: &str, value: u32) {
        let autopilot = autopilot_layer(&format!("{field} = {value}"))
            .into_config(false)
            .unwrap()
            .autopilot;
        let resolved = match field {
            MAX_CONTINUATIONS_FIELD => autopilot.max_continuations,
            MAX_MINUTES_FIELD => autopilot.max_minutes,
            _ => Some(autopilot.error_retries),
        };
        assert_eq!(resolved, Some(value));
    }

    #[test_case(MAX_CONTINUATIONS_FIELD, MAX_AUTOPILOT_MAX_CONTINUATIONS + 1; "continuations_above_maximum")]
    #[test_case(MAX_MINUTES_FIELD, MIN_AUTOPILOT_MAX_MINUTES - 1; "minutes_below_minimum")]
    #[test_case(MAX_MINUTES_FIELD, MAX_AUTOPILOT_MAX_MINUTES + 1; "minutes_above_maximum")]
    #[test_case(ERROR_RETRIES_FIELD, MAX_AUTOPILOT_ERROR_RETRIES + 1; "retries_above_maximum")]
    fn autopilot_limits_out_of_range_are_rejected(field: &str, value: u32) {
        let error = autopilot_layer(&format!("{field} = {value}"))
            .into_config(false)
            .err()
            .expect("out-of-range autopilot limit");
        assert!(
            matches!(
                error,
                ConfigError::OutOfRange { section, field: refused, value: rejected, .. }
                    if section == AUTOPILOT_SECTION && refused == field && rejected == value
            ),
            "{error}"
        );
    }

    #[test]
    fn zero_continuations_remove_the_cap() {
        let autopilot = autopilot_layer(&format!(
            "{MAX_CONTINUATIONS_FIELD} = {UNCAPPED_AUTOPILOT_CONTINUATIONS}"
        ))
        .into_config(false)
        .unwrap()
        .autopilot;
        assert_eq!(autopilot.max_continuations, None);
    }

    #[test_case(0.0; "zero")]
    #[test_case(-1.0; "negative")]
    #[test_case(f64::INFINITY; "infinite")]
    fn autopilot_cost_must_be_positive_and_finite(cost: f64) {
        let mut raw = autopilot_layer("");
        raw.autopilot.as_mut().unwrap().max_cost = Some(cost);
        let error = raw
            .into_config(false)
            .err()
            .expect("invalid autopilot cost");
        assert!(
            matches!(
                error,
                ConfigError::OutOfRangeFloat { section, field, .. }
                    if section == AUTOPILOT_SECTION && field == MAX_COST_FIELD
            ),
            "{error}"
        );
    }

    #[test_case("max_continuation = 5", UNKNOWN_FIELD_ERROR; "unknown_field")]
    #[test_case("max_continuations = -1", UNSIGNED_REMINDER_ERROR; "negative_continuations")]
    fn invalid_autopilot_config_is_rejected(source: &str, expected: &str) {
        let error = toml::from_str::<RawConfig>(&format!("{AUTOPILOT_TABLE}\n{source}"))
            .expect_err("invalid autopilot option");
        assert!(error.to_string().contains(expected), "{error}");
    }

    #[test_case(""; "empty_table")]
    #[test_case("max_continuations = 1"; "lower_cap")]
    #[test_case("max_cost = 1.0"; "cost_cap")]
    fn project_layers_cannot_hold_autopilot(options: &str) {
        let mut raw = RawConfig::default();
        raw.merge(autopilot_layer(options));
        raw.merge_global(RawConfig::default());
        assert!(matches!(
            raw.into_config(false),
            Err(ConfigError::ProjectAutopilot)
        ));
    }

    #[test]
    fn global_layers_merge_autopilot_field_by_field() {
        let mut raw = autopilot_layer(&format!(
            "{MAX_CONTINUATIONS_FIELD} = {CUSTOM_AUTOPILOT_CONTINUATIONS}"
        ));
        raw.merge_global(autopilot_layer(&format!(
            "{MAX_COST_FIELD} = {CUSTOM_AUTOPILOT_COST}"
        )));
        raw.merge(RawConfig::default());
        assert_eq!(
            raw.into_config(false).unwrap().autopilot,
            AutopilotConfig {
                max_continuations: Some(CUSTOM_AUTOPILOT_CONTINUATIONS),
                max_cost: Some(CUSTOM_AUTOPILOT_COST),
                max_minutes: None,
                error_retries: DEFAULT_AUTOPILOT_ERROR_RETRIES,
            }
        );
    }

    const NOT_A_CHOICE: &str = "not-a-choice";

    /// The file text that sets `key` of `table` to `value`, a list when the key takes one.
    fn choice_text(table: &str, field: &ConfigField, value: &str) -> String {
        let value = match field.ty.ends_with("[]") {
            true => format!("['{value}']"),
            false => format!("'{value}'"),
        };
        format!("[{table}]\n{} = {value}", field.name)
    }

    #[test_case("ui", UiConfig::FIELDS; "ui")]
    #[test_case("agent", AgentConfig::FIELDS; "agent")]
    #[test_case("agent.messaging", MessagingConfig::FIELDS; "agent_messaging")]
    #[test_case("storage", StorageConfig::FIELDS; "storage")]
    #[test_case("storage.retention", RetentionConfig::FIELDS; "storage_retention")]
    #[test_case("worktrees", WorktreesConfig::FIELDS; "worktrees")]
    fn every_listed_choice_parses_and_nothing_else(table: &str, fields: &[ConfigField]) {
        assert!(fields.iter().any(|field| !field.choices.is_empty()));
        for field in fields.iter().filter(|field| !field.choices.is_empty()) {
            for choice in field.choices {
                let text = choice_text(table, field, choice);
                assert!(toml::from_str::<RawConfig>(&text).is_ok(), "{text}");
            }
            let text = choice_text(table, field, NOT_A_CHOICE);
            assert!(toml::from_str::<RawConfig>(&text).is_err(), "{text}");
        }
    }

    #[test_case("", CacheWarmingMode::Off; "default_off")]
    #[test_case("cache_warming = 'running'", CacheWarmingMode::Running; "running")]
    #[test_case("cache_warming = 'idle'", CacheWarmingMode::Idle; "idle")]
    fn global_layers_set_cache_warming(option: &str, expected: CacheWarmingMode) {
        let mut raw: RawConfig = toml::from_str(&format!("[agent]\n{option}")).unwrap();
        raw.merge(RawConfig::default());
        assert_eq!(
            raw.into_config(false).unwrap().agent.cache_warming,
            expected
        );
    }

    #[test_case("off"; "off")]
    #[test_case("idle"; "idle")]
    fn project_layers_cannot_hold_cache_warming(mode: &str) {
        let mut raw = RawConfig::default();
        raw.merge(toml::from_str(&format!("[agent]\ncache_warming = '{mode}'")).unwrap());
        assert!(matches!(
            raw.into_config(false),
            Err(ConfigError::ProjectCacheWarming)
        ));
    }

    #[test]
    fn shell_threshold_merges_and_serializes() {
        let mut raw: RawConfig =
            toml::from_str("[agent]\nshell_async_threshold_secs = 31").unwrap();
        raw.merge(RawConfig::default());
        let config = raw.into_config(false).unwrap();
        config.validate().unwrap();
        assert_eq!(config.agent.shell_async_threshold_secs, 31);
        assert_eq!(
            serde_json::to_value(&config.agent).unwrap()[SHELL_THRESHOLD_FIELD],
            31
        );
        assert_eq!(config.agent.background_reminder_turns, 0);
    }

    #[test_case("task_execution = 'auto'")]
    #[test_case("shell_execution = 'auto'")]
    #[test_case("shell_async_threshold_secs = -1")]
    #[test_case("shell_async_threshold_secs = 1.5")]
    #[test_case("shell_async_threshold_secs = '120'")]
    fn invalid_execution_config_is_rejected(source: &str) {
        assert!(toml::from_str::<RawConfig>(&format!("[agent]\n{source}")).is_err());
    }

    #[test_case(0, false)]
    #[test_case(1, true)]
    fn shell_threshold_requires_positive_value(value: u64, valid: bool) {
        let raw: RawConfig =
            toml::from_str(&format!("[agent]\n{SHELL_THRESHOLD_FIELD} = {value}")).unwrap();
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.validate().is_ok(), valid);
    }

    #[test_case(false, None => Ok(false); "unsupported_omitted")]
    #[test_case(false, Some(false) => Ok(false); "unsupported_foreground")]
    #[test_case(false, Some(true) => Err(TASK_SYNC_REQUIRED); "unsupported_background")]
    #[test_case(true, None => Ok(false); "supported_omitted")]
    #[test_case(true, Some(false) => Ok(false); "supported_foreground")]
    #[test_case(true, Some(true) => Ok(true); "supported_background")]
    fn task_background_follows_the_request_where_supported(
        supported: bool,
        requested: Option<bool>,
    ) -> Result<bool, &'static str> {
        resolve_task_background(supported, requested)
    }

    #[test_case(false, 121 => false; "unsupported_waits")]
    #[test_case(true, 119 => false; "below_threshold_waits")]
    #[test_case(true, 120 => false; "at_threshold_waits")]
    #[test_case(true, 121 => true; "above_threshold_returns_a_receipt")]
    fn shell_background_follows_the_requested_timeout(supported: bool, timeout: u64) -> bool {
        resolve_shell_background(&AgentConfig::default(), supported, timeout)
    }

    #[test_case(SHELL_THRESHOLD_FIELD, "120", Some(1))]
    fn execution_defaults_and_metadata(name: &str, default: &str, min: Option<u64>) {
        let config = RawConfig::default().into_config(false).unwrap();
        assert_eq!(
            config.agent.shell_async_threshold_secs,
            DEFAULT_SHELL_ASYNC_THRESHOLD_SECS
        );
        let field = AgentConfig::FIELDS
            .iter()
            .find(|field| field.name == name)
            .unwrap();
        assert_eq!(field.default.format_default(), default);
        assert_eq!(field.min, min);
    }

    /// What the loader parses: the text upgraded to the latest format first.
    fn parse_permissions(text: &str) -> PermissionsFileConfig {
        let loaded = migration::upgrade_text(text.to_owned(), &PERMISSIONS_FORMAT).unwrap();
        toml::from_str(&loaded.text).unwrap()
    }

    fn write_global_permissions(dir: &Path, content: &str) {
        let perms_dir = dir.join(".config/caudra");
        fs::create_dir_all(&perms_dir).unwrap();
        fs::write(perms_dir.join("permissions.toml"), content).unwrap();
    }

    fn global_config_dir(dir: &Path) -> PathBuf {
        dir.join(".config/caudra")
    }

    #[test_case(false; "global_and_project")]
    #[test_case(true; "global_only")]
    fn permission_sources_use_loaded_paths_and_bytes(global_only: bool) {
        const GLOBAL: &str = "[shell]\nallow = ['git status']\n";
        const PROJECT: &str = "[shell]\ndeny = ['git push *']\n";
        let temp = tempfile::tempdir().unwrap();
        let global = temp.path().join("global");
        let project = temp.path().join("project");
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(project.join(PROJECT_DIR)).unwrap();
        let global_file = global.join(PERMISSIONS_FILE);
        let project_file = project.join(PROJECT_DIR).join(PERMISSIONS_FILE);
        fs::write(&global_file, GLOBAL).unwrap();
        fs::write(&project_file, PROJECT).unwrap();
        let config = load_permissions_scoped(&project, Some(&global), !global_only);
        assert_eq!(config.loaded_sources.len(), if global_only { 1 } else { 2 });
        for source in config.loaded_sources {
            let (path, content) = match source.rule().effect {
                Effect::Allow => (&global_file, GLOBAL),
                Effect::Deny => (&project_file, PROJECT),
                Effect::Ask => panic!("Unexpected ask rule"),
            };
            assert_eq!(source.path(), fs::canonicalize(path).unwrap());
            assert_eq!(
                source.content_digest(),
                permission_source_digest(content.as_bytes())
            );
        }
    }

    #[test_case("[shell]\nallow = ['git status']\n", false; "memory_config_has_no_source")]
    #[test_case("[shell]\nallow = ['git * status']\n", true; "invalid_config_has_no_source")]
    fn permission_source_is_not_inferred_from_parsed_rules(source: &str, from_file: bool) {
        let config = if from_file {
            let temp = tempfile::tempdir().unwrap();
            fs::write(temp.path().join(PERMISSIONS_FILE), source).unwrap();
            load_permissions_scoped(temp.path(), Some(temp.path()), false)
        } else {
            build_permissions(parse_permissions(source), PermissionsFileConfig::default())
        };
        assert!(config.loaded_sources.is_empty());
    }

    #[test]
    fn global_permissions_read_the_environment() {
        const REMOTE: &str = "CAUDRA_TEST_PERMISSIONS_REMOTE";
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(ENV_FILE), format!("{REMOTE}=origin\n")).unwrap();
        fs::write(
            temp.path().join(PERMISSIONS_FILE),
            format!("[shell]\nallow = ['git push {{env:{REMOTE}}}']\n"),
        )
        .unwrap();
        let config = load_permissions_scoped(temp.path(), Some(temp.path()), false);
        assert!(
            config
                .rules
                .iter()
                .any(|rule| rule.scope.as_deref() == Some("git push origin")),
            "{:?}",
            config.rules
        );
    }

    #[test]
    fn project_permissions_that_read_the_environment_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join(PROJECT_DIR);
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join(PERMISSIONS_FILE),
            "[shell]\nallow = ['git push {env:HOME}']\n",
        )
        .unwrap();
        let config = load_permissions_scoped(temp.path(), None, true);
        assert_eq!(config.default, DefaultEffect::Deny);
        assert!(
            config
                .rules
                .iter()
                .all(|rule| rule.tool == ToolKey::Wildcard && rule.effect == Effect::Deny)
        );
    }

    #[test_case("12000", CompactionBuffer::Tokens(12_000) ; "tokens_number")]
    #[test_case("\"20%\"", CompactionBuffer::Percent(20) ; "percent_string")]
    #[test_case("\" 5 %\"", CompactionBuffer::Percent(5) ; "percent_with_spaces")]
    fn compaction_buffer_deserializes(json: &str, expected: CompactionBuffer) {
        let parsed: CompactionBuffer = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, expected);
    }

    #[test_case("500" ; "tokens_below_min")]
    #[test_case("-1" ; "negative_tokens")]
    #[test_case("\"0%\"" ; "zero_percent")]
    #[test_case("\"100%\"" ; "percent_too_high")]
    #[test_case("\"abc%\"" ; "non_numeric_percent")]
    fn compaction_buffer_rejects(json: &str) {
        assert!(serde_json::from_str::<CompactionBuffer>(json).is_err());
    }

    #[test_case(CompactionBuffer::Tokens(10_000), 64_000, 10_000 ; "tokens_ignore_window")]
    #[test_case(CompactionBuffer::Percent(20), 64_000, 12_800 ; "percent_of_window")]
    fn compaction_buffer_resolves(buffer: CompactionBuffer, window: u32, expected: u32) {
        assert_eq!(buffer.resolve(window), expected);
    }

    #[test]
    fn compaction_buffer_serializes_percent_as_string() {
        assert_eq!(
            serde_json::to_value(CompactionBuffer::Percent(20)).unwrap(),
            serde_json::json!("20%")
        );
        assert_eq!(
            serde_json::to_value(CompactionBuffer::Tokens(9_000)).unwrap(),
            serde_json::json!(9_000)
        );
    }

    #[test]
    fn empty_config_returns_defaults() {
        let config = RawConfig::default().into_config(false).unwrap();
        assert!(config.ui.splash_animation);
        assert_eq!(config.ui.notifications, NotificationMethod::Auto);
        assert_eq!(config.agent.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
        assert_eq!(
            config.provider.connect_timeout,
            Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS)
        );
        assert_eq!(
            config.storage.max_log_bytes,
            DEFAULT_MAX_LOG_BYTES_MB * 1024 * 1024
        );
        assert_eq!(
            config.storage.max_eager_load_bytes,
            DEFAULT_MAX_EAGER_LOAD_MB * 1024 * 1024
        );
    }

    #[test]
    fn ui_default_flash_duration_is_ten_seconds() {
        let ui = UiConfig::default();
        assert_eq!(ui.flash_duration_ms, DEFAULT_FLASH_DURATION_MS);
        assert_eq!(ui.flash_duration(), EXPECTED_DEFAULT_FLASH_DURATION);
    }

    #[test_case(""; "empty_config")]
    #[test_case("[ui]"; "empty_ui_table")]
    fn missing_flash_duration_resolves_to_ten_seconds(source: &str) {
        let raw: RawConfig = toml::from_str(source).unwrap();
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.ui.flash_duration_ms, DEFAULT_FLASH_DURATION_MS);
        assert_eq!(config.ui.flash_duration(), EXPECTED_DEFAULT_FLASH_DURATION);
    }

    #[test_case(CUSTOM_FLASH_DURATION_MS; "custom_duration")]
    #[test_case(ZERO_FLASH_DURATION_MS; "zero_duration")]
    fn explicit_flash_duration_is_preserved(milliseconds: u64) {
        let mut raw: RawConfig =
            toml::from_str(&format!("[ui]\nflash_duration_ms = {milliseconds}\n")).unwrap();
        raw.merge(RawConfig::default());
        let config = raw.into_config(false).unwrap();
        config.validate().unwrap();
        assert_eq!(config.ui.flash_duration_ms, milliseconds);
        assert_eq!(
            config.ui.flash_duration(),
            Duration::from_millis(milliseconds)
        );
    }

    /// The ceiling is the only thing standing between a long session and a
    /// refusal to open it, so a user must be able to move it.
    #[test_case(Some(2048), true; "raised")]
    #[test_case(Some(MIN_MAX_EAGER_LOAD_MB - 1), false; "below_the_floor")]
    #[test_case(None, true; "left_alone")]
    fn eager_load_ceiling_is_configurable(megabytes: Option<u64>, valid: bool) {
        let storage = StorageConfig::from_file(StorageFileConfig {
            max_eager_load_mb: megabytes,
            ..StorageFileConfig::default()
        });

        assert_eq!(
            storage.max_eager_load_bytes,
            megabytes.unwrap_or(DEFAULT_MAX_EAGER_LOAD_MB) * 1024 * 1024
        );
        assert_eq!(storage.validate().is_ok(), valid);
    }

    #[test_case("auto", NotificationMethod::Auto ; "auto")]
    #[test_case("osc9", NotificationMethod::Osc9 ; "osc9")]
    #[test_case("bell", NotificationMethod::Bell ; "bell")]
    #[test_case("off", NotificationMethod::Off ; "off")]
    fn notifications_deserialize(value: &str, expected: NotificationMethod) {
        let raw: RawConfig =
            toml::from_str(&format!("[ui]\nnotifications = \"{value}\"\n")).unwrap();
        assert_eq!(raw.into_config(false).unwrap().ui.notifications, expected);
    }

    #[test]
    fn notifications_reject_unknown_value() {
        let result: Result<RawConfig, _> = toml::from_str("[ui]\nnotifications = \"desktop\"\n");
        assert!(result.is_err());
    }

    #[test_case("sunburst", ImageModel::Sunburst ; "sunburst")]
    #[test_case("flare", ImageModel::Flare ; "flare")]
    fn image_model_deserialize(value: &str, expected: ImageModel) {
        let raw: RawConfig =
            toml::from_str(&format!("[agent]\nimage_model = \"{value}\"\n")).unwrap();
        assert_eq!(raw.into_config(false).unwrap().agent.image_model, expected);
    }

    #[test]
    fn image_model_defaults_to_sunburst() {
        let raw: RawConfig = toml::from_str("").unwrap();
        assert_eq!(
            raw.into_config(false).unwrap().agent.image_model,
            ImageModel::Sunburst
        );
    }

    #[test_case("trace", LogLevel::Trace ; "trace")]
    #[test_case("debug", LogLevel::Debug ; "debug")]
    #[test_case("info", LogLevel::Info ; "info")]
    #[test_case("warn", LogLevel::Warn ; "warn")]
    #[test_case("error", LogLevel::Error ; "error")]
    fn log_level_deserialize(value: &str, expected: LogLevel) {
        let raw: RawConfig =
            toml::from_str(&format!("[storage]\nlog_level = \"{value}\"\n")).unwrap();
        assert_eq!(raw.into_config(false).unwrap().storage.log_level, expected);
    }

    #[test]
    fn log_level_rejects_unknown_value() {
        let result: Result<RawConfig, _> = toml::from_str("[storage]\nlog_level = \"verbose\"\n");
        assert!(result.is_err());
    }

    #[test]
    fn partial_agent_config_preserves_unset_fields() {
        let raw = RawConfig {
            agent: AgentFileConfig {
                max_output_lines: Some(5000),
                ..Default::default()
            },
            ..Default::default()
        };
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.agent.max_output_lines, 5000);
        assert_eq!(config.agent.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
    }

    #[test_case(None, true; "default_enabled")]
    #[test_case(Some(false), false; "explicit_disabled")]
    #[test_case(Some(true), true; "explicit_enabled")]
    fn tool_json_repair_config_merges_independently(setting: Option<bool>, expected: bool) {
        let mut raw = RawConfig::default();
        raw.agent.eager_tool_dispatch = Some(false);
        raw.merge(RawConfig {
            agent: AgentFileConfig {
                tool_json_repair: setting,
                ..Default::default()
            },
            ..Default::default()
        });
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.agent.tool_json_repair, expected);
        assert!(!config.agent.eager_tool_dispatch);
    }

    #[test]
    fn builtin_system_prompt_profile_clears_global_selection() {
        let mut global = RawConfig {
            agent: AgentFileConfig {
                system_prompt_profile: Some("review".to_owned()),
                ..Default::default()
            },
            ..Default::default()
        };
        global.merge(RawConfig {
            agent: AgentFileConfig {
                system_prompt_profile: Some("builtin".to_owned()),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(
            global
                .into_config(false)
                .unwrap()
                .agent
                .system_prompt_profile,
            None
        );
    }

    #[test_case(None, None, true ; "default")]
    #[test_case(None, Some(false), false ; "legacy_fallback")]
    #[test_case(Some(true), Some(false), true ; "new_enabled_precedence")]
    #[test_case(Some(false), Some(true), false ; "new_disabled_precedence")]
    fn eager_tool_dispatch_config_precedence(new: Option<bool>, old: Option<bool>, expected: bool) {
        let mut raw = RawConfig::default();
        raw.merge(RawConfig {
            agent: AgentFileConfig {
                eager_tool_dispatch: new,
                eager_batch_dispatch: old,
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(
            raw.into_config(false).unwrap().agent.eager_tool_dispatch,
            expected
        );
    }

    #[test]
    fn merge_overlay_wins_field_by_field() {
        let mut base = RawConfig {
            ui: UiFileConfig {
                splash_animation: Some(false),
                notifications: Some(NotificationMethod::Bell),
                flash_duration_ms: Some(2000),
                ..Default::default()
            },
            agent: AgentFileConfig {
                max_output_lines: Some(3000),
                max_output_bytes: Some(80_000),
                ..Default::default()
            },
            ..Default::default()
        };
        let overlay = RawConfig {
            ui: UiFileConfig {
                notifications: Some(NotificationMethod::Off),
                ..Default::default()
            },
            agent: AgentFileConfig {
                max_output_lines: Some(5000),
                ..Default::default()
            },
            ..Default::default()
        };
        base.merge(overlay);

        assert_eq!(base.agent.max_output_lines, Some(5000), "overlay wins");
        assert_eq!(base.agent.max_output_bytes, Some(80_000), "base preserved");
        assert_eq!(base.ui.splash_animation, Some(false), "base preserved");
        assert_eq!(
            base.ui.notifications,
            Some(NotificationMethod::Off),
            "overlay wins"
        );
        assert_eq!(base.ui.flash_duration_ms, Some(2000), "base preserved");
    }

    #[test]
    fn provider_model_lists_inherit_replace_and_clear() {
        let mut global = RawConfig {
            provider: ProviderFileConfig {
                allowed_models: Some(vec!["anthropic/*".into()]),
                excluded_models: Some(vec!["*/*-preview".into()]),
                ..Default::default()
            },
            ..Default::default()
        };
        global.merge(RawConfig {
            provider: ProviderFileConfig {
                allowed_models: Some(Vec::new()),
                excluded_models: None,
                ..Default::default()
            },
            ..Default::default()
        });

        let provider = global.into_config(false).unwrap().provider;
        assert!(provider.allowed_models.is_empty());
        assert_eq!(provider.excluded_models, ["*/*-preview"]);
        assert!(provider.model_policy.allows("openai/gpt-5"));
        assert!(!provider.model_policy.allows("openai/gpt-5-preview"));
    }

    #[test]
    fn model_policy_matches_qualified_specs() {
        let config = RawConfig {
            provider: ProviderFileConfig {
                allowed_models: Some(vec!["openai/gpt-5".into(), "opencode/*".into()]),
                excluded_models: Some(vec!["*/*-preview".into()]),
                ..Default::default()
            },
            ..Default::default()
        }
        .into_config(false)
        .unwrap();
        let policy = &config.provider.model_policy;

        assert!(policy.allows("openai/gpt-5"));
        assert!(policy.allows("opencode/nvidia/openai/gpt-oss-120b"));
        assert!(!policy.allows("anthropic/claude-sonnet-4-6"));
        assert!(!policy.allows("opencode/gpt-5-preview"));

        let exclude_only = RawConfig {
            provider: ProviderFileConfig {
                excluded_models: Some(vec!["anthropic/*".into()]),
                ..Default::default()
            },
            ..Default::default()
        }
        .into_config(false)
        .unwrap();
        assert!(exclude_only.provider.model_policy.allows("openai/gpt-5"));
        assert!(
            !exclude_only
                .provider
                .model_policy
                .allows("anthropic/claude-sonnet-4-6")
        );
    }

    #[test]
    fn invalid_model_pattern_is_a_config_error() {
        let result = RawConfig {
            provider: ProviderFileConfig {
                allowed_models: Some(vec!["[".into()]),
                ..Default::default()
            },
            ..Default::default()
        }
        .into_config(false);

        assert!(matches!(
            result,
            Err(ConfigError::InvalidModelPattern { field: "allowed_models", pattern, .. }) if pattern == "["
        ));
    }

    #[test]
    fn merge_always_flags_overlay_wins() {
        let mut base = RawConfig {
            always_fast: Some(false),
            always_thinking: Some(AlwaysThinking::Mode("off".into())),
            ..Default::default()
        };
        let overlay = RawConfig {
            always_fast: Some(true),
            always_thinking: Some(AlwaysThinking::Toggle(true)),
            ..Default::default()
        };
        base.merge(overlay);

        assert_eq!(base.always_fast, Some(true), "overlay wins");
        assert_eq!(
            base.always_thinking,
            Some(AlwaysThinking::Toggle(true)),
            "overlay wins"
        );
    }

    #[test_case("", false, false; "defaults")]
    #[test_case("always_auto = true", true, false; "global_auto")]
    #[test_case("always_yolo = true", false, true; "global_yolo")]
    #[test_case("always_auto = true\nalways_yolo = true", true, true; "startup_chooses_precedence")]
    fn global_permission_mode_defaults_and_resolution(source: &str, auto: bool, yolo: bool) {
        let raw: RawConfig = toml::from_str(source).unwrap();
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.always_auto, auto);
        assert_eq!(config.always_yolo, yolo);
    }

    #[test_case("always_auto", true)]
    #[test_case("always_auto", false)]
    #[test_case("always_yolo", true)]
    #[test_case("always_yolo", false)]
    fn project_permission_mode_values_are_rejected(field: &str, value: bool) {
        for source in ["", "always_auto = true", "always_yolo = true"] {
            let mut global: RawConfig = toml::from_str(source).unwrap();
            let expected = (global.always_auto, global.always_yolo);
            let project: RawConfig = toml::from_str(&format!("{field} = {value}")).unwrap();
            global.merge(project);
            global.merge(RawConfig::default());
            assert_eq!((global.always_auto, global.always_yolo), expected);
            assert!(
                matches!(global.into_config(false), Err(ConfigError::ProjectPermissionMode(actual)) if actual == field)
            );
        }
    }

    #[test_case("always_auto = true", true, false)]
    #[test_case("always_yolo = true", false, true)]
    fn project_unrelated_settings_preserve_global_permission_mode(
        source: &str,
        auto: bool,
        yolo: bool,
    ) {
        let mut global: RawConfig = toml::from_str(source).unwrap();
        global.merge(toml::from_str("always_fast = true").unwrap());
        let config = global.into_config(false).unwrap();
        assert_eq!(config.always_auto, auto);
        assert_eq!(config.always_yolo, yolo);
        assert!(config.always_fast);
    }

    #[test_case(AlwaysThinking::Toggle(true), StoredThinking::Adaptive ; "toggle_true")]
    #[test_case(AlwaysThinking::Toggle(false), StoredThinking::Off ; "toggle_false")]
    #[test_case(AlwaysThinking::Budget(8192), StoredThinking::Budget { tokens: 8192 } ; "budget_number")]
    #[test_case(AlwaysThinking::Mode("xhigh".into()), StoredThinking::Effort { level: "xhigh".into() } ; "effort_xhigh")]
    #[test_case(AlwaysThinking::Mode("minimal".into()), StoredThinking::Effort { level: "minimal".into() } ; "effort_minimal")]
    fn always_thinking_toggle_resolve(input: AlwaysThinking, expected: StoredThinking) {
        assert_eq!(input.resolve(), Ok(expected));
    }

    #[test]
    fn into_config_resolves_always_thinking() {
        let defaults = RawConfig::default().into_config(false).unwrap();
        assert!(defaults.always_thinking.is_none());

        let raw = RawConfig {
            always_thinking: Some(AlwaysThinking::Mode("8192".into())),
            ..Default::default()
        };
        let config = raw.into_config(false).unwrap();
        assert_eq!(
            config.always_thinking,
            Some(StoredThinking::Budget { tokens: 8192 })
        );

        let raw = RawConfig {
            always_thinking: Some(AlwaysThinking::Mode("fast".into())),
            ..Default::default()
        };
        let err = raw.into_config(false).err().expect("expected config error");
        assert!(matches!(err, ConfigError::Thinking(_)));
    }

    #[test_case("max_output_bytes",  0 ; "zero_output_bytes")]
    #[test_case("max_output_lines",  0 ; "zero_output_lines")]
    #[test_case("max_output_bytes",  500 ; "below_min_output_bytes")]
    fn validate_rejects_invalid_agent(field: &str, value: usize) {
        let mut config = AgentConfig::default();
        match field {
            "max_output_bytes" => config.max_output_bytes = value,
            "max_output_lines" => config.max_output_lines = value,
            _ => unreachable!(),
        }
        let err = config.validate().unwrap_err();
        assert!(matches!(err, ConfigError::BelowMinimum { field: f, .. } if f == field));
    }

    #[test]
    fn tool_output_lines_per_tool_override() {
        let raw = RawConfig {
            ui: UiFileConfig {
                tool_output_lines: Some(ToolOutputLinesFile {
                    bash: Some(20),
                    read: Some(20),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.ui.tool_output_lines.bash, 20);
        assert_eq!(config.ui.tool_output_lines.get("shell"), 20);
        assert_eq!(config.ui.tool_output_lines.read, 20);
        assert_eq!(
            config.ui.tool_output_lines.index,
            ToolOutputLines::DEFAULT.index
        );
    }

    #[test]
    fn tool_output_lines_code_budget_loads() {
        let raw: RawConfig = toml::from_str("[ui.tool_output_lines]\ncode = 9\n").unwrap();
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.ui.tool_output_lines.get("code"), 9);
    }

    #[test_case(false, None,        true  ; "enabled_by_default")]
    #[test_case(false, Some(false), false ; "disabled_in_config")]
    #[test_case(true,  Some(true),  false ; "cli_flag_forces_disabled")]
    fn shell_output_filter_config(no_rtk: bool, configured: Option<bool>, expected: bool) {
        let raw = RawConfig {
            agent: AgentFileConfig {
                shell_output_filter: configured,
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(
            raw.into_config(no_rtk).unwrap().agent.shell_output_filter,
            expected
        );
    }

    #[test_case("", false, true; "enabled_by_default")]
    #[test_case("shell_workdir_redirect = false", false, false; "disabled_in_config")]
    #[test_case("shell_workdir_redirect = true", false, true; "enabled_in_config")]
    #[test_case("shell_native_redirect = 'off'", false, true; "native_redirect_off_is_independent")]
    #[test_case("shell_workdir_redirect = false\nshell_native_redirect = 'enforce'", false, false; "native_redirect_enforce_is_independent")]
    #[test_case("shell_workdir_redirect = true\nshell_output_filter = false", true, true; "output_filter_and_cli_flag_are_independent")]
    fn shell_workdir_redirect_config(source: &str, no_rtk: bool, expected: bool) {
        let raw: RawConfig = toml::from_str(&format!("[agent]\n{source}")).unwrap();
        let config = raw.into_config(no_rtk).unwrap();
        assert_eq!(config.agent.shell_workdir_redirect, expected);
        assert_eq!(
            serde_json::to_value(&config.agent).unwrap()[SHELL_WORKDIR_REDIRECT_FIELD],
            expected
        );
    }

    #[test_case(false, "", false; "false_survives_empty_overlay")]
    #[test_case(false, "shell_native_redirect = 'off'", false; "false_survives_unrelated_overlay")]
    #[test_case(true, "shell_workdir_redirect = false", false; "overlay_disables")]
    #[test_case(false, "shell_workdir_redirect = true", true; "overlay_enables")]
    fn shell_workdir_redirect_merge(base: bool, overlay: &str, expected: bool) {
        let mut raw: RawConfig =
            toml::from_str(&format!("[agent]\n{SHELL_WORKDIR_REDIRECT_FIELD} = {base}")).unwrap();
        raw.merge(toml::from_str(&format!("[agent]\n{overlay}")).unwrap());
        raw.merge(RawConfig::default());
        assert_eq!(
            raw.into_config(false).unwrap().agent.shell_workdir_redirect,
            expected
        );
    }

    #[test_case(None,        true  ; "titles_generated_by_default")]
    #[test_case(Some(false), false ; "titles_disabled_in_config")]
    fn generate_titles_config(configured: Option<bool>, expected: bool) {
        let raw = RawConfig {
            agent: AgentFileConfig {
                generate_titles: configured,
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(
            raw.into_config(false).unwrap().agent.generate_titles,
            expected
        );
    }

    #[test_case(None,        true  ; "memory_summarized_by_default")]
    #[test_case(Some(false), false ; "summarize_memory_disabled_in_config")]
    fn summarize_memory_config(configured: Option<bool>, expected: bool) {
        let raw = RawConfig {
            agent: AgentFileConfig {
                summarize_memory: configured,
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(
            raw.into_config(false).unwrap().agent.summarize_memory,
            expected
        );
    }

    #[test_case(true, "", true; "true_survives_empty_overlay")]
    #[test_case(true, "generate_titles = false", true; "true_survives_unrelated_overlay")]
    #[test_case(true, "summarize_memory = false", false; "overlay_disables")]
    #[test_case(false, "summarize_memory = true", true; "overlay_enables")]
    fn summarize_memory_merge(base: bool, overlay: &str, expected: bool) {
        let mut raw: RawConfig =
            toml::from_str(&format!("[agent]\nsummarize_memory = {base}")).unwrap();
        raw.merge(toml::from_str(&format!("[agent]\n{overlay}")).unwrap());
        raw.merge(RawConfig::default());
        assert_eq!(
            raw.into_config(false).unwrap().agent.summarize_memory,
            expected
        );
    }

    #[test]
    fn summarize_memory_config_metadata() {
        assert!(AgentConfig::default().summarize_memory);
        let field = AgentConfig::FIELDS
            .iter()
            .find(|f| f.name == "summarize_memory")
            .expect("summarize_memory field not found");
        assert_eq!(field.default.format_default(), "true");
    }

    #[test_case(None,        true  ; "requirements_appended_by_default")]
    #[test_case(Some(false), false ; "requirements_disabled_in_config")]
    fn compaction_requirements_config(configured: Option<bool>, expected: bool) {
        let raw = RawConfig {
            agent: AgentFileConfig {
                compaction_requirements: configured,
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(
            raw.into_config(false)
                .unwrap()
                .agent
                .compaction_requirements,
            expected
        );
    }

    #[test_case("", 0 ; "default_disabled")]
    #[test_case("background_reminder_turns = 0", 0 ; "periodic_disabled")]
    #[test_case("background_reminder_turns = 13", CUSTOM_BACKGROUND_REMINDER_TURNS ; "custom")]
    fn background_reminder_config(source: &str, expected: u32) {
        let raw: RawConfig = toml::from_str(&format!("[agent]\n{source}")).unwrap();
        let config = raw.into_config(false).unwrap();
        config.validate().unwrap();
        assert_eq!(config.agent.background_reminder_turns, expected);
        assert_eq!(
            serde_json::to_value(&config.agent).unwrap()[BACKGROUND_REMINDER_FIELD],
            expected
        );
    }

    #[test_case(-1_i64 ; "negative")]
    #[test_case(i64::from(u32::MAX) + 1 ; "overflow")]
    fn background_reminder_config_rejects_invalid_unsigned(value: i64) {
        let source = format!("[agent]\n{BACKGROUND_REMINDER_FIELD} = {value}");
        let error = toml::from_str::<RawConfig>(&source).unwrap_err();
        assert!(error.to_string().contains(UNSIGNED_REMINDER_ERROR));
    }

    #[test_case(CUSTOM_BACKGROUND_REMINDER_TURNS, 0 ; "disable_on_reload")]
    #[test_case(0, CUSTOM_BACKGROUND_REMINDER_TURNS ; "enable_on_reload")]
    fn background_reminder_config_merge_and_reload(initial: u32, updated: u32) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        for value in [initial, updated] {
            fs::write(
                &path,
                format!("[agent]\n{BACKGROUND_REMINDER_FIELD} = {value}"),
            )
            .unwrap();
            let mut raw = RawConfig::default();
            raw.merge(toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap());
            raw.merge(RawConfig::default());
            let config = raw.into_config(false).unwrap();
            config.validate().unwrap();
            assert_eq!(config.agent.background_reminder_turns, value);
        }
        let mut raw: RawConfig =
            toml::from_str(&format!("[agent]\n{BACKGROUND_REMINDER_FIELD} = {initial}")).unwrap();
        raw.merge(
            toml::from_str(&format!("[agent]\n{BACKGROUND_REMINDER_FIELD} = {updated}")).unwrap(),
        );
        assert_eq!(
            raw.into_config(false)
                .unwrap()
                .agent
                .background_reminder_turns,
            updated
        );
    }

    #[test_case(DEFAULT_BACKGROUND_REMINDER_TURNS)]
    fn background_reminder_config_metadata(default: u32) {
        assert_eq!(AgentConfig::default().background_reminder_turns, default);
        let field = AgentConfig::FIELDS
            .iter()
            .find(|field| field.name == BACKGROUND_REMINDER_FIELD)
            .unwrap();
        assert_eq!(field.ty, "u32");
        assert_eq!(field.default.format_default(), default.to_string());
        assert_eq!(field.min, None);
        assert!(
            field
                .description
                .contains("0 disables periodic refresh only")
        );
        assert!(
            field
                .description
                .contains("state-change or post-compaction")
        );
    }

    #[test_case("", true ; "enabled_by_default")]
    #[test_case("todo_reminder = true", true ; "explicitly_enabled")]
    #[test_case("todo_reminder = false", false ; "disabled")]
    fn todo_reminder_config(source: &str, expected: bool) {
        let raw: RawConfig = toml::from_str(&format!("[agent]\n{source}")).unwrap();
        let config = raw.into_config(false).unwrap();
        config.validate().unwrap();
        assert_eq!(config.agent.todo_reminder, expected);
        assert_eq!(
            serde_json::to_value(&config.agent).unwrap()[TODO_REMINDER_FIELD],
            expected
        );
    }

    #[test_case(Some(false), None, false ; "omitted_overlay_keeps_disabled")]
    #[test_case(Some(false), Some(true), true ; "overlay_reenables")]
    #[test_case(None, Some(false), false ; "overlay_disables")]
    fn todo_reminder_config_merge(base: Option<bool>, overlay: Option<bool>, expected: bool) {
        let layer = |todo_reminder| RawConfig {
            agent: AgentFileConfig {
                todo_reminder,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut raw = layer(base);
        raw.merge(layer(overlay));
        assert_eq!(
            raw.into_config(false).unwrap().agent.todo_reminder,
            expected
        );
    }

    #[test_case("", true ; "enabled_by_default")]
    #[test_case("task_model_choice = false", false ; "disabled")]
    fn task_model_choice_config(source: &str, expected: bool) {
        let raw: RawConfig = toml::from_str(&format!("[agent]\n{source}")).unwrap();
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.agent.task_model_choice, expected);
        assert_eq!(
            serde_json::to_value(&config.agent).unwrap()[TASK_MODEL_CHOICE_FIELD],
            expected
        );
    }

    #[test_case(Some(false), None, false ; "omitted_overlay_keeps_disabled")]
    #[test_case(None, Some(false), false ; "overlay_disables")]
    fn task_model_choice_config_merge(base: Option<bool>, overlay: Option<bool>, expected: bool) {
        let layer = |task_model_choice| RawConfig {
            agent: AgentFileConfig {
                task_model_choice,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut raw = layer(base);
        raw.merge(layer(overlay));
        assert_eq!(
            raw.into_config(false).unwrap().agent.task_model_choice,
            expected
        );
    }

    #[test_case("", true ; "enabled_by_default")]
    #[test_case("deferred_questions = false", false ; "disabled")]
    fn deferred_questions_config(source: &str, expected: bool) {
        let raw: RawConfig = toml::from_str(&format!("[agent]\n{source}")).unwrap();
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.agent.deferred_questions, expected);
        assert_eq!(
            serde_json::to_value(&config.agent).unwrap()[DEFERRED_QUESTIONS_FIELD],
            expected
        );
    }

    #[test_case("", true ; "enabled_by_default")]
    #[test_case("shell_eta = false", false ; "disabled")]
    fn shell_eta_config(source: &str, expected: bool) {
        let raw: RawConfig = toml::from_str(&format!("[agent]\n{source}")).unwrap();
        assert_eq!(raw.into_config(false).unwrap().agent.shell_eta, expected);
    }

    #[test]
    fn todo_reminder_config_rejects_non_boolean() {
        let source = format!("[agent]\n{TODO_REMINDER_FIELD} = \"off\"");
        let error = toml::from_str::<RawConfig>(&source).unwrap_err();
        assert!(error.to_string().contains(BOOLEAN_EXPECTED_ERROR));
    }

    #[test]
    fn todo_reminder_metadata_matches_runtime_default() {
        let field = AgentConfig::FIELDS
            .iter()
            .find(|field| field.name == TODO_REMINDER_FIELD)
            .unwrap();
        assert_eq!(field.ty, "bool");
        assert_eq!(
            field.default.format_default(),
            AgentConfig::default().todo_reminder.to_string()
        );
    }

    #[test_case("", DEFAULT_WORK_NUDGE_COMPACTIONS ; "enabled_by_default")]
    #[test_case("work_nudge_compactions = 0", 0 ; "disabled")]
    #[test_case("work_nudge_compactions = 5", 5 ; "custom")]
    fn work_nudge_config(source: &str, expected: u32) {
        let raw: RawConfig = toml::from_str(&format!("[agent]\n{source}")).unwrap();
        let config = raw.into_config(false).unwrap();
        config.validate().unwrap();
        assert_eq!(config.agent.work_nudge_compactions, expected);
        assert_eq!(
            serde_json::to_value(&config.agent).unwrap()[WORK_NUDGE_FIELD],
            expected
        );
    }

    #[test_case(Some(0), None, 0 ; "omitted_overlay_keeps_disabled")]
    #[test_case(Some(0), Some(3), 3 ; "overlay_reenables")]
    #[test_case(None, Some(0), 0 ; "overlay_disables")]
    fn work_nudge_config_merge(base: Option<u32>, overlay: Option<u32>, expected: u32) {
        let layer = |work_nudge_compactions| RawConfig {
            agent: AgentFileConfig {
                work_nudge_compactions,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut raw = layer(base);
        raw.merge(layer(overlay));
        assert_eq!(
            raw.into_config(false).unwrap().agent.work_nudge_compactions,
            expected
        );
    }

    #[test_case("-1" ; "negative")]
    #[test_case("\"off\"" ; "string")]
    fn work_nudge_config_rejects_non_count(value: &str) {
        let source = format!("[agent]\n{WORK_NUDGE_FIELD} = {value}");
        let error = toml::from_str::<RawConfig>(&source).unwrap_err();
        assert!(
            error.to_string().contains(UNSIGNED_REMINDER_ERROR),
            "{error}"
        );
    }

    #[test]
    fn work_nudge_metadata_matches_runtime_default() {
        let field = AgentConfig::FIELDS
            .iter()
            .find(|field| field.name == WORK_NUDGE_FIELD)
            .unwrap();
        assert_eq!(field.ty, "u32");
        assert_eq!(
            field.default.format_default(),
            AgentConfig::default().work_nudge_compactions.to_string()
        );
    }

    const WRONG_TOUCH: &str = "touch mode consulted the terminal when it was told not to";

    #[test_case(TouchMode::Auto, true,  true  ; "auto follows a terminal that reports touch")]
    #[test_case(TouchMode::Auto, false, false ; "auto follows a terminal that does not")]
    #[test_case(TouchMode::On,   false, true  ; "on overrides a terminal that does not")]
    #[test_case(TouchMode::Off,  true,  false ; "off overrides a terminal that does")]
    fn touch_mode_resolves_against_detection(mode: TouchMode, detected: bool, expected: bool) {
        assert_eq!(mode.enabled(|| detected), expected, "{WRONG_TOUCH}");
    }

    #[test_case("provider", "connect_timeout_secs", 0 ; "provider_zero_connect_timeout")]
    #[test_case("storage",  "max_log_files",        0 ; "storage_zero_log_files")]
    #[test_case("ui",       "mouse_scroll_lines",   0 ; "ui_zero_scroll_lines")]
    #[test_case("ui",       "max_input_lines",      0 ; "ui_zero_max_input_lines")]
    #[test_case("agent",    "max_output_lines",     1 ; "agent_output_lines_too_low")]
    fn validate_rejects_invalid_sections(section: &str, field: &str, value: u64) {
        let mut config = Config {
            decisions: DecisionsConfig::default(),
            always_yolo: false,
            always_auto: false,
            always_fast: false,
            always_thinking: None,
            ui: UiConfig::default(),
            agent: AgentConfig::default(),
            provider: ProviderConfig::default(),
            storage: StorageConfig::default(),
            telemetry: TelemetryConfig::default(),
            worktrees: WorktreesConfig::default(),
            automations: AutomationsConfig::default(),
            autopilot: AutopilotConfig::default(),
            web: WebConfig::default(),
            permissions: PermissionsConfig::default(),
            plugins: PluginsConfig::default(),
            websearch: WebsearchConfig::default(),
        };
        match (section, field) {
            ("provider", "connect_timeout_secs") => {
                config.provider.connect_timeout = Duration::from_secs(value)
            }
            ("storage", "max_log_files") => config.storage.max_log_files = value as u32,
            ("ui", "mouse_scroll_lines") => config.ui.mouse_scroll_lines = value as u32,
            ("ui", "max_input_lines") => config.ui.max_input_lines = value as u32,
            ("agent", "max_output_lines") => config.agent.max_output_lines = value as usize,
            _ => unreachable!(),
        }
        let err = config.validate().unwrap_err();
        assert!(matches!(
            err,
            ConfigError::BelowMinimum { section: s, field: f, .. } if s == section && f == field
        ));
    }

    #[cfg(unix)]
    #[test]
    fn permissions_load_from_a_read_only_global_config_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        fs::write(
            global.join("permissions.toml"),
            "[mcp.github]\ndeny = [\"delete\"]\n",
        )
        .unwrap();
        fs::set_permissions(&global, fs::Permissions::from_mode(0o555)).unwrap();
        if fs::write(global.join("probe"), b"x").is_ok() {
            return;
        }

        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        fs::set_permissions(&global, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(perms.rules.len(), 1);
        assert_eq!(perms.rules[0].effect, Effect::Deny);
        assert_eq!(
            perms.rules[0].tool,
            ToolKey::parse("github.delete").unwrap()
        );
    }

    #[test]
    fn permissions_loaded_from_permissions_file() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "default = \"allow\"\n\n\
             [shell]\nallow = [\n    \"cargo *\",\n]\ndeny = [\n    \"rm -rf *\",\n]\n",
        );

        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(perms.default, DefaultEffect::Prompt);
        assert_eq!(perms.rules.len(), 2);
        assert!(perms.rules.contains(&PermissionRule {
            tool: ToolKey::native("shell"),
            scope: Some("cargo *".into()),
            effect: Effect::Allow,
        }));
        assert!(perms.rules.contains(&PermissionRule {
            tool: ToolKey::native("shell"),
            scope: Some("rm -rf *".into()),
            effect: Effect::Deny,
        }));
        assert!(perms.project_allow_rules.is_empty());
        assert!(
            perms
                .review_candidates
                .contains(&PermissionReviewCandidate {
                    source: PermissionSource::Global,
                    kind: PermissionReviewKind::Default,
                    tool: None,
                    scope: None,
                })
        );
        assert_eq!(perms.review_candidates.len(), 1);
    }

    #[test]
    fn permissions_merge_global_and_project() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[shell]\nallow = [\"git *\"]\ndeny = [\"rm -rf *\"]\n",
        );
        let caudra_dir = dir.path().join(".caudra");
        fs::create_dir_all(&caudra_dir).unwrap();
        fs::write(
            caudra_dir.join("permissions.toml"),
            "[read]\nallow = true\n\
             [write]\ndeny = [\"/etc/*\"]\n\
             [shell]\nallow = [\"cargo test *\"]\n",
        )
        .unwrap();

        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(perms.default, DefaultEffect::Prompt);
        assert_eq!(perms.rules.len(), 3);
        assert!(perms.rules.contains(&PermissionRule {
            tool: ToolKey::native("shell"),
            scope: Some("git *".into()),
            effect: Effect::Allow,
        }));
        assert!(perms.rules.contains(&PermissionRule {
            tool: ToolKey::native("shell"),
            scope: Some("rm -rf *".into()),
            effect: Effect::Deny,
        }));
        assert!(perms.rules.contains(&PermissionRule {
            tool: ToolKey::native("write"),
            scope: Some("/etc/*".into()),
            effect: Effect::Deny,
        }));
        assert_eq!(
            perms.project_allow_rules,
            vec![PermissionRule {
                tool: ToolKey::native("shell"),
                scope: Some("cargo test *".into()),
                effect: Effect::Allow,
            }]
        );
        assert!(
            perms
                .review_candidates
                .contains(&PermissionReviewCandidate {
                    source: PermissionSource::Project,
                    kind: PermissionReviewKind::Rule,
                    tool: Some(ToolKey::native("read")),
                    scope: None,
                })
        );
        assert!(
            perms
                .review_candidates
                .contains(&PermissionReviewCandidate {
                    source: PermissionSource::Project,
                    kind: PermissionReviewKind::Rule,
                    tool: Some(ToolKey::native("shell")),
                    scope: Some("cargo test *".into()),
                })
        );
        assert_eq!(perms.review_candidates.len(), 2);
    }

    #[test]
    fn ask_rules_parse_for_native_and_mcp_tools_from_both_sources() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[shell]\nask = [\"git status *\"]\n\
             [mcp.deepwiki]\nask = [\"search\"]\n",
        );
        let caudra_dir = dir.path().join(".caudra");
        fs::create_dir_all(&caudra_dir).unwrap();
        fs::write(
            caudra_dir.join("permissions.toml"),
            "[shell]\nask = [\"cargo test *\"]\n\
             [mcp.github]\nask = true\n",
        )
        .unwrap();

        let permissions = load_permissions_inner(dir.path(), Some(global.as_path()));

        assert_eq!(permissions.rules.len(), 4);
        assert!(permissions.rules.contains(&PermissionRule {
            tool: ToolKey::native("shell"),
            scope: Some("git status *".into()),
            effect: Effect::Ask,
        }));
        assert!(permissions.rules.contains(&PermissionRule {
            tool: ToolKey::parse("deepwiki.search").unwrap(),
            scope: None,
            effect: Effect::Ask,
        }));
        assert!(permissions.rules.contains(&PermissionRule {
            tool: ToolKey::native("shell"),
            scope: Some("cargo test *".into()),
            effect: Effect::Ask,
        }));
        assert!(permissions.rules.contains(&PermissionRule {
            tool: ToolKey::McpServer {
                server: "github".into(),
            },
            scope: None,
            effect: Effect::Ask,
        }));
        assert!(permissions.project_allow_rules.is_empty());
        assert!(permissions.review_candidates.is_empty());
    }

    #[test_case("git status *" ; "bare_final_wildcard")]
    #[test_case("./bin_tool@host:key=value+next-1" ; "all_literal_characters")]
    #[test_case("a b c d e f g h" ; "eight_tokens")]
    #[test_case("gh api repos/tensorninja/*" ; "path_component")]
    #[test_case("gh api repos/tensorninja/* *" ; "path_component_and_any_arguments")]
    fn shell_command_pattern_accepts_token_grammar(pattern: &str) {
        assert_eq!(validate_shell_command_pattern(pattern), Ok(()));
    }

    #[test_case("", "1 to 8 tokens" ; "empty")]
    #[test_case("   ", "1 to 8 tokens" ; "whitespace_only")]
    #[test_case("git status*", "bare final token" ; "attached_wildcard")]
    #[test_case("git * status", "wildcard must be the final token" ; "non_final_wildcard")]
    #[test_case("\"git\" status", "invalid character" ; "quoted")]
    #[test_case("git\tstatus", "control character" ; "control_character")]
    #[test_case("a b c d e f g h i", "1 to 8 tokens" ; "nine_tokens")]
    #[test_case("git café", "only ASCII" ; "non_ascii_literal")]
    fn shell_command_pattern_rejects_invalid_grammar(pattern: &str, expected: &str) {
        let error = validate_shell_command_pattern(pattern).unwrap_err();
        assert!(error.contains(expected), "{error}");
    }

    #[test]
    fn shell_command_pattern_rejects_more_than_256_utf8_bytes() {
        let pattern = "a".repeat(caudra_storage::permission_state::COMMAND_PATTERN_MAX_BYTES + 1);
        assert_eq!(
            validate_shell_command_pattern(&pattern),
            Err("command pattern exceeds 256 UTF-8 bytes".into())
        );
    }

    #[test]
    fn invalid_shell_allow_or_ask_patterns_fail_closed() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[shell]\n\
             allow = [\"git status *\", \"git status*\"]\n\
             ask = [\"cargo test *\", \"git * status\"]\n\
             deny = [\"rm; -rf *\", \"\"]\n",
        );
        let caudra_dir = dir.path().join(".caudra");
        fs::create_dir_all(&caudra_dir).unwrap();
        fs::write(
            caudra_dir.join("permissions.toml"),
            "[shell]\nallow = [\"cargo check *\", \"cargo check*\"]\n",
        )
        .unwrap();

        let permissions = load_permissions_inner(dir.path(), Some(global.as_path()));

        assert_eq!(
            permissions.rules,
            [PermissionRule {
                tool: ToolKey::Wildcard,
                scope: None,
                effect: Effect::Deny,
            }]
        );
        assert_eq!(permissions.default, DefaultEffect::Deny);
        assert!(permissions.project_allow_rules.is_empty());
        assert!(permissions.review_candidates.is_empty());
    }

    #[test]
    fn project_default_allow_ignored() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        let caudra_dir = dir.path().join(".caudra");
        fs::create_dir_all(&caudra_dir).unwrap();
        fs::write(caudra_dir.join("permissions.toml"), "default = \"allow\"\n").unwrap();

        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(perms.default, DefaultEffect::Prompt);
        assert_eq!(
            perms.review_candidates,
            vec![PermissionReviewCandidate {
                source: PermissionSource::Project,
                kind: PermissionReviewKind::Default,
                tool: None,
                scope: None,
            }]
        );
    }

    #[test]
    fn project_wildcard_allow_cannot_override_global_deny() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "default = \"deny\"\n");
        let caudra_dir = dir.path().join(".caudra");
        fs::create_dir_all(&caudra_dir).unwrap();
        fs::write(
            caudra_dir.join("permissions.toml"),
            "[\"*\"]\nallow = true\n",
        )
        .unwrap();

        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(perms.default, DefaultEffect::Deny);
        assert!(perms.rules.is_empty());
        assert!(perms.project_allow_rules.is_empty());
        assert_eq!(
            perms.review_candidates,
            vec![PermissionReviewCandidate {
                source: PermissionSource::Project,
                kind: PermissionReviewKind::Rule,
                tool: Some(ToolKey::Wildcard),
                scope: None,
            }]
        );
    }

    #[test]
    fn global_shell_allow_is_active_while_other_allows_remain_review_candidates() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "default = \"allow\"\n\
             [\"*\"]\nallow = [\"*\"]\ndefault = \"allow\"\n\
             [shell]\nallow = [\"*\", \"git status *\"]\ndefault = \"allow\"\n\
             [read]\nallow = [\"src/**\"]\n\
             [mcp.deepwiki]\nallow = true\ndefault = \"allow\"\n\
             [mcp.github]\nallow = [\"search\", \"*\"]\n",
        );

        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(perms.default, DefaultEffect::Prompt);
        assert_eq!(perms.rules.len(), 2);
        assert!(perms.rules.contains(&PermissionRule {
            tool: ToolKey::native("shell"),
            scope: Some("*".into()),
            effect: Effect::Allow,
        }));
        assert!(perms.rules.contains(&PermissionRule {
            tool: ToolKey::native("shell"),
            scope: Some("git status *".into()),
            effect: Effect::Allow,
        }));
        assert!(perms.project_allow_rules.is_empty());
        assert!(perms.tool_defaults.is_empty());
        assert_eq!(perms.review_candidates.len(), 9);
        assert!(
            !perms
                .review_candidates
                .contains(&PermissionReviewCandidate {
                    source: PermissionSource::Global,
                    kind: PermissionReviewKind::Rule,
                    tool: Some(ToolKey::native("shell")),
                    scope: None,
                })
        );
        assert!(
            perms
                .review_candidates
                .contains(&PermissionReviewCandidate {
                    source: PermissionSource::Global,
                    kind: PermissionReviewKind::Rule,
                    tool: Some(ToolKey::native("read")),
                    scope: Some("src/**".into()),
                })
        );
        assert!(
            perms
                .review_candidates
                .contains(&PermissionReviewCandidate {
                    source: PermissionSource::Global,
                    kind: PermissionReviewKind::Default,
                    tool: Some(ToolKey::Wildcard),
                    scope: None,
                })
        );
        assert!(
            perms
                .review_candidates
                .contains(&PermissionReviewCandidate {
                    source: PermissionSource::Global,
                    kind: PermissionReviewKind::Default,
                    tool: Some(ToolKey::McpServer {
                        server: "deepwiki".into(),
                    }),
                    scope: None,
                })
        );
    }

    #[test]
    fn all_deny_and_non_allow_default_forms_remain_active() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "default = \"deny\"\n\
             [\"*\"]\ndeny = true\n\
             [shell]\ndeny = [\"rm *\"]\ndefault = \"prompt\"\n\
             [write]\ndeny = true\ndefault = \"deny\"\n\
             [mcp.server]\ndeny = true\ndefault = \"prompt\"\n\
             [mcp.github]\ndeny = [\"delete\", \"*\"]\ndefault = \"deny\"\n",
        );

        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(perms.default, DefaultEffect::Deny);
        assert!(perms.review_candidates.is_empty());
        assert_eq!(perms.rules.len(), 6);
        assert!(perms.rules.iter().all(|rule| rule.effect == Effect::Deny));
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::native("shell")),
            Some(&DefaultEffect::Prompt)
        );
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::native("write")),
            Some(&DefaultEffect::Deny)
        );
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::McpServer {
                server: "server".into(),
            }),
            Some(&DefaultEffect::Prompt)
        );
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::McpServer {
                server: "github".into(),
            }),
            Some(&DefaultEffect::Deny)
        );
        assert!(
            perms
                .rules
                .iter()
                .any(|rule| rule.tool == ToolKey::Wildcard)
        );
    }

    #[test]
    fn malformed_project_permissions_fail_closed_instead_of_inheriting_global_allows() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[shell]\nallow = [\"git status *\"]\n");
        let project_dir = dir.path().join(PROJECT_DIR);
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(
            project_dir.join(PERMISSIONS_FILE),
            "[shell]\ndeny = [\"git push *\"]\nask = 1\n",
        )
        .unwrap();

        let permissions = load_permissions_inner(dir.path(), Some(global.as_path()));

        assert_eq!(permissions.default, DefaultEffect::Deny);
        assert_eq!(
            permissions.rules,
            [PermissionRule {
                tool: ToolKey::Wildcard,
                scope: None,
                effect: Effect::Deny,
            }]
        );
    }

    #[test]
    fn an_unversioned_permissions_file_reads_as_the_first_version() {
        const RULES: &str = "[shell]\ndeny = [\"git push *\"]\n";
        let versioned: PermissionsFileConfig = toml::from_str(&format!(
            "{CONFIG_VERSION_KEY} = {PERMISSIONS_VERSION}\n{RULES}"
        ))
        .unwrap();
        let unversioned = parse_permissions(RULES);

        assert_eq!(
            build_permissions(versioned, PermissionsFileConfig::default()).rules,
            build_permissions(unversioned, PermissionsFileConfig::default()).rules
        );
    }

    #[test]
    fn a_legacy_permissions_file_renames_its_tool_tables() {
        const LEGACY: &str = "\
# Team permissions

default = \"prompt\"
code_refs.default = \"allow\"

# never push from the agent
[bash]
allow = [\"git status *\"]
deny = [\"git push *\"]

[code_context]
default = \"allow\"

[python_execution]
deny = true

[\"*\"]
default = \"prompt\"

[mcp.github]
allow = [\"list_issues\"]
";
        const UPGRADED: &str = "\
# Team permissions

# Added by Caudra 9.9.9 on 2026-10-20: the format version of this file, which Caudra now requires.
version = 2

default = \"prompt\"
# Upgraded by Caudra 9.9.9 on 2026-10-20: renamed `code_refs` to `source_code_refs`, because the code-graph tools are now `source_code_context` and `source_code_refs`.
source_code_refs.default = \"allow\"

# Upgraded by Caudra 9.9.9 on 2026-10-20: renamed `bash` to `shell`, because the `bash` tool is now `shell`.
#
# never push from the agent
[shell]
allow = [\"git status *\"]
deny = [\"git push *\"]

# Upgraded by Caudra 9.9.9 on 2026-10-20: renamed `code_context` to `source_code_context`, because the code-graph tools are now `source_code_context` and `source_code_refs`.
[source_code_context]
default = \"allow\"

# Upgraded by Caudra 9.9.9 on 2026-10-20: renamed `python_execution` to `code`, because the `python_execution` tool is now `code`.
[code]
deny = true

[\"*\"]
default = \"prompt\"

[mcp.github]
allow = [\"list_issues\"]
";

        let text = migration::migrate(LEGACY, &PERMISSIONS_FORMAT, &migration::test_stamp())
            .unwrap()
            .text
            .unwrap();
        let config: PermissionsFileConfig = toml::from_str(&text).unwrap();

        assert_eq!(text, UPGRADED);
        assert_eq!(
            config
                .tools
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "*",
                CODE_TOOL_NAME,
                "shell",
                "source_code_context",
                "source_code_refs"
            ])
        );
        assert_eq!(
            validate_project_permissions(Path::new(PERMISSIONS_FILE), &text),
            Ok(())
        );
    }

    #[test]
    fn a_legacy_bash_table_beside_a_shell_table_is_a_conflict() {
        const BOTH: &str = "[bash]\ndeny = [\"git push *\"]\n\n[shell]\nallow = [\"ls\"]\n";

        assert_eq!(
            migration::migrate(BOTH, &PERMISSIONS_FORMAT, &migration::test_stamp()),
            Err(MigrationError::Conflict {
                file: PERMISSIONS_FILE,
                old: "bash".to_owned(),
                new: "shell".to_owned(),
            })
        );
    }

    #[test]
    fn the_permissions_parser_requires_the_version() {
        let expected = ConfigVersionError::Missing {
            latest: PERMISSIONS_VERSION,
        };

        let Err(error) = toml::from_str::<PermissionsFileConfig>("[shell]\n") else {
            panic!("a file without a version parsed");
        };

        assert!(error.to_string().contains(&expected.to_string()), "{error}");
    }

    #[test_case(true ; "global")]
    #[test_case(false ; "project")]
    fn newer_permissions_version_fails_closed(global_file: bool) {
        const GLOBAL_ALLOW: &str = "[shell]\nallow = [\"git status *\"]\n";
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        let newer = format!(
            "{CONFIG_VERSION_KEY} = {}\n{GLOBAL_ALLOW}",
            PERMISSIONS_VERSION + 1
        );
        if global_file {
            write_global_permissions(dir.path(), &newer);
        } else {
            write_global_permissions(dir.path(), GLOBAL_ALLOW);
            let project_dir = dir.path().join(PROJECT_DIR);
            fs::create_dir_all(&project_dir).unwrap();
            fs::write(project_dir.join(PERMISSIONS_FILE), &newer).unwrap();
        }

        let permissions = load_permissions_inner(dir.path(), Some(global.as_path()));

        assert_eq!(permissions.default, DefaultEffect::Deny);
        assert_eq!(
            permissions.rules,
            [PermissionRule {
                tool: ToolKey::Wildcard,
                scope: None,
                effect: Effect::Deny,
            }]
        );
    }

    #[test]
    fn project_trust_material_includes_denies_and_asks() {
        let global = PermissionsFileConfig::default();
        let project = parse_permissions(
            "[shell]\nallow = [\"git *\"]\nask = [\"git push *\"]\ndeny = [\"git clean *\"]\n",
        );

        let permissions = build_permissions(global, project);

        assert_eq!(permissions.project_allow_rules.len(), 1);
        assert_eq!(permissions.project_restrictive_rules.len(), 2);
        assert!(
            permissions
                .project_restrictive_rules
                .iter()
                .any(|rule| rule.effect == Effect::Ask)
        );
        assert!(
            permissions
                .project_restrictive_rules
                .iter()
                .any(|rule| rule.effect == Effect::Deny)
        );
    }

    #[test]
    fn no_permissions_file_returns_defaults() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(perms.default, DefaultEffect::Prompt);
        assert!(perms.rules.is_empty());
        assert!(perms.project_allow_rules.is_empty());
    }

    #[test]
    fn global_allow_and_deny_rules_are_active() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[shell]\nallow = [\"git *\"]\ndeny = [\"rm *\"]\n",
        );

        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(perms.rules.len(), 2);
        assert!(perms.rules.iter().any(|rule| rule.effect == Effect::Allow));
        assert!(perms.rules.iter().any(|rule| rule.effect == Effect::Deny));
        assert!(perms.project_allow_rules.is_empty());
        assert!(perms.review_candidates.is_empty());
    }

    #[test]
    fn permissions_default_deny_global() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "default = \"deny\"\n");

        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(perms.default, DefaultEffect::Deny);
    }

    #[test]
    fn permissions_default_per_tool() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "default = \"deny\"\n\n[shell]\ndefault = \"allow\"\nallow = [\"cargo *\"]\n",
        );

        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(perms.default, DefaultEffect::Deny);
        assert!(!perms.tool_defaults.contains_key(&ToolKey::native("shell")));
        assert_eq!(perms.review_candidates.len(), 1);
        assert!(perms.rules.contains(&PermissionRule {
            tool: ToolKey::native("shell"),
            scope: Some("cargo *".into()),
            effect: Effect::Allow,
        }));
        assert!(
            perms
                .review_candidates
                .contains(&PermissionReviewCandidate {
                    source: PermissionSource::Global,
                    kind: PermissionReviewKind::Default,
                    tool: Some(ToolKey::native("shell")),
                    scope: None,
                })
        );
    }

    #[test]
    fn permissions_default_merge_project_overrides_global_per_tool() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[shell]\ndefault = \"allow\"\n");
        let caudra_dir = dir.path().join(".caudra");
        fs::create_dir_all(&caudra_dir).unwrap();
        fs::write(
            caudra_dir.join("permissions.toml"),
            "[shell]\ndefault = \"deny\"\n",
        )
        .unwrap();

        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::native("shell")).copied(),
            Some(DefaultEffect::Deny)
        );
    }

    #[test]
    fn project_prompt_defaults_cannot_weaken_global_denies() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "default = \"deny\"\n\n[shell]\ndefault = \"deny\"\n",
        );
        let caudra_dir = dir.path().join(".caudra");
        fs::create_dir_all(&caudra_dir).unwrap();
        fs::write(
            caudra_dir.join("permissions.toml"),
            "default = \"prompt\"\n\n[shell]\ndefault = \"prompt\"\n",
        )
        .unwrap();

        let permissions = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(permissions.default, DefaultEffect::Deny);
        assert_eq!(
            permissions
                .tool_defaults
                .get(&ToolKey::native("shell"))
                .copied(),
            Some(DefaultEffect::Deny)
        );
    }

    #[test]
    fn project_default_deny_allowed() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        let caudra_dir = dir.path().join(".caudra");
        fs::create_dir_all(&caudra_dir).unwrap();
        fs::write(caudra_dir.join("permissions.toml"), "default = \"deny\"\n").unwrap();

        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(perms.default, DefaultEffect::Deny);
    }

    #[test]
    fn global_permissions_loader_ignores_project_rules() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "default = \"allow\"\n");
        let caudra_dir = dir.path().join(".caudra");
        fs::create_dir_all(&caudra_dir).unwrap();
        fs::write(caudra_dir.join("permissions.toml"), "default = \"deny\"\n").unwrap();

        let permissions = load_permissions_scoped(dir.path(), Some(&global), false);

        assert_eq!(permissions.default, DefaultEffect::Prompt);
    }

    #[test]
    fn env_file_precedence() {
        const GLOBAL_ONLY: &str = "TEST_CAUDRA_GLOBAL_ONLY";
        const PROJECT_SHADOWS: &str = "TEST_CAUDRA_PROJECT_SHADOWS";
        const PROCESS_WINS: &str = "TEST_CAUDRA_PROCESS_WINS";

        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        fs::write(
            global.join(".env"),
            format!("{GLOBAL_ONLY}=global\n{PROJECT_SHADOWS}=global\n{PROCESS_WINS}=global"),
        )
        .unwrap();

        let caudra_dir = dir.path().join(".caudra");
        fs::create_dir_all(&caudra_dir).unwrap();
        fs::write(
            caudra_dir.join(".env"),
            format!("{PROJECT_SHADOWS}=project\n{PROCESS_WINS}=project"),
        )
        .unwrap();

        unsafe {
            std::env::remove_var(GLOBAL_ONLY);
            std::env::remove_var(PROJECT_SHADOWS);
            std::env::set_var(PROCESS_WINS, "process");
        }

        load_env_files_with_global(dir.path(), Some(&global));

        assert_eq!(std::env::var(GLOBAL_ONLY).unwrap(), "global");
        assert_eq!(std::env::var(PROJECT_SHADOWS).unwrap(), "project");
        assert_eq!(std::env::var(PROCESS_WINS).unwrap(), "process");

        unsafe {
            std::env::remove_var(GLOBAL_ONLY);
            std::env::remove_var(PROJECT_SHADOWS);
            std::env::remove_var(PROCESS_WINS);
        }
    }

    fn decision_endpoint(api_key_env: &str) -> DecisionEndpoint {
        DecisionEndpoint {
            api_key_env: Some(api_key_env.into()),
            ..DecisionEndpoint::new(
                DECISION_ENDPOINT_NAME,
                DecisionProtocol::TypeSafe,
                DECISION_BASE_URL.parse().unwrap(),
            )
        }
    }

    fn decision_env_key(endpoint: &DecisionEndpoint) -> Option<String> {
        endpoint.api_key(None).unwrap().map(|key| key.value)
    }

    #[test]
    fn decision_credentials_follow_the_global_environment_layers() {
        const KEY: &str = "CAUDRA_TEST_DECISION_COLLIDING_KEY";
        const PROCESS_CREDENTIAL: &str = "process-test-credential";
        const GLOBAL_CREDENTIAL: &str = "global-test-credential";
        const PROJECT_CREDENTIAL: &str = "project-test-credential";

        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        let project = dir.path().join(PROJECT_DIR);
        fs::create_dir_all(&project).unwrap();
        fs::write(global.join(ENV_FILE), format!("{KEY}={GLOBAL_CREDENTIAL}")).unwrap();
        fs::write(
            project.join(ENV_FILE),
            format!("{KEY}={PROJECT_CREDENTIAL}"),
        )
        .unwrap();
        let endpoint = decision_endpoint(KEY);
        unsafe { env::set_var(KEY, PROCESS_CREDENTIAL) };
        load_env_files_scoped(dir.path(), Some(&global), true);
        assert_eq!(
            decision_env_key(&endpoint).as_deref(),
            Some(PROCESS_CREDENTIAL)
        );
        unsafe { env::remove_var(KEY) };
        load_env_files_scoped(dir.path(), Some(&global), true);
        assert_eq!(env::var(KEY).unwrap(), PROJECT_CREDENTIAL);
        assert_eq!(
            decision_env_key(&endpoint).as_deref(),
            Some(GLOBAL_CREDENTIAL)
        );
        unsafe { env::remove_var(KEY) };
        load_env_files_scoped(dir.path(), None, true);
        assert_eq!(env::var(KEY).unwrap(), PROJECT_CREDENTIAL);
        assert_eq!(decision_env_key(&endpoint), None);
        unsafe { env::remove_var(KEY) };
    }

    const CURRENT_STARTUP: &str = "http://startup.test";
    const CURRENT_EDITED: &str = "http://edited.test";
    const CURRENT_PROJECT: &str = "http://project.test";
    const CURRENT_PROCESS: &str = "http://process.test";

    fn env_layers(dir: &TempDir) -> (PathBuf, PathBuf) {
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        let project = dir.path().join(PROJECT_DIR);
        fs::create_dir_all(&project).unwrap();
        (global.join(ENV_FILE), project.join(ENV_FILE))
    }

    #[test]
    fn an_edit_to_the_global_env_shows_on_the_next_read() {
        const KEY: &str = "CAUDRA_TEST_CURRENT_GLOBAL_EDITED";
        let dir = TempDir::new().unwrap();
        let (global_env, _) = env_layers(&dir);
        let global = global_env.parent().unwrap();
        fs::write(&global_env, format!("{KEY}={CURRENT_STARTUP}")).unwrap();
        load_env_files_scoped(dir.path(), Some(global), true);

        fs::write(&global_env, format!("{KEY}={CURRENT_EDITED}")).unwrap();
        let edited = current_env_value_in(KEY, Some(global)).unwrap();
        fs::write(&global_env, "").unwrap();
        let removed = current_env_value_in(KEY, Some(global)).unwrap();
        let process = env::var(KEY).unwrap();
        unsafe { env::remove_var(KEY) };

        assert_eq!(edited.as_deref(), Some(CURRENT_EDITED));
        assert_eq!(removed, None);
        assert_eq!(process, CURRENT_STARTUP);
    }

    #[test]
    fn a_project_env_never_supplies_a_current_value() {
        const KEY: &str = "CAUDRA_TEST_CURRENT_PROJECT_ONLY";
        let dir = TempDir::new().unwrap();
        let (global_env, project_env) = env_layers(&dir);
        let global = global_env.parent().unwrap();
        fs::write(&project_env, format!("{KEY}={CURRENT_PROJECT}")).unwrap();
        load_env_files_scoped(dir.path(), Some(global), true);

        let project_only = current_env_value_in(KEY, Some(global)).unwrap();
        fs::write(&global_env, format!("{KEY}={CURRENT_EDITED}")).unwrap();
        let global_added = current_env_value_in(KEY, Some(global)).unwrap();
        unsafe { env::remove_var(KEY) };

        assert_eq!(project_only, None);
        assert_eq!(global_added.as_deref(), Some(CURRENT_EDITED));
    }

    #[test]
    fn an_inherited_variable_wins_over_the_global_env() {
        const KEY: &str = "CAUDRA_TEST_CURRENT_INHERITED";
        let dir = TempDir::new().unwrap();
        let (global_env, _) = env_layers(&dir);
        let global = global_env.parent().unwrap();
        fs::write(&global_env, format!("{KEY}={CURRENT_EDITED}")).unwrap();
        unsafe { env::set_var(KEY, CURRENT_PROCESS) };
        load_env_files_scoped(dir.path(), Some(global), true);

        let current = current_env_value_in(KEY, Some(global)).unwrap();
        unsafe { env::remove_var(KEY) };

        assert_eq!(current.as_deref(), Some(CURRENT_PROCESS));
    }

    #[test]
    fn decision_credentials_cannot_come_from_project_env_even_with_custom_key_name() {
        const KEY: &str = "TEST_CAUDRA_DECISION_PROJECT_CREDENTIAL";
        const VALUE: &str = "project-controlled-test-value";
        let dir = TempDir::new().unwrap();
        let project = dir.path().join(PROJECT_DIR);
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join(".env"), format!("{KEY}={VALUE}")).unwrap();
        load_env_files_scoped(dir.path(), None, true);
        assert_eq!(std::env::var(KEY).unwrap(), VALUE);
        assert_eq!(decision_env_key(&decision_endpoint(KEY)), None);
        unsafe { std::env::remove_var(KEY) };
    }

    #[test]
    fn project_env_cannot_set_caudra_proxies() {
        const PROXY_VARIABLES: [&str; 8] = [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
        ];
        const HTTPS_PROXY: &str = "HTTPS_PROXY";
        const GLOBAL_PROXY: &str = "http://global-proxy.test:3128";
        const PROJECT_PROXY: &str = "http://project-proxy.test:3128";

        for name in PROXY_VARIABLES {
            unsafe { env::remove_var(name) };
        }
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        let project = dir.path().join(PROJECT_DIR);
        fs::create_dir_all(&project).unwrap();
        fs::write(
            global.join(ENV_FILE),
            format!("{HTTPS_PROXY}={GLOBAL_PROXY}"),
        )
        .unwrap();
        fs::write(
            project.join(ENV_FILE),
            format!(
                "{HTTPS_PROXY}={PROJECT_PROXY}\nHTTP_PROXY={PROJECT_PROXY}\nNO_PROXY=api.openai.com"
            ),
        )
        .unwrap();
        load_env_files_scoped(dir.path(), Some(&global), true);
        assert_eq!(env::var(HTTPS_PROXY).unwrap(), PROJECT_PROXY);
        assert_eq!(
            proxy_env(),
            ProxyEnv::from_lookup(|name| (name == HTTPS_PROXY).then(|| GLOBAL_PROXY.to_owned()))
        );
        for name in PROXY_VARIABLES {
            unsafe { env::remove_var(name) };
        }
    }

    #[test]
    fn decision_credentials_accept_global_env_and_reject_header_injection() {
        const KEY: &str = "TEST_CAUDRA_DECISION_GLOBAL_CREDENTIAL";
        const VALUE: &str = "global-test-value";
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(".env"), format!("{KEY}={VALUE}")).unwrap();
        load_env_files_scoped(dir.path(), Some(dir.path()), false);
        let endpoint = decision_endpoint(KEY);
        assert_eq!(decision_env_key(&endpoint).as_deref(), Some(VALUE));
        unsafe { std::env::set_var(KEY, "invalid\r\nheader") };
        assert!(endpoint.api_key(None).is_err());
        unsafe { std::env::remove_var(KEY) };
    }

    #[test]
    fn global_env_loader_does_not_read_project_file() {
        const GLOBAL: &str = "TEST_CAUDRA_REMOTE_GLOBAL";
        const PROJECT_CANARY: &str = "TEST_CAUDRA_REMOTE_PROJECT_CANARY";

        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        fs::write(global.join(".env"), format!("{GLOBAL}=loaded")).unwrap();
        let caudra_dir = dir.path().join(".caudra");
        fs::create_dir_all(&caudra_dir).unwrap();
        fs::write(
            caudra_dir.join(".env"),
            format!("{PROJECT_CANARY}=executed"),
        )
        .unwrap();
        unsafe {
            std::env::remove_var(GLOBAL);
            std::env::remove_var(PROJECT_CANARY);
        }

        load_env_files_scoped(dir.path(), Some(&global), false);

        assert_eq!(std::env::var(GLOBAL).unwrap(), "loaded");
        assert!(std::env::var_os(PROJECT_CANARY).is_none());
        unsafe {
            std::env::remove_var(GLOBAL);
            std::env::remove_var(PROJECT_CANARY);
        }
    }

    #[test_case("HERDR_ENV", false ; "herdr_env_is_process_only")]
    #[test_case("HERDR_PANE_ID", false ; "herdr_pane_id_is_process_only")]
    #[test_case("HERDR_BIN_PATH", false ; "herdr_bin_path_is_process_only")]
    #[test_case("HERDR_SOCKET_PATH", false ; "herdr_socket_path_is_process_only")]
    #[test_case("HERDR_WORKSPACE_ID", false ; "herdr_workspace_id_is_process_only")]
    #[test_case("HERDR_TAB_ID", false ; "herdr_tab_id_is_process_only")]
    #[test_case("ORCA_PANE_KEY", false ; "orca_pane_key_is_process_only")]
    #[test_case("ORCA_WORKTREE_ID", false ; "orca_worktree_id_is_process_only")]
    #[test_case("ORCA_CLI_COMMAND", false ; "orca_cli_command_is_process_only")]
    #[test_case("WORKCELL_MCP_CODE_WORKER", false ; "workcell_worker_is_process_only")]
    #[test_case("TEST_CAUDRA_ENV_FILE_VAR", true ; "ordinary_env_file_var_is_allowed")]
    fn env_file_var_loading_policy(key: &str, expected: bool) {
        assert_eq!(env_file_var_is_allowed(key), expected);
    }

    #[test]
    fn merge_plugins_overlay_wins_per_key() {
        let mut base: RawConfig = toml::from_str(
            "[plugins.index]\nenabled = true\n\
             [plugins.websearch]\nenabled = true\n\
             [plugins.grep]\nenabled = true\nsearch_result_limit = 200\nmax_line_bytes = 900\n",
        )
        .unwrap();
        let overlay: RawConfig = toml::from_str(
            "[plugins.websearch]\nenabled = false\n\
             [plugins.alpha_tool]\nenabled = true\n\
             [plugins.grep]\nsearch_result_limit = 50\n",
        )
        .unwrap();

        base.merge(overlay);
        assert_eq!(
            base.plugins["index"].enabled,
            Some(true),
            "base-only key preserved"
        );
        assert_eq!(
            base.plugins["websearch"].enabled,
            Some(false),
            "overlay replaces"
        );
        assert_eq!(
            base.plugins["alpha_tool"].enabled,
            Some(true),
            "overlay-only key added"
        );
        let grep = &base.plugins["grep"];
        assert_eq!(
            grep.enabled,
            Some(true),
            "enabled preserved when overlay omits it"
        );
        assert_eq!(
            grep.opts["search_result_limit"],
            serde_json::json!(50),
            "overlay opt wins"
        );
        assert_eq!(
            grep.opts["max_line_bytes"],
            serde_json::json!(900),
            "base opt preserved"
        );
    }

    #[test]
    fn show_thinking_deserializes_true() {
        let raw: RawConfig = toml::from_str("[ui]\nshow_thinking = true\n").unwrap();
        assert!(raw.ui.show_thinking.unwrap());
    }

    #[test]
    fn show_thinking_deserializes_false() {
        let raw: RawConfig = toml::from_str("[ui]\nshow_thinking = false\n").unwrap();
        assert!(!raw.ui.show_thinking.unwrap());
    }

    #[test]
    fn retention_defaults_to_reclaiming_only() {
        let config = RawConfig::default().into_config(false).unwrap();
        let retention = config.storage.retention;
        assert_eq!(retention.group_by, GroupBy::Directory);
        assert_eq!(
            retention.sweep_interval_hours,
            DEFAULT_RETENTION_SWEEP_INTERVAL_HOURS
        );
        assert!(retention.trim.is_empty(), "{NO_DEFAULT_DELETION}");
        assert!(retention.forget.is_empty(), "{NO_DEFAULT_DELETION}");
    }

    #[test]
    fn ephemeral_storage_defaults_off_and_parses_true() {
        let defaults = RawConfig::default().into_config(false).unwrap();
        let configured: RawConfig = toml::from_str("[storage]\nephemeral = true\n").unwrap();

        assert!(!defaults.storage.ephemeral);
        assert!(configured.into_config(false).unwrap().storage.ephemeral);
    }

    #[test_case(None, true; "default_enabled")]
    #[test_case(Some(true), true; "explicit_enabled")]
    #[test_case(Some(false), false; "explicit_disabled")]
    fn snapshots_enabled_survives_limit_overlay(enabled: Option<bool>, expected: bool) {
        let source = enabled.map_or_else(String::new, |enabled| {
            format!("[storage.snapshots]\nenabled = {enabled}\n")
        });
        let mut base: RawConfig = toml::from_str(&source).unwrap();
        base.merge(toml::from_str("[storage.snapshots]\nmax_files = 1\n").unwrap());
        let snapshots = base.into_config(false).unwrap().storage.snapshots;
        assert_eq!(snapshots.enabled, expected);
        assert_eq!(snapshots.max_files, 1);
    }

    /// The change store refuses a zero limit, so a zero would leave every
    /// call unrecorded without saying why.
    #[test_case("max_bytes_mb"; "total_bytes")]
    #[test_case("max_files"; "files")]
    #[test_case("max_file_bytes_mb"; "file_bytes")]
    fn a_zero_snapshot_limit_is_refused(field: &str) {
        let raw: RawConfig =
            toml::from_str(&format!("[storage.snapshots]\n{field} = 0\n")).unwrap();
        let error = raw.into_config(false).unwrap().validate().unwrap_err();
        assert!(
            matches!(
                error,
                ConfigError::BelowMinimum { section: SNAPSHOTS_SECTION, field: refused, .. }
                    if refused == field
            ),
            "{error}"
        );
    }

    #[test]
    fn retention_policies_parse_and_project_replaces_whole_policy() {
        let mut base: RawConfig = toml::from_str(
            "[storage.retention]\ngroup_by = \"none\"\nsweep_interval_hours = 0\n\
             [storage.retention.trim]\nkeep_last = 5\nkeep_within = \"2y5m7d3h\"\n\
             keep_within_daily = \"\"\n\
             [storage.retention.forget]\nkeep_weekly = 4\n",
        )
        .unwrap();
        let overlay: RawConfig =
            toml::from_str("[storage.retention.trim]\nkeep_daily = 7\n").unwrap();

        base.merge(overlay);
        let retention = base.into_config(false).unwrap().storage.retention;

        assert_eq!(retention.group_by, GroupBy::None);
        assert_eq!(retention.sweep_interval_hours, 0);
        assert_eq!(
            retention.trim,
            KeepPolicy {
                keep_daily: Some(7),
                ..KeepPolicy::default()
            }
        );
        assert_eq!(
            retention.forget,
            KeepPolicy {
                keep_weekly: Some(4),
                ..KeepPolicy::default()
            }
        );
    }

    #[test]
    fn retention_rejects_unknown_rules_and_bad_durations() {
        assert!(
            toml::from_str::<RawConfig>("[storage.retention.trim]\nkeep_fortnightly = 1\n")
                .is_err()
        );
        assert!(
            toml::from_str::<RawConfig>("[storage.retention.trim]\nkeep_within = \"7w\"\n")
                .is_err()
        );
    }

    #[test]
    fn show_thinking_missing_defaults_true() {
        let raw: RawConfig = toml::from_str("").unwrap();
        let config = raw.into_config(false).unwrap();
        assert!(
            config.ui.show_thinking,
            "reasoning is part of the answer and shows unless it is turned off"
        );
    }

    #[test_case("" => true; "missing_defaults_on")]
    #[test_case("[ui]\nchangelog_on_update = false\n" => false; "disabled")]
    #[test_case("[ui]\nchangelog_on_update = true\n" => true; "enabled")]
    fn changelog_on_update_deserializes(toml: &str) -> bool {
        let raw: RawConfig = toml::from_str(toml).unwrap();
        raw.into_config(false).unwrap().ui.changelog_on_update
    }

    #[test_case(true; "enabled")]
    #[test_case(false; "disabled")]
    fn changelog_on_update_overlay_wins(enabled: bool) {
        let mut base: RawConfig =
            toml::from_str(&format!("[ui]\nchangelog_on_update = {}\n", !enabled)).unwrap();
        base.merge(toml::from_str(&format!("[ui]\nchangelog_on_update = {enabled}\n")).unwrap());
        assert_eq!(
            base.into_config(false).unwrap().ui.changelog_on_update,
            enabled
        );
    }

    #[test_case(true; "enabled")]
    #[test_case(false; "disabled")]
    fn update_check_deserializes(enabled: bool) {
        let raw: RawConfig = toml::from_str(&format!("[ui]\nupdate_check = {enabled}\n")).unwrap();
        assert_eq!(raw.ui.update_check, Some(enabled));
        assert_eq!(raw.into_config(false).unwrap().ui.update_check, enabled);
    }

    #[test]
    fn update_check_missing_defaults_on() {
        let raw: RawConfig = toml::from_str("").unwrap();
        let config = raw.into_config(false).unwrap();
        assert!(config.ui.update_check);
        assert!(UiConfig::default().update_check);
        assert_eq!(config.ui.update_channel, UpdateChannel::Auto);
        assert_eq!(UiConfig::default().update_channel, UpdateChannel::Auto);
    }

    #[test_case(true; "enabled")]
    #[test_case(false; "disabled")]
    fn update_check_overlay_wins(enabled: bool) {
        let mut base: RawConfig =
            toml::from_str(&format!("[ui]\nupdate_check = {}\n", !enabled)).unwrap();
        base.merge(toml::from_str(&format!("[ui]\nupdate_check = {enabled}\n")).unwrap());
        assert_eq!(base.into_config(false).unwrap().ui.update_check, enabled);
    }

    #[test_case("auto", UpdateChannel::Auto; "auto")]
    #[test_case("stable", UpdateChannel::Stable; "stable")]
    #[test_case("preview", UpdateChannel::Preview; "preview")]
    fn update_channel_deserializes(channel: &str, expected: UpdateChannel) {
        let raw: RawConfig =
            toml::from_str(&format!("[ui]\nupdate_channel = '{channel}'\n")).unwrap();
        assert_eq!(raw.ui.update_channel.as_ref(), Some(&expected));
        assert_eq!(raw.into_config(false).unwrap().ui.update_channel, expected);
    }

    #[test_case("'nightly'"; "unknown")]
    #[test_case("true"; "boolean")]
    fn update_channel_rejects_invalid(channel: &str) {
        assert!(
            toml::from_str::<RawConfig>(&format!("[ui]\nupdate_channel = {channel}\n")).is_err()
        );
    }

    #[test_case("", UpdateChannel::Preview; "missing_preserves_base")]
    #[test_case("update_channel = 'auto'", UpdateChannel::Auto; "explicit_auto")]
    #[test_case("update_channel = 'stable'", UpdateChannel::Stable; "stable_override")]
    fn update_channel_overlay_wins(field: &str, expected: UpdateChannel) {
        let mut base: RawConfig =
            toml::from_str("[ui]\nupdate_check = false\nupdate_channel = 'preview'\n").unwrap();
        base.merge(toml::from_str(&format!("[ui]\n{field}\n")).unwrap());
        let config = base.into_config(false).unwrap();
        assert_eq!(config.ui.update_channel, expected);
        assert!(!config.ui.update_check);
    }

    #[test_case("" => InlineImages::Auto; "missing_defaults_to_auto")]
    #[test_case("[ui]\ninline_images = 'kitty'\n" => InlineImages::Kitty; "kitty")]
    #[test_case("[ui]\ninline_images = 'blocks'\n" => InlineImages::Blocks; "blocks")]
    #[test_case("[ui]\ninline_images = 'sextants'\n" => InlineImages::Sextants; "sextants")]
    #[test_case("[ui]\ninline_images = 'off'\n" => InlineImages::Off; "off")]
    #[test_case("[ui]\ninline_images = false\n" => InlineImages::Off; "false_is_off")]
    #[test_case("[ui]\ninline_images = true\n" => InlineImages::Auto; "true_is_auto")]
    fn inline_images_deserializes(toml: &str) -> InlineImages {
        let raw: RawConfig = toml::from_str(toml).unwrap();
        raw.into_config(false).unwrap().ui.inline_images
    }

    #[test_case("" => StoredMode::Plan; "missing_defaults_to_plan")]
    #[test_case("[ui]\ndefault_mode = 'plan'\n" => StoredMode::Plan; "plan")]
    #[test_case("[ui]\ndefault_mode = 'build'\n" => StoredMode::Build; "build")]
    fn default_mode_deserializes(toml: &str) -> StoredMode {
        let raw: RawConfig = toml::from_str(toml).unwrap();
        raw.into_config(false).unwrap().ui.default_mode
    }

    #[test_case("'auto'"; "unknown")]
    #[test_case("'Build'"; "capitalized")]
    #[test_case("true"; "boolean")]
    fn default_mode_rejects_invalid(mode: &str) {
        assert!(toml::from_str::<RawConfig>(&format!("[ui]\ndefault_mode = {mode}\n")).is_err());
    }

    #[test_case("" => StoredMode::Build; "missing_preserves_base")]
    #[test_case("default_mode = 'plan'" => StoredMode::Plan; "project_overrides")]
    fn default_mode_overlay_wins(field: &str) -> StoredMode {
        let mut base: RawConfig = toml::from_str("[ui]\ndefault_mode = 'build'\n").unwrap();
        base.merge(toml::from_str(&format!("[ui]\n{field}\n")).unwrap());
        base.into_config(false).unwrap().ui.default_mode
    }

    #[test]
    fn max_input_lines_defaults_and_deserializes() {
        let raw: RawConfig = toml::from_str("").unwrap();
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.ui.max_input_lines, DEFAULT_MAX_INPUT_LINES);

        let raw: RawConfig = toml::from_str("[ui]\nmax_input_lines = 5\n").unwrap();
        assert_eq!(raw.ui.max_input_lines.unwrap(), 5);
    }

    const CARD_DEFAULTS_MSG: &str = "a reader who never wrote a [ui] table still gets the folded \
        reads and the fixed window the docs promise, so the defaults are part of the contract";
    const COLLAPSE_LIST_IS_THE_READERS_MSG: &str = "the list replaces the default outright rather \
        than adding to it, and an empty list is the documented way to opt out, so it must survive \
        as empty instead of falling back";
    const SCROLL_CARD_CONFIGURED_MSG: &str = "the window height is the reader's, and 0 is the \
        documented way to turn scrolling off rather than an unset value";
    const CARD_OVERLAY_MSG: &str = "a project layer sits nearer the reader than the global one, \
        so its card settings win field by field";
    const COLLAPSE_OVERRIDE_TOOL: &str = "shell";
    const SCROLL_CARD_LINES_OFF: u32 = 0;
    const CONFIGURED_SCROLL_CARD_LINES: u32 = 40;

    /// Most readers never write a `[ui]` table, so these defaults are what the
    /// transcript actually looks like. Losing any of them silently changes
    /// every session: an empty collapse list reopens every read, and a zero
    /// window turns the fixed shell cards and reasoning blocks back into
    /// unbounded bodies.
    #[test]
    fn card_display_defaults_survive_a_config_that_names_neither() {
        let config: RawConfig = toml::from_str("").unwrap();
        let config = config.into_config(false).unwrap();

        assert_eq!(
            config.ui.always_collapsed, DEFAULT_ALWAYS_COLLAPSED,
            "{CARD_DEFAULTS_MSG}"
        );
        assert_eq!(
            config.ui.scroll_card_lines, DEFAULT_SCROLL_CARD_LINES,
            "{CARD_DEFAULTS_MSG}"
        );
        assert_eq!(
            config.ui.thinking_lines, DEFAULT_THINKING_LINES,
            "{CARD_DEFAULTS_MSG}"
        );
    }

    /// `unwrap_or_else` on an `Option<Vec<_>>` cannot tell "unset" from "set to
    /// nothing" unless the deserializer keeps the two apart, and the docs sell
    /// `[]` as the way out of the feature. A collapse list that fell back to the
    /// default here would leave a reader unable to turn folding off at all.
    #[test_case("[]", &[] ; "empty_list_opts_out")]
    #[test_case("[\"shell\"]", &[COLLAPSE_OVERRIDE_TOOL] ; "named_tool_replaces_the_default")]
    fn always_collapsed_takes_the_configured_list(list: &str, expected: &[&str]) {
        let raw: RawConfig = toml::from_str(&format!("[ui]\nalways_collapsed = {list}\n")).unwrap();
        let config = raw.into_config(false).unwrap();

        assert_eq!(
            config.ui.always_collapsed, expected,
            "{COLLAPSE_LIST_IS_THE_READERS_MSG}"
        );
    }

    /// `0` is a documented setting rather than an absent one: it restores the
    /// old budget-plus-notice card. Treating it as unset would silently hand
    /// back the fixed window the reader just asked to be rid of.
    #[test_case(SCROLL_CARD_LINES_OFF ; "scrolling_off")]
    #[test_case(CONFIGURED_SCROLL_CARD_LINES ; "taller_window")]
    fn scroll_card_lines_takes_the_configured_height(lines: u32) {
        let raw: RawConfig =
            toml::from_str(&format!("[ui]\nscroll_card_lines = {lines}\n")).unwrap();
        let config = raw.into_config(false).unwrap();

        assert_eq!(
            config.ui.scroll_card_lines, lines,
            "{SCROLL_CARD_CONFIGURED_MSG}"
        );
    }

    /// The reasoning window follows the same rule: `0` asks for every block
    /// whole, and reading it as unset would hand the window straight back.
    #[test_case(SCROLL_CARD_LINES_OFF ; "scrolling_off")]
    #[test_case(CONFIGURED_SCROLL_CARD_LINES ; "taller_window")]
    fn thinking_lines_takes_the_configured_height(lines: u32) {
        let raw: RawConfig = toml::from_str(&format!("[ui]\nthinking_lines = {lines}\n")).unwrap();
        let config = raw.into_config(false).unwrap();

        assert_eq!(
            config.ui.thinking_lines, lines,
            "{SCROLL_CARD_CONFIGURED_MSG}"
        );
    }

    /// Every field rides the same `merge_option!` list, and a field left off it
    /// is not a compile error — it just silently pins the global value, so a
    /// project could never soften or tighten any of these settings.
    #[test]
    fn card_display_overlay_wins_over_the_layer_below() {
        let mut base: RawConfig = toml::from_str(&format!(
            "[ui]\nscroll_card_lines = {CONFIGURED_SCROLL_CARD_LINES}\nthinking_lines = {CONFIGURED_SCROLL_CARD_LINES}\nalways_collapsed = []\n"
        ))
        .unwrap();
        base.merge(
            toml::from_str(&format!(
                "[ui]\nscroll_card_lines = {SCROLL_CARD_LINES_OFF}\nthinking_lines = {SCROLL_CARD_LINES_OFF}\nalways_collapsed = [\"{COLLAPSE_OVERRIDE_TOOL}\"]\n"
            ))
            .unwrap(),
        );

        assert_eq!(
            base.ui.scroll_card_lines,
            Some(SCROLL_CARD_LINES_OFF),
            "{CARD_OVERLAY_MSG}"
        );
        assert_eq!(
            base.ui.thinking_lines,
            Some(SCROLL_CARD_LINES_OFF),
            "{CARD_OVERLAY_MSG}"
        );
        assert_eq!(
            base.ui.always_collapsed.as_deref(),
            Some([COLLAPSE_OVERRIDE_TOOL.to_owned()].as_slice()),
            "{CARD_OVERLAY_MSG}"
        );
    }

    #[test_case("[ui]\nsplash_animaton = true\n" ; "top_level_typo")]
    #[test_case("agent = { bash_timeout_secs = 60 }\n" ; "moved_bash_timeout")]
    #[test_case("agent = { search_result_limit = 50 }\n" ; "moved_search_limit")]
    #[test_case("[index]\nmax_file_size_mb = 4\n" ; "removed_index_section")]
    #[test_case("[tools.bash]\nenabled = true\n" ; "removed_tools_table")]
    fn deny_unknown_fields_rejects(toml_str: &str) {
        let result: Result<RawConfig, _> = toml::from_str(toml_str);
        assert!(
            result.is_err(),
            "unknown field should be rejected: {toml_str}"
        );
    }

    #[test]
    fn deny_unknown_fields_accepts_valid_plugins() {
        const VALID: &str =
            "[plugins.bash]\nenabled = true\n[plugins.websearch]\nenabled = false\n";
        let result: Result<RawConfig, _> = toml::from_str(VALID);
        assert!(
            result.is_ok(),
            "valid plugins section should parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn plugin_extra_keys_parse_into_opts() {
        let raw: RawConfig =
            toml::from_str("[plugins.bash]\nenabled = true\ntimeout_secs = 180\n").unwrap();
        let bash = &raw.plugins["bash"];
        assert_eq!(bash.enabled, Some(true));
        assert_eq!(bash.opts["timeout_secs"], serde_json::json!(180));
    }

    #[test]
    fn index_config_defaults_and_wires_native_host_limit() {
        let config = RawConfig::default().into_config(false).unwrap();
        assert_eq!(
            config.agent.index_max_file_size_mb,
            DEFAULT_INDEX_MAX_FILE_SIZE_MB
        );
        assert!(config.plugins.names.contains(&"index".into()));

        let config: RawConfig =
            toml::from_str("[plugins.index]\nenabled = false\nmax_file_size_mb = 4\n").unwrap();
        let config = config.into_config(false).unwrap();
        assert_eq!(config.agent.index_max_file_size_mb, 4);
        assert!(config.agent.disabled_tools.contains(&"file_index".into()));
        assert!(!config.plugins.names.contains(&"index".into()));
    }

    #[test_case("max_file_size_mb = 0", "below minimum" ; "below_minimum")]
    #[test_case("max_file_size_mb = 17", "exceeds maximum" ; "above_maximum")]
    #[test_case("max_file_size_mb = 9223372036854775807", "exceeds maximum" ; "multiplication_overflow")]
    #[test_case("max_file_size_mb = \"2\"", "expected an integer" ; "wrong_type")]
    #[test_case("max_file_size = 2", "unknown option" ; "unknown_field")]
    fn index_config_is_strict(option: &str, expected: &str) {
        let raw: RawConfig = toml::from_str(&format!("[plugins.index]\n{option}\n")).unwrap();
        let error = raw.into_config(false).err().expect("invalid index option");
        assert!(error.to_string().contains(expected), "{error}");
    }

    #[test_case("", DEFAULT_SKILL_PLUGIN_DEV, DEFAULT_SKILL_WORKFLOW_DEV, DEFAULT_SKILL_AUTOMATION_DEV, DEFAULT_SKILL_DOCS, DEFAULT_SKILL_PSTACK ; "defaults")]
    #[test_case("plugin_dev = true", true, DEFAULT_SKILL_WORKFLOW_DEV, DEFAULT_SKILL_AUTOMATION_DEV, DEFAULT_SKILL_DOCS, DEFAULT_SKILL_PSTACK ; "plugin_dev_alone")]
    #[test_case("workflow_dev = false", DEFAULT_SKILL_PLUGIN_DEV, false, DEFAULT_SKILL_AUTOMATION_DEV, DEFAULT_SKILL_DOCS, DEFAULT_SKILL_PSTACK ; "workflow_dev_alone")]
    #[test_case("automation_dev = false", DEFAULT_SKILL_PLUGIN_DEV, DEFAULT_SKILL_WORKFLOW_DEV, false, DEFAULT_SKILL_DOCS, DEFAULT_SKILL_PSTACK ; "automation_dev_alone")]
    #[test_case("docs = false", DEFAULT_SKILL_PLUGIN_DEV, DEFAULT_SKILL_WORKFLOW_DEV, DEFAULT_SKILL_AUTOMATION_DEV, false, DEFAULT_SKILL_PSTACK ; "docs_alone")]
    #[test_case("pstack = true", DEFAULT_SKILL_PLUGIN_DEV, DEFAULT_SKILL_WORKFLOW_DEV, DEFAULT_SKILL_AUTOMATION_DEV, DEFAULT_SKILL_DOCS, true ; "pstack_alone")]
    #[test_case("plugin_dev = true\nworkflow_dev = false\nautomation_dev = false\ndocs = false\npstack = true", true, false, false, false, true ; "all")]
    fn skill_flags_are_read_independently(
        options: &str,
        plugin_dev: bool,
        workflow_dev: bool,
        automation_dev: bool,
        docs: bool,
        pstack: bool,
    ) {
        let raw: RawConfig = toml::from_str(&format!("[plugins.skill]\n{options}\n")).unwrap();
        let config = raw.into_config(false).unwrap();
        assert_eq!(
            config.agent.builtin_skills,
            BuiltinSkills {
                plugin_dev,
                workflow_dev,
                automation_dev,
                docs,
                pstack,
            }
        );
    }

    #[test]
    fn pstack_is_off_by_default() {
        let config = RawConfig::default().into_config(false).unwrap();
        assert!(!config.agent.builtin_skills.pstack);
    }

    #[test]
    fn automation_dev_skill_is_offered_by_default() {
        let config = RawConfig::default().into_config(false).unwrap();
        assert!(config.agent.builtin_skills.automation_dev);
    }

    #[test_case("workflow_dev = \"no\"", "expected a boolean" ; "wrong_type")]
    #[test_case("workflows = false", "unknown option" ; "unknown_field")]
    fn skill_config_is_strict(option: &str, expected: &str) {
        let raw: RawConfig = toml::from_str(&format!("[plugins.skill]\n{option}\n")).unwrap();
        let error = raw.into_config(false).err().expect("invalid skill option");
        assert!(error.to_string().contains(expected), "{error}");
    }

    #[test]
    fn index_config_accepts_checked_maximum() {
        let raw: RawConfig = toml::from_str(&format!(
            "[plugins.index]\nmax_file_size_mb = {MAX_INDEX_MAX_FILE_SIZE_MB}\n"
        ))
        .unwrap();

        assert_eq!(
            raw.into_config(false).unwrap().agent.index_max_file_size_mb,
            MAX_INDEX_MAX_FILE_SIZE_MB
        );
    }

    #[test]
    fn into_config_wires_plugin_names_and_opts() {
        let raw: RawConfig = toml::from_str(
            "[plugins.bash]\ntimeout_secs = 180\n[plugins.websearch]\nenabled = false\n",
        )
        .unwrap();
        let config = raw.into_config(false).unwrap();
        assert!(config.plugins.names.contains(&"bash".to_string()));
        assert!(!config.plugins.names.contains(&"websearch".to_string()));
        assert!(
            config.plugins.names.contains(&"index".to_string()),
            "untouched builtin stays"
        );
        assert_eq!(
            config.plugins.opts["bash"]["timeout_secs"],
            serde_json::json!(180)
        );
        assert!(
            !config.plugins.opts.contains_key("websearch"),
            "enabled-only tables produce no opts"
        );
    }

    #[test]
    fn from_plugins_default() {
        let plugins = PluginsConfig::from_plugins(HashMap::new());
        let expected: Vec<String> = DEFAULT_BUILTINS.iter().map(|s| s.to_string()).collect();
        assert_eq!(plugins.names, expected);
        assert!(plugins.enabled);
    }

    #[test]
    fn from_plugins_enable_disable_and_sort() {
        let mut entries = HashMap::new();
        entries.insert("websearch".to_string(), plugin_enabled(false));
        entries.insert("zeta".to_string(), plugin_enabled(true));
        entries.insert("alpha".to_string(), plugin_enabled(true));
        entries.insert("custom_tool".to_string(), PluginFileConfig::default());

        let plugins = PluginsConfig::from_plugins(entries);
        assert!(
            !plugins.names.contains(&"websearch".to_string()),
            "disabled builtin removed"
        );
        assert!(
            plugins.names.contains(&"index".to_string()),
            "untouched builtin stays"
        );
        assert!(
            plugins.names.contains(&"bash".to_string()),
            "bash is a default builtin"
        );
        assert!(
            !plugins.names.contains(&"custom_tool".to_string()),
            "enabled=None non-default ignored"
        );

        let extras: Vec<_> = plugins
            .names
            .iter()
            .filter(|t| !DEFAULT_BUILTINS.contains(&t.as_str()))
            .cloned()
            .collect();
        assert_eq!(
            extras,
            vec!["alpha", "zeta"],
            "extras sorted alphabetically"
        );
    }

    #[test]
    fn merge_tool_output_lines_field_level_overlay() {
        let mut base = RawConfig {
            ui: UiFileConfig {
                tool_output_lines: Some(ToolOutputLinesFile {
                    bash: Some(50),
                    read: Some(30),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let overlay = RawConfig {
            ui: UiFileConfig {
                tool_output_lines: Some(ToolOutputLinesFile {
                    bash: Some(100),
                    grep: Some(15),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        base.merge(overlay);
        let tol = base.ui.tool_output_lines.as_ref().unwrap();
        assert_eq!(tol.bash, Some(100), "overlay wins");
        assert_eq!(tol.read, Some(30), "base preserved");
        assert_eq!(tol.grep, Some(15), "overlay added");
    }

    #[test]
    fn default_builtins_sorted() {
        for pair in DEFAULT_BUILTINS.windows(2) {
            assert!(
                pair[0] < pair[1],
                "DEFAULT_BUILTINS not sorted: {:?} >= {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test_case("enabled = false" ; "enabled_false")]
    #[test_case("search_result_limit = 50" ; "opts_only")]
    fn unknown_plugin_name_errors(body: &str) {
        let raw: RawConfig = toml::from_str(&format!("[plugins.gerp]\n{body}\n")).unwrap();
        let Err(err) = raw.into_config(false) else {
            panic!("plugins.gerp should be rejected");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("no bundled plugin is named \"gerp\"") && msg.contains("grep"),
            "error should name the typo and list bundled plugins, got: {msg}"
        );
    }

    #[test]
    fn disabled_plugin_keeps_opts_but_not_load_entry() {
        let raw: RawConfig =
            toml::from_str("[plugins.bash]\nenabled = false\ntimeout_secs = 180\n").unwrap();
        let config = raw.into_config(false).unwrap();
        assert!(!config.plugins.names.contains(&"bash".to_string()));
        assert_eq!(
            config.plugins.opts["bash"]["timeout_secs"],
            serde_json::json!(180),
            "opts survive for when the plugin is re-enabled"
        );
    }

    const PLUGIN_TOOL_DRIFT: &str = "a plugins.<name> key must map to registered tools or be \
                                     listed in TOOLLESS_PLUGINS";

    #[test_case("bash", &["shell"] ; "bash_disables_shell")]
    #[test_case("edit", &["file_edit", "file_apply_patch"] ; "edit_disables_both_edit_tools")]
    #[test_case("read", &["file_read"] ; "read_disables_file_read")]
    #[test_case("index", &["file_index"] ; "index_disables_the_renamed_file_index")]
    fn disabled_plugin_disables_the_tools_it_was_replaced_by(plugin: &str, expected: &[&str]) {
        let raw: RawConfig =
            toml::from_str(&format!("[plugins.{plugin}]\nenabled = false\n")).unwrap();
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.agent.disabled_tools, expected);
    }

    #[test_case("list" ; "list")]
    #[test_case("sessions" ; "sessions")]
    #[test_case("tool_output" ; "tool_output")]
    fn disabling_a_toolless_plugin_names_no_tool(plugin: &str) {
        let raw: RawConfig =
            toml::from_str(&format!("[plugins.{plugin}]\nenabled = false\n")).unwrap();
        assert!(
            raw.into_config(false)
                .unwrap()
                .agent
                .disabled_tools
                .is_empty()
        );
    }

    #[test_case("shell" ; "builtin")]
    #[test_case("list_sessions" ; "peer_discovery")]
    #[test_case("send_message" ; "peer_send")]
    #[test_case("publish_message" ; "peer_publish")]
    #[test_case("read_topic" ; "peer_history")]
    #[test_case("work_assignment" ; "peer_work")]
    #[test_case("github.create_issue" ; "mcp_tool")]
    #[test_case("github.*" ; "mcp_server")]
    fn agent_disabled_tools_accepts(tool: &str) {
        let raw: RawConfig =
            toml::from_str(&format!("[agent]\ndisabled_tools = [\"{tool}\"]\n")).unwrap();
        assert_eq!(raw.into_config(false).unwrap().agent.disabled_tools, [tool]);
    }

    #[test_case("file_wrte" ; "typo")]
    #[test_case("bash" ; "legacy_plugin_id_is_not_a_tool")]
    #[test_case("local_document_read" ; "removed_document_read")]
    #[test_case("local_document_write" ; "removed_document_write")]
    #[test_case("code_context" ; "renamed_source_code_context")]
    #[test_case("code_refs" ; "renamed_source_code_refs")]
    #[test_case("local_document_apply_patch" ; "removed_document_patch")]
    #[test_case("*" ; "bare_wildcard")]
    fn agent_disabled_tools_rejects_unknown_name(tool: &str) {
        let raw: RawConfig =
            toml::from_str(&format!("[agent]\ndisabled_tools = [\"{tool}\"]\n")).unwrap();
        let error = raw
            .into_config(false)
            .err()
            .expect("unknown tool")
            .to_string();
        assert!(error.contains("no tool is named"), "{error}");
    }

    #[test_case("[agent]\ndisabled_tools = [\"code\"]\n", None ; "global")]
    #[test_case("[agent]\ndisabled_tools = [\"shell\"]\n", Some("[agent]\ndisabled_tools = [\"code\"]\n") ; "project")]
    fn disabling_code_names_why_it_is_required(global: &str, project: Option<&str>) {
        let mut merged: RawConfig = toml::from_str(global).unwrap();
        if let Some(project) = project {
            merged.merge(toml::from_str(project).unwrap());
        }
        let error = merged
            .into_config(false)
            .err()
            .expect("code is required")
            .to_string();
        assert!(error.contains(CODE_REQUIRED), "{error}");
    }

    #[test]
    fn project_disabled_tools_extend_global_and_cannot_reenable() {
        let mut merged: RawConfig =
            toml::from_str("[agent]\ndisabled_tools = [\"shell\"]\n").unwrap();
        merged.merge(toml::from_str("[agent]\ndisabled_tools = [\"websearch\"]\n").unwrap());
        assert_eq!(
            merged.into_config(false).unwrap().agent.disabled_tools,
            ["shell", "websearch"]
        );
    }

    #[test]
    fn a_project_without_the_key_keeps_the_global_list() {
        let mut merged: RawConfig =
            toml::from_str("[agent]\ndisabled_tools = [\"shell\"]\n").unwrap();
        merged.merge(toml::from_str("[agent]\nstale_read_check = false\n").unwrap());
        assert_eq!(
            merged.into_config(false).unwrap().agent.disabled_tools,
            ["shell"]
        );
    }

    #[test]
    fn cli_and_config_entries_are_deduplicated() {
        let raw: RawConfig = toml::from_str(
            "[agent]\ndisabled_tools = [\"shell\"]\n\n[plugins.bash]\nenabled = false\n",
        )
        .unwrap();
        assert_eq!(
            raw.into_config(false).unwrap().agent.disabled_tools,
            ["shell"]
        );
    }

    #[test]
    fn every_default_builtin_maps_to_a_tool_or_is_listed_as_toolless() {
        for plugin in DEFAULT_BUILTINS {
            let tools = plugin_tools(plugin);
            if TOOLLESS_PLUGINS.contains(plugin) {
                assert!(tools.is_empty(), "{PLUGIN_TOOL_DRIFT}: {plugin}");
                continue;
            }
            assert!(!tools.is_empty(), "{PLUGIN_TOOL_DRIFT}: {plugin}");
            for tool in tools {
                assert!(
                    is_builtin_tool(tool),
                    "{PLUGIN_TOOL_DRIFT}: {plugin} -> {tool}"
                );
            }
        }
    }

    #[test_case("shell", "shell", true ; "exact")]
    #[test_case("shell", "file_read", false ; "different_name")]
    #[test_case("github.*", "github.create_issue", true ; "server_wildcard")]
    #[test_case("github.*", "gitlab.create_issue", false ; "other_server")]
    #[test_case("github.create_issue", "github.create_issue", true ; "qualified_exact")]
    fn tool_pattern_matching(pattern: &str, name: &str, expected: bool) {
        assert_eq!(tool_pattern_matches(pattern, name), expected);
    }

    #[test]
    fn internal_companions_cannot_be_disabled() {
        for name in INTERNAL_COMPANION_TOOL_NAMES {
            assert!(is_tool_enabled(&[(*name).to_owned()], name));
        }
    }

    #[test]
    fn edit_sub_tool_toggles_flow_as_edit_opts() {
        let raw: RawConfig =
            toml::from_str("[plugins.edit]\nmultiedit = false\nedit_lines = true\n").unwrap();
        let config = raw.into_config(false).unwrap();
        assert_eq!(
            config.plugins.opts["edit"]["multiedit"],
            serde_json::json!(false)
        );
        assert_eq!(
            config.plugins.opts["edit"]["edit_lines"],
            serde_json::json!(true)
        );
        assert!(config.agent.disabled_tools.is_empty());
    }

    #[test]
    fn permissions_mcp_allows_from_both_sources_are_review_only() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[mcp.deepwiki]\nallow = [\"search\", \"fetch\"]\n",
        );
        let caudra_dir = dir.path().join(".caudra");
        fs::create_dir_all(&caudra_dir).unwrap();
        fs::write(
            caudra_dir.join("permissions.toml"),
            "[mcp.github]\nallow = [\"search\", \"*\"]\n",
        )
        .unwrap();

        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert!(perms.rules.is_empty());
        assert!(perms.project_allow_rules.is_empty());
        assert_eq!(perms.review_candidates.len(), 4);
        assert_eq!(
            perms
                .review_candidates
                .iter()
                .filter(|candidate| candidate.source == PermissionSource::Global)
                .count(),
            2
        );
        assert_eq!(
            perms
                .review_candidates
                .iter()
                .filter(|candidate| candidate.source == PermissionSource::Project)
                .count(),
            2
        );
        assert!(
            perms
                .review_candidates
                .iter()
                .all(|candidate| candidate.kind == PermissionReviewKind::Rule)
        );
    }

    #[test]
    fn permissions_mcp_server_wide_global_allow_is_review_only() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[mcp.deepwiki]\nallow = true\n");
        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert!(perms.rules.is_empty());
        assert!(perms.project_allow_rules.is_empty());
        assert_eq!(
            perms.review_candidates,
            vec![PermissionReviewCandidate {
                source: PermissionSource::Global,
                kind: PermissionReviewKind::Rule,
                tool: Some(ToolKey::McpServer {
                    server: "deepwiki".into(),
                }),
                scope: None,
            }]
        );
    }

    #[test]
    fn permissions_mcp_deny_true_is_active() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[mcp.server]\ndeny = true\n");
        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(
            perms.rules[0].tool,
            ToolKey::McpServer {
                server: "server".into(),
            }
        );
        assert_eq!(perms.rules[0].effect, Effect::Deny);
    }

    #[test]
    fn explicit_default_preserved_with_deprecated_deny_true() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[mcp.server]\ndefault = \"allow\"\ndeny = true\n",
        );
        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert!(!perms.tool_defaults.contains_key(&ToolKey::McpServer {
            server: "server".into()
        }));
        assert!(
            perms
                .review_candidates
                .contains(&PermissionReviewCandidate {
                    source: PermissionSource::Global,
                    kind: PermissionReviewKind::Default,
                    tool: Some(ToolKey::McpServer {
                        server: "server".into()
                    }),
                    scope: None,
                })
        );
        assert_eq!(perms.rules[0].effect, Effect::Deny);
    }

    #[test]
    fn permissions_mcp_deny_rules() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[mcp.github]\ndeny = [\"admin_delete\"]\n");
        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(perms.rules.len(), 1);
        assert_eq!(
            perms.rules[0].tool,
            ToolKey::McpTool {
                server: "github".into(),
                tool: "admin_delete".into()
            }
        );
        assert_eq!(perms.rules[0].effect, Effect::Deny);
    }

    #[test]
    fn permissions_mcp_dotted_tool_name_rejected() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[mcp.myserver]\nallow = [\"web.search\"]\n");
        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(perms.default, DefaultEffect::Deny);
        assert_eq!(perms.rules.len(), 1);
        assert!(matches!(perms.rules[0].tool, ToolKey::Wildcard));
        assert_eq!(perms.rules[0].effect, Effect::Deny);
    }

    #[test]
    fn malformed_mcp_policy_fails_closed() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[mcp.github]\ndeny = 1\n");

        let permissions = load_permissions_inner(dir.path(), Some(global.as_path()));

        assert_eq!(permissions.default, DefaultEffect::Deny);
        assert_eq!(permissions.rules.len(), 1);
        assert!(matches!(permissions.rules[0].tool, ToolKey::Wildcard));
        assert_eq!(permissions.rules[0].effect, Effect::Deny);
    }

    #[test]
    fn permissions_mcp_default_allow() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "default = \"deny\"\n\n[mcp.exa]\ndefault = \"allow\"\n",
        );
        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert!(!perms.tool_defaults.contains_key(&ToolKey::McpServer {
            server: "exa".into()
        }));
        assert!(
            perms
                .review_candidates
                .contains(&PermissionReviewCandidate {
                    source: PermissionSource::Global,
                    kind: PermissionReviewKind::Default,
                    tool: Some(ToolKey::McpServer {
                        server: "exa".into()
                    }),
                    scope: None,
                })
        );
    }

    #[test]
    fn permissions_mcp_default_prompt() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[mcp.exa]\ndefault = \"prompt\"\nallow = [\"search\"]\n",
        );
        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::McpServer {
                server: "exa".into()
            }),
            Some(&DefaultEffect::Prompt),
            "MCP server default = prompt should be extracted"
        );
        assert!(perms.rules.is_empty());
        assert!(perms.project_allow_rules.is_empty());
        assert_eq!(
            perms.review_candidates,
            vec![PermissionReviewCandidate {
                source: PermissionSource::Global,
                kind: PermissionReviewKind::Rule,
                tool: Some(ToolKey::McpTool {
                    server: "exa".into(),
                    tool: "search".into()
                }),
                scope: None,
            }]
        );
    }

    #[test]
    fn invalid_native_tool_sections_fail_closed() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        for section in ["", "shell "] {
            write_global_permissions(dir.path(), &format!("[\"{section}\"]\ndeny = true\n"));
            let permissions = load_permissions_inner(dir.path(), Some(global.as_path()));
            assert_eq!(permissions.default, DefaultEffect::Deny);
            assert_eq!(permissions.rules.len(), 1);
            assert!(matches!(permissions.rules[0].tool, ToolKey::Wildcard));
            assert_eq!(permissions.rules[0].effect, Effect::Deny);
        }
    }

    const BUDGET_DRIFT: &str = "a first-party tool must resolve to its own \
         ui.tool_output_lines budget; falling through to `other` silently \
         ignores what the user configured";

    /// Every field distinct, so an arm reading the wrong one cannot pass by
    /// coinciding with the right value.
    const DISTINCT_BUDGETS: ToolOutputLines = ToolOutputLines {
        bash: 1,
        code: 2,
        task: 3,
        index: 4,
        grep: 5,
        read: 6,
        write: 7,
        web: 8,
        other: 9,
    };

    #[test]
    fn every_tool_resolves_to_the_budget_it_is_documented_under() {
        for (field, tools) in ToolOutputLines::FIELD_TOOLS {
            let (_, want) = DISTINCT_BUDGETS
                .fields()
                .into_iter()
                .find(|(name, _)| name == field)
                .expect("every documented budget is a real field");
            for tool in *tools {
                assert_eq!(DISTINCT_BUDGETS.get(tool), want, "{BUDGET_DRIFT}: {tool}");
            }
        }
    }

    /// Adding or renaming a tool has to be a decision about its budget, not a
    /// silent fall-through nobody notices.
    #[test]
    fn every_registered_tool_is_documented_under_a_budget() {
        for name in WORKCELL_NATIVE_TOOL_NAMES
            .iter()
            .chain(CAUDRA_NATIVE_TOOL_NAMES)
        {
            assert!(
                ToolOutputLines::FIELD_TOOLS
                    .iter()
                    .any(|(_, tools)| tools.contains(name)),
                "{BUDGET_DRIFT}: {name} is not listed"
            );
        }
    }
}
