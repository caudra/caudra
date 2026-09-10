mod acp;
mod logs;
mod storage;
mod subcmd;
mod tui;

use std::path::Path;
use std::process::ExitCode;

use color_eyre::Result;
use color_eyre::eyre::Context;

use caudra_config::Config;
use caudra_storage::{EphemeralRoot, StateDir};

use crate::cli::{AuthAction, Cli, Command, McpAction, normalize_tool_name};
use crate::update;

const WORKCELL_CODE_WORKER_ENV: &str = "WORKCELL_MCP_CODE_WORKER";

fn run_storage(persistent: StateDir, ephemeral: bool) -> Result<(StateDir, Option<EphemeralRoot>)> {
    if !ephemeral {
        return Ok((persistent, None));
    }
    let (storage, root) =
        StateDir::activate_ephemeral(persistent).context("create ephemeral state directory")?;
    Ok((storage, Some(root)))
}

/// One choke point for every native tool, so a new entry point cannot boot
/// with half the built-ins missing. Workcell owns the file, web, shell, and
/// code contracts; `caudra_agent::tools::native` owns the rest.
fn register_builtin_tools(cwd: &Path) -> Result<caudra_workcell::WorkcellHost> {
    let registry = caudra_agent::tools::ToolRegistry::global();
    let worker = std::env::var_os(WORKCELL_CODE_WORKER_ENV).map(std::path::PathBuf::from);
    let host = caudra_workcell::WorkcellHost::new_production(cwd, worker.as_deref())
        .context("initialize native Workcell tools")?;
    host.register(registry)
        .context("register native Workcell tools")?;
    caudra_agent::tools::native::register(registry).context("register native Caudra tools")?;
    for warning in host.warnings() {
        eprintln!("warning: {warning}");
    }
    Ok(host)
}

/// Every entry point resolves config here, so the CLI tool flags cannot apply
/// in the TUI and silently go missing from `caudra tools`.
fn load_config(plugin_host: &caudra_lua::PluginHost, cli: &Cli, cwd: &Path) -> Result<Config> {
    let raw_config = plugin_host
        .load_init_files_or_skip(cli.no_plugins, cwd)
        .context("load init.lua files")?;

    let mut config = raw_config
        .unwrap_or_default()
        .into_config(cli.no_rtk)
        .context("invalid config")?;
    config.permissions = caudra_config::load_permissions(cwd);

    if cli.yolo || config.always_yolo {
        config.permissions.yolo = true;
    }
    if !cli.allowed_tools.is_empty() {
        config.agent.allowed_tools = cli
            .allowed_tools
            .iter()
            .map(|t| normalize_tool_name(t))
            .collect::<Result<Vec<_>>>()?;
    }
    if !cli.disallowed_tools.is_empty() {
        config.agent.disabled_tools.extend(
            cli.disallowed_tools
                .iter()
                .map(|t| normalize_tool_name(t))
                .collect::<Result<Vec<_>>>()?,
        );
    }
    config.validate()?;
    Ok(config)
}

/// Notes live outside the project, where every effectful tool would otherwise
/// prompt. Registered under a reserved owner so a plugin reload replaces these
/// rules rather than stacking duplicates.
fn install_native_permission_rules(
    plugin_rules: &caudra_agent::permissions::PluginRuleStore,
    cwd: &Path,
) {
    plugin_rules.replace(
        caudra_agent::tools::native::memory::RULE_OWNER,
        caudra_agent::tools::native::memory::permission_rules(cwd),
    );
}

/// Native tools read their options from here rather than from config
/// directly, so `caudra-agent` stays free of a config dependency it would
/// otherwise need only for two numbers.
///
/// The `caudra-plugin-dev` skill is rendered from the live Lua API docs, so
/// only `caudra-lua` can build it. Not installing it is how
/// `plugins.skill.plugin_dev = false` takes effect.
fn configure_native_tools(agent: &caudra_config::AgentConfig) {
    if agent.skill_plugin_dev {
        caudra_agent::tools::native::skill::install_builtin_skill(
            caudra_lua::docs_render::plugin_dev_skill(),
        );
    }
    if agent.skill_workflow_dev {
        caudra_agent::tools::native::skill::install_builtin_skill(
            caudra_agent::workflow::workflow_dev_skill(),
        );
    }
    caudra_agent::tools::native::task::set_max_concurrent(agent.task_max_concurrent);
}

pub fn dispatch(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Some(Command::Auth { action }) => {
            let storage = StateDir::resolve().context("resolve data directory")?;
            match action {
                AuthAction::Login { provider, method } => {
                    subcmd::auth_login(provider.as_deref(), method, &storage)?
                }
                AuthAction::Logout { provider } => subcmd::auth_logout(&provider, &storage)?,
                AuthAction::Status => subcmd::auth_status(&storage)?,
            }
        }
        Some(Command::Index { path }) => {
            subcmd::index(&path, cli.no_plugins, cli.no_jit)?;
        }
        Some(Command::Models) => subcmd::models(cli.no_plugins, cli.no_jit)?,
        Some(Command::Mcp { action }) => {
            let storage = StateDir::resolve().context("resolve data directory")?;
            match action {
                McpAction::Auth { server } => subcmd::mcp_auth(&server, &storage)?,
                McpAction::Logout { server } => subcmd::mcp_logout(&server, &storage)?,
            }
        }
        Some(Command::Update { yes, no_color }) => {
            update::update(yes, no_color).map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
        }
        Some(Command::Rollback) => {
            update::rollback().map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
        }
        Some(Command::Acp { model, yolo }) => {
            acp::run(
                model,
                yolo,
                cli.ephemeral,
                cli.no_plugins,
                cli.no_jit,
                cli.system_prompt_profile,
            )?;
        }
        Some(Command::Tools {
            enabled_only,
            json,
            names,
            schemas,
        }) => {
            subcmd::tools(&cli, enabled_only, json, names, schemas)?;
        }
        Some(Command::Skills {
            ref name,
            names,
            json,
            dirs,
        }) => {
            subcmd::skills(&cli, name.as_deref(), names, json, dirs)?;
        }
        Some(Command::Logs {
            follow,
            level,
            lines,
            json,
        }) => {
            logs::run(follow, level, lines, json)?;
        }
        Some(Command::Storage { action }) => {
            storage::run(action, cli.no_plugins, cli.no_jit)?;
        }
        Some(Command::Prompt {
            variant,
            plan,
            tools,
            names,
        }) => {
            subcmd::prompt(
                &variant,
                plan,
                tools,
                names,
                cli.no_plugins,
                cli.no_jit,
                cli.no_rtk,
                cli.model.as_deref(),
                cli.system_prompt_profile.as_deref(),
            )?;
        }
        None => return tui::run(cli),
    }
    Ok(ExitCode::SUCCESS)
}
