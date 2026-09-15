use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use caudra_config_macro::ConfigSection;
use caudra_storage::paths;
use caudra_storage::retention::{Duration as RetentionDuration, GroupBy, KeepPolicy};
use caudra_storage::thinking::{StoredThinking, ThinkingParseError};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use thiserror::Error;
use tracing::warn;

const PROJECT_DIR: &str = ".caudra";
const PERMISSIONS_FILE: &str = "permissions.toml";
const SHELL_PERMISSION_TOOLS: &[&str] = &["bash", "shell"];
/// Tools whose card never opens on its own. A truncated prefix of one of
/// these bodies carries nothing: a read and a fetch are windows into a
/// document, and a glob, a grep and an index are ordered by path or by source
/// rather than by relevance, so the visible lines are the alphabetically
/// first ones rather than the answer. Each already states its result in the
/// row annotation, so the fold loses no summary. The code-graph lookups are
/// absent on purpose: they rank their rows, so the prefix is the headline.
const DEFAULT_ALWAYS_COLLAPSED: &[&str] = &[
    "file_read",
    "file_glob",
    "file_grep",
    "file_index",
    "webfetch",
];
const PROCESS_ONLY_ENV_VARS: &[&str] = &[
    "HERDR_ENV",
    "HERDR_PANE_ID",
    "HERDR_BIN_PATH",
    "HERDR_SOCKET_PATH",
    "WORKCELL_MCP_CODE_WORKER",
];

pub mod providers;
pub mod steering;
pub mod workcell;

pub use steering::SteeringConfig;

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 50 * 1024;
pub const DEFAULT_MAX_OUTPUT_LINES: usize = 2000;
pub const DEFAULT_FLASH_DURATION_MS: u64 = 1500;
pub const DEFAULT_TYPEWRITER_MS_PER_CHAR: u64 = 4;
pub const DEFAULT_WHICH_KEY_DELAY_MS: u64 = 250;
pub const DEFAULT_MOUSE_SCROLL_LINES: u32 = 3;
pub const DEFAULT_SCROLL_CARD_LINES: u32 = 10;
pub const DEFAULT_MAX_INPUT_LINES: u32 = 20;

pub const MIN_MAX_INPUT_LINES: u32 = 1;

pub const MAX_SERVER_NAME_LEN: usize = 64;

pub const DEFAULT_COMPACTION_BUFFER: CompactionBuffer = CompactionBuffer::Percent(20);
/// Windows that already exclude output need less held back, since the reserve
/// only has to absorb estimation drift rather than a whole response.
pub const DEFAULT_INPUT_BUDGET_COMPACTION_BUFFER: CompactionBuffer = CompactionBuffer::Percent(10);

pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_STREAM_TIMEOUT_SECS: u64 = 300;

pub const DEFAULT_MAX_LOG_BYTES_MB: u64 = 200;
pub const DEFAULT_MAX_LOG_FILES: u32 = 10;
pub const DEFAULT_LOG_LEVEL: LogLevel = LogLevel::Info;
pub const DEFAULT_INPUT_HISTORY_SIZE: usize = 100;
pub const DEFAULT_EPHEMERAL: bool = false;
pub const DEFAULT_RETENTION_SWEEP_INTERVAL_HOURS: u64 = 24;
pub const DEFAULT_RETENTION_TRIM_KEEP_LAST: u32 = 20;
pub const DEFAULT_RETENTION_TRIM_KEEP_WITHIN_DAYS: u32 = 90;
pub const DEFAULT_SNAPSHOTS_ENABLED: bool = true;
pub const DEFAULT_SNAPSHOT_MAX_BYTES_MB: u64 = 512;
pub const DEFAULT_SNAPSHOT_MAX_FILES: u64 = 50_000;
pub const DEFAULT_SNAPSHOT_MAX_FILE_BYTES_MB: u64 = 100;

pub const MIN_OUTPUT_BYTES: usize = 1024;
pub const MIN_OUTPUT_LINES: usize = 10;
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
pub const MIN_INPUT_HISTORY_SIZE: usize = 10;
pub const MIN_CONNECT_TIMEOUT_SECS: u64 = 1;
pub const MIN_STREAM_TIMEOUT_SECS: u64 = 10;
/// Off by default: writing Caudra plugins is a niche task, and the skill's
/// entry costs description tokens in every session that never writes one.
pub const DEFAULT_SKILL_PLUGIN_DEV: bool = false;
/// On by default: the skill is how the model learns to write a workflow for
/// the session it is in, and a workflow is the answer to many multi-step asks.
pub const DEFAULT_SKILL_WORKFLOW_DEV: bool = true;
const SKILL_PLUGIN_DEV_FIELD: &str = "plugin_dev";
const SKILL_WORKFLOW_DEV_FIELD: &str = "workflow_dev";
const SKILL_FIELDS: [&str; 2] = [SKILL_PLUGIN_DEV_FIELD, SKILL_WORKFLOW_DEV_FIELD];
pub const DEFAULT_TASK_MAX_CONCURRENT: usize = 8;
pub const MIN_TASK_MAX_CONCURRENT: usize = 1;
pub const DEFAULT_INDEX_MAX_FILE_SIZE_MB: usize = 2;
pub const MIN_INDEX_MAX_FILE_SIZE_MB: usize = 1;
/// Workcell briefly holds the input bytes and parser-owned source together,
/// so this caps those two buffers at 32 MiB before tree allocation.
pub const MAX_INDEX_MAX_FILE_SIZE_MB: usize = 16;

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

/// Which of [`DEFAULT_BUILTINS`] production still loads from Lua. Empty: every
/// built-in is native now. The sources stay in the tree as Lua-API coverage
/// and as worked examples for plugin authors, so tests and docgen can still
/// load them by name.
pub const ACTIVE_DEFAULT_LUA_PLUGINS: &[&str] = &[];

/// Caudra's own native tools: session-shaped work, orchestration, and the
/// interactive surfaces. Workcell owns everything protocol-neutral.
pub const CAUDRA_NATIVE_TOOL_NAMES: &[&str] = &[
    "batch",
    "image_generate",
    "local_document_apply_patch",
    "local_document_read",
    "local_document_write",
    "memory",
    "question",
    "skill",
    "task",
    "todo_write",
    "tool_output",
    "view_image",
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
    "python_execution",
    "code_map",
    "code_context",
    "code_refs",
    "code_impact",
    "code_expand",
    "execution_environment",
];

/// Tools the agent reaches for on its own to page through an oversized result.
/// They stay enabled whatever the filters say, or a truncated result becomes
/// unreadable.
pub const INTERNAL_COMPANION_TOOL_NAMES: &[&str] = &["tool_output"];

/// The code graph is one mode of work, entered once. Loading its five tools
/// separately would spend five prompt-cache prefixes to answer one question.
pub const CODE_GRAPH_GROUP: &str = "code graph";

/// Built-ins kept out of the request array until `tool_search` loads them.
///
/// Deferred because most sessions never call them, not because there are too
/// many: the code graph answers "I do not know this codebase", `image_generate`
/// needs a subscription and an intent to draw, and `execution_environment`
/// reports host facts `shell` can also reach. `python_execution` is deferred for
/// its size as well as its rate: its manual for Monty's Python subset is the
/// largest definition in the array, and `shell`'s own description already tells
/// the model a code execution tool exists. Everything else is either used
/// constantly or is the only way to do something.
///
/// Tools sharing a group load together. A name here must also appear in one of
/// the registration lists above, or it defers something that does not exist.
///
/// Whether a run actually withholds them is decided per model by
/// [`DeferBuiltinTools`]: the saving is real for a small model and a loss for a
/// capable one, which pays a prompt-cache prefix to load what it would have
/// used anyway.
pub const DEFERRED_BUILTIN_TOOLS: &[DeferredBuiltin] = &[
    DeferredBuiltin::grouped("code_map", CODE_GRAPH_GROUP),
    DeferredBuiltin::grouped("code_context", CODE_GRAPH_GROUP),
    DeferredBuiltin::grouped("code_refs", CODE_GRAPH_GROUP),
    DeferredBuiltin::grouped("code_impact", CODE_GRAPH_GROUP),
    DeferredBuiltin::grouped("code_expand", CODE_GRAPH_GROUP),
    DeferredBuiltin::alone("execution_environment"),
    DeferredBuiltin::alone("image_generate"),
    DeferredBuiltin::alone("python_execution"),
];

pub struct DeferredBuiltin {
    pub name: &'static str,
    pub group: Option<&'static str>,
}

impl DeferredBuiltin {
    const fn grouped(name: &'static str, group: &'static str) -> Self {
        Self {
            name,
            group: Some(group),
        }
    }

    const fn alone(name: &'static str) -> Self {
        Self { name, group: None }
    }
}

pub fn is_deferred_builtin(name: &str) -> bool {
    DEFERRED_BUILTIN_TOOLS
        .iter()
        .any(|deferred| deferred.name == name)
}

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
    Str(&'static str),
}

impl ConfigValue {
    pub fn format_default(&self) -> String {
        match self {
            Self::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            Self::U64(v) => v.to_string(),
            Self::Str(s) => (*s).to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ConfigField {
    pub name: &'static str,
    pub ty: &'static str,
    pub default: ConfigValue,
    pub min: Option<u64>,
    pub env: Option<&'static str>,
    pub description: &'static str,
}

pub const TOP_LEVEL_FIELDS: &[ConfigField] = &[
    ConfigField {
        name: "always_yolo",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        env: None,
        description: "Start every session with YOLO mode (skip permission prompts, deny rules still apply)",
    },
    ConfigField {
        name: "always_fast",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        env: None,
        description: "Start every session with Anthropic fast mode (Opus only; ignored otherwise)",
    },
    ConfigField {
        name: "always_thinking",
        ty: "bool | string",
        default: ConfigValue::Bool(false),
        min: None,
        env: None,
        description: "Start every session with extended thinking (true/\"adaptive\", \"off\", an effort level (\"minimal\" to \"max\"), or a token budget)",
    },
];

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid config: agent.steering.{field}: {message}")]
    InvalidSteering { field: String, message: String },
    #[error("invalid config: {section}.{field} = {value} is below minimum ({min})")]
    BelowMinimum {
        section: &'static str,
        field: &'static str,
        value: u64,
        min: u64,
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

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct RawConfig {
    pub always_yolo: Option<bool>,
    pub always_fast: Option<bool>,
    pub always_thinking: Option<AlwaysThinking>,
    #[serde(default)]
    pub ui: UiFileConfig,
    pub agent: AgentFileConfig,
    pub provider: ProviderFileConfig,
    pub storage: StorageFileConfig,
    pub telemetry: TelemetryConfig,
    pub plugins: HashMap<String, PluginFileConfig>,
}

impl RawConfig {
    pub fn merge(&mut self, overlay: RawConfig) {
        merge_option!(self, overlay, always_yolo, always_fast, always_thinking);
        self.ui.merge(overlay.ui);
        self.agent.merge(overlay.agent);
        self.provider.merge(overlay.provider);
        self.storage.merge(overlay.storage);
        self.telemetry.merge(overlay.telemetry);
        for (name, plugin) in overlay.plugins {
            let entry = self.plugins.entry(name).or_default();
            if plugin.enabled.is_some() {
                entry.enabled = plugin.enabled;
            }
            entry.opts.extend(plugin.opts);
        }
    }

    pub fn into_config(self, no_rtk: bool) -> Result<Config, ConfigError> {
        self.validate_plugin_tables()?;
        let index_max_file_size_mb = self.index_max_file_size_mb()?;
        let task_max_concurrent = self.task_max_concurrent()?;
        let skill_plugin_dev = self.skill_flag(SKILL_PLUGIN_DEV_FIELD, DEFAULT_SKILL_PLUGIN_DEV)?;
        let skill_workflow_dev =
            self.skill_flag(SKILL_WORKFLOW_DEV_FIELD, DEFAULT_SKILL_WORKFLOW_DEV)?;
        let disabled_tools = self.resolve_disabled_tools()?;
        let config = Config {
            always_yolo: self.always_yolo.unwrap_or(false),
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
                skill_plugin_dev,
                skill_workflow_dev,
            ),
            provider: ProviderConfig::from_file(self.provider)?,
            storage: StorageConfig::from_file(self.storage),
            telemetry: self.telemetry,
            permissions: PermissionsConfig::default(),
            plugins: PluginsConfig::from_plugins(self.plugins),
        };
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
        const FIELD: &str = "max_file_size_mb";
        let invalid = |message: String| ConfigError::InvalidNativeToolOption {
            plugin: "index",
            field: FIELD.into(),
            message,
        };
        let Some(opts) = self.native_tool_opts("index", &[FIELD])? else {
            return Ok(DEFAULT_INDEX_MAX_FILE_SIZE_MB);
        };
        let Some(value) = opts.get(FIELD) else {
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
        const FIELD: &str = "max_concurrent";
        let Some(opts) = self.native_tool_opts("task", &[FIELD])? else {
            return Ok(DEFAULT_TASK_MAX_CONCURRENT);
        };
        let Some(value) = opts.get(FIELD) else {
            return Ok(DEFAULT_TASK_MAX_CONCURRENT);
        };
        let invalid = |message: String| ConfigError::InvalidNativeToolOption {
            plugin: "task",
            field: FIELD.into(),
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

#[derive(Deserialize, Default, Debug)]
#[serde(default)]
pub struct PluginFileConfig {
    pub enabled: Option<bool>,
    /// Plugin-specific options passed through opaquely; each plugin declares
    /// and validates its own via `caudra.api.register_options`.
    #[serde(flatten)]
    pub opts: JsonMap<String, JsonValue>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct UiFileConfig {
    pub splash_animation: Option<bool>,
    pub scrollbar: Option<bool>,
    pub touch: Option<TouchMode>,
    pub notifications: Option<NotificationMethod>,
    pub math: Option<MathStyle>,
    pub mermaid: Option<MermaidStyle>,
    pub flash_duration_ms: Option<u64>,
    pub which_key_delay_ms: Option<u64>,
    pub typewriter_ms_per_char: Option<u64>,
    pub mouse_scroll_lines: Option<u32>,
    pub scroll_card_lines: Option<u32>,
    pub always_collapsed: Option<Vec<String>>,
    pub show_thinking: Option<bool>,
    pub show_reminders: Option<bool>,
    pub theme: Option<String>,
    pub theme_light: Option<String>,
    pub clock_format: Option<ClockFormat>,
    pub tool_output_lines: Option<ToolOutputLinesFile>,
    pub max_input_lines: Option<u32>,
    pub update_check: Option<bool>,
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
            notifications,
            flash_duration_ms,
            which_key_delay_ms,
            typewriter_ms_per_char,
            mouse_scroll_lines,
            scroll_card_lines,
            always_collapsed,
            show_thinking,
            show_reminders,
            theme,
            theme_light,
            clock_format,
            max_input_lines,
            update_check
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

/// Whether the tools in [`DEFERRED_BUILTIN_TOOLS`] start outside the request
/// array.
///
/// `Auto` declares them upfront for a model known to be non-small, because it
/// can choose well from a long list and a mid-session load resets a prompt-cache
/// prefix it was already paying to keep. Small and unknown models defer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeferBuiltinTools {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct ToolOutputLinesFile {
    pub bash: Option<usize>,
    pub python_execution: Option<usize>,
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
            self,
            overlay,
            bash,
            python_execution,
            task,
            index,
            grep,
            read,
            write,
            web,
            other
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

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct AgentFileConfig {
    pub steering: Option<SteeringConfig>,
    pub system_prompt_profile: Option<String>,
    pub max_output_bytes: Option<usize>,
    pub max_output_lines: Option<usize>,
    pub compaction_buffer: Option<CompactionBuffer>,
    pub compaction_instructions: Option<String>,
    pub post_compaction_instructions: Option<String>,
    pub generate_titles: Option<bool>,
    pub stale_read_check: Option<bool>,
    pub tool_json_repair: Option<bool>,
    pub eager_batch_dispatch: Option<bool>,
    pub eager_tool_dispatch: Option<bool>,
    pub shell_output_filter: Option<bool>,
    pub defer_builtin_tools: Option<DeferBuiltinTools>,
    pub disabled_tools: Option<Vec<String>>,
}

impl AgentFileConfig {
    fn merge(&mut self, overlay: AgentFileConfig) {
        if let Some(steering) = overlay.steering {
            self.steering.get_or_insert_default().merge(steering);
        }
        merge_option!(
            self,
            overlay,
            system_prompt_profile,
            max_output_bytes,
            max_output_lines,
            compaction_buffer,
            compaction_instructions,
            post_compaction_instructions,
            generate_titles,
            stale_read_check,
            tool_json_repair,
            eager_batch_dispatch,
            eager_tool_dispatch,
            shell_output_filter,
            defer_builtin_tools
        );
        // Restriction only, unlike every other list here: a project must not be
        // able to hand itself back a tool the global config took away.
        if let Some(overlay) = overlay.disabled_tools {
            self.disabled_tools.get_or_insert_default().extend(overlay);
        }
    }
}

#[derive(Deserialize, Default, Debug)]
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

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct StorageFileConfig {
    pub max_log_bytes_mb: Option<u64>,
    pub max_log_files: Option<u32>,
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

#[derive(Deserialize, Default, Debug)]
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
#[derive(Deserialize, Default, Debug)]
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

#[derive(Debug, Clone)]
struct ParsedPermissionRule {
    tool: ToolKey,
    effect: Effect,
}

#[derive(Default)]
struct PermissionsFileConfig {
    default: Option<DefaultEffect>,
    tools: HashMap<String, ToolPermissions>,
    mcp_rules: Vec<ParsedPermissionRule>,
    mcp_defaults: HashMap<ToolKey, DefaultEffect>,
}

impl<'de> Deserialize<'de> for PermissionsFileConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let table = toml::Table::deserialize(deserializer)?;
        let default = table
            .get("default")
            .map(|value| {
                DefaultEffect::deserialize(value.clone()).map_err(serde::de::Error::custom)
            })
            .transpose()?;

        let mut tools = HashMap::new();
        let mut mcp_rules = Vec::new();
        let mut mcp_defaults = HashMap::new();

        for (k, v) in table.iter() {
            if k == "default" {
                continue;
            }
            if k == "mcp" {
                // TOML [mcp.server] creates nested table: mcp → {server → {...}}
                if let Some(mcp_table) = v.as_table() {
                    for (server_name, server_value) in mcp_table {
                        if let Some(server_table) = server_value.as_table() {
                            parse_mcp_server_table(
                                server_name,
                                server_table,
                                &mut mcp_rules,
                                &mut mcp_defaults,
                            )
                            .map_err(serde::de::Error::custom)?;
                        } else {
                            return Err(serde::de::Error::custom(format!(
                                "[mcp.{server_name}] is not a table"
                            )));
                        }
                    }
                } else {
                    return Err(serde::de::Error::custom("[mcp] is not a table"));
                }
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
            mcp_rules,
            mcp_defaults,
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
    pub yolo: bool,
}

#[derive(Clone)]
pub struct Config {
    pub always_yolo: bool,
    pub always_fast: bool,
    pub always_thinking: Option<StoredThinking>,
    pub ui: UiConfig,
    pub agent: AgentConfig,
    pub provider: ProviderConfig,
    pub storage: StorageConfig,
    pub telemetry: TelemetryConfig,
    pub permissions: PermissionsConfig,
    pub plugins: PluginsConfig,
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
        desc = "Touch-friendly pointer handling: auto, on, or off. Widens the scrollbar's hit zone so a finger can tap it, scrolls one line per wheel event instead of mouse_scroll_lines, and leaves text selection to the terminal. Auto detects Termux around Caudra itself, which SSH does not carry, so set this to on when reaching Caudra from a phone over SSH"
    )]
    pub touch: TouchMode,

    #[config(
        default = NotificationMethod::Auto,
        ty = "string",
        default_doc = "auto",
        desc = "Terminal notification method: auto, osc9, bell, or off"
    )]
    pub notifications: NotificationMethod,

    #[config(
        default = MathStyle::Unicode,
        ty = "string",
        default_doc = "unicode",
        desc = "How LaTeX maths renders: unicode (approximate with Unicode) or raw (show the LaTeX source)"
    )]
    pub math: MathStyle,

    #[config(
        default = MermaidStyle::Unicode,
        ty = "string",
        default_doc = "unicode",
        desc = "How mermaid flowcharts render: unicode (draw them with box-drawing characters) or off (leave the fence as code)"
    )]
    pub mermaid: MermaidStyle,

    #[config(default = DEFAULT_FLASH_DURATION_MS, desc = "Duration of flash messages (ms)")]
    pub flash_duration_ms: u64,

    #[config(
        default = DEFAULT_WHICH_KEY_DELAY_MS,
        desc = "How long Ctrl+X waits before listing the chords it can still reach (ms). 0 shows the list at once"
    )]
    pub which_key_delay_ms: u64,

    #[config(default = DEFAULT_TYPEWRITER_MS_PER_CHAR, desc = "Typewriter effect speed (ms/char)")]
    pub typewriter_ms_per_char: u64,

    #[config(default = DEFAULT_MOUSE_SCROLL_LINES, min = MIN_MOUSE_SCROLL_LINES, desc = "Lines per mouse wheel scroll")]
    pub mouse_scroll_lines: u32,

    #[config(
        default = DEFAULT_SCROLL_CARD_LINES,
        desc = "Rows of body a shell, python_execution or task card draws. The window follows new output while it sits at the bottom and pauses when scrolled up. Click inside a window to give it the wheel, which passes back to the transcript at either edge, and drag the bar in its last column to move it directly. `0` turns scrolling off, restoring the `ui.tool_output_lines` budget for those tools. A write is never windowed: it is drawn whole at any setting, as the file it created or as the diff of what it replaced"
    )]
    pub scroll_card_lines: u32,

    #[config(
        ty = "string[]",
        default = "DEFAULT_ALWAYS_COLLAPSED.iter().map(|t| (*t).to_owned()).collect()",
        default_doc = "[\"file_read\", \"file_glob\", \"file_grep\", \"file_index\", \"webfetch\"]",
        desc = "Tools whose card never opens on its own: the call stays a single row in every view mode until you click it. A server-qualified name still matches, so `file_read` also covers `mcp_File_read`. Set to `[]` to opt out"
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
        default = true,
        desc = "Show the messages Caudra writes into the conversation on your behalf: standing reminders, goal check-ins, nudges, and continuations. Each is one dim row that expands on click to the exact text the model was sent. Turn this off to keep the transcript to the conversation alone"
    )]
    pub show_reminders: bool,

    #[config(default = ClockFormat::System, ty = "String", default_doc = "system", desc = "Clock format for timestamps: \"12h\", \"24h\", or \"system\" (follow the OS preference, 24h when unknown)")]
    pub clock_format: ClockFormat,

    #[config(
        default = false,
        env = "CAUDRA_ENABLE_UPDATE_CHECK",
        desc = "Ask GitHub for the latest release on startup and show it in the splash. Off by default, so Caudra makes no such request unless you turn this on"
    )]
    pub update_check: bool,

    #[config(skip, default = "None")]
    pub theme: Option<String>,

    #[config(skip, default = "None")]
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
            flash_duration_ms: f.flash_duration_ms.unwrap_or(DEFAULT_FLASH_DURATION_MS),
            which_key_delay_ms: f.which_key_delay_ms.unwrap_or(DEFAULT_WHICH_KEY_DELAY_MS),
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
            show_reminders: f.show_reminders.unwrap_or(true),
            clock_format: f.clock_format.unwrap_or_default(),
            update_check: f.update_check.unwrap_or(false),
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
    pub python_execution: usize,
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
        python_execution: 5,
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
        ("python_execution", Self::DEFAULT.python_execution),
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
        ("python_execution", &["python_execution"]),
        ("task", &["task"]),
        (
            "index",
            &[
                "file_index",
                "code_map",
                "code_context",
                "code_refs",
                "code_impact",
                "code_expand",
            ],
        ),
        ("grep", &["file_grep", "file_glob"]),
        ("read", &["file_read", "local_document_read"]),
        (
            "write",
            &[
                "file_write",
                "file_edit",
                "file_apply_patch",
                "image_generate",
                "local_document_apply_patch",
                "local_document_write",
                "memory",
            ],
        ),
        ("web", &["webfetch", "websearch"]),
        (
            "other",
            &[
                "batch",
                "execution_environment",
                "question",
                "skill",
                "todo_write",
                "tool_output",
                "view_image",
                "workflow",
            ],
        ),
    ];

    fn from_file(f: Option<ToolOutputLinesFile>) -> Self {
        let d = Self::DEFAULT;
        let f = f.unwrap_or_default();
        Self {
            bash: f.bash.unwrap_or(d.bash),
            python_execution: f.python_execution.unwrap_or(d.python_execution),
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
            ("python_execution", self.python_execution),
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
            "python_execution" => self.python_execution,
            "task" => self.task,
            "index" | "file_index" | "code_map" | "code_context" | "code_refs" | "code_impact"
            | "code_expand" => self.index,
            "file_grep" | "file_glob" | "grep" | "glob" => self.grep,
            "file_read" | "local_document_read" | "read" => self.read,
            "local_document_apply_patch" | "local_document_write" | "memory" => self.write,
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

#[derive(Debug, Clone, ConfigSection, Serialize)]
#[config(section = "agent")]
pub struct AgentConfig {
    // Sharing immutable policy keeps tool contexts from cloning the full model map.
    #[config(skip, default = "Arc::default()")]
    pub steering: Arc<SteeringConfig>,

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
        default_doc = "20%, or 10% when the model's window excludes output",
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
        desc = "Name a new session by summarizing its first prompt with the Title model"
    )]
    pub generate_titles: bool,

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
        default = DeferBuiltinTools::Auto,
        ty = "string",
        default_doc = "auto",
        desc = "When the on-demand built-in tools start outside the request array: `auto` defers them for a small model or one with no supply metadata and declares them upfront for a known non-small model, `always` defers for every model, `never` declares them upfront"
    )]
    pub defer_builtin_tools: DeferBuiltinTools,

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

    #[config(skip, default = DEFAULT_SKILL_PLUGIN_DEV)]
    pub skill_plugin_dev: bool,

    #[config(skip, default = DEFAULT_SKILL_WORKFLOW_DEV)]
    pub skill_workflow_dev: bool,
}

impl AgentConfig {
    fn from_file(
        file: AgentFileConfig,
        no_rtk: bool,
        disabled_tools: Vec<String>,
        index_max_file_size_mb: usize,
        task_max_concurrent: usize,
        skill_plugin_dev: bool,
        skill_workflow_dev: bool,
    ) -> Self {
        Self {
            no_rtk,
            steering: Arc::new(file.steering.unwrap_or_default()),
            system_prompt_profile: file
                .system_prompt_profile
                .filter(|profile| profile != "builtin"),
            max_output_bytes: file.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES),
            max_output_lines: file.max_output_lines.unwrap_or(DEFAULT_MAX_OUTPUT_LINES),
            compaction_buffer: file.compaction_buffer,
            compaction_instructions: file.compaction_instructions,
            post_compaction_instructions: file.post_compaction_instructions,
            generate_titles: file.generate_titles.unwrap_or(true),
            stale_read_check: file.stale_read_check.unwrap_or(true),
            tool_json_repair: file.tool_json_repair.unwrap_or(true),
            eager_tool_dispatch: file
                .eager_tool_dispatch
                .or(file.eager_batch_dispatch)
                .unwrap_or(true),
            shell_output_filter: !no_rtk && file.shell_output_filter.unwrap_or(true),
            defer_builtin_tools: file.defer_builtin_tools.unwrap_or_default(),
            max_turns: None,
            allowed_tools: Vec::new(),
            disabled_tools,
            index_max_file_size_mb,
            task_max_concurrent,
            skill_plugin_dev,
            skill_workflow_dev,
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

    #[config(default = DEFAULT_LOG_LEVEL, ty = "string", default_doc = "info",
             desc = "Minimum severity written to the log file: trace, debug, info, warn, or error. RUST_LOG overrides it")]
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
            log_level: f.log_level.unwrap_or(DEFAULT_LOG_LEVEL),
            input_history_size: f.input_history_size.unwrap_or(DEFAULT_INPUT_HISTORY_SIZE),
            ephemeral: f.ephemeral.unwrap_or(DEFAULT_EPHEMERAL),
            retention: RetentionConfig::from_file(f.retention.unwrap_or_default()),
            snapshots: SnapshotsConfig::from_file(f.snapshots.unwrap_or_default()),
        }
    }
}

/// What one workspace capture may cost before Caudra refuses the workspace and
/// turns file revert off for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotsConfig {
    pub enabled: bool,
    /// Doubles as the object-store cap, because a working tree larger than the
    /// cap is over budget from its very first snapshot.
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
            env: None,
            description: "Capture workspace snapshots. `false` keeps existing snapshots restorable but takes no new ones, so file revert stops covering new work",
        },
        ConfigField {
            name: "max_bytes_mb",
            ty: "u64",
            default: ConfigValue::U64(DEFAULT_SNAPSHOT_MAX_BYTES_MB),
            min: None,
            env: None,
            description: "Largest working tree a capture will take, and the cap on one session's object store. A workspace above it loses file revert rather than paying for a snapshot the store cannot keep",
        },
        ConfigField {
            name: "max_files",
            ty: "u64",
            default: ConfigValue::U64(DEFAULT_SNAPSHOT_MAX_FILES),
            min: None,
            env: None,
            description: "Most files a capture will take, counted after ignore rules",
        },
        ConfigField {
            name: "max_file_bytes_mb",
            ty: "u64",
            default: ConfigValue::U64(DEFAULT_SNAPSHOT_MAX_FILE_BYTES_MB),
            min: None,
            env: None,
            description: "Largest single file a capture will take. A bigger one is left out of the snapshot and left alone on disk, so it cannot be reverted",
        },
    ];

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
            env: None,
            description: "Evaluate policies per working directory (`directory`) or across every session (`none`)",
        },
        ConfigField {
            name: "sweep_interval_hours",
            ty: "u64",
            default: ConfigValue::U64(DEFAULT_RETENTION_SWEEP_INTERVAL_HOURS),
            min: None,
            env: None,
            description: "Hours between background sweeps. `0` disables the sweep; `caudra storage` commands still work",
        },
        ConfigField {
            name: "trim",
            ty: "table",
            default: ConfigValue::Str("{ keep_last = 20, keep_within = \"90d\" }"),
            min: None,
            env: None,
            description: "Sessions outside this policy lose snapshots, tool output files, archives, and large rich outputs but stay resumable",
        },
        ConfigField {
            name: "forget",
            ty: "table",
            default: ConfigValue::Str("{}"),
            min: None,
            env: None,
            description: "Sessions outside this policy are deleted. Empty means never delete automatically",
        },
    ];

    pub fn default_trim() -> KeepPolicy {
        KeepPolicy {
            keep_last: Some(DEFAULT_RETENTION_TRIM_KEEP_LAST),
            keep_within: Some(RetentionDuration {
                days: DEFAULT_RETENTION_TRIM_KEEP_WITHIN_DAYS,
                ..RetentionDuration::default()
            }),
            ..KeepPolicy::default()
        }
    }

    fn from_file(f: RetentionFileConfig) -> Self {
        Self {
            group_by: f.group_by.unwrap_or_default(),
            sweep_interval_hours: f
                .sweep_interval_hours
                .unwrap_or(DEFAULT_RETENTION_SWEEP_INTERVAL_HOURS),
            trim: f.trim.unwrap_or_else(Self::default_trim),
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

    #[config(default = None, ty = "string", default_doc = "-",
             env = "OTEL_EXPORTER_OTLP_PROTOCOL",
             desc = "OTLP protocol: `grpc`, `http/protobuf`, or `http/json`. Required when an exporter is `otlp`")]
    pub protocol: Option<String>,

    #[config(default = None, ty = "string", default_doc = "-",
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

    #[config(default = None, ty = "string", default_doc = "-",
             env = "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
             desc = "Metrics-only protocol override")]
    pub metrics_protocol: Option<String>,

    #[config(default = None, ty = "string", default_doc = "-",
             env = "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
             desc = "Metrics-only endpoint, used verbatim with no path appended")]
    pub metrics_endpoint: Option<String>,

    #[config(default = None, ty = "table", default_doc = "{}",
             env = "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
             desc = "Metrics-only headers, merged over `headers`")]
    pub metrics_headers: Option<BTreeMap<String, String>>,

    #[config(default = None, ty = "integer", default_doc = "-",
             env = "OTEL_EXPORTER_OTLP_METRICS_TIMEOUT",
             desc = "Metrics-only request timeout (ms)")]
    pub metrics_timeout_ms: Option<u64>,

    #[config(default = None, ty = "string", default_doc = "-",
             env = "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
             desc = "Logs-only protocol override")]
    pub logs_protocol: Option<String>,

    #[config(default = None, ty = "string", default_doc = "-",
             env = "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
             desc = "Logs-only endpoint, used verbatim with no path appended")]
    pub logs_endpoint: Option<String>,

    #[config(default = None, ty = "table", default_doc = "{}",
             env = "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
             desc = "Logs-only headers, merged over `headers`")]
    pub logs_headers: Option<BTreeMap<String, String>>,

    #[config(default = None, ty = "integer", default_doc = "-",
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
        self.ui.validate_all()?;
        self.agent.validate()?;
        self.agent.steering.validate()?;
        self.provider.validate()?;
        self.storage.validate()?;
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

fn push_mcp_tool_rule(
    rules: &mut Vec<ParsedPermissionRule>,
    server_name: &str,
    tool_name: &str,
    effect: Effect,
) -> Result<(), String> {
    let qualified = format!("{server_name}.{tool_name}");
    match ToolKey::parse(&qualified) {
        Ok(key) => rules.push(ParsedPermissionRule { tool: key, effect }),
        Err(error) => return Err(format!("invalid MCP tool name: {error}")),
    }
    Ok(())
}

fn parse_mcp_server_table(
    server_name: &str,
    table: &toml::Table,
    rules: &mut Vec<ParsedPermissionRule>,
    mcp_defaults: &mut HashMap<ToolKey, DefaultEffect>,
) -> Result<(), String> {
    if !is_valid_server_name(server_name) {
        return Err(format!(
            "invalid MCP server name {server_name}; expected only alphanumeric characters and hyphens"
        ));
    }

    for (key, value) in table {
        match key.as_str() {
            "allow" | "ask" | "deny" => {
                let effect = match key.as_str() {
                    "allow" => Effect::Allow,
                    "ask" => Effect::Ask,
                    "deny" => Effect::Deny,
                    _ => unreachable!(),
                };
                match value {
                    toml::Value::Array(arr) => {
                        for item in arr {
                            let tool_name = item.as_str().ok_or_else(|| {
                                format!("[mcp.{server_name}].{key} entries must be strings")
                            })?;
                            if tool_name == "*" {
                                rules.push(ParsedPermissionRule {
                                    tool: ToolKey::McpServer {
                                        server: server_name.into(),
                                    },
                                    effect,
                                });
                                continue;
                            }
                            push_mcp_tool_rule(rules, server_name, tool_name, effect)?;
                        }
                    }
                    toml::Value::Boolean(true) => {
                        rules.push(ParsedPermissionRule {
                            tool: ToolKey::McpServer {
                                server: server_name.into(),
                            },
                            effect,
                        });
                    }
                    toml::Value::Boolean(false) => {
                        // No-op: explicitly disabled.
                    }
                    toml::Value::String(s) => {
                        let tool_name = s.as_str();
                        if tool_name == "*" {
                            // Treat `allow = "*"` the same as `allow = ["*"]` —
                            // create a hard McpServer rule, not a default.
                            rules.push(ParsedPermissionRule {
                                tool: ToolKey::McpServer {
                                    server: server_name.into(),
                                },
                                effect,
                            });
                        } else {
                            push_mcp_tool_rule(rules, server_name, tool_name, effect)?;
                        }
                    }
                    _ => {
                        return Err(format!(
                            "[mcp.{server_name}].{key} must be an array of tool names, a tool name, or a boolean"
                        ));
                    }
                }
            }
            "default" => {
                if let Ok(d) = value.clone().try_into::<DefaultEffect>() {
                    mcp_defaults.insert(
                        ToolKey::McpServer {
                            server: server_name.into(),
                        },
                        d,
                    );
                } else {
                    return Err(format!(
                        "invalid [mcp.{server_name}].default; expected allow, deny, or prompt"
                    ));
                }
            }
            other => return Err(format!("unknown key [mcp.{server_name}].{other}")),
        }
    }
    Ok(())
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
    for (key, d) in &global.mcp_defaults {
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
    for (key, d) in &project.mcp_defaults {
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
    push_parsed_rules(&mut rules, &global.mcp_rules, Effect::Deny);
    push_rules(&mut rules, &global.tools, Effect::Deny);
    push_rules(&mut rules, &project.tools, Effect::Deny);
    push_parsed_rules(&mut rules, &project.mcp_rules, Effect::Deny);
    for config in [&global, &project] {
        push_rules(&mut rules, &config.tools, Effect::Ask);
        push_parsed_rules(&mut rules, &config.mcp_rules, Effect::Ask);
    }
    push_rules(&mut rules, &global.tools, Effect::Allow);

    let mut project_allow_rules = Vec::new();
    push_rules(&mut project_allow_rules, &project.tools, Effect::Allow);

    let mut project_restrictive_rules = Vec::new();
    push_rules(&mut project_restrictive_rules, &project.tools, Effect::Deny);
    push_parsed_rules(
        &mut project_restrictive_rules,
        &project.mcp_rules,
        Effect::Deny,
    );
    push_rules(&mut project_restrictive_rules, &project.tools, Effect::Ask);
    push_parsed_rules(
        &mut project_restrictive_rules,
        &project.mcp_rules,
        Effect::Ask,
    );

    let mut review_candidates = Vec::new();
    push_review_candidates(&mut review_candidates, PermissionSource::Global, &global);
    push_review_candidates(&mut review_candidates, PermissionSource::Project, &project);
    PermissionsConfig {
        default,
        tool_defaults,
        rules,
        project_allow_rules,
        project_restrictive_rules,
        review_candidates,
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
    parsed_rules: &[ParsedPermissionRule],
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
    for (tool, default) in &config.mcp_defaults {
        if *default == DefaultEffect::Allow {
            candidates.push(PermissionReviewCandidate {
                source,
                kind: PermissionReviewKind::Default,
                tool: Some(tool.clone()),
                scope: None,
            });
        }
    }
    for rule in &config.mcp_rules {
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
    !PROCESS_ONLY_ENV_VARS.contains(&key)
}

fn load_env_files_with_global(cwd: &Path, global: Option<&Path>) {
    load_env_files_scoped(cwd, global, true);
}

fn load_env_files_scoped(cwd: &Path, global: Option<&Path>, include_project: bool) {
    let mut vars = HashMap::new();
    if let Some(path) = global {
        collect_env_vars(&path.join(".env"), &mut vars);
    }
    if include_project {
        collect_env_vars(&cwd.join(PROJECT_DIR).join(".env"), &mut vars);
    }

    for (key, value) in vars {
        if env_file_var_is_allowed(&key) && std::env::var_os(&key).is_none() {
            // SAFETY: single-threaded at startup, before any async runtime
            unsafe { std::env::set_var(&key, &value) };
        }
    }
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
    let mut global_perms = PermissionsFileConfig::default();
    if let Some(dir) = global_dir {
        let path = dir.join(PERMISSIONS_FILE);
        match read_permissions_file(&path) {
            Ok(Some(permissions)) => global_perms = permissions,
            Ok(None) => {}
            Err(error) => {
                warn!(path = %path.display(), error, "permissions failed closed");
                return fail_closed_permissions();
            }
        }
    }

    if !include_project {
        return build_permissions(global_perms, PermissionsFileConfig::default());
    }
    let project_path = cwd.join(PROJECT_DIR).join(PERMISSIONS_FILE);
    let project_perms = match read_permissions_file(&project_path) {
        Ok(Some(permissions)) => permissions,
        Ok(None) => PermissionsFileConfig::default(),
        Err(error) => {
            warn!(path = %project_path.display(), error, "permissions failed closed");
            return fail_closed_permissions();
        }
    };

    build_permissions(global_perms, project_perms)
}

fn read_permissions_file(path: &Path) -> Result<Option<PermissionsFileConfig>, String> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read permissions: {error}")),
    };
    toml::from_str(&content)
        .map(Some)
        .map_err(|error| format!("cannot parse permissions: {error}"))
}

fn fail_closed_permissions() -> PermissionsConfig {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use test_case::test_case;

    fn plugin_enabled(enabled: bool) -> PluginFileConfig {
        PluginFileConfig {
            enabled: Some(enabled),
            opts: JsonMap::new(),
        }
    }

    fn write_global_permissions(dir: &Path, content: &str) {
        let perms_dir = dir.join(".config/caudra");
        fs::create_dir_all(&perms_dir).unwrap();
        fs::write(perms_dir.join("permissions.toml"), content).unwrap();
    }

    fn global_config_dir(dir: &Path) -> PathBuf {
        dir.join(".config/caudra")
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

    #[test_case("auto", DeferBuiltinTools::Auto ; "auto")]
    #[test_case("always", DeferBuiltinTools::Always ; "always")]
    #[test_case("never", DeferBuiltinTools::Never ; "never")]
    fn defer_builtin_tools_deserialize(value: &str, expected: DeferBuiltinTools) {
        let raw: RawConfig =
            toml::from_str(&format!("[agent]\ndefer_builtin_tools = \"{value}\"\n")).unwrap();
        assert_eq!(
            raw.into_config(false).unwrap().agent.defer_builtin_tools,
            expected
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
            always_yolo: Some(false),
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
            always_yolo: Some(true),
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

        assert_eq!(base.always_yolo, Some(true), "overlay wins");
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
            always_yolo: false,
            always_fast: false,
            always_thinking: None,
            ui: UiConfig::default(),
            agent: AgentConfig::default(),
            provider: ProviderConfig::default(),
            storage: StorageConfig::default(),
            telemetry: TelemetryConfig::default(),
            permissions: PermissionsConfig::default(),
            plugins: PluginsConfig::default(),
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
             [bash]\nallow = [\n    \"cargo *\",\n]\ndeny = [\n    \"rm -rf *\",\n]\n",
        );

        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(perms.default, DefaultEffect::Prompt);
        assert_eq!(perms.rules.len(), 2);
        assert!(perms.rules.contains(&PermissionRule {
            tool: ToolKey::native("bash"),
            scope: Some("cargo *".into()),
            effect: Effect::Allow,
        }));
        assert!(perms.rules.contains(&PermissionRule {
            tool: ToolKey::native("bash"),
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
            "[bash]\nallow = [\"git *\"]\ndeny = [\"rm -rf *\"]\n",
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
            tool: ToolKey::native("bash"),
            scope: Some("git *".into()),
            effect: Effect::Allow,
        }));
        assert!(perms.rules.contains(&PermissionRule {
            tool: ToolKey::native("bash"),
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
             [bash]\nallow = true\ndefault = \"allow\"\n\
             [shell]\nallow = [\"git status *\"]\n\
             [read]\nallow = [\"src/**\"]\n\
             [mcp.deepwiki]\nallow = true\ndefault = \"allow\"\n\
             [mcp.github]\nallow = [\"search\", \"*\"]\n",
        );

        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(perms.default, DefaultEffect::Prompt);
        assert_eq!(perms.rules.len(), 2);
        assert!(perms.rules.contains(&PermissionRule {
            tool: ToolKey::native("bash"),
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
                    tool: Some(ToolKey::native("bash")),
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
             [bash]\ndeny = [\"rm *\"]\ndefault = \"prompt\"\n\
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
            perms.tool_defaults.get(&ToolKey::native("bash")),
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
    fn project_trust_material_includes_denies_and_asks() {
        let global = PermissionsFileConfig::default();
        let project: PermissionsFileConfig = toml::from_str(
            "[shell]\nallow = [\"git *\"]\nask = [\"git push *\"]\ndeny = [\"git clean *\"]\n",
        )
        .unwrap();

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
            "[bash]\nallow = [\"git *\"]\ndeny = [\"rm *\"]\n",
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
            "default = \"deny\"\n\n[bash]\ndefault = \"allow\"\nallow = [\"cargo *\"]\n",
        );

        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(perms.default, DefaultEffect::Deny);
        assert!(!perms.tool_defaults.contains_key(&ToolKey::native("bash")));
        assert_eq!(perms.review_candidates.len(), 1);
        assert!(perms.rules.contains(&PermissionRule {
            tool: ToolKey::native("bash"),
            scope: Some("cargo *".into()),
            effect: Effect::Allow,
        }));
        assert!(
            perms
                .review_candidates
                .contains(&PermissionReviewCandidate {
                    source: PermissionSource::Global,
                    kind: PermissionReviewKind::Default,
                    tool: Some(ToolKey::native("bash")),
                    scope: None,
                })
        );
    }

    #[test]
    fn permissions_default_merge_project_overrides_global_per_tool() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[bash]\ndefault = \"allow\"\n");
        let caudra_dir = dir.path().join(".caudra");
        fs::create_dir_all(&caudra_dir).unwrap();
        fs::write(
            caudra_dir.join("permissions.toml"),
            "[bash]\ndefault = \"deny\"\n",
        )
        .unwrap();

        let perms = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::native("bash")).copied(),
            Some(DefaultEffect::Deny)
        );
    }

    #[test]
    fn project_prompt_defaults_cannot_weaken_global_denies() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "default = \"deny\"\n\n[bash]\ndefault = \"deny\"\n",
        );
        let caudra_dir = dir.path().join(".caudra");
        fs::create_dir_all(&caudra_dir).unwrap();
        fs::write(
            caudra_dir.join("permissions.toml"),
            "default = \"prompt\"\n\n[bash]\ndefault = \"prompt\"\n",
        )
        .unwrap();

        let permissions = load_permissions_inner(dir.path(), Some(global.as_path()));
        assert_eq!(permissions.default, DefaultEffect::Deny);
        assert_eq!(
            permissions
                .tool_defaults
                .get(&ToolKey::native("bash"))
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
    fn retention_defaults_trim_only() {
        let config = RawConfig::default().into_config(false).unwrap();
        let retention = config.storage.retention;
        assert_eq!(retention.group_by, GroupBy::Directory);
        assert_eq!(
            retention.sweep_interval_hours,
            DEFAULT_RETENTION_SWEEP_INTERVAL_HOURS
        );
        assert_eq!(retention.trim, RetentionConfig::default_trim());
        assert!(retention.forget.is_empty());
    }

    #[test]
    fn ephemeral_storage_defaults_off_and_parses_true() {
        let defaults = RawConfig::default().into_config(false).unwrap();
        let configured: RawConfig = toml::from_str("[storage]\nephemeral = true\n").unwrap();

        assert!(!defaults.storage.ephemeral);
        assert!(configured.into_config(false).unwrap().storage.ephemeral);
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

    #[test_case(true; "enabled")]
    #[test_case(false; "disabled")]
    fn update_check_deserializes(enabled: bool) {
        let raw: RawConfig = toml::from_str(&format!("[ui]\nupdate_check = {enabled}\n")).unwrap();
        assert_eq!(raw.ui.update_check, Some(enabled));
    }

    #[test]
    fn update_check_missing_defaults_off() {
        let raw: RawConfig = toml::from_str("").unwrap();
        let config = raw.into_config(false).unwrap();
        assert!(
            !config.ui.update_check,
            "the release check must stay off until asked for"
        );
    }

    #[test]
    fn update_check_overlay_wins() {
        let mut base: RawConfig = toml::from_str("[ui]\nupdate_check = false\n").unwrap();
        base.merge(toml::from_str("[ui]\nupdate_check = true\n").unwrap());
        assert_eq!(base.ui.update_check, Some(true));
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

    /// Most readers never write a `[ui]` table, so these two defaults are what
    /// the transcript actually looks like. Losing either silently changes every
    /// session: an empty collapse list reopens every read, and a zero window
    /// turns the fixed shell and write cards back into unbounded bodies.
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

    /// Both fields ride the same `merge_option!` list, and a field left off it
    /// is not a compile error — it just silently pins the global value, so a
    /// project could never soften or tighten either setting.
    #[test]
    fn card_display_overlay_wins_over_the_layer_below() {
        let mut base: RawConfig = toml::from_str(&format!(
            "[ui]\nscroll_card_lines = {CONFIGURED_SCROLL_CARD_LINES}\nalways_collapsed = []\n"
        ))
        .unwrap();
        base.merge(
            toml::from_str(&format!(
                "[ui]\nscroll_card_lines = {SCROLL_CARD_LINES_OFF}\nalways_collapsed = [\"{COLLAPSE_OVERRIDE_TOOL}\"]\n"
            ))
            .unwrap(),
        );

        assert_eq!(
            base.ui.scroll_card_lines,
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

    #[test_case("", DEFAULT_SKILL_PLUGIN_DEV, DEFAULT_SKILL_WORKFLOW_DEV ; "defaults")]
    #[test_case("plugin_dev = true", true, DEFAULT_SKILL_WORKFLOW_DEV ; "plugin_dev_alone")]
    #[test_case("workflow_dev = false", DEFAULT_SKILL_PLUGIN_DEV, false ; "workflow_dev_alone")]
    #[test_case("plugin_dev = true\nworkflow_dev = false", true, false ; "both")]
    fn skill_flags_are_read_independently(options: &str, plugin_dev: bool, workflow_dev: bool) {
        let raw: RawConfig = toml::from_str(&format!("[plugins.skill]\n{options}\n")).unwrap();
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.agent.skill_plugin_dev, plugin_dev);
        assert_eq!(config.agent.skill_workflow_dev, workflow_dev);
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
    #[test_case("github.create_issue" ; "mcp_tool")]
    #[test_case("github.*" ; "mcp_server")]
    fn agent_disabled_tools_accepts(tool: &str) {
        let raw: RawConfig =
            toml::from_str(&format!("[agent]\ndisabled_tools = [\"{tool}\"]\n")).unwrap();
        assert_eq!(raw.into_config(false).unwrap().agent.disabled_tools, [tool]);
    }

    #[test_case("file_wrte" ; "typo")]
    #[test_case("bash" ; "legacy_plugin_id_is_not_a_tool")]
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
        python_execution: 2,
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
