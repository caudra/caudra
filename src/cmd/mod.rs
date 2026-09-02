mod acp;
mod migrate;
mod storage;
mod subcmd;
mod tui;

use std::path::Path;

use color_eyre::Result;
use color_eyre::eyre::Context;

use caudra_storage::StateDir;

use crate::cli::{AuthAction, Cli, Command, McpAction, MigrateAction};
use crate::update;

const WORKCELL_CODE_WORKER_ENV: &str = "WORKCELL_MCP_CODE_WORKER";

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
        caudra_agent::tools::native::skill::set_builtin_skill(
            caudra_lua::docs_render::plugin_dev_skill(),
        );
    }
    caudra_agent::tools::native::task::set_max_concurrent(agent.task_max_concurrent);
}

pub fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Command::Auth { action }) => {
            let storage = StateDir::resolve().context("resolve data directory")?;
            match action {
                AuthAction::Login { provider } => {
                    subcmd::auth_login(provider.as_deref(), &storage)?
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
                cli.no_plugins,
                cli.no_jit,
                cli.system_prompt_profile,
            )?;
        }
        Some(Command::Migrate { action }) => match action {
            MigrateAction::Xdg => migrate::xdg()?,
        },
        Some(Command::Storage { action }) => storage::run(action)?,
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
        None => {
            tui::run(cli)?;
        }
    }
    Ok(())
}
