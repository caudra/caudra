mod acp;
mod config;
mod decisions;
mod logs;
mod permissions;
mod sandbox;
mod sandbox_transfer;
mod storage;
mod subcmd;
mod tui;
mod workcell_runtime;
mod worktree;

use std::env;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::Context;

use caudra_agent::herdr::{HerdrEnv, herdr_skill};
use caudra_agent::tools::ToolRegistry;
use caudra_agent::tools::native::skill::install_builtin_skill;
use caudra_config::config_file::{
    self, ConfigFileError, global_init_lua_path, project_init_lua_path,
};
use caudra_config::{AgentConfig, Config, Feature, FeatureDisabled, FeatureFlags, RawConfig};
use caudra_lua::PluginHost;
use caudra_storage::sessions::PermissionMode;
use caudra_storage::{EphemeralRoot, StateDir};

use crate::cli::{AuthAction, Cli, Command, McpAction, WorkcellAuthAction, normalize_tool_name};
use crate::docs;
use crate::sdk_mode::AUTO_PERMISSION_MODE;
use crate::startup::Startup;
use crate::update;

/// The three names Workcell's shell forwards from this process into every
/// command it runs. Windows reads the last two, Unix the first.
const TEMP_DIR_VARS: [&str; 3] = ["TMPDIR", "TMP", "TEMP"];
const INIT_LUA_SKIPPED: &str = "init.lua not loaded, because Lua plugins are experimental and turned off; move its settings to caudra.toml (`caudra config example` lists them all), or set `experimental.lua_plugins = true` in the global caudra.toml and restart Caudra";
const FEATURES_CHANGED: &str =
    "the [experimental] table in the global caudra.toml changed; restart Caudra to apply it";
const AUTO_NEEDS_ENGINE: &str = "always_auto is set, but Auto mode is experimental and turned off, so sessions start in Ask; set `experimental.decision_engine = true` in the global caudra.toml and restart Caudra to use it";

fn run_storage(persistent: StateDir, ephemeral: bool) -> Result<(StateDir, Option<EphemeralRoot>)> {
    if !ephemeral {
        return Ok((persistent, None));
    }
    let (storage, root) =
        StateDir::activate_ephemeral(persistent).context("create ephemeral state directory")?;
    Ok((storage, Some(root)))
}

fn permission_mode_seed(config: &Config) -> PermissionMode {
    if config.permissions.yolo || config.always_yolo {
        PermissionMode::Yolo
    } else if config.always_auto {
        PermissionMode::Auto
    } else {
        PermissionMode::Ask
    }
}

/// Every entry point builds its Lua host here, so none can boot a VM the
/// startup policy left off. A disabled host runs no VM, thread, or watchdog.
fn plugin_host(runs_lua: bool, no_jit: bool, registry: Arc<ToolRegistry>) -> Result<PluginHost> {
    if !runs_lua {
        return Ok(PluginHost::disabled());
    }
    PluginHost::with_jit(registry, !no_jit).context("initialize lua plugin host")
}

fn cli_plugin_host(cli: &Cli, registry: Arc<ToolRegistry>) -> Result<PluginHost> {
    plugin_host(cli.runs_lua(), cli.no_jit, registry)
}

/// Layers the ordinary settings, lowest first: the global `caudra.toml`, the
/// global `init.lua`, the project `.caudra/caudra.toml`, then the project
/// `init.lua`. A remote workspace has no local project, so it skips both
/// project layers. The `init.lua` layers run only on a live Lua host, and no
/// layer can turn an experiment on: that was settled at startup.
fn load_settings(
    plugin_host: &PluginHost,
    startup: &Startup,
    cwd: &Path,
    remote: bool,
) -> Result<RawConfig> {
    let mut settings = RawConfig::default();
    if let Some(config_dir) = &startup.config_dir {
        settings =
            config_file::load_global_config(&config_file::global_config_path(config_dir))?.settings;
        if plugin_host.is_enabled()
            && let Some(lua) = plugin_host
                .run_global_init(config_dir)
                .context("run the global init.lua")?
        {
            settings.merge_global(lua);
        }
    }
    if !remote {
        settings.merge(config_file::load_project_config(
            &config_file::project_config_path(cwd),
        )?);
        if plugin_host.is_enabled()
            && let Some(lua) = plugin_host
                .run_project_init(cwd)
                .context("run the project init.lua")?
        {
            settings.merge(lua);
        }
    }
    Ok(settings)
}

/// What a settings load deliberately left out, worded for the user. Finding
/// an `init.lua` only checks that the file exists; nothing reads it.
fn settings_notices(cli: &Cli, cwd: &Path, remote: bool) -> Vec<String> {
    let mut notices = Vec::new();
    if !cli.startup.features.enabled(Feature::LuaPlugins) {
        let skipped: Vec<String> = cli
            .startup
            .config_dir
            .iter()
            .map(|dir| global_init_lua_path(dir))
            .chain((!remote).then(|| project_init_lua_path(cwd)))
            .filter(|path| path.is_file())
            .map(|path| path.display().to_string())
            .collect();
        if !skipped.is_empty() {
            notices.push(format!("{INIT_LUA_SKIPPED}: {}", skipped.join(", ")));
        }
    }
    if cli.startup.features_changed() {
        notices.push(FEATURES_CHANGED.to_owned());
    }
    notices
}

/// A configured Auto that the startup policy cannot honour starts sessions in
/// Ask; saying so keeps the setting from looking ignored.
fn auto_notice(config: &Config) -> Option<String> {
    (config.always_auto && !config.permissions.decision_engine)
        .then(|| AUTO_NEEDS_ENGINE.to_owned())
}

/// Settles what only the startup policy decides, once the permissions are
/// loaded, so no settings layer or reload can change it.
fn apply_startup_policy(config: &mut Config, features: FeatureFlags) {
    config.agent.features = features;
    config.permissions.decision_engine = features.enabled(Feature::DecisionEngine);
}

/// Every entry point resolves config here, so the CLI tool flags cannot apply
/// in the TUI and silently go missing from `caudra tools`.
fn load_config(plugin_host: &PluginHost, cli: &Cli, cwd: &Path, remote: bool) -> Result<Config> {
    let mut config = load_settings(plugin_host, &cli.startup, cwd, remote)?
        .into_config(cli.no_rtk)
        .context("invalid config")?;
    config.storage.snapshots.enabled &= !cli.no_snapshots;
    config.permissions = if remote {
        caudra_config::load_global_permissions()
    } else {
        caudra_config::load_permissions(cwd)
    };
    apply_startup_policy(&mut config, cli.startup.features);

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

/// A linked worktree's one-time import of the notes and plans it kept before
/// it shared its main checkout's state. A failure leaves them where they were,
/// so it warns rather than stopping the session.
fn adopt_checkout_state(storage: &StateDir, cwd: &Path) {
    if let Err(error) = caudra_storage::projects::adopt_checkout_state(storage, cwd) {
        tracing::warn!(
            %error,
            cwd = %cwd.display(),
            "could not import a linked worktree's own notes and plans"
        );
    }
}

/// Moves sessions out of the removed worktrees of `cwd`'s repository, saying
/// what moved. A failure leaves them for the next run, so it only warns.
fn reconcile_worktrees(storage: &StateDir, cwd: &Path) -> Vec<String> {
    caudra_storage::worktrees::reconcile(storage, cwd)
        .map(|moved| moved.iter().map(ToString::to_string).collect())
        .unwrap_or_else(|error| {
            tracing::warn!(
                %error,
                cwd = %cwd.display(),
                "could not move sessions back out of removed worktrees"
            );
            Vec::new()
        })
}

/// Native tools read their options from here rather than from config
/// directly, so `caudra-agent` stays free of a config dependency it would
/// otherwise need only for two numbers.
///
/// Not installing a builtin skill is how its `plugins.skill` switch takes
/// effect, and how an experiment that is off keeps its authoring skill out of
/// sight. The `caudra-plugin-dev` skill is rendered from the live Lua API
/// docs, so only `caudra-lua` can build it, and `caudra-docs` reads the user
/// docs this binary embeds. The `herdr` skill exists only inside a Herdr pane,
/// the one place its CLI reaches a session.
fn configure_native_tools(agent: &AgentConfig) {
    if agent.builtin_skills.plugin_dev && agent.features.enabled(Feature::LuaPlugins) {
        install_builtin_skill(caudra_lua::docs_render::plugin_dev_skill());
    }
    if agent.builtin_skills.workflow_dev && agent.features.enabled(Feature::Workflows) {
        install_builtin_skill(caudra_agent::workflow::workflow_dev_skill());
    }
    if agent.builtin_skills.docs {
        install_builtin_skill(docs::skill());
    }
    if let Some(herdr) = HerdrEnv::detect() {
        install_builtin_skill(herdr_skill(herdr));
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

/// Refuses an experiment the invocation names before anything reads its
/// configuration, credentials, stores, or network. Help hides these; this is
/// what stops them running.
fn require_requested_features(cli: &Cli) -> Result<(), FeatureDisabled> {
    let features = cli.startup.features;
    cli.workcell.require_features(features)?;
    if cli.auto || cli.permission_mode.as_deref() == Some(AUTO_PERMISSION_MODE) {
        features.require(Feature::DecisionEngine)?;
    }
    if cli.name.is_some() {
        features.require(Feature::CrossSessionMessaging)?;
    }
    match &cli.command {
        Some(
            Command::Sandbox { .. }
            | Command::Auth {
                action: AuthAction::Sandbox { .. },
            },
        ) => features.require(Feature::Sandboxes),
        Some(Command::Auth {
            action: AuthAction::Workcell { .. },
        }) => features.require(Feature::RemoteWorkcell),
        Some(Command::Remote { .. }) if !features.enabled(Feature::Sandboxes) => {
            features.require(Feature::RemoteWorkcell)
        }
        Some(Command::Decisions { .. }) => features.require(Feature::DecisionEngine),
        _ => Ok(()),
    }
}

/// Settings notices for subcommands. A session reports its own once its
/// config is loaded, in the TUI or on stderr.
fn report_settings_notices(cli: &Cli) {
    if !cli.command.as_ref().is_some_and(Command::loads_settings) {
        return;
    }
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    for notice in settings_notices(cli, &cwd, cli.workcell.is_set()) {
        eprintln!("warning: {notice}");
    }
}

pub fn dispatch(mut cli: Cli, startup: Result<Startup, ConfigFileError>) -> Result<ExitCode> {
    caudra_storage::paths::check_namespace_override()?;
    cli.startup = match startup {
        Ok(startup) => startup,
        Err(_)
            if cli
                .command
                .as_ref()
                .is_some_and(Command::runs_without_config) =>
        {
            Startup::default()
        }
        Err(error) => return Err(error.into()),
    };
    require_requested_features(&cli)?;
    report_settings_notices(&cli);
    // Before anything opens a private file through one of these directories,
    // and before the scratch redirect creates any more of them.
    let tightened = caudra_storage::paths::tighten_private_dirs();
    redirect_temp_dir();
    match cli.command.take() {
        Some(Command::Sandbox { action }) => {
            let storage = StateDir::resolve().context("resolve data directory")?;
            sandbox::run(action, &storage, cli.startup.features)?;
        }
        Some(Command::Permissions { action, database }) => {
            if cli.workcell.is_set() || cli.ephemeral {
                return Err(color_eyre::eyre::eyre!(
                    "permission administration requires local persistent storage"
                ));
            }
            permissions::run(action, database)?;
        }
        Some(Command::Decisions { action }) => {
            decisions::run(action, &cli)?;
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
                    cli.startup.features,
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
        Some(Command::Acp { model }) => {
            acp::run(model.as_deref(), &cli)?;
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
        Some(Command::Config { action }) => config::run(action)?,
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
                    cli.startup.features,
                )?;
                if runtime.is_remote() {
                    return Err(color_eyre::eyre::eyre!(
                        "storage and snapshot commands are disabled for remote Workcell sessions"
                    ));
                }
            }
            storage::run(action, &cli)?;
        }
        Some(Command::Prompt {
            variant,
            plan,
            tools,
            names,
        }) => {
            subcmd::prompt(&cli, &variant, plan, tools, names)?;
        }
        None => return tui::run(cli, tightened),
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    use caudra_agent::tools::ToolRegistry;
    use caudra_config::{Feature, FeatureDisabled, FeatureFlags, RawConfig};
    use caudra_storage::sessions::PermissionMode;
    use clap::Parser;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        INIT_LUA_SKIPPED, Result, auto_notice, cli_plugin_host, load_config, load_settings,
        permission_mode_seed, require_requested_features, settings_notices,
    };
    use crate::cli::Cli;
    use crate::startup::Startup;

    const MESSAGING_NAME: &str = "ci-watcher";
    const GLOBAL_TOML: &str = "config/caudra.toml";
    const GLOBAL_LUA: &str = "config/init.lua";
    const PROJECT_TOML: &str = "project/.caudra/caudra.toml";
    const PROJECT_LUA: &str = "project/.caudra/init.lua";
    const BROKEN_LUA: &str = "error('init.lua ran')";
    const LUA_ON: FeatureFlags = FeatureFlags::NONE.with(Feature::LuaPlugins);

    /// A global config directory and a project beside it, holding `files`.
    struct Fixture {
        root: TempDir,
        startup: Startup,
    }

    impl Fixture {
        fn new(features: FeatureFlags, files: &[(&str, &str)]) -> Self {
            let root = TempDir::new().unwrap();
            for (path, contents) in files {
                let path = root.path().join(path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, contents).unwrap();
            }
            fs::create_dir_all(root.path().join("project")).unwrap();
            let startup = Startup {
                features,
                config_dir: Some(root.path().join("config")),
            };
            Self { root, startup }
        }

        fn project(&self) -> PathBuf {
            self.root.path().join("project")
        }

        fn cli(&self, args: &[&str]) -> Cli {
            let mut cli = Cli::parse_from(args);
            cli.startup = self.startup.clone();
            cli
        }

        fn settings(&self, remote: bool) -> Result<RawConfig> {
            let cli = self.cli(&["caudra"]);
            let mut host = cli_plugin_host(&cli, Arc::new(ToolRegistry::new()))?;
            let settings = load_settings(&host, &self.startup, &self.project(), remote);
            host.shutdown_checked()?;
            settings
        }
    }

    #[test_case(false, Some(true); "local_project_wins")]
    #[test_case(true, Some(false); "remote_skips_the_project")]
    fn project_toml_layers_over_global_toml(remote: bool, scrollbar: Option<bool>) {
        let fixture = Fixture::new(
            FeatureFlags::NONE,
            &[
                (GLOBAL_TOML, "always_fast = true\n[ui]\nscrollbar = false\n"),
                (PROJECT_TOML, "[ui]\nscrollbar = true\n"),
            ],
        );
        let settings = fixture.settings(remote).unwrap();
        assert_eq!(settings.always_fast, Some(true));
        assert_eq!(settings.ui.scrollbar, scrollbar);
    }

    #[test]
    fn each_init_lua_sits_above_its_own_toml() {
        let fixture = Fixture::new(
            LUA_ON,
            &[
                (GLOBAL_TOML, "always_fast = true\n[ui]\nscrollbar = true\n"),
                (
                    GLOBAL_LUA,
                    "caudra.setup({ always_fast = false, ui = { scrollbar = false } })",
                ),
                (PROJECT_TOML, "[ui]\nscrollbar = true\n"),
            ],
        );
        let settings = fixture.settings(false).unwrap();
        assert_eq!(settings.always_fast, Some(false));
        assert_eq!(settings.ui.scrollbar, Some(true));
    }

    #[test_case(FeatureFlags::NONE, &["caudra"]; "experiment_off")]
    #[test_case(LUA_ON, &["caudra", "--no-plugins"]; "no_plugins_forces_it_off")]
    fn no_init_lua_runs_without_lua(features: FeatureFlags, args: &[&str]) {
        let fixture = Fixture::new(
            features,
            &[(GLOBAL_LUA, BROKEN_LUA), (PROJECT_LUA, BROKEN_LUA)],
        );
        let cli = fixture.cli(args);
        let host = cli_plugin_host(&cli, Arc::new(ToolRegistry::new())).unwrap();
        assert!(!host.is_enabled());
        let config = load_config(&host, &cli, &fixture.project(), false).unwrap();
        assert_eq!(config.agent.features, features);
    }

    #[test_case(false; "local")]
    #[test_case(true; "remote")]
    fn a_live_host_runs_only_the_applicable_init_lua(remote: bool) {
        let fixture = Fixture::new(LUA_ON, &[(PROJECT_LUA, BROKEN_LUA)]);
        assert_eq!(fixture.settings(remote).is_err(), !remote);
    }

    #[test]
    fn skipped_init_lua_is_reported_without_being_read() {
        let fixture = Fixture::new(
            FeatureFlags::NONE,
            &[(GLOBAL_LUA, BROKEN_LUA), (PROJECT_LUA, BROKEN_LUA)],
        );
        let cli = fixture.cli(&["caudra"]);
        let notices = settings_notices(&cli, &fixture.project(), false);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].starts_with(INIT_LUA_SKIPPED), "{notices:?}");
        assert!(notices[0].contains(GLOBAL_LUA), "{notices:?}");
        assert!(notices[0].contains(PROJECT_LUA), "{notices:?}");
        let remote = settings_notices(&cli, &fixture.project(), true);
        assert!(!remote[0].contains(PROJECT_LUA), "{remote:?}");
    }

    #[test_case("[experimental]\nworkflows = true\n"; "experimental_table")]
    #[test_case("[experimental]\n"; "empty_experimental_table")]
    fn a_project_cannot_opt_in(project: &str) {
        let fixture = Fixture::new(FeatureFlags::NONE, &[(PROJECT_TOML, project)]);
        assert!(fixture.settings(false).is_err());
    }

    #[test_case(FeatureFlags::NONE, true; "engine_off")]
    #[test_case(FeatureFlags::NONE.with(Feature::DecisionEngine), false; "engine_on")]
    fn configured_auto_follows_the_decision_engine(features: FeatureFlags, noticed: bool) {
        let fixture = Fixture::new(features, &[(GLOBAL_TOML, "always_auto = true\n")]);
        let cli = fixture.cli(&["caudra"]);
        let host = cli_plugin_host(&cli, Arc::new(ToolRegistry::new())).unwrap();
        let config = load_config(&host, &cli, &fixture.project(), false).unwrap();
        assert_eq!(config.permissions.decision_engine, !noticed);
        assert_eq!(permission_mode_seed(&config), PermissionMode::Auto);
        assert_eq!(auto_notice(&config).is_some(), noticed);
    }

    #[test_case(FeatureFlags::NONE, false; "experiment_off")]
    #[test_case(FeatureFlags::NONE.with(Feature::CrossSessionMessaging), true; "experiment_on")]
    fn messaging_name_requires_the_experiment(features: FeatureFlags, allowed: bool) {
        let mut cli = Cli::parse_from(["caudra", "--name", MESSAGING_NAME]);
        cli.startup.features = features;
        assert_eq!(
            require_requested_features(&cli)
                .err()
                .map(|FeatureDisabled(feature)| feature),
            (!allowed).then_some(Feature::CrossSessionMessaging)
        );
    }

    #[test_case(false, false, PermissionMode::Ask; "default_ask")]
    #[test_case(true, false, PermissionMode::Auto; "global_auto")]
    #[test_case(false, true, PermissionMode::Yolo; "global_yolo")]
    #[test_case(true, true, PermissionMode::Yolo; "yolo_takes_precedence")]
    fn global_permission_seed(auto: bool, yolo: bool, expected: PermissionMode) {
        let config = RawConfig {
            always_auto: Some(auto),
            always_yolo: Some(yolo),
            ..RawConfig::default()
        }
        .into_config(false)
        .unwrap();
        assert_eq!(permission_mode_seed(&config), expected);
    }
}
