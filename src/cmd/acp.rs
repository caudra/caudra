use std::env;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::Context;

use caudra_agent::prompt::profile::PromptProfileCatalog;
use caudra_agent::tools::ToolRegistry;
use caudra_config::{load_env_files, load_permissions};
use caudra_lua::PluginHost;
use caudra_storage::StateDir;

use crate::setup;

pub fn run(
    model_arg: Option<String>,
    yolo: bool,
    ephemeral: bool,
    no_plugins: bool,
    no_jit: bool,
    profile_arg: Option<String>,
) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    caudra_providers::model_registry::load_from_storage(&storage)
        .context("load model purpose bindings")?;

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);
    let _workcell_host = super::register_builtin_tools(&cwd)?;

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

    let model = setup::resolve_model(model_arg.as_deref(), &config.provider, &storage)?;

    let _logging = setup::init_logging(&config.storage);
    setup::init_telemetry(&config.telemetry);
    setup::install_panic_log_hook();
    setup::warn_ignored_provider_fields();
    setup::report_startup(setup::MODE_ACP, &model, &cwd);

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
