use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use caudra_acp::{AcpAccounts, AcpRuntime, AcpRuntimeGuard, AcpRuntimeResolver};
use color_eyre::Result;
use color_eyre::eyre::Context;

use caudra_agent::prompt::profile::PromptProfileCatalog;
use caudra_agent::tools::ToolRegistry;
use caudra_config::{ProviderConfig, load_permissions};
use caudra_lua::PluginHost;
use caudra_providers::Timeouts;
use caudra_providers::model::Model;
use caudra_providers::provider::available_model_specs;
use caudra_storage::sessions::StoredMode;
use caudra_storage::workspace_binding::StoredWorkspaceBinding;
use caudra_storage::{StateDir, hold_instance};

use super::workcell_runtime::WorkcellRuntime;
use crate::cli::{Cli, WorkcellSelectorArgs};
use crate::setup;
use crate::startup::Startup;

struct SessionResources {
    plugin_host: Option<PluginHost>,
    _runtime: WorkcellRuntime,
}

impl AcpRuntimeGuard for SessionResources {
    fn shutdown(&mut self) -> std::result::Result<(), String> {
        if let Some(mut host) = self.plugin_host.take() {
            host.shutdown_checked().map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

impl Drop for SessionResources {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            tracing::error!(%error, "ACP plugin shutdown failed");
        }
    }
}

/// The accounts as `caudra acp` was started on: the same model resolution
/// and provider config, read again whenever the server asks.
struct CliAccounts {
    model_arg: Option<String>,
    provider: ProviderConfig,
    storage: StateDir,
}

impl AcpAccounts for CliAccounts {
    fn check(&self, model: &Model) -> std::result::Result<(), String> {
        caudra_acp::check_credentials(model)
    }

    fn default_model(&self) -> std::result::Result<Model, String> {
        resolve_acp_model(self.model_arg.as_deref(), &self.provider, &self.storage)
            .map_err(|error| error.to_string())
    }

    fn model_specs(&self) -> Vec<String> {
        available_model_specs(&self.provider.model_policy)
    }
}

/// A machine signed in nowhere still starts when nothing names a model, so
/// the editor connects and its sessions ask for the sign-in.
fn resolve_acp_model(
    model_arg: Option<&str>,
    provider: &ProviderConfig,
    storage: &StateDir,
) -> Result<Model> {
    setup::resolve_model(model_arg, provider, storage, StoredMode::Build).or_else(|error| {
        if model_arg.is_some() || provider.default_model.is_some() {
            return Err(error);
        }
        setup::signed_out_model(&provider.model_policy).ok_or(error)
    })
}

fn runtime_resolver(
    storage: StateDir,
    selection: WorkcellSelectorArgs,
    startup: Startup,
    runs_lua: bool,
    no_jit: bool,
    no_snapshots: bool,
) -> AcpRuntimeResolver {
    Arc::new(
        move |cwd: PathBuf, stored: Option<StoredWorkspaceBinding>| {
            let resolve = || -> Result<AcpRuntime> {
                let mut selection = selection.clone();
                startup.features.require_source(stored.as_ref())?;
                if let Some(binding) = &stored {
                    super::sandbox::recover_binding_source(&mut selection, &storage, binding)?;
                }
                let registry = Arc::new(ToolRegistry::default());
                let runtime = WorkcellRuntime::initialize_session(
                    &selection,
                    &cwd,
                    &storage,
                    &registry,
                    startup.features,
                )?;
                if !runtime.is_remote() {
                    super::adopt_checkout_state(&storage, &cwd);
                    super::reconcile_worktrees(&storage, &cwd);
                }
                if stored.is_some() {
                    StoredWorkspaceBinding::validate_resume_identity(
                        stored.as_ref(),
                        runtime.stored_binding(),
                    )?;
                }
                let plugin_host = super::plugin_host(runs_lua, no_jit, Arc::clone(&registry))?;
                let mut config =
                    super::load_settings(&plugin_host, &startup, &cwd, runtime.is_remote())?
                        .into_config(false)?;
                config.storage.snapshots.enabled &= !no_snapshots;
                config.permissions = if runtime.is_remote() {
                    caudra_config::load_global_permissions()
                } else {
                    load_permissions(&cwd)
                };
                super::apply_startup_policy(&mut config, startup.features);
                config.permissions.yolo |= config.always_yolo;
                config.validate()?;
                if let Some(host) = runtime.local_host() {
                    host.websearch().apply(&config.websearch);
                }
                super::configure_native_tools(&config);
                super::install_native_permission_rules(&plugin_host.plugin_rules(), &cwd);
                plugin_host.warm_jit();
                let seed_permission_mode = Some(super::permission_mode_seed(&config));
                Ok(AcpRuntime {
                    prompt_slots: Arc::new(
                        plugin_host
                            .event_handle()
                            .collect_prompt_slots(&config.agent),
                    ),
                    prompt_profiles: Arc::new(PromptProfileCatalog::discover_user(
                        config.agent.builtin_skills.pstack,
                    )),
                    config: config.agent,
                    permissions_config: config.permissions,
                    decisions_config: config.decisions,
                    seed_permission_mode,
                    snapshots: config.storage.snapshots,
                    plugin_rules: plugin_host.plugin_rules(),
                    registry,
                    workspace_binding: runtime.stored_binding().cloned(),
                    workspace_session: runtime.workspace_session().cloned(),
                    remote_project_context: runtime.remote_project_context().cloned(),
                    local_documents: runtime.local_documents().cloned(),
                    remote_environment: runtime.is_remote().then(|| {
                        caudra_agent::headless::RemoteEnvironment {
                            cwd: runtime.display().cwd.clone(),
                            platform: runtime.display().platform.clone(),
                        }
                    }),
                    guard: Some(Box::new(SessionResources {
                        plugin_host: Some(plugin_host),
                        _runtime: runtime,
                    })),
                })
            };
            resolve().map_err(|error| {
                format!("ACP runtime resolution failed; detached, no local fallback: {error}")
            })
        },
    )
}

pub fn run(model_arg: Option<&str>, cli: &Cli) -> Result<()> {
    // Every phase up to `init_logging` runs without a subscriber, so its cost is
    // invisible unless it is measured here and reported once the sink exists.
    let started = Instant::now();
    let mut phase_start = started;
    let mut lap = || {
        let elapsed = phase_start.elapsed().as_millis() as u64;
        phase_start = Instant::now();
        elapsed
    };
    let storage = StateDir::resolve().context("resolve data directory")?;
    let _instance = hold_instance(&storage).context("register this Caudra as running")?;
    let state_dir_ms = lap();
    caudra_providers::model_registry::load_from_storage(&storage)
        .context("load model purpose bindings")?;
    caudra_providers::calibration::load_from_storage(&storage);
    let model_registry_ms = lap();

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    caudra_config::load_global_env_file();

    let mut plugin_host = super::cli_plugin_host(cli, Arc::new(ToolRegistry::default()))?;
    let mut config = super::load_settings(&plugin_host, &cli.startup, &cwd, true)?
        .into_config(false)
        .context("invalid config")?;
    config.permissions = caudra_config::load_global_permissions();
    super::apply_startup_policy(&mut config, cli.startup.features);

    if cli.yolo || config.always_yolo {
        config.permissions.yolo = true;
    }
    config.validate()?;
    let (storage, _ephemeral_root) =
        super::run_storage(storage, cli.ephemeral || config.storage.ephemeral)?;
    super::configure_native_tools(&config);
    super::install_native_permission_rules(&plugin_host.plugin_rules(), &cwd);

    plugin_host.warm_jit();

    let timeouts = Timeouts {
        connect: config.provider.connect_timeout,
        stream: config.provider.stream_timeout,
    };

    let model = resolve_acp_model(model_arg, &config.provider, &storage)?;
    let build_stack_ms = lap();

    let _logging = setup::init_logging(&config.storage);
    let init_logging_ms = lap();
    cli.startup.upgrade.log();
    setup::apply_storage_limits(&config.storage);
    setup::register_change_stores();
    setup::init_telemetry(&config.telemetry);
    setup::install_panic_log_hook();
    setup::warn_ignored_provider_fields();
    setup::report_startup(setup::MODE_ACP, &model, &cwd);
    tracing::info!(
        state_dir_ms,
        model_registry_ms,
        build_stack_ms,
        init_logging_ms,
        total_ms = started.elapsed().as_millis() as u64,
        "startup phases"
    );

    let thinking = config
        .always_thinking
        .clone()
        .map(caudra_providers::ThinkingConfig::from)
        .unwrap_or_default();

    plugin_host.shutdown_checked()?;
    let accounts = Arc::new(CliAccounts {
        model_arg: model_arg.map(str::to_owned),
        provider: config.provider.clone(),
        storage: storage.clone(),
    });
    caudra_acp::run(caudra_acp::AcpParams {
        model,
        timeouts,
        thinking,
        system_prompt_profile_override: cli.system_prompt_profile.clone(),
        permission_mode: cli.permission_mode_override(),
        model_policy: Arc::new(config.provider.model_policy.clone()),
        accounts,
        runtime_resolver: runtime_resolver(
            storage,
            cli.workcell.clone(),
            cli.startup.clone(),
            cli.runs_lua(),
            cli.no_jit,
            cli.no_snapshots,
        ),
    })
}
