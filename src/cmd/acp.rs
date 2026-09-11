use std::env;
use std::sync::Arc;
use std::time::Instant;

use color_eyre::Result;
use color_eyre::eyre::Context;

use caudra_agent::prompt::profile::PromptProfileCatalog;
use caudra_agent::tools::ToolRegistry;
use caudra_config::{load_env_files, load_permissions};
use caudra_lua::PluginHost;
use caudra_storage::StateDir;
use caudra_storage::sessions::StoredMode;

use crate::setup;

pub fn run(
    model_arg: Option<String>,
    yolo: bool,
    ephemeral: bool,
    no_plugins: bool,
    no_jit: bool,
    profile_arg: Option<String>,
) -> Result<()> {
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
    let state_dir_ms = lap();
    caudra_providers::model_registry::load_from_storage(&storage)
        .context("load model purpose bindings")?;
    let model_registry_ms = lap();

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);
    let env_files_ms = lap();
    let _workcell_host = super::register_builtin_tools(&cwd)?;
    let register_tools_ms = lap();

    let mut plugin_host = PluginHost::with_jit(Arc::clone(ToolRegistry::global_arc()), !no_jit)
        .context("initialize lua plugin host")?;

    let raw_config = plugin_host
        .load_init_files_or_skip(no_plugins, &cwd)
        .context("load init.lua files")?;

    let mut config = raw_config
        .unwrap_or_default()
        .into_config(false)
        .context("invalid config")?;
    config.permissions = load_permissions(&cwd);

    if yolo || config.always_yolo {
        config.permissions.yolo = true;
    }
    config.validate()?;
    let (storage, _ephemeral_root) =
        super::run_storage(storage, ephemeral || config.storage.ephemeral)?;
    super::configure_native_tools(&config.agent);
    super::install_native_permission_rules(&plugin_host.plugin_rules(), &cwd);

    plugin_host
        .load_production_builtins(&config.plugins)
        .context("load builtin plugins")?;

    let timeouts = caudra_providers::Timeouts {
        connect: config.provider.connect_timeout,
        stream: config.provider.stream_timeout,
    };

    let model = setup::resolve_model(
        model_arg.as_deref(),
        &config.provider,
        &storage,
        StoredMode::Build,
    )?;
    let build_stack_ms = lap();

    let _logging = setup::init_logging(&config.storage);
    let init_logging_ms = lap();
    setup::init_telemetry(&config.telemetry);
    setup::install_panic_log_hook();
    setup::warn_ignored_provider_fields();
    setup::report_startup(setup::MODE_ACP, &model, &cwd);
    tracing::info!(
        state_dir_ms,
        model_registry_ms,
        env_files_ms,
        register_tools_ms,
        build_stack_ms,
        init_logging_ms,
        total_ms = started.elapsed().as_millis() as u64,
        "startup phases"
    );

    let prompt_slots = plugin_host
        .event_handle()
        .collect_prompt_slots(&config.agent);
    let prompt_profiles = Arc::new(PromptProfileCatalog::discover_user());
    let thinking = config
        .always_thinking
        .clone()
        .map(caudra_providers::ThinkingConfig::from)
        .unwrap_or_default();

    caudra_acp::run(caudra_acp::AcpParams {
        model,
        config: config.agent,
        permissions_config: config.permissions,
        timeouts,
        initial_wd: cwd,
        prompt_slots: Arc::new(prompt_slots),
        thinking,
        prompt_profiles,
        system_prompt_profile_override: profile_arg,
        yolo,
        model_policy: Arc::new(config.provider.model_policy.clone()),
        plugin_rules: plugin_host.plugin_rules(),
    })
}
