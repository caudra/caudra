use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use color_eyre::Result;
use color_eyre::eyre::bail;

use caudra_agent::tools::all_builtin_tool_names;
use caudra_config::is_disableable_tool;
use caudra_storage::retention::{Duration as RetentionDuration, GroupBy, KeepPolicy};

use crate::print::OutputFormat;

const DEFAULT_LOG_LINES: usize = 200;

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

    /// Non-interactive mode. Runs the prompt and exits. Compatible with Claude Code's --print flag
    #[arg(short, long)]
    pub print: bool,

    /// Initial message. Combined with piped stdin when both are present
    #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
    pub prompt: Option<String>,

    /// Store session data in a temporary directory removed when Caudra exits
    #[arg(long, global = true)]
    pub ephemeral: bool,

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

    /// Resume a specific session by its ID
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
    #[arg(long, alias = "dangerously-skip-permissions")]
    pub yolo: bool,

    /// Exit after the agent completes (for automation workflows)
    #[arg(long)]
    pub exit_on_done: bool,

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
}

impl Cli {
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
}

#[derive(Subcommand)]
pub enum Command {
    /// Manage API authentication
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// List all available models
    Models,
    /// Run the index tool on a file to see how it looks like
    Index { path: String },
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
        /// Skip all permission prompts
        #[arg(long)]
        yolo: bool,
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
    /// Show per-workspace working-tree snapshot stores, largest first
    Snapshots {
        /// Emit JSON
        #[arg(long)]
        json: bool,
        /// Also list the manifests each store holds
        #[arg(long)]
        manifests: bool,
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
    /// Trimming removes workspace snapshots, retained tool output files, rewind
    /// archives, and large rich tool output records. The conversation stays
    /// and the session can still be resumed. Without any --keep-* flag the
    /// configured storage.retention.trim policy applies.
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
    use test_case::test_case;

    const LOGS_NOT_PARSED: &str = "expected the logs subcommand";
    const TRIM_NOT_PARSED: &str = "expected the storage trim subcommand";

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

    /// Naming a session is how a snapshot store gets reclaimed by hand, so the
    /// IDs must reach `trim` and must not be silently mixed with a scope that
    /// selects a different set of sessions.
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

    #[test]
    fn prompt_flag_parses() {
        let cli = Cli::try_parse_from(["caudra", "--prompt", "hello"]).unwrap();

        assert_eq!(cli.prompt.as_deref(), Some("hello"));
        assert!(!cli.print);
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
}
