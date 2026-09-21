mod acp;
mod logs;
mod permissions;
mod sandbox;
mod sandbox_transfer;
mod storage;
mod subcmd;
mod tui;
mod workcell_runtime;

use std::env;
use std::path::Path;
use std::process::ExitCode;

use color_eyre::Result;
use color_eyre::eyre::Context;

use caudra_config::Config;
use caudra_storage::{EphemeralRoot, StateDir};

use crate::cli::{AuthAction, Cli, Command, McpAction, WorkcellAuthAction, normalize_tool_name};
use crate::update;

/// The three names Workcell's shell forwards from this process into every
/// command it runs. Windows reads the last two, Unix the first.
const TEMP_DIR_VARS: [&str; 3] = ["TMPDIR", "TMP", "TEMP"];

fn run_storage(persistent: StateDir, ephemeral: bool) -> Result<(StateDir, Option<EphemeralRoot>)> {
    if !ephemeral {
        return Ok((persistent, None));
    }
    let (storage, root) =
        StateDir::activate_ephemeral(persistent).context("create ephemeral state directory")?;
    Ok((storage, Some(root)))
}

/// Every entry point resolves config here, so the CLI tool flags cannot apply
/// in the TUI and silently go missing from `caudra tools`.
fn load_config(
    plugin_host: &caudra_lua::PluginHost,
    cli: &Cli,
    cwd: &Path,
    remote: bool,
) -> Result<Config> {
    let raw_config = if remote {
        plugin_host.load_global_init_file_or_skip(cli.no_plugins)
    } else {
        plugin_host.load_init_files_or_skip(cli.no_plugins, cwd)
    }
    .context("load init.lua files")?;

    let mut config = raw_config
        .unwrap_or_default()
        .into_config(cli.no_rtk)
        .context("invalid config")?;
    config.permissions = if remote {
        caudra_config::load_global_permissions()
    } else {
        caudra_config::load_permissions(cwd)
    };

    if cli.yolo || config.always_yolo {
        config.permissions.yolo = true;
    }
    if let Some(max) = cli.max_turns {
        config.agent.max_turns = Some(max);
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

/// Points the temp-directory variables at Caudra's own scratch directory, so a
/// command's `mktemp`, a build tool's cache, and everything else that honors
/// them land there instead of littering the shared temp root. Workcell's shell
/// clears the child environment and forwards exactly these three names from
/// this process, so writing them here is what reaches a tool call.
///
/// Resolving the scratch path first fixes the process temp root; the variables
/// are written only afterwards, so a later resolution cannot nest the namespace
/// inside itself. A failure leaves them untouched and scratch work returns to
/// the shared temp root, which the environment block still reports accurately.
/// No subscriber exists this early, so the warning is best-effort.
///
/// The directory is keyed to the project the user launched in and stays there
/// for the life of the process. `/cd` cannot move it, because writing the
/// environment once threads exist is exactly what makes `set_var` unsafe.
/// Permission policy answers that by pre-allowing the whole scratch root, so a
/// path handed to the model before a directory change stays writable after one.
///
/// Must run before any thread exists, which is why `dispatch` calls it first.
fn redirect_temp_dir() {
    let scratch = match env::current_dir()
        .and_then(|cwd| caudra_storage::projects::project_scratch_dir(&cwd))
    {
        Ok(scratch) => scratch,
        Err(error) => {
            tracing::warn!(%error, "scratch directory unavailable, leaving the temp directory alone");
            return;
        }
    };
    if env::temp_dir() == scratch {
        return;
    }
    for name in TEMP_DIR_VARS {
        // SAFETY: called before any thread is spawned, so nothing can be
        // reading the environment concurrently.
        unsafe { env::set_var(name, &scratch) };
    }
}

pub fn dispatch(mut cli: Cli) -> Result<ExitCode> {
    caudra_storage::paths::check_namespace_override()?;
    redirect_temp_dir();
    match cli.command.take() {
        Some(Command::Sandbox { action }) => {
            let storage = StateDir::resolve().context("resolve data directory")?;
            sandbox::run(action, &storage)?;
        }
        Some(Command::Permissions { action, database }) => {
            if cli.workcell.is_set() || cli.ephemeral {
                return Err(color_eyre::eyre::eyre!(
                    "permission administration requires local persistent storage"
                ));
            }
            permissions::run(action, database)?;
        }
        Some(Command::Auth { action }) => {
            let storage = StateDir::resolve().context("resolve data directory")?;
            match action {
                AuthAction::Sandbox { action } => sandbox::auth(action, &storage)?,
                AuthAction::Login { provider, method } => {
                    subcmd::auth_login(provider.as_deref(), method, &storage)?
                }
                AuthAction::Logout { provider } => subcmd::auth_logout(&provider, &storage)?,
                AuthAction::Status => subcmd::auth_status(&storage)?,
                AuthAction::Workcell { action } => match action {
                    WorkcellAuthAction::Set { name, stdin } => {
                        subcmd::workcell_credential_set(&name, stdin, &storage)?
                    }
                    WorkcellAuthAction::List => subcmd::workcell_credential_list(&storage)?,
                    WorkcellAuthAction::Delete { name } => {
                        subcmd::workcell_credential_delete(&name, &storage)?
                    }
                },
            }
        }
        Some(Command::Index { path }) => {
            subcmd::index(&cli, &path)?;
        }
        Some(Command::Remote { args }) => subcmd::remote_control(&cli, &args)?,
        Some(Command::Models { jobs }) => subcmd::models(&cli, jobs)?,
        Some(Command::Mcp { action }) => {
            let storage = StateDir::resolve().context("resolve data directory")?;
            let runtime = if cli.workcell.is_set() {
                let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                Some(workcell_runtime::WorkcellRuntime::initialize(
                    &cli.workcell,
                    &cwd,
                    &storage,
                    caudra_agent::tools::ToolRegistry::global(),
                )?)
            } else {
                None
            };
            match action {
                McpAction::Auth { server } => subcmd::mcp_auth(
                    &server,
                    &storage,
                    runtime.as_ref().is_some_and(|r| r.is_remote()),
                )?,
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
                model.as_deref(),
                yolo,
                cli.ephemeral,
                cli.no_plugins,
                cli.no_jit,
                cli.system_prompt_profile.as_deref(),
                &cli.workcell,
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
            if cli.workcell.is_set() {
                let storage = StateDir::resolve().context("resolve data directory")?;
                let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                let runtime = workcell_runtime::WorkcellRuntime::initialize(
                    &cli.workcell,
                    &cwd,
                    &storage,
                    caudra_agent::tools::ToolRegistry::global(),
                )?;
                if runtime.is_remote() {
                    return Err(color_eyre::eyre::eyre!(
                        "storage and snapshot commands are disabled for remote Workcell sessions"
                    ));
                }
            }
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
                &cli.workcell,
            )?;
        }
        None => return tui::run(cli),
    }
    Ok(ExitCode::SUCCESS)
}
