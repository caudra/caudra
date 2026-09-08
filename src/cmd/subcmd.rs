use std::env;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::{Context, bail};

use caudra_agent::mcp::{McpSession, config as mcp_config, oauth as mcp_oauth};
use caudra_agent::tools::{
    DescriptionContext, RegisteredTool, SHELL_TOOL_NAME, ToolAudience, ToolFilter, ToolRegistry,
    VIEW_IMAGE_TOOL_NAME, capability_exclusions, credential_exclusions, deferral, is_tool_enabled,
};
use caudra_config::providers::{
    Protocol, ProviderDef, ProvidersConfig, all_builtins, builtin_provider, resolve_api_key_env,
    resolve_base_url, resolve_default_model, resolve_display_name, resolve_login_url, slugify,
};
use caudra_config::{
    AgentConfig, Config, DefaultEffect, PermissionsConfig, ToolKey, load_env_files,
    load_permissions,
};
use caudra_lua::PluginHost;
use caudra_providers::provider::fetch_all_models;
use caudra_providers::{Model, ProviderData, Timeouts, catalog_providers};
use caudra_providers::{anthropic_auth, copilot_auth, dynamic, openai_auth, xai_auth};
use caudra_storage::StateDir;
use caudra_storage::auth::{
    ProviderAuth, ProviderCredentials, delete_provider_credentials, load_provider_credentials,
    save_provider_credentials, try_load_provider_auth,
};
use caudra_storage::model::persist_model;

use crate::cli::{AuthMethod, Cli, normalize_tool_name};
use crate::setup::resolve_model;

const AUTH_STATUS_EMPTY: &str = "       ";
const AUTH_STATUS_ENV: &str = "\x1b[33m~ env  \x1b[0m";
const AUTH_STATUS_KEY: &str = "\x1b[32m✓ key  \x1b[0m";
const AUTH_STATUS_OAUTH: &str = "\x1b[32m✓ oauth\x1b[0m";
const PROVIDER_SLUG_WIDTH: usize = 14;

#[derive(Debug, PartialEq, Eq)]
enum LoginRoute {
    AnthropicOauth,
    OpenAiOauth,
    XaiOauth,
    Copilot,
    ApiKey,
}

pub fn auth_login(
    provider: Option<&str>,
    method: Option<AuthMethod>,
    storage: &StateDir,
) -> Result<()> {
    match provider {
        Some(provider) => login_slug(&slugify(provider), method, storage)?,
        None => login_interactive(storage)?,
    }
    Ok(())
}

fn login_slug(slug: &str, method: Option<AuthMethod>, storage: &StateDir) -> Result<()> {
    match login_route(slug, method)? {
        LoginRoute::AnthropicOauth => anthropic_auth::login(storage)?,
        LoginRoute::OpenAiOauth => openai_auth::login(storage)?,
        LoginRoute::XaiOauth => xai_auth::login(storage)?,
        LoginRoute::Copilot => copilot_auth::login(storage)?,
        LoginRoute::ApiKey => login_api_key_slug(slug, storage)?,
    }
    Ok(())
}

fn login_route(slug: &str, method: Option<AuthMethod>) -> Result<LoginRoute> {
    match (slug, method) {
        ("anthropic", Some(AuthMethod::ApiKey)) | ("openai", Some(AuthMethod::ApiKey)) => {
            Ok(LoginRoute::ApiKey)
        }
        ("anthropic", None | Some(AuthMethod::Oauth)) => Ok(LoginRoute::AnthropicOauth),
        ("openai", None | Some(AuthMethod::Oauth)) => Ok(LoginRoute::OpenAiOauth),
        ("xai", None) => Ok(LoginRoute::XaiOauth),
        ("copilot", None) => Ok(LoginRoute::Copilot),
        (_, Some(_)) => bail!("--method is supported only for Anthropic and OpenAI"),
        _ => Ok(LoginRoute::ApiKey),
    }
}

fn login_api_key_slug(slug: &str, storage: &StateDir) -> Result<()> {
    if builtin_provider(slug).is_none()
        && dynamic::display_name(slug).is_none()
        && ProvidersConfig::load().get(slug).is_none()
        && let Some(provider_data) = caudra_providers::catalog_provider(slug)
    {
        login_catalog_provider(&provider_data, storage)
    } else {
        login_provider(slug, storage)
    }
}

fn prompt_auth_method(display_name: &str) -> Result<AuthMethod> {
    println!();
    println!("  Authenticate with {display_name}:");
    println!();
    println!("  1. Subscription OAuth");
    println!("  2. API key");
    println!();
    print!("  Select [1-2]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    match input.trim() {
        "1" => Ok(AuthMethod::Oauth),
        "2" => Ok(AuthMethod::ApiKey),
        _ => bail!("invalid selection"),
    }
}

fn login_provider(slug: &str, storage: &StateDir) -> Result<()> {
    let builtin = builtin_provider(slug);
    let is_custom = ProvidersConfig::load().get(slug).is_some();
    if builtin.is_none() && dynamic::display_name(slug).is_none() && !is_custom {
        bail!("unknown provider '{slug}'");
    }

    if builtin.is_none() && dynamic::auth_providers().iter().any(|(s, _)| *s == slug) {
        dynamic::login(slug)?;
        return Ok(());
    }

    let mut config = ProvidersConfig::load();
    let def = config.get(slug).cloned();

    let plan = select_plan(slug, builtin, def.as_ref())?;

    let needs_url = builtin.is_some_and(|b| b.needs_url);
    let host_url = if needs_url {
        Some(prompt_host_url(
            slug,
            &resolve_display_name(slug, def.as_ref()),
            def.as_ref(),
        )?)
    } else {
        None
    };

    let api_key_optional = needs_url;
    let login_url = resolve_login_url(slug, plan.as_deref());
    let api_key = prompt_api_key(
        login_url.as_deref(),
        &resolve_display_name(slug, def.as_ref()),
        api_key_optional,
    )?;

    let mut provider_def = def.unwrap_or_default();
    if let Some(plan_name) = &plan {
        provider_def.plan = Some(plan_name.clone());
    }
    if let Some(url) = &host_url {
        provider_def.base_url = Some(url.clone());
    }

    let has_key = !api_key.is_empty();
    if has_key {
        let creds = ProviderCredentials {
            api_key,
            host: None,
        };
        save_provider_credentials(storage, slug, &creds).context("save credentials")?;
    }

    if plan.is_some() || needs_url || host_url.is_some() || builtin.is_none() {
        config.upsert(slug.to_string(), provider_def);
        config.save().context("save providers.toml")?;
    }

    let default_model = if needs_url {
        None
    } else {
        resolve_default_model(slug, config.get(slug))
    };
    if let Some(model) = &default_model {
        persist_model(storage, model);
    }

    println!();
    let display = resolve_display_name(slug, config.get(slug));
    println!("  \x1b[32m✓\x1b[0m Configured: {}", display);
    if let Some(url) = resolve_base_url(slug, config.get(slug)) {
        println!("  Endpoint: {}", url);
    }
    if let Some(model) = &default_model {
        println!("  Default model: {}", model);
    }
    if has_key {
        println!("  Credentials: ~/.local/state/caudra/auth/{}.json", slug);
    } else {
        let env_var = resolve_api_key_env(slug, config.get(slug));
        println!(
            "  Set API key via: {} or run: caudra auth login {}",
            env_var, slug
        );
    }

    Ok(())
}

fn login_interactive(storage: &StateDir) -> Result<()> {
    let builtins = all_builtins();
    let config = ProvidersConfig::load();
    let custom_slugs: Vec<&String> = config
        .providers
        .keys()
        .filter(|s| builtin_provider(s).is_none() && *s != "opencode")
        .collect();
    let catalog_entries = catalog_providers();
    let custom_idx = builtins.len() + custom_slugs.len() + catalog_entries.len() + 1;
    let number_width = custom_idx.to_string().len();
    let slug_width = builtins
        .iter()
        .map(|provider| provider.slug.len())
        .chain(custom_slugs.iter().map(|slug| slug.len()))
        .chain(catalog_entries.iter().map(|provider| provider.slug.len()))
        .max()
        .unwrap_or(PROVIDER_SLUG_WIDTH)
        .max(PROVIDER_SLUG_WIDTH);

    println!();
    println!("  Available providers:");
    println!();
    for (i, b) in builtins.iter().enumerate() {
        let status = match try_load_provider_auth(storage, b.slug) {
            Ok(Some(ProviderAuth::OAuth(_))) => AUTH_STATUS_OAUTH,
            Ok(Some(ProviderAuth::ApiKey(_))) => AUTH_STATUS_KEY,
            _ if env::var(b.default_api_key_env).is_ok() => AUTH_STATUS_ENV,
            _ => AUTH_STATUS_EMPTY,
        };
        let number = i + 1;
        let slug = b.slug;
        let display = b.display_name;
        println!("  {status} {number:>number_width$}. {slug:<slug_width$} {display}");
    }
    let mut idx = builtins.len();
    for slug in &custom_slugs {
        idx += 1;
        let status = if load_provider_credentials(storage, slug).is_some() {
            AUTH_STATUS_KEY
        } else {
            AUTH_STATUS_EMPTY
        };
        let display = config
            .get(slug)
            .and_then(|d| d.display_name.as_deref())
            .unwrap_or(slug);
        println!("  {status} {idx:>number_width$}. {slug:<slug_width$} {display}");
    }

    for cat in &catalog_entries {
        idx += 1;
        let status = if load_provider_credentials(storage, &cat.slug).is_some() {
            AUTH_STATUS_KEY
        } else {
            AUTH_STATUS_EMPTY
        };
        let slug = &cat.slug;
        let display = &cat.display_name;
        println!("  {status} {idx:>number_width$}. {slug:<slug_width$} {display}");
    }
    println!("  {AUTH_STATUS_EMPTY} {custom_idx:>number_width$}. Custom provider...");
    println!();

    print!("  Select [1-{}]: ", custom_idx);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let choice: usize = input.trim().parse().context("enter a number")?;

    if choice == 0 || choice > custom_idx {
        bail!("invalid selection");
    }

    if choice == custom_idx {
        login_custom(storage)?;
    } else if choice <= builtins.len() {
        let slug = builtins[choice - 1].slug;
        let method = match slug {
            "anthropic" | "openai" => Some(prompt_auth_method(&resolve_display_name(
                slug,
                config.get(slug),
            ))?),
            _ => None,
        };
        login_slug(slug, method, storage)?;
    } else if choice <= builtins.len() + custom_slugs.len() {
        let slug = custom_slugs[choice - builtins.len() - 1];
        login_provider(slug, storage)?;
    } else {
        let provider = &catalog_entries[choice - builtins.len() - custom_slugs.len() - 1];
        login_catalog_provider(provider, storage)?;
    }

    Ok(())
}

fn login_catalog_provider(provider: &ProviderData, storage: &StateDir) -> Result<()> {
    println!();
    if let Some(ref var) = provider.env_keys.first() {
        println!("  Provider: {} (env: {var})", provider.slug);
    } else {
        println!("  Provider: {}", provider.slug);
    }
    print!("  API key: ");
    io::stdout().flush()?;
    let mut key = String::new();
    io::stdin().read_line(&mut key)?;
    let key = key.trim().to_string();
    if key.is_empty() {
        println!("  Skipped (no key entered)");
        return Ok(());
    }
    let creds = ProviderCredentials {
        api_key: key,
        host: None,
    };
    save_provider_credentials(storage, &provider.slug, &creds).context("save credentials")?;
    println!("  \x1b[32m✓\x1b[0m Saved credentials for {}", provider.slug);
    println!(
        "  Credentials: ~/.local/state/caudra/auth/{}.json",
        provider.slug
    );
    println!(
        "  You can also set via: {}",
        provider
            .env_keys
            .first()
            .cloned()
            .unwrap_or_else(|| "API key environment variable".to_string())
    );
    Ok(())
}

fn custom_protocol(input: &str) -> Option<Protocol> {
    match input.trim() {
        "1" | "openai" => Some(Protocol::Openai),
        "2" | "openai-responses" => Some(Protocol::OpenaiResponses),
        "3" | "anthropic" => Some(Protocol::Anthropic),
        "4" | "google" => Some(Protocol::Google),
        _ => None,
    }
}

fn login_custom(storage: &StateDir) -> Result<()> {
    print!("  Provider name: ");
    io::stdout().flush()?;
    let mut name = String::new();
    io::stdin().read_line(&mut name)?;
    let slug = slugify(&name);
    if slug.is_empty() {
        bail!("provider name cannot be empty");
    }

    println!("  Protocol:");
    println!("    1. openai           (OpenAI Chat Completions)");
    println!("    2. openai-responses (OpenAI Responses API)");
    println!("    3. anthropic        (Anthropic messages API)");
    println!("    4. google           (Google Gemini API)");
    print!("  Select [1-4]: ");
    io::stdout().flush()?;
    let mut proto_input = String::new();
    io::stdin().read_line(&mut proto_input)?;
    let protocol = custom_protocol(&proto_input)
        .ok_or_else(|| color_eyre::eyre::eyre!("invalid protocol selection"))?;

    print!("  Base URL: ");
    io::stdout().flush()?;
    let mut url_input = String::new();
    io::stdin().read_line(&mut url_input)?;
    let base_url = url_input.trim().to_string();
    if base_url.is_empty() {
        bail!("base URL cannot be empty");
    }

    let display_name = format!("Custom ({slug})");
    let api_key_env = format!("{}_API_KEY", slug.to_uppercase().replace('-', "_"));

    print!("  API key (or Enter to skip): ");
    io::stdout().flush()?;
    let mut key_input = String::new();
    io::stdin().read_line(&mut key_input)?;
    let api_key = key_input.trim().to_string();

    let mut config = ProvidersConfig::load();
    let provider_def = ProviderDef {
        display_name: Some(display_name),
        protocol: Some(protocol),
        base_url: Some(base_url.clone()),
        api_key_env: Some(api_key_env.clone()),
        discover_models: true,
        ..Default::default()
    };

    let has_key = !api_key.is_empty();
    if has_key {
        let creds = ProviderCredentials {
            api_key,
            host: None,
        };
        save_provider_credentials(storage, &slug, &creds).context("save credentials")?;
    }

    config.upsert(slug.clone(), provider_def);
    config.save().context("save providers.toml")?;

    println!();
    println!("  \x1b[32m✓\x1b[0m Configured: {}", slug);
    println!("  Endpoint: {}", base_url);
    if has_key {
        println!("  Credentials: ~/.local/state/caudra/auth/{}.json", slug);
    } else {
        println!(
            "  Set API key via: {} or run: caudra auth login {}",
            api_key_env, slug
        );
    }
    println!("  Use with: caudra -m {}/<model>", slug);

    Ok(())
}

fn select_plan(
    slug: &str,
    builtin: Option<&'static caudra_config::providers::BuiltInProvider>,
    def: Option<&ProviderDef>,
) -> Result<Option<String>> {
    let plans = builtin.and_then(|b| b.plans);
    if plans.is_none_or(|p| p.len() <= 1) {
        if let Some(d) = def {
            return Ok(d.plan.clone());
        }
        return Ok(None);
    }
    let plans = plans.unwrap();

    if let Some(d) = def
        && d.plan.is_some()
    {
        return Ok(d.plan.clone());
    }

    println!();
    println!("  {} plan:", resolve_display_name(slug, def));
    for (i, (_key, plan)) in plans.iter().enumerate() {
        println!("    {}. {} ({})", i + 1, plan.display_name, plan.base_url);
    }
    println!();
    print!("  Select [1-{}]: ", plans.len());
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let choice: usize = input.trim().parse().context("enter a number")?;
    if choice == 0 || choice > plans.len() {
        bail!("invalid plan selection");
    }
    Ok(Some(plans[choice - 1].0.to_string()))
}

fn prompt_host_url(slug: &str, display_name: &str, def: Option<&ProviderDef>) -> Result<String> {
    let default = resolve_base_url(slug, def).unwrap_or_default();
    print!("  {} host URL [{}]: ", display_name, default);
    io::stdout().flush()?;

    let mut url = String::new();
    io::stdin().read_line(&mut url)?;
    let url = url.trim().to_string();

    Ok(if url.is_empty() { default } else { url })
}

fn prompt_api_key(url: Option<&str>, display_name: &str, optional: bool) -> Result<String> {
    if let Some(url) = url {
        if let Err(e) = open::that(url) {
            tracing::warn!(error = %e, "failed to open browser");
        }
        println!("  Opened {} in your browser.", url);
    }
    if optional {
        print!("  {} API key (or Enter to skip): ", display_name);
    } else {
        print!("  {} API key: ", display_name);
    }
    io::stdout().flush()?;

    let mut api_key = String::new();
    io::stdin().read_line(&mut api_key)?;
    let api_key = api_key.trim().to_string();

    Ok(api_key)
}

pub fn auth_logout(provider: &str, storage: &StateDir) -> Result<()> {
    let slug = slugify(provider);
    match slug.as_str() {
        "anthropic" => anthropic_auth::logout(storage)?,
        "openai" => openai_auth::logout(storage)?,
        "xai" => xai_auth::logout(storage)?,
        "copilot" => copilot_auth::logout(storage)?,
        _ => {
            let mut config = ProvidersConfig::load();
            let deleted =
                delete_provider_credentials(storage, &slug).context("delete credentials")?;
            if deleted {
                println!("Removed credentials for '{}'.", slug);
            }
            if config.remove(&slug) {
                config.save().context("save providers.toml")?;
            }
            if !deleted && builtin_provider(&slug).is_none() {
                dynamic::logout(&slug)?;
            }
        }
    }
    Ok(())
}

pub fn auth_status(storage: &StateDir) -> Result<()> {
    let config = ProvidersConfig::load();
    let builtins = all_builtins();

    println!();
    for b in &builtins {
        let def = config.get(b.slug);
        let display = resolve_display_name(b.slug, def);
        let auth = try_load_provider_auth(storage, b.slug)
            .with_context(|| format!("load credentials for '{}'", b.slug))?;

        if matches!(&auth, Some(ProviderAuth::OAuth(_))) {
            println!("  \x1b[32m✓\x1b[0m {:<14} {} (oauth)", b.slug, display);
        } else if let Some(ProviderAuth::ApiKey(creds)) = &auth {
            let plan_info = def
                .and_then(|d| d.plan.as_deref())
                .map(|p| format!(" ({p})"))
                .unwrap_or_default();
            println!(
                "  \x1b[32m✓\x1b[0m {:<14} {} (key: {}){}",
                b.slug,
                display,
                creds.masked_api_key(),
                plan_info
            );
        } else if env::var(b.default_api_key_env).is_ok() {
            println!(
                "  \x1b[33m~\x1b[0m {:<14} {} (via {})",
                b.slug, display, b.default_api_key_env
            );
        } else if def.is_some_and(|d| d.base_url.is_some()) {
            println!("  \x1b[34m●\x1b[0m {:<14} {} (configured)", b.slug, display);
        } else {
            println!(
                "  \x1b[31m✗\x1b[0m {:<14} {} (run: caudra auth login {})",
                b.slug, display, b.slug
            );
        }
    }

    for (slug, def) in &config.providers {
        // 'opencode' could show up here, when the user configured free models on that provider.
        if builtin_provider(slug).is_some()
            || (slug == "opencode" && def.enable_free_models.is_some())
        {
            continue;
        }
        let display = def.display_name.as_deref().unwrap_or(slug);
        if let Some(creds) = load_provider_credentials(storage, slug) {
            println!(
                "  \x1b[32m✓\x1b[0m {:<14} {} (key: {})",
                slug,
                display,
                creds.masked_api_key()
            );
        } else {
            let default_env = format!("{}_API_KEY", slug.to_uppercase().replace('-', "_"));
            let env_var = def.api_key_env.as_deref().unwrap_or(&default_env);
            if env::var(env_var).is_ok() {
                println!(
                    "  \x1b[33m~\x1b[0m {:<14} {} (via {})",
                    slug, display, env_var
                );
            } else {
                println!(
                    "  \x1b[31m✗\x1b[0m {:<14} {} (run: caudra auth login {})",
                    slug, display, slug
                );
            }
        }
    }
    // Catalog providers from models.dev
    let catalog_entries = catalog_providers();
    if !catalog_entries.is_empty() {
        println!("  \x1b[1mCatalog Providers (models.dev):\x1b[0m");
        for entry in &catalog_entries {
            if let Some(creds) = load_provider_credentials(storage, &entry.slug) {
                println!(
                    "  \x1b[32m✓\x1b[0m {:<14} {} (key: {})",
                    entry.slug,
                    entry.display_name,
                    creds.masked_api_key()
                );
            } else if let Some(env) = entry.env_key_set() {
                println!(
                    "  \x1b[33m~\x1b[0m {:<14} {} (via {})",
                    entry.slug, entry.display_name, env
                );
            } else {
                println!(
                    "  \x1b[31m✗\x1b[0m {:<14} {} (run: caudra auth login {})",
                    entry.slug, entry.display_name, entry.slug
                );
            }
        }
        println!();
    }

    Ok(())
}

pub fn models(no_plugins: bool, no_jit: bool) -> Result<()> {
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);

    let host = PluginHost::with_jit(Arc::clone(ToolRegistry::global_arc()), !no_jit)
        .context("initialize lua plugin host")?;
    let config = load_effective_config(&host, no_plugins, &cwd)?;

    smol::block_on(fetch_all_models(
        &config.provider.model_policy,
        |batch| {
            for model in batch.models {
                println!("{model}");
            }
            for warning in batch.warnings {
                eprintln!("warning: {warning}");
            }
        },
        None,
    ));
    Ok(())
}

fn load_effective_config(host: &PluginHost, no_plugins: bool, cwd: &Path) -> Result<Config> {
    host.load_init_files_or_skip(no_plugins, cwd)
        .context("load init.lua files")?
        .unwrap_or_default()
        .into_config(false)
        .context("invalid config")
}

pub fn index(path: &str, no_plugins: bool, no_jit: bool) -> Result<()> {
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);
    let _workcell_host = super::register_builtin_tools(&cwd)?;

    let mut host = PluginHost::with_jit(Arc::clone(ToolRegistry::global_arc()), !no_jit)
        .context("initialize lua plugin host")?;

    let raw_config = host
        .load_init_files_or_skip(no_plugins, &cwd)
        .context("load init.lua files")?;

    let mut config = raw_config
        .unwrap_or_default()
        .into_config(false)
        .context("invalid config")?;
    config.permissions = load_permissions(&cwd);
    super::configure_native_tools(&config.agent);
    super::install_native_permission_rules(&host.plugin_rules(), &cwd);

    host.load_production_builtins(&config.plugins)
        .context("load builtin plugins")?;

    ensure_index_enabled(&config.agent)?;
    let reg = ToolRegistry::global_arc();
    let entry = reg
        .get("index")
        .ok_or_else(|| color_eyre::eyre::eyre!("index tool not registered"))?;
    print!("{}", execute_index(entry, path, config.agent, &cwd)?);
    Ok(())
}

fn ensure_index_enabled(config: &caudra_config::AgentConfig) -> Result<()> {
    if !is_tool_enabled(&config.disabled_tools, "index") {
        bail!("index is disabled by plugins.index.enabled = false");
    }
    Ok(())
}

fn execute_index(
    entry: RegisteredTool,
    path: &str,
    config: caudra_config::AgentConfig,
    project_cwd: &Path,
) -> Result<String> {
    let input = serde_json::json!({"path": path});
    let inv = entry
        .tool
        .parse(&input)
        .map_err(|e| color_eyre::eyre::eyre!("parse index input: {e}"))?;
    let mut ctx = caudra_agent::tools::cli_tool_ctx(project_cwd);
    ctx.config = config;
    let result = smol::block_on(async { inv.execute(&ctx).await });
    match result.output {
        Ok(output) => Ok(output.as_text()),
        Err(e) => bail!("index failed: {e}"),
    }
}

pub fn mcp_auth(server: &str, storage: &StateDir) -> Result<()> {
    smol::block_on(async {
        let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
        let (config, _) = mcp_config::load_config(&cwd);
        let raw = config
            .mcp
            .get(server)
            .ok_or_else(|| color_eyre::eyre::eyre!("unknown MCP server: {server}"))?;
        let (url, oauth) = match mcp_config::parse_server(server.to_owned(), raw.clone())?.transport
        {
            mcp_config::Transport::Http { url, oauth, .. } => (url, oauth),
            _ => color_eyre::eyre::bail!("server '{server}' is not an HTTP transport"),
        };
        let resolved_addresses = mcp_config::resolve_url_addresses(&url)
            .map_err(|error| color_eyre::eyre::eyre!("cannot resolve MCP URL: {error}"))?;
        mcp_oauth::authenticate(
            server,
            &url,
            None,
            storage,
            mcp_oauth::Interaction::Cli,
            oauth,
            Some(&resolved_addresses),
        )
        .await?;
        eprintln!("Successfully authenticated with MCP server '{server}'");
        Ok(())
    })
}

pub fn mcp_logout(server: &str, storage: &StateDir) -> Result<()> {
    let deleted = caudra_storage::auth::delete_mcp_auth(storage, server)?;
    if deleted {
        eprintln!("Removed OAuth credentials for MCP server '{server}'");
    } else {
        eprintln!("No stored credentials for MCP server '{server}'");
    }
    Ok(())
}

const SOURCE_MCP: &str = "mcp";
/// `permissions.toml` still accepts the pre-rename section for the shell tool.
const LEGACY_SHELL_KEY: &str = "bash";
const REASON_DISALLOWED_FLAG: &str = "--disallowed-tools";
const REASON_CONFIG: &str = "disabled by config";
const REASON_NO_VISION: &str = "model has no vision support";
const REASON_NOT_ALLOWED: &str = "not in --allowed-tools";
const REASON_COMPANION: &str = "always on (internal companion)";
const REASON_NO_SUBSCRIPTION: &str = "no ChatGPT subscription";
const REASON_OTHER_EDITOR: &str = "model uses the other editing tool";
const REASON_DEFERRED: &str = "deferred behind tool_search";
/// Padded to a common width so the name column starts at one column.
const STATE_ON: &str = "on  ";
const STATE_OFF: &str = "off ";
const STATE_LAZY: &str = "lazy";

#[derive(serde::Serialize)]
struct ToolRow {
    name: String,
    source: String,
    enabled: bool,
    /// Enabled, and absent from the request array until `tool_search` loads it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    deferred: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    permission: Option<&'static str>,
}

/// Why a built-in would not reach the model, in the order a user would ask:
/// what they typed, then what their config says, then what the model and the
/// allow list leave behind. An enabled tool only earns a note when something
/// asked for it to be off and did not get it.
fn builtin_note(
    name: &str,
    enabled: bool,
    deferred: bool,
    cli_disallowed: &[String],
    config: &AgentConfig,
    model: &Model,
) -> Option<&'static str> {
    let named_off = cli_disallowed.iter().any(|tool| tool == name);
    let config_off = config.disabled_tools.iter().any(|tool| tool == name);
    if enabled {
        if named_off || config_off {
            return Some(REASON_COMPANION);
        }
        return deferred.then_some(REASON_DEFERRED);
    }
    if named_off {
        return Some(REASON_DISALLOWED_FLAG);
    }
    if config_off {
        return Some(REASON_CONFIG);
    }
    if capability_exclusions(model).contains(&name) {
        return Some(match name {
            VIEW_IMAGE_TOOL_NAME => REASON_NO_VISION,
            _ => REASON_OTHER_EDITOR,
        });
    }
    if credential_exclusions().contains(&name) {
        return Some(REASON_NO_SUBSCRIPTION);
    }
    (!config.allowed_tools.is_empty()).then_some(REASON_NOT_ALLOWED)
}

fn permission_default(permissions: &PermissionsConfig, keys: &[ToolKey]) -> Option<&'static str> {
    let effect = keys
        .iter()
        .find_map(|key| permissions.tool_defaults.get(key))?;
    match effect {
        DefaultEffect::Allow => Some("allow"),
        DefaultEffect::Deny => Some("deny"),
        DefaultEffect::Prompt => None,
    }
}

fn builtin_rows(
    registry: &ToolRegistry,
    filter: &ToolFilter,
    config: &Config,
    cli_disallowed: &[String],
    model: &Model,
) -> Vec<ToolRow> {
    let mut rows: Vec<ToolRow> = registry
        .iter()
        .iter()
        .map(|entry| {
            let name = entry.name();
            let keys = match name {
                SHELL_TOOL_NAME => vec![ToolKey::native(name), ToolKey::native(LEGACY_SHELL_KEY)],
                _ => vec![ToolKey::native(name)],
            };
            let enabled = filter.matches(name);
            let deferred = enabled && deferral::is_deferred(name, &config.agent.allowed_tools);
            ToolRow {
                name: name.to_owned(),
                source: entry.source.as_log_field().into_owned(),
                enabled,
                deferred,
                note: builtin_note(name, enabled, deferred, cli_disallowed, &config.agent, model),
                permission: permission_default(&config.permissions, &keys),
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

fn mcp_rows(mcp: Option<&McpSession>, permissions: &PermissionsConfig) -> Vec<ToolRow> {
    let Some(mcp) = mcp else {
        return Vec::new();
    };
    let mut rows: Vec<ToolRow> = mcp
        .request_snapshot()
        .tool_inventory()
        .into_iter()
        .map(|tool| {
            let keys = [
                ToolKey::parse(&tool.qualified_name),
                ToolKey::parse(&format!("{}.*", tool.server)),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
            ToolRow {
                enabled: !tool.disabled,
                deferred: !tool.disabled && tool.deferred,
                note: match (tool.disabled, tool.deferred) {
                    (true, _) => Some(REASON_CONFIG),
                    (false, true) => Some(REASON_DEFERRED),
                    _ => None,
                },
                permission: permission_default(permissions, &keys),
                source: format!("{SOURCE_MCP}:{}", tool.server),
                name: tool.wire_name,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

fn print_group(heading: &str, rows: &[ToolRow]) {
    if rows.is_empty() {
        return;
    }
    let name_width = rows.iter().map(|row| row.name.len()).max().unwrap_or(0);
    let source_width = rows.iter().map(|row| row.source.len()).max().unwrap_or(0);
    println!("{heading}");
    for row in rows {
        let state = match (row.enabled, row.deferred) {
            (false, _) => STATE_OFF,
            (true, true) => STATE_LAZY,
            (true, false) => STATE_ON,
        };
        let mut detail = row.note.map(str::to_owned).unwrap_or_default();
        if let Some(permission) = row.permission {
            if !detail.is_empty() {
                detail.push_str(", ");
            }
            detail.push_str(&format!("permission: {permission}"));
        }
        let detail = if detail.is_empty() {
            String::new()
        } else {
            format!("  ({detail})")
        };
        let line = format!(
            "  {state}  {:name_width$}  {:source_width$}{detail}",
            row.name, row.source
        );
        println!("{}", line.trim_end());
    }
    println!();
}

pub fn tools(cli: &Cli, enabled_only: bool, json: bool, names: bool, schemas: bool) -> Result<()> {
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);
    let _workcell_host = super::register_builtin_tools(&cwd)?;

    let reg = ToolRegistry::global_arc();
    let mut host =
        PluginHost::with_jit(Arc::clone(reg), !cli.no_jit).context("initialize lua plugin host")?;
    let config = super::load_config(&host, cli, &cwd)?;
    super::configure_native_tools(&config.agent);
    super::install_native_permission_rules(&host.plugin_rules(), &cwd);
    host.load_production_builtins(&config.plugins)
        .context("load builtin plugins")?;

    let storage = StateDir::resolve().context("resolve data directory")?;
    let mut model = resolve_model(cli.model.as_deref(), &config.provider, &storage)?;
    caudra_providers::provider::adjust_model(&mut model, Timeouts::default())?;
    let filter = ToolFilter::from_config(&config.agent, &model, &[]);

    let (mcp_handle, mcp_errors) = smol::block_on(caudra_agent::mcp::start_connected(&cwd));
    if !mcp_errors.is_empty() {
        eprintln!("warning: {mcp_errors}");
    }
    let mcp = mcp_handle.map(|handle| {
        McpSession::new(handle, &[]).with_disabled_tools(&config.agent.disabled_tools)
    });

    if schemas {
        let ctx = DescriptionContext {
            filter: &filter,
            audience: ToolAudience::MAIN,
            workflow: false,
        };
        let mut defs = reg.definitions(
            &caudra_agent::template::env_vars(),
            &ctx,
            model.supports_tool_examples(),
        );
        if let Some(mcp) = &mcp {
            mcp.request_snapshot().extend_tools(&mut defs);
        }
        println!("{}", serde_json::to_string_pretty(&defs)?);
        return Ok(());
    }

    let cli_disallowed = cli
        .disallowed_tools
        .iter()
        .map(|tool| normalize_tool_name(tool))
        .collect::<Result<Vec<_>>>()?;
    let mut builtin = builtin_rows(reg, &filter, &config, &cli_disallowed, &model);
    let mut mcp_tools = mcp_rows(mcp.as_ref(), &config.permissions);
    if enabled_only {
        builtin.retain(|row| row.enabled);
        mcp_tools.retain(|row| row.enabled);
    }

    if names {
        for row in builtin.iter().chain(&mcp_tools) {
            println!("{}", row.name);
        }
    } else if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "model": model.spec(),
                "builtin": builtin,
                "mcp": mcp_tools,
            }))?
        );
    } else {
        print_group("Built-in", &builtin);
        print_group("MCP", &mcp_tools);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn prompt(
    variant: &crate::cli::PromptVariant,
    plan: bool,
    tools: bool,
    names: bool,
    no_plugins: bool,
    no_jit: bool,
    no_rtk: bool,
    model_arg: Option<&str>,
    profile_arg: Option<&str>,
) -> Result<()> {
    use crate::cli::PromptVariant;
    use caudra_agent::agent::{build_system_prompt, load_instruction_text};
    use caudra_agent::prompt::{
        PromptId, TASK_BUILD_CONTRACT, TASK_PLAN_CONTRACT, assemble_task_with_filter,
    };
    use caudra_agent::template;
    use caudra_agent::tools::{DescriptionContext, ToolAudience, ToolFilter, ToolRegistry};

    if plan && !matches!(variant, PromptVariant::System) {
        bail!("--plan can only be used with the 'system' prompt variant");
    }
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);
    let _workcell_host = super::register_builtin_tools(&cwd)?;

    let vars = template::env_vars();
    let reg = ToolRegistry::global_arc();
    let mut host =
        PluginHost::with_jit(Arc::clone(reg), !no_jit).context("initialize lua plugin host")?;
    let raw_config = host
        .load_init_files_or_skip(no_plugins, &cwd)
        .context("load init.lua files")?;
    let config = raw_config
        .unwrap_or_default()
        .into_config(no_rtk)
        .context("invalid config")?;
    super::configure_native_tools(&config.agent);
    super::install_native_permission_rules(&host.plugin_rules(), &cwd);
    host.load_production_builtins(&config.plugins)
        .context("load builtin plugins")?;

    let cwd_str = cwd.to_string_lossy();
    let instructions = load_instruction_text(&cwd_str);
    let slots = host.event_handle().collect_prompt_slots(&config.agent);
    let prompt_profiles = caudra_agent::prompt::profile::PromptProfileCatalog::discover_user();
    let profile_name = profile_arg.or(config.agent.system_prompt_profile.as_deref());
    let system_prompt_profile = prompt_profiles
        .resolve(profile_name)
        .context("resolve system prompt profile")?;
    let storage = StateDir::resolve().context("resolve data directory")?;
    let mut model = crate::setup::resolve_model(model_arg, &config.provider, &storage)?;
    caudra_providers::provider::adjust_model(&mut model, caudra_providers::Timeouts::default())?;
    let filter = ToolFilter::from_config(&config.agent, &model, &[]);

    if tools {
        let thinking = config
            .always_thinking
            .clone()
            .map(caudra_providers::ThinkingConfig::from)
            .unwrap_or_default();
        let bindings = prompt_profiles.bind_for_tasks(
            &model,
            &thinking,
            &config.provider.model_policy,
            caudra_providers::Timeouts::default(),
        );
        let vars = vars.set(
            "{task_system_prompt_profiles}",
            bindings.task_tool_summary("Caudra's built-in task prompt"),
        );
        let ctx = DescriptionContext {
            filter: &filter,
            audience: ToolAudience::MAIN,
            workflow: false,
        };
        let defs = reg.definitions(&vars, &ctx, model.supports_tool_examples());
        if names {
            for name in defs
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| d["name"].as_str())
            {
                println!("{name}");
            }
        } else {
            println!("{}", serde_json::to_string_pretty(&defs)?);
        }
        return Ok(());
    }

    let output = match variant {
        PromptVariant::System => {
            let mode = if plan {
                caudra_agent::AgentMode::Plan(std::path::PathBuf::from("plan.md"))
            } else {
                caudra_agent::AgentMode::Build
            };
            build_system_prompt(
                &vars,
                &mode,
                &instructions,
                &slots,
                &filter,
                &model,
                system_prompt_profile.as_deref(),
            )
        }
        PromptVariant::Research => vars
            .apply(&assemble_task_with_filter(
                PromptId::Research,
                &slots,
                &filter,
                &instructions,
                system_prompt_profile.as_deref(),
                TASK_PLAN_CONTRACT,
            ))
            .into_owned(),
        PromptVariant::General => vars
            .apply(&assemble_task_with_filter(
                PromptId::General,
                &slots,
                &filter,
                &instructions,
                system_prompt_profile.as_deref(),
                TASK_BUILD_CONTRACT,
            ))
            .into_owned(),
    };

    print!("{output}");
    Ok(())
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    use test_case::test_case;

    #[test_case("OpenAI", None, LoginRoute::OpenAiOauth ; "normalized_openai_defaults_to_oauth")]
    #[test_case("Anthropic", None, LoginRoute::AnthropicOauth ; "normalized_anthropic_defaults_to_oauth")]
    #[test_case("openai", Some(AuthMethod::ApiKey), LoginRoute::ApiKey ; "openai_api_key")]
    #[test_case("anthropic", Some(AuthMethod::ApiKey), LoginRoute::ApiKey ; "anthropic_api_key")]
    #[test_case("xAI", None, LoginRoute::XaiOauth ; "normalized_xai")]
    #[test_case("Copilot", None, LoginRoute::Copilot ; "normalized_copilot")]
    #[test_case("google", None, LoginRoute::ApiKey ; "ordinary_provider")]
    fn provider_login_routes(raw: &str, method: Option<AuthMethod>, expected: LoginRoute) {
        assert_eq!(login_route(&slugify(raw), method).unwrap(), expected);
    }

    #[test]
    fn oauth_method_rejects_non_subscription_provider() {
        assert!(login_route("google", Some(AuthMethod::Oauth)).is_err());
    }

    #[test_case("1", Some(Protocol::Openai) ; "chat_by_number")]
    #[test_case("openai", Some(Protocol::Openai) ; "chat_by_name")]
    #[test_case("2", Some(Protocol::OpenaiResponses) ; "responses_by_number")]
    #[test_case("openai-responses", Some(Protocol::OpenaiResponses) ; "responses_by_name")]
    #[test_case("unknown", None ; "unknown_protocol")]
    fn custom_protocol_choices(input: &str, expected: Option<Protocol>) {
        assert_eq!(custom_protocol(input), expected);
    }
}

#[cfg(test)]
mod tools_tests {
    use super::*;

    const MODEL_SPEC: &str = "anthropic/claude-opus-4-8";

    fn agent_config(disabled: &[&str], allowed: &[&str]) -> caudra_config::AgentConfig {
        caudra_config::AgentConfig {
            disabled_tools: disabled.iter().map(|t| (*t).to_string()).collect(),
            allowed_tools: allowed.iter().map(|t| (*t).to_string()).collect(),
            ..Default::default()
        }
    }

    fn note(
        name: &str,
        enabled: bool,
        cli: &[&str],
        config: &caudra_config::AgentConfig,
    ) -> Option<&'static str> {
        let cli: Vec<String> = cli.iter().map(|t| (*t).to_string()).collect();
        let model = caudra_providers::Model::from_spec(MODEL_SPEC).unwrap();
        let deferred = enabled && deferral::is_deferred(name, &config.allowed_tools);
        builtin_note(name, enabled, deferred, &cli, config, &model)
    }

    #[test]
    fn a_config_disabled_tool_says_so() {
        let config = agent_config(&["shell"], &[]);
        assert_eq!(note("shell", false, &[], &config), Some(REASON_CONFIG));
    }

    /// The flag wins the explanation: it is the thing the user just typed.
    #[test]
    fn a_flag_disabled_tool_names_the_flag() {
        let config = agent_config(&["shell"], &[]);
        assert_eq!(
            note("shell", false, &["shell"], &config),
            Some(REASON_DISALLOWED_FLAG)
        );
    }

    #[test]
    fn a_tool_outside_the_allow_list_says_so() {
        let config = agent_config(&[], &["file_read"]);
        assert_eq!(note("shell", false, &[], &config), Some(REASON_NOT_ALLOWED));
    }

    #[test]
    fn an_ordinary_enabled_tool_has_no_note() {
        assert_eq!(note("shell", true, &[], &agent_config(&[], &[])), None);
    }

    /// Enabled but absent from the request array until asked for, which is
    /// exactly the state a `caudra tools` reader would otherwise misread as
    /// "the model can see this".
    #[test]
    fn a_deferred_tool_says_it_waits_behind_tool_search() {
        let config = agent_config(&[], &[]);
        for deferred in caudra_config::DEFERRED_BUILTIN_TOOLS {
            assert_eq!(
                note(deferred.name, true, &[], &config),
                Some(REASON_DEFERRED),
                "{}",
                deferred.name
            );
        }
    }

    /// Asking for a tool by name is asking for it upfront, so the allow list
    /// cancels the deferral rather than leaving the user to search for what
    /// they already named.
    #[test]
    fn an_allow_listed_deferred_tool_is_not_deferred() {
        let name = caudra_config::DEFERRED_BUILTIN_TOOLS[0].name;
        assert_eq!(note(name, true, &[], &agent_config(&[], &[name])), None);
    }

    /// Naming a companion is not an error, but the report has to say the
    /// request did not take.
    #[test]
    fn a_companion_asked_to_turn_off_stays_on_with_a_note() {
        for name in caudra_config::INTERNAL_COMPANION_TOOL_NAMES {
            let config = agent_config(&[name], &[]);
            assert_eq!(note(name, true, &[], &config), Some(REASON_COMPANION));
        }
    }
}

#[cfg(test)]
mod index_tests {
    use super::*;

    #[test]
    fn one_shot_index_honors_enablement_and_source_size() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.rs");
        std::fs::write(&source, "pub fn run() {}\n").unwrap();
        std::fs::create_dir(root.path().join("nested")).unwrap();
        let registry = Arc::new(ToolRegistry::new());
        let host = caudra_workcell::WorkcellHost::new(root.path(), None).unwrap();
        host.register(&registry).unwrap();
        let entry = registry.get("index").unwrap();

        let output = execute_index(
            entry.clone(),
            "source.rs",
            caudra_config::AgentConfig::default(),
            root.path(),
        )
        .unwrap();
        assert_eq!(output, "fns:\n  pub run() [1]");

        let directory = execute_index(
            entry.clone(),
            ".",
            caudra_config::AgentConfig::default(),
            root.path(),
        )
        .unwrap();
        assert_eq!(directory, "nested/\nsource.rs");

        let disabled = caudra_config::AgentConfig {
            disabled_tools: vec!["index".into()],
            ..Default::default()
        };
        assert!(ensure_index_enabled(&disabled).is_err());

        std::fs::write(&source, vec![b' '; 1024 * 1024 + 1]).unwrap();
        let limited = caudra_config::AgentConfig {
            index_max_file_size_mb: 1,
            ..Default::default()
        };
        let error = execute_index(entry, "source.rs", limited, root.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exceeds maximum size of 1048576 bytes")
        );
    }
}
