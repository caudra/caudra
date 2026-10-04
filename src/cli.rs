use std::path::PathBuf;

use clap::builder::{PossibleValue, PossibleValuesParser, TypedValueParser};
use clap::{
    Args, Command as ClapCommand, CommandFactory, Error as CliError, FromArgMatches, Parser,
    Subcommand, ValueEnum, error::ErrorKind, value_parser,
};
use color_eyre::Result;
use color_eyre::eyre::bail;

use caudra_agent::peers::script::{DEFAULT_LABEL, parse_label};
use caudra_agent::peers::topics::{parse_pattern, parse_topic};
use caudra_agent::peers::{parse_handle, parse_handle_address};
use caudra_agent::tools::all_builtin_tool_names;
use caudra_config::files::{self, ConfigFile};
use caudra_config::sandbox::LeaseSeconds;
use caudra_config::{Feature, FeatureDisabled, FeatureFlags, is_disableable_tool};
use caudra_storage::auth::WorkcellCredentialName;
use caudra_storage::retention::{Duration as RetentionDuration, GroupBy, KeepPolicy};
use caudra_storage::sessions::PermissionMode;

use crate::print::OutputFormat;
use crate::startup::Startup;

const DEFAULT_LOG_LINES: usize = 200;
const DEFAULT_MESSAGE_LIMIT: u32 = 20;
const MAX_MESSAGE_LIMIT: i64 = 1000;
const PERMISSION_MODE_CONFLICT: &str = "--auto cannot be used with --yolo";
const NO_REFERENCE: &str = "this file has no reference";

#[derive(Clone, ValueEnum, Default)]
pub enum PromptVariant {
    #[default]
    System,
    Research,
    General,
}

#[derive(Clone, ValueEnum, Default)]
pub enum InputFormat {
    #[default]
    Text,
    StreamJson,
}

#[derive(Clone, Copy, ValueEnum, Default)]
#[value(rename_all = "lower")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

impl From<LogLevel> for caudra_storage::log::record::Level {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Trace => Self::Trace,
            LogLevel::Debug => Self::Debug,
            LogLevel::Info => Self::Info,
            LogLevel::Warn => Self::Warn,
            LogLevel::Error => Self::Error,
        }
    }
}

#[derive(Parser)]
#[command(
    name = "caudra",
    version,
    about = "Terminal coding agent that turns context into effective action"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub workcell: WorkcellSelectorArgs,

    /// Non-interactive mode. Runs the prompt and exits. Compatible with Claude Code's --print flag
    #[arg(short, long)]
    pub print: bool,

    /// Initial message. Combined with piped stdin when both are present
    #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
    pub prompt: Option<String>,

    /// Store session data in a temporary directory removed when Caudra exits
    #[arg(long, global = true)]
    pub ephemeral: bool,

    #[arg(
        long,
        global = true,
        help = "Turn off file change recording and file revert for this run, locally and remotely"
    )]
    pub no_snapshots: bool,

    /// Attach an image to the prompt in --print mode as vision content (repeatable)
    #[arg(long = "image", value_name = "PATH")]
    pub images: Vec<PathBuf>,

    /// Model spec (provider/model-id). Defaults to last used model, or claude-opus-4-6
    #[arg(short, long, global = true)]
    pub model: Option<String>,

    /// Include full turn-by-turn messages in --print output
    #[arg(long)]
    pub verbose: bool,

    /// Resume the most recent session in this directory
    #[arg(short = 'c', long = "continue")]
    pub continue_session: bool,

    /// Resume a specific session by its ID, in the directory it works in
    #[arg(short = 's', long, alias = "resume")]
    pub session: Option<String>,

    /// Output format for --print mode
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub output_format: OutputFormat,

    /// Input format (text or stream-json for SDK mode)
    #[arg(long, value_enum, default_value_t = InputFormat::Text)]
    pub input_format: InputFormat,

    /// Skip loading custom commands from every user and project command directory
    #[arg(long)]
    pub no_commands: bool,

    /// Disable model-facing shell output filtering.
    #[arg(long, global = true)]
    pub no_rtk: bool,

    /// Skip user `init.lua` files (global and project). The Lua host stays
    /// up and every built-in tool is native, so nothing else is lost. Use
    /// this to recover from a broken `init.lua` or keymap override. Only Lua
    /// `init.lua` files are affected; `permissions.toml`, custom commands,
    /// and env files load as usual.
    #[arg(long, global = true)]
    pub no_plugins: bool,

    /// Run plugin Lua on the interpreter with full debug info (no native codegen)
    #[arg(long, global = true)]
    pub no_jit: bool,

    /// Skip all permission prompts (allow everything)
    #[arg(
        long,
        alias = "dangerously-skip-permissions",
        global = true,
        conflicts_with = "auto"
    )]
    pub yolo: bool,

    /// Run unmatched tool calls automatically while preserving explicit permission prompts
    #[arg(long, global = true, conflicts_with = "yolo")]
    pub auto: bool,

    /// Exit after the agent completes (for automation workflows)
    #[arg(long)]
    pub exit_on_done: bool,

    /// Give the initial session this cross-session messaging name instead of a generated one; a new session also takes it as its title
    #[arg(long, value_name = "NAME", value_parser = parse_handle, conflicts_with = "print")]
    pub name: Option<String>,

    /// Subscribe the initial session to a cross-session topic pattern, kept across resumes; repeat for more
    #[arg(long = "topic", value_name = "PATTERN", value_parser = parse_pattern, conflicts_with = "print")]
    pub topics: Vec<String>,

    /// Let cross-session broadcasts reach the initial session, kept across resumes
    #[arg(long, conflicts_with = "print")]
    pub receive_broadcasts: bool,

    /// Pre-approve tools (comma-separated). Accepts PascalCase (Claude Code) or snake_case.
    #[arg(
        long,
        value_delimiter = ',',
        visible_alias = "allowedTools",
        global = true
    )]
    pub allowed_tools: Vec<String>,

    /// Disallowed tools (comma-separated).
    #[arg(
        long,
        value_delimiter = ',',
        visible_alias = "disallowedTools",
        global = true
    )]
    pub disallowed_tools: Vec<String>,

    /// Session ID for SDK mode
    #[arg(long)]
    pub session_id: Option<String>,

    /// Fork the loaded session under a new ID
    #[arg(long)]
    pub fork_session: bool,

    /// Maximum number of agent turns
    #[arg(long)]
    pub max_turns: Option<u32>,

    /// System prompt override
    #[arg(long)]
    pub system_prompt: Option<String>,

    /// Select a user system prompt profile
    #[arg(long, value_name = "NAME", conflicts_with = "system_prompt")]
    pub system_prompt_profile: Option<String>,

    /// Append to system prompt
    #[arg(long)]
    pub append_system_prompt: Option<String>,

    /// Permission mode for SDK
    #[arg(long)]
    pub permission_mode: Option<String>,

    /// Include partial streaming messages in SDK output
    #[arg(long)]
    pub include_partial_messages: bool,

    /// Permission prompt tool (accepted for compatibility but ignored)
    #[arg(long, hide = true)]
    pub permission_prompt_tool: Option<String>,

    // Accepted but ignored, so Claude Code SDK callers don't break.
    #[arg(long, hide = true)]
    pub fallback_model: Option<String>,
    #[arg(long, hide = true)]
    pub settings: Option<String>,
    #[arg(long, hide = true)]
    pub setting_sources: Option<String>,
    #[arg(long, hide = true)]
    pub add_dir: Option<String>,
    #[arg(long, hide = true)]
    pub strict_mcp_config: bool,
    #[arg(long, hide = true)]
    pub include_hook_events: bool,
    #[arg(long, hide = true)]
    pub mcp_config: Option<String>,
    #[arg(long, hide = true)]
    pub tools: Option<String>,
    #[arg(long, hide = true)]
    pub betas: Option<String>,
    #[arg(long, hide = true)]
    pub max_thinking_tokens: Option<String>,
    #[arg(long, hide = true)]
    pub effort: Option<String>,
    #[arg(long, hide = true)]
    pub json_schema: Option<String>,
    #[arg(long, hide = true)]
    pub max_budget_usd: Option<String>,
    #[arg(long, hide = true)]
    pub thinking: Option<String>,
    #[arg(long, hide = true)]
    pub thinking_display: Option<String>,

    /// Never parsed from arguments: `dispatch` fills it from the global
    /// caudra.toml read before parsing.
    #[arg(skip)]
    pub startup: Startup,
}

impl Cli {
    /// Parses the process arguments against a tree whose help lists only
    /// what `features` turns on. Hidden commands still parse, so running one
    /// names the switch it needs instead of calling it unknown.
    pub fn parse_for(features: FeatureFlags) -> Result<Self, CliError> {
        let mut matches = Self::command_for(features).try_get_matches()?;
        Self::from_arg_matches_mut(&mut matches)
            .map_err(|error| error.format(&mut Self::command_for(features)))?
            .validate()
    }

    pub fn command_for(features: FeatureFlags) -> ClapCommand {
        let off = |feature| !features.enabled(feature);
        let sandboxes_off = off(Feature::Sandboxes);
        let direct_off = off(Feature::RemoteWorkcell);
        Self::command()
            .mut_arg("sandbox", |arg| arg.hide(sandboxes_off))
            .mut_arg("sandbox_resume", |arg| arg.hide(sandboxes_off))
            .mut_arg("profile", |arg| arg.hide(direct_off))
            .mut_arg("endpoint", |arg| arg.hide(direct_off))
            .mut_arg("cwd", |arg| arg.hide(direct_off))
            .mut_arg("credential_ref", |arg| arg.hide(direct_off))
            .mut_arg("auto", |arg| arg.hide(off(Feature::DecisionEngine)))
            .mut_arg("name", |arg| arg.hide(off(Feature::CrossSessionMessaging)))
            .mut_arg("topics", |arg| {
                arg.hide(off(Feature::CrossSessionMessaging))
            })
            .mut_arg("receive_broadcasts", |arg| {
                arg.hide(off(Feature::CrossSessionMessaging))
            })
            .mut_arg("no_jit", |arg| arg.hide(off(Feature::LuaPlugins)))
            .mut_subcommand("sandbox", |command| command.hide(sandboxes_off))
            .mut_subcommand("remote", |command| {
                command.hide(sandboxes_off && direct_off)
            })
            .mut_subcommand("decisions", |command| {
                command.hide(off(Feature::DecisionEngine))
            })
            .mut_subcommand("message", |command| {
                command.hide(off(Feature::CrossSessionMessaging))
            })
            .mut_subcommand("auth", |auth| {
                auth.mut_subcommand("sandbox", |command| command.hide(sandboxes_off))
                    .mut_subcommand("workcell", |command| command.hide(direct_off))
            })
    }

    pub fn validate(self) -> Result<Self, CliError> {
        if self.auto && self.yolo {
            return Err(
                Self::command().error(ErrorKind::ArgumentConflict, PERMISSION_MODE_CONFLICT)
            );
        }
        Ok(self)
    }

    pub fn permission_mode_override(&self) -> Option<PermissionMode> {
        if self.yolo {
            Some(PermissionMode::Yolo)
        } else if self.auto {
            Some(PermissionMode::Auto)
        } else {
            None
        }
    }

    pub fn warn_ignored_flags(&self) {
        let ignored = [
            (
                "permission-prompt-tool",
                self.permission_prompt_tool.is_some(),
            ),
            ("fallback-model", self.fallback_model.is_some()),
            ("settings", self.settings.is_some()),
            ("setting-sources", self.setting_sources.is_some()),
            ("add-dir", self.add_dir.is_some()),
            ("strict-mcp-config", self.strict_mcp_config),
            ("include-hook-events", self.include_hook_events),
            ("mcp-config", self.mcp_config.is_some()),
            ("tools", self.tools.is_some()),
            ("betas", self.betas.is_some()),
            ("max-thinking-tokens", self.max_thinking_tokens.is_some()),
            ("effort", self.effort.is_some()),
            ("json-schema", self.json_schema.is_some()),
            ("max-budget-usd", self.max_budget_usd.is_some()),
            ("thinking", self.thinking.is_some()),
            ("thinking-display", self.thinking_display.is_some()),
        ];
        for (flag, set) in &ignored {
            if *set {
                eprintln!("warning: --{flag} is accepted but ignored");
            }
        }
    }

    pub fn is_sdk_mode(&self) -> bool {
        self.print && matches!(self.input_format, InputFormat::StreamJson)
    }

    /// Lua runs only when the global caudra.toml opts in, and `--no-plugins`
    /// still forces it off.
    pub fn runs_lua(&self) -> bool {
        !self.no_plugins && self.startup.features.enabled(Feature::LuaPlugins)
    }
}

#[derive(Args, Default, Clone)]
pub struct WorkcellSelectorArgs {
    /// Attach an existing saved sandbox. Never creates an instance implicitly.
    #[arg(long, value_name = "NAME", global = true, conflicts_with_all = ["profile", "endpoint", "cwd", "credential_ref"])]
    pub sandbox: Option<String>,

    /// Explicitly permit cold-boot resume of a paused selected sandbox
    #[arg(long, global = true)]
    pub sandbox_resume: bool,

    /// Select [workcell.profiles.NAME] from the local user workcell.toml (version = 1)
    #[arg(
        long = "workcell-profile",
        value_name = "NAME",
        global = true,
        conflicts_with_all = ["endpoint", "cwd", "credential_ref"]
    )]
    pub profile: Option<String>,

    /// Select a Workcell endpoint (HTTPS, or HTTP on a numeric loopback address)
    #[arg(
        long = "workcell-endpoint",
        value_name = "URL",
        global = true,
        requires = "cwd",
        conflicts_with = "profile"
    )]
    pub endpoint: Option<String>,

    /// Root-relative working directory at the selected Workcell endpoint
    #[arg(
        long = "workcell-cwd",
        value_name = "PATH",
        global = true,
        requires = "endpoint",
        conflicts_with = "profile"
    )]
    pub cwd: Option<String>,

    /// Named bearer credential reference (credential:NAME)
    #[arg(
        long = "workcell-credential-ref",
        value_name = "credential:NAME",
        global = true,
        requires_all = ["endpoint", "cwd"],
        conflicts_with = "profile"
    )]
    pub credential_ref: Option<String>,
}

impl WorkcellSelectorArgs {
    pub fn is_set(&self) -> bool {
        self.sandbox.is_some() || self.is_direct()
    }

    /// A selector that connects to a Workcell endpoint without a managed
    /// sandbox in between.
    pub fn is_direct(&self) -> bool {
        self.profile.is_some()
            || self.endpoint.is_some()
            || self.cwd.is_some()
            || self.credential_ref.is_some()
    }

    /// Refuses a selector whose experiment is off, before anything reads its
    /// profiles, credentials, or network.
    pub fn require_features(&self, features: FeatureFlags) -> Result<(), FeatureDisabled> {
        if self.sandbox.is_some() || self.sandbox_resume {
            features.require(Feature::Sandboxes)?;
        }
        if self.is_direct() {
            features.require(Feature::RemoteWorkcell)?;
        }
        Ok(())
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Manage saved sandbox instances (daemon setup remains an operator action)
    Sandbox {
        #[command(subcommand)]
        action: SandboxAction,
    },
    /// Manage API authentication
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// List all available models
    Models {
        /// Show the effective model for every model job
        #[arg(long)]
        jobs: bool,
    },
    /// Run the index tool on a file to see how it looks like
    Index { path: String },
    /// Inspect or recover the selected remote workspace without running a model
    Remote {
        /// status, pending, reconnect, reconcile, or acknowledge ID --accept-possible-effects
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Manage MCP server authentication
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// Update caudra to the latest version
    Update {
        /// Skip confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
        /// Disable syntax highlighting
        #[arg(long)]
        no_color: bool,
    },
    /// Rollback to the previous version
    Rollback,
    /// Run as an ACP (Agent Client Protocol) server over stdio
    Acp {
        /// Model spec (provider/model-id)
        #[arg(short, long)]
        model: Option<String>,
    },
    /// Show the rendered system prompt or tool definitions
    Prompt {
        /// Prompt variant: system (default), research, general
        #[arg(value_enum, default_value_t = PromptVariant::System)]
        variant: PromptVariant,
        /// Append the plan mode reminder to the system prompt
        #[arg(long)]
        plan: bool,
        /// Show tool definitions (JSON) instead of prompt text
        #[arg(long)]
        tools: bool,
        /// With --tools: show only tool names, one per line
        #[arg(long, requires = "tools")]
        names: bool,
    },
    /// List every tool with the config and CLI rules applied
    ///
    /// Unrelated to the `--tools` compatibility flag, which is ignored.
    Tools {
        /// Omit the tools that are turned off
        #[arg(long)]
        enabled_only: bool,
        /// Full records as JSON
        #[arg(long, conflicts_with_all = ["names", "schemas"])]
        json: bool,
        /// Tool names only, one per line
        #[arg(long, conflicts_with_all = ["json", "schemas"])]
        names: bool,
        /// The tool definitions as the provider receives them
        #[arg(long, conflicts_with_all = ["json", "names"])]
        schemas: bool,
    },
    /// List every skill with the directory precedence applied
    Skills {
        /// Print one skill's body, exactly as the model receives it
        #[arg(value_name = "NAME", conflicts_with_all = ["names", "json", "dirs"])]
        name: Option<String>,
        /// Skill names only, one per line
        #[arg(long, conflicts_with_all = ["json", "dirs"])]
        names: bool,
        /// Full records as JSON
        #[arg(long, conflicts_with_all = ["names", "dirs"])]
        json: bool,
        /// Every candidate directory: selected, superseded, or missing
        #[arg(long, conflicts_with_all = ["names", "json"])]
        dirs: bool,
    },
    /// Show config files and the settings they take
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Print the structured log
    Logs {
        /// Keep printing new records as they arrive
        #[arg(short, long)]
        follow: bool,
        /// Hide records below this level
        #[arg(short, long, value_name = "LEVEL", default_value = "info")]
        level: LogLevel,
        /// How many records to print before following
        #[arg(short = 'n', long, value_name = "COUNT", default_value_t = DEFAULT_LOG_LINES)]
        lines: usize,
        /// Emit the records as they are stored, one JSON object per line
        #[arg(long)]
        json: bool,
    },
    /// Inspect and maintain session storage
    Storage {
        #[command(subcommand)]
        action: StorageAction,
    },
    #[command(about = "Inspect and export the opt-in decision-engine log")]
    Decisions {
        #[command(subcommand)]
        action: DecisionAction,
    },
    #[command(
        about = "Inspect permissions and discover review-only patterns without starting an agent"
    )]
    Permissions {
        #[arg(
            long,
            global = true,
            value_name = "ABSOLUTE_CAUDRA_SQLITE",
            help = "Select an existing canonical caudra.sqlite path; default uses this build's data namespace. Not accepted by audit"
        )]
        database: Option<PathBuf>,
        #[command(subcommand)]
        action: PermissionAction,
    },
    /// Publish, broadcast, send, and read cross-session messages from scripts
    Message {
        #[command(subcommand)]
        action: MessageAction,
    },
}

impl Command {
    /// Commands that never read settings, so a broken global caudra.toml
    /// cannot stop an update, a rollback, a look at the logs, or the settings
    /// reference that helps fix it.
    pub fn runs_without_config(&self) -> bool {
        matches!(
            self,
            Self::Update { .. } | Self::Rollback | Self::Logs { .. } | Self::Config { .. }
        )
    }

    pub fn loads_settings(&self) -> bool {
        matches!(
            self,
            Self::Index { .. }
                | Self::Models { .. }
                | Self::Acp { .. }
                | Self::Prompt { .. }
                | Self::Tools { .. }
                | Self::Skills { .. }
                | Self::Storage { .. }
                | Self::Decisions { .. }
                | Self::Message { .. }
        )
    }
}

#[derive(Subcommand)]
pub enum MessageAction {
    /// Publish to every live session subscribed to a topic
    Publish {
        /// The exact topic, such as ci.failures
        #[arg(long, value_name = "TOPIC", value_parser = parse_topic)]
        topic: String,
        #[command(flatten)]
        message: MessageArgs,
    },
    /// Send to every live session that receives broadcasts
    Broadcast {
        #[command(flatten)]
        message: MessageArgs,
    },
    /// Send to the live session holding a unique messaging name
    Send {
        /// The messaging name, with or without its @
        #[arg(long, value_name = "NAME", value_parser = parse_handle_address)]
        to: String,
        #[command(flatten)]
        message: MessageArgs,
    },
    /// Print recorded messages, oldest first
    Log {
        /// Only topics this pattern matches, such as ci.*
        #[arg(long, value_name = "PATTERN", value_parser = parse_pattern, conflicts_with_all = ["broadcast", "with"])]
        topic: Option<String>,
        /// Only broadcasts
        #[arg(long, conflicts_with = "with")]
        broadcast: bool,
        /// Only direct messages to or from whoever held this messaging name
        #[arg(long, value_name = "NAME", value_parser = parse_handle_address)]
        with: Option<String>,
        /// How many of the newest messages to print
        #[arg(
            short = 'n',
            long,
            value_name = "COUNT",
            default_value_t = DEFAULT_MESSAGE_LIMIT,
            value_parser = value_parser!(u32).range(1..=MAX_MESSAGE_LIMIT)
        )]
        limit: u32,
        /// One JSON object per message, without session ids
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
pub struct MessageArgs {
    /// The sender name recipients see; one name shares one rate limit
    #[arg(long, value_name = "LABEL", default_value = DEFAULT_LABEL, value_parser = parse_label)]
    pub from: String,
    /// Print the receipt as JSON
    #[arg(long)]
    pub json: bool,
    /// The message text; read from stdin when omitted
    #[arg(value_name = "TEXT")]
    pub text: Option<String>,
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// List every file Caudra reads settings from, and which ones exist
    Files,
    /// Print every setting a file takes, commented out and set to its default
    Example {
        /// The file, by stem or by name, such as mcp or mcp.toml
        #[arg(
            value_name = "FILE",
            value_parser = example_file_parser(),
            default_value = files::CAUDRA.stem()
        )]
        file: &'static ConfigFile,
    },
}

/// The stems are the listed values, and the file names are aliases.
fn example_file_parser() -> impl TypedValueParser<Value = &'static ConfigFile> {
    PossibleValuesParser::new(
        files::examples().map(|file| PossibleValue::new(file.stem()).alias(file.name)),
    )
    .try_map(|name| files::find_example(&name).ok_or(NO_REFERENCE))
}

#[derive(Subcommand)]
pub enum DecisionAction {
    #[command(about = "Show decision-engine configuration without contacting the endpoint")]
    Status,
    #[command(about = "Show logged decision counts, latency and labelled agreement as JSON")]
    Stats {
        #[arg(long)]
        feature: Option<String>,
    },
    #[command(about = "Export labelled decisions as laya-evals JSONL to stdout")]
    Export {
        #[arg(long)]
        feature: Option<String>,
    },
    #[command(about = "Delete all locally logged decisions")]
    Purge {
        #[arg(long, required = true)]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub enum PermissionAction {
    #[command(
        about = "Propose command patterns from bounded history under declared standard-Bash assumptions; never installs rules"
    )]
    Discover {
        #[arg(
            long,
            value_name = "ABSOLUTE_PATH",
            help = "Match the stored current project cwd exactly (approximate historical context); defaults to the current directory, without resolving historical paths"
        )]
        project: Option<PathBuf>,
        #[arg(
            long,
            value_name = "COUNT",
            help = "Maximum proposals; default 10, clamped to 1–64"
        )]
        limit: Option<usize>,
        #[arg(
            long,
            value_name = "RFC3339",
            help = "Include history UUIDv7 creation times at or after this cutoff; not proof of execution time"
        )]
        since: Option<String>,
        #[arg(
            long,
            help = "Emit safe observed literal values, pattern definitions and evidence as JSON; omitted inputs are never printed"
        )]
        json: bool,
    },
    #[command(
        about = "Summarize a bounded read-only sample of permission log events without exposing log values"
    )]
    Audit {
        #[arg(
            long,
            value_name = "JSON_LOG",
            help = "Read this file instead of the current canonical log; rotated files are not scanned"
        )]
        log: Option<PathBuf>,
        #[arg(
            long,
            value_name = "RFC3339",
            help = "Include events at or after this timestamp within the bounded tail sample"
        )]
        since: Option<String>,
        #[arg(
            long,
            value_name = "BYTES",
            help = "Tail read budget, including boundary probe; default 8 MiB, clamped to 1 byte–32 MiB"
        )]
        max_bytes: Option<u64>,
    },
    #[command(
        about = "Review all persistent and conversation permission rules without changing storage"
    )]
    Inventory {
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long, value_name = "ABSOLUTE_PATH")]
        known_root: Vec<String>,
        #[arg(
            long,
            help = "Export raw structured records as JSON instead of a human-readable review"
        )]
        json: bool,
    },
    #[command(
        about = "Recover typed permission reviews from hash-verified history candidates; dry-run by default"
    )]
    RepairReview {
        #[arg(
            long,
            help = "Retry unavailable or incomplete recovered reviews; never replace approved reviews"
        )]
        retry_unavailable: bool,
        #[arg(
            long,
            help = "Back up and apply metadata-only repairs; stop all sessions and storage readers first"
        )]
        apply: bool,
        #[arg(
            long,
            help = "Print aggregate counts only as JSON, never history or candidate values"
        )]
        json: bool,
    },
    #[command(
        about = "Preview an explicit old-to-new permission transfer; never follows the old root's current symlink"
    )]
    Rebind {
        #[arg(long, value_name = "HISTORICAL_ABSOLUTE_PATH")]
        old_root: PathBuf,
        #[arg(long, value_name = "CANONICAL_DIRECTORY")]
        new_root: PathBuf,
        #[arg(
            long,
            value_name = "JSON_FILE",
            help = "Explicit hash candidates: {\"paths\":[\"/old/path\"],\"values\":[\"command text\"]}; never executed or persisted"
        )]
        candidates: Option<PathBuf>,
        #[arg(long, value_name = "FULL_RULE_ID")]
        select: Vec<String>,
        #[arg(long, requires_all = ["confirm", "select"], help = "Apply selected replacements with all sessions using this database stopped")]
        apply: bool,
        #[arg(long, requires = "apply", value_name = "PREVIEW_FINGERPRINT")]
        confirm: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum StorageAction {
    /// Print the session database path
    Path,
    /// Show aggregate database, row, and artifact statistics
    Stats {
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Show per-workspace file change record stores, largest first
    Snapshots {
        /// Emit JSON
        #[arg(long)]
        json: bool,
        /// Also list each holding session with its record count
        #[arg(long)]
        records: bool,
    },
    /// Check database and foreign-key integrity
    Check,
    /// Checkpoint the write-ahead log
    Checkpoint {
        /// Truncate the WAL after checkpointing
        #[arg(long)]
        truncate: bool,
    },
    /// Reclaim a bounded number of freelist pages
    Vacuum {
        #[arg(long, default_value_t = 1024)]
        pages: u32,
    },
    /// List sessions with their last activity, size, and retention state
    Sessions {
        /// Only sessions for this working directory
        #[arg(long, value_name = "DIR")]
        directory: Option<String>,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Demote sessions outside a keep policy, or the given session IDs, to the
    /// transcript tier
    ///
    /// Trimming releases the session's local file change records and removes
    /// retained tool output files, rewind archives, and large rich tool output
    /// records. The conversation stays and the session can still be resumed.
    /// Without any --keep-* flag the configured storage.retention.trim policy
    /// applies.
    Trim {
        /// Session IDs to trim regardless of policy
        #[arg(value_name = "ID", conflicts_with_all = ["directory", "group_by"])]
        ids: Vec<String>,
        #[command(flatten)]
        policy: KeepPolicyArgs,
        #[command(flatten)]
        scope: PolicyScopeArgs,
        /// Show the plan without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Emit the plan and outcomes as JSON
        #[arg(long)]
        json: bool,
    },
    /// Delete sessions outside a keep policy, or the given session IDs
    ///
    /// Without any --keep-* flag the configured storage.retention.forget
    /// policy applies. An empty policy is refused unless
    /// --unsafe-allow-remove-all is combined with --directory.
    Forget {
        /// Session IDs to delete regardless of policy
        #[arg(value_name = "ID", conflicts_with_all = ["directory", "group_by", "unsafe_allow_remove_all"])]
        ids: Vec<String>,
        #[command(flatten)]
        policy: KeepPolicyArgs,
        #[command(flatten)]
        scope: PolicyScopeArgs,
        /// Show the plan without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Emit the plan and outcomes as JSON
        #[arg(long)]
        json: bool,
        /// Run prune afterwards when at least one session was forgotten
        #[arg(long)]
        prune: bool,
    },
    /// Reclaim space no session references: cleanup jobs, orphaned artifact
    /// directories, the WAL, and freelist pages
    Prune {
        /// Show what would be reclaimed without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Report lifetime token spend, which outlives the sessions that produced it
    Usage {
        /// Only spend recorded within this duration, such as `30d`
        #[arg(long, value_name = "DURATION")]
        since: Option<RetentionDuration>,
        /// How to aggregate the rows
        #[arg(long, value_enum, default_value_t = UsageGrouping::Model)]
        group_by: UsageGrouping,
        /// Emit JSON
        #[arg(long)]
        json: bool,
        /// Delete recorded spend older than this duration instead of reporting
        #[arg(long, value_name = "DURATION", conflicts_with_all = ["since", "group_by"])]
        prune_older_than: Option<RetentionDuration>,
    },
    /// Keep sessions regardless of any policy
    Pin {
        #[arg(value_name = "ID", required = true)]
        ids: Vec<String>,
    },
    /// Make sessions subject to policies again
    Unpin {
        #[arg(value_name = "ID", required = true)]
        ids: Vec<String>,
    },
}

/// How `storage usage` folds the hourly ledger rows together.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageGrouping {
    Model,
    Provider,
    Project,
    /// Chat against what Caudra spent on its own: goals, compaction, titles.
    Purpose,
    Day,
    Month,
    Total,
}

/// Which sessions to keep, in `restic forget` terms. A session is kept when
/// it matches at least one rule. Durations are like `90d` or `2y5m7d3h`.
#[derive(Args, Debug, Default, Clone)]
pub struct KeepPolicyArgs {
    /// Keep the N most recently active sessions
    #[arg(long, value_name = "N")]
    pub keep_last: Option<u32>,
    /// For the last N hours with sessions, keep the newest session of each
    #[arg(long, value_name = "N")]
    pub keep_hourly: Option<u32>,
    /// For the last N days with sessions, keep the newest session of each
    #[arg(long, value_name = "N")]
    pub keep_daily: Option<u32>,
    /// For the last N ISO weeks with sessions, keep the newest session of each
    #[arg(long, value_name = "N")]
    pub keep_weekly: Option<u32>,
    /// For the last N months with sessions, keep the newest session of each
    #[arg(long, value_name = "N")]
    pub keep_monthly: Option<u32>,
    /// For the last N years with sessions, keep the newest session of each
    #[arg(long, value_name = "N")]
    pub keep_yearly: Option<u32>,
    /// Keep every session active within this duration
    #[arg(long, value_name = "DURATION")]
    pub keep_within: Option<RetentionDuration>,
    /// Keep hourly sessions active within this duration
    #[arg(long, value_name = "DURATION")]
    pub keep_within_hourly: Option<RetentionDuration>,
    /// Keep daily sessions active within this duration
    #[arg(long, value_name = "DURATION")]
    pub keep_within_daily: Option<RetentionDuration>,
    /// Keep weekly sessions active within this duration
    #[arg(long, value_name = "DURATION")]
    pub keep_within_weekly: Option<RetentionDuration>,
    /// Keep monthly sessions active within this duration
    #[arg(long, value_name = "DURATION")]
    pub keep_within_monthly: Option<RetentionDuration>,
    /// Keep yearly sessions active within this duration
    #[arg(long, value_name = "DURATION")]
    pub keep_within_yearly: Option<RetentionDuration>,
}

impl KeepPolicyArgs {
    /// `None` when no flag was given, so the configured policy applies.
    pub fn policy(&self) -> Option<KeepPolicy> {
        let policy = KeepPolicy {
            keep_last: self.keep_last,
            keep_hourly: self.keep_hourly,
            keep_daily: self.keep_daily,
            keep_weekly: self.keep_weekly,
            keep_monthly: self.keep_monthly,
            keep_yearly: self.keep_yearly,
            keep_within: self.keep_within,
            keep_within_hourly: self.keep_within_hourly,
            keep_within_daily: self.keep_within_daily,
            keep_within_weekly: self.keep_within_weekly,
            keep_within_monthly: self.keep_within_monthly,
            keep_within_yearly: self.keep_within_yearly,
        };
        (policy != KeepPolicy::default()).then_some(policy)
    }
}

#[derive(Args, Debug, Default, Clone)]
pub struct PolicyScopeArgs {
    /// Evaluate the policy per working directory, or across every session
    #[arg(long, value_name = "directory|none")]
    pub group_by: Option<GroupBy>,
    /// Only sessions for this working directory
    #[arg(long, value_name = "DIR")]
    pub directory: Option<String>,
    /// Allow an empty policy to act on every session matched by --directory
    #[arg(long, requires = "directory")]
    pub unsafe_allow_remove_all: bool,
}

#[derive(Subcommand)]
pub enum McpAction {
    /// Authenticate with an MCP server
    Auth {
        /// Server name from config
        server: String,
    },
    /// Remove stored OAuth credentials for an MCP server
    Logout {
        /// Server name from config
        server: String,
    },
}

#[derive(Subcommand)]
pub enum AuthAction {
    /// Manage purpose-scoped sandbox lifecycle API keys (never Workcell traffic tokens)
    Sandbox {
        #[command(subcommand)]
        action: SandboxAuthAction,
    },
    /// Authenticate with a provider (interactive if no provider specified)
    Login {
        /// Provider slug (e.g. zai, openai, xai). Omit for interactive selection.
        provider: Option<String>,
        /// Authentication method for Anthropic or OpenAI
        #[arg(long, value_enum, requires = "provider")]
        method: Option<AuthMethod>,
    },
    /// Remove stored credentials for a provider
    Logout {
        /// Provider slug (e.g. openai, xai)
        provider: String,
    },
    /// Show authentication status for all providers
    Status,
    /// Manage named Workcell bearer credentials in owner-only local auth state
    Workcell {
        #[command(subcommand)]
        action: WorkcellAuthAction,
    },
}

#[derive(Subcommand)]
pub enum WorkcellAuthAction {
    /// Store or replace a named bearer credential (not OS-keyring encrypted)
    Set {
        /// Credential name referenced as credential:NAME
        name: WorkcellCredentialName,
        /// Read the bearer token from stdin instead of a hidden terminal prompt
        #[arg(long)]
        stdin: bool,
    },
    /// List credential names and update times without reading bearer values
    List,
    /// Delete a named bearer credential
    Delete {
        /// Credential name
        name: WorkcellCredentialName,
    },
}

#[derive(Subcommand)]
pub enum SandboxAuthAction {
    /// Generate and save a 256-bit API namespace key; emits no secret
    Generate {
        name: WorkcellCredentialName,
    },
    /// Store a lifecycle key from a hidden prompt or bounded stdin
    Set {
        name: WorkcellCredentialName,
        #[arg(long)]
        stdin: bool,
    },
    /// List names without printing key values
    List,
    Delete {
        name: WorkcellCredentialName,
    },
}

#[derive(Subcommand)]
pub enum SandboxAction {
    /// Reviewed file transfer. Emits JSON lines; never copies on attach or without confirmation.
    Transfer(SandboxTransferArgs),
    /// Read daemon capabilities and immutable catalog; does not start or install anything
    Doctor {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        local: bool,
    },
    /// Explicitly allocate from a saved profile, then verify live Workcell identity
    Create {
        name: String,
        #[arg(long, id = "launch_profile")]
        profile: String,
    },
    /// Show saved records, or live owner-scoped instances for an explicit provider
    List {
        #[arg(long)]
        provider: Option<String>,
    },
    /// Reconcile by operation lookup, never replay an unknown create
    #[command(alias = "reconcile")]
    Inspect {
        name: String,
    },
    /// Explicitly acknowledge an unresolved lifecycle failure; never claims success or retries
    AcknowledgeFailure {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    /// Verify a saved sandbox, or explicitly save a borrowed provider instance
    Attach {
        name: String,
        #[arg(long, requires = "instance")]
        provider: Option<String>,
        #[arg(long, requires = "provider")]
        instance: Option<String>,
        #[arg(long, id = "attach_cwd", default_value = ".")]
        cwd: String,
    },
    Resume {
        name: String,
        /// Running lease; 0 runs until paused or deleted (the daemon must allow it)
        #[arg(long)]
        lease_seconds: LeaseSeconds,
        #[arg(long)]
        yes: bool,
    },
    Pause {
        name: String,
    },
    /// Lengthen the running lease; 0 runs until paused or deleted. Never shortens a lease
    Extend {
        name: String,
        /// Running lease from now; 0 runs until paused or deleted (the daemon must allow it)
        #[arg(long)]
        lease_seconds: LeaseSeconds,
    },
    /// Delete owned disks only with --yes; borrowed records detach unless --destroy-borrowed
    Delete {
        name: String,
        #[arg(long)]
        yes: bool,
        #[arg(long, requires = "yes")]
        destroy_borrowed: bool,
    },
    /// Detach the local record; never stop the VM or delete its disk
    Detach {
        name: String,
    },
    /// Explicitly cancel an in-progress create, not a running instance
    Cancel {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    /// Preview a strict policy JSON file; Test evaluates rules without DNS or a real probe
    Network {
        name: String,
        #[arg(long)]
        policy: PathBuf,
        #[arg(long)]
        test: Option<String>,
        #[arg(long, requires = "yes")]
        apply: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Preview an import/build/gc/inspect local-admin JSON request; --yes executes offline
    Images {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        request: PathBuf,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum SandboxTransferMode {
    Compare,
    Seed,
    Push,
    Pull,
    Reconcile,
}

#[derive(Args)]
pub struct SandboxTransferArgs {
    pub mode: SandboxTransferMode,
    pub name: String,
    #[arg(long)]
    pub local_root: PathBuf,
    /// Relative to the Workcell workspace root, independent of the agent cwd.
    #[arg(long)]
    pub remote_root: String,
    /// Exact relative file paths, repeated for each selection. No implicit select-all.
    #[arg(long = "select")]
    pub selected: Vec<String>,
    #[arg(long)]
    pub dry_run: bool,
    /// Headless replies must name the emitted request_id/plan_id; EOF always denies.
    #[arg(long)]
    pub json_input: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum AuthMethod {
    Oauth,
    ApiKey,
}

/// MCP names arrive already qualified (`server.tool`, `server.*`) and are
/// case-sensitive, so only bare built-in names get the PascalCase rewrite.
pub fn normalize_tool_name(name: &str) -> Result<String> {
    let result = if name.contains('.') {
        name.to_owned()
    } else {
        let mut result = String::with_capacity(name.len() + 4);
        for (i, c) in name.chars().enumerate() {
            if c.is_ascii_uppercase() {
                if i > 0 {
                    result.push('_');
                }
                result.push(c.to_ascii_lowercase());
            } else {
                result.push(c);
            }
        }
        result
    };
    if !is_disableable_tool(&result) {
        bail!(
            "unknown tool '{}'. Valid tools: {} (MCP tools use `server.tool` or `server.*`)",
            name,
            all_builtin_tool_names().join(", ")
        );
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use caudra_agent::peers::INVALID_HANDLE;
    use caudra_agent::peers::script::INVALID_LABEL;
    use caudra_agent::peers::topics::INVALID_PATTERN;
    use caudra_config::workcell::{
        WorkcellProfiles, WorkcellSelection, WorkcellSelectionError, select_workcell,
    };
    use test_case::test_case;

    const LOGS_NOT_PARSED: &str = "expected the logs subcommand";
    const CONFIG_EXAMPLE_NOT_PARSED: &str = "expected the config example subcommand";
    const MODELS_NOT_PARSED: &str = "expected the models subcommand";
    const TRIM_NOT_PARSED: &str = "expected the storage trim subcommand";
    const SNAPSHOTS_NOT_PARSED: &str = "expected the storage snapshots subcommand";
    const MODEL_SPEC: &str = "openai/gpt-5";
    const PERMISSIONS_NOT_PARSED: &str = "expected permission rebind subcommand";
    const PERMISSION_DATABASE: &str = "/explicit-copy/caudra.sqlite";
    const MESSAGE_NOT_PARSED: &str = "expected a message subcommand";
    const MESSAGING_NAME: &str = "ci-watcher";
    const MESSAGE_TEXT: &str = "Nightly build failed";
    const SENDER_LABEL: &str = "nightly-ci";

    #[test_case(&["caudra", "--auto"]; "root_auto")]
    #[test_case(&["caudra", "acp", "--auto"]; "acp_auto")]
    #[test_case(&["caudra", "--auto", "acp"]; "auto_before_acp")]
    #[test_case(&["caudra", "-p", "--auto", "--prompt", "hello"]; "print_auto")]
    fn auto_flag_is_global(args: &[&str]) {
        let cli = Cli::try_parse_from(args).and_then(Cli::validate).unwrap();
        assert_eq!(cli.permission_mode_override(), Some(PermissionMode::Auto));
    }

    #[test_case(&["caudra", "--auto", "--yolo"]; "root_conflict")]
    #[test_case(&["caudra", "acp", "--auto", "--yolo"]; "acp_conflict")]
    #[test_case(&["caudra", "--auto", "acp", "--yolo"]; "cross_scope_conflict")]
    #[test_case(&["caudra", "--yolo", "acp", "--auto"]; "reverse_cross_scope_conflict")]
    #[test_case(&["caudra", "--auto", "acp", "--dangerously-skip-permissions"]; "cross_scope_alias_conflict")]
    #[test_case(&["caudra", "--auto", "--dangerously-skip-permissions"]; "alias_conflict")]
    fn auto_and_yolo_are_mutually_exclusive(args: &[&str]) {
        assert_eq!(
            Cli::try_parse_from(args)
                .and_then(Cli::validate)
                .err()
                .map(|error| error.kind()),
            Some(ErrorKind::ArgumentConflict)
        );
    }

    #[test_case("stats"; "stats")]
    #[test_case("export"; "export")]
    fn decision_log_filters_are_preserved(action: &str) {
        let cli = Cli::try_parse_from(["caudra", "decisions", action, "--feature", "permission"])
            .unwrap();
        assert!(matches!(cli.command, Some(Command::Decisions {
            action: DecisionAction::Stats { feature: Some(feature) }
                | DecisionAction::Export { feature: Some(feature) },
        }) if feature == "permission"));
    }

    #[test_case(false; "requires_confirmation")]
    #[test_case(true; "confirmed")]
    fn decision_log_purge_requires_confirmation(confirmed: bool) {
        let mut args = vec!["caudra", "decisions", "purge"];
        if confirmed {
            args.push("--yes");
        }
        assert_eq!(Cli::try_parse_from(args).is_ok(), confirmed);
    }

    #[test_case(&[], false; "default")]
    #[test_case(&["--no-snapshots"], true; "tui")]
    #[test_case(&["--print", "--no-snapshots"], true; "print")]
    #[test_case(&["--print", "--input-format", "stream-json", "--no-snapshots"], true; "sdk")]
    #[test_case(&["--no-snapshots", "acp"], true; "before_acp")]
    #[test_case(&["acp", "--no-snapshots"], true; "after_acp")]
    #[test_case(&["--workcell-profile", "dev", "--no-snapshots"], true; "remote")]
    fn no_snapshots_is_global(args: &[&str], expected: bool) {
        let cli = Cli::try_parse_from(["caudra"].into_iter().chain(args.iter().copied())).unwrap();
        assert_eq!(cli.no_snapshots, expected);
    }

    #[test_case(false; "human_proposals")]
    #[test_case(true; "json_proposals")]
    fn permission_discover_parses_read_only_project_and_sample_options(json: bool) {
        let mut args = vec![
            "caudra",
            "permissions",
            "discover",
            "--project",
            "/historical/project",
            "--limit",
            "3",
            "--since",
            "2026-01-01T00:00:00Z",
            "--database",
            PERMISSION_DATABASE,
        ];
        if json {
            args.push("--json");
        }
        let cli = Cli::try_parse_from(args).unwrap();
        assert!(matches!(cli.command, Some(Command::Permissions {
            database: Some(path), action: PermissionAction::Discover { project: Some(_), limit: Some(3), since: Some(_), json: actual }
        }) if actual == json && path.to_str() == Some(PERMISSION_DATABASE)));
    }

    #[test_case("--apply"; "no_apply_mode")]
    #[test_case("--install"; "no_automatic_installation")]
    fn permission_discovery_cannot_install_rules(flag: &str) {
        assert!(Cli::try_parse_from(["caudra", "permissions", "discover", flag]).is_err());
    }

    #[test_case(false; "before_subcommand")]
    #[test_case(true; "after_subcommand")]
    fn permission_database_selection_is_global_to_permission_subcommands(after: bool) {
        let args = if after {
            vec![
                "caudra",
                "permissions",
                "repair-review",
                "--retry-unavailable",
                "--database",
                PERMISSION_DATABASE,
            ]
        } else {
            vec![
                "caudra",
                "permissions",
                "--database",
                PERMISSION_DATABASE,
                "repair-review",
                "--retry-unavailable",
            ]
        };
        let cli = Cli::try_parse_from(args).unwrap();
        assert!(matches!(cli.command, Some(Command::Permissions {
            database: Some(path), action: PermissionAction::RepairReview { retry_unavailable: true, apply: false, .. }
        }) if path.to_str() == Some(PERMISSION_DATABASE)));
    }

    #[test_case(false; "human_by_default")]
    #[test_case(true; "json_export")]
    fn permission_inventory_output_mode(json: bool) {
        let mut args = vec!["caudra", "permissions", "inventory"];
        if json {
            args.push("--json");
        }
        let cli = Cli::try_parse_from(args).unwrap();
        assert!(
            matches!(cli.command, Some(Command::Permissions { action: PermissionAction::Inventory { json: actual, .. }, .. }) if actual == json)
        );
    }

    #[test_case(false; "dry_run_by_default")]
    #[test_case(true; "explicit_apply")]
    fn permission_review_repair_requires_explicit_apply(apply: bool) {
        let mut args = vec!["caudra", "permissions", "repair-review", "--json"];
        if apply {
            args.push("--apply");
        }
        let cli = Cli::try_parse_from(args).unwrap();
        assert!(
            matches!(cli.command, Some(Command::Permissions { action: PermissionAction::RepairReview { apply: actual, json: true, retry_unavailable: false }, .. }) if actual == apply)
        );
    }

    #[test]
    fn permission_audit_parses_bounded_read_only_options() {
        let cli = Cli::try_parse_from([
            "caudra",
            "permissions",
            "audit",
            "--log",
            "sample.log",
            "--since",
            "2026-01-01T00:00:00Z",
            "--max-bytes",
            "1024",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Permissions {
                action: PermissionAction::Audit {
                    log: Some(_),
                    since: Some(_),
                    max_bytes: Some(1024)
                },
                ..
            })
        ));
        assert!(Cli::try_parse_from(["caudra", "permissions", "audit", "--apply"]).is_err());
    }

    #[test]
    fn permission_rebind_defaults_to_preview() {
        let cli = Cli::try_parse_from([
            "caudra",
            "permissions",
            "rebind",
            "--old-root",
            "/old",
            "--new-root",
            "/new",
        ])
        .unwrap();
        let Some(Command::Permissions {
            action:
                PermissionAction::Rebind {
                    apply,
                    confirm,
                    select,
                    ..
                },
            ..
        }) = cli.command
        else {
            panic!("{PERMISSIONS_NOT_PARSED}");
        };
        assert!(!apply);
        assert!(confirm.is_none());
        assert!(select.is_empty());
    }

    #[test_case(vec!["--apply"]; "apply_without_selection_and_confirmation")]
    #[test_case(vec!["--apply", "--select", "rule"]; "apply_without_confirmation")]
    #[test_case(vec!["--apply", "--confirm", "fingerprint"]; "apply_without_selection")]
    #[test_case(vec!["--confirm", "fingerprint"]; "confirmation_without_apply")]
    fn permission_rebind_apply_requires_explicit_review(extra: Vec<&str>) {
        let mut args = vec![
            "caudra",
            "permissions",
            "rebind",
            "--old-root",
            "/old",
            "--new-root",
            "/new",
        ];
        args.extend(extra);
        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test_case("FileRead", "file_read")]
    #[test_case("Shell", "shell")]
    #[test_case("PythonExecution", "python_execution")]
    #[test_case("ExecutionEnvironment", "execution_environment")]
    #[test_case("python_execution", "python_execution"; "snake_passthrough")]
    fn normalize_tool_name_valid_inputs(input: &str, expected: &str) {
        assert_eq!(normalize_tool_name(input).unwrap(), expected);
    }

    #[test]
    fn the_logs_subcommand_defaults_to_a_bounded_non_following_read() {
        let cli = Cli::try_parse_from(["caudra", "logs"]).unwrap();
        let Some(Command::Logs {
            follow,
            level,
            lines,
            json,
        }) = cli.command
        else {
            panic!("{LOGS_NOT_PARSED}");
        };
        assert!(!follow);
        assert!(!json);
        assert_eq!(lines, DEFAULT_LOG_LINES);
        assert!(matches!(level, LogLevel::Info));
    }

    #[test]
    fn the_logs_subcommand_takes_short_flags() {
        let cli = Cli::try_parse_from(["caudra", "logs", "-f", "-l", "warn", "-n", "10"]).unwrap();
        let Some(Command::Logs {
            follow,
            level,
            lines,
            ..
        }) = cli.command
        else {
            panic!("{LOGS_NOT_PARSED}");
        };
        assert!(follow);
        assert_eq!(lines, 10);
        assert!(matches!(level, LogLevel::Warn));
    }

    #[test]
    fn the_logs_subcommand_rejects_an_unknown_level() {
        assert!(Cli::try_parse_from(["caudra", "logs", "--level", "chatty"]).is_err());
    }

    #[test_case(&["caudra", "config", "example"], true ; "config_example")]
    #[test_case(&["caudra", "config", "files"], true ; "config_files")]
    #[test_case(&["caudra", "logs"], true ; "logs")]
    #[test_case(&["caudra", "rollback"], true ; "rollback")]
    #[test_case(&["caudra", "models"], false ; "models")]
    #[test_case(&["caudra", "tools"], false ; "tools")]
    fn only_settings_free_commands_run_without_config(args: &[&str], expected: bool) {
        let cli = Cli::try_parse_from(args).unwrap();
        assert_eq!(
            cli.command.as_ref().map(Command::runs_without_config),
            Some(expected)
        );
    }

    #[test_case(&[], files::CAUDRA.name ; "caudra_toml_by_default")]
    #[test_case(&["mcp"], files::MCP.name ; "stem")]
    #[test_case(&["mcp.toml"], files::MCP.name ; "file_name")]
    #[test_case(&["sandboxes"], files::SANDBOXES.name ; "experimental_file")]
    fn config_example_takes_a_file_by_stem_or_name(extra: &[&str], expected: &str) {
        let args = ["caudra", "config", "example"].iter().chain(extra);
        let Some(Command::Config {
            action: ConfigAction::Example { file },
        }) = Cli::try_parse_from(args).unwrap().command
        else {
            panic!("{CONFIG_EXAMPLE_NOT_PARSED}");
        };
        assert_eq!(file.name, expected);
    }

    #[test_case("init.lua" ; "file_without_a_reference")]
    #[test_case("settings" ; "unknown_name")]
    fn config_example_rejects_other_files(file: &str) {
        let error = Cli::try_parse_from(["caudra", "config", "example", file])
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
    }

    #[test_case(&["caudra", "models"], false, None ; "plain_listing")]
    #[test_case(&["caudra", "models", "--jobs"], true, None ; "jobs")]
    #[test_case(
        &["caudra", "--model", MODEL_SPEC, "models", "--jobs"],
        true,
        Some(MODEL_SPEC)
        ; "global_model_before_subcommand"
    )]
    #[test_case(
        &["caudra", "models", "--jobs", "-m", MODEL_SPEC],
        true,
        Some(MODEL_SPEC)
        ; "global_model_after_subcommand"
    )]
    fn models_modes_use_the_global_model_option(
        args: &[&str],
        expected_jobs: bool,
        expected_model: Option<&str>,
    ) {
        let cli = Cli::try_parse_from(args).unwrap();
        let Some(Command::Models { jobs }) = cli.command else {
            panic!("{MODELS_NOT_PARSED}");
        };

        assert_eq!(jobs, expected_jobs);
        assert_eq!(cli.model.as_deref(), expected_model);
    }

    #[test]
    fn normalize_tool_name_rejects_unknown() {
        let result = normalize_tool_name("NonExistentTool");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown tool"));
    }

    #[test]
    fn normalize_tool_name_rejects_removed_lua_tool() {
        assert!(normalize_tool_name("MultiEdit").is_err());
    }

    #[test]
    fn system_prompt_profile_parses_and_conflicts_with_raw_override() {
        let cli = Cli::try_parse_from(["caudra", "--system-prompt-profile", "review"]).unwrap();
        assert_eq!(cli.system_prompt_profile.as_deref(), Some("review"));
        assert!(
            Cli::try_parse_from([
                "caudra",
                "--system-prompt-profile",
                "review",
                "--system-prompt",
                "raw",
            ])
            .is_err()
        );
    }

    #[test_case("ci-watcher", true; "lowercase_words")]
    #[test_case("9lives", true; "leading_digit")]
    #[test_case("CI-watcher", false; "uppercase")]
    #[test_case("-watcher", false; "leading_hyphen")]
    #[test_case("@ci-watcher", false; "address_prefix")]
    fn messaging_name_is_validated_while_parsing(name: &str, valid: bool) {
        let flag = format!("--name={name}");
        match Cli::try_parse_from(["caudra", flag.as_str()]) {
            Ok(cli) => {
                assert!(valid);
                assert_eq!(cli.name.as_deref(), Some(name));
            }
            Err(error) => {
                assert!(!valid);
                assert!(error.to_string().contains(INVALID_HANDLE), "{error}");
            }
        }
    }

    #[test_case(&["--name", "ci-watcher"]; "name")]
    #[test_case(&["--topic", "ci.*"]; "topic")]
    #[test_case(&["--receive-broadcasts"]; "broadcasts")]
    fn messaging_flags_belong_to_interactive_sessions(flags: &[&str]) {
        let args = ["caudra"].iter().chain(flags).chain(&["--print"]);
        assert_eq!(
            Cli::try_parse_from(args).err().map(|error| error.kind()),
            Some(ErrorKind::ArgumentConflict)
        );
    }

    #[test_case(&["--topic=ci.*", "--topic=deploy.**"], Some(&["ci.*", "deploy.**"][..]); "repeated_patterns")]
    #[test_case(&["--topic=CI"], None; "invalid_pattern")]
    #[test_case(&["--topic=ci.**.failures"], None; "inner_tail_wildcard")]
    fn topic_patterns_are_validated_while_parsing(flags: &[&str], expected: Option<&[&str]>) {
        match Cli::try_parse_from(["caudra"].iter().chain(flags)) {
            Ok(cli) => assert_eq!(
                Some(cli.topics),
                expected.map(|topics| topics.iter().map(|topic| (*topic).to_owned()).collect())
            ),
            Err(error) => {
                assert!(expected.is_none());
                assert!(error.to_string().contains(INVALID_PATTERN), "{error}");
            }
        }
    }

    #[test_case(FeatureFlags::NONE, true; "experiment_off")]
    #[test_case(FeatureFlags::NONE.with(Feature::CrossSessionMessaging), false; "experiment_on")]
    fn message_command_is_hidden_until_enabled(features: FeatureFlags, hidden: bool) {
        let command = Cli::command_for(features);
        assert_eq!(
            command
                .find_subcommand("message")
                .map(ClapCommand::is_hide_set),
            Some(hidden)
        );
    }

    #[test_case(&["--topic", "ci.failures"], true; "concrete_topic")]
    #[test_case(&["--topic", "ci.*"], false; "wildcard")]
    #[test_case(&[], false; "no_topic")]
    fn message_publish_needs_one_concrete_topic(flags: &[&str], valid: bool) {
        let args = ["caudra", "message", "publish"]
            .iter()
            .chain(flags)
            .chain(&[MESSAGE_TEXT]);
        assert_eq!(Cli::try_parse_from(args).is_ok(), valid);
    }

    #[test_case(MESSAGING_NAME; "bare_name")]
    #[test_case("@ci-watcher"; "address")]
    fn message_send_takes_a_name_with_or_without_its_prefix(to: &str) {
        let args = ["caudra", "message", "send", "--to", to, MESSAGE_TEXT];
        let Some(Command::Message {
            action: MessageAction::Send { to, message },
        }) = Cli::try_parse_from(args).unwrap().command
        else {
            panic!("{MESSAGE_NOT_PARSED}");
        };
        assert_eq!(to, MESSAGING_NAME);
        assert_eq!(message.from, DEFAULT_LABEL);
        assert_eq!(message.text.as_deref(), Some(MESSAGE_TEXT));
    }

    #[test_case(SENDER_LABEL, true; "plain_label")]
    #[test_case("", false; "empty_label")]
    #[test_case("nightly\u{1b}ci", false; "control_character")]
    fn message_sender_label_is_validated_while_parsing(label: &str, valid: bool) {
        let flag = format!("--from={label}");
        match Cli::try_parse_from(["caudra", "message", "broadcast", flag.as_str()]) {
            Ok(Cli {
                command:
                    Some(Command::Message {
                        action: MessageAction::Broadcast { message },
                    }),
                ..
            }) => {
                assert!(valid);
                assert_eq!(message.from, label);
                assert!(message.text.is_none());
            }
            Ok(_) => panic!("{MESSAGE_NOT_PARSED}"),
            Err(error) => {
                assert!(!valid);
                assert!(error.to_string().contains(INVALID_LABEL), "{error}");
            }
        }
    }

    #[test_case(&["--topic", "ci.*", "--broadcast"]; "topic_and_broadcasts")]
    #[test_case(&["--topic", "ci.*", "--with", MESSAGING_NAME]; "topic_and_name")]
    #[test_case(&["--broadcast", "--with", MESSAGING_NAME]; "broadcasts_and_name")]
    fn message_log_takes_one_filter(flags: &[&str]) {
        let args = ["caudra", "message", "log"].iter().chain(flags);
        assert_eq!(
            Cli::try_parse_from(args).err().map(|error| error.kind()),
            Some(ErrorKind::ArgumentConflict)
        );
    }

    #[test_case(&[], Some(DEFAULT_MESSAGE_LIMIT); "default_limit")]
    #[test_case(&["-n", "1000"], Some(1000); "largest_limit")]
    #[test_case(&["-n", "0"], None; "zero")]
    #[test_case(&["--limit", "1001"], None; "over_the_limit")]
    fn message_log_limit_is_bounded(flags: &[&str], expected: Option<u32>) {
        let args = ["caudra", "message", "log"].iter().chain(flags);
        let limit = Cli::try_parse_from(args).ok().map(|cli| match cli.command {
            Some(Command::Message {
                action: MessageAction::Log { limit, .. },
            }) => limit,
            _ => panic!("{MESSAGE_NOT_PARSED}"),
        });
        assert_eq!(limit, expected);
    }

    /// Naming a session is how its change records get released by hand, so
    /// the IDs must reach `trim` and must not be silently mixed with a scope
    /// that selects a different set of sessions.
    #[test]
    fn trim_accepts_session_ids_and_rejects_a_conflicting_scope() {
        const SESSION_ID: &str = "CessP4gmzDyKuw7PHSTkd";

        let cli = Cli::try_parse_from(["caudra", "storage", "trim", SESSION_ID]).unwrap();
        let Some(Command::Storage {
            action: StorageAction::Trim { ids, .. },
        }) = cli.command
        else {
            panic!("{TRIM_NOT_PARSED}");
        };
        assert_eq!(ids, vec![SESSION_ID.to_owned()]);
        assert!(
            Cli::try_parse_from([
                "caudra",
                "storage",
                "trim",
                SESSION_ID,
                "--directory",
                "/tmp"
            ])
            .is_err()
        );
    }

    #[test_case(&[], false; "stores_only")]
    #[test_case(&["--records"], true; "with_holders")]
    fn storage_snapshots_lists_holders_only_when_asked(args: &[&str], expected: bool) {
        let cli = Cli::try_parse_from(
            ["caudra", "storage", "snapshots"]
                .into_iter()
                .chain(args.iter().copied()),
        )
        .unwrap();
        let Some(Command::Storage {
            action: StorageAction::Snapshots { records, .. },
        }) = cli.command
        else {
            panic!("{SNAPSHOTS_NOT_PARSED}");
        };
        assert_eq!(records, expected);
    }

    #[test]
    fn prompt_flag_parses() {
        let cli = Cli::try_parse_from(["caudra", "--prompt", "hello"]).unwrap();

        assert_eq!(cli.prompt.as_deref(), Some("hello"));
        assert!(!cli.print);
    }

    #[test]
    fn remote_control_preserves_explicit_acknowledgement() {
        let cli = Cli::try_parse_from([
            "caudra",
            "--workcell-profile",
            "test",
            "remote",
            "acknowledge",
            "operation",
            "--accept-possible-effects",
        ])
        .unwrap();
        let Some(Command::Remote { args }) = cli.command else {
            panic!("expected remote control");
        };
        assert!(matches!(
            caudra_workspace::WorkspaceControlCommand::parse(&args.join(" ")).unwrap(),
            caudra_workspace::WorkspaceControlCommand::Acknowledge(_)
        ));
    }

    #[test]
    fn prompt_accepts_leading_hyphen() {
        let cli = Cli::try_parse_from(["caudra", "--prompt", "-v is broken"]).unwrap();

        assert_eq!(cli.prompt.as_deref(), Some("-v is broken"));
    }

    #[test]
    fn print_short_flag_still_means_print() {
        let cli = Cli::try_parse_from(["caudra", "-p", "--prompt", "x"]).unwrap();

        assert!(cli.print);
        assert_eq!(cli.prompt.as_deref(), Some("x"));
    }

    #[test_case("fix the bug"; "quoted_sentence")]
    #[test_case("mdoels"; "mistyped_subcommand")]
    fn rejects_bare_positional(arg: &str) {
        let kind = Cli::try_parse_from(["caudra", arg]).err().map(|e| e.kind());

        assert_eq!(kind, Some(clap::error::ErrorKind::InvalidSubcommand));
    }

    #[test]
    fn rejects_positional_after_subcommand() {
        assert!(Cli::try_parse_from(["caudra", "models", "extra"]).is_err());
    }

    #[test]
    fn ephemeral_flag_parses() {
        let cli = Cli::try_parse_from(["caudra", "--ephemeral"]).unwrap();

        assert!(cli.ephemeral);
    }

    #[test]
    fn provider_auth_method_parses() {
        let cli = Cli::try_parse_from(["caudra", "auth", "login", "openai", "--method", "api-key"])
            .unwrap();

        assert!(matches!(
            cli.command,
            Some(Command::Auth {
                action: AuthAction::Login {
                    provider: Some(provider),
                    method: Some(AuthMethod::ApiKey),
                }
            }) if provider == "openai"
        ));
    }

    #[test]
    fn provider_auth_method_requires_provider() {
        assert!(Cli::try_parse_from(["caudra", "auth", "login", "--method", "oauth"]).is_err());
    }

    fn workcell_selection(cli: &Cli) -> Result<WorkcellSelection, WorkcellSelectionError> {
        select_workcell(
            &WorkcellProfiles::default(),
            cli.workcell.profile.as_deref(),
            cli.workcell.endpoint.as_deref(),
            cli.workcell.cwd.as_deref(),
            cli.workcell.credential_ref.as_deref(),
        )
    }

    #[test]
    fn no_workcell_selector_preserves_the_embedded_default() {
        let cli = Cli::try_parse_from(["caudra"]).unwrap();

        assert_eq!(workcell_selection(&cli), Ok(WorkcellSelection::Embedded));
    }

    #[test_case(&["caudra", "sandbox", "create", "dev", "--profile", "rust"])]
    #[test_case(&["caudra", "sandbox", "doctor", "--provider", "local", "--local"])]
    #[test_case(&["caudra", "sandbox", "attach", "dev", "--provider", "local", "--instance", "vm-id"])]
    #[test_case(&["caudra", "sandbox", "list"])]
    #[test_case(&["caudra", "sandbox", "inspect", "dev"])]
    #[test_case(&["caudra", "sandbox", "pause", "dev"])]
    #[test_case(&["caudra", "sandbox", "resume", "dev", "--lease-seconds", "300", "--yes"])]
    #[test_case(&["caudra", "sandbox", "extend", "dev", "--lease-seconds", "300"])]
    #[test_case(&["caudra", "sandbox", "delete", "dev", "--yes"])]
    #[test_case(&["caudra", "sandbox", "reconcile", "dev"])]
    #[test_case(&["caudra", "sandbox", "detach", "dev"])]
    #[test_case(&["caudra", "sandbox", "cancel", "dev", "--yes"])]
    #[test_case(&["caudra", "sandbox", "network", "dev", "--policy", "/policy.json", "--test", "example.test"])]
    #[test_case(&["caudra", "sandbox", "network", "dev", "--policy", "/policy.json", "--apply", "--yes"])]
    #[test_case(&["caudra", "sandbox", "images", "--provider", "local", "--request", "/admin.json"])]
    #[test_case(&["caudra", "auth", "sandbox", "generate", "local"])]
    #[test_case(&["caudra", "auth", "sandbox", "set", "local", "--stdin"])]
    #[test_case(&["caudra", "models", "--sandbox", "missing"])]
    fn sandbox_commands_parse_without_remote_profile_collisions(args: &[&str]) {
        assert!(Cli::try_parse_from(args).is_ok());
    }

    #[test_case(&["caudra", "sandbox", "create", "dev"])]
    #[test_case(&["caudra", "sandbox", "create", "--profile", "rust"])]
    #[test_case(&["caudra", "--sandbox", "dev", "--workcell-profile", "other"])]
    #[test_case(&["caudra", "--sandbox", "dev", "--workcell-endpoint", "http://127.0.0.1:8080", "--workcell-cwd", "."])]
    #[test_case(&["caudra", "auth", "sandbox", "set", "local", "secret"])]
    #[test_case(&["caudra", "sandbox", "network", "dev", "--policy", "/policy.json", "--apply"])]
    fn sandbox_commands_require_explicit_create_and_keep_secrets_out_of_argv(args: &[&str]) {
        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test]
    fn profile_and_direct_workcell_flags_conflict() {
        assert!(
            Cli::try_parse_from([
                "caudra",
                "--workcell-profile",
                "production",
                "--workcell-endpoint",
                "https://workcell.example",
                "--workcell-cwd",
                "project",
            ])
            .is_err()
        );
    }

    #[test_case(&["caudra", "--workcell-endpoint", "https://workcell.example"] ; "endpoint_only")]
    #[test_case(&["caudra", "--workcell-cwd", "project"] ; "cwd_only")]
    #[test_case(&["caudra", "--workcell-credential-ref", "credential:production"] ; "credential_only")]
    fn incomplete_direct_workcell_group_is_rejected(args: &[&str]) {
        assert!(Cli::try_parse_from(args).is_err());
    }

    #[test]
    fn direct_remote_workcell_requires_named_credentials() {
        let cli = Cli::try_parse_from([
            "caudra",
            "--workcell-endpoint",
            "https://workcell.example",
            "--workcell-cwd",
            "project",
        ])
        .unwrap();

        assert_eq!(
            workcell_selection(&cli),
            Err(WorkcellSelectionError::MissingRemoteCredential)
        );
    }

    #[test]
    fn direct_loopback_workcell_may_be_unauthenticated() {
        let cli = Cli::try_parse_from([
            "caudra",
            "--workcell-endpoint",
            "http://127.0.0.1:8080",
            "--workcell-cwd",
            "project",
        ])
        .unwrap();

        assert!(matches!(
            workcell_selection(&cli),
            Ok(WorkcellSelection::Remote(selection)) if selection.credential_ref.is_none()
        ));
    }

    #[test]
    fn direct_workcell_accepts_only_a_named_credential_reference() {
        let cli = Cli::try_parse_from([
            "caudra",
            "--workcell-endpoint",
            "https://workcell.example",
            "--workcell-cwd",
            "project",
            "--workcell-credential-ref",
            "credential:production",
        ])
        .unwrap();
        assert!(workcell_selection(&cli).is_ok());

        let environment_reference = Cli::try_parse_from([
            "caudra",
            "--workcell-endpoint",
            "https://workcell.example",
            "--workcell-cwd",
            "project",
            "--workcell-credential-ref",
            "env:WORKCELL_TOKEN",
        ])
        .unwrap();
        assert!(matches!(
            workcell_selection(&environment_reference),
            Err(WorkcellSelectionError::CredentialRef(_))
        ));
    }

    #[test]
    fn workcell_auth_set_accepts_no_bearer_value_argument() {
        let cli =
            Cli::try_parse_from(["caudra", "auth", "workcell", "set", "production", "--stdin"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Auth {
                action: AuthAction::Workcell {
                    action: WorkcellAuthAction::Set { stdin: true, .. }
                }
            })
        ));
        assert!(
            Cli::try_parse_from([
                "caudra",
                "auth",
                "workcell",
                "set",
                "production",
                "bearer-must-not-be-argv",
            ])
            .is_err()
        );
    }
}
